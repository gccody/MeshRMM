use std::time::{Duration, Instant};

use meshrmm_protocol_types::{AgentCommand, RemoteSessionId, SessionBootstrap, SignalMessage};
use sea_query::{Expr, ExprTrait, Query};
use tokio::sync::mpsc;

use super::{
    Identity, Message, PeerOut, Record, Refusal, Role, Sessions, delete, device_offline, ended,
    token_matches, unauthorized, unix_ms, user_permissions,
};
use crate::{
    agents,
    db::tables::RemoteSessions,
    http::{ApiError, AppState},
    rbac::Permission,
    realtime::ToAgent,
    secrets::new_token,
    time::now_ms,
    users,
};

#[derive(Debug)]
struct Peer {
    id: u64,
    sender: mpsc::Sender<PeerOut>,
}

pub(super) struct SessionActor {
    state: AppState,
    sessions: Sessions,
    id: u64,
    record: Record,
    /// The idle deadline, in Unix milliseconds.
    expires_at: i64,
    peers: [Option<Peer>; 2],
    next_peer: u64,
    /// An error a peer reported before the other end connected, kept for it:
    /// the Agent can fail, or its user decline, before the viewer is there.
    pending_terminal: Option<(Role, String)>,
}

/// Whether the actor keeps running.
enum Next {
    Continue,
    Stop,
}

impl SessionActor {
    pub(super) fn new(
        state: AppState,
        sessions: Sessions,
        id: u64,
        record: Record,
        expires_at: i64,
    ) -> Self {
        Self {
            state,
            sessions,
            id,
            record,
            expires_at,
            peers: [None, None],
            next_peer: 0,
            pending_terminal: None,
        }
    }

    pub(super) async fn run(mut self, mut inbox: mpsc::Receiver<Message>) {
        let mut access = self.state.presence.access_changes();
        loop {
            let remaining = Duration::from_millis(unix_ms(self.expires_at - now_ms()));
            let next = tokio::select! {
                message = inbox.recv() => match message {
                    Some(message) => self.handle(message).await,
                    None => Next::Stop,
                },
                // A user or role changed: the technician may have lost access.
                Ok(()) = access.changed() => match self.still_allowed().await {
                    Ok(Some(reason)) => {
                        self.expire(reason).await;
                        Next::Stop
                    }
                    Ok(None) | Err(_) => Next::Continue,
                },
                () = tokio::time::sleep_until((Instant::now() + remaining).into()) => {
                    self.expire("the session was idle for too long").await;
                    Next::Stop
                }
            };
            if matches!(next, Next::Stop) {
                break;
            }
        }
        self.sessions.forget(&self.record.session_id, self.id).await;
    }

    fn live(&self) -> bool {
        now_ms() < self.expires_at
    }

    fn token_for(&self, role: Role) -> &str {
        match role {
            Role::Client => &self.record.client_token,
            Role::Agent => &self.record.agent_request.signaling_token,
        }
    }

    fn check(&self, role: Role, token: &str) -> Result<(), Refusal> {
        if !self.live() {
            Err(Refusal::Gone)
        } else if !token_matches(token, self.token_for(role)) {
            Err(Refusal::Unauthorized)
        } else {
            Ok(())
        }
    }

    async fn handle(&mut self, message: Message) -> Next {
        match message {
            Message::Check { role, token, reply } => {
                let _ = reply.send(self.check(role, &token));
            }
            Message::Join {
                role,
                token,
                peer,
                reply,
            } => {
                let joined = self
                    .check(role, &token)
                    .is_ok()
                    .then(|| self.join(role, peer));
                let _ = reply.send(joined);
            }
            Message::Signal { role, peer, text } => return self.signal(role, peer, text).await,
            Message::Left { role, peer } => {
                if self.peers[role.index()]
                    .as_ref()
                    .is_some_and(|current| current.id == peer)
                {
                    self.peers[role.index()] = None;
                }
            }
            Message::Resume { token, reply } => {
                let (result, next) = self.resume(&token).await;
                let _ = reply.send(result);
                return next;
            }
            Message::End { token, reply } => {
                if !token_matches(&token, &self.record.client_token) {
                    let _ = reply.send(Err(unauthorized()));
                    return Next::Continue;
                }
                self.expire("the session was ended by its viewer").await;
                let _ = reply.send(Ok(()));
                return Next::Stop;
            }
            Message::Identity { token, reply } => {
                let identity = match self.check(Role::Client, &token) {
                    Ok(()) => Ok(Identity {
                        user_id: self.record.user_id.clone(),
                        device_id: self.record.device_id.clone(),
                    }),
                    Err(Refusal::Unauthorized) => Err(unauthorized()),
                    Err(Refusal::Gone) => Err(ended()),
                };
                let _ = reply.send(identity);
            }
            Message::Current { reply } => {
                let _ = reply.send(self.live().then(|| self.record.agent_request.clone()));
            }
            Message::Expire { reason, reply } => {
                self.expire(reason).await;
                let _ = reply.send(());
                return Next::Stop;
            }
        }
        Next::Continue
    }

    fn join(&mut self, role: Role, sender: mpsc::Sender<PeerOut>) -> u64 {
        self.next_peer += 1;
        let id = self.next_peer;
        if let Some(old) = self.peers[role.index()].replace(Peer { id, sender }) {
            let _ = old
                .sender
                .try_send(PeerOut::Close(4000, "superseded peer connection"));
        }
        if self
            .pending_terminal
            .as_ref()
            .is_some_and(|(destination, _)| *destination == role)
            && let Some((_, text)) = self.pending_terminal.take()
            && !self.send(role, text.clone())
        {
            self.pending_terminal = Some((role, text));
        }
        tracing::debug!(
            session_id = self.record.session_id,
            ?role,
            "signaling peer connected"
        );
        id
    }

    /// Sends `text` to the `role` socket. Returns whether it was queued.
    fn send(&mut self, role: Role, text: String) -> bool {
        let Some(peer) = &self.peers[role.index()] else {
            return false;
        };
        if peer.sender.try_send(PeerOut::Text(text)).is_ok() {
            return true;
        }
        // Closed, or so far behind it is stuck.
        let _ = peer
            .sender
            .try_send(PeerOut::Close(1011, "signaling peer fell behind"));
        self.peers[role.index()] = None;
        false
    }

    fn close_peer(&mut self, role: Role, code: u16, reason: &'static str) {
        if let Some(peer) = self.peers[role.index()].take() {
            let _ = peer.sender.try_send(PeerOut::Close(code, reason));
        }
    }

    async fn signal(&mut self, role: Role, peer: u64, text: String) -> Next {
        // A superseded socket's last messages are dropped.
        if !self.peers[role.index()]
            .as_ref()
            .is_some_and(|current| current.id == peer)
        {
            return Next::Continue;
        }
        let signal = match serde_json::from_str::<SignalMessage>(&text) {
            Ok(signal) => signal,
            Err(_) => {
                self.close_peer(role, 1007, "invalid signaling JSON");
                return Next::Continue;
            }
        };
        match signal {
            SignalMessage::Activity | SignalMessage::EndSession if role != Role::Client => {
                self.close_peer(role, 1008, "only the viewer may send this message");
            }
            SignalMessage::Activity => return self.touch().await,
            SignalMessage::EndSession => {
                self.expire("the session was ended by its viewer").await;
                return Next::Stop;
            }
            SignalMessage::Error { .. } => {
                let destination = role.other();
                if !self.send(destination, text.clone()) {
                    self.pending_terminal = Some((destination, text));
                }
            }
            _ => {
                self.send(role.other(), text);
            }
        }
        Next::Continue
    }

    /// Moves the idle deadline forward for the viewer's activity. The stored
    /// deadline is the one that counts, so the session keeps its old one if
    /// storing fails, and ends if its row is gone.
    async fn touch(&mut self) -> Next {
        if !self.live() {
            return Next::Continue;
        }
        let expires_at = now_ms() + self.record.idle_timeout_ms;
        let updated = self
            .state
            .database
            .execute(
                &Query::update()
                    .table(RemoteSessions::Table)
                    .value(RemoteSessions::ExpiresAt, expires_at)
                    .and_where(Expr::col(RemoteSessions::Id).eq(self.record.session_id.as_str()))
                    .to_owned(),
            )
            .await;
        match updated {
            Ok(0) => {
                self.expire("the session was replaced").await;
                Next::Stop
            }
            Ok(_) => {
                self.expires_at = expires_at;
                Next::Continue
            }
            Err(error) => {
                tracing::warn!(session_id = self.record.session_id, %error, "could not store a remote session's deadline");
                Next::Continue
            }
        }
    }

    /// Checks the viewer may still use the session, gives both ends new ICE
    /// servers and a new deadline, and asks the Agent to reconnect.
    async fn resume(&mut self, token: &str) -> (Result<SessionBootstrap, ApiError>, Next) {
        if !token_matches(token, &self.record.client_token) {
            return (Err(unauthorized()), Next::Continue);
        }
        if !self.live() {
            self.expire("the session was idle for too long").await;
            return (Err(ended()), Next::Stop);
        }
        match self.still_allowed().await {
            Ok(None) => {}
            Ok(Some(reason)) => {
                self.expire(reason).await;
                return (Err(ended()), Next::Stop);
            }
            Err(error) => return (Err(error), Next::Continue),
        }
        let expires_at = now_ms() + self.record.idle_timeout_ms;
        let mut record = self.record.clone();
        record.agent_request.expires_at_unix_ms = unix_ms(expires_at);
        record.agent_request.ice_servers = self.state.turn.ice_servers(&record.session_id);
        // The Agent ignores a request that only moves the deadline. A new
        // token makes it restart its side of the session, which a viewer
        // that resumes needs.
        record.agent_request.signaling_token = new_token();
        let stored = self
            .state
            .database
            .execute(
                &Query::update()
                    .table(RemoteSessions::Table)
                    .value(RemoteSessions::RecordEncrypted, record.seal(&self.state))
                    .value(RemoteSessions::ExpiresAt, expires_at)
                    .and_where(Expr::col(RemoteSessions::Id).eq(record.session_id.as_str()))
                    .to_owned(),
            )
            .await;
        match stored {
            Ok(0) => {
                // Its row is gone, so it no longer holds the device.
                self.expire("the session was replaced").await;
                return (Err(ended()), Next::Stop);
            }
            Ok(_) => {}
            Err(error) => return (Err(error.into()), Next::Continue),
        }
        self.record = record;
        self.expires_at = expires_at;
        self.close_peer(Role::Agent, 4000, "superseded peer connection");
        let request = ToAgent::Session(self.record.agent_request.clone());
        if !self.state.agents.send(&self.record.device_id, request) {
            // The Agent gets the new request when it reconnects.
            return (Err(device_offline()), Next::Continue);
        }
        tracing::info!(
            session_id = self.record.session_id,
            "remote session resumed"
        );
        (Ok(self.record.bootstrap()), Next::Continue)
    }

    /// Why the session may no longer continue, if it can't: its device was
    /// removed, or its technician disabled or no longer allowed to connect.
    async fn still_allowed(&self) -> Result<Option<&'static str>, ApiError> {
        let mut database = &self.state.database;
        if !agents::is_active(&mut database, &self.record.device_id).await? {
            return Ok(Some("the device was removed"));
        }
        let Some(user) = users::by_id(&mut database, &self.record.user_id)
            .await?
            .filter(|user| !user.disabled)
        else {
            return Ok(Some("the technician's account was disabled"));
        };
        let permissions = user_permissions(&mut database, &user.id).await?;
        let mut required = vec![Permission::SessionsConnect];
        if self.record.agent_request.start_in_background {
            required.push(Permission::SessionsConnectBackground);
        }
        if !required
            .iter()
            .all(|permission| permissions.contains(permission))
        {
            return Ok(Some("the technician may no longer connect"));
        }
        Ok(None)
    }

    /// Ends the session: its row, its TURN relays, both sockets, and the
    /// Agent's side.
    async fn expire(&mut self, reason: &'static str) {
        self.expires_at = 0;
        delete(&self.state, &self.record.session_id).await;
        self.state.turn.session_ended(&self.record.session_id).await;
        for role in [Role::Client, Role::Agent] {
            self.close_peer(role, 4001, reason);
        }
        self.state.agents.send(
            &self.record.device_id,
            AgentCommand::EndSession {
                session_id: RemoteSessionId::new(self.record.session_id.as_str()),
            },
        );
        tracing::info!(
            session_id = self.record.session_id,
            reason,
            "remote session ended"
        );
    }
}
