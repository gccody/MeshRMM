use std::sync::{Arc, Mutex};
use std::time::Duration;

use meshrmm_protocol::{AgentSessionRequest, QualityPreset};
use meshrmm_signaling_client::{ReconnectBackoff, is_terminal_websocket_error};

use super::config::{Config, ExecutionMode};
use super::platform::{PlatformScreenStreamer, ScreenStreamer};
use super::signaling::session_signal_url;

pub async fn run(
    config: &Config,
    request: AgentSessionRequest,
    mode: ExecutionMode,
) -> anyhow::Result<()> {
    let session_id = request.session_id.clone();
    let signal_url = session_signal_url(config.server.as_str(), session_id.as_str(), "agent")?;
    let bitrate_bits_per_second =
        QualityPreset::BestQuality.bitrate(config.bitrate_bits_per_second);
    let streamer: Arc<Mutex<Box<dyn ScreenStreamer>>> =
        Arc::new(Mutex::new(Box::new(PlatformScreenStreamer::new(
            config.frames_per_second,
            bitrate_bits_per_second,
            mode == ExecutionMode::Worker,
        ))));
    tracing::info!(session_id = %session_id, "remote session requested");
    let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15));
    loop {
        match super::transport::run_sender(
            signal_url.clone(),
            request.signaling_token.as_str(),
            request.ice_servers.clone(),
            Arc::clone(&streamer),
            session_id.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if is_terminal_websocket_error(&error) => {
                return Err(error);
            }
            Err(error) => {
                let delay = backoff.next_delay();
                tracing::warn!(
                    error = ?error,
                    session_id = %session_id,
                    retry_seconds = delay.as_secs(),
                    "remote sender disconnected; waiting to resume"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}
