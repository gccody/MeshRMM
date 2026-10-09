use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use meshrmm_protocol::{
    Display, DisplayId, RemoteSessionId, SessionMessage, TogglePolicy, VideoStreamId,
};
use meshrmm_session_transport::ServiceChannel;
use tokio::sync::{Notify, mpsc, watch};
use webrtc::data_channel::RTCDataChannel;

use super::ControlCommand;
use super::audio::apply_audio_event;
use super::service_workers::WorkerQueues;
use crate::remote::audio_mode::{AudioEvent, AudioMode};
use crate::remote::session_close::SessionClose;

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

/// Everything a control-channel message can be routed to.
#[derive(Clone)]
pub(super) struct ControlRouting {
    pub(super) queues: WorkerQueues,
    pub(super) commands: mpsc::UnboundedSender<ControlCommand>,
    pub(super) decoder_ready: Arc<Notify>,
    pub(super) session_close: Arc<SessionClose>,
    pub(super) audio_mode: Arc<watch::Sender<AudioMode>>,
}

pub(super) fn wire_control_channel(
    channel: &RTCDataChannel,
    control_service: &ServiceChannel,
    control_open: &Arc<Notify>,
    idle_policy: TogglePolicy,
    routing: ControlRouting,
) {
    let notify = Arc::clone(control_open);
    let closed_tx = routing.commands.clone();
    let opening = control_service.notifier();
    let closing = control_service.notifier();
    let initial_idle = routing.queues.maintenance.clone();
    channel.on_open(Box::new(move || {
        let _ = initial_idle.try_send(SessionMessage::SetPreventIdleLock {
            enabled: idle_policy.enabled,
        });
        opening.notify_waiters();
        let notify = Arc::clone(&notify);
        Box::pin(async move { notify.notify_one() })
    }));
    channel.on_close(Box::new(move || {
        closing.notify_waiters();
        let closed_tx = closed_tx.clone();
        Box::pin(async move {
            let _ = closed_tx.send(ControlCommand::ChannelClosed);
        })
    }));
    channel.on_message(Box::new(move |message| {
        let routing = routing.clone();
        Box::pin(async move {
            let command = match SessionMessage::decode(&message.data) {
                Ok(message) => routing.route(message),
                Err(error) => {
                    tracing::warn!(error = %error, "discarding invalid control message");
                    None
                }
            };
            if let Some(command) = command {
                let _ = routing.commands.send(command);
            }
        })
    }));
}

impl ControlRouting {
    fn route(&self, message: SessionMessage) -> Option<ControlCommand> {
        match message {
            SessionMessage::RequestKeyframe { .. } => {
                self.decoder_ready.notify_one();
                Some(ControlCommand::Keyframe)
            }
            SessionMessage::SetBitrate { bits_per_second } => {
                Some(ControlCommand::Bitrate(bits_per_second))
            }
            SessionMessage::ViewerCapabilities {
                profiles,
                quality,
                chroma,
                headless_resolution,
            } => {
                apply_audio_event(&self.audio_mode, AudioEvent::ViewerCapabilities);
                Some(ControlCommand::ViewerCapabilities {
                    profiles,
                    quality,
                    chroma,
                    headless_resolution,
                })
            }
            SessionMessage::SetHeadlessResolution { resolution } => {
                Some(ControlCommand::HeadlessResolution(resolution))
            }
            SessionMessage::SetAudio { enabled, formats } => {
                apply_audio_event(
                    &self.audio_mode,
                    AudioEvent::SetAudio {
                        enabled,
                        formats: &formats,
                    },
                );
                None
            }
            SessionMessage::SetQuality { preset } => Some(ControlCommand::Quality(preset)),
            SessionMessage::SetChroma { mode } => Some(ControlCommand::Chroma(mode)),
            SessionMessage::SetDisplayBorder { enabled } => {
                Some(ControlCommand::DisplayBorder(enabled))
            }
            SessionMessage::SetRecording { enabled } => Some(ControlCommand::Recording(enabled)),
            SessionMessage::SetCursorCapture { enabled } => {
                Some(ControlCommand::CursorCapture(enabled))
            }
            SessionMessage::VideoProfileRejected { profile, reason } => {
                Some(ControlCommand::VideoProfileRejected { profile, reason })
            }
            SessionMessage::SelectDisplay { display_id } => {
                Some(ControlCommand::SelectDisplay(display_id))
            }
            SessionMessage::SetSessionCloseAction { action } => {
                self.session_close.set_action(action);
                None
            }
            SessionMessage::SetClearClipboardOnClose { enabled } => {
                self.session_close.set_clear_clipboard(enabled);
                None
            }
            SessionMessage::Stop { .. } => Some(ControlCommand::Stop),
            message => self.enqueue(message),
        }
    }

    /// Hands a message to its worker, reporting a full or closed queue.
    fn enqueue(&self, message: SessionMessage) -> Option<ControlCommand> {
        let queues = &self.queues;
        match message {
            message @ (SessionMessage::SendSecureAttention
            | SessionMessage::PromptForCredentials
            | SessionMessage::AutofillCredentials
            | SessionMessage::ForgetCredentials
            | SessionMessage::SetWallpaperHidden { .. }
            | SessionMessage::SetPreventIdleLock { .. }
            | SessionMessage::SetBlackout { .. }
            | SessionMessage::SetAgentInputBlocked { .. }
            | SessionMessage::Restart { .. }) => {
                queues.maintenance.try_send(message).err().map(|_| {
                    ControlCommand::MaintenanceError(
                        "maintenance command queue full or closed".into(),
                    )
                })
            }
            SessionMessage::Annotate(annotation) => {
                queues.annotation.try_send(annotation).err().map(|_| {
                    ControlCommand::MaintenanceError("annotation queue full or closed".into())
                })
            }
            SessionMessage::Input(event) => {
                if queues.input.try_send(event).is_err() {
                    // Never silently drop a key-up. End the session so
                    // the input worker releases all pressed keys.
                    Some(ControlCommand::Stop)
                } else {
                    None
                }
            }
            message
            @ (SessionMessage::Clipboard { .. } | SessionMessage::ClipboardChunk { .. }) => {
                queues.clipboard.try_send(message).err().map(|_| {
                    ControlCommand::MaintenanceError("clipboard queue full or closed".into())
                })
            }
            SessionMessage::FileTransfer(message) => {
                queues.files.try_send(message).err().map(|_| {
                    ControlCommand::MaintenanceError("file-transfer queue full or closed".into())
                })
            }
            message @ (SessionMessage::ChatAvailable | SessionMessage::Chat { .. }) => queues
                .chat
                .try_send(message)
                .err()
                .map(|_| ControlCommand::MaintenanceError("chat queue full or closed".into())),
            _ => None,
        }
    }
}
