use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use meshrmm_protocol::{
    AudioFormat, CONTROL_CHANNEL_LABEL, CONTROL_CHANNEL_PROTOCOL, ChromaMode, Codec,
    DEFAULT_FRAGMENT_PAYLOAD, Display, DisplayId, IceServer, QualityPreset, RemoteSessionId,
    SessionMessage, SessionState, SignalMessage, VideoProfile, VideoStreamId, fragment_frame,
};
use meshrmm_session_transport::{
    CHAT_CHANNEL, CLIPBOARD_CHANNEL, FILE_CHANNEL, ServiceChannel, ServiceRoute,
};
use meshrmm_signaling_client::SignalingConnection;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use url::Url;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::{RTCDataChannel, data_channel_init::RTCDataChannelInit};
use webrtc::ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer};
use webrtc::peer_connection::{
    RTCPeerConnection, configuration::RTCConfiguration,
    peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription,
};
use webrtc::stats::StatsReportType;

use super::audio_mode::{AudioEvent, AudioMode, buffered_audio_limit};
use super::bitrate::{
    AdaptiveBitrate, EncoderStatus, RestartLadder, VideoPacer, video_pacing_bitrate,
};
use super::platform::{ScreenStreamer, StartedScreen, monotonic_timestamp_us};
use super::sender_failure::{
    failure_signal, initial_start_error, profile_start_error, transport_failure,
};
use super::sender_progress::SenderProgress;
use super::session_close::SessionClose;
use super::signaling::authenticated_websocket;
use super::video::LatestFrameSlot;

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
    },
    Quality(QualityPreset),
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

const VIDEO_BUFFER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(80);
const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;
const DESKTOP_LIFECYCLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const DESKTOP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const DISCONNECTED_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);
/// Decides the audio mode for viewers that never send `ViewerCapabilities`.
const AUDIO_MODE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);
const AUDIO_STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// Without new audio for this long the stream counts as idle: WASAPI
/// loopback delivers nothing while the device is silent.
const AUDIO_IDLE: std::time::Duration = std::time::Duration::from_millis(100);
/// Audio formats this Agent can send.
const SUPPORTED_AUDIO_FORMATS: &[AudioFormat] = &[AudioFormat::Pcm16];

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

fn profile_candidates(
    profiles: &[VideoProfile],
    requested_chroma: ChromaMode,
    rejected: &[VideoProfile],
) -> Vec<VideoProfile> {
    let mut candidates = Vec::new();
    for chroma in [requested_chroma, ChromaMode::Yuv420] {
        for codec in [Codec::H265, Codec::H264] {
            let profile = VideoProfile { codec, chroma };
            if profiles.contains(&profile)
                && !rejected.contains(&profile)
                && !candidates.contains(&profile)
            {
                candidates.push(profile);
            }
        }
    }
    candidates
}

fn start_first_profile(
    streamer: &Arc<Mutex<Box<dyn ScreenStreamer>>>,
    display_id: DisplayId,
    stream_id: VideoStreamId,
    slot: &Arc<LatestFrameSlot>,
    candidates: &[VideoProfile],
) -> anyhow::Result<StartedScreen> {
    let mut failures = Vec::new();
    for profile in candidates {
        let result = {
            let mut streamer = lock_streamer(streamer)?;
            streamer.set_codec(profile.codec);
            streamer.set_chroma(profile.chroma);
            streamer.start(Some(display_id), stream_id, Arc::clone(slot))
        };
        match result {
            Ok(started) => return Ok(started),
            Err(error) => {
                tracing::warn!(?profile, error = ?error, "hardware encoder profile unavailable");
                failures.push((*profile, error));
            }
        }
    }
    Err(profile_start_error(failures))
}

// Session-scoped state is owned by the caller so it survives sender reconnects.
#[allow(clippy::too_many_arguments)]
pub async fn run_sender(
    signal_url: Url,
    signaling_token: &str,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    idle_policy: meshrmm_protocol::IdlePolicy,
    start_in_background: bool,
    session_close: Arc<SessionClose>,
    progress: &SenderProgress,
) -> anyhow::Result<()> {
    let (socket, _) = authenticated_websocket(signal_url, signaling_token).await?;
    let mut signal = SignalingConnection::new(socket);
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
    signal: &mut SignalingConnection,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    failure_reported: &mut bool,
    idle_policy: meshrmm_protocol::IdlePolicy,
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
            let mut stream = None;
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
                if stream.as_ref().is_some_and(meshrmm_audio::Capture::healthy) {
                    continue;
                }
                stream = None;
                let sender = audio_tx.clone();
                match meshrmm_audio::capture(move |packet| {
                    let _ = sender.try_send(packet);
                }) {
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
        let mut bytes_sent = 0_u64;
        let mut packets_dropped = 0_u64;
        let mut stats_started = tokio::time::Instant::now();
        loop {
            let packet = tokio::select! {
                packet = audio_rx.recv() => match packet {
                    Some(packet) => packet,
                    None => break,
                },
                _ = tokio::time::sleep(AUDIO_IDLE) => {
                    sender_bits.store(0, Ordering::Relaxed);
                    if bytes_sent == 0 {
                        stats_started = tokio::time::Instant::now();
                    }
                    continue;
                }
            };
            let mode = *sender_mode.borrow();
            let Some(bits) = meshrmm_audio::pcm_bits_per_second(&packet) else {
                continue;
            };
            if !mode.captures()
                || !audio_input.is_console_session()
                || audio_channel.ready_state() != RTCDataChannelState::Open
            {
                sender_bits.store(0, Ordering::Relaxed);
                continue;
            }
            sender_bits.store(bits, Ordering::Relaxed);
            if audio_channel.buffered_amount().await >= buffered_audio_limit(bits) {
                packets_dropped += 1;
            } else {
                bytes_sent += packet.len() as u64;
                if audio_channel.send(&Bytes::from(packet)).await.is_err() {
                    break;
                }
            }
            if stats_started.elapsed() >= AUDIO_STATS_INTERVAL {
                tracing::info!(
                    ?mode,
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
                enabled: idle_policy.prevent_idle_lock,
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
                    }) => {
                        apply_audio_event(&audio_mode, AudioEvent::ViewerCapabilities);
                        Some(ControlCommand::ViewerCapabilities {
                            profiles,
                            quality,
                            chroma,
                        })
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
                        | SessionMessage::SetAgentInputBlocked { .. }),
                    ) => maintenance_tx.try_send(message).err().map(|_| {
                        ControlCommand::MaintenanceError(
                            "maintenance command queue full or closed".into(),
                        )
                    }),
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
                        .then_some(DisplayId(meshrmm_remote_screen::background::DISPLAY_ID)),
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
                match incoming? {
                    Message::Text(text) => {
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
                    Message::Ping(payload) => signal.send(Message::Pong(payload)).await?,
                    Message::Close(frame) => {
                        break Err(meshrmm_signaling_client::signaling_close_error(frame));
                    }
                    _ => {}
                }
            }
            Some(command) = control_rx.recv() => {
                match command {
                    command @ (ControlCommand::Keyframe | ControlCommand::Bitrate(_) | ControlCommand::RestartBitrate(_)
                        | ControlCommand::Quality(_) | ControlCommand::ViewerCapabilities { .. }
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

fn spawn_file_worker(
    input: Arc<dyn super::platform::ScreenInput>,
    channel: ServiceChannel,
    route: Option<Arc<ServiceRoute>>,
) -> anyhow::Result<(
    mpsc::Sender<meshrmm_protocol::FileMessage>,
    super::native_task::NativeTask,
)> {
    let (sender, mut commands) = mpsc::channel(meshrmm_file_transfer::COMMAND_QUEUE);
    let task = super::native_task::NativeTask::spawn(
        "meshrmm-files",
        move |mut stop| async move {
            let channel = if let Some(route) = route {
                tokio::select! { channel = route.resolve(channel) => match channel { Ok(channel) => channel, Err(error) => { tracing::warn!(%error, "service route unavailable"); return; } }, _ = stop.changed() => return }
            } else {
                channel
            };
            let ready = input.files_ready();
            let mut pending = true;
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        if let Err(error) = input.apply_files(message) { tracing::warn!(%error, "file helper unavailable"); }
                    }
                    _ = ready.notified() => pending = true,
                    capacity = channel.writable(), if pending && channel.ready_state() == RTCDataChannelState::Open => {
                        if let Err(error) = capacity { tracing::warn!(%error, "file channel unavailable"); break; }
                        if let Some(message) = input.poll_files() {
                            if let Err(error) = send_control_message(&channel, SessionMessage::FileTransfer(message)).await {
                                tracing::warn!(%error, "file-transfer send failed");
                                break;
                            }
                        } else { pending = false; }
                    }
                }
            }
        },
    )?;
    Ok((sender, task))
}

fn spawn_chat_worker(
    input: Arc<dyn super::platform::ScreenInput>,
    channel: ServiceChannel,
    route: Option<Arc<ServiceRoute>>,
) -> anyhow::Result<(mpsc::Sender<SessionMessage>, super::native_task::NativeTask)> {
    let (sender, mut commands) = mpsc::channel(64);
    let task = super::native_task::NativeTask::spawn("meshrmm-chat", move |mut stop| async move {
        let channel = if let Some(route) = route {
            tokio::select! { channel = route.resolve(channel) => match channel { Ok(channel) => channel, Err(error) => { tracing::warn!(%error, "service route unavailable"); return; } }, _ = stop.changed() => return }
        } else {
            channel
        };
        let ready = input.chat_ready();
        let result: anyhow::Result<()> = async {
            loop {
                if *stop.borrow() { break; }
                tokio::select! {
                    _ = stop.changed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        match message {
                            SessionMessage::ChatAvailable => {
                                input.start_chat()?;
                                send_control_message(&channel, SessionMessage::ChatAvailable).await?;
                            }
                            SessionMessage::Chat { text } => input.apply_chat(text)?,
                            _ => {},
                        }
                    }
                    _ = ready.notified() => {
                        while let Some(text) = input.poll_chat()? {
                            send_control_message(&channel, SessionMessage::Chat { text }).await?;
                        }
                    }
                }
            }
            Ok(())
        }.await;
        input.stop_chat();
        if let Err(error) = result {
            tracing::warn!(%error, "chat worker stopped");
        }
    })?;
    Ok((sender, task))
}

fn spawn_clipboard_worker(
    input: Arc<dyn super::platform::ScreenInput>,
    channel: ServiceChannel,
    route: Option<Arc<ServiceRoute>>,
) -> anyhow::Result<(mpsc::Sender<SessionMessage>, super::native_task::NativeTask)> {
    let (sender, mut commands) = mpsc::channel(1024);
    let task = super::native_task::NativeTask::spawn(
        "meshrmm-clipboard",
        move |mut stop| async move {
            let channel = if let Some(route) = route {
                tokio::select! { channel = route.resolve(channel) => match channel { Ok(channel) => channel, Err(error) => { tracing::warn!(%error, "service route unavailable"); return; } }, _ = stop.changed() => return }
            } else {
                channel
            };
            let ready = input.clipboard_ready();
            let mut receiver = meshrmm_protocol::ClipboardReceiver::default();
            let mut outgoing = std::collections::VecDeque::new();
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        match receiver.receive(message) {
                            Ok(Some(content)) => {
                                outgoing.clear();
                                if let Err(error) = input.apply_clipboard(content) { tracing::warn!(%error, "clipboard apply failed"); }
                            }
                            Ok(None) => {},
                            Err(error) => tracing::warn!(%error, "invalid clipboard payload"),
                        }
                    }
                    _ = async {
                        if let Some(ready) = &ready { ready.notified().await; }
                        else { poll.tick().await; }
                    }, if channel.ready_state() == RTCDataChannelState::Open => {
                        match input.poll_clipboard().and_then(|content| Ok(content.map(|c| c.messages()).transpose()?)) {
                            Ok(Some(messages)) => outgoing = messages.into(),
                            Ok(None) => {},
                            Err(error) => tracing::warn!(%error, "clipboard poll failed"),
                        }
                    }
                    capacity = channel.writable(), if !outgoing.is_empty() => {
                        if let Err(error) = capacity { tracing::warn!(%error, "clipboard channel unavailable"); break; }
                        if let Some(message) = outgoing.pop_front()
                            && let Err(error) = send_control_message(&channel, message).await {
                                tracing::warn!(%error, "clipboard send failed");
                                break;
                            }
                    }
                }
            }
        },
    )?;
    Ok((sender, task))
}

fn spawn_input_worker(
    input: Arc<dyn super::platform::ScreenInput>,
    channel: Arc<RTCDataChannel>,
    errors: mpsc::UnboundedSender<ControlCommand>,
) -> anyhow::Result<(
    mpsc::Sender<meshrmm_protocol::RemoteInput>,
    super::native_task::NativeTask,
)> {
    let cleanup_input = Arc::clone(&input);
    let status_channel = Arc::clone(&channel);
    let input_errors = errors.clone();
    let (updates, mut pending) = mpsc::channel(8);
    // This task owns no native resources; dropping the worker closes its queue.
    tokio::spawn(async move {
        while let Some(message) = pending.recv().await {
            if channel.ready_state() != RTCDataChannelState::Open {
                continue;
            }
            if let Err(error) = send_control_message(&channel, message).await {
                let _ = errors.send(ControlCommand::MaintenanceError(format!(
                    "input status: {error:#}"
                )));
                break;
            }
        }
    });
    let mut cursor = None;
    let mut ownership = None;
    let mut pointer_display = None;
    let mut state = None;
    let mut credentials = None;
    Ok(super::native_task::command_worker(
        "meshrmm-input",
        1024,
        std::time::Duration::from_millis(16),
        move |event| {
            if let Some(event) = event {
                if let Err(error) = input.apply(event) {
                    tracing::warn!(%error, "remote input failed; releasing session input");
                    let _ = input_errors.send(ControlCommand::Stop);
                }
            } else if status_channel.ready_state() == RTCDataChannelState::Open {
                let viewer_controls_input = input.viewer_controls_input();
                if ownership != Some(viewer_controls_input)
                    && input_errors
                        .send(ControlCommand::InputOwnership(viewer_controls_input))
                        .is_ok()
                {
                    ownership = Some(viewer_controls_input);
                }
                if let Some(next) = input.credential_state()
                    && credentials.as_ref() != Some(&next)
                    && updates
                        .try_send(SessionMessage::CredentialState(next.clone()))
                        .is_ok()
                {
                    credentials = Some(next);
                }
                let next_pointer = input.agent_pointer_display();
                if pointer_display != Some(next_pointer)
                    && updates
                        .try_send(SessionMessage::AgentPointerDisplay {
                            display_id: next_pointer,
                        })
                        .is_ok()
                {
                    pointer_display = Some(next_pointer);
                }
                let shape = input.cursor_shape();
                if cursor != Some(shape)
                    && updates
                        .try_send(SessionMessage::CursorShape { shape })
                        .is_ok()
                {
                    cursor = Some(shape);
                }
                if let Some(next) = input.maintenance_state()
                    && (state.as_ref() != Some(&next)
                        || matches!(next, SessionMessage::MaintenanceError { .. }))
                    && updates.try_send(next.clone()).is_ok()
                {
                    state = Some(next);
                }
            }
        },
        move || {
            let _ = cleanup_input.release_all();
        },
    )?)
}

async fn run_capture_control(
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    slot: Arc<LatestFrameSlot>,
    control_channel: Arc<RTCDataChannel>,
    startup: CaptureStartup,
    mut commands: mpsc::Receiver<ControlCommand>,
    started_tx: tokio::sync::oneshot::Sender<anyhow::Result<StartedScreen>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut stream_id = VideoStreamId(1);
    let CaptureStartup {
        quality_ceiling,
        encoder_status,
        initial_display,
        session_close,
    } = startup;
    // Congestion steps belong to one connection; a resumed sender starts at
    // the quality bitrate and adapts again.
    lock_streamer(&streamer)?.set_congestion_bitrate(None);
    let started = lock_streamer(&streamer)?.start(initial_display, stream_id, Arc::clone(&slot));
    let started = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = started_tx.send(Err(initial_start_error(error)));
            return Ok(());
        }
    };
    let mut displays = started.displays.clone();
    let mut active_display = started.active_display.clone();
    let mut format = started.format;
    let configured_maximum_bitrate = format.bitrate_bits_per_second;
    quality_ceiling.store(configured_maximum_bitrate, Ordering::Release);
    let mut active_profile = format.profile();
    let mut viewer_profiles = vec![active_profile];
    let mut requested_chroma = ChromaMode::Yuv420;
    let mut capture_cursor = true;
    let mut recording = false;
    let mut viewer_controls_input = false;
    let mut rejected_profiles = Vec::new();
    let mut capture_running = true;
    let mut capture_unavailable_since = None::<std::time::Instant>;
    let mut capture_retry_after = std::time::Instant::now();
    let _ = started_tx.send(Ok(started));
    let mut desktop_interval = tokio::time::interval(DESKTOP_LIFECYCLE_INTERVAL);
    desktop_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        session_close.set_target(&active_display.session);
        // Live CodecAPI bitrate changes are unsafe on HEVC hardware encoders.
        encoder_status.publish(
            format.bitrate_bits_per_second,
            format.codec != Codec::H265,
            recording,
        );
        tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                match command {
                    ControlCommand::Keyframe => {
                        if let Err(error) = lock_streamer(&streamer)?.request_keyframe() {
                            tracing::warn!(error = %error, "could not request a keyframe while the desktop is changing");
                        }
                    }
                    ControlCommand::Bitrate(value) => {
                        // Several hardware HEVC MFTs accept the CodecAPI call and
                        // then terminate asynchronously on the next frame. That
                        // turns every AIMD adjustment into a capture restart and
                        // bootstrap keyframe. Keep HEVC at the selected quality
                        // preset; congestion handling can still drop frames and
                        // request recovery without destabilizing the encoder.
                        // A queued adjustment from before a preset change must
                        // never raise the encoder above the new quality ceiling.
                        let value = value.min(quality_ceiling.load(Ordering::Acquire));
                        if let Err(error) = lock_streamer(&streamer)?.set_adaptive_bitrate(value) {
                            tracing::warn!(error = %error, "could not set bitrate while the desktop is changing");
                        }
                    }
                    ControlCommand::RestartBitrate(value) => {
                        // Restarting with static settings is the safe way to
                        // change an HEVC encoder's bitrate. Later restarts in
                        // this connection keep the congestion bitrate.
                        let ceiling = quality_ceiling.load(Ordering::Acquire);
                        let value = value.min(ceiling).max(1);
                        let previous = format.bitrate_bits_per_second;
                        if !capture_running || format.codec != Codec::H265 || value == previous {
                            continue;
                        }
                        lock_streamer(&streamer)?.set_congestion_bitrate((value < ceiling).then_some(value));
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let candidates = profile_candidates(&viewer_profiles, requested_chroma, &rejected_profiles);
                        match start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates) {
                            Ok(started) => {
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                                format = started.format;
                                capture_unavailable_since = None;
                                send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                }).await?;
                                tracing::info!(previous_bits_per_second = previous, bits_per_second = format.bitrate_bits_per_second, ?active_profile, stream_id = stream_id.0, "restarted the video encoder at a congestion bitrate step");
                            }
                            Err(error) => {
                                // A congestion step must never end the session;
                                // the desktop lifecycle retries the start.
                                capture_running = false;
                                capture_unavailable_since = Some(std::time::Instant::now());
                                capture_retry_after = std::time::Instant::now();
                                tracing::warn!(error = ?error, bits_per_second = value, "video encoder did not restart at a congestion bitrate step; retrying");
                            }
                        }
                    }
                    ControlCommand::Quality(quality)
                    | ControlCommand::ViewerCapabilities { quality, .. } => {
                        let value = quality.bitrate(configured_maximum_bitrate);
                        if let ControlCommand::ViewerCapabilities { profiles, chroma, .. } = command {
                            viewer_profiles = profiles;
                            requested_chroma = chroma;
                            rejected_profiles.clear();
                        }
                        quality_ceiling.store(value, Ordering::Release);
                        // Recreate the encoder with its static bitrate settings.
                        // Live CodecAPI updates may be ignored, rejected, or even
                        // terminate HEVC encoders after the call reports success.
                        lock_streamer(&streamer)?.set_bitrate(value);
                        let capture_changed = lock_streamer(&streamer)?.set_quality(quality);
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        if capture_running
                            && !capture_changed
                            && candidates.first() == Some(&active_profile)
                            && format.bitrate_bits_per_second == value
                        {
                            // Echo the settled configuration even when no
                            // restart is needed. The viewer deliberately does
                            // not paint the mandatory bootstrap profile until
                            // capability negotiation has completed.
                            send_control_message(
                                &control_channel,
                                SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                },
                            )
                            .await?;
                            tracing::info!(?active_profile, ?quality, ?requested_chroma, "video quality/profile selection retained active configuration");
                            continue;
                        }

                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let started = start_first_profile(
                            &streamer,
                            active_display.id,
                            stream_id,
                            &slot,
                            &candidates,
                        )?;
                        displays = started.displays;
                        active_display = started.active_display;
                        active_profile = started.format.profile();
                        format = started.format;
                        capture_running = true;
                        capture_unavailable_since = None;

                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                        tracing::info!(?active_profile, ?quality, bits_per_second = value, "video quality/profile selection applied");
                    }
                    ControlCommand::DisplayBorder(enabled) => {
                        let result = lock_streamer(&streamer)?.set_display_border(enabled);
                        if let Err(error) = result {
                            send_control_message(&control_channel, SessionMessage::MaintenanceError { reason: format!("Display border: {error:#}") }).await?;
                        }
                    }
                    ControlCommand::CursorCapture(enabled) | ControlCommand::InputOwnership(enabled) | ControlCommand::Recording(enabled) => {
                        if matches!(command, ControlCommand::CursorCapture(_)) {
                            capture_cursor = enabled;
                            tracing::info!(enabled, "viewer cursor capture selection applied");
                        } else if matches!(command, ControlCommand::Recording(_)) {
                            recording = enabled;
                        } else {
                            viewer_controls_input = enabled;
                        }
                        let update = lock_streamer(&streamer)?.set_cursor_capture(capture_cursor_for_session(capture_cursor, viewer_controls_input, recording));
                        match update {
                            Ok(false) => {}
                            Err(error) => tracing::warn!(%error, "could not update cursor capture while the desktop is changing"),
                            Ok(true) => {
                                // The console-mode WGC backend needs its existing
                                // reconfiguration path; service capture updates in place.
                                lock_streamer(&streamer)?.stop()?;
                                slot.clear();
                                stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                                let candidates = profile_candidates(&viewer_profiles, requested_chroma, &rejected_profiles);
                                let started = start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates)?;
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                                format = started.format;
                                capture_running = true;
                                capture_unavailable_since = None;
                                send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                }).await?;
                            }
                        }
                    }
                    ControlCommand::Chroma(chroma) => {
                        requested_chroma = chroma;
                        rejected_profiles.clear();
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        if candidates.first() == Some(&active_profile) {
                            tracing::info!(?active_profile, ?requested_chroma, "chroma selection retained active profile");
                            continue;
                        }
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let started = start_first_profile(
                            &streamer,
                            active_display.id,
                            stream_id,
                            &slot,
                            &candidates,
                        )?;
                        displays = started.displays;
                        active_display = started.active_display;
                        active_profile = started.format.profile();
                        format = started.format;
                        capture_running = true;
                        capture_unavailable_since = None;

                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                        tracing::info!(?active_profile, ?requested_chroma, "viewer chroma selection applied");
                    }
                    ControlCommand::VideoProfileRejected { profile, reason } => {
                        if profile != active_profile {
                            tracing::warn!(?profile, reason, "viewer rejected an inactive video profile");
                            continue;
                        }
                        rejected_profiles.push(profile);
                        tracing::warn!(?profile, reason, "viewer rejected hardware video profile; trying fallback");
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let started = start_first_profile(
                            &streamer,
                            active_display.id,
                            stream_id,
                            &slot,
                            &candidates,
                        )?;
                        displays = started.displays;
                        active_display = started.active_display;
                        active_profile = started.format.profile();
                        format = started.format;
                        capture_running = true;
                        capture_unavailable_since = None;

                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                    }
                    ControlCommand::SelectDisplay(display_id) => {
                        if display_id == active_display.id && capture_running {
                            continue;
                        }
                        let Some(selected) = displays.iter().find(|display| display.id == display_id).cloned() else {
                            tracing::warn!(display_id = display_id.0, "viewer requested an unavailable display");
                            continue;
                        };
                        let switch_started = std::time::Instant::now();
                        capture_running = false;
                        capture_unavailable_since = Some(std::time::Instant::now());
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        let restart = lock_streamer(&streamer)?.switch_display(
                            selected.id, stream_id, Arc::clone(&slot),
                        );
                        let restart = match restart {
                            Ok(started) => Ok(started),
                            Err(error) => {
                                tracing::warn!(?error, "fast display switch failed; retrying supported profiles");
                                lock_streamer(&streamer)?.stop()?;
                                start_first_profile(&streamer, selected.id, stream_id, &slot, &candidates)
                            }
                        };
                        match restart {
                            Ok(started) => {
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                        format = started.format;
                                capture_running = true;
                                capture_unavailable_since = None;

                                send_control_message(
                                    &control_channel,
                                    SessionMessage::DisplayConfiguration {
                                        displays: displays.clone(),
                                        active_display_id: active_display.id,
                                        stream_id,
                                        format: started.format,
                                    },
                                ).await?;
                                tracing::info!(switch_ms = switch_started.elapsed().as_millis(), display_id = active_display.id.0, display_name = %active_display.name, stream_id = stream_id.0, "remote display switched");
                            }
                            Err(error) => {
                                if selected.session != active_display.session
                                {
                                    send_control_message(&control_channel, SessionMessage::MaintenanceError {
                                        reason: format!("{} session could not start: {error:#}", selected.session.label()),
                                    }).await?;
                                    // A failed session switch must not strand the viewer
                                    // on a blank desktop or silently inject console input.
                                    let restored = start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates)?;
                                    displays = restored.displays;
                                    active_display = restored.active_display;
                                    active_profile = restored.format.profile();
                                    format = restored.format;
                                    capture_running = true;
                                    capture_unavailable_since = None;
                                    send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                        displays: displays.clone(), active_display_id: active_display.id,
                                        stream_id, format,
                                    }).await?;
                                } else {
                                    active_display = selected;
                                    capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
                                    tracing::warn!(error = ?error, "display switch is waiting for an interactive desktop");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ = desktop_interval.tick() => {
                if capture_running {
                    let capture_ended = lock_streamer(&streamer)?.poll_ended();
                    if let Some(capture_result) = capture_ended {
                        if let Err(error) = capture_result {
                            tracing::warn!(error = ?error, stream_id = stream_id.0, ?active_profile, configured_bitrate_bits_per_second = quality_ceiling.load(Ordering::Acquire), "visible Windows desktop changed; replacing capture helper");
                        } else {
                            tracing::warn!(stream_id = stream_id.0, ?active_profile, configured_bitrate_bits_per_second = quality_ceiling.load(Ordering::Acquire), "desktop capture helper stopped; replacing it");
                        }
                        capture_running = false;
                        capture_unavailable_since = Some(std::time::Instant::now());
                        capture_retry_after = std::time::Instant::now();
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                    }
                }
                if !capture_running && std::time::Instant::now() >= capture_retry_after {
                    let candidates = profile_candidates(
                        &viewer_profiles,
                        requested_chroma,
                        &rejected_profiles,
                    );
                    let restart = start_first_profile(
                        &streamer,
                        active_display.id,
                        stream_id,
                        &slot,
                        &candidates,
                    );
                    match restart {
                        Ok(started) => {
                            displays = started.displays;
                            active_display = started.active_display;
                            active_profile = started.format.profile();
                        format = started.format;
                            capture_running = true;

                            let recovery_ms = capture_unavailable_since
                                .take()
                                .map(|started| started.elapsed().as_millis())
                                .unwrap_or_default();
                            send_control_message(
                                &control_channel,
                                SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format: started.format,
                                },
                            ).await?;
                            tracing::info!(stream_id = stream_id.0, display_id = active_display.id.0, recovery_ms, "remote session moved to the visible Windows desktop");
                        }
                        Err(error) => {
                            capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
                            tracing::warn!(error = ?error, "waiting for a Windows login or application desktop");
                        }
                    }
                }
            },

        }
    }
}

async fn report_sender_failure(connection: &mut SignalingConnection, error: &anyhow::Error) {
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

async fn log_network_stats(peer: &RTCPeerConnection) {
    for report in peer.get_stats().await.reports.into_values() {
        if let StatsReportType::CandidatePair(pair) = report
            && pair.nominated
        {
            tracing::info!(
                rtt_ms = pair.current_round_trip_time * 1_000.0,
                available_outgoing_bitrate = pair.available_outgoing_bitrate,
                packets_sent = pair.packets_sent,
                bytes_sent = pair.bytes_sent,
                "WebRTC network statistics"
            );
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

async fn create_peer(
    ice_servers: &[IceServer],
    outgoing: mpsc::UnboundedSender<SignalMessage>,
    state: mpsc::UnboundedSender<RTCPeerConnectionState>,
    certificate: webrtc::peer_connection::certificate::RTCCertificate,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let api = APIBuilder::new().build();
    let configuration = RTCConfiguration {
        certificates: vec![certificate],
        ice_servers: ice_servers
            .iter()
            .map(|server| RTCIceServer {
                urls: server.urls.clone(),
                username: server.username.clone().unwrap_or_default(),
                credential: server.credential.clone().unwrap_or_default(),
            })
            .collect(),
        ..Default::default()
    };
    let peer = Arc::new(api.new_peer_connection(configuration).await?);
    peer.sctp()
        .transport()
        .ice_transport()
        .on_selected_candidate_pair_change(Box::new(|pair| {
            Box::pin(async move {
                let pair = pair.to_string();
                let path = if pair.to_ascii_lowercase().contains("relay") {
                    "turn"
                } else {
                    "direct"
                };
                tracing::info!(connection_path = path, candidate_pair = %pair, "ICE selected candidate pair");
            })
        }));
    peer.on_ice_candidate(Box::new(move |candidate| {
        let outgoing = outgoing.clone();
        Box::pin(async move {
            let signal = match candidate {
                Some(candidate) => match candidate.to_json() {
                    Ok(candidate) => SignalMessage::IceCandidate {
                        candidate: candidate.candidate,
                        sdp_mid: candidate.sdp_mid,
                        sdp_mline_index: candidate.sdp_mline_index,
                        username_fragment: candidate.username_fragment,
                    },
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to serialize local ICE candidate");
                        return;
                    }
                },
                None => SignalMessage::IceComplete,
            };
            let _ = outgoing.send(signal);
        })
    }));
    peer.on_peer_connection_state_change(Box::new(move |new_state| {
        let state = state.clone();
        Box::pin(async move {
            let _ = state.send(new_state);
        })
    }));
    Ok(peer)
}

#[allow(clippy::too_many_arguments)]
fn spawn_control_start(
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
        // Viewers answer this configuration with their audio preference and
        // capabilities. Very old viewers send neither; give them audio anyway.
        tokio::time::sleep(AUDIO_MODE_BACKSTOP).await;
        apply_audio_event(&audio_mode, AudioEvent::Backstop);
    })
}

async fn send_control_message(
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

#[allow(clippy::too_many_arguments)]
fn spawn_video_sender(
    channel: Arc<RTCDataChannel>,
    open: Arc<Notify>,
    decoder_ready: Arc<Notify>,
    slot: Arc<LatestFrameSlot>,
    recovery: mpsc::UnboundedSender<ControlCommand>,
    quality_ceiling: Arc<AtomicU32>,
    encoder_status: Arc<EncoderStatus>,
    audio_bits: Arc<AtomicU32>,
    failure: mpsc::UnboundedSender<anyhow::Error>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        open.notified().await;
        // Control and video use independent SCTP streams. Wait for the viewer
        // to confirm its decoder/presenter is initialized before sending the
        // bootstrap keyframe. Discard predictive frames captured during
        // connection setup, but retain the newest IDR so a completely static
        // login screen can paint immediately. We still request a fresh IDR to
        // establish a current predictive chain for subsequent frames.
        decoder_ready.notified().await;
        slot.clear_pending();
        let _ = recovery.send(ControlCommand::Keyframe);
        let mut bootstrap_keyframe =
            match tokio::time::timeout(std::time::Duration::from_secs(10), slot.keyframe()).await {
                Ok(frame) => {
                    slot.discard_through(frame.frame_id);
                    Some(frame)
                }
                Err(_) => {
                    let _ = failure.send(anyhow::anyhow!(
                        "capture/encoder produced no bootstrap keyframe for 10 seconds"
                    ));
                    return;
                }
            };
        let mut frames_sent = 0_u64;
        let mut buffered_frames_dropped = 0_u64;
        let mut obsolete_frames_dropped = 0_u64;
        let mut recovery_frames_dropped = 0_u64;
        let mut bytes_sent = 0_u64;
        let mut stats_started_us = monotonic_timestamp_us();
        let mut last_sent = None::<(VideoStreamId, u64)>;
        let mut recovering = false;
        let mut last_keyframe_request_us = 0_u64;
        let mut bitrate = AdaptiveBitrate::new(quality_ceiling.load(Ordering::Acquire).max(1));
        let mut ladder = RestartLadder::new(quality_ceiling.load(Ordering::Acquire).max(1));
        let mut pacer = VideoPacer::default();
        loop {
            let source = if let Some(frame) = bootstrap_keyframe.take() {
                frame
            } else {
                // Desktop Duplication may produce no frame while the display is
                // completely static. The main sender loop independently polls
                // the capture worker for real failures, so idleness is not an
                // error and this task can wait until the next changed frame.
                slot.next().await
            };

            let mut reference_chain_lost = false;
            if let Some((last_stream_id, last_frame_id)) = last_sent
                && (source.stream_id != last_stream_id
                    || source.frame_id != last_frame_id.wrapping_add(1))
                && !source.keyframe
            {
                recovering = true;
                reference_chain_lost = true;
                tracing::warn!(
                    last_frame_id,
                    frame_id = source.frame_id,
                    stream_id = source.stream_id.0,
                    "encoded reference frame was skipped; waiting for a recovery keyframe"
                );
            }

            let now_us = monotonic_timestamp_us();
            let requested_maximum = quality_ceiling.load(Ordering::Acquire).max(1);
            let live_bitrate = encoder_status.live_bitrate();
            let encoder_bitrate = encoder_status.bits_per_second();
            if let Some(bits_per_second) = bitrate.set_maximum(requested_maximum)
                && live_bitrate
            {
                let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
            }
            ladder.set_maximum(requested_maximum);
            let mut buffered_bytes = channel.buffered_amount().await;
            if live_bitrate {
                if let Some(bits_per_second) =
                    bitrate.observe(now_us, buffered_bytes, slot.len(), reference_chain_lost)
                {
                    let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
                    tracing::info!(
                        bits_per_second,
                        "adapted video bitrate to current transport capacity"
                    );
                }
            } else {
                // Judge congestion against the rate the encoder actually
                // runs at, and change it only by restarting the encoder.
                bitrate.rebase(encoder_bitrate);
                let queued_frames = slot.len();
                if let Some(bits_per_second) = ladder.observe(
                    now_us,
                    encoder_bitrate,
                    bitrate.congested(buffered_bytes, queued_frames, reference_chain_lost),
                    bitrate.healthy(buffered_bytes, queued_frames),
                    encoder_status.recording(),
                ) {
                    let _ = recovery.send(ControlCommand::RestartBitrate(bits_per_second));
                    tracing::info!(
                        previous_bits_per_second = encoder_bitrate,
                        bits_per_second,
                        buffered_bytes,
                        queued_frames,
                        "requested a video encoder restart at a new bitrate step"
                    );
                }
            }
            if recovering && !source.keyframe {
                recovery_frames_dropped += 1;
                if last_keyframe_request_us == 0
                    || now_us.saturating_sub(last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
                {
                    let _ = recovery.send(ControlCommand::Keyframe);
                    last_keyframe_request_us = now_us.max(1);
                }
                continue;
            }

            let congested_bytes = bitrate.congested_bytes();
            let drain_bytes = bitrate.drain_bytes();
            if buffered_bytes >= congested_bytes {
                let drain_started = tokio::time::Instant::now();
                while buffered_bytes > drain_bytes
                    && drain_started.elapsed() < VIDEO_BUFFER_DRAIN_TIMEOUT
                {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    buffered_bytes = channel.buffered_amount().await;
                }
            }
            if buffered_bytes >= congested_bytes {
                buffered_frames_dropped += 1;
                recovering = true;
                let queued_frames_dropped = slot.drop_pending();
                if live_bitrate
                    && let Some(bits_per_second) =
                        bitrate.observe(now_us, buffered_bytes, queued_frames_dropped, true)
                {
                    let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
                    tracing::warn!(
                        bits_per_second,
                        buffered_bytes,
                        queued_frames_dropped,
                        "transport stayed congested; reduced bitrate and reset the predictive chain"
                    );
                }
                if last_keyframe_request_us == 0
                    || now_us.saturating_sub(last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
                {
                    let _ = recovery.send(ControlCommand::Keyframe);
                    last_keyframe_request_us = now_us.max(1);
                }
                tracing::debug!(
                    frame_id = source.frame_id,
                    buffered_bytes,
                    "dropping frame and requesting H.264 recovery after the transport drain deadline"
                );
                continue;
            }
            let mut frame = (*source).clone();
            frame.send_timestamp_us = monotonic_timestamp_us();
            let packets = match fragment_frame(&frame, DEFAULT_FRAGMENT_PAYLOAD) {
                Ok(packets) => packets,
                Err(error) => {
                    tracing::warn!(error = %error, frame_id = frame.frame_id, "failed to fragment encoded frame");
                    continue;
                }
            };
            let mut complete = true;
            for packet in packets {
                let bytes = match packet.encode() {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to encode video packet");
                        complete = false;
                        break;
                    }
                };
                let delay_us = pacer.reserve(
                    monotonic_timestamp_us(),
                    bytes.len(),
                    video_pacing_bitrate(
                        quality_ceiling.load(Ordering::Acquire),
                        audio_bits.load(Ordering::Relaxed),
                    ),
                );
                if delay_us != 0 {
                    tokio::time::sleep(std::time::Duration::from_micros(delay_us)).await;
                }
                bytes_sent = bytes_sent.saturating_add(bytes.len() as u64);
                if let Err(error) = channel.send(&Bytes::from(bytes)).await {
                    tracing::warn!(error = %error, "video data channel send failed");
                    let _ = failure.send(transport_failure(format!(
                        "video data channel send failed: {error}"
                    )));
                    return;
                }
            }
            if complete {
                frames_sent += 1;
                last_sent = Some((source.stream_id, source.frame_id));
                if source.keyframe {
                    recovering = false;
                }
            } else {
                // Never continue a predictive chain after only part of an
                // access unit was submitted to SCTP.
                recovering = true;
                obsolete_frames_dropped += 1;
            }
            let now_us = monotonic_timestamp_us();
            let elapsed_us = now_us.saturating_sub(stats_started_us);
            if elapsed_us >= 2_000_000 {
                let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
                tracing::info!(
                    stream_fps = frames_sent as f64 / elapsed_seconds,
                    transport_bitrate_bits_per_second = bytes_sent as f64 * 8.0 / elapsed_seconds,
                    frames_sent,
                    buffered_frames_dropped,
                    obsolete_frames_dropped,
                    recovery_frames_dropped,
                    encoded_frames_dropped = slot.dropped(),
                    encoder_bitrate_bits_per_second = encoder_status.bits_per_second(),
                    "video transport statistics"
                );
                frames_sent = 0;
                buffered_frames_dropped = 0;
                obsolete_frames_dropped = 0;
                recovery_frames_dropped = 0;
                bytes_sent = 0;
                stats_started_us = now_us;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_negotiation_prefers_hevc_and_falls_back_to_420() {
        let profiles = [
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv420,
            },
            VideoProfile {
                codec: Codec::H265,
                chroma: ChromaMode::Yuv420,
            },
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv444,
            },
        ];

        assert_eq!(
            profile_candidates(&profiles, ChromaMode::Yuv444, &[]),
            vec![
                VideoProfile {
                    codec: Codec::H264,
                    chroma: ChromaMode::Yuv444,
                },
                VideoProfile {
                    codec: Codec::H265,
                    chroma: ChromaMode::Yuv420,
                },
                VideoProfile {
                    codec: Codec::H264,
                    chroma: ChromaMode::Yuv420,
                },
            ]
        );
    }

    #[test]
    fn rejected_video_profiles_are_not_retried() {
        let h265_444 = VideoProfile {
            codec: Codec::H265,
            chroma: ChromaMode::Yuv444,
        };
        let h264_444 = VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv444,
        };
        let h264_420 = VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv420,
        };

        assert_eq!(
            profile_candidates(
                &[h265_444, h264_444, h264_420],
                ChromaMode::Yuv444,
                &[h265_444],
            ),
            vec![h264_444, h264_420]
        );
    }
}

#[cfg(test)]
mod service_isolation_tests {
    use super::super::platform::ScreenInput;
    use super::*;
    #[test]
    fn recording_keeps_cursor_for_both_input_owners_and_restores_preference() {
        for show_cursor in [false, true] {
            for viewer_controls_input in [false, true] {
                assert!(super::capture_cursor_for_session(
                    show_cursor,
                    viewer_controls_input,
                    true
                ));
                assert_eq!(
                    super::capture_cursor_for_session(show_cursor, viewer_controls_input, false),
                    show_cursor && !viewer_controls_input
                );
            }
        }
    }

    use meshrmm_protocol::{ClipboardContent, CursorShape, FileMessage, RemoteInput};

    struct TestInput {
        events: mpsc::UnboundedSender<&'static str>,
        file_gate: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl ScreenInput for TestInput {
        fn set_prevent_idle_lock(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_wallpaper_hidden(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn apply_files(&self, _: FileMessage) -> anyhow::Result<()> {
            self.events.send("file blocked")?;
            self.file_gate
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))?;
            Ok(())
        }
        fn apply(&self, _: RemoteInput) -> anyhow::Result<()> {
            self.events.send("input")?;
            Ok(())
        }
        fn apply_chat(&self, _: String) -> anyhow::Result<()> {
            self.events.send("chat")?;
            Ok(())
        }
        fn apply_clipboard(&self, _: ClipboardContent) -> anyhow::Result<()> {
            self.events.send("clipboard")?;
            Ok(())
        }
        fn release_all(&self) -> anyhow::Result<()> {
            self.events.send("released")?;
            Ok(())
        }
        fn set_blackout(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn set_agent_input_blocked(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn maintenance_state(&self) -> Option<SessionMessage> {
            None
        }
        fn viewer_controls_input(&self) -> bool {
            false
        }

        fn agent_pointer_display(&self) -> Option<DisplayId> {
            None
        }

        fn cursor_shape(&self) -> CursorShape {
            CursorShape::Default
        }
        fn files_ready(&self) -> Arc<tokio::sync::Notify> {
            Arc::new(tokio::sync::Notify::new())
        }
        fn poll_files(&self) -> Option<FileMessage> {
            None
        }
        fn chat_ready(&self) -> Arc<tokio::sync::Notify> {
            Arc::new(tokio::sync::Notify::new())
        }
        fn poll_chat(&self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
            Ok(None)
        }
        fn start_chat(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop_chat(&self) {}
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_file_operation_does_not_delay_input_chat_or_clipboard() {
        let (events, mut received) = mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let input: Arc<dyn ScreenInput> = Arc::new(TestInput {
            events,
            file_gate: Mutex::new(gate),
        });
        let channel = Arc::new(RTCDataChannel::default());
        let (errors, _) = mpsc::unbounded_channel();
        let (files, mut file_task) = spawn_file_worker(
            input.clone(),
            ServiceChannel::new(channel.clone()).await,
            None,
        )
        .unwrap();
        let (keys, mut input_task) =
            spawn_input_worker(input.clone(), channel.clone(), errors).unwrap();
        let (chat, mut chat_task) = spawn_chat_worker(
            input.clone(),
            ServiceChannel::new(channel.clone()).await,
            None,
        )
        .unwrap();
        let (clipboard, mut clipboard_task) =
            spawn_clipboard_worker(input, ServiceChannel::new(channel).await, None).unwrap();
        files.try_send(FileMessage::Pick).unwrap();
        assert_eq!(received.recv().await, Some("file blocked"));
        keys.try_send(RemoteInput::PointerMove {
            display_id: DisplayId(1),
            x: 0,
            y: 0,
        })
        .unwrap();
        chat.try_send(SessionMessage::Chat {
            text: "still responsive".into(),
        })
        .unwrap();
        for message in ClipboardContent::from("independent").messages().unwrap() {
            clipboard.try_send(message).unwrap();
        }
        let mut observed = Vec::new();
        for _ in 0..3 {
            observed.push(
                tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        observed.sort();
        assert_eq!(observed, ["chat", "clipboard", "input"]);
        release.send(()).unwrap();
        file_task.shutdown().await;
        input_task.shutdown().await;
        chat_task.shutdown().await;
        clipboard_task.shutdown().await;
        assert_eq!(received.recv().await, Some("released"));
    }
}
