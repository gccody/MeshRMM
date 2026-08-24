use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, CONTROL_CHANNEL_PROTOCOL, CursorShape, DEFAULT_FRAGMENT_PAYLOAD,
    Display, DisplayId, IceServer, RemoteInput, RemoteSessionId, SessionMessage, SessionState,
    SignalMessage, VideoStreamId, fragment_frame,
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

use super::clipboard::ClipboardSync;
use super::input::WindowsInputController;
use super::platform::{ScreenStreamer, monotonic_timestamp_us};
use super::signaling::authenticated_websocket;
use super::video::LatestFrameSlot;

enum ControlCommand {
    Keyframe,
    Bitrate(u32),
    SelectDisplay(DisplayId),
    Input(RemoteInput),
    Clipboard(String),
    ChannelClosed,
    Stop,
}

const VIDEO_BUFFER_DRAIN_BYTES: usize = 64 * 1024;
const VIDEO_BUFFER_CONGESTED_BYTES: usize = 192 * 1024;
const VIDEO_BUFFER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(80);
const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;
const BITRATE_DECREASE_INTERVAL_US: u64 = 500_000;
const BITRATE_INCREASE_INTERVAL_US: u64 = 5_000_000;

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

    fn observe(
        &mut self,
        now_us: u64,
        buffered_bytes: usize,
        queued_frames: usize,
        reference_chain_lost: bool,
    ) -> Option<u32> {
        let congested = reference_chain_lost
            || buffered_bytes >= VIDEO_BUFFER_CONGESTED_BYTES
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

        let healthy = buffered_bytes <= VIDEO_BUFFER_DRAIN_BYTES && queued_frames <= 1;
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
                // H.264 access units form a predictive chain. Keep this
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
        control_channel.on_message(Box::new(move |message| {
            let tx = control_messages_tx.clone();
            let decoder_ready = Arc::clone(&decoder_ready);
            Box::pin(async move {
                let command = match SessionMessage::decode(&message.data) {
                    Ok(SessionMessage::RequestKeyframe { .. }) => {
                        decoder_ready.notify_one();
                        Some(ControlCommand::Keyframe)
                    }
                    Ok(SessionMessage::SetBitrate { bits_per_second }) => {
                        Some(ControlCommand::Bitrate(bits_per_second))
                    }
                    Ok(SessionMessage::SelectDisplay { display_id }) => {
                        Some(ControlCommand::SelectDisplay(display_id))
                    }
                    Ok(SessionMessage::Input(input)) => Some(ControlCommand::Input(input)),
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
    let displays = lock_streamer(&streamer)?.displays()?;
    let mut active_display = displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| displays.first())
        .cloned()
        .context("Windows reported no active displays")?;
    let mut stream_id = VideoStreamId(1);
    let format = {
        let mut streamer = streamer
            .lock()
            .map_err(|_| anyhow::anyhow!("screen streamer lock is poisoned"))?;
        streamer.start(active_display.id, stream_id, Arc::clone(&slot))?
    };
    let mut input = WindowsInputController::new();
    input.set_active_display(active_display.clone())?;
    let mut clipboard = match ClipboardSync::new() {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(error = %error, "clipboard sync is unavailable for this Agent session");
            None
        }
    };
    let video_sender = spawn_video_sender(
        Arc::clone(&video_channel),
        Arc::clone(&video_open),
        decoder_ready,
        Arc::clone(&slot),
        control_tx.clone(),
        format.bitrate_bits_per_second,
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
    let mut cursor_interval = tokio::time::interval(std::time::Duration::from_millis(16));
    cursor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut clipboard_interval = tokio::time::interval(std::time::Duration::from_millis(250));
    clipboard_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sent_cursor_shape = None::<CursorShape>;
    let mut offer_sent = false;
    let mut remote_description_set = false;
    let mut pending_candidates = Vec::new();
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
                    ControlCommand::Keyframe => lock_streamer(&streamer)?.request_keyframe()?,
                    ControlCommand::Bitrate(value) => lock_streamer(&streamer)?.set_bitrate(value)?,
                    ControlCommand::Input(event) => {
                        if let Err(error) = input.apply(event) {
                            tracing::warn!(error = %error, "discarding invalid remote input");
                        }
                    }
                    ControlCommand::Clipboard(text) => {
                        if let Some(clipboard) = clipboard.as_mut()
                            && let Err(error) = clipboard.apply(text)
                        {
                            tracing::warn!(error = %error, "discarding viewer clipboard update");
                        }
                    }
                    ControlCommand::SelectDisplay(display_id) => {
                        if display_id == active_display.id {
                            continue;
                        }
                        let Some(selected) = displays.iter().find(|display| display.id == display_id).cloned() else {
                            tracing::warn!(display_id = display_id.0, "viewer requested an unavailable display");
                            continue;
                        };
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let format = lock_streamer(&streamer)?.start(
                            selected.id,
                            stream_id,
                            Arc::clone(&slot),
                        )?;
                        input.set_active_display(selected.clone())?;
                        active_display = selected;
                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format,
                            },
                        ).await?;
                        tracing::info!(display_id = active_display.id.0, display_name = %active_display.name, stream_id = stream_id.0, "remote display switched");
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
                && clipboard.is_some()
                && control_channel.ready_state() == RTCDataChannelState::Open => {
                if let Some(clipboard) = clipboard.as_mut() {
                    match clipboard.poll() {
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
            _ = stats_interval.tick() => {
                let capture_ended = lock_streamer(&streamer)?.poll_ended();
                if let Some(capture_result) = capture_ended {
                    if let Err(error) = capture_result {
                        tracing::error!(error = ?error, "Windows GPU capture/encode worker failed");
                        break Err(error);
                    }
                    break Err(anyhow::anyhow!("Windows GPU capture/encode worker stopped unexpectedly"));
                }
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
    maximum_bitrate: u32,
    failure: mpsc::UnboundedSender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        open.notified().await;
        // Control and video use independent SCTP streams. Wait for the viewer
        // to confirm its decoder/presenter is initialized before sending the
        // bootstrap keyframe. Discard frames captured during connection setup,
        // then wait for the fresh IDR requested by the initialized viewer.
        decoder_ready.notified().await;
        slot.clear();
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
        let mut bitrate = AdaptiveBitrate::new(maximum_bitrate);
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

            if buffered_bytes >= VIDEO_BUFFER_CONGESTED_BYTES {
                let drain_started = tokio::time::Instant::now();
                while buffered_bytes > VIDEO_BUFFER_DRAIN_BYTES
                    && drain_started.elapsed() < VIDEO_BUFFER_DRAIN_TIMEOUT
                {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    buffered_bytes = channel.buffered_amount().await;
                }
            }
            if buffered_bytes >= VIDEO_BUFFER_CONGESTED_BYTES {
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

        assert_eq!(
            bitrate.observe(1_000, VIDEO_BUFFER_CONGESTED_BYTES, 0, false),
            Some(9_000_000)
        );
        assert_eq!(
            bitrate.observe(2_000, VIDEO_BUFFER_CONGESTED_BYTES, 0, false),
            None,
            "decreases are rate limited"
        );
        assert_eq!(
            bitrate.observe(
                1_000 + BITRATE_DECREASE_INTERVAL_US,
                VIDEO_BUFFER_CONGESTED_BYTES,
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
}
