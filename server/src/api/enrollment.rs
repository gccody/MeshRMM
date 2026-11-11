//! Enrollment: a user issues a short-lived Agent installer, and the Agent it
//! installs redeems it for the device's credential.
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use meshrmm_protocol_types::DevicePlatform;
use sea_query::{Expr, ExprTrait, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{companies, deployment_tokens, limit_token_attempts};
use crate::{
    agent_rollout,
    agents::{self, bearer_token, enrollment_token, hashes_match},
    audit::{self, Actor, Target},
    auth::{Authorized, limits::ip_key},
    db::{
        Executor,
        tables::{AgentInstallTokens, Agents},
    },
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    rbac::Permission,
    secrets::{new_token, token_hash},
    time::{MINUTE_MS, now_ms},
    users::new_id,
};

/// An installer works this long after it is issued, and an interrupted
/// install can redeem it again until then.
pub const INSTALLER_TTL_MS: i64 = 30 * MINUTE_MS;
const PLATFORMS: &[&str] = &["windows-x64", "windows-arm64", "macos", "linux"];
const MIN_REDEMPTION_KEY_LENGTH: usize = 32;
const MAX_REDEMPTION_KEY_LENGTH: usize = 128;
/// The capture settings a new Agent starts with.
const FRAMES_PER_SECOND: u32 = 60;
const BITRATE_BITS_PER_SECOND: u32 = 12_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallerRequest {
    platform: String,
    /// The company the enrolled device joins, if any.
    #[serde(default)]
    company_id: Option<String>,
}

/// What the website embeds in the installer it downloads.
#[derive(Debug, Serialize)]
pub struct InstallerBootstrap {
    pub(super) server: String,
    pub(super) install_token: String,
    pub(super) expires_at_unix_ms: i64,
}

/// `POST /v1/agent-installers`: issues an installer token for one device.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<InstallerRequest>,
) -> Result<(StatusCode, Json<InstallerBootstrap>), ApiError> {
    actor.require(Permission::DevicesEnroll)?;
    if !PLATFORMS.contains(&request.platform.as_str()) {
        return Err(ApiError::bad_request(format!(
            "platform must be one of: {}",
            PLATFORMS.join(", ")
        )));
    }
    let id = new_id();
    let token = new_token();
    let now = now_ms();
    let expires_at = now + INSTALLER_TTL_MS;
    let mut transaction = state.database.begin().await?;
    let company_id = match &request.company_id {
        Some(company_id) => Some(companies::ensure_exists(&mut transaction, company_id).await?),
        None => None,
    };
    // Someone limited to some companies enrolls only into them: a device in
    // no company is out of their reach.
    match &company_id {
        Some(company_id) => actor.require_company(Permission::DevicesEnroll, company_id)?,
        None if !actor.everywhere(Permission::DevicesEnroll) => {
            return Err(
                ApiError::bad_request("choose one of your companies for the device")
                    .with_code("company_required"),
            );
        }
        None => {}
    }
    transaction
        .execute(
            &Query::insert()
                .into_table(AgentInstallTokens::Table)
                .columns([
                    AgentInstallTokens::Id,
                    AgentInstallTokens::TokenHash,
                    AgentInstallTokens::CreatedByUserId,
                    AgentInstallTokens::Platform,
                    AgentInstallTokens::CompanyId,
                    AgentInstallTokens::CreatedAt,
                    AgentInstallTokens::ExpiresAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    token_hash(&token).into(),
                    actor.user.id.as_str().into(),
                    request.platform.as_str().into(),
                    company_id.clone().into(),
                    now.into(),
                    expires_at.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "agent_installer.issue",
        Target::agent_installer(&id),
        json!({ "platform": request.platform, "company_id": company_id }),
    )
    .await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(InstallerBootstrap {
            server: state.config.public_origin(),
            install_token: token,
            expires_at_unix_ms: expires_at,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RedeemRequest {
    /// The computer's name.
    name: String,
    /// A random secret the endpoint generated and keeps, so only it can
    /// redeem the installer again after an interrupted install.
    redemption_key: String,
    /// The operating system the Agent runs on.
    platform: DevicePlatform,
}

/// The Agent's configuration file.
#[derive(Debug, Serialize)]
pub struct AgentConfig {
    server: String,
    device_id: String,
    agent_token: String,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    json_logs: bool,
}

#[derive(sqlx::FromRow)]
struct ClaimRow {
    id: String,
    created_by_user_id: String,
    device_id: String,
    company_id: Option<String>,
}

/// The device a redemption enrolls or recovers, and what it redeemed.
struct Claim {
    device_id: String,
    created_by_user_id: String,
    company_id: Option<String>,
    source: Source,
}

enum Source {
    Installer { id: String },
    DeploymentToken { id: String, name: String },
}

impl From<ClaimRow> for Claim {
    fn from(row: ClaimRow) -> Self {
        Self {
            device_id: row.device_id,
            created_by_user_id: row.created_by_user_id,
            company_id: row.company_id,
            source: Source::Installer { id: row.id },
        }
    }
}

impl From<deployment_tokens::Claim> for Claim {
    fn from(claim: deployment_tokens::Claim) -> Self {
        Self {
            device_id: claim.device_id,
            created_by_user_id: claim.created_by_user_id,
            company_id: Some(claim.company_id),
            source: Source::DeploymentToken {
                id: claim.token_id,
                name: claim.token_name,
            },
        }
    }
}

#[derive(sqlx::FromRow)]
struct ExistingAgent {
    auth_token_hash: String,
    deletion_requested_at: Option<i64>,
}

fn rejected() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "the Agent installer is invalid, expired, or claimed by another computer",
    )
    .with_code("installer_rejected")
}

/// `POST /v1/agent-installers/redeem`, from the Agent installer with the
/// installer token as its bearer token. The first redemption claims the
/// installer for the computer and enrolls a device; redeeming again with the
/// same name and redemption key, until the installer expires, returns the
/// same credential.
///
/// The bearer token may instead be a deployment token, which enrolls a
/// device into its company for each computer that redeems it; see
/// [`deployment_tokens::claim`]. Every refusal, whichever token it was and
/// whyever, gets the same answer.
pub async fn redeem(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(request): JsonBody<RedeemRequest>,
) -> Result<Json<AgentConfig>, ApiError> {
    limit_token_attempts(&state, ip)?;
    let install_token = bearer_token(&headers).ok_or_else(rejected)?.to_owned();
    let name = agents::normalize_name(&request.name).ok_or_else(|| {
        ApiError::bad_request(format!(
            "the computer name must be 1 to {} characters on one line",
            agents::MAX_NAME_CHARS
        ))
    })?;
    if !(MIN_REDEMPTION_KEY_LENGTH..=MAX_REDEMPTION_KEY_LENGTH)
        .contains(&request.redemption_key.len())
    {
        return Err(ApiError::bad_request(
            "the installer's redemption key is missing or invalid; download a new installer",
        ));
    }
    let installer_hash = token_hash(&install_token);
    let key_hash = token_hash(&request.redemption_key);
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    let claim: Claim =
        match claim_installer(&mut transaction, &installer_hash, &key_hash, &name, now).await? {
            Some(claim) => claim.into(),
            None => {
                deployment_tokens::claim(&mut transaction, &installer_hash, &key_hash, &name, now)
                    .await?
                    .ok_or_else(rejected)?
                    .into()
            }
        };
    let agent_token = enrollment_token(&install_token, &request.redemption_key);
    let agent_token_hash = token_hash(&agent_token);
    let recovered = enroll_device(
        &mut transaction,
        &claim,
        &name,
        request.platform,
        &agent_token_hash,
        now,
    )
    .await?;
    let (label, action, mut metadata) = match &claim.source {
        Source::Installer { id } => (
            "Agent installer",
            "agent_installer.redeem",
            json!({ "installer_id": id }),
        ),
        Source::DeploymentToken { id, name } => (
            "Deployment token",
            "deployment_token.redeem",
            json!({
                "deployment_token_id": id,
                "deployment_token_name": name,
                "company_id": claim.company_id,
            }),
        ),
    };
    metadata["issued_by_user_id"] = json!(claim.created_by_user_id);
    metadata["name"] = json!(name);
    metadata["platform"] = json!(request.platform);
    metadata["recovered"] = json!(recovered);
    audit::record(
        &mut transaction,
        &Actor {
            user_id: None,
            label: label.to_owned(),
            ip: Some(ip),
            api_token: None,
        },
        action,
        Target::device(&claim.device_id),
        metadata,
    )
    .await?;
    transaction.commit().await?;
    state.auth.token_attempts.refund(&ip_key(ip));
    tracing::info!(device_id = claim.device_id, recovered, "Agent enrolled");
    state.presence.refresh(&claim.device_id).await;
    if !recovered {
        crate::automation::device_enrolled(&state, &claim.device_id).await;
    }
    Ok(Json(AgentConfig {
        server: state.config.public_origin(),
        device_id: claim.device_id,
        agent_token,
        frames_per_second: FRAMES_PER_SECOND,
        bitrate_bits_per_second: BITRATE_BITS_PER_SECOND,
        json_logs: false,
    }))
}

/// Claims the installer for this computer, or finds the claim this computer
/// made earlier. `None` when no live installer has this hash and key.
async fn claim_installer(
    executor: &mut impl Executor,
    installer_hash: &str,
    key_hash: &str,
    name: &str,
    now: i64,
) -> Result<Option<ClaimRow>, ApiError> {
    executor
        .execute(
            &Query::update()
                .table(AgentInstallTokens::Table)
                .value(AgentInstallTokens::UsedAt, now)
                .value(AgentInstallTokens::DeviceId, new_id())
                .value(AgentInstallTokens::ComputerName, name)
                .value(AgentInstallTokens::RedemptionKeyHash, key_hash)
                .and_where(Expr::col(AgentInstallTokens::TokenHash).eq(installer_hash))
                .and_where(Expr::col(AgentInstallTokens::ExpiresAt).gt(now))
                .and_where(Expr::col(AgentInstallTokens::UsedAt).is_null())
                .to_owned(),
        )
        .await?;
    // Claimed by this request just now, or earlier by this computer.
    Ok(executor
        .fetch_optional(
            &Query::select()
                .columns([
                    AgentInstallTokens::Id,
                    AgentInstallTokens::CreatedByUserId,
                    AgentInstallTokens::DeviceId,
                    AgentInstallTokens::CompanyId,
                ])
                .from(AgentInstallTokens::Table)
                .and_where(Expr::col(AgentInstallTokens::TokenHash).eq(installer_hash))
                .and_where(Expr::col(AgentInstallTokens::ExpiresAt).gt(now))
                .and_where(Expr::col(AgentInstallTokens::RedemptionKeyHash).eq(key_hash))
                .and_where(Expr::col(AgentInstallTokens::ComputerName).eq(name))
                .to_owned(),
        )
        .await?)
}

/// Adds the claimed device, or confirms the installer's credential still
/// belongs to it. Returns whether the device already existed.
async fn enroll_device(
    executor: &mut impl Executor,
    claim: &Claim,
    name: &str,
    platform: DevicePlatform,
    agent_token_hash: &str,
    now: i64,
) -> Result<bool, ApiError> {
    let existing: Option<ExistingAgent> = executor
        .fetch_optional(
            &Query::select()
                .columns([Agents::AuthTokenHash, Agents::DeletionRequestedAt])
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(claim.device_id.as_str()))
                .to_owned(),
        )
        .await?;
    let recovered = match existing {
        None => {
            executor
                .execute(
                    &Query::insert()
                        .into_table(Agents::Table)
                        .columns([
                            Agents::Id,
                            Agents::Name,
                            Agents::Platform,
                            Agents::AuthTokenHash,
                            Agents::CreatedByUserId,
                            Agents::CompanyId,
                            Agents::CreatedAt,
                            Agents::UpdatedAt,
                            Agents::RolloutBucket,
                        ])
                        .values_panic([
                            claim.device_id.as_str().into(),
                            name.into(),
                            platform.as_str().into(),
                            agent_token_hash.into(),
                            claim.created_by_user_id.as_str().into(),
                            claim.company_id.clone().into(),
                            now.into(),
                            now.into(),
                            i64::from(agent_rollout::bucket(&claim.device_id)).into(),
                        ])
                        .to_owned(),
                )
                .await?;
            false
        }
        // The credential was rotated or the device deleted since, so the
        // installer's credential no longer works.
        Some(agent)
            if agent.deletion_requested_at.is_none()
                && hashes_match(&agent.auth_token_hash, agent_token_hash) =>
        {
            true
        }
        Some(_) => return Err(rejected()),
    };
    Ok(recovered)
}
