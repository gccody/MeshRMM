use std::sync::{Arc, Mutex};
use std::time::Duration;

use meshrmm_protocol::{AgentSessionRequest, QualityPreset};
use meshrmm_signaling_client::{ReconnectBackoff, is_terminal_websocket_error};

use super::config::{Config, ExecutionMode};
use super::platform::{PlatformScreenStreamer, ScreenStreamer};
use super::sender_progress::SenderProgress;
use super::session_close::SessionClose;
use super::signaling::session_signal_url;

pub async fn run(
    config: &Config,
    request: AgentSessionRequest,
    mode: ExecutionMode,
    session_close: Arc<SessionClose>,
) -> anyhow::Result<()> {
    let session_id = request.session_id.clone();
    let signal_url = session_signal_url(config.server.as_str(), session_id.as_str(), "agent")?;
    tracing::info!(session_id = %session_id, "remote session requested");
    if let Some(approval) = super::connection_approval::ConnectionApproval::for_request(&request)
        && !super::connection_approval::obtain(
            &approval,
            &signal_url,
            &request.signaling_token,
            mode,
        )
        .await?
    {
        tracing::info!(session_id = %session_id, "the user declined the remote session");
        return Ok(());
    }
    let bitrate_bits_per_second =
        QualityPreset::BestQuality.bitrate(config.bitrate_bits_per_second);
    #[cfg(target_os = "macos")]
    let streamer: Arc<Mutex<Box<dyn ScreenStreamer>>> = {
        // A Mac has no private desktop to work on without the user seeing it.
        anyhow::ensure!(
            !request.start_in_background,
            "background sessions are not available on macOS"
        );
        let notification =
            super::connection_notification::ConnectionNotification::for_request(&request)
                .filter(|notification| notification.allowed(false) && notification.pending());
        if let Some(notification) = &notification {
            notification.mark_shown();
        }
        Arc::new(Mutex::new(Box::new(PlatformScreenStreamer::new(
            config.frames_per_second,
            bitrate_bits_per_second,
            mode == ExecutionMode::Service,
            super::macos::SessionUi {
                viewer_name: request.viewer_name.clone(),
                show_banner: request.session_banner,
                notification: notification.map(|notification| notification.text().to_owned()),
                blackout_message: meshrmm_protocol::render_blackout_message(
                    &request.blackout_message,
                    &request.viewer_name,
                ),
            },
        )?)))
    };
    #[cfg(windows)]
    let streamer: Arc<Mutex<Box<dyn ScreenStreamer>>> =
        Arc::new(Mutex::new(Box::new(PlatformScreenStreamer::new(
            config.frames_per_second,
            bitrate_bits_per_second,
            mode == ExecutionMode::Worker,
            request.viewer_name.clone(),
            meshrmm_protocol::render_blackout_message(
                &request.blackout_message,
                &request.viewer_name,
            ),
            request.session_banner,
            super::connection_notification::ConnectionNotification::for_request(&request),
            config
                .config_path
                .with_file_name("autofill-credentials.dat"),
        ))));
    let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15));
    let progress = SenderProgress::default();
    loop {
        match super::transport::run_sender(
            signal_url.clone(),
            request.signaling_token.as_str(),
            request.ice_servers.clone(),
            Arc::clone(&streamer),
            session_id.clone(),
            request.idle_policy,
            request.start_in_background,
            Arc::clone(&session_close),
            &progress,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if is_terminal_websocket_error(&error) => {
                // The server revoked or expired the session, so it cannot resume.
                session_close.run(&session_id);
                return Err(error);
            }
            Err(error)
                if error
                    .downcast_ref::<meshrmm_session_transport::identity::IdentityError>()
                    .is_some() =>
            {
                return Err(error);
            }
            Err(error) => {
                // A stable connection restarts the schedule. Per viewer resume
                // the server restarts this task, so this matters only when the
                // sender fails without one.
                let streamed_for = progress.take().map(|since| since.elapsed());
                let delay = backoff.delay_after(streamed_for);
                tracing::warn!(
                    error = ?error,
                    session_id = %session_id,
                    retry_seconds = delay.as_secs(),
                    streamed_seconds = streamed_for.map(|streamed| streamed.as_secs()),
                    "remote sender disconnected; waiting to resume"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}
