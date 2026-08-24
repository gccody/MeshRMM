use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, CONTROL_CHANNEL_PROTOCOL, ChromaMode, Codec, CursorShape,
    DEFAULT_FRAGMENT_PAYLOAD, Display, DisplayId, IceServer, QualityPreset, RemoteSessionId,
    SessionMessage, SessionState, SignalMessage, VideoProfile, VideoStreamId, fragment_frame,
};
use meshrmm_signaling_client::Socket;
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

use super::platform::{ScreenStreamer, StartedScreen, monotonic_timestamp_us};
use super::signaling::authenticated_websocket;
use super::video::LatestFrameSlot;

enum ControlCommand {
    Keyframe,
    Bitrate(u32),
    ViewerCapabilities {
        profiles: Vec<VideoProfile>,
        quality: QualityPreset,
        chroma: ChromaMode,
    },
    Quality(QualityPreset),
    Chroma(ChromaMode),
    VideoProfileRejected {
        profile: VideoProfile,
        reason: String,
    },
    SelectDisplay(DisplayId),
    Clipboard(String),
    ChannelClosed,
    Stop,
}

const VIDEO_BUFFER_DRAIN_MS: u32 = 50;
const VIDEO_BUFFER_CONGESTED_MS: u32 = 150;
const VIDEO_BUFFER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(80);
const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;
const BITRATE_DECREASE_INTERVAL_US: u64 = 500_000;
const BITRATE_INCREASE_INTERVAL_US: u64 = 5_000_000;
const DESKTOP_LIFECYCLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const DESKTOP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug)]
struct AdaptiveBitrate {
    minimum: u32,
    maximum: u32,
    current: u32,
    last_decrease_us: u64,
    healthy_since_us: u64,
}

impl AdaptiveBitrate {
    fn new(maximum: u32) -> Self {
        let minimum = (maximum / 8).max(1_000_000).min(maximum);
        Self {
            minimum,
            maximum,
            current: maximum,
            last_decrease_us: 0,
            healthy_since_us: 0,
        }
    }

    fn set_maximum(&mut self, maximum: u32) -> Option<u32> {
        let maximum = maximum.max(1);
        if self.maximum == maximum {
            return None;
        }
        self.maximum = maximum;
        self.minimum = (maximum / 8).max(500_000).min(maximum);
        self.current = maximum;
        self.last_decrease_us = 0;
        self.healthy_since_us = 0;
        Some(maximum)
    }

    fn observe(
        &mut self,
        now_us: u64,
        buffered_bytes: usize,
        queued_frames: usize,
        reference_chain_lost: bool,
    ) -> Option<u32> {
        let congested = reference_chain_lost
            || buffered_bytes >= self.congested_bytes()
            || queued_frames >= (super::video::MAX_ENCODED_FRAME_QUEUE * 4) / 5;
        if congested {
            self.healthy_since_us = 0;
            if self.last_decrease_us == 0
                || now_us.saturating_sub(self.last_decrease_us) >= BITRATE_DECREASE_INTERVAL_US
            {
                self.last_decrease_us = now_us.max(1);
                let reduced = ((u64::from(self.current) * 3) / 4) as u32;
                let reduced = reduced.max(self.minimum);
                if reduced < self.current {
                    self.current = reduced;
                    return Some(self.current);
                }
            }
            return None;
        }

        let healthy = buffered_bytes <= self.drain_bytes() && queued_frames <= 1;
        if !healthy || self.current >= self.maximum {
            self.healthy_since_us = 0;
            return None;
        }
        if self.healthy_since_us == 0 {
            self.healthy_since_us = now_us.max(1);
            return None;
        }
        if now_us.saturating_sub(self.healthy_since_us) >= BITRATE_INCREASE_INTERVAL_US {
            self.healthy_since_us = now_us.max(1);
            let increase = (self.current / 10).max(250_000);
            self.current = self.current.saturating_add(increase).min(self.maximum);
            return Some(self.current);
        }
        None
    }

    fn drain_bytes(&self) -> usize {
        bitrate_duration_bytes(self.current, VIDEO_BUFFER_DRAIN_MS)
    }

    fn congested_bytes(&self) -> usize {
        bitrate_duration_bytes(self.current, VIDEO_BUFFER_CONGESTED_MS)
    }
}

fn bitrate_duration_bytes(bits_per_second: u32, duration_ms: u32) -> usize {
    usize::try_from(
        u64::from(bits_per_second)
            .saturating_mul(u64::from(duration_ms))
            .div_ceil(8_000),
    )
    .unwrap_or(usize::MAX)
    .max(16 * 1024)
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
                failures.push(format!("{profile:?}: {error:#}"));
            }
        }
    }
    anyhow::bail!(
        "no mutually supported hardware video profile could start: {}",
        failures.join("; ")
    )
}

pub async fn run_sender(
    signal_url: Url,
    signaling_token: &str,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
) -> anyhow::Result<()> {
    let (socket, _) = authenticated_websocket(signal_url, signaling_token).await?;
    let (mut signal_writer, mut signal_reader) = socket.split();
    let mut failure_reported = false;
    let result = run_connected_sender(
        &mut signal_writer,
        &mut signal_reader,
        ice_servers,
        streamer,
        session_id,
        &mut failure_reported,
    )
    .await;
    if let Err(error) = &result
        && !failure_reported
    {
        report_sender_failure(&mut signal_writer, error).await;
    }
    result
}

async fn run_connected_sender(
    signal_writer: &mut SplitSink<Socket, Message>,
    signal_reader: &mut SplitStream<Socket>,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    failure_reported: &mut bool,
) -> anyhow::Result<()> {
    // Input has its own synchronized controller so capture startup, encoder
    // recovery, and video teardown never hold the path used by control events.
    let input = lock_streamer(&streamer)?.input_controller();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlCommand>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
    let (video_failure_tx, mut video_failure_rx) = mpsc::unbounded_channel::<String>();
    let peer = create_peer(&ice_servers, outgoing_tx.clone(), state_tx).await?;

    let video_open = Arc::new(Notify::new());
    let control_open = Arc::new(Notify::new());
    let decoder_ready = Arc::new(Notify::new());
    let video_channel = peer
        .create_data_channel(
            "meshrmm-video-v1",
            Some(RTCDataChannelInit {
                // Inter-frame video access units form a predictive chain. Keep this
                // dedicated video stream ordered so benign SCTP reordering is
                // not mistaken for packet loss. One retry bounds head-of-line
                // delay, and input remains isolated on the control stream.
                ordered: Some(true),
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
    {
        let notify = Arc::clone(&control_open);
        let decoder_ready = Arc::clone(&decoder_ready);
        let closed_tx = control_tx.clone();
        control_channel.on_open(Box::new(move || {
            let notify = Arc::clone(&notify);
            Box::pin(async move { notify.notify_one() })
        }));
        control_channel.on_close(Box::new(move || {
            let closed_tx = closed_tx.clone();
            Box::pin(async move {
                let _ = closed_tx.send(ControlCommand::ChannelClosed);
            })
        }));
        let control_messages_tx = control_tx.clone();
        let control_input = Arc::clone(&input);
        control_channel.on_message(Box::new(move |message| {
            let tx = control_messages_tx.clone();
            let decoder_ready = Arc::clone(&decoder_ready);
            let input = Arc::clone(&control_input);
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
                    }) => Some(ControlCommand::ViewerCapabilities {
                        profiles,
                        quality,
                        chroma,
                    }),
                    Ok(SessionMessage::SetQuality { preset }) => {
                        Some(ControlCommand::Quality(preset))
                    }
                    Ok(SessionMessage::SetChroma { mode }) => Some(ControlCommand::Chroma(mode)),
                    Ok(SessionMessage::VideoProfileRejected { profile, reason }) => {
                        Some(ControlCommand::VideoProfileRejected { profile, reason })
                    }
                    Ok(SessionMessage::SelectDisplay { display_id }) => {
                        Some(ControlCommand::SelectDisplay(display_id))
                    }
                    Ok(SessionMessage::Input(event)) => {
                        if let Err(error) = input.apply(event) {
                            tracing::warn!(error = %error, "discarding invalid remote input");
                        }
                        None
                    }
                    Ok(SessionMessage::Clipboard { text }) => Some(ControlCommand::Clipboard(text)),
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

    let slot = Arc::new(LatestFrameSlot::default());
    let mut stream_id = VideoStreamId(1);
    let started = {
        let mut streamer = streamer
            .lock()
            .map_err(|_| anyhow::anyhow!("screen streamer lock is poisoned"))?;
        streamer.start(None, stream_id, Arc::clone(&slot))?
    };
    let mut displays = started.displays;
    let mut active_display = started.active_display;
    let format = started.format;
    let configured_maximum_bitrate = format.bitrate_bits_per_second;
    let quality_ceiling = Arc::new(AtomicU32::new(configured_maximum_bitrate));
    let mut active_profile = format.profile();
    let mut viewer_profiles = vec![active_profile];
    let mut requested_chroma = ChromaMode::Yuv420;
    let mut rejected_profiles = Vec::new();
    let mut capture_running = true;
    let video_sender = spawn_video_sender(
        Arc::clone(&video_channel),
        Arc::clone(&video_open),
        decoder_ready,
        Arc::clone(&slot),
        control_tx.clone(),
        Arc::clone(&quality_ceiling),
        video_failure_tx,
    );
    spawn_control_start(
        Arc::clone(&control_channel),
        Arc::clone(&control_open),
        session_id.clone(),
        displays.clone(),
        active_display.id,
        stream_id,
        format,
    );

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    stats_interval.tick().await;
    let mut desktop_interval = tokio::time::interval(DESKTOP_LIFECYCLE_INTERVAL);
    desktop_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    desktop_interval.tick().await;
    let mut cursor_interval = tokio::time::interval(std::time::Duration::from_millis(16));
    cursor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut clipboard_interval = tokio::time::interval(std::time::Duration::from_millis(250));
    clipboard_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sent_cursor_shape = None::<CursorShape>;
    let mut offer_sent = false;
    let mut remote_description_set = false;
    let mut pending_candidates = Vec::new();
    let mut capture_unavailable_since = None::<std::time::Instant>;
    let mut capture_retry_after = std::time::Instant::now();
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
            Some(signal) = outgoing_rx.recv() => {
                let json = serde_json::to_string(&signal)?;
                signal_writer.send(Message::Text(json.into())).await?;
            }
            incoming = signal_reader.next() => {
                let Some(incoming) = incoming else { break Err(anyhow::anyhow!("signaling connection closed")); };
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
                                    pending_candidates.push(candidate);
                                }
                            }
                            SignalMessage::PeerLeft => break Ok(()),
                            SignalMessage::Error { message } => break Err(anyhow::anyhow!(message)),
                            _ => {}
                        }
                    }
                    Message::Ping(payload) => signal_writer.send(Message::Pong(payload)).await?,
                    Message::Close(_) => break Ok(()),
                    _ => {}
                }
            }
            Some(command) = control_rx.recv() => {
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
                        if let Err(error) = lock_streamer(&streamer)?.set_adaptive_bitrate(value) {
                            tracing::warn!(error = %error, "could not set bitrate while the desktop is changing");
                        }
                    }
                    ControlCommand::Quality(preset) => {
                        let value = preset.bitrate(configured_maximum_bitrate);
                        quality_ceiling.store(value, Ordering::Release);
                        if let Err(error) = lock_streamer(&streamer)?.set_bitrate(value) {
                            tracing::warn!(error = %error, "could not apply viewer quality preset");
                        } else {
                            tracing::info!(?preset, bits_per_second = value, "viewer quality preset applied");
                        }
                    }
                    ControlCommand::ViewerCapabilities { profiles, quality, chroma } => {
                        let value = quality.bitrate(configured_maximum_bitrate);
                        quality_ceiling.store(value, Ordering::Release);
                        if let Err(error) = lock_streamer(&streamer)?.set_bitrate(value) {
                            tracing::warn!(error = %error, "could not apply initial viewer quality preset");
                        }
                        viewer_profiles = profiles;
                        requested_chroma = chroma;
                        rejected_profiles.clear();
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        if candidates.first() == Some(&active_profile) {
                            tracing::info!(?active_profile, ?quality, ?requested_chroma, "video profile negotiation retained active profile");
                            continue;
                        }

                        lock_streamer(&streamer)?.stop()?;
                        capture_running = false;
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
                        capture_running = true;
                        capture_unavailable_since = None;
                        sent_cursor_shape = None;
                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                        tracing::info!(?active_profile, ?quality, bits_per_second = value, "video profile negotiation completed");
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
                        capture_running = false;
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
                        capture_running = true;
                        capture_unavailable_since = None;
                        sent_cursor_shape = None;
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
                        capture_running = false;
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
                        capture_running = true;
                        capture_unavailable_since = None;
                        sent_cursor_shape = None;
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
                    ControlCommand::Clipboard(text) => {
                        if let Err(error) = input.apply_clipboard(text) {
                            tracing::warn!(error = %error, "discarding viewer clipboard update");
                        }
                    }
                    ControlCommand::SelectDisplay(display_id) => {
                        if display_id == active_display.id && capture_running {
                            continue;
                        }
                        let Some(selected) = displays.iter().find(|display| display.id == display_id).cloned() else {
                            tracing::warn!(display_id = display_id.0, "viewer requested an unavailable display");
                            continue;
                        };
                        lock_streamer(&streamer)?.stop()?;
                        capture_running = false;
                        capture_unavailable_since = Some(std::time::Instant::now());
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        let restart = start_first_profile(
                            &streamer,
                            selected.id,
                            stream_id,
                            &slot,
                            &candidates,
                        );
                        match restart {
                            Ok(started) => {
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                                capture_running = true;
                                capture_unavailable_since = None;
                                sent_cursor_shape = None;
                                send_control_message(
                                    &control_channel,
                                    SessionMessage::DisplayConfiguration {
                                        displays: displays.clone(),
                                        active_display_id: active_display.id,
                                        stream_id,
                                        format: started.format,
                                    },
                                ).await?;
                                tracing::info!(display_id = active_display.id.0, display_name = %active_display.name, stream_id = stream_id.0, "remote display switched");
                            }
                            Err(error) => {
                                active_display = selected;
                                capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
                                tracing::warn!(error = ?error, "display switch is waiting for an interactive desktop");
                            }
                        }
                    }
                    ControlCommand::Stop => break Ok(()),
                    ControlCommand::ChannelClosed => {
                        break Err(anyhow::anyhow!(
                            "remote input/control channel closed unexpectedly"
                        ));
                    }
                }
            }
            _ = clipboard_interval.tick(), if session_state == SessionState::Streaming
                && control_channel.ready_state() == RTCDataChannelState::Open => {
                match input.poll_clipboard() {
                    Ok(Some(text)) => {
                        send_control_message(
                            &control_channel,
                            SessionMessage::Clipboard { text },
                        ).await?;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(error = %error, "could not synchronize the Agent clipboard"),
                }
            }
            _ = cursor_interval.tick(), if session_state == SessionState::Streaming => {
                let shape = input.cursor_shape();
                if sent_cursor_shape != Some(shape)
                    && control_channel.ready_state() == RTCDataChannelState::Open
                {
                    send_control_message(
                        &control_channel,
                        SessionMessage::CursorShape { shape },
                    ).await?;
                    sent_cursor_shape = Some(shape);
                }
            }
            Some(state) = state_rx.recv() => {
                tracing::info!(?state, session_id = %session_id, "WebRTC connection state changed");
                if state == RTCPeerConnectionState::Connected
                    && session_state == SessionState::Connecting
                {
                    session_state = session_state.transition(SessionState::Streaming)?;
                }
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed | RTCPeerConnectionState::Disconnected) {
                    break Err(anyhow::anyhow!("WebRTC connection ended in state {state:?}"));
                }
            }
            Some(error) = video_failure_rx.recv() => break Err(anyhow::anyhow!(error)),
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
                            capture_running = true;
                            sent_cursor_shape = None;
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
            _ = stats_interval.tick() => {
                log_network_stats(&peer).await;
            },
            }
        }
    }
    .await;

    if let Err(error) = &result {
        report_sender_failure(signal_writer, error).await;
        *failure_reported = true;
    }
    video_sender.abort();
    if let Err(error) = input.release_all() {
        tracing::warn!(error = %error, "failed to release remote input during cleanup");
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
    let stop_result = lock_streamer(&streamer).and_then(|mut streamer| streamer.stop());
    if let Err(error) = stop_result {
        tracing::warn!(error = %error, "screen streamer did not stop cleanly");
        if result.is_ok() {
            result = Err(error.context("screen streamer cleanup failed"));
        }
    }
    if let Err(error) = peer.close().await {
        tracing::warn!(error = %error, "WebRTC peer did not close cleanly");
        if result.is_ok() {
            result = Err(error).context("failed to close WebRTC peer");
        }
    }
    session_state = session_state.transition(SessionState::Idle)?;
    tracing::info!(
        session_id = %session_id,
        ?session_state,
        encoded_frames_dropped = slot.dropped(),
        "remote sender session stopped"
    );
    result
}

async fn report_sender_failure(
    signal_writer: &mut SplitSink<Socket, Message>,
    error: &anyhow::Error,
) {
    let signal = SignalMessage::Error {
        message: format!("{error:#}"),
    };
    match serde_json::to_string(&signal) {
        Ok(message) => {
            if let Err(send_error) = signal_writer.send(Message::Text(message.into())).await {
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
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let api = APIBuilder::new().build();
    let configuration = RTCConfiguration {
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

fn spawn_control_start(
    channel: Arc<RTCDataChannel>,
    open: Arc<Notify>,
    session_id: RemoteSessionId,
    displays: Vec<Display>,
    active_display_id: DisplayId,
    stream_id: VideoStreamId,
    format: meshrmm_protocol::VideoFormat,
) {
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
        }
    });
}

async fn send_control_message(
    channel: &RTCDataChannel,
    message: SessionMessage,
) -> anyhow::Result<()> {
    let bytes = message
        .encode()
        .context("failed to encode remote control message")?;
    channel
        .send(&Bytes::from(bytes))
        .await
        .context("failed to send remote control message")?;
    Ok(())
}

fn spawn_video_sender(
    channel: Arc<RTCDataChannel>,
    open: Arc<Notify>,
    decoder_ready: Arc<Notify>,
    slot: Arc<LatestFrameSlot>,
    recovery: mpsc::UnboundedSender<ControlCommand>,
    quality_ceiling: Arc<AtomicU32>,
    failure: mpsc::UnboundedSender<String>,
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
                    let _ = failure.send(
                        "capture/encoder produced no bootstrap keyframe for 10 seconds".into(),
                    );
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
            if let Some(bits_per_second) = bitrate.set_maximum(requested_maximum) {
                let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
            }
            let mut buffered_bytes = channel.buffered_amount().await;
            if let Some(bits_per_second) =
                bitrate.observe(now_us, buffered_bytes, slot.len(), reference_chain_lost)
            {
                let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
                tracing::info!(
                    bits_per_second,
                    "adapted video bitrate to current transport capacity"
                );
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
                if let Some(bits_per_second) =
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
                bytes_sent = bytes_sent.saturating_add(bytes.len() as u64);
                if let Err(error) = channel.send(&Bytes::from(bytes)).await {
                    tracing::warn!(error = %error, "video data channel send failed");
                    let _ = failure.send(format!("video data channel send failed: {error}"));
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
    fn adaptive_bitrate_uses_aimd_without_oscillating() {
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        let congested_bytes = bitrate.congested_bytes();

        assert_eq!(
            bitrate.observe(1_000, congested_bytes, 0, false),
            Some(9_000_000)
        );
        let congested_bytes = bitrate.congested_bytes();
        assert_eq!(
            bitrate.observe(2_000, congested_bytes, 0, false),
            None,
            "decreases are rate limited"
        );
        let congested_bytes = bitrate.congested_bytes();
        assert_eq!(
            bitrate.observe(
                1_000 + BITRATE_DECREASE_INTERVAL_US,
                congested_bytes,
                0,
                false,
            ),
            Some(6_750_000)
        );

        let healthy_start = 2_000_000;
        assert_eq!(bitrate.observe(healthy_start, 0, 0, false), None);
        assert_eq!(
            bitrate.observe(healthy_start + BITRATE_INCREASE_INTERVAL_US, 0, 0, false),
            Some(7_425_000)
        );
    }

    #[test]
    fn adaptive_bitrate_never_drops_below_its_floor() {
        let mut bitrate = AdaptiveBitrate::new(4_000_000);
        let mut now_us = 1;
        for _ in 0..20 {
            let _ = bitrate.observe(now_us, usize::MAX, usize::MAX, true);
            now_us += BITRATE_DECREASE_INTERVAL_US;
        }
        assert_eq!(bitrate.current, 1_000_000);
    }

    #[test]
    fn quality_ceiling_change_takes_effect_immediately() {
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        assert_eq!(bitrate.set_maximum(3_000_000), Some(3_000_000));
        assert_eq!(bitrate.current, 3_000_000);
        assert_eq!(bitrate.maximum, 3_000_000);

        assert_eq!(bitrate.set_maximum(6_000_000), Some(6_000_000));
        assert_eq!(bitrate.current, 6_000_000);
        assert_eq!(bitrate.set_maximum(6_000_000), None);
    }

    #[test]
    fn transport_buffer_thresholds_represent_time_not_a_fixed_byte_count() {
        let low = AdaptiveBitrate::new(3_000_000);
        let high = AdaptiveBitrate::new(12_000_000);

        assert_eq!(low.drain_bytes(), 18_750);
        assert_eq!(low.congested_bytes(), 56_250);
        assert_eq!(high.drain_bytes(), 75_000);
        assert_eq!(high.congested_bytes(), 225_000);
    }

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
