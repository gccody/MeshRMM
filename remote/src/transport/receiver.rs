//! The signaling loop: exchanges the WebRTC offer, answer and candidates
//! with the server, watches the connection and ends the session. Once the
//! peers connect, the session outlives the signaling socket, which
//! reconnects in the background.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, ChromaMode, Codec, IceServer, SessionBootstrap, SessionMessage,
    SessionState, SignalErrorCode, SignalMessage, VideoProfile,
};
use meshrmm_session_transport::ServiceChannel;
use meshrmm_session_transport::identity::PeerIdentity;
use meshrmm_signaling_client::SessionSignaling;
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
use super::failure::{FailureKind, SessionFailure};
use super::services::{ServiceInbox, start_viewer_services};
use super::video::install_video_handler;
use super::{ActivePresenter, ReceiverLifecycle, ViewerResumeState};
use crate::config::Config;
use crate::debug::DebugInfo;
use crate::launch_status::{self, LaunchStatus};
use crate::signaling::session_signal_url;

const SESSION_ACTIVITY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const NEGOTIATION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const DISCONNECTED_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn run_receiver(
    config: &Config,
    bootstrap: SessionBootstrap,
    resume_state: ViewerResumeState,
) -> anyhow::Result<()> {
    apply_bootstrap_policies(&resume_state, &bootstrap);
    // Resumes keep the session and its client token.
    resume_state.toolbox.connect(
        &config.server,
        bootstrap.session_id.as_str(),
        &bootstrap.signaling_token,
    );
    let identity = PeerIdentity::load(&meshrmm_session_transport::identity::viewer_directory()?)?;
    let debug = DebugInfo::new(bootstrap.session_id.as_str());
    let url = session_signal_url(&config.server, bootstrap.session_id.as_str())?;
    // Connecting can take a while on a bad network; Cancel must not wait for it.
    let mut signaling = tokio::select! {
        signaling = SessionSignaling::connect(url, bootstrap.signaling_token.clone()) => signaling?,
        () = crate::shutdown::wait() => return Ok(()),
    };
    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (state_tx, state_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
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
    let (presentation_failure_tx, presentation_failure_rx) = mpsc::unbounded_channel::<String>();
    let lifecycle = ReceiverLifecycle {
        presentation_failure: presentation_failure_tx,
        shutting_down: Arc::new(AtomicBool::new(false)),
        progress: Arc::clone(&resume_state.progress),
        reconnect_status: Arc::clone(&resume_state.reconnect_status),
        restarting: Arc::clone(&resume_state.restarting),
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
    let mut session = SignalingLoop {
        bootstrap: &bootstrap,
        resume_state: &resume_state,
        identity: &identity,
        debug: &debug,
        peer: &peer,
        outgoing: &outgoing_tx,
        presenter: &presenter,
        viewer_control: &viewer_control,
        lifecycle: &lifecycle,
        session_state,
        presenter_missing_since: Some(tokio::time::Instant::now()),
        statistics_log: crate::debug::StatisticsLog::default(),
        remote_description_set: false,
        pending_candidates: Vec::new(),
        disconnected_since: None,
        peer_connected: false,
        offer_received: false,
        awaiting_approval_since: None,
    };
    let mut events = SignalingEvents::start(outgoing_rx, state_rx, presentation_failure_rx).await;
    // Pointer pacing must not depend on the receiver loop being available: a
    // control-channel send can await long enough for a short movement burst to
    // end. This activity-driven flusher always queues that burst's newest
    // position after the coalescing window.
    let pointer_flusher = tokio::spawn(flush_pointer_motion(viewer_control.clone()));
    let result = session.run(&mut signaling, &mut events).await;
    session
        .finish(result, &mut signaling, pointer_flusher)
        .await
}

/// Applies the session's policies, keeping the technician's choices from
/// earlier connections where there are any.
fn apply_bootstrap_policies(resume_state: &ViewerResumeState, bootstrap: &SessionBootstrap) {
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
    resume_state
        .idle_disconnect
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_policy(bootstrap.idle_disconnect);
    resume_state
        .clear_clipboard
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .policy = bootstrap.clear_clipboard_policy;
}

/// The channels and timers that wake the signaling loop.
struct SignalingEvents {
    outgoing: mpsc::UnboundedReceiver<SignalMessage>,
    peer_state: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
    presentation_failure: mpsc::UnboundedReceiver<String>,
    stats: tokio::time::Interval,
    activity: tokio::time::Interval,
    negotiation: tokio::time::Interval,
}

impl SignalingEvents {
    async fn start(
        outgoing: mpsc::UnboundedReceiver<SignalMessage>,
        peer_state: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
        presentation_failure: mpsc::UnboundedReceiver<String>,
    ) -> Self {
        let mut stats = tokio::time::interval(std::time::Duration::from_secs(2));
        stats.tick().await;
        let mut activity = tokio::time::interval(SESSION_ACTIVITY_INTERVAL);
        activity.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        activity.tick().await;
        let mut negotiation = tokio::time::interval(NEGOTIATION_RETRY_INTERVAL);
        negotiation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        negotiation.tick().await;
        Self {
            outgoing,
            peer_state,
            presentation_failure,
            stats,
            activity,
            negotiation,
        }
    }
}

/// One connection attempt's signaling and health checks, from the first
/// offer until the session ends.
struct SignalingLoop<'a> {
    bootstrap: &'a SessionBootstrap,
    resume_state: &'a ViewerResumeState,
    identity: &'a PeerIdentity,
    debug: &'a DebugInfo,
    peer: &'a RTCPeerConnection,
    outgoing: &'a mpsc::UnboundedSender<SignalMessage>,
    presenter: &'a Mutex<Option<ActivePresenter>>,
    viewer_control: &'a ViewerControlQueue,
    lifecycle: &'a ReceiverLifecycle,
    session_state: SessionState,
    presenter_missing_since: Option<tokio::time::Instant>,
    statistics_log: crate::debug::StatisticsLog,
    remote_description_set: bool,
    pending_candidates: Vec<RTCIceCandidateInit>,
    disconnected_since: Option<tokio::time::Instant>,
    // Whether WebRTC connected in this attempt, and whether the Agent
    // answered at all, tell a blocked network path from a silent Agent.
    peer_connected: bool,
    offer_received: bool,
    awaiting_approval_since: Option<tokio::time::Instant>,
}

impl SignalingLoop<'_> {
    async fn run(
        &mut self,
        signaling: &mut SessionSignaling,
        events: &mut SignalingEvents,
    ) -> anyhow::Result<()> {
        loop {
            tokio::select! {
            Some(signal) = events.outgoing.recv() => {
                signaling.send(Message::Text(serde_json::to_string(&signal)?.into())).await?;
            }
            _ = events.activity.tick() => {
                self.outgoing.send(SignalMessage::Activity)?;
            }
            _ = events.negotiation.tick(), if self.session_state == SessionState::Connecting => {
                self.outgoing.send(SignalMessage::Ready)?;
            }
            incoming = signaling.next() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(_)) => continue,
                    Some(Err(error)) if meshrmm_signaling_client::is_terminal_websocket_error(&error) => return Err(error),
                    Some(Err(error)) => return Err(SessionFailure::new(FailureKind::SignalingLost, format!("{error:#}")).into()),
                    None => return Err(SessionFailure::new(FailureKind::SignalingLost, "signaling connection closed").into()),
                };
                self.handle_signal(serde_json::from_str(text.as_str())?).await?;
            }
            Some(state) = events.peer_state.recv() => self.handle_peer_state(state, signaling)?,
            Some(error) = events.presentation_failure.recv() => {
                tracing::error!(%error, "viewer presentation path reported a terminal failure");
                return Err(SessionFailure::new(FailureKind::PresentationFailed, error).into());
            }
            _ = events.stats.tick() => {
                if self.check_health().await?.is_break() {
                    return Ok(());
                }
            },
            _ = tokio::signal::ctrl_c() => return Ok(()),
            () = crate::shutdown::wait() => return Ok(()),
            }
        }
    }

    async fn handle_signal(&mut self, signal: SignalMessage) -> anyhow::Result<()> {
        match signal {
            SignalMessage::Offer { sdp } => self.answer_offer(sdp).await?,
            SignalMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
                username_fragment,
            } => {
                let candidate = RTCIceCandidateInit {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                    username_fragment,
                };
                if self.remote_description_set {
                    self.peer.add_ice_candidate(candidate).await?;
                } else {
                    if self.pending_candidates.len() >= 256 {
                        anyhow::bail!("too many pending ICE candidates");
                    }
                    self.pending_candidates.push(candidate);
                }
            }
            SignalMessage::PeerLeft => {
                return Err(SessionFailure::new(
                    FailureKind::AgentLeft,
                    "Agent disconnected from the remote session",
                )
                .into());
            }
            SignalMessage::AwaitingApproval { remaining_seconds } if !self.offer_received => {
                launch_status::report(LaunchStatus::AwaitingApproval { remaining_seconds });
                // The video deadline starts once the user answers.
                let now = tokio::time::Instant::now();
                self.awaiting_approval_since.get_or_insert(now);
                self.presenter_missing_since = Some(now);
            }
            SignalMessage::Error { message, code } => {
                if code == Some(SignalErrorCode::IdentityMismatch)
                    || message.starts_with("Peer identity verification failed:")
                {
                    return Err(meshrmm_session_transport::identity::IdentityError(message).into());
                }
                return Err(SessionFailure::new(FailureKind::AgentReported(code), message).into());
            }
            _ => {}
        }
        Ok(())
    }

    async fn answer_offer(&mut self, sdp: String) -> anyhow::Result<()> {
        self.offer_received = true;
        if let Some(since) = self.awaiting_approval_since.take() {
            self.resume_state.add_approval_wait(since.elapsed());
        }
        launch_status::report(LaunchStatus::EstablishingConnection);
        self.debug
            .set_peer_fingerprint(self.identity.verify_sdp(&sdp)?);
        self.peer
            .set_remote_description(RTCSessionDescription::offer(sdp)?)
            .await?;
        self.remote_description_set = true;
        for candidate in self.pending_candidates.drain(..) {
            self.peer.add_ice_candidate(candidate).await?;
        }
        let answer = self.peer.create_answer(None).await?;
        self.peer.set_local_description(answer).await?;
        let local = self
            .peer
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("WebRTC did not retain its local answer"))?;
        self.outgoing
            .send(SignalMessage::Answer { sdp: local.sdp })?;
        Ok(())
    }

    fn handle_peer_state(
        &mut self,
        state: RTCPeerConnectionState,
        signaling: &mut SessionSignaling,
    ) -> anyhow::Result<()> {
        tracing::info!(?state, session_id = %self.bootstrap.session_id, "WebRTC connection state changed");
        self.debug
            .set_connection_state(format!("{state:?}").to_ascii_lowercase());
        if state == RTCPeerConnectionState::Connected
            && self.session_state == SessionState::Connecting
        {
            self.session_state = self.session_state.transition(SessionState::Streaming)?;
            self.outgoing.send(SignalMessage::Activity)?;
            launch_status::report(LaunchStatus::StartingDisplay);
        }
        if state == RTCPeerConnectionState::Connected {
            // The session no longer depends on signaling.
            signaling.peer_connected();
            self.peer_connected = true;
            self.disconnected_since = None;
        } else if state == RTCPeerConnectionState::Disconnected {
            self.disconnected_since
                .get_or_insert_with(tokio::time::Instant::now);
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            return Err(SessionFailure::new(
                peer_failure_kind(self.peer_connected),
                format!("WebRTC connection ended in state {state:?}"),
            )
            .into());
        }
        Ok(())
    }

    /// The periodic check of the connection and the video. Breaks when the
    /// presenter ended cleanly.
    async fn check_health(&mut self) -> anyhow::Result<ControlFlow<()>> {
        if self
            .disconnected_since
            .is_some_and(|since| since.elapsed() >= DISCONNECTED_GRACE_PERIOD)
        {
            return Err(SessionFailure::new(
                peer_failure_kind(self.peer_connected),
                format!(
                    "WebRTC remained disconnected for {} seconds",
                    DISCONNECTED_GRACE_PERIOD.as_secs()
                ),
            )
            .into());
        }
        self.check_video_arrived()?;
        update_network_stats(self.peer, self.debug, self.statistics_log.due()).await;
        self.poll_presenter_end()
    }

    fn check_video_arrived(&mut self) -> anyhow::Result<()> {
        let presenter_missing = self.presenter.lock().is_ok_and(|guard| {
            let presented = guard
                .as_ref()
                .and_then(|active| active.presenter.first_presented_at());
            self.lifecycle.observe_presentation(presented);
            presented.is_none()
        });
        if presenter_missing {
            let waiting_since = self
                .presenter_missing_since
                .get_or_insert_with(tokio::time::Instant::now);
            if waiting_since.elapsed() >= std::time::Duration::from_secs(30) {
                return Err(SessionFailure::new(
                    video_timeout_kind(self.offer_received, self.peer_connected),
                    "timed out waiting 30 seconds for the remote video stream; check the Agent's WebRTC and ICE logs",
                )
                .into());
            }
        } else {
            self.presenter_missing_since = None;
        }
        Ok(())
    }

    /// Asks the Agent for a fallback profile when a presenter fails with
    /// anything but H.264 4:2:0; otherwise its end ends the session.
    fn poll_presenter_end(&mut self) -> anyhow::Result<ControlFlow<()>> {
        let ended = self.presenter.lock().ok().and_then(|guard| {
            guard.as_ref().and_then(|active| {
                active
                    .presenter
                    .poll_ended()
                    .map(|ended| (active.profile, ended))
            })
        });
        let Some((profile, ended)) = ended else {
            return Ok(ControlFlow::Continue(()));
        };
        match (profile, ended) {
            (profile, Err(reason))
                if profile
                    != (VideoProfile {
                        codec: Codec::H264,
                        chroma: ChromaMode::Yuv420,
                    }) =>
            {
                tracing::warn!(%reason, ?profile, "video presentation failed; requesting profile fallback");
                self.viewer_control
                    .send(SessionMessage::VideoProfileRejected { profile, reason });
                if let Ok(mut guard) = self.presenter.lock()
                    && let Some(mut failed) = guard.take()
                {
                    failed.presenter.stop();
                }
                self.presenter_missing_since = Some(tokio::time::Instant::now());
                Ok(ControlFlow::Continue(()))
            }
            (_, ended) => ended.map(ControlFlow::Break).map_err(|reason| {
                SessionFailure::new(FailureKind::PresentationFailed, reason).into()
            }),
        }
    }

    /// Ends the attempt: tells the server when the viewer ended the session,
    /// keeps the window up when the session may resume, and closes the peer.
    async fn finish(
        mut self,
        result: anyhow::Result<()>,
        signaling: &mut SessionSignaling,
        pointer_flusher: tokio::task::JoinHandle<()>,
    ) -> anyhow::Result<()> {
        if let Some(since) = self.awaiting_approval_since.take() {
            self.resume_state.add_approval_wait(since.elapsed());
        }
        // A frame can finish between the last health poll and a transport failure.
        if let Ok(guard) = self.presenter.lock() {
            self.lifecycle.observe_presentation(
                guard
                    .as_ref()
                    .and_then(|active| active.presenter.first_presented_at()),
            );
        }
        self.lifecycle.shutting_down.store(true, Ordering::Release);
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
            if let Err(error) = signaling.send(Message::Text(end_message.into())).await {
                tracing::warn!(error = %error, "failed to notify the server that the viewer ended the session");
            }
        }

        let mut session_state = self.session_state;
        if matches!(
            session_state,
            SessionState::Requested
                | SessionState::Signaling
                | SessionState::Connecting
                | SessionState::Streaming
        ) {
            session_state = session_state.transition(SessionState::Closing)?;
        }
        if let Some(mut active) = self
            .presenter
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            match &result {
                Ok(()) => active.presenter.stop(),
                // The session may resume; keep its window up until then.
                Err(error) => self.resume_state.keep_while_reconnecting(active, error),
            }
        }
        self.viewer_control.chat.set_available(false);
        let mut result = result;
        if let Err(error) = self.peer.close().await {
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
}

/// A WebRTC failure is a lost connection only if it ever connected; otherwise
/// no network path opened.
fn peer_failure_kind(peer_connected: bool) -> FailureKind {
    if peer_connected {
        FailureKind::PeerConnectionLost
    } else {
        FailureKind::PeerNeverConnected
    }
}

/// No video arrived in time. If the Agent answered but WebRTC never
/// connected, the network path is at fault rather than the Agent's capture.
fn video_timeout_kind(offer_received: bool, peer_connected: bool) -> FailureKind {
    if offer_received && !peer_connected {
        FailureKind::PeerNeverConnected
    } else {
        FailureKind::VideoTimeout
    }
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
                meshrmm_audio::OPUS_CHANNEL => {
                    channel.on_message(Box::new(move |message| {
                        audio.receive_opus(&message.data);
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
                label if let Some(route) = service_routes.get(label).cloned() => {
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
