// The Windows viewer is a GUI app: no console window flashes up or lingers.
// Fatal errors are shown in a message box, and the log is a file.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "macos", test))]
mod annotation;
mod clipboard;
mod config;
mod debug;
#[cfg(windows)]
mod deep_link;
mod errors;
mod h264;
mod http;
mod idle_disconnect;
mod input;
mod launch_status;
mod matroska;
mod platform;
mod preferences;
mod reconnect;
mod recording;
mod shortcuts;
mod shutdown;
mod signaling;
#[cfg(windows)]
mod single_instance;
#[cfg(any(windows, test))]
mod stream_reset;
#[cfg(any(windows, target_os = "macos", test))]
mod toolbar;
mod transport;
#[cfg(any(windows, target_os = "macos"))]
mod updater;
#[cfg(any(windows, test))]
mod video_layout;

use anyhow::Context;
use launch_status::LaunchStatus;
use meshrmm_signaling_client::ReconnectBackoff;
use reconnect::{Disposition, ReconnectPhase};
use std::time::{Duration, Instant};

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

/// A message for the user once the session has ended: its title and text.
type Notice = (&'static str, String);

/// Runs the session until it ends, or until the technician has been idle for
/// the chosen time. A recording still running is saved. Notices about how the
/// session ended go to `notices` for the caller to show once any
/// session-wide lock is released.
async fn run_session(config: config::Config, notices: &mut Vec<Notice>) -> anyhow::Result<()> {
    let resume_state = transport::ViewerResumeState::with_audio_muted(preferences::audio_muted());
    let session = run_resumable_session(&config, &resume_state);
    tokio::pin!(session);
    let result = tokio::select! {
        result = &mut session => result,
        minutes = disconnect_when_idle(&resume_state) => {
            tracing::info!(minutes, "the technician was idle; ending the remote session");
            shutdown::request("the session was idle");
            notices.push((
                "Session ended",
                format!(
                    "The session was disconnected after {} of inactivity.",
                    idle_disconnect::label(Some(minutes))
                ),
            ));
            session.await
        }
    };
    resume_state.close_reconnecting_window();
    let recording = resume_state.clone();
    let recording_notice = tokio::task::spawn_blocking(move || recording.finish_recording())
        .await
        .unwrap_or_default();
    notices.extend(recording_notice.map(|notice| ("Session recording", notice)));
    result
}

/// Completes with the idle time, in minutes, once the technician has been
/// idle that long.
async fn disconnect_when_idle(resume_state: &transport::ViewerResumeState) -> u32 {
    let mut check = tokio::time::interval(idle_disconnect::CHECK_INTERVAL);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        check.tick().await;
        if let Some(minutes) = resume_state.idle_disconnect_expired(Instant::now()) {
            return minutes;
        }
    }
}

/// Requests the remote session unless the user cancels first. A request
/// cancelled in flight leaves its lease to expire on the server: without the
/// response there is nothing to end it with.
async fn request_session(
    config: &config::Config,
) -> anyhow::Result<Option<meshrmm_protocol::SessionBootstrap>> {
    launch_status::report(LaunchStatus::RequestingSession);
    tokio::select! {
        biased;
        () = shutdown::wait() => Ok(None),
        bootstrap = signaling::create_session(config) => {
            bootstrap.context("remote session request failed").map(Some)
        }
    }
}

async fn run_resumable_session(
    config: &config::Config,
    resume_state: &transport::ViewerResumeState,
) -> anyhow::Result<()> {
    let mut bootstrap = match config.bootstrap.clone() {
        Some(bootstrap) => bootstrap,
        None => match request_session(config).await? {
            Some(bootstrap) => bootstrap,
            None => return Ok(()),
        },
    };
    tracing::info!(
        session_id = %bootstrap.session_id,
        expires_at_unix_ms = bootstrap.expires_at_unix_ms,
        "remote session authorized"
    );
    if shutdown::requested() {
        // Cancelled after the session was created: release its lease now.
        end_session_after_disconnect(config, &bootstrap).await;
        return Ok(());
    }
    let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15));
    if bootstrap.start_in_background {
        resume_state.select_background_display();
    }
    let startup_started = Instant::now();
    let mut startup_failures = 0_u32;
    loop {
        resume_state.begin_attempt();
        let error =
            match transport::run_receiver(config, bootstrap.clone(), resume_state.clone()).await {
                Ok(()) => {
                    end_session_after_disconnect(config, &bootstrap).await;
                    return Ok(());
                }
                Err(error) => error,
            };
        if shutdown::requested() {
            end_session_after_disconnect(config, &bootstrap).await;
            return Ok(());
        }
        let ever_presented = resume_state.ever_presented();
        if !ever_presented {
            startup_failures += 1;
        }
        let disposition = reconnect::disposition(
            &error,
            ever_presented,
            startup_failures,
            startup_started
                .elapsed()
                .saturating_sub(resume_state.approval_wait()),
        );
        if disposition != Disposition::Retry {
            tracing::error!(
                error = ?error,
                ?disposition,
                startup_failures,
                session_id = %bootstrap.session_id,
                "remote viewer stopped retrying the session"
            );
            if let Err(cleanup) = signaling::end_session(config, &bootstrap).await {
                tracing::warn!(%cleanup, "could not acknowledge terminal session cleanup");
            }
            return Err(reconnect::stopped_error(&error));
        }
        let failed_at = Instant::now();
        let streamed_for = resume_state.attempt_streamed_for(failed_at);
        let delay = backoff.delay_after(streamed_for);
        let status = resume_state.record_failure(&error, failed_at);
        // Shows the reason in the kept window while the session resumes.
        resume_state.set_reconnect_phase(ReconnectPhase::Attempting);
        tracing::warn!(
            error = ?error,
            session_id = %bootstrap.session_id,
            retry_seconds = delay.as_secs(),
            streamed_seconds = streamed_for.map(|streamed| streamed.as_secs()),
            reason = ?status.reason,
            startup_failures,
            "remote viewer disconnected; waiting to resume"
        );
        if !ever_presented {
            launch_status::report(LaunchStatus::Retrying {
                attempt: startup_failures + 1,
                max: reconnect::STARTUP_ATTEMPTS,
            });
        }
        let resumed = tokio::select! {
            resumed = signaling::resume_session(config, &bootstrap) => resumed,
            () = shutdown::wait() => {
                end_session_after_disconnect(config, &bootstrap).await;
                return Ok(());
            }
        };
        match resumed {
            Ok(refreshed) => {
                bootstrap = refreshed;
                tracing::info!(
                    session_id = %bootstrap.session_id,
                    expires_at_unix_ms = bootstrap.expires_at_unix_ms,
                    "refreshed remote-session credentials for reconnect"
                );
            }
            Err(error) if signaling::is_terminal_session_error(&error) => {
                return Err(error).context("remote viewer session can no longer be resumed");
            }
            Err(error) => {
                let reason = reconnect::classify_resume_failure(&error);
                if let Some(reason) = reason {
                    resume_state.update_reconnect_status(|status| status.reason = reason);
                }
                tracing::warn!(
                    error = ?error,
                    session_id = %bootstrap.session_id,
                    ?reason,
                    "could not refresh resume credentials; retrying the existing session"
                );
            }
        }
        // "Retry now" ends only this wait: a click from before it is ignored,
        // and skipping the wait leaves the backoff where it is.
        let retry = reconnect::retry_generation();
        resume_state.set_reconnect_phase(ReconnectPhase::Waiting {
            until: Instant::now() + delay,
        });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = reconnect::wait_for_retry_after(retry) => {
                tracing::info!(session_id = %bootstrap.session_id, "reconnecting now at the user's request");
            }
            () = shutdown::wait() => {
                end_session_after_disconnect(config, &bootstrap).await;
                return Ok(());
            }
        }
        resume_state.set_reconnect_phase(ReconnectPhase::Attempting);
    }
}

/// Releases the device lease after the session ended by choice. A failure
/// is logged rather than reported: the disconnect itself succeeded, and the
/// server expires the lease on its own. When the user asked to stop (Cancel,
/// Quit, closing the window, or a replacing link), the release gets a short
/// budget so an unreachable server cannot hold the viewer open.
async fn end_session_after_disconnect(
    config: &config::Config,
    bootstrap: &meshrmm_protocol::SessionBootstrap,
) {
    let release = signaling::end_session(config, bootstrap);
    let result = match shutdown::lease_release_budget(shutdown::requested()) {
        Some(budget) => match tokio::time::timeout(budget, release).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "the server did not confirm session cleanup within {} seconds",
                budget.as_secs()
            )),
        },
        None => release.await,
    };
    if let Err(error) = result {
        tracing::warn!(
            error = ?error,
            session_id = %bootstrap.session_id,
            "could not confirm session cleanup after the viewer disconnected"
        );
    }
}

/// `--third-party-notices` prints the licenses of bundled third-party code.
fn third_party_notices_requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--third-party-notices")
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    // Lets the identity commands print when run from a terminal.
    platform::attach_parent_console();
    if third_party_notices_requested() {
        print!("{}", meshrmm_audio::THIRD_PARTY_NOTICES);
        return ExitCode::SUCCESS;
    }
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
    let mut notices = Vec::new();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create the viewer network runtime")
        .and_then(|runtime| runtime.block_on(run_windows_viewer(&mut notices)));
    platform::close_launch_status();
    // The instance mutex was released with the session, so a new link does
    // not wait on these dialogs.
    for (title, notice) in notices {
        platform::show_notice(title, &notice);
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
async fn run_windows_viewer(notices: &mut Vec<Notice>) -> anyhow::Result<()> {
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
    // Cancel while the previous viewer was closing takes effect here: the
    // request below returns at once, and a session passed in by an update
    // is ended when the session starts.
    if config.bootstrap.is_none() {
        match request_session(&config).await? {
            Some(bootstrap) => config.bootstrap = Some(bootstrap),
            None => return Ok(()),
        }
    }
    let update = tokio::select! {
        biased;
        // Downloads stay in memory, so abandoning one leaves nothing behind.
        () = shutdown::wait() => Ok(false),
        update = updater::check_and_schedule(&config) => update,
    };
    match update {
        Ok(true) => Ok(()),
        Ok(false) => run_session(config, notices).await,
        Err(error) => {
            tracing::warn!(error = ?error, "client update check failed; continuing with this launch");
            run_session(config, notices).await
        }
    }
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    if third_party_notices_requested() {
        print!("{}", meshrmm_audio::THIRD_PARTY_NOTICES);
        return Ok(());
    }
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
            match runtime.block_on(request_session(&config))? {
                Some(bootstrap) => config.bootstrap = Some(bootstrap),
                None => return Ok(()),
            }
        }
        let mut notices = Vec::new();
        let update = runtime.block_on(async {
            tokio::select! {
                biased;
                // Downloads stay in memory, so abandoning one leaves nothing behind.
                () = shutdown::wait() => Ok(false),
                update = updater::check_and_schedule(&config, deep_link.as_deref()) => update,
            }
        });
        let result = match update {
            Ok(true) => Ok(()),
            Ok(false) => runtime.block_on(run_session(config, &mut notices)),
            Err(error) => {
                tracing::warn!(error = ?error, "client update check failed; continuing with this launch");
                runtime.block_on(run_session(config, &mut notices))
            }
        };
        for (title, notice) in notices {
            platform::show_notice(title, &notice);
        }
        result
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("MeshRMM remote-client supports Windows and macOS")
}
