use std::ops::ControlFlow;

use meshrmm_protocol::{RemoteSessionId, SessionMessage, SessionState, SignalMessage};
use meshrmm_session_transport::identity::PeerIdentity;
use meshrmm_signaling_client::SessionSignaling;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::{
    RTCPeerConnection, peer_connection_state::RTCPeerConnectionState,
    sdp::session_description::RTCSessionDescription,
};

use super::control_channel::send_control_message;
use super::peer::log_network_stats;
use super::{ControlCommand, DISCONNECTED_GRACE_PERIOD};
use crate::remote::sender_failure::transport_failure;
use crate::remote::sender_progress::SenderProgress;

pub(super) struct SenderEvents {
    pub(super) outgoing: mpsc::UnboundedReceiver<SignalMessage>,
    pub(super) control: mpsc::UnboundedReceiver<ControlCommand>,
    pub(super) state: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
    pub(super) video_failure: mpsc::UnboundedReceiver<anyhow::Error>,
}

/// Offer/answer progress with the viewer over signaling.
#[derive(Default)]
pub(super) struct Negotiation {
    offer_sent: bool,
    remote_description_set: bool,
    pending_candidates: Vec<RTCIceCandidateInit>,
}

pub(super) struct SenderLoop<'a> {
    pub(super) signal: &'a mut SessionSignaling,
    pub(super) peer: &'a RTCPeerConnection,
    pub(super) identity: &'a PeerIdentity,
    pub(super) outgoing_tx: &'a mpsc::UnboundedSender<SignalMessage>,
    pub(super) control_channel: &'a RTCDataChannel,
    pub(super) capture_tx: &'a mpsc::Sender<ControlCommand>,
    pub(super) session_id: &'a RemoteSessionId,
    pub(super) progress: &'a SenderProgress,
    pub(super) session_state: SessionState,
    pub(super) negotiation: Negotiation,
    pub(super) disconnected_since: Option<tokio::time::Instant>,
}

impl SenderLoop<'_> {
    pub(super) async fn run(&mut self, events: &mut SenderEvents) -> anyhow::Result<()> {
        let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(2));
        stats_interval.tick().await;
        loop {
            tokio::select! {
            Some(outgoing) = events.outgoing.recv() => {
                let json = serde_json::to_string(&outgoing)?;
                self.signal.send(Message::Text(json.into())).await?;
            }
            incoming = self.signal.next() => {
                let Some(incoming) = incoming else { break Err(transport_failure("signaling connection closed")); };
                let Message::Text(text) = incoming? else { continue; };
                let signal: SignalMessage = serde_json::from_str(text.as_str())?;
                self.handle_signal(signal).await?;
            }
            Some(command) = events.control.recv() => {
                if self.handle_command(command).await?.is_break() {
                    break Ok(());
                }
            }
            Some(state) = events.state.recv() => self.handle_state(state)?,
            Some(error) = events.video_failure.recv() => break Err(error),
            _ = stats_interval.tick() => {
                if self.disconnected_since.is_some_and(|since| since.elapsed() >= DISCONNECTED_GRACE_PERIOD) {
                    break Err(transport_failure(format!(
                        "WebRTC remained disconnected for {} seconds",
                        DISCONNECTED_GRACE_PERIOD.as_secs()
                    )));
                }
                log_network_stats(self.peer).await;
            },
            }
        }
    }

    async fn handle_signal(&mut self, signal: SignalMessage) -> anyhow::Result<()> {
        let negotiation = &mut self.negotiation;
        match signal {
            SignalMessage::Ready if !negotiation.offer_sent => {
                let offer = self.peer.create_offer(None).await?;
                self.peer.set_local_description(offer).await?;
                let local = self
                    .peer
                    .local_description()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("WebRTC did not retain its local offer"))?;
                self.outgoing_tx
                    .send(SignalMessage::Offer { sdp: local.sdp })?;
                negotiation.offer_sent = true;
            }
            SignalMessage::Answer { sdp } => {
                self.identity.verify_sdp(&sdp)?;
                self.peer
                    .set_remote_description(RTCSessionDescription::answer(sdp)?)
                    .await?;
                negotiation.remote_description_set = true;
                for candidate in negotiation.pending_candidates.drain(..) {
                    self.peer.add_ice_candidate(candidate).await?;
                }
            }
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
                if negotiation.remote_description_set {
                    self.peer.add_ice_candidate(candidate).await?;
                } else {
                    if negotiation.pending_candidates.len() >= 256 {
                        anyhow::bail!("too many pending ICE candidates");
                    }
                    negotiation.pending_candidates.push(candidate);
                }
            }
            SignalMessage::PeerLeft => {
                return Err(transport_failure(
                    "viewer disconnected from the remote session",
                ));
            }
            SignalMessage::Error { message, .. } => return Err(anyhow::anyhow!(message)),
            _ => {}
        }
        Ok(())
    }

    /// Breaks when the session ends without an error.
    async fn handle_command(&mut self, command: ControlCommand) -> anyhow::Result<ControlFlow<()>> {
        match command {
            command @ (ControlCommand::Keyframe
            | ControlCommand::Bitrate(_)
            | ControlCommand::RestartBitrate(_)
            | ControlCommand::Quality(_)
            | ControlCommand::ViewerCapabilities { .. }
            | ControlCommand::HeadlessResolution(_)
            | ControlCommand::DisplayBorder(_)
            | ControlCommand::Chroma(_)
            | ControlCommand::CursorCapture(_)
            | ControlCommand::InputOwnership(_)
            | ControlCommand::Recording(_)
            | ControlCommand::VideoProfileRejected { .. }
            | ControlCommand::SelectDisplay(_)) => {
                self.capture_tx
                    .try_send(command)
                    .map_err(|_| anyhow::anyhow!("capture command queue full or closed"))?;
            }
            ControlCommand::MaintenanceError(reason) => {
                send_control_message(
                    self.control_channel,
                    SessionMessage::MaintenanceError { reason },
                )
                .await?;
            }
            ControlCommand::Stop => return Ok(ControlFlow::Break(())),
            ControlCommand::ChannelClosed => {
                // The viewer closes its old channels before resuming the
                // session. End this sender quietly so its expected
                // teardown cannot surface as a fatal error in the new
                // connection.
                tracing::info!("viewer control channel closed; awaiting session resume");
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn handle_state(&mut self, state: RTCPeerConnectionState) -> anyhow::Result<()> {
        tracing::info!(?state, session_id = %self.session_id, "WebRTC connection state changed");
        if state == RTCPeerConnectionState::Connected
            && self.session_state == SessionState::Connecting
        {
            self.session_state = self.session_state.transition(SessionState::Streaming)?;
            self.progress.mark_streaming(std::time::Instant::now());
        }
        if state == RTCPeerConnectionState::Connected {
            // The session no longer depends on signaling.
            self.signal.peer_connected();
            self.disconnected_since = None;
        } else if state == RTCPeerConnectionState::Disconnected {
            self.disconnected_since
                .get_or_insert_with(tokio::time::Instant::now);
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            return Err(transport_failure(format!(
                "WebRTC connection ended in state {state:?}"
            )));
        }
        Ok(())
    }
}
