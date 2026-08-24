use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, CursorShape, EncodedFrame, FrameReassembler, IceServer,
    ReassemblyConfig, ReassemblyOutcome, SessionBootstrap, SessionMessage, SessionState,
    SignalMessage, VideoPacket, VideoStreamId,
};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use webrtc::api::APIBuilder;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};
use webrtc::ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer};
use webrtc::peer_connection::{
    RTCPeerConnection, configuration::RTCConfiguration,
    peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription,
};
use webrtc::stats::StatsReportType;

use crate::clipboard::ClipboardSync;
use crate::config::Config;
use crate::debug::DebugInfo;
use crate::platform::{ControlSink, Presenter, monotonic_timestamp_us};
use crate::signaling::{authenticated_websocket, session_signal_url};

const SESSION_ACTIVITY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const CLIPBOARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const POINTER_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);
const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;

struct ActivePresenter {
    stream_id: VideoStreamId,
    presenter: Presenter,
}

struct VideoReceiveState {
    reassembler: FrameReassembler,
    stream_id: Option<VideoStreamId>,
    last_accepted_frame_id: Option<u64>,
    waiting_for_keyframe: bool,
    last_keyframe_request_us: u64,
}

impl VideoReceiveState {
    fn new() -> Self {
        Self {
            reassembler: FrameReassembler::new(ReassemblyConfig::default()),
            stream_id: None,
            last_accepted_frame_id: None,
            waiting_for_keyframe: true,
            last_keyframe_request_us: 0,
        }
    }

    fn mark_loss(&mut self) {
        self.waiting_for_keyframe = true;
    }

    fn keyframe_request_due(&mut self, now_us: u64) -> bool {
        if self.last_keyframe_request_us == 0
            || now_us.saturating_sub(self.last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
        {
            self.last_keyframe_request_us = now_us.max(1);
            true
        } else {
            false
        }
    }

    fn accept_completed(
        &mut self,
        frame: EncodedFrame,
        now_us: u64,
    ) -> (Option<EncodedFrame>, bool) {
        if self.stream_id != Some(frame.stream_id) {
            self.stream_id = Some(frame.stream_id);
            self.last_accepted_frame_id = None;
            self.waiting_for_keyframe = true;
            self.last_keyframe_request_us = 0;
        }

        let gap = self
            .last_accepted_frame_id
            .is_some_and(|last| frame.frame_id != last.wrapping_add(1));
        if gap && !frame.keyframe {
            self.waiting_for_keyframe = true;
            tracing::warn!(
                last_frame_id = ?self.last_accepted_frame_id,
                frame_id = frame.frame_id,
                stream_id = frame.stream_id.0,
                "H.264 frame gap detected; suppressing deltas until a keyframe arrives"
            );
        }

        if frame.keyframe {
            self.waiting_for_keyframe = false;
            self.last_keyframe_request_us = 0;
            self.last_accepted_frame_id = Some(frame.frame_id);
            return (Some(frame), false);
        }
        if self.waiting_for_keyframe {
            return (None, self.keyframe_request_due(now_us));
        }

        self.last_accepted_frame_id = Some(frame.frame_id);
        (Some(frame), false)
    }
}

/// Mouse-move events can arrive substantially faster than the network can
/// usefully deliver them. Keep only the newest unsent position so transient
/// congestion cannot put keyboard and button events behind an unbounded trail
/// of stale pointer positions on the reliable control stream.
#[derive(Clone)]
struct ViewerControlQueue {
    outgoing: mpsc::UnboundedSender<SessionMessage>,
    input: Arc<Mutex<ViewerInputState>>,
    pointer_changed: Arc<Notify>,
}

struct ViewerInputState {
    enabled: bool,
    pending_pointer: Option<SessionMessage>,
}

impl ViewerControlQueue {
    fn new(outgoing: mpsc::UnboundedSender<SessionMessage>) -> Self {
        Self {
            outgoing,
            input: Arc::new(Mutex::new(ViewerInputState {
                enabled: false,
                pending_pointer: None,
            })),
            pointer_changed: Arc::new(Notify::new()),
        }
    }

    fn send(&self, message: SessionMessage) {
        let is_input = matches!(&message, SessionMessage::Input(_));
        let Ok(mut input) = self.input.lock() else {
            return;
        };
        if is_input && !input.enabled {
            return;
        }
        if matches!(
            &message,
            SessionMessage::Input(meshrmm_protocol::RemoteInput::PointerMove { .. })
        ) {
            input.pending_pointer = Some(message);
            drop(input);
            self.pointer_changed.notify_one();
            return;
        }

        let pending_pointer = if matches!(
            &message,
            SessionMessage::Input(
                meshrmm_protocol::RemoteInput::PointerButtonAt { .. }
                    | meshrmm_protocol::RemoteInput::WheelAt { .. }
            )
        ) {
            // The positioned action supersedes any older unsent motion.
            input.pending_pointer.take();
            None
        } else {
            // Preserve pointer-before-action ordering for legacy/non-positioned
            // messages while still coalescing ordinary motion.
            input.pending_pointer.take()
        };
        drop(input);
        if let Some(pending_pointer) = pending_pointer {
            let _ = self.outgoing.send(pending_pointer);
        }
        let _ = self.outgoing.send(message);
    }

    fn flush_pointer(&self) {
        let pending = self.input.lock().ok().and_then(|mut input| {
            input
                .enabled
                .then(|| input.pending_pointer.take())
                .flatten()
        });
        if let Some(message) = pending {
            let _ = self.outgoing.send(message);
        }
    }

    fn set_input_enabled(&self, enabled: bool) {
        if let Ok(mut input) = self.input.lock() {
            input.enabled = enabled;
            if !enabled {
                input.pending_pointer = None;
            }
        }
    }
}

async fn flush_pointer_motion(queue: ViewerControlQueue) {
    loop {
        queue.pointer_changed.notified().await;
        tokio::time::sleep(POINTER_FLUSH_INTERVAL).await;
        queue.flush_pointer();
    }
}

pub async fn run_receiver(config: &Config, bootstrap: SessionBootstrap) -> anyhow::Result<()> {
    let debug = DebugInfo::new(bootstrap.session_id.as_str());
    let url = session_signal_url(&config.server, bootstrap.session_id.as_str())?;
    let socket = authenticated_websocket(url, &bootstrap.signaling_token).await?;
    let (mut signal_writer, mut signal_reader) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
    let peer = create_peer(
        &bootstrap.ice_servers,
        outgoing_tx.clone(),
        state_tx,
        debug.clone(),
    )
    .await?;
    let presenter = Arc::new(Mutex::new(None::<ActivePresenter>));
    let control_channel = Arc::new(Mutex::new(None::<Arc<RTCDataChannel>>));
    let (viewer_control_tx, mut viewer_control_rx) = mpsc::unbounded_channel::<SessionMessage>();
    let viewer_control = ViewerControlQueue::new(viewer_control_tx);
    let (remote_clipboard_tx, mut remote_clipboard_rx) = mpsc::unbounded_channel::<String>();
    let (presentation_failure_tx, mut presentation_failure_rx) =
        mpsc::unbounded_channel::<String>();
    install_data_channel_handler(
        &peer,
        Arc::clone(&presenter),
        Arc::clone(&control_channel),
        viewer_control.clone(),
        remote_clipboard_tx,
        presentation_failure_tx,
        debug.clone(),
    );

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    let startup_started = tokio::time::Instant::now();
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    stats_interval.tick().await;
    let mut activity_interval = tokio::time::interval(SESSION_ACTIVITY_INTERVAL);
    activity_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    activity_interval.tick().await;
    let mut clipboard = match ClipboardSync::new(true) {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(error = %error, "clipboard sync is unavailable for this viewer session");
            None
        }
    };
    let mut clipboard_interval = tokio::time::interval(CLIPBOARD_POLL_INTERVAL);
    clipboard_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut remote_description_set = false;
    let mut pending_candidates = Vec::new();
    // Pointer pacing must not depend on the receiver loop being available: a
    // control-channel send can await long enough for a short movement burst to
    // end. This activity-driven flusher always queues that burst's newest
    // position after the coalescing window.
    let pointer_flusher = tokio::spawn(flush_pointer_motion(viewer_control.clone()));
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
            Some(signal) = outgoing_rx.recv() => {
                signal_writer.send(Message::Text(serde_json::to_string(&signal)?.into())).await?;
            }
            Some(message) = viewer_control_rx.recv() => {
                let channel = control_channel
                    .lock()
                    .ok()
                    .and_then(|channel| channel.clone())
                    .context("control data channel is unavailable")?;
                let bytes = message.encode().context("failed to encode viewer control message")?;
                channel.send(&Bytes::from(bytes)).await
                    .context("failed to send viewer control message")?;
            }
            Some(text) = remote_clipboard_rx.recv() => {
                if let Some(clipboard) = clipboard.as_mut()
                    && let Err(error) = clipboard.apply(text)
                {
                    tracing::warn!(error = %error, "discarding remote clipboard update");
                }
            }
            _ = clipboard_interval.tick(), if session_state == SessionState::Streaming && clipboard.is_some() => {
                let channel_open = control_channel
                    .lock()
                    .ok()
                    .and_then(|channel| channel.clone())
                    .is_some_and(|channel| channel.ready_state() == RTCDataChannelState::Open);
                if channel_open && let Some(clipboard) = clipboard.as_mut() {
                    match clipboard.poll() {
                        Ok(Some(text)) => viewer_control.send(SessionMessage::Clipboard { text }),
                        Ok(None) => {}
                        Err(error) => tracing::warn!(error = %error, "could not synchronize the viewer clipboard"),
                    }
                }
            }
            _ = activity_interval.tick(), if session_state == SessionState::Streaming => {
                outgoing_tx.send(SignalMessage::Activity)?;
            }
            incoming = signal_reader.next() => {
                let Some(incoming) = incoming else {
                    break Err(anyhow::anyhow!("signaling connection closed"));
                };
                match incoming? {
                    Message::Text(text) => {
                        let signal: SignalMessage = serde_json::from_str(text.as_str())?;
                        match signal {
                            SignalMessage::Offer { sdp } => {
                                peer.set_remote_description(RTCSessionDescription::offer(sdp)?).await?;
                                remote_description_set = true;
                                for candidate in pending_candidates.drain(..) {
                                    peer.add_ice_candidate(candidate).await?;
                                }
                                let answer = peer.create_answer(None).await?;
                                peer.set_local_description(answer).await?;
                                let local = peer.local_description().await
                                    .ok_or_else(|| anyhow::anyhow!("WebRTC did not retain its local answer"))?;
                                outgoing_tx.send(SignalMessage::Answer { sdp: local.sdp })?;
                            }
                            SignalMessage::IceCandidate { candidate, sdp_mid, sdp_mline_index, username_fragment } => {
                                let candidate = RTCIceCandidateInit { candidate, sdp_mid, sdp_mline_index, username_fragment };
                                if remote_description_set {
                                    peer.add_ice_candidate(candidate).await?;
                                } else {
                                    pending_candidates.push(candidate);
                                }
                            }
                            SignalMessage::PeerLeft => {
                                break Err(anyhow::anyhow!("Agent disconnected from the remote session"));
                            }
                            SignalMessage::Error { message } => break Err(anyhow::anyhow!(message)),
                            _ => {}
                        }
                    }
                    Message::Ping(payload) => signal_writer.send(Message::Pong(payload)).await?,
                    Message::Close(_) => break Ok(()),
                    _ => {}
                }
            }
            Some(state) = state_rx.recv() => {
                tracing::info!(?state, session_id = %bootstrap.session_id, "WebRTC connection state changed");
                debug.set_connection_state(format!("{state:?}").to_ascii_lowercase());
                if state == RTCPeerConnectionState::Connected
                    && session_state == SessionState::Connecting
                {
                    session_state = session_state.transition(SessionState::Streaming)?;
                    outgoing_tx.send(SignalMessage::Activity)?;
                }
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed | RTCPeerConnectionState::Disconnected) {
                    break Err(anyhow::anyhow!("WebRTC connection ended in state {state:?}"));
                }
            }
            Some(error) = presentation_failure_rx.recv() => {
                break Err(anyhow::anyhow!(error));
            }
            _ = stats_interval.tick() => {
                let presenter_missing = presenter.lock().is_ok_and(|guard| guard.is_none());
                if presenter_missing && startup_started.elapsed() >= std::time::Duration::from_secs(30) {
                    break Err(anyhow::anyhow!(
                        "timed out waiting 30 seconds for the remote video stream; check the Agent's WebRTC and ICE logs"
                    ));
                }
                update_network_stats(&peer, &debug).await;
                let ended = presenter
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().and_then(|active| active.presenter.poll_ended()));
                if let Some(ended) = ended {
                    break ended.map_err(anyhow::Error::msg);
                }
            },
            _ = tokio::signal::ctrl_c() => break Ok(()),
            }
        }
    }
    .await;
    pointer_flusher.abort();
    let _ = pointer_flusher.await;

    if matches!(
        session_state,
        SessionState::Requested
            | SessionState::Signaling
            | SessionState::Connecting
            | SessionState::Streaming
    ) {
        session_state = session_state.transition(SessionState::Closing)?;
    }
    if let Ok(mut guard) = presenter.lock()
        && let Some(mut active) = guard.take()
    {
        active.presenter.stop();
    }
    let mut result = result;
    if let Err(error) = peer.close().await {
        tracing::warn!(error = %error, "WebRTC peer did not close cleanly");
        if result.is_ok() {
            result = Err(error).context("failed to close WebRTC peer");
        }
    }
    session_state = session_state.transition(SessionState::Idle)?;
    tracing::info!(?session_state, "remote viewer session stopped");
    result
}

async fn update_network_stats(peer: &RTCPeerConnection, debug: &DebugInfo) {
    let reports = peer.get_stats().await.reports;
    let mut candidates = HashMap::new();
    for report in reports.values() {
        if let StatsReportType::LocalCandidate(candidate)
        | StatsReportType::RemoteCandidate(candidate) = report
        {
            let relay = candidate.candidate_type.to_string() == "relay";
            let relay_protocol = if candidate.relay_protocol.is_empty() {
                String::new()
            } else {
                format!(" via {}", candidate.relay_protocol)
            };
            candidates.insert(
                candidate.id.clone(),
                (
                    format!(
                        "{} {} {}:{}{}",
                        candidate.candidate_type,
                        candidate.network_type,
                        candidate.ip,
                        candidate.port,
                        relay_protocol,
                    ),
                    relay,
                ),
            );
        }
    }
    for report in reports.into_values() {
        if let StatsReportType::CandidatePair(pair) = report
            && pair.nominated
        {
            let local = candidates
                .get(&pair.local_candidate_id)
                .cloned()
                .unwrap_or_else(|| (pair.local_candidate_id.clone(), false));
            let remote = candidates
                .get(&pair.remote_candidate_id)
                .cloned()
                .unwrap_or_else(|| (pair.remote_candidate_id.clone(), false));
            let path = if local.1 || remote.1 {
                "TURN relay"
            } else {
                "P2P / direct"
            };
            debug.update_network(
                pair.current_round_trip_time * 1_000.0,
                pair.available_incoming_bitrate,
                pair.packets_received,
                pair.bytes_received,
                local.0,
                remote.0,
                path,
            );
            tracing::info!(
                rtt_ms = pair.current_round_trip_time * 1_000.0,
                available_incoming_bitrate = pair.available_incoming_bitrate,
                packets_received = pair.packets_received,
                bytes_received = pair.bytes_received,
                "WebRTC network statistics"
            );
        }
    }
}

async fn create_peer(
    ice_servers: &[IceServer],
    outgoing: mpsc::UnboundedSender<SignalMessage>,
    state: mpsc::UnboundedSender<RTCPeerConnectionState>,
    debug: DebugInfo,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let peer = Arc::new(
        APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration {
                ice_servers: ice_servers
                    .iter()
                    .map(|server| RTCIceServer {
                        urls: server.urls.clone(),
                        username: server.username.clone().unwrap_or_default(),
                        credential: server.credential.clone().unwrap_or_default(),
                    })
                    .collect(),
                ..Default::default()
            })
            .await?,
    );
    peer.sctp()
        .transport()
        .ice_transport()
        .on_selected_candidate_pair_change(Box::new(move |pair| {
            let debug = debug.clone();
            Box::pin(async move {
                let pair = pair.to_string();
                let path = if pair.to_ascii_lowercase().contains("relay") {
                    "TURN relay"
                } else {
                    "P2P / direct"
                };
                debug.set_selected_pair(path, &pair);
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

fn install_data_channel_handler(
    peer: &Arc<RTCPeerConnection>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    control_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    viewer_control: ViewerControlQueue,
    remote_clipboard: mpsc::UnboundedSender<String>,
    presentation_failure: mpsc::UnboundedSender<String>,
    debug: DebugInfo,
) {
    peer.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let presenter = Arc::clone(&presenter);
        let control_channel = Arc::clone(&control_channel);
        let viewer_control = viewer_control.clone();
        let remote_clipboard = remote_clipboard.clone();
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        Box::pin(async move {
            debug.set_data_channel(channel.label(), "open");
            match channel.label() {
                CONTROL_CHANNEL_LABEL => {
                    if let Ok(mut active) = control_channel.lock() {
                        *active = Some(Arc::clone(&channel));
                    }
                    install_control_handler(
                        channel,
                        presenter,
                        viewer_control,
                        remote_clipboard,
                        presentation_failure,
                        debug,
                    )
                }
                "meshrmm-video-v1" => {
                    install_video_handler(channel, presenter, viewer_control, debug)
                }
                label => tracing::warn!(label, "ignoring unknown WebRTC data channel"),
            }
        })
    }));
}

fn install_control_handler(
    channel: Arc<RTCDataChannel>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    viewer_control: ViewerControlQueue,
    remote_clipboard: mpsc::UnboundedSender<String>,
    presentation_failure: mpsc::UnboundedSender<String>,
    debug: DebugInfo,
) {
    {
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        channel.on_close(Box::new(move || {
            let presentation_failure = presentation_failure.clone();
            let debug = debug.clone();
            Box::pin(async move {
                debug.set_data_channel(CONTROL_CHANNEL_LABEL, "closed");
                let _ = presentation_failure.send(
                    "remote input/control channel closed while video was still active".into(),
                );
            })
        }));
    }
    let cursor_shape = Arc::new(Mutex::new(CursorShape::Default));
    channel.on_message(Box::new(move |message| {
        let presenter = Arc::clone(&presenter);
        let cursor_shape = Arc::clone(&cursor_shape);
        let viewer_control = viewer_control.clone();
        let remote_clipboard = remote_clipboard.clone();
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        Box::pin(async move {
            match SessionMessage::decode(&message.data) {
                Ok(SessionMessage::DisplayConfiguration {
                    displays,
                    active_display_id,
                    stream_id,
                    format,
                }) => {
                    let Some(active_display) = displays
                        .iter()
                        .find(|display| display.id == active_display_id)
                        .cloned()
                    else {
                        tracing::error!(display_id = active_display_id.0, "Agent selected an unknown display");
                        return;
                    };
                    let message_queue = viewer_control.clone();
                    let input_gate = viewer_control.clone();
                    let sink = ControlSink::new(
                        move |message| message_queue.send(message),
                        move |enabled| input_gate.set_input_enabled(enabled),
                    );
                    debug.configure_stream(
                        active_display.name.clone(),
                        format.width,
                        format.height,
                        format.frames_per_second,
                        format!("{:?}", format.codec),
                    );
                    match Presenter::start(
                        format,
                        active_display.clone(),
                        displays,
                        sink,
                        debug.clone(),
                    ) {
                        Ok(new_presenter) => {
                            if let Ok(shape) = cursor_shape.lock() {
                                new_presenter.set_cursor_shape(*shape);
                            }
                            let mut old = presenter
                                .lock()
                                .ok()
                                .and_then(|mut guard| guard.replace(ActivePresenter {
                                    stream_id,
                                    presenter: new_presenter,
                                }));
                            if let Some(old) = old.as_mut() {
                                old.presenter.stop();
                            }
                            let request = SessionMessage::RequestKeyframe { stream_id };
                            viewer_control.send(request);
                            tracing::info!(display_id = active_display_id.0, display_name = %active_display.name, width = format.width, height = format.height, fps = format.frames_per_second, codec = ?format.codec, "remote control stream configured");
                        }
                        Err(error) => {
                            let message = format!(
                                "hardware decoder/presenter initialization failed: {error:#}"
                            );
                            tracing::error!(error = %error, "hardware decoder/presenter initialization failed");
                            let _ = presentation_failure.send(message);
                        }
                    }
                }
                Ok(SessionMessage::Stop { reason }) => tracing::info!(reason, "Agent stopped stream"),
                Ok(SessionMessage::CursorShape { shape }) => {
                    if let Ok(mut current) = cursor_shape.lock() {
                        *current = shape;
                    }
                    if let Ok(guard) = presenter.lock()
                        && let Some(active) = guard.as_ref()
                    {
                        active.presenter.set_cursor_shape(shape);
                    }
                }
                Ok(SessionMessage::Clipboard { text }) => {
                    let _ = remote_clipboard.send(text);
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(error = %error, "discarding invalid control message"),
            }
        })
    }));
}

fn install_video_handler(
    channel: Arc<RTCDataChannel>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    viewer_control: ViewerControlQueue,
    debug: DebugInfo,
) {
    {
        let debug = debug.clone();
        channel.on_close(Box::new(move || {
            let debug = debug.clone();
            Box::pin(async move {
                debug.set_data_channel("meshrmm-video-v1", "closed");
            })
        }));
    }
    let receive_state = Arc::new(tokio::sync::Mutex::new(VideoReceiveState::new()));
    channel.on_message(Box::new(move |message| {
        let receive_state = Arc::clone(&receive_state);
        let presenter = Arc::clone(&presenter);
        let viewer_control = viewer_control.clone();
        let debug = debug.clone();
        Box::pin(async move {
            let received_at_us = monotonic_timestamp_us();
            let packet = match VideoPacket::decode(&message.data) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::warn!(error = %error, "discarding invalid video packet");
                    return;
                }
            };
            let packet_stream_id = packet.stream_id;
            let mut receive_state = receive_state.lock().await;
            let incomplete_before = receive_state.reassembler.stats().incomplete_frames_dropped;
            let outcome = receive_state.reassembler.push(packet, received_at_us);
            let stats = receive_state.reassembler.stats();
            let mut request_keyframe = false;
            if stats.incomplete_frames_dropped > incomplete_before {
                receive_state.mark_loss();
                request_keyframe = receive_state.keyframe_request_due(received_at_us);
            }
            let completed = if let ReassemblyOutcome::Completed(frame) = outcome {
                let (frame, request) = receive_state.accept_completed(frame, received_at_us);
                request_keyframe |= request;
                frame
            } else {
                None
            };
            drop(receive_state);
            if request_keyframe {
                viewer_control.send(SessionMessage::RequestKeyframe {
                    stream_id: packet_stream_id,
                });
                tracing::warn!(
                    stream_id = packet_stream_id.0,
                    "requested an H.264 recovery keyframe"
                );
            }
            if let Some(frame) = completed {
                let encode_us = frame
                    .encode_complete_timestamp_us
                    .saturating_sub(frame.capture_timestamp_us);
                debug.record_received_frame(
                    encode_us,
                    stats.completed_frames,
                    stats.incomplete_frames_dropped,
                    stats.stale_packets_dropped,
                    stats.duplicate_packets,
                    stats.invalid_packets,
                );
                if let Ok(guard) = presenter.lock()
                    && let Some(active) = guard.as_ref()
                    && active.stream_id == frame.stream_id
                {
                    active.presenter.publish(frame, received_at_us);
                }
                tracing::trace!(encode_us, received_at_us, "encoded frame reassembled");
                if stats.completed_frames.is_multiple_of(120) {
                    tracing::info!(
                        frames_received = stats.completed_frames,
                        incomplete_frames_dropped = stats.incomplete_frames_dropped,
                        stale_packets_dropped = stats.stale_packets_dropped,
                        duplicate_packets = stats.duplicate_packets,
                        invalid_packets = stats.invalid_packets,
                        "video reassembly statistics"
                    );
                }
            }
        })
    }));
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{DisplayId, PointerButton, RemoteInput};

    use super::*;

    fn pointer(x: u16, y: u16) -> SessionMessage {
        SessionMessage::Input(RemoteInput::PointerMove {
            display_id: DisplayId(1),
            x,
            y,
        })
    }

    fn active_queue() -> (ViewerControlQueue, mpsc::UnboundedReceiver<SessionMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx);
        queue.set_input_enabled(true);
        (queue, rx)
    }

    fn encoded_frame(frame_id: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(7),
            frame_id,
            capture_timestamp_us: 1,
            encode_complete_timestamp_us: 2,
            send_timestamp_us: 3,
            keyframe,
            data: vec![1],
        }
    }

    #[test]
    fn video_recovery_suppresses_deltas_across_a_frame_gap() {
        let mut state = VideoReceiveState::new();

        let (frame, request) = state.accept_completed(encoded_frame(10, false), 1_000);
        assert!(frame.is_none());
        assert!(request);

        let (frame, request) = state.accept_completed(encoded_frame(11, true), 2_000);
        assert_eq!(frame.unwrap().frame_id, 11);
        assert!(!request);

        let (frame, request) = state.accept_completed(encoded_frame(12, false), 3_000);
        assert_eq!(frame.unwrap().frame_id, 12);
        assert!(!request);

        let (frame, request) = state.accept_completed(encoded_frame(14, false), 4_000);
        assert!(frame.is_none());
        assert!(request);

        let (frame, request) = state.accept_completed(encoded_frame(15, false), 5_000);
        assert!(frame.is_none());
        assert!(!request, "recovery requests are rate limited");

        let (frame, request) = state.accept_completed(encoded_frame(16, true), 6_000);
        assert_eq!(frame.unwrap().frame_id, 16);
        assert!(!request);
    }

    #[test]
    fn video_recovery_retries_a_missing_keyframe() {
        let mut state = VideoReceiveState::new();
        let (_, first_request) = state.accept_completed(encoded_frame(1, false), 1_000);
        let (_, retry) =
            state.accept_completed(encoded_frame(2, false), 1_000 + KEYFRAME_RETRY_INTERVAL_US);
        assert!(first_request);
        assert!(retry);
    }

    #[test]
    fn pointer_motion_is_coalesced_to_the_latest_position() {
        let (queue, mut rx) = active_queue();

        queue.send(pointer(10, 20));
        queue.send(pointer(30, 40));
        assert!(rx.try_recv().is_err());

        queue.flush_pointer();
        assert_eq!(rx.try_recv().unwrap(), pointer(30, 40));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pointer_position_is_flushed_before_a_button_event() {
        let (queue, mut rx) = active_queue();
        let button = SessionMessage::Input(RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: PointerButton::Left,
            pressed: true,
        });

        queue.send(pointer(30, 40));
        queue.send(button.clone());

        assert_eq!(rx.try_recv().unwrap(), pointer(30, 40));
        assert_eq!(rx.try_recv().unwrap(), button);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn positioned_button_supersedes_pending_pointer_motion() {
        let (queue, mut rx) = active_queue();
        let button = SessionMessage::Input(RemoteInput::PointerButtonAt {
            display_id: DisplayId(1),
            x: 50,
            y: 60,
            button: PointerButton::Left,
            pressed: true,
        });

        queue.send(pointer(30, 40));
        queue.send(button.clone());

        assert_eq!(rx.try_recv().unwrap(), button);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn input_is_discarded_until_the_viewer_is_foreground() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx);
        let button = SessionMessage::Input(RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: PointerButton::Left,
            pressed: true,
        });

        queue.send(pointer(10, 20));
        queue.send(button);
        queue.flush_pointer();
        assert!(rx.try_recv().is_err());

        queue.set_input_enabled(true);
        queue.send(pointer(30, 40));
        queue.flush_pointer();
        assert_eq!(rx.try_recv().unwrap(), pointer(30, 40));
    }

    #[test]
    fn backgrounding_discards_pending_pointer_motion() {
        let (queue, mut rx) = active_queue();

        queue.send(pointer(30, 40));
        queue.set_input_enabled(false);
        queue.flush_pointer();

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn movement_burst_automatically_flushes_its_final_position() {
        let (queue, mut rx) = active_queue();
        let flusher = tokio::spawn(flush_pointer_motion(queue.clone()));

        queue.send(pointer(10, 20));
        queue.send(pointer(30, 40));
        queue.send(pointer(50, 60));

        let sent = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("pointer flush timed out")
            .expect("pointer queue closed");
        assert_eq!(sent, pointer(50, 60));
        assert!(rx.try_recv().is_err());

        flusher.abort();
    }
}
