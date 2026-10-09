use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use meshrmm_protocol::{
    AudioFormat, CONTROL_CHANNEL_LABEL, CONTROL_CHANNEL_PROTOCOL, ChromaMode, DisplayId,
    HeadlessResolution, IceServer, QualityPreset, RemoteSessionId, SessionMessage, SessionState,
    SignalMessage, VideoProfile, VideoStreamId,
};
use meshrmm_session_transport::{
    CHAT_CHANNEL, CLIPBOARD_CHANNEL, FILE_CHANNEL, ServiceChannel, ServiceRoute,
};
use meshrmm_signaling_client::SessionSignaling;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use url::Url;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::{
    RTCPeerConnection, peer_connection_state::RTCPeerConnectionState,
    sdp::session_description::RTCSessionDescription,
};

use super::audio_mode::{AudioEvent, AudioMode, buffered_audio_limit};
use super::bitrate::EncoderStatus;
use super::platform::ScreenStreamer;
use super::sender_failure::{failure_signal, transport_failure};
use super::sender_progress::SenderProgress;
use super::session_close::SessionClose;
use super::video::LatestFrameSlot;

mod capture_control;
mod control_channel;
mod peer;
#[cfg(test)]
mod service_isolation_tests;
mod service_workers;
#[cfg(test)]
mod tests;
mod video_sender;

use capture_control::run_capture_control;
use control_channel::{send_control_message, spawn_control_start};
use peer::{create_peer, log_network_stats};
use service_workers::{
    spawn_chat_worker, spawn_clipboard_worker, spawn_file_worker, spawn_input_worker,
};
use video_sender::spawn_video_sender;

// Cancellation and startup errors must release resources just like normal teardown.
struct SenderCleanup {
    peer: Arc<RTCPeerConnection>,
    capture: Option<super::native_task::NativeTask>,
    workers: Vec<super::native_task::NativeTask>,
    tasks: Vec<tokio::task::AbortHandle>,
    closed: bool,
}

impl Drop for SenderCleanup {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if self.closed {
            return;
        }
        let peer = Arc::clone(&self.peer);
        tokio::spawn(async move {
            let _ = peer.close().await;
        });
    }
}

// Recordings persist encoded frames, so their pointer must be composed by capture.
fn capture_cursor_for_session(
    show_cursor: bool,
    viewer_controls_input: bool,
    recording: bool,
) -> bool {
    recording || (show_cursor && !viewer_controls_input)
}

enum ControlCommand {
    MaintenanceError(String),
    Keyframe,
    Bitrate(u32),
    /// Restart an encoder that cannot change bitrate live (HEVC).
    RestartBitrate(u32),
    ViewerCapabilities {
        profiles: Vec<VideoProfile>,
        quality: QualityPreset,
        chroma: ChromaMode,
        headless_resolution: HeadlessResolution,
    },
    Quality(QualityPreset),
    HeadlessResolution(HeadlessResolution),
    Chroma(ChromaMode),
    CursorCapture(bool),
    Recording(bool),
    DisplayBorder(bool),
    InputOwnership(bool),
    VideoProfileRejected {
        profile: VideoProfile,
        reason: String,
    },
    SelectDisplay(DisplayId),
    ChannelClosed,
    Stop,
}

struct CaptureStartup {
    quality_ceiling: Arc<AtomicU32>,
    encoder_status: Arc<EncoderStatus>,
    initial_display: Option<DisplayId>,
    session_close: Arc<SessionClose>,
}

const DISCONNECTED_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);
const AUDIO_STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// Without new audio for this long the stream counts as idle: WASAPI
/// loopback delivers nothing while the device is silent.
const AUDIO_IDLE: std::time::Duration = std::time::Duration::from_millis(100);
/// Audio formats this Agent can send.
const SUPPORTED_AUDIO_FORMATS: &[AudioFormat] = &[AudioFormat::Opus, AudioFormat::Pcm16];

fn apply_audio_event(mode: &tokio::sync::watch::Sender<AudioMode>, event: AudioEvent<'_>) {
    mode.send_if_modified(|mode| {
        let next = mode.next(event, SUPPORTED_AUDIO_FORMATS);
        if next == *mode {
            return false;
        }
        tracing::info!(previous = ?*mode, mode = ?next, "remote audio mode changed");
        *mode = next;
        true
    });
}

// Session-scoped state is owned by the caller so it survives sender reconnects.
#[allow(clippy::too_many_arguments)]
pub async fn run_sender(
    signal_url: Url,
    signaling_token: &str,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    idle_policy: meshrmm_protocol::TogglePolicy,
    start_in_background: bool,
    session_close: Arc<SessionClose>,
    progress: &SenderProgress,
) -> anyhow::Result<()> {
    let mut signal = SessionSignaling::connect(signal_url, signaling_token.to_owned()).await?;
    let mut failure_reported = false;
    let result = run_connected_sender(
        &mut signal,
        ice_servers,
        streamer,
        session_id,
        &mut failure_reported,
        idle_policy,
        start_in_background,
        session_close,
        progress,
    )
    .await;
    if let Err(error) = &result
        && !failure_reported
    {
        report_sender_failure(&mut signal, error).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_connected_sender(
    signal: &mut SessionSignaling,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    failure_reported: &mut bool,
    idle_policy: meshrmm_protocol::TogglePolicy,
    start_in_background: bool,
    session_close: Arc<SessionClose>,
    progress: &SenderProgress,
) -> anyhow::Result<()> {
    // Input has its own synchronized controller so capture startup, encoder
    // recovery, and video teardown never hold the path used by control events.
    let input = lock_streamer(&streamer)?.input_controller();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlCommand>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
    let (video_failure_tx, mut video_failure_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let identity = meshrmm_session_transport::identity::PeerIdentity::load(
        &crate::installer::identity_directory()?,
    )?;
    let peer = create_peer(
        &ice_servers,
        outgoing_tx.clone(),
        state_tx,
        identity.certificate.clone(),
    )
    .await?;
    let mut cleanup = SenderCleanup {
        peer: Arc::clone(&peer),
        capture: None,
        workers: Vec::new(),
        tasks: Vec::new(),
        closed: false,
    };

    let audio_channel = peer
        .create_data_channel(
            meshrmm_audio::CHANNEL,
            Some(RTCDataChannelInit {
                ordered: Some(true),
                max_retransmits: Some(0),
                protocol: Some(meshrmm_audio::PROTOCOL.into()),
                ..Default::default()
            }),
        )
        .await?;
    // Created up front: the offer is made before the viewer's version is known.
    let opus_channel = peer
        .create_data_channel(
            meshrmm_audio::OPUS_CHANNEL,
            Some(RTCDataChannelInit {
                ordered: Some(true),
                max_retransmits: Some(0),
                protocol: Some(meshrmm_audio::OPUS_PROTOCOL.into()),
                ..Default::default()
            }),
        )
        .await?;
    // The viewer decides whether and how audio is sent; capture only then.
    let audio_mode = Arc::new(tokio::sync::watch::Sender::new(AudioMode::Undetermined));
    // The audio bitrate being streamed now, left out of video pacing.
    let audio_bits = Arc::new(AtomicU32::new(0));
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);
    let audio_input = Arc::clone(&input);
    let mut capture_mode = audio_mode.subscribe();
    let audio_capture = super::native_task::NativeTask::spawn(
        "meshrmm-audio-capture",
        move |mut stop| async move {
            let mut stream: Option<Box<dyn super::platform::AudioStream>> = None;
            let mut retry = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    // Start or stop at once when the viewer mutes or unmutes.
                    Ok(()) = capture_mode.changed() => {}
                    _ = retry.tick() => {}
                }
                let mode = *capture_mode.borrow_and_update();
                if !mode.captures() || !audio_input.is_console_session() {
                    if stream.take().is_some() {
                        tracing::info!(?mode, "system audio capture stopped");
                    }
                    continue;
                }
                if stream.as_ref().is_some_and(|stream| stream.healthy()) {
                    continue;
                }
                stream = None;
                let sender = audio_tx.clone();
                match audio_input.start_audio(Box::new(move |packet| {
                    let _ = sender.try_send(packet);
                })) {
                    Ok(capture) => stream = Some(capture),
                    Err(error) => tracing::debug!(%error, "system audio unavailable; retrying"),
                }
            }
        },
    )?;
    cleanup.workers.push(audio_capture);
    let audio_input = Arc::clone(&input);
    let sender_mode = audio_mode.subscribe();
    let sender_bits = Arc::clone(&audio_bits);
    let audio_sender = tokio::spawn(async move {
        let mut opus = None::<meshrmm_audio::OpusEncoder>;
        let mut opus_unavailable = false;
        let mut bytes_sent = 0_u64;
        let mut packets_dropped = 0_u64;
        let mut stats_started = tokio::time::Instant::now();
        'send: loop {
            let packet = tokio::select! {
                packet = audio_rx.recv() => match packet {
                    Some(packet) => packet,
                    None => break,
                },
                _ = tokio::time::sleep(AUDIO_IDLE) => {
                    sender_bits.store(0, Ordering::Relaxed);
                    // Encode what follows the gap as a new stream.
                    if let Some(encoder) = opus.as_mut() {
                        encoder.reset();
                    }
                    if bytes_sent == 0 {
                        stats_started = tokio::time::Instant::now();
                    }
                    continue;
                }
            };
            let mode = *sender_mode.borrow();
            if !mode.captures() || !audio_input.is_console_session() {
                sender_bits.store(0, Ordering::Relaxed);
                continue;
            }
            if mode == AudioMode::Opus && opus.is_none() && !opus_unavailable {
                match meshrmm_audio::OpusEncoder::new() {
                    Ok(encoder) => opus = Some(encoder),
                    Err(error) => {
                        // The viewer that asked for Opus also plays PCM.
                        tracing::warn!(%error, "Opus encoder unavailable; sending PCM audio");
                        opus_unavailable = true;
                    }
                }
            }
            let (format, channel, packets, bits) =
                match opus.as_mut().filter(|_| mode == AudioMode::Opus) {
                    Some(encoder) => match encoder.encode(&packet) {
                        Ok(packets) => (
                            AudioFormat::Opus,
                            &opus_channel,
                            packets,
                            meshrmm_audio::OPUS_BITS_PER_SECOND,
                        ),
                        Err(error) => {
                            tracing::debug!(%error, "discarding audio the Opus encoder rejected");
                            continue;
                        }
                    },
                    None => {
                        if let Some(encoder) = opus.as_mut() {
                            encoder.reset();
                        }
                        let Some(bits) = meshrmm_audio::pcm_bits_per_second(&packet) else {
                            continue;
                        };
                        (AudioFormat::Pcm16, &audio_channel, vec![packet], bits)
                    }
                };
            if channel.ready_state() != RTCDataChannelState::Open {
                sender_bits.store(0, Ordering::Relaxed);
                continue;
            }
            sender_bits.store(bits, Ordering::Relaxed);
            for packet in packets {
                if channel.buffered_amount().await >= buffered_audio_limit(bits) {
                    packets_dropped += 1;
                    continue;
                }
                bytes_sent += packet.len() as u64;
                if channel.send(&Bytes::from(packet)).await.is_err() {
                    break 'send;
                }
            }
            if stats_started.elapsed() >= AUDIO_STATS_INTERVAL {
                tracing::info!(
                    ?mode,
                    ?format,
                    nominal_bits_per_second = bits,
                    audio_bits_per_second =
                        bytes_sent as f64 * 8.0 / stats_started.elapsed().as_secs_f64(),
                    packets_dropped,
                    "audio transport statistics"
                );
                bytes_sent = 0;
                packets_dropped = 0;
                stats_started = tokio::time::Instant::now();
            }
        }
        sender_bits.store(0, Ordering::Relaxed);
    });
    cleanup.tasks.push(audio_sender.abort_handle());

    let video_open = Arc::new(Notify::new());
    let control_open = Arc::new(Notify::new());
    let decoder_ready = Arc::new(Notify::new());
    let video_channel = peer
        .create_data_channel(
            "meshrmm-video-v1",
            Some(RTCDataChannelInit {
                // Video must not sit behind a lost SCTP message. Frame IDs and
                // keyframe recovery already protect the predictive chain, while
                // the reliable control stream remains independently ordered.
                ordered: Some(false),
                max_retransmits: Some(1),
                protocol: Some("meshrmm.video.v1".into()),
                ..Default::default()
            }),
        )
        .await
        .context("failed to create unreliable video data channel")?;
    {
        let notify = Arc::clone(&video_open);
        video_channel.on_open(Box::new(move || {
            let notify = Arc::clone(&notify);
            Box::pin(async move { notify.notify_one() })
        }));
    }
    let control_channel = peer
        .create_data_channel(
            CONTROL_CHANNEL_LABEL,
            Some(RTCDataChannelInit {
                ordered: Some(true),
                protocol: Some(CONTROL_CHANNEL_PROTOCOL.into()),
                ..Default::default()
            }),
        )
        .await
        .context("failed to create reliable control data channel")?;
    let (input_tx, input_task) = spawn_input_worker(
        Arc::clone(&input),
        Arc::clone(&control_channel),
        control_tx.clone(),
    )?;
    cleanup.workers.push(input_task);
    let maintenance_input = Arc::clone(&input);
    let cleanup_input = Arc::clone(&input);
    let maintenance_errors = control_tx.clone();
    let restart_session = session_id.clone();
    let (maintenance_tx, maintenance_task) = super::native_task::command_worker(
        "meshrmm-maintenance",
        32,
        std::time::Duration::from_secs(3600),
        move |message| {
            let result = match message {
                Some(
                    message @ (SessionMessage::PromptForCredentials
                    | SessionMessage::AutofillCredentials
                    | SessionMessage::ForgetCredentials),
                ) => maintenance_input.credential_command(message),
                Some(SessionMessage::SendSecureAttention) => {
                    if !maintenance_input.is_console_session() {
                        Err(anyhow::anyhow!(
                            "Ctrl+Alt+Del is only available for the console session"
                        ))
                    } else {
                        super::secure_attention::send()
                    }
                }
                Some(SessionMessage::SetPreventIdleLock { enabled }) => {
                    maintenance_input.set_prevent_idle_lock(idle_policy.effective(Some(enabled)))
                }
                Some(SessionMessage::SetWallpaperHidden { hidden }) => {
                    maintenance_input.set_wallpaper_hidden(hidden)
                }
                Some(SessionMessage::SetBlackout { enabled }) => {
                    maintenance_input.set_blackout(enabled)
                }
                Some(SessionMessage::SetAgentInputBlocked { blocked }) => {
                    maintenance_input.set_agent_input_blocked(blocked)
                }
                Some(SessionMessage::Restart { safe_mode }) => {
                    super::connection_approval::remember_across_restart(&restart_session)
                        .and_then(|()| crate::power::restart(safe_mode))
                        .context("Restart")
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                let _ =
                    maintenance_errors.send(ControlCommand::MaintenanceError(format!("{error:#}")));
            }
        },
        move || {
            let _ = cleanup_input.set_prevent_idle_lock(false);
            let _ = cleanup_input.set_wallpaper_hidden(false);
            let _ = cleanup_input.set_blackout(false);
            let _ = cleanup_input.set_agent_input_blocked(false);
        },
    )?;
    cleanup.workers.push(maintenance_task);
    let annotation_input = Arc::clone(&input);
    let cleanup_annotations = Arc::clone(&input);
    let annotation_errors = control_tx.clone();
    // Its own queue: a slow overlay never holds up input or maintenance.
    let (annotation_tx, annotation_task) = super::native_task::command_worker(
        "meshrmm-annotation",
        1024,
        std::time::Duration::from_secs(3600),
        move |annotation| {
            if let Some(annotation) = annotation
                && let Err(error) = annotation_input.annotate(annotation)
            {
                let _ = annotation_errors.send(ControlCommand::MaintenanceError(format!(
                    "Annotate: {error:#}"
                )));
            }
        },
        move || {
            let _ = cleanup_annotations.annotate(meshrmm_protocol::Annotation::Clear);
        },
    )?;
    cleanup.workers.push(annotation_task);
    let control_service = ServiceChannel::new(control_channel.clone()).await;
    let clipboard_route = Arc::new(ServiceRoute::default());
    let file_route = Arc::new(ServiceRoute::default());
    let chat_route = Arc::new(ServiceRoute::default());
    let (clipboard_tx, clipboard_task) = spawn_clipboard_worker(
        Arc::clone(&input),
        control_service.clone(),
        Some(clipboard_route.clone()),
    )?;
    cleanup.workers.push(clipboard_task);
    let (chat_tx, chat_task) = spawn_chat_worker(
        Arc::clone(&input),
        control_service.clone(),
        Some(chat_route.clone()),
    )?;
    cleanup.workers.push(chat_task);
    let (files_tx, files_task) = spawn_file_worker(
        Arc::clone(&input),
        control_service.clone(),
        Some(file_route.clone()),
    )?;
    cleanup.workers.push(files_task);
    {
        let notify = Arc::clone(&control_open);
        let decoder_ready = Arc::clone(&decoder_ready);
        let closed_tx = control_tx.clone();
        let opening = control_service.notifier();
        let closing = control_service.notifier();
        let initial_idle = maintenance_tx.clone();
        control_channel.on_open(Box::new(move || {
            let _ = initial_idle.try_send(SessionMessage::SetPreventIdleLock {
                enabled: idle_policy.enabled,
            });
            opening.notify_waiters();
            let notify = Arc::clone(&notify);
            Box::pin(async move { notify.notify_one() })
        }));
        control_channel.on_close(Box::new(move || {
            closing.notify_waiters();
            let closed_tx = closed_tx.clone();
            Box::pin(async move {
                let _ = closed_tx.send(ControlCommand::ChannelClosed);
            })
        }));
        let control_messages_tx = control_tx.clone();
        let input_tx = input_tx.clone();
        let files_tx = files_tx.clone();
        let chat_tx = chat_tx.clone();
        let clipboard_tx = clipboard_tx.clone();
        let maintenance_tx = maintenance_tx.clone();
        let annotation_tx = annotation_tx.clone();
        let message_session_close = Arc::clone(&session_close);
        let message_audio_mode = Arc::clone(&audio_mode);
        control_channel.on_message(Box::new(move |message| {
            let session_close = Arc::clone(&message_session_close);
            let audio_mode = Arc::clone(&message_audio_mode);
            let tx = control_messages_tx.clone();
            let decoder_ready = Arc::clone(&decoder_ready);
            let input_tx = input_tx.clone();
            let files_tx = files_tx.clone();
            let chat_tx = chat_tx.clone();
            let clipboard_tx = clipboard_tx.clone();
            let maintenance_tx = maintenance_tx.clone();
            let annotation_tx = annotation_tx.clone();
            Box::pin(async move {
                let command = match SessionMessage::decode(&message.data) {
                    Ok(SessionMessage::RequestKeyframe { .. }) => {
                        decoder_ready.notify_one();
                        Some(ControlCommand::Keyframe)
                    }
                    Ok(SessionMessage::SetBitrate { bits_per_second }) => {
                        Some(ControlCommand::Bitrate(bits_per_second))
                    }
                    Ok(SessionMessage::ViewerCapabilities {
                        profiles,
                        quality,
                        chroma,
                        headless_resolution,
                    }) => {
                        apply_audio_event(&audio_mode, AudioEvent::ViewerCapabilities);
                        Some(ControlCommand::ViewerCapabilities {
                            profiles,
                            quality,
                            chroma,
                            headless_resolution,
                        })
                    }
                    Ok(SessionMessage::SetHeadlessResolution { resolution }) => {
                        Some(ControlCommand::HeadlessResolution(resolution))
                    }
                    Ok(SessionMessage::SetAudio { enabled, formats }) => {
                        apply_audio_event(
                            &audio_mode,
                            AudioEvent::SetAudio {
                                enabled,
                                formats: &formats,
                            },
                        );
                        None
                    }
                    Ok(SessionMessage::SetQuality { preset }) => {
                        Some(ControlCommand::Quality(preset))
                    }
                    Ok(SessionMessage::SetChroma { mode }) => Some(ControlCommand::Chroma(mode)),
                    Ok(SessionMessage::SetDisplayBorder { enabled }) => {
                        Some(ControlCommand::DisplayBorder(enabled))
                    }
                    Ok(SessionMessage::SetRecording { enabled }) => {
                        Some(ControlCommand::Recording(enabled))
                    }
                    Ok(SessionMessage::SetCursorCapture { enabled }) => {
                        Some(ControlCommand::CursorCapture(enabled))
                    }
                    Ok(SessionMessage::VideoProfileRejected { profile, reason }) => {
                        Some(ControlCommand::VideoProfileRejected { profile, reason })
                    }
                    Ok(SessionMessage::SelectDisplay { display_id }) => {
                        Some(ControlCommand::SelectDisplay(display_id))
                    }
                    Ok(
                        message @ (SessionMessage::SendSecureAttention
                        | SessionMessage::PromptForCredentials
                        | SessionMessage::AutofillCredentials
                        | SessionMessage::ForgetCredentials
                        | SessionMessage::SetWallpaperHidden { .. }
                        | SessionMessage::SetPreventIdleLock { .. }
                        | SessionMessage::SetBlackout { .. }
                        | SessionMessage::SetAgentInputBlocked { .. }
                        | SessionMessage::Restart { .. }),
                    ) => maintenance_tx.try_send(message).err().map(|_| {
                        ControlCommand::MaintenanceError(
                            "maintenance command queue full or closed".into(),
                        )
                    }),
                    Ok(SessionMessage::Annotate(annotation)) => {
                        annotation_tx.try_send(annotation).err().map(|_| {
                            ControlCommand::MaintenanceError(
                                "annotation queue full or closed".into(),
                            )
                        })
                    }
                    Ok(SessionMessage::Input(event)) => {
                        if input_tx.try_send(event).is_err() {
                            // Never silently drop a key-up. End the session so
                            // the input worker releases all pressed keys.
                            Some(ControlCommand::Stop)
                        } else {
                            None
                        }
                    }
                    Ok(
                        message @ (SessionMessage::Clipboard { .. }
                        | SessionMessage::ClipboardChunk { .. }),
                    ) => clipboard_tx.try_send(message).err().map(|_| {
                        ControlCommand::MaintenanceError("clipboard queue full or closed".into())
                    }),
                    Ok(SessionMessage::FileTransfer(message)) => {
                        files_tx.try_send(message).err().map(|_| {
                            ControlCommand::MaintenanceError(
                                "file-transfer queue full or closed".into(),
                            )
                        })
                    }
                    Ok(message @ (SessionMessage::ChatAvailable | SessionMessage::Chat { .. })) => {
                        chat_tx.try_send(message).err().map(|_| {
                            ControlCommand::MaintenanceError("chat queue full or closed".into())
                        })
                    }
                    Ok(SessionMessage::SetSessionCloseAction { action }) => {
                        session_close.set_action(action);
                        None
                    }
                    Ok(SessionMessage::SetClearClipboardOnClose { enabled }) => {
                        session_close.set_clear_clipboard(enabled);
                        None
                    }
                    Ok(SessionMessage::Stop { .. }) => Some(ControlCommand::Stop),
                    Ok(_) => None,
                    Err(error) => {
                        tracing::warn!(error = %error, "discarding invalid control message");
                        None
                    }
                };
                if let Some(command) = command {
                    let _ = tx.send(command);
                }
            })
        }));
    }

    for (label, route) in [
        (CLIPBOARD_CHANNEL, clipboard_route),
        (FILE_CHANNEL, file_route),
        (CHAT_CHANNEL, chat_route),
    ] {
        let channel = peer
            .create_data_channel(
                label,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    protocol: Some(label.into()),
                    ..Default::default()
                }),
            )
            .await?;
        let channel = ServiceChannel::new(channel).await;
        route.attach(channel.clone());
        let clipboard = clipboard_tx.clone();
        let files = files_tx.clone();
        let chat = chat_tx.clone();
        let errors = control_tx.clone();
        channel.on_message(Box::new(move |message| {
            let route = route.clone();
            let clipboard = clipboard.clone();
            let files = files.clone();
            let chat = chat.clone();
            let errors = errors.clone();
            Box::pin(async move {
                match SessionMessage::decode(&message.data) {
                    Ok(SessionMessage::ServiceChannelReady) => route.peer_ready(),
                    Ok(message)
                        if meshrmm_session_transport::service_label(&message) == Some(label) =>
                    {
                        let accepted = match message {
                            SessionMessage::FileTransfer(message) => {
                                files.try_send(message).is_ok()
                            }
                            message @ (SessionMessage::Chat { .. }
                            | SessionMessage::ChatAvailable) => chat.try_send(message).is_ok(),
                            message => clipboard.try_send(message).is_ok(),
                        };
                        if !accepted {
                            let _ = errors.send(ControlCommand::MaintenanceError(format!(
                                "{label} queue full or closed"
                            )));
                        }
                    }
                    _ => tracing::warn!(label, "discarding invalid service-channel message"),
                }
            })
        }));
        meshrmm_session_transport::announce(channel);
    }

    let slot = Arc::new(LatestFrameSlot::default());
    let stream_id = VideoStreamId(1);
    let quality_ceiling = Arc::new(AtomicU32::new(1));
    let encoder_status = Arc::new(EncoderStatus::default());
    let (capture_tx, capture_rx) = mpsc::channel(64);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let capture_streamer = Arc::clone(&streamer);
    let capture_slot = Arc::clone(&slot);
    let capture_channel = Arc::clone(&control_channel);
    let capture_ceiling = Arc::clone(&quality_ceiling);
    let capture_encoder_status = Arc::clone(&encoder_status);
    let capture_failure = video_failure_tx.clone();
    let capture_task =
        super::native_task::NativeTask::spawn("meshrmm-capture-control", move |stop| async move {
            let result = run_capture_control(
                capture_streamer.clone(),
                capture_slot,
                capture_channel,
                CaptureStartup {
                    quality_ceiling: capture_ceiling,
                    encoder_status: capture_encoder_status,
                    initial_display: start_in_background
                        .then_some(DisplayId(meshrmm_protocol::BACKGROUND_DISPLAY_ID.0)),
                    session_close,
                },
                capture_rx,
                started_tx,
                stop,
            )
            .await;
            if let Err(error) = result {
                let _ = capture_failure.send(error.context("capture worker"));
            }
            if let Err(error) = lock_streamer(&capture_streamer).and_then(|mut s| s.shutdown()) {
                tracing::warn!(%error, "capture worker cleanup failed");
            }
        })?;
    cleanup.capture = Some(capture_task);
    let started = match started_rx
        .await
        .context("capture worker stopped before startup")
        .and_then(|result| result)
    {
        Ok(started) => started,
        Err(error) => {
            // Finish the old worker before a reconnect can reuse the streamer.
            // Heartbeats continue in the independent signaling pump.
            for worker in &mut cleanup.workers {
                worker.shutdown().await;
            }
            if let Some(capture) = cleanup.capture.as_mut() {
                capture.shutdown().await;
            }
            return Err(error);
        }
    };
    let displays = started.displays;
    let active_display = started.active_display;
    let format = started.format;
    let video_sender = spawn_video_sender(
        Arc::clone(&video_channel),
        Arc::clone(&video_open),
        decoder_ready,
        Arc::clone(&slot),
        control_tx.clone(),
        Arc::clone(&quality_ceiling),
        encoder_status,
        Arc::clone(&audio_bits),
        video_failure_tx,
    );
    cleanup.tasks.push(video_sender.abort_handle());
    let control_start = spawn_control_start(
        Arc::clone(&control_channel),
        Arc::clone(&control_open),
        session_id.clone(),
        displays.clone(),
        active_display.id,
        stream_id,
        format,
        Arc::clone(&audio_mode),
    );
    cleanup.tasks.push(control_start.abort_handle());

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    stats_interval.tick().await;
    let mut offer_sent = false;
    let mut remote_description_set = false;
    let mut pending_candidates = Vec::new();
    let mut disconnected_since = None::<tokio::time::Instant>;
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
            Some(outgoing) = outgoing_rx.recv() => {
                let json = serde_json::to_string(&outgoing)?;
                signal.send(Message::Text(json.into())).await?;
            }
            incoming = signal.next() => {
                let Some(incoming) = incoming else { break Err(transport_failure("signaling connection closed")); };
                let Message::Text(text) = incoming? else { continue; };
                let signal: SignalMessage = serde_json::from_str(text.as_str())?;
                match signal {
                    SignalMessage::Ready if !offer_sent => {
                        let offer = peer.create_offer(None).await?;
                        peer.set_local_description(offer).await?;
                        let local = peer.local_description().await
                            .ok_or_else(|| anyhow::anyhow!("WebRTC did not retain its local offer"))?;
                        outgoing_tx.send(SignalMessage::Offer { sdp: local.sdp })?;
                        offer_sent = true;
                    }
                    SignalMessage::Answer { sdp } => {
                        identity.verify_sdp(&sdp)?;
                        peer.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
                        remote_description_set = true;
                        for candidate in pending_candidates.drain(..) {
                            peer.add_ice_candidate(candidate).await?;
                        }
                    }
                    SignalMessage::IceCandidate { candidate, sdp_mid, sdp_mline_index, username_fragment } => {
                        let candidate = RTCIceCandidateInit { candidate, sdp_mid, sdp_mline_index, username_fragment };
                        if remote_description_set {
                            peer.add_ice_candidate(candidate).await?;
                        } else {
                            if pending_candidates.len() >= 256 { anyhow::bail!("too many pending ICE candidates"); }
                            pending_candidates.push(candidate);
                        }
                    }
                    SignalMessage::PeerLeft => {
                        break Err(transport_failure("viewer disconnected from the remote session"));
                    }
                    SignalMessage::Error { message, .. } => break Err(anyhow::anyhow!(message)),
                    _ => {}
                }
            }
            Some(command) = control_rx.recv() => {
                match command {
                    command @ (ControlCommand::Keyframe | ControlCommand::Bitrate(_) | ControlCommand::RestartBitrate(_)
                        | ControlCommand::Quality(_) | ControlCommand::ViewerCapabilities { .. } | ControlCommand::HeadlessResolution(_)
                        | ControlCommand::DisplayBorder(_) | ControlCommand::Chroma(_) | ControlCommand::CursorCapture(_) | ControlCommand::InputOwnership(_) | ControlCommand::Recording(_) | ControlCommand::VideoProfileRejected { .. }
                        | ControlCommand::SelectDisplay(_)) => {
                        capture_tx.try_send(command).map_err(|_| anyhow::anyhow!("capture command queue full or closed"))?;
                    }
                    ControlCommand::MaintenanceError(reason) => {
                        send_control_message(&control_channel, SessionMessage::MaintenanceError { reason }).await?;
                    }
                    ControlCommand::Stop => break Ok(()),
                    ControlCommand::ChannelClosed => {
                        // The viewer closes its old channels before resuming the
                        // session. End this sender quietly so its expected
                        // teardown cannot surface as a fatal error in the new
                        // connection.
                        tracing::info!("viewer control channel closed; awaiting session resume");
                        break Ok(());
                    }
                }
            }
            Some(state) = state_rx.recv() => {
                tracing::info!(?state, session_id = %session_id, "WebRTC connection state changed");
                if state == RTCPeerConnectionState::Connected
                    && session_state == SessionState::Connecting
                {
                    session_state = session_state.transition(SessionState::Streaming)?;
                    progress.mark_streaming(std::time::Instant::now());
                }
                if state == RTCPeerConnectionState::Connected {
                    // The session no longer depends on signaling.
                    signal.peer_connected();
                    disconnected_since = None;
                } else if state == RTCPeerConnectionState::Disconnected {
                    disconnected_since.get_or_insert_with(tokio::time::Instant::now);
                }
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                    break Err(transport_failure(format!("WebRTC connection ended in state {state:?}")));
                }
            }
            Some(error) = video_failure_rx.recv() => break Err(error),
            _ = stats_interval.tick() => {
                if disconnected_since.is_some_and(|since| since.elapsed() >= DISCONNECTED_GRACE_PERIOD) {
                    break Err(transport_failure(format!(
                        "WebRTC remained disconnected for {} seconds",
                        DISCONNECTED_GRACE_PERIOD.as_secs()
                    )));
                }
                log_network_stats(&peer).await;
            },
            }
        }
    }
    .await;

    if let Err(error) = &result {
        report_sender_failure(signal, error).await;
        *failure_reported = true;
    }
    video_sender.abort();
    let _ = video_sender.await;
    control_start.abort();
    let _ = control_start.await;
    for worker in &mut cleanup.workers {
        worker.shutdown().await;
    }
    if matches!(
        session_state,
        SessionState::Requested
            | SessionState::Signaling
            | SessionState::Connecting
            | SessionState::Streaming
    ) {
        session_state = session_state.transition(SessionState::Closing)?;
    }
    let mut result = result;
    if let Some(capture) = cleanup.capture.as_mut() {
        capture.shutdown().await;
    }
    if let Err(error) = peer.close().await {
        tracing::warn!(error = %error, "WebRTC peer did not close cleanly");
        if result.is_ok() {
            result = Err(error).context("failed to close WebRTC peer");
        }
    }
    cleanup.closed = true;
    session_state = session_state.transition(SessionState::Idle)?;
    tracing::info!(
        session_id = %session_id,
        ?session_state,
        encoded_frames_dropped = slot.dropped(),
        "remote sender session stopped"
    );
    result
}

async fn report_sender_failure(connection: &mut SessionSignaling, error: &anyhow::Error) {
    let Some(signal) = failure_signal(error) else {
        tracing::debug!(error = %error, "not reporting a transport failure the viewer detects itself");
        return;
    };
    match serde_json::to_string(&signal) {
        Ok(message) => {
            if let Err(send_error) = connection.send(Message::Text(message.into())).await {
                tracing::warn!(error = %send_error, "failed to report sender failure to viewer");
            }
        }
        Err(send_error) => {
            tracing::warn!(error = %send_error, "failed to encode sender failure for viewer");
        }
    }
}

fn lock_streamer(
    streamer: &Arc<Mutex<Box<dyn ScreenStreamer>>>,
) -> anyhow::Result<std::sync::MutexGuard<'_, Box<dyn ScreenStreamer>>> {
    streamer
        .lock()
        .map_err(|_| anyhow::anyhow!("screen streamer lock is poisoned"))
}
