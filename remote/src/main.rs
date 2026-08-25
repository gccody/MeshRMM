mod clipboard;
mod config;
mod debug;
#[cfg(any(target_os = "macos", test))]
mod h264;
mod platform;
mod signaling;
mod transport;
#[cfg(any(windows, target_os = "macos"))]
mod updater;

use anyhow::Context;
use meshrmm_signaling_client::ReconnectBackoff;
use std::time::Duration;

fn initialize(launch_deep_link: Option<&str>) -> anyhow::Result<config::Config> {
    #[cfg(windows)]
    register_windows_deep_link_handler();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install the Rustls ring crypto provider"))?;
    #[cfg(target_os = "macos")]
    let config = config::Config::load_with_deep_link(launch_deep_link)?;
    #[cfg(not(target_os = "macos"))]
    let config = {
        let _ = launch_deep_link;
        config::Config::load()?
    };
    initialize_tracing(&config)?;
    Ok(config)
}

fn initialize_tracing(config: &config::Config) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    #[cfg(any(windows, target_os = "macos"))]
    let log_path = {
        #[cfg(windows)]
        let path = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .context("Windows did not provide LOCALAPPDATA for viewer logging")?
            .join("MeshRMM")
            .join("remote.log");
        #[cfg(target_os = "macos")]
        let path = std::path::PathBuf::from(objc2_foundation::NSHomeDirectory().to_string())
            .join("Library")
            .join("Logs")
            .join("MeshRMM")
            .join("remote.log");
        let parent = path
            .parent()
            .context("macOS viewer log has no parent directory")?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create viewer log directory {}", parent.display())
        })?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open viewer log {}", path.display()))?;
        let writer = std::sync::Mutex::new(log);
        if config.json_logs {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_thread_ids(true)
                .with_thread_names(true)
                .json()
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_ansi(false)
                .init();
        }
        path
    };

    #[cfg(all(not(windows), not(target_os = "macos")))]
    if config.json_logs {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    #[cfg(any(windows, target_os = "macos"))]
    tracing::info!(
        process_id = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        log_path = %log_path.display(),
        "viewer logging initialized"
    );
    Ok(())
}

#[cfg(windows)]
fn register_windows_deep_link_handler() {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let command = format!("\"{}\" \"%1\"", executable.display());
    let entries = [
        (
            r"HKCU\Software\Classes\meshrmm",
            "URL:MeshRMM Remote Protocol",
        ),
        (r"HKCU\Software\Classes\meshrmm", ""),
        (
            r"HKCU\Software\Classes\meshrmm\shell\open\command",
            command.as_str(),
        ),
    ];
    for (index, (key, value)) in entries.iter().enumerate() {
        let mut arguments = vec!["add", key, "/ve", "/d", value, "/f"];
        if index == 1 {
            arguments = vec!["add", key, "/v", "URL Protocol", "/d", value, "/f"];
        }
        let _ = std::process::Command::new("reg.exe")
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

async fn run_session(config: config::Config) -> anyhow::Result<()> {
    let mut bootstrap = signaling::create_session(&config)
        .await
        .context("remote session request failed")?;
    tracing::info!(
        session_id = %bootstrap.session_id,
        expires_at_unix_ms = bootstrap.expires_at_unix_ms,
        "remote session authorized"
    );
    let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15));
    let resume_state = transport::ViewerResumeState::default();
    loop {
        match transport::run_receiver(&config, bootstrap.clone(), resume_state.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) if signaling::is_terminal_session_error(&error) => {
                return Err(error).context("remote viewer session can no longer be resumed");
            }
            Err(error) => {
                let delay = backoff.next_delay();
                tracing::warn!(
                    error = ?error,
                    session_id = %bootstrap.session_id,
                    retry_seconds = delay.as_secs(),
                    "remote viewer disconnected; waiting to resume"
                );
                match signaling::resume_session(&config, &bootstrap).await {
                    Ok(refreshed) => {
                        bootstrap = refreshed;
                        tracing::info!(
                            session_id = %bootstrap.session_id,
                            expires_at_unix_ms = bootstrap.expires_at_unix_ms,
                            "refreshed remote-session credentials for reconnect"
                        );
                    }
                    Err(error) if signaling::is_terminal_session_error(&error) => {
                        return Err(error)
                            .context("remote viewer session can no longer be resumed");
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = ?error,
                            session_id = %bootstrap.session_id,
                            "could not refresh resume credentials; retrying the existing session"
                        );
                    }
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if updater::is_helper_invocation() {
        return updater::apply_scheduled_update();
    }
    let config = initialize(None)?;
    match updater::check_and_schedule(&config).await {
        Ok(true) => Ok(()),
        Ok(false) => run_session(config).await,
        Err(error) => {
            tracing::warn!(error = ?error, "client update check failed; continuing with this launch");
            run_session(config).await
        }
    }
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    if updater::is_helper_invocation() {
        return updater::apply_scheduled_update();
    }
    platform::run_application(move |deep_link| {
        let config = initialize(deep_link.as_deref())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to create the macOS network runtime")?;
        match runtime.block_on(updater::check_and_schedule(&config, deep_link.as_deref())) {
            Ok(true) => Ok(()),
            Ok(false) => runtime.block_on(run_session(config)),
            Err(error) => {
                tracing::warn!(error = ?error, "client update check failed; continuing with this launch");
                runtime.block_on(run_session(config))
            }
        }
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("MeshRMM remote-client supports Windows and macOS")
}
