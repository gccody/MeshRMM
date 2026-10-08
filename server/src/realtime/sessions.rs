//! Remote sessions: a viewer and an Agent exchanging WebRTC signaling
//! through the server.
//!
//! Each live session is an actor: a task that owns the session's record, its
//! two signaling sockets and its idle deadline, and handles one message at a
//! time. The record is kept in `remote_sessions`, sealed with the instance
//! key, so a session survives a restart: [`Sessions::restore`] starts an
//! actor for each one still live, the viewer resumes, and the Agent's
//! control connection replays the session when it reconnects.
//!
//! A device has at most one session. Its row's `expires_at` is the idle
//! deadline: the viewer's activity moves it forward, and when it passes the
//! session ends.
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use meshrmm_protocol_types::{
    AgentCommand, AgentSessionRequest, ConnectionApproval, IdleDisconnectPolicy, RemoteSessionId,
    SessionBootstrap, SignalMessage, TogglePolicy,
};
use sea_query::{Expr, ExprTrait, OnConflict, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};

use axum::extract::ws::{Message as WsMessage, WebSocket};

use super::{LIVENESS, ToAgent, close_frame, send};
use crate::{
    agents,
    audit::{self, Actor, Target},
    db::{self, Executor, tables::RemoteSessions},
    http::{ApiError, AppState},
    rbac::{self, Permission, Permissions},
    secrets::new_token,
    settings,
    time::now_ms,
    users::{self, User, new_id},
};

/// The largest signaling message a peer may send. SDP offers with many
/// candidates are a few kilobytes.
pub const MAX_SIGNAL_BYTES: usize = 64 * 1024;
/// Messages waiting for a slow peer before it counts as gone.
const PEER_BACKLOG: usize = 256;
const INBOX: usize = 64;

/// How long a session lasts without the viewer's activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub idle: Duration,
    /// The first deadline, before the viewer has connected and reported
    /// activity: long enough for the Agent's user to answer an approval
    /// prompt.
    pub start: Duration,
}

impl Timeouts {
    pub fn new(idle: Duration) -> Self {
        Self {
            idle,
            start: idle.max(Duration::from_secs(15 * 60)),
        }
    }
}

/// Which end of a session a signaling socket is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The viewer.
    Client,
    Agent,
}

impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "client" => Some(Self::Client),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Client => Self::Agent,
            Self::Agent => Self::Client,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Client => 0,
            Self::Agent => 1,
        }
    }
}

/// What the session sends a signaling socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOut {
    Text(String),
    Close(u16, &'static str),
}

/// Why a signaling socket was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unauthorized,
    Gone,
}

/// The user a session's viewer acts for.
#[derive(Debug, Clone)]
pub struct Identity {
    pub user_id: String,
    pub device_id: String,
}

/// A session as stored. The Agent's request is kept exactly as the Agent
/// last received it: an Agent that reconnects gets it again, and restarts
/// the session if it differs from the one it is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    session_id: String,
    device_id: String,
    user_id: String,
    client_token: String,
    idle_timeout_ms: i64,
    idle_disconnect: IdleDisconnectPolicy,
    display_border: bool,
    agent_request: AgentSessionRequest,
}

impl Record {
    fn bootstrap(&self) -> SessionBootstrap {
        let request = &self.agent_request;
        SessionBootstrap {
            start_in_background: request.start_in_background,
            idle_policy: request.idle_policy,
            idle_disconnect: self.idle_disconnect,
            clear_clipboard_policy: request.clear_clipboard_policy,
            display_border: self.display_border,
            session_id: request.session_id.clone(),
            signaling_token: self.client_token.clone(),
            expires_at_unix_ms: request.expires_at_unix_ms,
            ice_servers: request.ice_servers.clone(),
        }
    }

    fn context(session_id: &str) -> String {
        format!("remote-session:{session_id}")
    }

    fn seal(&self, state: &AppState) -> Vec<u8> {
        let json = serde_json::to_vec(self).expect("session records serialize");
        state
            .instance_key
            .encrypt(&Self::context(&self.session_id), &json)
    }

    fn open(state: &AppState, session_id: &str, sealed: &[u8]) -> anyhow::Result<Self> {
        let json = state
            .instance_key
            .decrypt(&Self::context(session_id), sealed)?;
        let record: Self = serde_json::from_slice(&json)?;
        anyhow::ensure!(
            record.session_id == session_id,
            "the record belongs to another session"
        );
        Ok(record)
    }
}

fn token_matches(supplied: &str, expected: &str) -> bool {
    agents::hashes_match(
        &crate::secrets::token_hash(supplied),
        &crate::secrets::token_hash(expected),
    )
}

fn unauthorized() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNAUTHORIZED,
        "remote session authentication failed",
    )
    .with_code("session_unauthenticated")
}

pub fn ended() -> ApiError {
    ApiError::new(axum::http::StatusCode::GONE, "the remote session has ended")
        .with_code("session_ended")
}

fn device_offline() -> ApiError {
    ApiError::conflict("the device is offline").with_code("device_offline")
}

enum Message {
    Check {
        role: Role,
        token: String,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    Join {
        role: Role,
        token: String,
        peer: mpsc::Sender<PeerOut>,
        reply: oneshot::Sender<Option<u64>>,
    },
    Signal {
        role: Role,
        peer: u64,
        text: String,
    },
    Left {
        role: Role,
        peer: u64,
    },
    Resume {
        token: String,
        reply: oneshot::Sender<Result<SessionBootstrap, ApiError>>,
    },
    End {
        token: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Identity {
        token: String,
        reply: oneshot::Sender<Result<Identity, ApiError>>,
    },
    Current {
        reply: oneshot::Sender<Option<AgentSessionRequest>>,
    },
    Expire {
        reason: &'static str,
        reply: oneshot::Sender<()>,
    },
}

/// A live session's actor.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    sender: mpsc::Sender<Message>,
}

impl SessionHandle {
    /// Asks the actor; `None` once the session has ended.
    async fn ask<T>(&self, message: impl FnOnce(oneshot::Sender<T>) -> Message) -> Option<T> {
        let (reply, answer) = oneshot::channel();
        self.sender.send(message(reply)).await.ok()?;
        answer.await.ok()
    }

    /// Whether `token` may open the `role` socket.
    pub async fn check(&self, role: Role, token: &str) -> Result<(), Refusal> {
        let token = token.to_owned();
        self.ask(|reply| Message::Check { role, token, reply })
            .await
            .unwrap_or(Err(Refusal::Gone))
    }

    /// Makes `peer` the session's `role` socket, replacing any earlier one.
    /// Returns its ID, or `None` if the session ended or the token is wrong.
    pub async fn join(&self, role: Role, token: &str, peer: mpsc::Sender<PeerOut>) -> Option<u64> {
        let token = token.to_owned();
        self.ask(|reply| Message::Join {
            role,
            token,
            peer,
            reply,
        })
        .await
        .flatten()
    }

    /// Hands the session a message from its `role` socket. Returns whether
    /// the session is still live.
    pub async fn signal(&self, role: Role, peer: u64, text: String) -> bool {
        self.sender
            .send(Message::Signal { role, peer, text })
            .await
            .is_ok()
    }

    pub async fn left(&self, role: Role, peer: u64) {
        let _ = self.sender.send(Message::Left { role, peer }).await;
    }

    pub async fn resume(&self, token: &str) -> Result<SessionBootstrap, ApiError> {
        let token = token.to_owned();
        self.ask(|reply| Message::Resume { token, reply })
            .await
            .unwrap_or_else(|| Err(ended()))
    }

    /// Ends the session for its viewer. Ending one that already ended
    /// succeeds.
    pub async fn end(&self, token: &str) -> Result<(), ApiError> {
        let token = token.to_owned();
        self.ask(|reply| Message::End { token, reply })
            .await
            .unwrap_or(Ok(()))
    }

    pub async fn identity(&self, token: &str) -> Result<Identity, ApiError> {
        let token = token.to_owned();
        self.ask(|reply| Message::Identity { token, reply })
            .await
            .unwrap_or_else(|| Err(ended()))
    }

    /// Ends the session. Returns once it has ended.
    pub async fn expire(&self, reason: &'static str) {
        self.ask(|reply| Message::Expire { reason, reply }).await;
    }

    async fn current(&self) -> Option<AgentSessionRequest> {
        self.ask(|reply| Message::Current { reply }).await.flatten()
    }
}

/// The live sessions' actors.
#[derive(Debug, Clone)]
pub struct Sessions {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    timeouts: Timeouts,
    actors: Mutex<HashMap<String, (u64, mpsc::Sender<Message>)>>,
    next_actor: AtomicU64,
}

/// A device's live session.
#[derive(Debug)]
pub struct DeviceSession {
    pub session_id: String,
    /// The technician who started it.
    pub user_id: String,
    pub handle: SessionHandle,
}

/// A session to start for a technician.
#[derive(Debug)]
pub struct NewSession<'a> {
    pub device_id: &'a str,
    pub user: &'a User,
    pub start_in_background: bool,
    pub reason: &'a str,
    pub actor: Actor,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    record_encrypted: Vec<u8>,
    expires_at: i64,
}

fn session_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            RemoteSessions::Id,
            RemoteSessions::RecordEncrypted,
            RemoteSessions::ExpiresAt,
        ])
        .from(RemoteSessions::Table)
        .to_owned()
}

impl Sessions {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            inner: Arc::new(Inner {
                timeouts,
                actors: Mutex::default(),
                next_actor: AtomicU64::new(0),
            }),
        }
    }

    pub fn timeouts(&self) -> Timeouts {
        self.inner.timeouts
    }

    /// Starts an actor for every session that was live when the server
    /// stopped. Returns how many.
    pub async fn restore(&self, state: &AppState) -> anyhow::Result<usize> {
        let rows: Vec<SessionRow> = state
            .database
            .fetch_all(
                &session_select()
                    .and_where(Expr::col(RemoteSessions::ExpiresAt).gt(now_ms()))
                    .to_owned(),
            )
            .await?;
        let mut actors = self.inner.actors.lock().await;
        let mut restored = 0;
        for row in rows {
            if actors.contains_key(&row.id) {
                continue;
            }
            match Record::open(state, &row.id, &row.record_encrypted) {
                Ok(record) => {
                    let entry = self.spawn(state, record, row.expires_at);
                    actors.insert(row.id, entry);
                    restored += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = row.id,
                        error = format!("{error:#}"),
                        "could not read a remote session; ending it"
                    );
                    delete(state, &row.id).await;
                }
            }
        }
        Ok(restored)
    }

    /// The session's actor, started from its stored record if it has none.
    /// `None` if there is no such session.
    pub async fn get(
        &self,
        state: &AppState,
        session_id: &str,
    ) -> Result<Option<SessionHandle>, ApiError> {
        if let Some(handle) = self.running(session_id).await {
            return Ok(Some(handle));
        }
        // Read without holding the map, so lookups of unknown IDs don't hold
        // up the others.
        let row: Option<SessionRow> = state
            .database
            .fetch_optional(
                &session_select()
                    .and_where(Expr::col(RemoteSessions::Id).eq(session_id))
                    .and_where(Expr::col(RemoteSessions::ExpiresAt).gt(now_ms()))
                    .to_owned(),
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let record = match Record::open(state, &row.id, &row.record_encrypted) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not read a remote session; ending it"
                );
                delete(state, &row.id).await;
                return Ok(None);
            }
        };
        let mut actors = self.inner.actors.lock().await;
        if let Some((_, sender)) = actors.get(session_id)
            && !sender.is_closed()
        {
            return Ok(Some(SessionHandle {
                sender: sender.clone(),
            }));
        }
        let entry = self.spawn(state, record, row.expires_at);
        let handle = SessionHandle {
            sender: entry.1.clone(),
        };
        actors.insert(row.id, entry);
        Ok(Some(handle))
    }

    async fn running(&self, session_id: &str) -> Option<SessionHandle> {
        let actors = self.inner.actors.lock().await;
        let (_, sender) = actors.get(session_id)?;
        (!sender.is_closed()).then(|| SessionHandle {
            sender: sender.clone(),
        })
    }

    /// The device's session, if it has one.
    pub async fn for_device(
        &self,
        state: &AppState,
        device_id: &str,
    ) -> Result<Option<DeviceSession>, ApiError> {
        let row: Option<(String, String)> = state
            .database
            .fetch_optional(
                &Query::select()
                    .columns([RemoteSessions::Id, RemoteSessions::UserId])
                    .from(RemoteSessions::Table)
                    .and_where(Expr::col(RemoteSessions::DeviceId).eq(device_id))
                    .and_where(Expr::col(RemoteSessions::ExpiresAt).gt(now_ms()))
                    .to_owned(),
            )
            .await?;
        let Some((session_id, user_id)) = row else {
            return Ok(None);
        };
        Ok(self
            .get(state, &session_id)
            .await?
            .map(|handle| DeviceSession {
                session_id,
                user_id,
                handle,
            }))
    }

    /// The session request to give the device's Agent when it connects.
    pub async fn replay(&self, state: &AppState, device_id: &str) -> Option<AgentSessionRequest> {
        match self.for_device(state, device_id).await {
            Ok(Some(session)) => session.handle.current().await,
            Ok(None) => None,
            Err(_) => {
                tracing::warn!(device_id, "could not look up the session to replay");
                None
            }
        }
    }

    /// Ends the device's session, if it has one. Returns whether it did.
    pub async fn end_for_device(
        &self,
        state: &AppState,
        device_id: &str,
        reason: &'static str,
    ) -> Result<bool, ApiError> {
        match self.for_device(state, device_id).await? {
            Some(session) => {
                session.handle.expire(reason).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Starts a session on the device for the user and asks its Agent to
    /// join. The caller has checked the user may.
    pub async fn create(
        &self,
        state: &AppState,
        new: NewSession<'_>,
    ) -> Result<SessionBootstrap, ApiError> {
        if !state.agents.is_connected(new.device_id) {
            return Err(device_offline());
        }
        let policy = settings::load(&mut &state.database).await?;
        let session_id = new_id();
        let now = now_ms();
        let timeouts = self.inner.timeouts;
        let expires_at = now + millis(timeouts.start);
        let reason = meshrmm_protocol_types::connection_reason(new.reason);
        let record = Record {
            session_id: session_id.clone(),
            device_id: new.device_id.to_owned(),
            user_id: new.user.id.clone(),
            client_token: new_token(),
            idle_timeout_ms: millis(timeouts.idle),
            idle_disconnect: IdleDisconnectPolicy {
                minutes: policy
                    .idle_disconnect_minutes
                    .and_then(|minutes| u32::try_from(minutes).ok()),
                allow_override: policy.allow_idle_disconnect_override,
            },
            display_border: policy.display_border,
            agent_request: AgentSessionRequest {
                start_in_background: new.start_in_background,
                idle_policy: TogglePolicy {
                    enabled: policy.prevent_idle_lock,
                    allow_override: policy.allow_idle_override,
                },
                clear_clipboard_policy: TogglePolicy {
                    enabled: policy.clear_clipboard_on_close,
                    allow_override: policy.allow_clear_clipboard_override,
                },
                blackout_message: policy.blackout_message,
                session_banner: policy.session_banner,
                connection_notification: policy.connection_notification,
                background_connection_notification: policy.background_connection_notification,
                connection_notification_message: policy.connection_notification_message,
                connection_approval: policy.connection_approval.then(|| ConnectionApproval {
                    message: policy.connection_approval_message,
                    timeout_seconds: u32::try_from(policy.connection_approval_timeout_seconds)
                        .unwrap_or(30),
                    lock_idle_seconds: u32::try_from(policy.connection_approval_lock_idle_seconds)
                        .unwrap_or(0),
                }),
                connection_reason: reason.to_owned(),
                viewer_name: meshrmm_protocol_types::session_viewer_name(&new.user.display_name),
                session_id: RemoteSessionId::new(session_id.as_str()),
                signaling_token: new_token(),
                expires_at_unix_ms: unix_ms(expires_at),
                ice_servers: state.turn.ice_servers(&session_id),
            },
        };
        let mut transaction = state.database.begin().await?;
        // A session that expired but whose row remains no longer holds the
        // device.
        transaction
            .execute(
                &Query::delete()
                    .from_table(RemoteSessions::Table)
                    .and_where(Expr::col(RemoteSessions::DeviceId).eq(new.device_id))
                    .and_where(Expr::col(RemoteSessions::ExpiresAt).lte(now))
                    .to_owned(),
            )
            .await?;
        let inserted = transaction
            .execute(
                &Query::insert()
                    .into_table(RemoteSessions::Table)
                    .columns([
                        RemoteSessions::Id,
                        RemoteSessions::DeviceId,
                        RemoteSessions::UserId,
                        RemoteSessions::RecordEncrypted,
                        RemoteSessions::CreatedAt,
                        RemoteSessions::ExpiresAt,
                    ])
                    .values_panic([
                        session_id.as_str().into(),
                        new.device_id.into(),
                        new.user.id.as_str().into(),
                        record.seal(state).into(),
                        now.into(),
                        expires_at.into(),
                    ])
                    .on_conflict(
                        OnConflict::column(RemoteSessions::DeviceId)
                            .do_nothing()
                            .to_owned(),
                    )
                    .to_owned(),
            )
            .await?;
        if inserted == 0 {
            return Err(
                ApiError::conflict("the device already has an active remote session")
                    .with_code("session_in_progress"),
            );
        }
        audit::record(
            &mut transaction,
            &new.actor,
            "remote.session_create",
            Target::device(new.device_id),
            json!({
                "session_id": session_id,
                "start_in_background": new.start_in_background,
                "reason": reason,
            }),
        )
        .await?;
        transaction.commit().await?;
        let bootstrap = record.bootstrap();
        let request = record.agent_request.clone();
        let handle = {
            let mut actors = self.inner.actors.lock().await;
            let entry = self.spawn(state, record, expires_at);
            let handle = SessionHandle {
                sender: entry.1.clone(),
            };
            actors.insert(session_id.clone(), entry);
            handle
        };
        if !state.agents.send(new.device_id, ToAgent::Session(request)) {
            handle.expire("the device went offline").await;
            return Err(device_offline());
        }
        tracing::info!(
            session_id,
            device_id = new.device_id,
            "remote session started"
        );
        Ok(bootstrap)
    }

    fn spawn(
        &self,
        state: &AppState,
        record: Record,
        expires_at: i64,
    ) -> (u64, mpsc::Sender<Message>) {
        let id = self.inner.next_actor.fetch_add(1, Ordering::Relaxed);
        let (sender, inbox) = mpsc::channel(INBOX);
        state.turn.session_started(&record.session_id);
        let actor = SessionActor {
            state: state.clone(),
            sessions: self.clone(),
            id,
            record,
            expires_at,
            peers: [None, None],
            next_peer: 0,
            pending_terminal: None,
        };
        tokio::spawn(actor.run(inbox));
        (id, sender)
    }

    async fn forget(&self, session_id: &str, actor: u64) {
        let mut actors = self.inner.actors.lock().await;
        if actors.get(session_id).is_some_and(|(id, _)| *id == actor) {
            actors.remove(session_id);
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn unix_ms(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

async fn delete(state: &AppState, session_id: &str) {
    let deleted = state
        .database
        .execute(
            &Query::delete()
                .from_table(RemoteSessions::Table)
                .and_where(Expr::col(RemoteSessions::Id).eq(session_id))
                .to_owned(),
        )
        .await;
    let Err(error) = deleted else {
        return;
    };
    tracing::warn!(session_id, %error, "could not remove an ended remote session");
    // A row left with a future deadline would let the session be resumed.
    // An expired one is ignored, and maintenance removes it.
    let expired = state
        .database
        .execute(
            &Query::update()
                .table(RemoteSessions::Table)
                .value(RemoteSessions::ExpiresAt, 0)
                .and_where(Expr::col(RemoteSessions::Id).eq(session_id))
                .to_owned(),
        )
        .await;
    if let Err(error) = expired {
        tracing::error!(session_id, %error, "could not mark an ended remote session expired");
    }
}

#[derive(Debug)]
struct Peer {
    id: u64,
    sender: mpsc::Sender<PeerOut>,
}

struct SessionActor {
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
    async fn run(mut self, mut inbox: mpsc::Receiver<Message>) {
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

/// Serves a session's `role` signaling socket until it closes, the session
/// ends, or a newer socket for the same role replaces it.
pub async fn serve_peer(mut socket: WebSocket, handle: SessionHandle, role: Role, token: String) {
    let (sender, mut outgoing) = mpsc::channel(PEER_BACKLOG);
    let Some(peer) = handle.join(role, &token, sender).await else {
        let _ = socket
            .send(close_frame(4001, "the remote session has ended"))
            .await;
        return;
    };
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            out = outgoing.recv() => match out {
                Some(PeerOut::Text(text)) => {
                    if !send(&mut socket, WsMessage::Text(text.into())).await {
                        break;
                    }
                }
                Some(PeerOut::Close(code, reason)) => {
                    send(&mut socket, close_frame(code, reason)).await;
                    break;
                }
                None => break,
            },
            frame = socket.recv() => {
                last_heard = Instant::now();
                match frame {
                    Some(Ok(WsMessage::Text(text))) if text.len() <= MAX_SIGNAL_BYTES => {
                        if !handle.signal(role, peer, text.to_string()).await {
                            send(&mut socket, close_frame(4001, "the remote session has ended")).await;
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Text(_) | WsMessage::Binary(_))) => {
                        send(&mut socket, close_frame(1009, "invalid signaling message")).await;
                        break;
                    }
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Close(_)) | Err(_)) | None => break,
                }
            }
            () = tokio::time::sleep_until((last_heard + LIVENESS).into()) => break,
        }
    }
    handle.left(role, peer).await;
}

/// The user's permissions through their roles.
pub async fn user_permissions(
    executor: &mut impl Executor,
    user_id: &str,
) -> db::Result<Permissions> {
    Ok(rbac::permissions_of(
        &rbac::user_roles(executor, user_id).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_deadline_allows_time_for_approval() {
        let short = Timeouts::new(Duration::from_secs(60));
        assert_eq!(short.start, Duration::from_secs(15 * 60));
        let long = Timeouts::new(Duration::from_secs(3600));
        assert_eq!(long.start, Duration::from_secs(3600));
    }

    #[test]
    fn roles_parse_and_pair() {
        assert_eq!(Role::parse("client"), Some(Role::Client));
        assert_eq!(Role::parse("agent"), Some(Role::Agent));
        assert_eq!(Role::parse("viewer"), None);
        assert_eq!(Role::Client.other(), Role::Agent);
        assert_eq!(Role::Agent.other(), Role::Client);
    }
}
