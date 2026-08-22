use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use pulsermm_protocol_types::{
    AgentSessionRequest, ApiError, IceServer, RemoteSessionId, SessionBootstrap,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use worker::{query, *};

mod agent_coordinator;
mod company_presence;
mod infrastructure;
mod remote_session;
mod routes;

use infrastructure::*;
use routes::*;

const DEFAULT_SESSION_IDLE_TIMEOUT_SECONDS: u64 = 900;
const MAX_SESSION_IDLE_TIMEOUT_SECONDS: u64 = 3600;
const DEFAULT_DASHBOARD_IDLE_TIMEOUT_MINUTES: u32 = 4 * 60;
const MIN_DASHBOARD_IDLE_TIMEOUT_MINUTES: u32 = 5;
const MAX_DASHBOARD_IDLE_TIMEOUT_MINUTES: u32 = 24 * 60;
const HANDOFF_TTL_MS: u64 = 60_000;
const AGENT_INSTALL_TTL_MS: u64 = 30 * 60 * 1000;
const AGENT_EVENT_SUBSCRIPTION_TTL_MS: u64 = 60_000;

#[derive(Debug, Deserialize, Serialize)]
struct Company {
    id: String,
    name: String,
    dashboard_idle_timeout_minutes: u32,
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
struct UpdateCompanySettingsRequest {
    dashboard_idle_timeout_minutes: u32,
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
        (Method::Put, ["v1", "company", "settings"]) => {
            update_company_settings(&mut request, &environment).await
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
        "SELECT id, name, dashboard_idle_timeout_minutes FROM companies WHERE id = ?1",
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

fn cors(response: Response, environment: &Env) -> Result<Response> {
    let origin = environment.var("DASHBOARD_ORIGIN")?.to_string();
    response.with_cors(
        &Cors::new()
            .with_origins(vec![origin.as_str()])
            .with_methods(vec![
                Method::Get,
                Method::Post,
                Method::Put,
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

    #[test]
    fn dashboard_idle_timeout_has_safe_bounds() {
        assert!(validate_dashboard_idle_timeout(5).is_ok());
        assert!(validate_dashboard_idle_timeout(240).is_ok());
        assert!(validate_dashboard_idle_timeout(1440).is_ok());
        assert!(validate_dashboard_idle_timeout(4).is_err());
        assert!(validate_dashboard_idle_timeout(1441).is_err());
    }
}
