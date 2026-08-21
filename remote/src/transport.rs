use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use pulsermm_protocol::{
    FrameReassembler, IceServer, ReassemblyConfig, ReassemblyOutcome, SessionBootstrap,
    SessionMessage, SessionState, SignalMessage, VideoPacket, VideoStreamId,
};
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

use crate::config::Config;
use crate::platform::{ControlSink, Presenter, monotonic_timestamp_us};
use crate::signaling::{authenticated_websocket, session_signal_url};

struct ActivePresenter {
    stream_id: VideoStreamId,
    presenter: Presenter,
}

pub async fn run_receiver(config: &Config, bootstrap: SessionBootstrap) -> anyhow::Result<()> {
    let url = session_signal_url(&config.server, bootstrap.session_id.as_str())?;
    let socket = authenticated_websocket(url, &bootstrap.signaling_token).await?;
    let (mut signal_writer, mut signal_reader) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
    let peer = create_peer(&bootstrap.ice_servers, outgoing_tx.clone(), state_tx).await?;
    let presenter = Arc::new(Mutex::new(None::<ActivePresenter>));
    let control_channel = Arc::new(Mutex::new(None::<Arc<RTCDataChannel>>));
    let (viewer_control_tx, mut viewer_control_rx) = mpsc::unbounded_channel::<SessionMessage>();
    let (presentation_failure_tx, mut presentation_failure_rx) =
        mpsc::unbounded_channel::<String>();
    install_data_channel_handler(
        &peer,
        Arc::clone(&presenter),
        Arc::clone(&control_channel),
        viewer_control_tx,
        presentation_failure_tx,
    );

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    let startup_started = tokio::time::Instant::now();
    let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
    stats_interval.tick().await;
    let mut remote_description_set = false;
    let mut pending_candidates = Vec::new();
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
                if state == RTCPeerConnectionState::Connected
                    && session_state == SessionState::Connecting
                {
                    session_state = session_state.transition(SessionState::Streaming)?;
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
                log_network_stats(&peer).await;
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

async fn log_network_stats(peer: &RTCPeerConnection) {
    for report in peer.get_stats().await.reports.into_values() {
        if let StatsReportType::CandidatePair(pair) = report
            && pair.nominated
        {
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

fn install_data_channel_handler(
    peer: &Arc<RTCPeerConnection>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    control_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    viewer_control: mpsc::UnboundedSender<SessionMessage>,
    presentation_failure: mpsc::UnboundedSender<String>,
) {
    peer.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let presenter = Arc::clone(&presenter);
        let control_channel = Arc::clone(&control_channel);
        let viewer_control = viewer_control.clone();
        let presentation_failure = presentation_failure.clone();
        Box::pin(async move {
            match channel.label() {
                "pulsermm-control-v2" => {
                    if let Ok(mut active) = control_channel.lock() {
                        *active = Some(Arc::clone(&channel));
                    }
                    install_control_handler(
                        channel,
                        presenter,
                        viewer_control,
                        presentation_failure,
                    )
                }
                "pulsermm-video-v1" => install_video_handler(channel, presenter),
                label => tracing::warn!(label, "ignoring unknown WebRTC data channel"),
            }
        })
    }));
}

fn install_control_handler(
    channel: Arc<RTCDataChannel>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    viewer_control: mpsc::UnboundedSender<SessionMessage>,
    presentation_failure: mpsc::UnboundedSender<String>,
) {
    channel.on_message(Box::new(move |message| {
        let presenter = Arc::clone(&presenter);
        let viewer_control = viewer_control.clone();
        let presentation_failure = presentation_failure.clone();
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
                    let sink_tx = viewer_control.clone();
                    let sink: ControlSink = Arc::new(move |message| {
                        let _ = sink_tx.send(message);
                    });
                    match Presenter::start(format, active_display.clone(), displays, sink) {
                        Ok(new_presenter) => {
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
                            let _ = viewer_control.send(request);
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
                Ok(_) => {}
                Err(error) => tracing::warn!(error = %error, "discarding invalid control message"),
            }
        })
    }));
}

fn install_video_handler(
    channel: Arc<RTCDataChannel>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
) {
    let reassembler = Arc::new(tokio::sync::Mutex::new(FrameReassembler::new(
        ReassemblyConfig::default(),
    )));
    channel.on_message(Box::new(move |message| {
        let reassembler = Arc::clone(&reassembler);
        let presenter = Arc::clone(&presenter);
        Box::pin(async move {
            let received_at_us = monotonic_timestamp_us();
            let packet = match VideoPacket::decode(&message.data) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::warn!(error = %error, "discarding invalid video packet");
                    return;
                }
            };
            let mut reassembler = reassembler.lock().await;
            let outcome = reassembler.push(packet, received_at_us);
            let stats = reassembler.stats();
            drop(reassembler);
            if let ReassemblyOutcome::Completed(frame) = outcome {
                let encode_us = frame
                    .encode_complete_timestamp_us
                    .saturating_sub(frame.capture_timestamp_us);
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
