use futures_util::lock::Mutex;
use meshrmm_protocol_types::{
    AgentCommand, AgentSessionRequest, AgentStatusMessage, RemoteSessionId,
};
use serde::{Deserialize, Serialize};
use worker::{query, *};

use crate::company_presence::{self, PresenceMutation};

const AGENT_TAG: &str = "agent";
const COMPANY_HEADER: &str = "X-Mesh-Company-Id";
const DEVICE_HEADER: &str = "X-Mesh-Device-Id";
const UNINSTALL_HEADER: &str = "X-Mesh-Uninstall-Requested";
const IDENTITY_KEY: &str = "agent_identity";
const PRESENCE_DELIVERY_KEY: &str = "presence_delivery";
const ACTIVE_SESSION_KEY: &str = "active_session";
const ACTIVE_SESSION_LEASE_KEY: &str = "active_session_lease";
/// The rotated credential last sent to the Agent, in plaintext, kept only
/// while D1 still holds its hash as the pending credential.
const PENDING_ROTATION_KEY: &str = "pending_rotation";
const OFFLINE_FOR_ROTATION: &str =
    "Agent must be online to receive its new credential; current credential remains valid";

#[derive(Debug, Deserialize)]
struct CredentialState {
    pending_auth_token_hash: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AgentIdentity {
    company_id: String,
    device_id: String,
    connection_id: String,
    #[serde(default)]
    uninstall_requested: bool,
}

// Persisted outbox: retries survive eviction/deployment and carry a monotonic
// generation so a delayed delivery cannot undo a replacement connection.
#[derive(Debug, Default, Deserialize, Serialize)]
struct PresenceDelivery {
    generation: u64,
    connection_id: String,
    connected: bool,
    acknowledged: bool,
}

// Lease renewals are stored apart from the active session request, which must
// stay exactly as the Agent received it: the Agent restarts a live session when
// a replay after reconnecting differs from the request it is running.
#[derive(Debug, Deserialize, Serialize)]
struct SessionLease {
    session_id: RemoteSessionId,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
struct SessionEnded {
    session_id: String,
}

#[durable_object]
pub struct AgentCoordinator {
    state: State,
    environment: Env,
    /// Serializes connections, presence publication and credential rotation.
    presence_lock: Mutex<()>,
}

impl DurableObject for AgentCoordinator {
    fn new(state: State, environment: Env) -> Self {
        Self {
            state,
            environment,
            presence_lock: Mutex::new(()),
        }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        match (request.method(), request.path().as_str()) {
            (Method::Get, "/connect") => {
                let _guard = self.presence_lock.lock().await;
                let uninstall_requested = required_header(&request, UNINSTALL_HEADER)? == "true";
                let identity = AgentIdentity {
                    company_id: required_header(&request, COMPANY_HEADER)?,
                    device_id: required_header(&request, DEVICE_HEADER)?,
                    connection_id: crate::random_token(),
                    uninstall_requested,
                };
                crate::validate_identifier(&identity.company_id, "company ID")?;
                crate::validate_identifier(&identity.device_id, "device ID")?;
                for socket in self.state.get_websockets_with_tag(AGENT_TAG) {
                    let _ = socket.close(Some(4000), Some("superseded Agent connection"));
                }
                // Arm recovery before changing the durable identity or accepting a socket.
                self.state.storage().set_alarm(30_000_i64).await?;
                self.state.storage().put(IDENTITY_KEY, &identity).await?;
                let pair = WebSocketPair::new()?;
                pair.server
                    .serialize_attachment(identity.connection_id.clone())?;
                self.state
                    .accept_websocket_with_tags(&pair.server, &[AGENT_TAG]);
                if uninstall_requested {
                    pair.server
                        .send_with_str(serde_json::to_string(&AgentCommand::Uninstall)?)?;
                } else {
                    // A failed publication stays in the outbox, and the alarm armed
                    // above retries it, so the Agent is accepted either way.
                    if let Err(error) = self.publish_presence(&identity, true).await {
                        console_error!(
                            "event=agent_presence_publish_failed connected=true error={}",
                            error
                        );
                    }
                    if let Some((session, expires_at_unix_ms)) = self.active_session().await?
                        && expires_at_unix_ms > Date::now().as_millis()
                    {
                        pair.server.send_with_str(session_payload(&session)?)?;
                        console_log!(
                            "event=agent_session_resumed session_id={}",
                            session.session_id
                        );
                    }
                }
                if !uninstall_requested {
                    match self.pending_rotation(&identity).await {
                        Ok(Some(command)) => pair
                            .server
                            .send_with_str(serde_json::to_string(&command)?)?,
                        Ok(None) => {}
                        // Rotating again resends the credential, so accept the Agent.
                        Err(error) => {
                            console_error!("event=agent_rotation_check_failed error={}", error)
                        }
                    }
                }
                console_log!("event=agent_signaling_connected");
                Response::from_websocket(pair.client)
            }
            (Method::Post, "/rotate-token") => {
                let _guard = self.presence_lock.lock().await;
                let company_id = required_header(&request, COMPANY_HEADER)?;
                let device_id = required_header(&request, DEVICE_HEADER)?;
                self.rotate_token(&company_id, &device_id).await
            }
            (Method::Post, "/uninstall") => {
                if let Some(agent) = self
                    .state
                    .get_websockets_with_tag(AGENT_TAG)
                    .into_iter()
                    .next()
                {
                    agent.send_with_str(serde_json::to_string(&AgentCommand::Uninstall)?)?;
                }
                Response::ok("uninstall queued")
            }
            (Method::Post, "/request") => {
                let session: AgentSessionRequest = request.json().await?;
                if self.revoke_if_company_inactive().await? {
                    return Response::error("company is not active", 403);
                }
                if self
                    .active_session()
                    .await?
                    .is_some_and(|(_, expires_at_unix_ms)| {
                        expires_at_unix_ms > Date::now().as_millis()
                    })
                {
                    return Response::error("Agent already has an active remote session", 409);
                }
                let agents = self.state.get_websockets_with_tag(AGENT_TAG);
                if agents.is_empty() {
                    return Response::error("Agent is offline", 409);
                }
                self.store_session(&session).await?;
                let payload = session_payload(&session)?;
                let mut delivered = false;
                for agent in agents {
                    match agent.send_with_str(&payload) {
                        Ok(()) => {
                            delivered = true;
                            break;
                        }
                        Err(error) => {
                            console_error!("event=agent_session_notify_failed error={}", error);
                            let _ = agent.close(Some(1011), Some("stale Agent connection"));
                        }
                    }
                }
                if !delivered {
                    self.forget_session().await?;
                    return Response::error("Agent is offline", 409);
                }
                console_log!(
                    "event=agent_session_notified session_id={}",
                    session.session_id
                );
                Response::ok("notified")
            }
            (Method::Post, "/resume-request") => {
                let session: AgentSessionRequest = request.json().await?;
                if self.revoke_if_company_inactive().await? {
                    return Response::error("company is not active", 403);
                }
                if !self.owns_session(&session).await? {
                    return Response::error("remote session no longer owns this Agent", 410);
                }
                self.store_session(&session).await?;
                let payload = session_payload(&session)?;
                let mut delivered = false;
                for agent in self.state.get_websockets_with_tag(AGENT_TAG) {
                    match agent.send_with_str(&payload) {
                        Ok(()) => {
                            delivered = true;
                            break;
                        }
                        Err(error) => {
                            console_error!(
                                "event=agent_session_resume_notify_failed error={}",
                                error
                            );
                            let _ = agent.close(Some(1011), Some("stale Agent connection"));
                        }
                    }
                }
                if !delivered {
                    return Response::error("Agent is offline", 409);
                }
                console_log!(
                    "event=agent_session_resume_refreshed session_id={}",
                    session.session_id
                );
                Response::ok("refreshed")
            }
            (Method::Post, "/lease") => {
                let session: AgentSessionRequest = request.json().await?;
                if self.revoke_if_company_inactive().await? {
                    return Response::error("company is not active", 403);
                }
                if !self.owns_session(&session).await? {
                    return Response::error("remote session no longer owns this Agent", 410);
                }
                self.state
                    .storage()
                    .put(
                        ACTIVE_SESSION_LEASE_KEY,
                        &SessionLease {
                            session_id: session.session_id,
                            expires_at_unix_ms: session.expires_at_unix_ms,
                        },
                    )
                    .await?;
                Response::ok("renewed")
            }
            (Method::Post, "/revoke") => {
                self.revoke().await?;
                Response::ok("revoked")
            }
            (Method::Post, "/close-session") => {
                let active = self
                    .state
                    .storage()
                    .get::<AgentSessionRequest>(ACTIVE_SESSION_KEY)
                    .await?;
                if let Some(active) = active {
                    // Expire credentials and disconnect peers before releasing the lease.
                    // The session callback and fallback both match the captured ID so a
                    // concurrent replacement session can never be cleared accidentally.
                    let request = Request::new("https://session.internal/expire", Method::Post)?;
                    let response = crate::object_stub(
                        &self.environment,
                        "REMOTE_SESSION",
                        active.session_id.as_str(),
                    )?
                    .fetch_with_request(request)
                    .await?;
                    crate::ensure_success(response, "close remote session").await?;
                    self.clear_session(active.session_id.as_str()).await?;
                    Response::from_json(&serde_json::json!({ "closed": true }))
                } else {
                    Response::from_json(&serde_json::json!({ "closed": false }))
                }
            }
            (Method::Post, "/session-ended") => {
                let ended: SessionEnded = request.json().await?;
                self.clear_session(&ended.session_id).await?;
                Response::ok("cleared")
            }
            (Method::Get, "/status") => {
                let connected = !self.state.get_websockets_with_tag(AGENT_TAG).is_empty();
                Response::from_json(&serde_json::json!({ "connected": connected }))
            }
            _ => Response::error("not found", 404),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        let _guard = self.presence_lock.lock().await;
        if let Some(identity) = self
            .state
            .storage()
            .get::<AgentIdentity>(IDENTITY_KEY)
            .await?
        {
            let mut connected = !identity.uninstall_requested
                && self
                    .state
                    .get_websockets_with_tag(AGENT_TAG)
                    .iter()
                    .any(|socket| {
                        socket
                            .deserialize_attachment::<String>()
                            .ok()
                            .flatten()
                            .is_some_and(|id| id == identity.connection_id)
                    });
            if connected && self.revoke_if_company_inactive().await? {
                connected = false;
            }
            self.publish_presence(&identity, connected).await?;
        }
        Response::ok("presence delivered")
    }

    async fn websocket_message(
        &self,
        socket: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        match message {
            WebSocketIncomingMessage::String(value) if value == "ping" => {
                socket.send_with_str("pong")?;
            }
            WebSocketIncomingMessage::String(value) => {
                match serde_json::from_str::<AgentStatusMessage>(&value) {
                    Ok(AgentStatusMessage::UninstallScheduled) => {
                        self.acknowledge_uninstall(&socket).await?;
                    }
                    Err(_) => {
                        socket.close(Some(1003), Some("unsupported Agent registry message"))?;
                    }
                }
            }
            WebSocketIncomingMessage::Binary(_) => {
                socket.close(Some(1003), Some("unsupported Agent registry message"))?;
            }
        }
        Ok(())
    }

    async fn websocket_close(
        &self,
        socket: WebSocket,
        code: usize,
        _reason: String,
        was_clean: bool,
    ) -> Result<()> {
        // Complete the close handshake so a retry cannot mistake this socket
        // for an established connection.
        let _ = socket.close(Some(1000), Some("Agent disconnected"));
        self.publish_disconnected_if_current(&socket).await;
        console_log!(
            "event=agent_signaling_closed code={} clean={}",
            code,
            was_clean
        );
        Ok(())
    }

    async fn websocket_error(&self, socket: WebSocket, error: Error) -> Result<()> {
        let _ = socket.close(Some(1011), Some("Agent signaling failed"));
        self.publish_disconnected_if_current(&socket).await;
        console_error!("event=agent_signaling_error error={}", error);
        Ok(())
    }
}

impl AgentCoordinator {
    /// Ends the active session and disconnects the Agent.
    async fn revoke(&self) -> Result<()> {
        let session = self
            .state
            .storage()
            .get::<AgentSessionRequest>(ACTIVE_SESSION_KEY)
            .await?;
        self.forget_session().await?;
        for socket in self.state.get_websockets_with_tag(AGENT_TAG) {
            if let Some(active) = &session {
                let _ = socket.send_with_str(serde_json::to_string(&AgentCommand::EndSession {
                    session_id: active.session_id.clone(),
                })?);
            }
            let _ = socket.close(Some(4001), Some("Agent authorization revoked"));
        }
        if let Some(active) = session {
            let request = Request::new("https://session.internal/expire", Method::Post)?;
            crate::object_stub(
                &self.environment,
                "REMOTE_SESSION",
                active.session_id.as_str(),
            )?
            .fetch_with_request(request)
            .await?;
        }
        Ok(())
    }

    /// Sends the connected Agent a new credential, or again the one already
    /// staged, so a rotation the Agent never completed can be retried without
    /// locking it out. D1 accepts the current credential until the Agent
    /// authenticates with the new one, which promotes it.
    async fn rotate_token(&self, company_id: &str, device_id: &str) -> Result<Response> {
        let db = self.environment.d1("DB")?;
        let Some(credential) = credential_state(&db, company_id, device_id).await? else {
            return Response::error("Agent not found", 404);
        };
        let agents = self.state.get_websockets_with_tag(AGENT_TAG);
        if agents.is_empty() {
            return Response::error(OFFLINE_FOR_ROTATION, 409);
        }
        let storage = self.state.storage();
        let staged = storage
            .get::<AgentCommand>(PENDING_ROTATION_KEY)
            .await?
            .filter(|command| {
                rotation_matches(command, credential.pending_auth_token_hash.as_deref())
            });
        let (command, created_hash) = match staged {
            Some(command) => (command, None),
            None => {
                // No staged credential matches the pending hash, so the Agent
                // was never sent it and replacing it cannot lock the Agent out.
                let token = crate::random_token();
                let hash = crate::sha256_hex(&token);
                let changes = query!(
                    &db,
                    "UPDATE agents SET pending_auth_token_hash = ?1, updated_at = ?2 WHERE id = ?3 AND company_id = ?4 AND deletion_requested_at IS NULL AND pending_auth_token_hash IS ?5",
                    hash,
                    crate::now_ms_i64()?,
                    device_id,
                    company_id,
                    credential.pending_auth_token_hash
                )?
                .run()
                .await?
                .meta()?
                .and_then(|meta| meta.changes)
                .unwrap_or_default();
                if changes == 0 {
                    return Response::error(
                        "the Agent's credential changed during the rotation; try again",
                        409,
                    );
                }
                let command = AgentCommand::RotateToken { token };
                storage.put(PENDING_ROTATION_KEY, &command).await?;
                (command, Some(hash))
            }
        };
        let payload = serde_json::to_string(&command)?;
        let redelivered = created_hash.is_none();
        if agents
            .iter()
            .any(|agent| agent.send_with_str(&payload).is_ok())
        {
            console_log!("event=agent_rotation_sent redelivered={}", redelivered);
            return Response::from_json(&serde_json::json!({ "redelivered": redelivered }));
        }
        // Nothing was sent, so withdraw a credential this request staged.
        if let Some(hash) = created_hash {
            query!(
                &db,
                "UPDATE agents SET pending_auth_token_hash = NULL WHERE id = ?1 AND pending_auth_token_hash = ?2",
                device_id,
                hash
            )?
            .run()
            .await?;
            storage.delete(PENDING_ROTATION_KEY).await?;
        }
        Response::error(OFFLINE_FOR_ROTATION, 409)
    }

    /// The staged rotation to resend to a connecting Agent. One that D1 no
    /// longer expects was promoted or abandoned: it is deleted, because the
    /// plaintext credential must not outlive its use and an Agent that
    /// adopted a credential D1 does not accept would be locked out.
    async fn pending_rotation(&self, identity: &AgentIdentity) -> Result<Option<AgentCommand>> {
        let storage = self.state.storage();
        let Some(command) = storage.get::<AgentCommand>(PENDING_ROTATION_KEY).await? else {
            return Ok(None);
        };
        let db = self.environment.d1("DB")?;
        let pending = credential_state(&db, &identity.company_id, &identity.device_id)
            .await?
            .and_then(|credential| credential.pending_auth_token_hash);
        if rotation_matches(&command, pending.as_deref()) {
            return Ok(Some(command));
        }
        storage.delete(PENDING_ROTATION_KEY).await?;
        console_log!("event=agent_rotation_cleared");
        Ok(None)
    }

    /// Revokes the Agent when its company is no longer active, so a suspension
    /// whose revocation did not reach this coordinator still takes effect at
    /// the next session request or alarm. A failed lookup revokes nothing.
    async fn revoke_if_company_inactive(&self) -> Result<bool> {
        let Some(identity) = self
            .state
            .storage()
            .get::<AgentIdentity>(IDENTITY_KEY)
            .await?
        else {
            return Ok(false);
        };
        let active = async {
            let db = self.environment.d1("DB")?;
            query!(
                &db,
                "SELECT 1 AS active FROM companies WHERE id = ?1 AND status = 'active'",
                identity.company_id
            )?
            .first::<i64>(Some("active"))
            .await
        }
        .await;
        match active {
            Ok(Some(_)) => Ok(false),
            Ok(None) => {
                console_log!("event=agent_revoked_for_inactive_company");
                self.revoke().await?;
                Ok(true)
            }
            Err(error) => {
                console_error!("event=agent_company_check_failed error={}", error);
                Ok(false)
            }
        }
    }

    async fn clear_session(&self, session_id: &str) -> Result<()> {
        if self
            .state
            .storage()
            .get::<AgentSessionRequest>(ACTIVE_SESSION_KEY)
            .await?
            .is_some_and(|active| active.session_id.as_str() == session_id)
        {
            self.forget_session().await?;
            if let Some(agent) = self
                .state
                .get_websockets_with_tag(AGENT_TAG)
                .into_iter()
                .next()
            {
                agent.send_with_str(serde_json::to_string(&AgentCommand::EndSession {
                    session_id: meshrmm_protocol_types::RemoteSessionId::new(session_id.to_owned()),
                })?)?;
            }
            console_log!("event=agent_session_cleared session_id={}", session_id);
        }
        Ok(())
    }

    /// Returns the active session as it was last delivered to the Agent, and
    /// its expiry including lease renewals.
    async fn active_session(&self) -> Result<Option<(AgentSessionRequest, u64)>> {
        let Some(session) = self
            .state
            .storage()
            .get::<AgentSessionRequest>(ACTIVE_SESSION_KEY)
            .await?
        else {
            return Ok(None);
        };
        let lease = self
            .state
            .storage()
            .get::<SessionLease>(ACTIVE_SESSION_LEASE_KEY)
            .await?;
        let expires_at_unix_ms = session_expiry(&session, lease.as_ref());
        Ok(Some((session, expires_at_unix_ms)))
    }

    /// Records a session delivered to the Agent. Its request carries its own
    /// expiry, which replaces any earlier renewal.
    async fn store_session(&self, session: &AgentSessionRequest) -> Result<()> {
        self.state
            .storage()
            .put(ACTIVE_SESSION_KEY, session)
            .await?;
        self.state
            .storage()
            .delete(ACTIVE_SESSION_LEASE_KEY)
            .await?;
        Ok(())
    }

    async fn forget_session(&self) -> Result<()> {
        self.state.storage().delete(ACTIVE_SESSION_KEY).await?;
        self.state
            .storage()
            .delete(ACTIVE_SESSION_LEASE_KEY)
            .await?;
        Ok(())
    }

    async fn owns_session(&self, requested: &AgentSessionRequest) -> Result<bool> {
        Ok(self
            .active_session()
            .await?
            .is_some_and(|(active, expires_at_unix_ms)| {
                lease_matches(
                    &active,
                    expires_at_unix_ms,
                    requested,
                    Date::now().as_millis(),
                )
            }))
    }

    async fn acknowledge_uninstall(&self, socket: &WebSocket) -> Result<()> {
        let Some(connection_id) = socket.deserialize_attachment::<String>()? else {
            return Ok(());
        };
        let Some(identity) = self
            .state
            .storage()
            .get::<AgentIdentity>(IDENTITY_KEY)
            .await?
        else {
            return Ok(());
        };
        if identity.connection_id != connection_id {
            return Ok(());
        }

        console_log!(
            "event=agent_uninstall_scheduled device_id={}",
            identity.device_id
        );
        socket.close(Some(4001), Some("Agent uninstall scheduled"))?;
        Ok(())
    }

    async fn publish_disconnected_if_current(&self, socket: &WebSocket) {
        let _guard = self.presence_lock.lock().await;
        if let Err(error) = self.state.storage().set_alarm(30_000_i64).await {
            console_error!("event=agent_presence_retry_schedule_failed error={}", error);
            return;
        }
        let connection_id = match socket.deserialize_attachment::<String>() {
            Ok(Some(connection_id)) => connection_id,
            Ok(None) => return,
            Err(error) => {
                console_error!("event=agent_connection_attachment_failed error={}", error);
                return;
            }
        };
        match self
            .state
            .storage()
            .get::<AgentIdentity>(IDENTITY_KEY)
            .await
        {
            Ok(Some(identity)) if identity.connection_id == connection_id => {
                if let Err(error) = self.publish_presence(&identity, false).await {
                    console_error!(
                        "event=agent_presence_publish_failed connected=false error={}",
                        error
                    );
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(error) => console_error!("event=agent_identity_read_failed error={}", error),
        }
    }

    async fn publish_presence(&self, identity: &AgentIdentity, connected: bool) -> Result<()> {
        let mut delivery = self
            .state
            .storage()
            .get::<PresenceDelivery>(PRESENCE_DELIVERY_KEY)
            .await?
            .unwrap_or_default();
        if delivery.connection_id != identity.connection_id || delivery.connected != connected {
            delivery.generation = delivery
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::RustError("presence generation exhausted".into()))?;
            delivery.connection_id = identity.connection_id.clone();
            delivery.connected = connected;
            delivery.acknowledged = false;
        }
        if !delivery.acknowledged {
            // Write the retry alarm before the outbox; neither a failed RPC nor
            // termination between awaits can silently discard a transition.
            self.state.storage().set_alarm(30_000_i64).await?;
            self.state
                .storage()
                .put(PRESENCE_DELIVERY_KEY, &delivery)
                .await?;
            let mutation = PresenceMutation::Connection {
                agent_id: identity.device_id.clone(),
                connected,
                generation: delivery.generation,
            };
            company_presence::publish(&self.environment, &identity.company_id, &mutation).await?;
            delivery.acknowledged = true;
            self.state
                .storage()
                .put(PRESENCE_DELIVERY_KEY, &delivery)
                .await?;
        }
        self.state.storage().delete_alarm().await?;
        Ok(())
    }
}

async fn credential_state(
    db: &D1Database,
    company_id: &str,
    device_id: &str,
) -> Result<Option<CredentialState>> {
    query!(
        db,
        "SELECT pending_auth_token_hash FROM agents WHERE id = ?1 AND company_id = ?2 AND deletion_requested_at IS NULL",
        device_id,
        company_id
    )?
    .first::<CredentialState>(None)
    .await
}

/// Whether `command` carries the credential whose hash D1 holds as pending.
fn rotation_matches(command: &AgentCommand, pending_hash: Option<&str>) -> bool {
    match (command, pending_hash) {
        (AgentCommand::RotateToken { token }, Some(pending)) => {
            crate::constant_time_eq(crate::sha256_hex(token).as_bytes(), pending.as_bytes())
        }
        _ => false,
    }
}

fn required_header(request: &Request, name: &str) -> Result<String> {
    request
        .headers()
        .get(name)?
        .ok_or_else(|| Error::RustError(format!("missing internal Agent header {name}")))
}

pub async fn request_uninstall(environment: &Env, device_id: &str) -> Result<()> {
    let request = Request::new("https://agent.internal/uninstall", Method::Post)?;
    let response = crate::object_stub(environment, "AGENT_COORDINATOR", device_id)?
        .fetch_with_request(request)
        .await?;
    crate::ensure_success(response, "notify Agent uninstall").await
}

fn lease_matches(
    active: &AgentSessionRequest,
    expires_at_unix_ms: u64,
    requested: &AgentSessionRequest,
    now: u64,
) -> bool {
    active.session_id == requested.session_id && expires_at_unix_ms > now
}

/// Coordinators written before leases were stored separately advanced the
/// request's own expiry, so a request without a matching lease keeps it.
fn session_expiry(session: &AgentSessionRequest, lease: Option<&SessionLease>) -> u64 {
    match lease {
        Some(lease) if lease.session_id == session.session_id => lease.expires_at_unix_ms,
        _ => session.expires_at_unix_ms,
    }
}

fn session_payload(session: &AgentSessionRequest) -> Result<String> {
    if session.start_in_background {
        Ok(serde_json::to_string(
            &AgentCommand::StartBackgroundSession {
                request: session.clone(),
            },
        )?)
    } else {
        Ok(serde_json::to_string(session)?)
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[test]
    fn resume_cannot_take_over_another_or_expired_session() {
        let active = AgentSessionRequest {
            start_in_background: false,
            idle_policy: Default::default(),
            blackout_message: String::new(),
            viewer_name: "Ada Lovelace".into(),
            session_id: meshrmm_protocol_types::RemoteSessionId::new("one"),
            signaling_token: "token".into(),
            expires_at_unix_ms: 100,
            ice_servers: vec![],
        };
        assert!(lease_matches(&active, 100, &active, 99));
        assert!(!lease_matches(&active, 100, &active, 100));
        let mut other = active.clone();
        other.session_id = meshrmm_protocol_types::RemoteSessionId::new("two");
        assert!(!lease_matches(&active, 100, &other, 1));
        let mut background = active.clone();
        background.start_in_background = true;
        let payload = session_payload(&background).unwrap();
        assert!(serde_json::from_str::<AgentSessionRequest>(&payload).is_err());
        assert!(matches!(
            serde_json::from_str::<AgentCommand>(&payload).unwrap(),
            AgentCommand::StartBackgroundSession { request } if request == background
        ));
        assert_eq!(session_expiry(&active, None), 100);
        let renewed = SessionLease {
            session_id: active.session_id.clone(),
            expires_at_unix_ms: 200,
        };
        assert_eq!(session_expiry(&active, Some(&renewed)), 200);
        let stale = SessionLease {
            session_id: other.session_id.clone(),
            expires_at_unix_ms: 300,
        };
        assert_eq!(session_expiry(&active, Some(&stale)), 100);
        assert_eq!(
            serde_json::from_str::<AgentSessionRequest>(&session_payload(&active).unwrap())
                .unwrap(),
            active
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_credential_pending_in_d1_is_resent() {
        let token = "a".repeat(64);
        let rotation = AgentCommand::RotateToken {
            token: token.clone(),
        };
        let pending = crate::sha256_hex(&token);
        assert!(rotation_matches(&rotation, Some(&pending)));
        // Promoted or withdrawn in D1, or replaced by another rotation.
        assert!(!rotation_matches(&rotation, None));
        assert!(!rotation_matches(
            &rotation,
            Some(&crate::sha256_hex(&"b".repeat(64)))
        ));
        assert!(!rotation_matches(&AgentCommand::Uninstall, Some(&pending)));
    }
}
