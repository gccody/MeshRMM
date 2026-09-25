//! The signaling loop: exchanges the WebRTC offer, answer and candidates
//! with the server, watches the connection and ends the session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, ChromaMode, Codec, IceServer, SessionBootstrap, SessionMessage,
    SessionState, SignalMessage, VideoProfile,
};
use meshrmm_session_transport::{SERVICE_CHANNELS, ServiceChannel};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer};
use webrtc::peer_connection::{
    RTCPeerConnection, configuration::RTCConfiguration,
    peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription,
};
use webrtc::stats::StatsReportType;

use super::control::{ViewerControlQueue, flush_pointer_motion, install_control_handler};
use super::services::{ServiceInbox, start_viewer_services};
use super::video::install_video_handler;
use super::{ActivePresenter, ReceiverLifecycle, ViewerResumeState};
use crate::config::Config;
use crate::debug::DebugInfo;
use crate::launch_status::{self, LaunchStatus};
use crate::signaling::{authenticated_websocket, session_signal_url};

const SESSION_ACTIVITY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const NEGOTIATION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const DISCONNECTED_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);
const SIGNAL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
const SIGNAL_LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn run_receiver(
    config: &Config,
    bootstrap: SessionBootstrap,
    resume_state: ViewerResumeState,
) -> anyhow::Result<()> {
    resume_state
        .display_border
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert(bootstrap.display_border);
    resume_state
        .idle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .policy = bootstrap.idle_policy;
    let identity = meshrmm_session_transport::identity::PeerIdentity::load(
        &meshrmm_session_transport::identity::viewer_directory()?,
    )?;
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
        identity.certificate.clone(),
    )
    .await?;
    let presenter = Arc::new(Mutex::new(None::<ActivePresenter>));
    let control_channel = tokio::sync::watch::channel(None::<ServiceChannel>).0;
    let (viewer_control_tx, viewer_control_rx) = mpsc::unbounded_channel::<SessionMessage>();
    let viewer_control = ViewerControlQueue::new(viewer_control_tx, resume_state.clone());
    let (presentation_failure_tx, mut presentation_failure_rx) =
        mpsc::unbounded_channel::<String>();
    let lifecycle = ReceiverLifecycle {
        presentation_failure: presentation_failure_tx,
        shutting_down: Arc::new(AtomicBool::new(false)),
    };
    let (remote_text_tx, _services) = start_viewer_services(
        viewer_control.clone(),
        control_channel.clone(),
        viewer_control_rx,
        lifecycle.clone(),
    )?;
    install_data_channel_handler(
        &peer,
        Arc::clone(&presenter),
        control_channel.clone(),
        viewer_control.clone(),
        remote_text_tx,
        debug.clone(),
        lifecycle.clone(),
    );

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    outgoing_tx.send(SignalMessage::Activity)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    launch_status::report(LaunchStatus::WaitingForRemoteComputer);
    let mut presenter_missing_since = Some(tokio::time::Instant::now());
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut statistics_log = crate::debug::StatisticsLog::default();
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
                                launch_status::report(LaunchStatus::EstablishingConnection);
                                debug.set_peer_fingerprint(identity.verify_sdp(&sdp)?);
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
                            SignalMessage::Error { message } => {
                                if message.starts_with("Peer identity verification failed:") {
                                    break Err(meshrmm_session_transport::identity::IdentityError(message).into());
                                }
                                break Err(anyhow::anyhow!(message));
                            }
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
                    launch_status::report(LaunchStatus::StartingDisplay);
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
                update_network_stats(&peer, &debug, statistics_log.due()).await;
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
            () = crate::shutdown::wait() => break Ok(()),
            }
        }
    }
    .await;
    lifecycle.shutting_down.store(true, Ordering::Release);
    pointer_flusher.abort();
    let _ = pointer_flusher.await;

    if result.is_ok()
        || result.as_ref().err().is_some_and(|error| {
            error
                .downcast_ref::<meshrmm_session_transport::identity::IdentityError>()
                .is_some()
        })
    {
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
    if let Some(mut active) = presenter.lock().ok().and_then(|mut guard| guard.take()) {
        if result.is_ok() {
            active.presenter.stop();
        } else {
            // The session may resume; keep its window up until then.
            resume_state.keep_while_reconnecting(active);
        }
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

/// Refreshes the diagnostics overlay, and logs the statistics when `log` is set.
async fn update_network_stats(peer: &RTCPeerConnection, debug: &DebugInfo, log: bool) {
    let reports = peer.get_stats().await.reports;
    let mut candidates = HashMap::new();
    for report in reports.values() {
        if let StatsReportType::DataChannel(channel) = report
            && log
        {
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
            if !log {
                continue;
            }
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
    certificate: webrtc::peer_connection::certificate::RTCCertificate,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let peer = Arc::new(
        APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration {
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
    control_channel: tokio::sync::watch::Sender<Option<ServiceChannel>>,
    viewer_control: ViewerControlQueue,
    remote_text: ServiceInbox,
    debug: DebugInfo,
    lifecycle: ReceiverLifecycle,
) {
    let audio = meshrmm_audio::Player::new(viewer_control.resume_state.audio.clone());
    peer.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let audio = audio.clone();
        let presenter = Arc::clone(&presenter);
        let control_channel = control_channel.clone();
        let viewer_control = viewer_control.clone();
        let remote_text = remote_text.clone();
        let service_routes = remote_text.routes.clone();
        let debug = debug.clone();
        let lifecycle = lifecycle.clone();
        Box::pin(async move {
            debug.set_data_channel(channel.label(), "open");
            match channel.label() {
                meshrmm_audio::CHANNEL => {
                    channel.on_message(Box::new(move |message| {
                        audio.receive(&message.data);
                        Box::pin(async {})
                    }));
                }
                CONTROL_CHANNEL_LABEL => {
                    let channel = ServiceChannel::new(channel).await;
                    control_channel.send_replace(Some(channel.clone()));
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
                    let channel = ServiceChannel::new(channel).await;
                    route.attach(channel.clone());
                    let label = channel.label().to_owned();
                    channel.on_message(Box::new(move |message| {
                        let route = route.clone();
                        let remote_text = remote_text.clone();
                        let label = label.clone();
                        Box::pin(async move {
                            match SessionMessage::decode(&message.data) {
                                Ok(SessionMessage::ServiceChannelReady) => route.peer_ready(),
                                Ok(message)
                                    if meshrmm_session_transport::service_label(&message)
                                        == Some(label.as_str()) =>
                                {
                                    remote_text.send(message)
                                }
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
