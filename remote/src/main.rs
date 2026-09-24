// The Windows viewer is a GUI app: no console window flashes up or lingers.
// Fatal errors are shown in a message box, and the log is a file.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod clipboard;
mod config;
mod debug;
#[cfg(windows)]
mod deep_link;
mod errors;
mod h264;
mod http;
mod matroska;
mod platform;
mod preferences;
mod recording;
mod shortcuts;
mod shutdown;
mod signaling;
#[cfg(windows)]
mod single_instance;
mod transport;
#[cfg(any(windows, target_os = "macos"))]
mod updater;
#[cfg(any(windows, test))]
mod video_layout;

use anyhow::Context;
use meshrmm_signaling_client::ReconnectBackoff;
use std::time::Duration;

/// How long a new viewer waits for the previous viewer of the same device to
/// release its session. Ending a session retries for up to about 16 seconds.
#[cfg(windows)]
const VIEWER_REPLACEMENT_TIMEOUT: Duration = Duration::from_secs(30);

fn initialize(launch_deep_link: Option<&str>) -> anyhow::Result<config::Config> {
    #[cfg(windows)]
    let registration = deep_link::register();
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
    #[cfg(windows)]
    if let Err(error) = registration {
        tracing::warn!(error = ?error, "could not register the meshrmm: link handler");
    }
    if let Some(ready) = std::env::var_os("MESHRMM_UPDATE_READY_FILE") {
        std::fs::write(ready, b"ready").context("could not acknowledge viewer initialization")?;
    }
    Ok(config)
}

fn initialize_tracing(config: &config::Config) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    #[cfg(any(windows, target_os = "macos"))]
    let log_path = {
        let (path, writer) = open_log()?;
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

/// Opens the viewer's log, which rotates at 10 MiB and keeps three older files.
#[cfg(any(windows, target_os = "macos"))]
fn open_log() -> anyhow::Result<(
    std::path::PathBuf,
    std::sync::Mutex<meshrmm_log_file::RotatingFile>,
)> {
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
        .context("viewer log has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create viewer log directory {}", parent.display()))?;
    let log = meshrmm_log_file::RotatingFile::open(&path)
        .with_context(|| format!("failed to open viewer log {}", path.display()))?;
    Ok((path, std::sync::Mutex::new(log)))
}

/// The update helper runs detached, with no window to report a failure in,
/// so it logs its steps to the viewer's log. It runs before any configuration
/// is loaded and always writes text.
#[cfg(any(windows, target_os = "macos"))]
fn initialize_update_helper_tracing() {
    let Ok((_, writer)) = open_log() else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_ansi(false)
        .try_init();
}

/// Runs the session until it ends. A recording still running is saved, and
/// its notice goes to `recording_notice` for the caller to show once any
/// session-wide lock is released.
async fn run_session(
    config: config::Config,
    recording_notice: &mut Option<String>,
) -> anyhow::Result<()> {
    let resume_state = transport::ViewerResumeState::default();
    let result = run_resumable_session(&config, &resume_state).await;
    resume_state.close_reconnecting_window();
    let recording = resume_state.clone();
    *recording_notice = tokio::task::spawn_blocking(move || recording.finish_recording())
        .await
        .unwrap_or_default();
    result
}

async fn run_resumable_session(
    config: &config::Config,
    resume_state: &transport::ViewerResumeState,
) -> anyhow::Result<()> {
    let mut bootstrap = match config.bootstrap.clone() {
        Some(bootstrap) => bootstrap,
        None => signaling::create_session(config)
            .await
            .context("remote session request failed")?,
    };
    tracing::info!(
        session_id = %bootstrap.session_id,
        expires_at_unix_ms = bootstrap.expires_at_unix_ms,
        "remote session authorized"
    );
    let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15));
    if bootstrap.start_in_background {
        resume_state.select_background_display();
    }
    loop {
        match transport::run_receiver(config, bootstrap.clone(), resume_state.clone()).await {
            Ok(()) => {
                end_session_after_disconnect(config, &bootstrap).await;
                return Ok(());
            }
            Err(error) if signaling::is_terminal_session_error(&error) => {
                if let Err(cleanup) = signaling::end_session(config, &bootstrap).await {
                    tracing::warn!(%cleanup, "could not acknowledge terminal session cleanup");
                }
                return Err(error).context("remote viewer session can no longer be resumed");
            }
            Err(error) => {
                let delay = backoff.next_delay();
                if shutdown::requested() {
                    end_session_after_disconnect(config, &bootstrap).await;
                    return Ok(());
                }
                tracing::warn!(
                    error = ?error,
                    session_id = %bootstrap.session_id,
                    retry_seconds = delay.as_secs(),
                    "remote viewer disconnected; waiting to resume"
                );
                match signaling::resume_session(config, &bootstrap).await {
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
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = shutdown::wait() => {
                        end_session_after_disconnect(config, &bootstrap).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Releases the device lease after the session ended by choice. A failure
/// is logged rather than reported: the disconnect itself succeeded, and the
/// server expires the lease on its own.
async fn end_session_after_disconnect(
    config: &config::Config,
    bootstrap: &meshrmm_protocol::SessionBootstrap,
) {
    if let Err(error) = signaling::end_session(config, bootstrap).await {
        tracing::warn!(
            error = ?error,
            session_id = %bootstrap.session_id,
            "could not confirm session cleanup after the viewer disconnected"
        );
    }
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    // Lets the identity commands print when run from a terminal.
    platform::attach_parent_console();
    platform::enable_dpi_awareness();
    match meshrmm_session_transport::identity::handle_command(
        meshrmm_session_transport::identity::viewer_directory,
    ) {
        Ok(true) => return ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error:#}");
            return ExitCode::FAILURE;
        }
    }
    if updater::is_helper_invocation() {
        // The detached helper has no window; it relaunches the viewer either way.
        initialize_update_helper_tracing();
        return match updater::apply_scheduled_update() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(error = ?error, "client update helper failed");
                ExitCode::FAILURE
            }
        };
    }
    let mut recording_notice = None;
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create the viewer network runtime")
        .and_then(|runtime| runtime.block_on(run_windows_viewer(&mut recording_notice)));
    // The instance mutex was released with the session, so a new link does
    // not wait on these dialogs.
    if let Some(notice) = recording_notice {
        platform::show_notice("Session recording", &notice);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = ?error, "remote viewer stopped with an error");
            platform::show_fatal_error(&errors::user_message(&error));
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
async fn run_windows_viewer(recording_notice: &mut Option<String>) -> anyhow::Result<()> {
    let mut config = initialize(None)?;
    // Held on the main thread until the process exits.
    let _instance = match config.device_id.as_deref() {
        Some(device_id) => Some(single_instance::claim(
            device_id,
            VIEWER_REPLACEMENT_TIMEOUT,
            || shutdown::request("a new dashboard link opened this device"),
        )?),
        None => None,
    };
    if config.bootstrap.is_none() {
        config.bootstrap = Some(
            signaling::create_session(&config)
                .await
                .context("remote session request failed")?,
        );
    }
    match updater::check_and_schedule(&config).await {
        Ok(true) => Ok(()),
        Ok(false) => run_session(config, recording_notice).await,
        Err(error) => {
            tracing::warn!(error = ?error, "client update check failed; continuing with this launch");
            run_session(config, recording_notice).await
        }
    }
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    if meshrmm_session_transport::identity::handle_command(
        meshrmm_session_transport::identity::viewer_directory,
    )? {
        return Ok(());
    }
    if updater::is_helper_invocation() {
        initialize_update_helper_tracing();
        tracing::info!(
            process_id = std::process::id(),
            "client update helper started"
        );
        let result = updater::apply_scheduled_update();
        match &result {
            Ok(()) => tracing::info!("client update installed"),
            Err(error) => tracing::error!(error = ?error, "client update helper failed"),
        }
        return result;
    }
    platform::run_application(move |deep_link| {
        let mut config = initialize(deep_link.as_deref())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to create the macOS network runtime")?;
        if config.bootstrap.is_none() {
            config.bootstrap = Some(
                runtime
                    .block_on(signaling::create_session(&config))
                    .context("remote session request failed")?,
            );
        }
        let mut recording_notice = None;
        let result = match runtime
            .block_on(updater::check_and_schedule(&config, deep_link.as_deref()))
        {
            Ok(true) => Ok(()),
            Ok(false) => runtime.block_on(run_session(config, &mut recording_notice)),
            Err(error) => {
                tracing::warn!(error = ?error, "client update check failed; continuing with this launch");
                runtime.block_on(run_session(config, &mut recording_notice))
            }
        };
        if let Some(notice) = recording_notice {
            platform::show_notice("Session recording", &notice);
        }
        result
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("MeshRMM remote-client supports Windows and macOS")
}
