use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, ChromaMode, Codec, CursorShape, EncodedFrame, FrameReassembler,
    IceServer, QualityPreset, ReassemblyConfig, ReassemblyOutcome, SessionBootstrap,
    SessionMessage, SessionState, SignalMessage, VideoPacket, VideoProfile, VideoStreamId,
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
use meshrmm_session_transport::{ServiceRoute, SERVICE_CHANNELS, CLIPBOARD_CHANNEL, FILE_CHANNEL, CHAT_CHANNEL};
use crate::config::Config;
use crate::debug::DebugInfo;
use crate::platform::{ControlSink, Presenter, monotonic_timestamp_us};
use crate::signaling::{authenticated_websocket, session_signal_url};

const SESSION_ACTIVITY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const CLIPBOARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const POINTER_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);
const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;
const NEGOTIATION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const DISCONNECTED_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);
const SIGNAL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
const SIGNAL_LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

struct ActivePresenter {
    stream_id: VideoStreamId,
    #[cfg(target_os = "macos")]
    format: meshrmm_protocol::VideoFormat,
    profile: VideoProfile,
    presenter: Presenter,
}

#[derive(Clone)]
struct ReceiverLifecycle {
    presentation_failure: mpsc::UnboundedSender<String>,
    shutting_down: Arc<AtomicBool>,
}

/// Viewer choices that should survive rebuilding the signaling and WebRTC
/// transports after a network change or remote reboot.
#[derive(Clone, Default)]
pub struct ViewerResumeState {
    technician_blocked: Arc<AtomicBool>,
    quality: Arc<Mutex<QualityPreset>>,
    chroma: Arc<Mutex<ChromaMode>>,
    display_id: Arc<Mutex<Option<meshrmm_protocol::DisplayId>>>,
}

#[cfg(target_os = "macos")]
fn can_reset_presenter_in_place(
    current: meshrmm_protocol::VideoFormat,
    format: meshrmm_protocol::VideoFormat,
) -> bool {
    // The sample-buffer layer reads dimensions from the replacement keyframe.
    // Display identity and resolution do not require a new native window.
    current.codec == format.codec && current.pixel_format == format.pixel_format
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

    fn poll_recovery(&mut self, now_us: u64) -> Option<VideoStreamId> {
        // The last packet of a burst can be lost on a static desktop. Expire
        // its incomplete frame even when no subsequent packets arrive.
        if self.reassembler.expire_stale(now_us) {
            self.mark_loss();
            tracing::warn!("incomplete video frame expired while waiting for more packets");
        }
        if self.waiting_for_keyframe && self.keyframe_request_due(now_us) {
            self.stream_id
        } else {
            None
        }
    }

    fn observe_stream(&mut self, stream_id: VideoStreamId) {
        if self.stream_id != Some(stream_id) {
            self.stream_id = Some(stream_id);
            self.last_accepted_frame_id = None;
            self.waiting_for_keyframe = true;
            self.last_keyframe_request_us = 0;
        }
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
        self.observe_stream(frame.stream_id);

        let gap = self
            .last_accepted_frame_id
            .is_some_and(|last| frame.frame_id != last.wrapping_add(1));
        if gap && !frame.keyframe {
            self.waiting_for_keyframe = true;
            tracing::warn!(
                last_frame_id = ?self.last_accepted_frame_id,
                frame_id = frame.frame_id,
                stream_id = frame.stream_id.0,
                "video frame gap detected; suppressing deltas until a keyframe arrives"
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
    maintenance: Arc<Mutex<crate::platform::MaintenanceState>>,
    files: meshrmm_file_transfer::TransferSession,
    chat: meshrmm_chat::ChatSession,
    outgoing: mpsc::UnboundedSender<SessionMessage>,
    service_senders: Arc<Mutex<HashMap<&'static str, mpsc::Sender<SessionMessage>>>>,
    input: Arc<Mutex<ViewerInputState>>,
    pointer_changed: Arc<Notify>,
    resume_state: ViewerResumeState,
}

struct ViewerInputState {
    enabled: bool,
    pending_pointer: Option<SessionMessage>,
}

impl ViewerControlQueue {
    fn new(
        outgoing: mpsc::UnboundedSender<SessionMessage>,
        resume_state: ViewerResumeState,
    ) -> Self {
        Self {
            maintenance: Arc::new(Mutex::new(crate::platform::MaintenanceState::default())),
            files: meshrmm_file_transfer::TransferSession::new(),
            chat: meshrmm_chat::ChatSession::default(),
            outgoing,
            service_senders: Arc::new(Mutex::new(HashMap::new())),
            input: Arc::new(Mutex::new(ViewerInputState {
                enabled: false,
                pending_pointer: None,
            })),
            pointer_changed: Arc::new(Notify::new()),
            resume_state,
        }
    }

    fn send(&self, message: SessionMessage) {
        if let SessionMessage::SelectDisplay { display_id } = &message
            && let Ok(mut selected) = self.resume_state.display_id.lock()
        {
            *selected = Some(*display_id);
        }
        let is_input = matches!(&message, SessionMessage::Input(_));
        let Ok(mut input) = self.input.lock() else {
            return;
        };
        if is_input && (!input.enabled || self.resume_state.technician_blocked.load(Ordering::SeqCst)) {
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
        if let Some(label) = meshrmm_session_transport::service_label(&message)
            && let Some(sender) = self.service_senders.lock().ok().and_then(|map| map.get(label).cloned()) {
            if sender.try_send(message).is_err() { tracing::warn!(label, "viewer service queue full or closed"); }
        } else {
            let _ = self.outgoing.send(message);
        }
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

pub async fn run_receiver(
    config: &Config,
    bootstrap: SessionBootstrap,
    resume_state: ViewerResumeState,
) -> anyhow::Result<()> {
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
    let (viewer_control_tx, viewer_control_rx) = mpsc::unbounded_channel::<SessionMessage>();
    let viewer_control = ViewerControlQueue::new(viewer_control_tx, resume_state.clone());
    let (presentation_failure_tx, mut presentation_failure_rx) =
        mpsc::unbounded_channel::<String>();
    let lifecycle = ReceiverLifecycle {
        presentation_failure: presentation_failure_tx,
        shutting_down: Arc::new(AtomicBool::new(false)),
    };
    let (remote_text_tx, service_routes, _services) = start_viewer_services(
        viewer_control.clone(), Arc::clone(&control_channel), viewer_control_rx, lifecycle.clone())?;
    install_data_channel_handler(
        &peer,
        Arc::clone(&presenter),
        Arc::clone(&control_channel),
        viewer_control.clone(),
        remote_text_tx,
        service_routes,
        debug.clone(),
        lifecycle.clone(),
    );

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    outgoing_tx.send(SignalMessage::Activity)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    let mut presenter_missing_since = Some(tokio::time::Instant::now());
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    stats_interval.tick().await;
    let mut heartbeat_interval = tokio::time::interval(SIGNAL_HEARTBEAT_INTERVAL);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat_interval.tick().await;
    let mut activity_interval = tokio::time::interval(SESSION_ACTIVITY_INTERVAL);
    activity_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    activity_interval.tick().await;
    let mut negotiation_interval = tokio::time::interval(NEGOTIATION_RETRY_INTERVAL);
    negotiation_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    negotiation_interval.tick().await;
    let mut remote_description_set = false;
    let mut pending_candidates = Vec::new();
    let mut disconnected_since = None::<tokio::time::Instant>;
    let mut last_signal_message = tokio::time::Instant::now();
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
            _ = activity_interval.tick() => {
                outgoing_tx.send(SignalMessage::Activity)?;
            }
            _ = negotiation_interval.tick(), if session_state == SessionState::Connecting => {
                outgoing_tx.send(SignalMessage::Ready)?;
            }
            incoming = signal_reader.next() => {
                let Some(incoming) = incoming else {
                    break Err(anyhow::anyhow!("signaling connection closed"));
                };
                last_signal_message = tokio::time::Instant::now();
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
                                    if pending_candidates.len() >= 256 { anyhow::bail!("too many pending ICE candidates"); }
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
                    Message::Close(frame) => {
                        break Err(meshrmm_signaling_client::signaling_close_error(frame));
                    }
                    _ => {}
                }
            }
            _ = heartbeat_interval.tick() => {
                if last_signal_message.elapsed() >= SIGNAL_LIVENESS_TIMEOUT {
                    break Err(anyhow::anyhow!(
                        "signaling server did not respond for {} seconds",
                        SIGNAL_LIVENESS_TIMEOUT.as_secs()
                    ));
                }
                signal_writer.send(Message::Ping(Default::default())).await
                    .context("failed to send signaling heartbeat")?;
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
                if state == RTCPeerConnectionState::Connected {
                    disconnected_since = None;
                } else if state == RTCPeerConnectionState::Disconnected {
                    disconnected_since.get_or_insert_with(tokio::time::Instant::now);
                }
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                    break Err(anyhow::anyhow!("WebRTC connection ended in state {state:?}"));
                }
            }
            Some(error) = presentation_failure_rx.recv() => {
                tracing::error!(%error, "viewer presentation path reported a terminal failure");
                break Err(anyhow::anyhow!(error));
            }
            _ = stats_interval.tick() => {
                if disconnected_since.is_some_and(|since| since.elapsed() >= DISCONNECTED_GRACE_PERIOD) {
                    break Err(anyhow::anyhow!(
                        "WebRTC remained disconnected for {} seconds",
                        DISCONNECTED_GRACE_PERIOD.as_secs()
                    ));
                }
                let presenter_missing = presenter.lock().is_ok_and(|guard| guard.is_none());
                if presenter_missing {
                    let waiting_since = presenter_missing_since
                        .get_or_insert_with(tokio::time::Instant::now);
                    if waiting_since.elapsed() >= std::time::Duration::from_secs(30) {
                        break Err(anyhow::anyhow!(
                            "timed out waiting 30 seconds for the remote video stream; check the Agent's WebRTC and ICE logs"
                        ));
                    }
                } else {
                    presenter_missing_since = None;
                }
                update_network_stats(&peer, &debug).await;
                let ended = presenter
                    .lock()
                    .ok()
                    .and_then(|guard| {
                        guard.as_ref().and_then(|active| {
                            active
                                .presenter
                                .poll_ended()
                                .map(|ended| (active.profile, ended))
                        })
                    });
                if let Some((profile, ended)) = ended {
                    match (profile, ended) {
                        (profile, Err(reason))
                            if profile
                                != (VideoProfile {
                                    codec: Codec::H264,
                                    chroma: ChromaMode::Yuv420,
                                }) =>
                        {
                            tracing::warn!(%reason, ?profile, "video presentation failed; requesting profile fallback");
                            viewer_control.send(SessionMessage::VideoProfileRejected {
                                profile,
                                reason,
                            });
                            if let Ok(mut guard) = presenter.lock()
                                && let Some(mut failed) = guard.take()
                            {
                                failed.presenter.stop();
                            }
                            presenter_missing_since = Some(tokio::time::Instant::now());
                        }
                        (_, ended) => break ended.map_err(anyhow::Error::msg),
                    }
                }
            },
            _ = tokio::signal::ctrl_c() => break Ok(()),
            }
        }
    }
    .await;
    lifecycle.shutting_down.store(true, Ordering::Release);
    pointer_flusher.abort();
    let _ = pointer_flusher.await;

    if result.is_ok() {
        let end_message = serde_json::to_string(&SignalMessage::EndSession)?;
        if let Err(error) = signal_writer.send(Message::Text(end_message.into())).await {
            tracing::warn!(error = %error, "failed to notify the server that the viewer ended the session");
        }
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
    if let Ok(mut guard) = presenter.lock()
        && let Some(mut active) = guard.take()
    {
        active.presenter.stop();
    }
    viewer_control.chat.set_available(false);
    let mut result = result;
    if let Err(error) = peer.close().await {
        tracing::warn!(error = %error, "WebRTC peer did not close cleanly");
        if result.is_ok() {
            result = Err(error).context("failed to close WebRTC peer");
        }
    }
    session_state = session_state.transition(SessionState::Idle)?;
    match &result {
        Ok(()) => tracing::info!(?session_state, "remote viewer session stopped cleanly"),
        Err(error) => {
            tracing::error!(error = ?error, ?session_state, "remote viewer session stopped with an error")
        }
    }
    result
}

async fn update_network_stats(peer: &RTCPeerConnection, debug: &DebugInfo) {
    let reports = peer.get_stats().await.reports;
    let mut candidates = HashMap::new();
    for report in reports.values() {
        if let StatsReportType::DataChannel(channel) = report {
            // ICE candidate-pair counters are not populated by webrtc-ice.
            // Data-channel counters measure the actual video/control traffic.
            tracing::info!(
                label = %channel.label,
                state = ?channel.state,
                messages_received = channel.messages_received,
                bytes_received = channel.bytes_received,
                messages_sent = channel.messages_sent,
                bytes_sent = channel.bytes_sent,
                "WebRTC data channel statistics"
            );
        }
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

#[derive(Clone)]
struct ServiceInbox(HashMap<&'static str, mpsc::Sender<SessionMessage>>);
impl ServiceInbox {
    fn send(&self, message: SessionMessage) {
        if let Some(label) = meshrmm_session_transport::service_label(&message)
            && let Some(sender) = self.0.get(label)
            && sender.try_send(message).is_err() {
                tracing::warn!(label, "viewer incoming service queue full or closed");
            }
    }
}
struct ViewerServices {
    stopping: Arc<AtomicBool>,
    tasks: Vec<tokio::task::AbortHandle>,
}
impl Drop for ViewerServices {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for task in &self.tasks { task.abort(); }
    }
}

async fn wait_control_channel(control: &Arc<Mutex<Option<Arc<RTCDataChannel>>>>) -> Arc<RTCDataChannel> {
    loop {
        if let Some(channel) = control.lock().ok().and_then(|c| c.clone())
            && channel.ready_state() == RTCDataChannelState::Open { return channel; }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

type ViewerServiceSetup = (ServiceInbox, HashMap<&'static str, Arc<ServiceRoute>>, ViewerServices);
fn start_viewer_services(
    viewer: ViewerControlQueue,
    control: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    mut controls: mpsc::UnboundedReceiver<SessionMessage>,
    lifecycle: ReceiverLifecycle,
) -> anyhow::Result<ViewerServiceSetup> {
    let mut owner = ViewerServices { stopping: lifecycle.shutting_down.clone(), tasks: Vec::new() };
    let control_writer = control.clone();
    let errors = lifecycle.presentation_failure.clone();
    let writer = tokio::spawn(async move {
        while let Some(message) = controls.recv().await {
            let channel = wait_control_channel(&control_writer).await;
            if let Err(error) = meshrmm_session_transport::send(&channel, message).await {
                let _ = errors.send(format!("input/control send failed: {error:#}"));
                break;
            }
        }
    });
    owner.tasks.push(writer.abort_handle());
    let mut inbox = HashMap::new();
    let mut routes = HashMap::new();
    for label in SERVICE_CHANNELS {
        let route = Arc::new(ServiceRoute::default());
        routes.insert(label, route.clone());
        let (outgoing, mut pending) = mpsc::channel::<SessionMessage>(if label == CLIPBOARD_CHANNEL { 1024 } else { 64 });
        viewer.service_senders.lock().unwrap().insert(label, outgoing);
        let fallback = control.clone();
        let writer = tokio::spawn(async move {
            let fallback = wait_control_channel(&fallback).await;
            let channel = route.resolve(fallback).await;
            while let Some(message) = pending.recv().await {
                // Bound bulk SCTP backlog; each stream waits independently.
                while channel.buffered_amount().await >= 64 * 1024 {
                    if channel.ready_state() != RTCDataChannelState::Open { return; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                if let Err(error) = meshrmm_session_transport::send(&channel, message).await {
                    tracing::warn!(label, %error, "viewer service send failed");
                    break;
                }
            }
        });
        owner.tasks.push(writer.abort_handle());
        let (incoming, mut messages) = mpsc::channel(1024);
        inbox.insert(label, incoming);
        let viewer = viewer.clone();
        let control = control.clone();
        let stopping = lifecycle.shutting_down.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new().name(format!("viewer-{label}")).spawn(move || {
            runtime.block_on(async move {
                let mut clipboard = if label == CLIPBOARD_CHANNEL { ClipboardSync::new(true).ok() } else { None };
                let mut receiver = meshrmm_protocol::ClipboardReceiver::default();
                let mut outgoing = std::collections::VecDeque::new();
                let mut announced = false;
                let mut poll = tokio::time::interval(std::time::Duration::from_millis(5));
                poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut clipboard_poll = tokio::time::Instant::now();
                while !stopping.load(Ordering::Acquire) {
                    tokio::select! {
                        message = messages.recv() => {
                            let Some(message) = message else { break; };
                            match message {
                                SessionMessage::FileTransfer(message) => viewer.files.receive(message),
                                SessionMessage::ChatAvailable => viewer.chat.set_available(true),
                                SessionMessage::Chat { text } => { viewer.chat.set_available(true); viewer.chat.receive(text); },
                                message => match receiver.receive(message) {
                                    Ok(Some(content)) => {
                                        outgoing.clear();
                                        if let Some(clipboard) = clipboard.as_mut()
                                            && let Err(error) = clipboard.apply(content) { tracing::warn!(%error, "viewer clipboard apply failed"); }
                                    }
                                    Ok(None) => {},
                                    Err(error) => tracing::warn!(%error, "invalid viewer clipboard payload"),
                                },
                            }
                        }
                        _ = poll.tick() => {
                            let open = control.lock().ok().and_then(|c| c.clone()).is_some_and(|c| c.ready_state() == RTCDataChannelState::Open);
                            if !open { continue; }
                            match label {
                                FILE_CHANNEL => { if let Some(message) = viewer.files.poll() { viewer.send(SessionMessage::FileTransfer(message)); } }
                                CHAT_CHANNEL => {
                                    if !announced { viewer.send(SessionMessage::ChatAvailable); announced = true; }
                                    if let Some(text) = viewer.chat.poll() { viewer.send(SessionMessage::Chat { text }); }
                                }
                                CLIPBOARD_CHANNEL => {
                                    if clipboard_poll.elapsed() >= CLIPBOARD_POLL_INTERVAL {
                                        clipboard_poll = tokio::time::Instant::now();
                                        if let Some(clipboard) = clipboard.as_mut() {
                                            match clipboard.poll().and_then(|c| Ok(c.map(|c| c.messages()).transpose()?)) {
                                                Ok(Some(messages)) => outgoing = messages.into(),
                                                Ok(None) => {},
                                                Err(error) => tracing::warn!(%error, "viewer clipboard poll failed"),
                                            }
                                        }
                                    }
                                    if let Some(message) = outgoing.pop_front() { viewer.send(message); }
                                }
                                _ => {},
                            }
                        }
                    }
                }
                if label == CHAT_CHANNEL { viewer.chat.set_available(false); }
            });
        })?;
    }
    Ok((ServiceInbox(inbox), routes, owner))
}

fn install_data_channel_handler(
    peer: &Arc<RTCPeerConnection>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    control_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    viewer_control: ViewerControlQueue,
    remote_text: ServiceInbox,
    service_routes: HashMap<&'static str, Arc<ServiceRoute>>,
    debug: DebugInfo,
    lifecycle: ReceiverLifecycle,
) {
    peer.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let presenter = Arc::clone(&presenter);
        let control_channel = Arc::clone(&control_channel);
        let viewer_control = viewer_control.clone();
        let remote_text = remote_text.clone();
        let service_routes = service_routes.clone();
        let debug = debug.clone();
        let lifecycle = lifecycle.clone();
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
                        remote_text,
                        lifecycle.presentation_failure,
                        debug,
                        lifecycle.shutting_down,
                    )
                }
                label if SERVICE_CHANNELS.contains(&label) => {
                    let route = service_routes.get(label).unwrap().clone();
                    route.attach(channel.clone());
                    let label = channel.label().to_owned();
                    channel.on_message(Box::new(move |message| {
                        let route = route.clone();
                        let remote_text = remote_text.clone();
                        let label = label.clone();
                        Box::pin(async move {
                            match SessionMessage::decode(&message.data) {
                                Ok(SessionMessage::ServiceChannelReady) => route.peer_ready(),
                                Ok(message) if meshrmm_session_transport::service_label(&message) == Some(label.as_str()) => remote_text.send(message),
                                _ => tracing::warn!(label, "invalid service channel message"),
                            }
                        })
                    }));
                    meshrmm_session_transport::announce(channel);
                }
                "meshrmm-video-v1" => {
                    install_video_handler(channel, presenter, viewer_control, debug, lifecycle)
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
    remote_text: ServiceInbox,
    presentation_failure: mpsc::UnboundedSender<String>,
    debug: DebugInfo,
    shutting_down: Arc<AtomicBool>,
) {
    {
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        let shutting_down = Arc::clone(&shutting_down);
        channel.on_close(Box::new(move || {
            let presentation_failure = presentation_failure.clone();
            let debug = debug.clone();
            let shutting_down = Arc::clone(&shutting_down);
            Box::pin(async move {
                debug.set_data_channel(CONTROL_CHANNEL_LABEL, "closed");
                if shutting_down.load(Ordering::Acquire) {
                    tracing::info!("viewer control data channel closed during viewer shutdown");
                    return;
                }
                tracing::error!("viewer control data channel closed while video was active");
                let _ = presentation_failure.send(
                    "remote input/control channel closed while video was still active".into(),
                );
            })
        }));
    }
    let cursor_shape = Arc::new(Mutex::new(CursorShape::Default));
    let capabilities_sent = Arc::new(AtomicBool::new(false));
    let supported_profiles = Arc::new(OnceLock::<Arc<Vec<VideoProfile>>>::new());
    let configurations_seen = Arc::new(AtomicU64::new(0));
    let resume_state = viewer_control.resume_state.clone();
    let quality_preset = Arc::clone(&resume_state.quality);
    let chroma_mode = Arc::clone(&resume_state.chroma);
    let selected_display_id = Arc::clone(&resume_state.display_id);
    channel.on_message(Box::new(move |message| {
        let presenter = Arc::clone(&presenter);
        let cursor_shape = Arc::clone(&cursor_shape);
        let capabilities_sent = Arc::clone(&capabilities_sent);
        let supported_profiles = Arc::clone(&supported_profiles);
        let configurations_seen = Arc::clone(&configurations_seen);
        let quality_preset = Arc::clone(&quality_preset);
        let chroma_mode = Arc::clone(&chroma_mode);
        let viewer_control = viewer_control.clone();
        let remote_text = remote_text.clone();
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        let selected_display_id = Arc::clone(&selected_display_id);
        Box::pin(async move {
            match SessionMessage::decode(&message.data) {
                Ok(SessionMessage::DisplayConfiguration {
                    displays,
                    active_display_id,
                    stream_id,
                    format,
                }) => {
                    let configuration_sequence =
                        configurations_seen.fetch_add(1, Ordering::AcqRel) + 1;
                    let previous = presenter.lock().ok().and_then(|guard| {
                        guard
                            .as_ref()
                            .map(|active| (active.stream_id, active.profile))
                    });
                    tracing::info!(
                        configuration_sequence,
                        previous_stream_id = previous.map(|value| value.0.0),
                        previous_profile = ?previous.map(|value| value.1),
                        stream_id = stream_id.0,
                        display_id = active_display_id.0,
                        width = format.width,
                        height = format.height,
                        fps = format.frames_per_second,
                        bitrate_bits_per_second = format.bitrate_bits_per_second,
                        codec = ?format.codec,
                        "viewer received display configuration"
                    );
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
                    // Probing Windows hardware MFTs can take noticeable time.
                    // Codec support is a viewer capability, so do it once per
                    // connection rather than again for the negotiated echo.
                    let profiles = supported_profiles
                        .get_or_init(|| {
                            Arc::new(crate::platform::supported_video_profiles(format))
                        })
                        .clone();
                    let resumed_display = selected_display_id
                        .lock()
                        .ok()
                        .and_then(|selected| *selected)
                        .filter(|selected| {
                            *selected != active_display_id
                                && displays.iter().any(|display| display.id == *selected)
                        });
                    let sink = ControlSink::new(
                        viewer_control.files.clone(),
                        move |message| message_queue.send(message),
                        move |enabled| input_gate.set_input_enabled(enabled),
                        viewer_control.chat.clone(),
                        Arc::clone(&viewer_control.resume_state.technician_blocked),
                        Arc::clone(&viewer_control.maintenance),
                        Arc::clone(&quality_preset),
                        Arc::clone(&chroma_mode),
                        #[cfg(windows)]
                        Arc::clone(&profiles),
                    );
                    debug.configure_stream(
                        active_display.name.clone(),
                        format.width,
                        format.height,
                        format.frames_per_second,
                        format.codec,
                    );
                    #[cfg(target_os = "macos")]
                    let reset_in_place = if capabilities_sent.load(Ordering::Acquire)
                        && let Ok(mut guard) = presenter.lock()
                        && let Some(active) = guard.as_mut()
                        && can_reset_presenter_in_place(active.format, format)
                    {
                        match active.presenter.reset_stream(format, active_display.clone(), displays.clone()) {
                            Ok(()) => {
                                let previous_stream_id = active.stream_id;
                                active.stream_id = stream_id;
                                active.format = format;
                                active.profile = format.profile();
                                tracing::info!(
                                    configuration_sequence,
                                    previous_stream_id = previous_stream_id.0,
                                    stream_id = stream_id.0,
                                    bitrate_bits_per_second = format.bitrate_bits_per_second,
                                    codec = ?format.codec,
                                    "reset the macOS decoder in place for a replacement stream"
                                );
                                true
                            }
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    configuration_sequence,
                                    previous_stream_id = active.stream_id.0,
                                    stream_id = stream_id.0,
                                    "could not reset the macOS decoder in place; replacing the presenter"
                                );
                                false
                            }
                        }
                    } else {
                        false
                    };
                    #[cfg(not(target_os = "macos"))]
                    let reset_in_place = false;

                    if reset_in_place {
                        viewer_control.send(SessionMessage::RequestKeyframe { stream_id });
                        if let Some(display_id) = resumed_display {
                            viewer_control.send(SessionMessage::SelectDisplay { display_id });
                        }
                        tracing::info!(configuration_sequence, stream_id = stream_id.0, display_id = active_display_id.0, display_name = %active_display.name, width = format.width, height = format.height, fps = format.frames_per_second, bitrate_bits_per_second = format.bitrate_bits_per_second, codec = ?format.codec, "remote control stream reconfigured without replacing its window");
                        return;
                    }
                    if !capabilities_sent.swap(true, Ordering::AcqRel) {
                        // The first configuration describes the Agent's mandatory
                        // bootstrap profile. Negotiate the best common profile
                        // before creating a visible presenter; the Agent echoes a
                        // settled configuration even when that profile is retained.
                        viewer_control.send(SessionMessage::ViewerCapabilities {
                            profiles: profiles.as_ref().clone(),
                            quality: sink.quality_preset(),
                            chroma: sink.chroma_mode(),
                        });
                        tracing::info!(
                            configuration_sequence,
                            stream_id = stream_id.0,
                            "viewer capabilities sent; waiting for the settled video profile"
                        );
                        return;
                    }
                    match Presenter::start(
                        format,
                        active_display.clone(),
                        displays,
                        sink.clone(),
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
                                    #[cfg(target_os = "macos")]
                                    format,
                                    profile: format.profile(),
                                    presenter: new_presenter,
                                }));
                            if let Some(old) = old.as_mut() {
                                tracing::warn!(
                                    configuration_sequence,
                                    previous_stream_id = old.stream_id.0,
                                    previous_profile = ?old.profile,
                                    stream_id = stream_id.0,
                                    codec = ?format.codec,
                                    "replacing the active macOS presenter after display configuration"
                                );
                                old.presenter.stop();
                            }
                            let request = SessionMessage::RequestKeyframe { stream_id };
                            viewer_control.send(request);
                            if let Some(display_id) = resumed_display {
                                viewer_control.send(SessionMessage::SelectDisplay { display_id });
                                tracing::info!(
                                    display_id = display_id.0,
                                    "restored viewer display selection after reconnect"
                                );
                            }
                            tracing::info!(configuration_sequence, stream_id = stream_id.0, display_id = active_display_id.0, display_name = %active_display.name, width = format.width, height = format.height, fps = format.frames_per_second, bitrate_bits_per_second = format.bitrate_bits_per_second, codec = ?format.codec, "remote control stream configured");
                        }
                        Err(error) => {
                            let message = format!(
                                "hardware decoder/presenter initialization failed: {error:#}"
                            );
                            tracing::error!(error = %error, "hardware decoder/presenter initialization failed");
                            if format.profile()
                                != (VideoProfile {
                                    codec: Codec::H264,
                                    chroma: ChromaMode::Yuv420,
                                })
                            {
                                viewer_control.send(SessionMessage::VideoProfileRejected {
                                    profile: format.profile(),
                                    reason: message,
                                });
                            } else {
                                let _ = presentation_failure.send(message);
                            }
                        }
                    }
                }
                Ok(SessionMessage::MaintenanceError { reason }) => {
                    if let Ok(mut state) = viewer_control.maintenance.lock() { state.error = Some(reason); }
                }
                Ok(SessionMessage::MaintenanceState { agent_input_blocked, blacked_out }) => {
                    if let Ok(mut state) = viewer_control.maintenance.lock() {
                        *state = crate::platform::MaintenanceState { available: true, agent_input_blocked, blacked_out, error: None };
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
                Ok(message @ (SessionMessage::FileTransfer(_) | SessionMessage::Clipboard { .. } | SessionMessage::ClipboardChunk { .. } | SessionMessage::Chat { .. } | SessionMessage::ChatAvailable)) => {
                    remote_text.send(message);
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
    lifecycle: ReceiverLifecycle,
) {
    let closed = Arc::new(AtomicBool::new(false));
    {
        let debug = debug.clone();
        let closed = Arc::clone(&closed);
        channel.on_close(Box::new(move || {
            let debug = debug.clone();
            let closed = Arc::clone(&closed);
            let lifecycle = lifecycle.clone();
            Box::pin(async move {
                closed.store(true, Ordering::Release);
                debug.set_data_channel("meshrmm-video-v1", "closed");
                if !lifecycle.shutting_down.load(Ordering::Acquire) {
                    tracing::error!("viewer video data channel closed during an active session");
                    let _ = lifecycle
                        .presentation_failure
                        .send("remote video channel closed during an active session".into());
                }
            })
        }));
    }
    let receive_state = Arc::new(tokio::sync::Mutex::new(VideoReceiveState::new()));
    {
        let receive_state = Arc::clone(&receive_state);
        let viewer_control = viewer_control.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_micros(KEYFRAME_RETRY_INTERVAL_US));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            while !closed.load(Ordering::Acquire) {
                interval.tick().await;
                let now_us = monotonic_timestamp_us();
                let stream_id = {
                    let mut state = receive_state.lock().await;
                    state.poll_recovery(now_us)
                };
                if let Some(stream_id) = stream_id {
                    viewer_control.send(SessionMessage::RequestKeyframe { stream_id });
                    tracing::debug!(
                        stream_id = stream_id.0,
                        "retrying a video recovery keyframe while no decodable frames arrive"
                    );
                }
            }
        });
    }
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
            receive_state.observe_stream(packet_stream_id);
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
                    "requested a video recovery keyframe"
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
        let queue = ViewerControlQueue::new(tx, ViewerResumeState::default());
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

    #[cfg(target_os = "macos")]
    #[test]
    fn technician_block_survives_focus_and_transport_rebuild() {
        let state = ViewerResumeState::default();
        state.technician_blocked.store(true, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, state.clone());
        queue.set_input_enabled(true);
        queue.send(SessionMessage::Input(RemoteInput::Key {
            display_id: DisplayId(1), scan_code: 30, extended: false, pressed: true,
        }));
        assert!(rx.try_recv().is_err());
        state.technician_blocked.store(false, Ordering::SeqCst);
        queue.send(SessionMessage::Input(RemoteInput::Key {
            display_id: DisplayId(1), scan_code: 30, extended: false, pressed: true,
        }));
        assert!(rx.try_recv().is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn monitor_resolution_changes_reuse_the_presenter_but_codec_changes_do_not() {
        let current = meshrmm_protocol::VideoFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 60,
            codec: Codec::H264,
            pixel_format: meshrmm_protocol::PixelFormat::Nv12,
            bitrate_bits_per_second: 12_000_000,
        };
        let replacement = meshrmm_protocol::VideoFormat {
            width: 2560,
            height: 1440,
            frames_per_second: 30,
            bitrate_bits_per_second: 8_000_000,
            ..current
        };
        assert!(can_reset_presenter_in_place(current, replacement));
        assert!(!can_reset_presenter_in_place(
            current,
            meshrmm_protocol::VideoFormat {
                codec: Codec::H265,
                ..replacement
            }
        ));
        assert!(!can_reset_presenter_in_place(
            current,
            meshrmm_protocol::VideoFormat {
                pixel_format: meshrmm_protocol::PixelFormat::Ayuv,
                ..replacement
            }
        ));
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
    fn video_recovery_can_retry_when_only_packet_fragments_arrive() {
        let mut state = VideoReceiveState::new();
        let stream_id = VideoStreamId(7);
        state.observe_stream(stream_id);
        state.mark_loss();

        assert_eq!(state.stream_id, Some(stream_id));
        assert!(state.waiting_for_keyframe);
        assert!(state.keyframe_request_due(1_000));
        assert!(!state.keyframe_request_due(1_001));
        assert!(state.keyframe_request_due(1_000 + KEYFRAME_RETRY_INTERVAL_US));
    }

    #[test]
    fn video_recovery_expires_a_trailing_fragment_without_new_packets() {
        let mut state = VideoReceiveState::new();
        state.accept_completed(encoded_frame(1, true), 1_000);
        let mut frame = encoded_frame(2, false);
        frame.data = vec![1; 6];
        let packet = meshrmm_protocol::fragment_frame(&frame, 3)
            .unwrap()
            .remove(0);
        assert!(matches!(
            state.reassembler.push(packet, 2_000),
            ReassemblyOutcome::Accepted
        ));
        assert_eq!(state.poll_recovery(2_001), None);

        let expired_at = 2_000 + ReassemblyConfig::default().stale_after.as_micros() as u64;
        assert_eq!(state.poll_recovery(expired_at), Some(VideoStreamId(7)));
        assert_eq!(state.reassembler.stats().incomplete_frames_dropped, 1);
        assert_eq!(state.poll_recovery(expired_at + 1), None);
        assert_eq!(
            state.poll_recovery(expired_at + KEYFRAME_RETRY_INTERVAL_US),
            Some(VideoStreamId(7))
        );
        state.accept_completed(
            encoded_frame(3, true),
            expired_at + KEYFRAME_RETRY_INTERVAL_US + 1,
        );
        assert_eq!(
            state.poll_recovery(expired_at + 2 * KEYFRAME_RETRY_INTERVAL_US),
            None
        );
    }

    #[test]
    fn video_recovery_does_not_treat_a_static_desktop_as_packet_loss() {
        let mut state = VideoReceiveState::new();
        state.accept_completed(encoded_frame(1, true), 1_000);
        assert_eq!(state.poll_recovery(60_000_000), None);
        assert!(!state.waiting_for_keyframe);
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
        let queue = ViewerControlQueue::new(tx, ViewerResumeState::default());
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
    fn secure_attention_is_sent_while_toolbar_has_keyboard_focus() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, ViewerResumeState::default());
        // Native toolbar controls can take focus away from the remote desktop.
        queue.set_input_enabled(false);
        queue.send(SessionMessage::SendSecureAttention);
        assert_eq!(rx.try_recv().unwrap(), SessionMessage::SendSecureAttention);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn backgrounding_discards_pending_pointer_motion() {
        let (queue, mut rx) = active_queue();

        queue.send(pointer(30, 40));
        queue.set_input_enabled(false);
        queue.flush_pointer();

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn display_selection_is_retained_for_a_reconnected_transport() {
        let resume_state = ViewerResumeState::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, resume_state.clone());

        queue.send(SessionMessage::SelectDisplay {
            display_id: DisplayId(42),
        });

        assert_eq!(
            *resume_state.display_id.lock().unwrap(),
            Some(DisplayId(42))
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            SessionMessage::SelectDisplay {
                display_id: DisplayId(42)
            }
        );
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
