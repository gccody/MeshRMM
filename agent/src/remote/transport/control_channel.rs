use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use meshrmm_protocol::{Display, DisplayId, RemoteSessionId, SessionMessage, VideoStreamId};
use tokio::sync::Notify;
use webrtc::data_channel::RTCDataChannel;

use super::apply_audio_event;
use crate::remote::audio_mode::{AudioEvent, AudioMode};

/// Decides the audio mode for viewers that never send `ViewerCapabilities`.
const AUDIO_MODE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_control_start(
    channel: Arc<RTCDataChannel>,
    open: Arc<Notify>,
    session_id: RemoteSessionId,
    displays: Vec<Display>,
    active_display_id: DisplayId,
    stream_id: VideoStreamId,
    format: meshrmm_protocol::VideoFormat,
    audio_mode: Arc<tokio::sync::watch::Sender<AudioMode>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        open.notified().await;
        let message = SessionMessage::DisplayConfiguration {
            displays,
            active_display_id,
            stream_id,
            format,
        };
        if let Err(error) = send_control_message(&channel, message).await {
            tracing::warn!(error = %error, %session_id, "failed to send stream configuration");
            return;
        }
        let device = SessionMessage::DeviceState {
            platform: meshrmm_protocol::DevicePlatform::current(),
            safe_mode: crate::power::booted_in_safe_mode(),
        };
        if let Err(error) = send_control_message(&channel, device).await {
            tracing::warn!(error = %error, %session_id, "failed to send the device state");
            return;
        }
        // Viewers answer this configuration with their audio preference and
        // capabilities. Very old viewers send neither; give them audio anyway.
        tokio::time::sleep(AUDIO_MODE_BACKSTOP).await;
        apply_audio_event(&audio_mode, AudioEvent::Backstop);
    })
}

pub(super) async fn send_control_message(
    channel: &RTCDataChannel,
    message: SessionMessage,
) -> anyhow::Result<()> {
    let bytes = message
        .encode()
        .context("failed to encode remote control message")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        channel.send(&Bytes::from(bytes)),
    )
    .await
    .context("remote control write timed out")?
    .context("failed to send remote control message")?;
    Ok(())
}
