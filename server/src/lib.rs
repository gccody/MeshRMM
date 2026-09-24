use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use meshrmm_protocol_types::{
    AgentSessionRequest, ApiError, IceServer, RemoteSessionId, SessionBootstrap,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use worker::{query, *};

mod agent_coordinator;
mod auth;
mod company_presence;
mod infrastructure;
mod remote_session;
mod routes;

use auth::*;
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
    blackout_message: String,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    display_border: bool,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    prevent_idle_lock: bool,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    allow_idle_override: bool,
    slug: Option<String>,
    status: String,
}

#[derive(Debug, Deserialize)]
struct TenantCompany {
    id: String,
    workos_organization_id: Option<String>,
    status: String,
}

#[derive(Debug, Deserialize)]
struct AgentCredentialRow {
    auth_token_hash: String,
    pending_auth_token_hash: Option<String>,
    company_id: String,
    deletion_requested_at: Option<i64>,
}

struct AgentAuthorization {
    company_id: String,
    deletion_requested: bool,
}

#[derive(Debug, Deserialize)]
struct HandoffRow {
    company_id: String,
    device_id: String,
    user_id: String,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    start_in_background: bool,
}

#[derive(Debug, Deserialize)]
struct AgentInstallTokenRow {
    id: String,
    company_id: String,
    created_by_user_id: String,
    device_id: String,
}

#[derive(Debug, Deserialize)]
struct AgentEventSubscriptionRow {
    company_id: String,
    user_id: String,
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
    fn has_permission(&self, permission: &str) -> bool {
        self.permissions
            .iter()
            .any(|candidate| candidate == permission)
            || self
                .role
                .as_deref()
                .is_some_and(|role| matches!(role, "admin" | "company_admin"))
            || self
                .roles
                .iter()
                .any(|role| matches!(role.as_str(), "admin" | "company_admin"))
    }

    fn is_company_admin(&self) -> bool {
        self.has_permission("company:settings:manage")
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
struct UpdateCompanySettingsRequest {
    dashboard_idle_timeout_minutes: u32,
    blackout_message: Option<String>,
    display_border: Option<bool>,
    prevent_idle_lock: Option<bool>,
    allow_idle_override: Option<bool>,
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
    #[serde(default)]
    redemption_key: String,
}

#[derive(Debug, Serialize)]
struct AgentConfig {
    server: String,
    device_id: String,
    agent_token: String,
    update_manifest_url: String,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    json_logs: bool,
}

#[derive(Debug, Deserialize)]
struct HandoffRequest {
    device_id: String,
    #[serde(default)]
    start_in_background: bool,
}

#[derive(Debug, Serialize)]
struct HandoffResponse {
    handoff_token: String,
    api_url: String,
    expires_at_unix_ms: u64,
    start_in_background: bool,
}

#[derive(Debug, Serialize)]
struct SessionInit<'a> {
    start_in_background: bool,
    idle_policy: meshrmm_protocol_types::IdlePolicy,
    blackout_message: &'a str,
    display_border: bool,
    viewer_name: &'a str,
    session_id: &'a str,
    device_id: &'a str,
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
        (Method::Get, ["v1", "auth", "invitations", "resolve"]) => {
            resolve_workos_invitation(&request, &environment).await
        }
        (Method::Get, ["v1", "account"]) => account(&request, &environment).await,
        (Method::Put, ["v1", "company", "settings"]) => {
            update_company_settings(&mut request, &environment).await
        }
        (Method::Get, ["v1", "platform", "companies"]) => {
            list_platform_companies(&request, &environment).await
        }
        (Method::Post, ["v1", "platform", "companies"]) => {
            create_platform_company(&mut request, &environment).await
        }
        (Method::Post, ["v1", "platform", "companies", company_id, "retry"]) => {
            retry_platform_company(&request, &environment, company_id).await
        }
        (Method::Post, ["v1", "platform", "companies", company_id, "domain"]) => {
            assign_platform_company_domain(&mut request, &environment, company_id).await
        }
        (Method::Post, ["v1", "platform", "companies", company_id, "suspend"]) => {
            suspend_platform_company(&request, &environment, company_id).await
        }
        (Method::Post, ["v1", "platform", "companies", company_id, "activate"]) => {
            activate_platform_company(&request, &environment, company_id).await
        }
        (Method::Get, ["v1", "agents"]) => list_agents(&request, &environment).await,
        (Method::Post, ["v1", "agents", "events", "subscriptions"]) => {
            create_agent_event_subscription(&request, &environment).await
        }
        (Method::Post, ["v1", "agents", "events", "subscriptions", "renew"]) => {
            renew_agent_event_subscription(&mut request, &environment).await
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
        (Method::Post, ["v1", "agents", device_id, "close-session"]) => {
            close_agent_session(&request, &environment, device_id).await
        }
        (Method::Post, ["v1", "agents", device_id, "rotate-token"]) => {
            rotate_agent_token(&request, &environment, device_id).await
        }
        (Method::Get, ["v1", "agents", device_id, "connect"]) => {
            let authorization = match authorize_agent(&request, &environment, device_id).await {
                Ok(authorization) => authorization,
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
                    ("X-Mesh-Company-Id", authorization.company_id.as_str()),
                    ("X-Mesh-Device-Id", device_id),
                    (
                        "X-Mesh-Uninstall-Requested",
                        if authorization.deletion_requested {
                            "true"
                        } else {
                            "false"
                        },
                    ),
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
        (Method::Post, ["v1", "remote", "sessions", session_id, "end"]) => {
            if Uuid::parse_str(session_id).is_err() {
                return cors(api_error(400, "invalid session ID")?, &environment);
            }
            forward_to_object(
                &environment,
                "REMOTE_SESSION",
                session_id,
                request,
                "https://session.internal/end",
                &[],
            )
            .await
        }
        (Method::Post, ["v1", "remote", "sessions", session_id, "resume"]) => {
            if Uuid::parse_str(session_id).is_err() {
                return cors(api_error(400, "invalid session ID")?, &environment);
            }
            forward_to_object(
                &environment,
                "REMOTE_SESSION",
                session_id,
                request,
                "https://session.internal/resume",
                &[],
            )
            .await
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

async fn authorize_agent(
    request: &Request,
    environment: &Env,
    device_id: &str,
) -> Result<AgentAuthorization> {
    validate_identifier(device_id, "device ID")?;
    let supplied = bearer_token(request)?;
    let db = environment.d1("DB")?;
    let credential = query!(
        &db,
        "SELECT auth_token_hash, pending_auth_token_hash, company_id, deletion_requested_at FROM agents WHERE id = ?1",
        device_id
    )?
    .first::<AgentCredentialRow>(None)
    .await?
    .ok_or_else(|| Error::RustError("unauthorized Agent".into()))?;
    let supplied_hash = sha256_hex(&supplied);
    let pending_matches = credential
        .pending_auth_token_hash
        .as_ref()
        .is_some_and(|pending| constant_time_eq(supplied_hash.as_bytes(), pending.as_bytes()));
    if !pending_matches
        && !constant_time_eq(
            supplied_hash.as_bytes(),
            credential.auth_token_hash.as_bytes(),
        )
    {
        return Err(Error::RustError("invalid Agent token".into()));
    }
    if let Some(tenant) = request_tenant_company(&db, request, environment).await? {
        if tenant.id != credential.company_id || tenant.status != "active" {
            return Err(Error::RustError(
                "Agent credential does not match the company hostname".into(),
            ));
        }
    } else if !is_legacy_control_plane_request(request, environment)? {
        return Err(Error::RustError("Agent company hostname is invalid".into()));
    }
    let active = query!(
        &db,
        "SELECT 1 AS allowed FROM companies WHERE id = ?1 AND status = 'active'",
        credential.company_id
    )?
    .first::<i64>(Some("allowed"))
    .await?
    .is_some();
    if !active {
        return Err(Error::RustError("company is not active".into()));
    }
    if pending_matches {
        query!(&db, "UPDATE agents SET auth_token_hash = ?1, pending_auth_token_hash = NULL WHERE id = ?2 AND pending_auth_token_hash = ?1", supplied_hash, device_id)?.run().await?;
    }
    Ok(AgentAuthorization {
        company_id: credential.company_id,
        deletion_requested: credential.deletion_requested_at.is_some(),
    })
}

async fn ensure_company_exists(db: &D1Database, company_id: &str) -> Result<()> {
    if query!(
        db,
        "SELECT id, name, dashboard_idle_timeout_minutes, blackout_message, display_border, prevent_idle_lock, allow_idle_override, slug, status FROM companies WHERE id = ?1",
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
    fn company_slugs_are_dns_safe_and_reserved_names_are_rejected() {
        assert_eq!(validate_slug("Acme-Support").unwrap(), "acme-support");
        assert!(validate_slug("admin").is_err());
        assert!(validate_slug("api").is_err());
        assert!(validate_slug("-acme").is_err());
        assert!(validate_slug("acme.example").is_err());
        assert!(validate_slug("a").is_err());
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

// D1 returns SQLite booleans as integers; API responses remain JSON booleans.
fn deserialize_sql_bool<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<bool, D::Error> {
    match u8::deserialize(deserializer)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(serde::de::Error::custom("expected SQLite boolean 0 or 1")),
    }
}

#[cfg(test)]
mod company_policy_tests {
    use super::*;
    #[test]
    fn sqlite_flags_are_exposed_as_json_booleans() {
        for enabled in [0, 1] {
            let row = serde_json::json!({"id":"c","name":"Company","dashboard_idle_timeout_minutes":60,"blackout_message":"Maintenance","display_border":enabled,"prevent_idle_lock":1,"allow_idle_override":0,"slug":"company","status":"active"});
            let company: Company = serde_json::from_value(row).unwrap();
            assert_eq!(
                serde_json::to_value(company).unwrap()["display_border"],
                enabled == 1
            );
        }
    }
}
