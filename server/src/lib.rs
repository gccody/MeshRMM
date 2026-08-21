use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use pulsermm_protocol::{
    AgentSessionRequest, ApiError, IceServer, RemoteSessionId, SessionBootstrap,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use worker::{query, *};

mod agent_coordinator;
mod company_presence;
mod remote_session;

const DEFAULT_SESSION_IDLE_TIMEOUT_SECONDS: u64 = 900;
const MAX_SESSION_IDLE_TIMEOUT_SECONDS: u64 = 3600;
const HANDOFF_TTL_MS: u64 = 60_000;
const AGENT_INSTALL_TTL_MS: u64 = 30 * 60 * 1000;
const AGENT_EVENT_SUBSCRIPTION_TTL_MS: u64 = 60_000;

#[derive(Debug, Deserialize, Serialize)]
struct Company {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct AgentCredentialRow {
    auth_token_hash: String,
    company_id: String,
}

#[derive(Debug, Deserialize)]
struct HandoffRow {
    company_id: String,
    device_id: String,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct AgentInstallTokenRow {
    id: String,
    company_id: String,
    created_by_user_id: String,
}

#[derive(Debug, Deserialize)]
struct AgentEventSubscriptionRow {
    company_id: String,
}

#[derive(Debug, Deserialize)]
struct WorkOsClaims {
    sub: String,
    client_id: String,
    iss: String,
    exp: u64,
    #[serde(default)]
    org_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug)]
struct Identity {
    user_id: String,
    company_id: String,
    role: Option<String>,
    roles: Vec<String>,
    permissions: Vec<String>,
}

impl Identity {
    fn is_admin(&self) -> bool {
        self.role.as_deref() == Some("admin") || self.roles.iter().any(|role| role == "admin")
    }
}

#[derive(Debug, Serialize)]
struct AccountResponse {
    user_id: String,
    company: Option<Company>,
    role: Option<String>,
    roles: Vec<String>,
    permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BootstrapCompanyRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct CreateAgentInstallerRequest {
    platform: String,
}

#[derive(Debug, Serialize)]
struct AgentInstallerBootstrap {
    server: String,
    install_token: String,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct AgentEventSubscription {
    subscription_token: String,
    websocket_url: String,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
struct RedeemAgentInstallerRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct AgentConfig {
    server: String,
    device_id: String,
    agent_token: String,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    json_logs: bool,
}

#[derive(Debug, Deserialize)]
struct HandoffRequest {
    device_id: String,
}

#[derive(Debug, Serialize)]
struct HandoffResponse {
    handoff_token: String,
    api_url: String,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct SessionInit<'a> {
    client_token: &'a str,
    agent_token: &'a str,
    expires_at_unix_ms: u64,
    idle_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct TurnResponse {
    #[serde(rename = "iceServers")]
    ice_servers: Vec<IceServer>,
}

#[event(fetch)]
async fn fetch(mut request: Request, environment: Env, _context: Context) -> Result<Response> {
    if request.method() == Method::Options {
        return cors(Response::empty()?, &environment);
    }

    let method = request.method();
    let path = request.path();
    let segments: Vec<_> = path.trim_matches('/').split('/').collect();
    let response = match (method, segments.as_slice()) {
        (Method::Get, ["healthz"]) => Response::ok("ok"),
        (Method::Get, ["v1", "account"]) => account(&request, &environment).await,
        (Method::Post, ["v1", "company", "bootstrap"]) => {
            bootstrap_company(&mut request, &environment).await
        }
        (Method::Get, ["v1", "agents"]) => list_agents(&request, &environment).await,
        (Method::Post, ["v1", "agents", "events", "subscriptions"]) => {
            create_agent_event_subscription(&request, &environment).await
        }
        (Method::Get, ["v1", "agents", "events"]) => {
            subscribe_agent_events(request, &environment).await
        }
        (Method::Post, ["v1", "agent-installers"]) => {
            create_agent_installer(&mut request, &environment).await
        }
        (Method::Post, ["v1", "agent-installers", "redeem"]) => {
            redeem_agent_installer(&mut request, &environment).await
        }
        (Method::Delete, ["v1", "agents", device_id]) => {
            delete_agent(&request, &environment, device_id).await
        }
        (Method::Post, ["v1", "agents", device_id, "rotate-token"]) => {
            rotate_agent_token(&request, &environment, device_id).await
        }
        (Method::Get, ["v1", "agents", device_id, "connect"]) => {
            let company_id = match authorize_agent(&request, &environment, device_id).await {
                Ok(company_id) => company_id,
                Err(_) => {
                    return cors(api_error(401, "Agent authentication failed")?, &environment);
                }
            };
            forward_to_object(
                &environment,
                "AGENT_COORDINATOR",
                device_id,
                request,
                "https://agent.internal/connect",
                &[
                    ("X-Pulse-Company-Id", company_id.as_str()),
                    ("X-Pulse-Device-Id", device_id),
                ],
            )
            .await
        }
        (Method::Post, ["v1", "remote", "handoffs"]) => {
            create_handoff(&mut request, &environment).await
        }
        (Method::Post, ["v1", "remote", "handoffs", "redeem"]) => {
            redeem_handoff(&request, &environment).await
        }
        (Method::Get, ["v1", "remote", "sessions", session_id, "signal"]) => {
            if Uuid::parse_str(session_id).is_err() {
                return cors(api_error(400, "invalid session ID")?, &environment);
            }
            let query = request.url()?.query().map(str::to_owned);
            let internal_url = match query {
                Some(query) => format!("https://session.internal/signal?{query}"),
                None => "https://session.internal/signal".to_owned(),
            };
            forward_to_object(
                &environment,
                "REMOTE_SESSION",
                session_id,
                request,
                &internal_url,
                &[],
            )
            .await
        }
        _ => api_error(404, "route not found"),
    }?;
    cors(response, &environment)
}

async fn account(request: &Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    account_for_identity(environment, identity).await
}

async fn account_for_identity(environment: &Env, identity: Identity) -> Result<Response> {
    let db = environment.d1("DB")?;
    let company = query!(
        &db,
        "SELECT id, name FROM companies WHERE id = ?1",
        identity.company_id
    )?
    .first::<Company>(None)
    .await?;
    Response::from_json(&AccountResponse {
        user_id: identity.user_id,
        company,
        role: identity.role,
        roles: identity.roles,
        permissions: identity.permissions,
    })
}

async fn bootstrap_company(request: &mut Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let body: BootstrapCompanyRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid company request".into()))?;
    let name = validate_name(&body.name, "company name")?;
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    query!(
        &db,
        "INSERT INTO companies (id, name, created_at) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO NOTHING",
        identity.company_id,
        name,
        now
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "company.bootstrap",
        "company",
        &identity.company_id,
        "{}",
    )
    .await?;
    account_for_identity(environment, identity).await
}

async fn list_agents(request: &Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    company_presence::snapshot(environment, &identity.company_id).await
}

async fn create_agent_event_subscription(request: &Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    ensure_company_exists(&db, &identity.company_id).await?;
    let now = now_ms_i64()?;
    query!(
        &db,
        "DELETE FROM agent_event_subscriptions WHERE expires_at <= ?1",
        now
    )?
    .run()
    .await?;
    let subscription_token = random_token();
    let token_hash = sha256_hex(&subscription_token);
    let expires_at = Date::now().as_millis() + AGENT_EVENT_SUBSCRIPTION_TTL_MS;
    let expires_at_i64 =
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?;
    query!(
        &db,
        "INSERT INTO agent_event_subscriptions (token_hash, company_id, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        token_hash,
        identity.company_id,
        identity.user_id,
        now,
        expires_at_i64
    )?
    .run()
    .await?;
    Response::from_json(&AgentEventSubscription {
        subscription_token,
        websocket_url: agent_event_websocket_url(environment)?,
        expires_at_unix_ms: expires_at,
    })
}

async fn subscribe_agent_events(request: Request, environment: &Env) -> Result<Response> {
    if request
        .headers()
        .get("Upgrade")?
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
    {
        return api_error(426, "WebSocket upgrade required");
    }
    let expected_origin = environment.var("DASHBOARD_ORIGIN")?.to_string();
    if request.headers().get("Origin")?.as_deref() != Some(expected_origin.as_str()) {
        return api_error(403, "dashboard origin is not allowed");
    }
    let supplied = request
        .url()?
        .query_pairs()
        .find_map(|(key, value)| (key == "token").then(|| value.into_owned()))
        .unwrap_or_default();
    if supplied.len() != 64 || !supplied.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return api_error(401, "Agent event subscription is invalid or expired");
    }
    let now = now_ms_i64()?;
    let token_hash = sha256_hex(&supplied);
    let db = environment.d1("DB")?;
    let subscription = query!(
        &db,
        "UPDATE agent_event_subscriptions SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 RETURNING company_id",
        now,
        token_hash
    )?
    .first::<AgentEventSubscriptionRow>(None)
    .await?;
    let Some(subscription) = subscription else {
        return api_error(401, "Agent event subscription is invalid or expired");
    };
    forward_to_object(
        environment,
        "COMPANY_PRESENCE",
        &subscription.company_id,
        request,
        "https://presence.internal/subscribe",
        &[("X-Pulse-Company-Id", subscription.company_id.as_str())],
    )
    .await
}

async fn create_agent_installer(request: &mut Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let body: CreateAgentInstallerRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid Agent installer request".into()))?;
    if body.platform != "windows-x64" {
        return api_error(400, "unsupported Agent installer platform");
    }

    let db = environment.d1("DB")?;
    ensure_company_exists(&db, &identity.company_id).await?;
    let now = now_ms_i64()?;
    query!(
        &db,
        "DELETE FROM agent_install_tokens WHERE expires_at <= ?1",
        now
    )?
    .run()
    .await?;
    let install_id = Uuid::new_v4().to_string();
    let install_token = random_token();
    let token_hash = sha256_hex(&install_token);
    let expires_at = Date::now().as_millis() + AGENT_INSTALL_TTL_MS;
    let expires_at_i64 =
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?;
    query!(
        &db,
        "INSERT INTO agent_install_tokens (id, token_hash, company_id, created_by_user_id, platform, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        install_id,
        token_hash,
        identity.company_id,
        identity.user_id,
        body.platform,
        now,
        expires_at_i64
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "agent_installer.issue",
        "agent_installer",
        &install_id,
        &serde_json::json!({ "platform": body.platform }).to_string(),
    )
    .await?;
    Response::from_json(&AgentInstallerBootstrap {
        server: public_api_url(environment)?,
        install_token,
        expires_at_unix_ms: expires_at,
    })
}

async fn redeem_agent_installer(request: &mut Request, environment: &Env) -> Result<Response> {
    let supplied = match bearer_token(request) {
        Ok(token) => token,
        Err(_) => return api_error(401, "Agent installer token is required"),
    };
    let body: RedeemAgentInstallerRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid Agent installer redemption".into()))?;
    let name = validate_name(&body.name, "computer name")?.to_owned();
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let token_hash = sha256_hex(&supplied);
    let ticket = query!(
        &db,
        "UPDATE agent_install_tokens SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 RETURNING id, company_id, created_by_user_id",
        now,
        token_hash
    )?
    .first::<AgentInstallTokenRow>(None)
    .await?;
    let Some(ticket) = ticket else {
        return api_error(401, "Agent installer is invalid, expired, or already used");
    };

    let device_id = Uuid::new_v4().to_string();
    let agent_token = random_token();
    let agent_token_hash = sha256_hex(&agent_token);
    query!(
        &db,
        "INSERT INTO agents (id, company_id, name, auth_token_hash, created_by_user_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        device_id,
        ticket.company_id,
        name,
        agent_token_hash,
        ticket.created_by_user_id,
        now
    )?
    .run()
    .await?;
    let identity = Identity {
        user_id: ticket.created_by_user_id,
        company_id: ticket.company_id,
        role: None,
        roles: Vec::new(),
        permissions: Vec::new(),
    };
    audit(
        &db,
        &identity,
        "agent_installer.redeem",
        "agent",
        &device_id,
        &serde_json::json!({ "installer_id": ticket.id }).to_string(),
    )
    .await?;
    let created = company_presence::PresenceMutation::Upsert {
        agent_id: device_id.clone(),
        name: Some(name.clone()),
        connected: Some(false),
    };
    if let Err(error) = company_presence::publish(environment, &identity.company_id, &created).await
    {
        console_error!("event=agent_created_publish_failed error={}", error);
    }
    Response::from_json(&AgentConfig {
        server: public_api_url(environment)?,
        device_id,
        agent_token,
        frames_per_second: 60,
        bitrate_bits_per_second: 12_000_000,
        json_logs: false,
    })
}

async fn delete_agent(request: &Request, environment: &Env, device_id: &str) -> Result<Response> {
    validate_identifier(device_id, "device ID")?;
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "DELETE FROM agents WHERE id = ?1 AND company_id = ?2",
        device_id,
        identity.company_id
    )?
    .run()
    .await?;
    if result
        .meta()?
        .and_then(|meta| meta.changes)
        .unwrap_or_default()
        == 0
    {
        return api_error(404, "Agent not found");
    }
    audit(&db, &identity, "agent.delete", "agent", device_id, "{}").await?;
    let deleted = company_presence::PresenceMutation::Delete {
        agent_id: device_id.to_owned(),
    };
    if let Err(error) = company_presence::publish(environment, &identity.company_id, &deleted).await
    {
        console_error!("event=agent_deleted_publish_failed error={}", error);
    }
    Response::empty().map(|response| response.with_status(204))
}

async fn rotate_agent_token(
    request: &Request,
    environment: &Env,
    device_id: &str,
) -> Result<Response> {
    validate_identifier(device_id, "device ID")?;
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let agent_token = random_token();
    let token_hash = sha256_hex(&agent_token);
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "UPDATE agents SET auth_token_hash = ?1, updated_at = ?2 WHERE id = ?3 AND company_id = ?4",
        token_hash,
        now,
        device_id,
        identity.company_id
    )?
    .run()
    .await?;
    if result
        .meta()?
        .and_then(|meta| meta.changes)
        .unwrap_or_default()
        == 0
    {
        return api_error(404, "Agent not found");
    }
    audit(
        &db,
        &identity,
        "agent.rotate_token",
        "agent",
        device_id,
        "{}",
    )
    .await?;
    Response::from_json(&serde_json::json!({
        "device_id": device_id,
        "agent_token": agent_token
    }))
}

async fn create_handoff(request: &mut Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    let body: HandoffRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid remote handoff request".into()))?;
    validate_identifier(&body.device_id, "device ID")?;
    let db = environment.d1("DB")?;
    let permitted = query!(
        &db,
        "SELECT 1 AS permitted FROM agents WHERE id = ?1 AND company_id = ?2",
        body.device_id,
        identity.company_id
    )?
    .first::<i64>(Some("permitted"))
    .await?
    .is_some();
    if !permitted {
        return api_error(404, "Agent not found");
    }
    let handoff_token = random_token();
    let token_hash = sha256_hex(&handoff_token);
    let created_at = Date::now().as_millis();
    let expires_at = created_at + HANDOFF_TTL_MS;
    query!(
        &db,
        "DELETE FROM remote_handoffs WHERE expires_at <= ?1",
        now_ms_i64()?
    )?
    .run()
    .await?;
    query!(
        &db,
        "INSERT INTO remote_handoffs (token_hash, company_id, device_id, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        token_hash,
        identity.company_id,
        body.device_id,
        identity.user_id,
        i64::try_from(created_at).map_err(|_| Error::RustError("clock overflow".into()))?,
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "remote.handoff_create",
        "agent",
        &body.device_id,
        "{}",
    )
    .await?;
    Response::from_json(&HandoffResponse {
        handoff_token,
        api_url: public_api_url(environment)?,
        expires_at_unix_ms: expires_at,
    })
}

async fn redeem_handoff(request: &Request, environment: &Env) -> Result<Response> {
    let supplied = match bearer_token(request) {
        Ok(token) => token,
        Err(_) => return api_error(401, "remote handoff token is required"),
    };
    let token_hash = sha256_hex(&supplied);
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let handoff = query!(
        &db,
        "UPDATE remote_handoffs SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 RETURNING company_id, device_id, user_id",
        now,
        token_hash
    )?
    .first::<HandoffRow>(None)
    .await?;
    let Some(handoff) = handoff else {
        return api_error(401, "remote handoff is invalid, expired, or already used");
    };
    let response = create_session_for_device(environment, &handoff.device_id).await?;
    let identity = Identity {
        user_id: handoff.user_id,
        company_id: handoff.company_id,
        role: None,
        roles: Vec::new(),
        permissions: Vec::new(),
    };
    audit(
        &db,
        &identity,
        "remote.session_create",
        "agent",
        &handoff.device_id,
        "{}",
    )
    .await?;
    Ok(response)
}

async fn create_session_for_device(environment: &Env, device_id: &str) -> Result<Response> {
    let session_id = Uuid::new_v4().to_string();
    let client_token = random_token();
    let agent_token = random_token();
    let idle_timeout_seconds = session_idle_timeout(environment)?;
    let idle_timeout_ms = idle_timeout_seconds * 1000;
    let expires_at_unix_ms = Date::now().as_millis() + idle_timeout_ms;
    let ice_servers = generate_ice_servers(environment, idle_timeout_seconds).await?;

    let init = SessionInit {
        client_token: &client_token,
        agent_token: &agent_token,
        expires_at_unix_ms,
        idle_timeout_ms,
    };
    let init_request = internal_json_request("https://session.internal/init", &init)?;
    let session_stub = object_stub(environment, "REMOTE_SESSION", &session_id)?;
    ensure_success(
        session_stub.fetch_with_request(init_request).await?,
        "initialize session",
    )
    .await?;

    let agent_request = AgentSessionRequest {
        session_id: RemoteSessionId::new(&session_id),
        signaling_token: agent_token,
        expires_at_unix_ms,
        ice_servers: ice_servers.clone(),
    };
    let notify_request = internal_json_request("https://agent.internal/request", &agent_request)?;
    let notify_response = object_stub(environment, "AGENT_COORDINATOR", device_id)?
        .fetch_with_request(notify_request)
        .await?;
    if !(200..300).contains(&notify_response.status_code()) {
        let cleanup = Request::new("https://session.internal/expire", Method::Post)?;
        let _ = session_stub.fetch_with_request(cleanup).await;
        return api_error(409, "target Agent is not connected");
    }

    console_log!(
        "event=remote_session_created session_id={} device_id={} expires_at_ms={}",
        session_id,
        device_id,
        expires_at_unix_ms
    );
    Response::from_json(&SessionBootstrap {
        session_id: RemoteSessionId::new(session_id),
        signaling_token: client_token,
        expires_at_unix_ms,
        ice_servers,
    })
}

async fn authorize_agent(request: &Request, environment: &Env, device_id: &str) -> Result<String> {
    validate_identifier(device_id, "device ID")?;
    let supplied = bearer_token(request)?;
    let db = environment.d1("DB")?;
    let credential = query!(
        &db,
        "SELECT auth_token_hash, company_id FROM agents WHERE id = ?1",
        device_id
    )?
    .first::<AgentCredentialRow>(None)
    .await?
    .ok_or_else(|| Error::RustError("unauthorized Agent".into()))?;
    if !constant_time_eq(
        sha256_hex(&supplied).as_bytes(),
        credential.auth_token_hash.as_bytes(),
    ) {
        return Err(Error::RustError("invalid Agent token".into()));
    }
    Ok(credential.company_id)
}

async fn authorize_workos_user(request: &Request, environment: &Env) -> Result<Identity> {
    let token = bearer_token(request)?;
    let client_id = environment.var("WORKOS_CLIENT_ID")?.to_string();
    validate_identifier(&client_id, "WorkOS client ID")?;
    let header = decode_header(&token)
        .map_err(|_| Error::RustError("invalid WorkOS access token".into()))?;
    if header.alg != Algorithm::RS256 {
        return Err(Error::RustError(
            "unsupported WorkOS token algorithm".into(),
        ));
    }
    let key_id = header
        .kid
        .ok_or_else(|| Error::RustError("WorkOS token is missing a key ID".into()))?;
    let jwks_url = format!("https://api.workos.com/sso/jwks/{client_id}");
    let mut jwks_response = Fetch::Url(jwks_url.parse()?).send().await?;
    if !(200..300).contains(&jwks_response.status_code()) {
        return Err(Error::RustError(format!(
            "WorkOS JWKS returned HTTP {}",
            jwks_response.status_code()
        )));
    }
    let jwks: JwkSet = jwks_response.json().await?;
    let jwk = jwks
        .find(&key_id)
        .ok_or_else(|| Error::RustError("WorkOS signing key was not found".into()))?;
    let decoding_key = DecodingKey::from_jwk(jwk)
        .map_err(|_| Error::RustError("invalid WorkOS signing key".into()))?;

    let issuer = environment
        .var("WORKOS_ISSUER")
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "https://api.workos.com".to_owned());
    let issuer_with_slash = format!("{}/", issuer.trim_end_matches('/'));
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp", "iss", "sub"]);
    validation.set_issuer(&[issuer.as_str(), issuer_with_slash.as_str()]);
    let claims = decode::<WorkOsClaims>(&token, &decoding_key, &validation)
        .map_err(|error| Error::RustError(format!("invalid WorkOS access token: {error}")))?
        .claims;
    if claims.client_id != client_id
        || claims.iss.trim_end_matches('/') != issuer.trim_end_matches('/')
        || claims.exp * 1000 <= Date::now().as_millis()
    {
        return Err(Error::RustError(
            "invalid WorkOS access token claims".into(),
        ));
    }
    let company_id = claims
        .org_id
        .ok_or_else(|| Error::RustError("WorkOS session has no organization".into()))?;
    validate_identifier(&company_id, "WorkOS organization ID")?;
    Ok(Identity {
        user_id: claims.sub,
        company_id,
        role: claims.role,
        roles: claims.roles,
        permissions: claims.permissions,
    })
}

async fn ensure_company_exists(db: &D1Database, company_id: &str) -> Result<()> {
    if query!(
        db,
        "SELECT id, name FROM companies WHERE id = ?1",
        company_id
    )?
    .first::<Company>(None)
    .await?
    .is_none()
    {
        return Err(Error::RustError("company has not been provisioned".into()));
    }
    Ok(())
}

async fn audit(
    db: &D1Database,
    identity: &Identity,
    action: &str,
    target_type: &str,
    target_id: &str,
    metadata_json: &str,
) -> Result<()> {
    query!(
        db,
        "INSERT INTO audit_events (id, company_id, actor_user_id, action, target_type, target_id, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        Uuid::new_v4().to_string(),
        identity.company_id,
        identity.user_id,
        action,
        target_type,
        target_id,
        metadata_json,
        now_ms_i64()?
    )?
    .run()
    .await?;
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    valid
        .then_some(())
        .ok_or_else(|| Error::RustError(format!("invalid {label}")))
}

fn validate_name<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let value = value.trim();
    if !value.is_empty() && value.len() <= 120 && !value.chars().any(char::is_control) {
        Ok(value)
    } else {
        Err(Error::RustError(format!("invalid {label}")))
    }
}

fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let left = Sha256::digest(left);
    let right = Sha256::digest(right);
    bool::from(left.ct_eq(&right))
}

fn bearer_token(request: &Request) -> Result<String> {
    request
        .headers()
        .get("Authorization")?
        .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned))
        .ok_or_else(|| Error::RustError("missing bearer token".into()))
}

fn public_api_url(environment: &Env) -> Result<String> {
    Ok(environment
        .var("PUBLIC_API_URL")?
        .to_string()
        .trim_end_matches('/')
        .to_owned())
}

fn agent_event_websocket_url(environment: &Env) -> Result<String> {
    let api_url = public_api_url(environment)?;
    let host = api_url
        .strip_prefix("https://")
        .ok_or_else(|| Error::RustError("PUBLIC_API_URL must use HTTPS".into()))?;
    Ok(format!("wss://{host}/v1/agents/events"))
}

fn now_ms_i64() -> Result<i64> {
    i64::try_from(Date::now().as_millis()).map_err(|_| Error::RustError("clock overflow".into()))
}

fn session_idle_timeout(environment: &Env) -> Result<u64> {
    let timeout = environment
        .var("REMOTE_SESSION_IDLE_TIMEOUT_SECONDS")
        // Preserve deployments that still inject the former variable name
        // outside wrangler.jsonc while changing its semantics to idle time.
        .or_else(|_| environment.var("REMOTE_SESSION_TTL_SECONDS"))
        .ok()
        .and_then(|value| value.to_string().parse().ok())
        .unwrap_or(DEFAULT_SESSION_IDLE_TIMEOUT_SECONDS);
    if (60..=MAX_SESSION_IDLE_TIMEOUT_SECONDS).contains(&timeout) {
        Ok(timeout)
    } else {
        Err(Error::RustError(
            "REMOTE_SESSION_IDLE_TIMEOUT_SECONDS must be between 60 and 3600".into(),
        ))
    }
}

async fn generate_ice_servers(environment: &Env, ttl_seconds: u64) -> Result<Vec<IceServer>> {
    let key_id = environment.secret("TURN_KEY_ID")?.to_string();
    let api_token = environment.secret("TURN_KEY_API_TOKEN")?.to_string();
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {api_token}"))?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(
            serde_json::to_string(&serde_json::json!({ "ttl": ttl_seconds }))?.into(),
        ));
    let request = Request::new_with_init(
        &format!(
            "https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
        ),
        &init,
    )?;
    let mut response = Fetch::Request(request).send().await?;
    if !(200..300).contains(&response.status_code()) {
        return Err(Error::RustError(format!(
            "TURN credential service returned HTTP {}",
            response.status_code()
        )));
    }
    let mut response: TurnResponse = response.json().await?;
    for server in &mut response.ice_servers {
        server.urls.retain(|url| supported_ice_url(url));
    }
    response
        .ice_servers
        .retain(|server| !server.urls.is_empty());
    if response.ice_servers.is_empty() {
        return Err(Error::RustError(
            "TURN credential service returned no WebRTC-compatible ICE servers".into(),
        ));
    }
    Ok(response.ice_servers)
}

fn supported_ice_url(url: &str) -> bool {
    url == "stun:stun.cloudflare.com:3478" || url == "turn:turn.cloudflare.com:3478?transport=udp"
}

fn object_stub(environment: &Env, binding: &str, name: &str) -> Result<Stub> {
    environment
        .durable_object(binding)?
        .id_from_name(name)?
        .get_stub()
}

async fn forward_to_object(
    environment: &Env,
    binding: &str,
    name: &str,
    request: Request,
    internal_url: &str,
    internal_headers: &[(&str, &str)],
) -> Result<Response> {
    let headers = request.headers().clone();
    for (name, value) in internal_headers {
        headers.set(name, value)?;
    }
    let mut init = RequestInit::new();
    init.with_method(request.method()).with_headers(headers);
    let internal = Request::new_with_init(internal_url, &init)?;
    object_stub(environment, binding, name)?
        .fetch_with_request(internal)
        .await
}

fn internal_json_request<T: Serialize>(url: &str, value: &T) -> Result<Request> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(value)?.into()));
    Request::new_with_init(url, &init)
}

async fn ensure_success(mut response: Response, action: &str) -> Result<()> {
    if (200..300).contains(&response.status_code()) {
        return Ok(());
    }
    let status = response.status_code();
    let body = response.text().await.unwrap_or_default();
    Err(Error::RustError(format!(
        "failed to {action}: HTTP {status}: {body}"
    )))
}

fn api_error(status: u16, message: &str) -> Result<Response> {
    Ok(Response::from_json(&ApiError {
        error: message.to_owned(),
    })?
    .with_status(status))
}

fn workos_auth_error(error: Error) -> Result<Response> {
    let detail = error.to_string();
    let reason = if detail.contains("no organization") {
        "missing_organization"
    } else if detail.contains("authorization header") || detail.contains("bearer") {
        "missing_bearer_token"
    } else if detail.contains("unsupported WorkOS token algorithm") {
        "unsupported_algorithm"
    } else if detail.contains("missing a key ID") {
        "missing_key_id"
    } else if detail.contains("JWKS returned HTTP") {
        "jwks_request_failed"
    } else if detail.contains("signing key was not found") {
        "signing_key_not_found"
    } else if detail.contains("invalid WorkOS signing key") {
        "invalid_signing_key"
    } else if detail.contains("invalid WorkOS access token claims") {
        "client_or_issuer_mismatch"
    } else if detail.contains("invalid WorkOS access token") {
        if detail.contains("InvalidSignature") || detail.contains("Invalid signature") {
            "invalid_signature"
        } else if detail.contains("InvalidIssuer") || detail.contains("Invalid issuer") {
            "invalid_issuer"
        } else if detail.contains("ExpiredSignature") || detail.contains("Expired signature") {
            "expired_token"
        } else if detail.contains("MissingRequiredClaim")
            || detail.contains("Missing required claim")
        {
            "missing_required_claim"
        } else if detail.contains("JSON") || detail.contains("Json") {
            "invalid_claims_shape"
        } else {
            "invalid_signature_or_standard_claims"
        }
    } else {
        "unexpected_validation_error"
    };

    console_error!(
        "{}",
        serde_json::json!({
            "event": "workos_auth_rejected",
            "reason": reason,
        })
    );

    if reason == "missing_organization" {
        api_error(
            401,
            "select a WorkOS organization before accessing company data",
        )
    } else {
        api_error(
            401,
            "your WorkOS session could not be verified; sign out and sign in again",
        )
    }
}

fn cors(response: Response, environment: &Env) -> Result<Response> {
    let origin = environment.var("DASHBOARD_ORIGIN")?.to_string();
    response.with_cors(
        &Cors::new()
            .with_origins(vec![origin.as_str()])
            .with_methods(vec![
                Method::Get,
                Method::Post,
                Method::Delete,
                Method::Options,
            ])
            .with_allowed_headers(vec!["Authorization", "Content-Type"]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_requires_exact_value() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secret-extra"));
    }

    #[test]
    fn identifiers_reject_path_syntax() {
        assert!(validate_identifier("device-01.example", "device ID").is_ok());
        assert!(validate_identifier("../device", "device ID").is_err());
        assert!(validate_identifier("device/01", "device ID").is_err());
    }

    #[test]
    fn filters_ice_urls_to_webrtc_rs_transports() {
        assert!(supported_ice_url("stun:stun.cloudflare.com:3478"));
        assert!(supported_ice_url(
            "turn:turn.cloudflare.com:3478?transport=udp"
        ));
        assert!(!supported_ice_url(
            "turns:turn.cloudflare.com:5349?transport=tcp"
        ));
    }
}
