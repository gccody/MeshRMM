mod config;
#[cfg(any(target_os = "macos", test))]
mod h264;
mod platform;
mod signaling;
mod transport;
#[cfg(any(windows, target_os = "macos"))]
mod updater;

use anyhow::Context;

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
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if config.json_logs {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    Ok(config)
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
            r"HKCU\Software\Classes\pulsermm",
            "URL:PulseRMM Remote Protocol",
        ),
        (r"HKCU\Software\Classes\pulsermm", ""),
        (
            r"HKCU\Software\Classes\pulsermm\shell\open\command",
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
    let bootstrap = signaling::create_session(&config)
        .await
        .context("remote session request failed")?;
    tracing::info!(
        session_id = %bootstrap.session_id,
        expires_at_unix_ms = bootstrap.expires_at_unix_ms,
        "remote session authorized"
    );
    transport::run_receiver(&config, bootstrap)
        .await
        .context("remote viewer session failed")
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
    anyhow::bail!("PulseRMM remote-client supports Windows and macOS")
}
