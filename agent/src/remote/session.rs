use std::sync::{Arc, Mutex};

use anyhow::Context;
use pulsermm_protocol::AgentSessionRequest;

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
    let streamer: Arc<Mutex<Box<dyn ScreenStreamer>>> =
        Arc::new(Mutex::new(Box::new(PlatformScreenStreamer::new(
            config.frames_per_second,
            config.bitrate_bits_per_second,
            mode == ExecutionMode::Worker,
        ))));
    tracing::info!(session_id = %session_id, "remote session requested");
    super::transport::run_sender(
        signal_url,
        request.signaling_token.as_str(),
        request.ice_servers,
        streamer,
        session_id,
    )
    .await
    .context("P2P sender session failed")
}
