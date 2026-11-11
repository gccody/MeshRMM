//! Deployment tokens: reusable enrollment credentials for installing the
//! Agent on many computers at once, through Group Policy, an MDM or a
//! script. Every device that redeems one joins the token's company, as a
//! device enrolled with a single-use installer for that company does.
//!
//! A token works until it expires, is revoked or reaches its use limit;
//! none of those touch the devices it already enrolled. Only its hash is
//! kept, and the website shows it once. Making and revoking one is
//! website-only, like the other routes that make durable credentials, so a
//! leaked API token can't make itself an enrollment credential.
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Alias, Expr, ExprTrait, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    companies,
    enrollment::{INSTALLER_TTL_MS, InstallerBootstrap},
};
use crate::{
    audit::{self, Target},
    auth::Authorized,
    db::{
        Executor,
        tables::{Companies, DeploymentTokenEnrollments, DeploymentTokens, Users},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    secrets::{new_token, token_hash},
    time::{DAY_MS, now_ms},
    users::new_id,
};

pub const MAX_NAME_LENGTH: usize = 80;
pub const DEFAULT_LIFETIME_DAYS: u32 = 30;
pub const MAX_LIFETIME_DAYS: u32 = 365;
pub const MAX_USES: u32 = 1_000_000;
/// Revoked and expired tokens stay listed this long after they expire.
pub const EXPIRED_RETENTION_MS: i64 = 30 * DAY_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenState {
    Active,
    Expired,
    Revoked,
    Exhausted,
}

#[derive(Debug, Serialize)]
pub struct Creator {
    id: String,
    /// `None` once the user is deleted.
    email: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TokenView {
    id: String,
    name: String,
    company_id: String,
    company_name: String,
    created_by: Creator,
    created_at: i64,
    expires_at: i64,
    /// `None` for no limit.
    max_uses: Option<i64>,
    uses: i64,
    last_used_at: Option<i64>,
    revoked_at: Option<i64>,
    state: TokenState,
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: String,
    name: String,
    company_id: String,
    company_name: String,
    created_by_user_id: String,
    created_by_email: Option<String>,
    created_at: i64,
    expires_at: i64,
    max_uses: Option<i64>,
    uses: i64,
    last_used_at: Option<i64>,
    revoked_at: Option<i64>,
}

impl TokenRow {
    fn view(self, now: i64) -> TokenView {
        let state = state_of(
            self.revoked_at,
            self.expires_at,
            self.max_uses,
            self.uses,
            now,
        );
        TokenView {
            id: self.id,
            name: self.name,
            company_id: self.company_id,
            company_name: self.company_name,
            created_by: Creator {
                id: self.created_by_user_id,
                email: self.created_by_email,
            },
            created_at: self.created_at,
            expires_at: self.expires_at,
            max_uses: self.max_uses,
            uses: self.uses,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
            state,
        }
    }
}

/// Revocation wins over expiry, and expiry over the limit.
fn state_of(
    revoked_at: Option<i64>,
    expires_at: i64,
    max_uses: Option<i64>,
    uses: i64,
    now: i64,
) -> TokenState {
    if revoked_at.is_some() {
        TokenState::Revoked
    } else if expires_at <= now {
        TokenState::Expired
    } else if max_uses.is_some_and(|max| uses >= max) {
        TokenState::Exhausted
    } else {
        TokenState::Active
    }
}

fn token_select() -> sea_query::SelectStatement {
    Query::select()
        .column((DeploymentTokens::Table, DeploymentTokens::Id))
        .column((DeploymentTokens::Table, DeploymentTokens::Name))
        .column((DeploymentTokens::Table, DeploymentTokens::CompanyId))
        .expr_as(
            Expr::col((Companies::Table, Companies::Name)),
            Alias::new("company_name"),
        )
        .column((DeploymentTokens::Table, DeploymentTokens::CreatedByUserId))
        .expr_as(
            Expr::col((Users::Table, Users::Email)),
            Alias::new("created_by_email"),
        )
        .column((DeploymentTokens::Table, DeploymentTokens::CreatedAt))
        .column((DeploymentTokens::Table, DeploymentTokens::ExpiresAt))
        .column((DeploymentTokens::Table, DeploymentTokens::MaxUses))
        .column((DeploymentTokens::Table, DeploymentTokens::Uses))
        .column((DeploymentTokens::Table, DeploymentTokens::LastUsedAt))
        .column((DeploymentTokens::Table, DeploymentTokens::RevokedAt))
        .from(DeploymentTokens::Table)
        .inner_join(
            Companies::Table,
            Expr::col((Companies::Table, Companies::Id))
                .equals((DeploymentTokens::Table, DeploymentTokens::CompanyId)),
        )
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((DeploymentTokens::Table, DeploymentTokens::CreatedByUserId)),
        )
        .to_owned()
}

async fn by_id(executor: &mut impl Executor, id: &str) -> Result<Option<TokenRow>, ApiError> {
    Ok(executor
        .fetch_optional(
            &token_select()
                .and_where(Expr::col((DeploymentTokens::Table, DeploymentTokens::Id)).eq(id))
                .to_owned(),
        )
        .await?)
}

#[derive(Debug, Serialize)]
pub struct TokenList {
    tokens: Vec<TokenView>,
}

/// `GET /v1/deployment-tokens`: every deployment token, newest first,
/// without its secret. API tokens may read it.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<TokenList>, ApiError> {
    actor.require(Permission::DevicesEnroll)?;
    let rows: Vec<TokenRow> = state
        .database
        .fetch_all(
            &token_select()
                .order_by(
                    (DeploymentTokens::Table, DeploymentTokens::CreatedAt),
                    Order::Desc,
                )
                .order_by((DeploymentTokens::Table, DeploymentTokens::Id), Order::Asc)
                .to_owned(),
        )
        .await?;
    let now = now_ms();
    Ok(Json(TokenList {
        tokens: rows.into_iter().map(|row| row.view(now)).collect(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    name: String,
    company_id: String,
    /// Days until it expires; [`DEFAULT_LIFETIME_DAYS`] when missing.
    #[serde(default)]
    expires_in_days: Option<u32>,
    /// How many devices it may enroll; missing or `null` for no limit.
    #[serde(default)]
    max_uses: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct CreatedToken {
    token: TokenView,
    /// What the Agent installers take, with the token as `install_token`.
    /// Shown once; only the token's hash is kept.
    bootstrap: InstallerBootstrap,
}

/// `POST /v1/deployment-tokens`, from the website only.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<NewToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    actor.require_website()?;
    actor.require(Permission::DevicesEnroll)?;
    let name = request.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the token name must be 1 to {MAX_NAME_LENGTH} characters"
        )));
    }
    let lifetime_days = request.expires_in_days.unwrap_or(DEFAULT_LIFETIME_DAYS);
    if !(1..=MAX_LIFETIME_DAYS).contains(&lifetime_days) {
        return Err(ApiError::bad_request(format!(
            "a deployment token lasts 1 to {MAX_LIFETIME_DAYS} days"
        )));
    }
    if request
        .max_uses
        .is_some_and(|uses| !(1..=MAX_USES).contains(&uses))
    {
        return Err(ApiError::bad_request(format!(
            "the use limit must be 1 to {MAX_USES}, or null for none"
        )));
    }
    let id = new_id();
    let token = new_token();
    let now = now_ms();
    let expires_at = now + i64::from(lifetime_days) * DAY_MS;
    let mut transaction = state.database.begin().await?;
    let company_id = companies::ensure_exists(&mut transaction, &request.company_id).await?;
    transaction
        .execute(
            &Query::insert()
                .into_table(DeploymentTokens::Table)
                .columns([
                    DeploymentTokens::Id,
                    DeploymentTokens::Name,
                    DeploymentTokens::TokenHash,
                    DeploymentTokens::CompanyId,
                    DeploymentTokens::CreatedByUserId,
                    DeploymentTokens::CreatedAt,
                    DeploymentTokens::ExpiresAt,
                    DeploymentTokens::MaxUses,
                ])
                .values_panic([
                    id.as_str().into(),
                    name.into(),
                    token_hash(&token).into(),
                    company_id.as_str().into(),
                    actor.user.id.as_str().into(),
                    now.into(),
                    expires_at.into(),
                    request.max_uses.map(i64::from).into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "deployment_token.create",
        Target::deployment_token(&id),
        json!({
            "name": name,
            "company_id": company_id,
            "expires_at": expires_at,
            "max_uses": request.max_uses,
        }),
    )
    .await?;
    let view = by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?
        .view(now);
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token: view,
            bootstrap: InstallerBootstrap {
                server: state.config.public_origin(),
                install_token: token,
                expires_at_unix_ms: expires_at,
            },
        }),
    ))
}

/// `DELETE /v1/deployment-tokens/{id}`, from the website only: the token
/// enrolls nothing more from now on. Devices it enrolled stay, and an
/// install it started can't finish. Revoking it again changes nothing.
pub async fn revoke(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require_website()?;
    actor.require(Permission::DevicesEnroll)?;
    let mut transaction = state.database.begin().await?;
    let token = by_id(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such deployment token"))?;
    let revoked = transaction
        .execute(
            &Query::update()
                .table(DeploymentTokens::Table)
                .value(DeploymentTokens::RevokedAt, now_ms())
                .and_where(Expr::col(DeploymentTokens::Id).eq(id.as_str()))
                .and_where(Expr::col(DeploymentTokens::RevokedAt).is_null())
                .to_owned(),
        )
        .await?;
    if revoked == 1 {
        audit::record(
            &mut transaction,
            &actor.actor(),
            "deployment_token.revoke",
            Target::deployment_token(&id),
            json!({
                "name": token.name,
                "company_id": token.company_id,
                "uses": token.uses,
            }),
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What a redemption of a deployment token claimed.
pub(super) struct Claim {
    pub token_id: String,
    pub token_name: String,
    pub created_by_user_id: String,
    pub company_id: String,
    pub device_id: String,
}

#[derive(sqlx::FromRow)]
struct LiveToken {
    id: String,
    name: String,
    created_by_user_id: String,
    company_id: String,
}

/// Claims a new device for the computer from the live deployment token
/// whose hash is `token_hash`, taking one of its uses, or finds the device
/// this computer claimed earlier with the same name and redemption key,
/// within [`INSTALLER_TTL_MS`] of then, without taking another. `None` when
/// the token is unknown, expired, revoked or used up, or the key was used
/// by another computer.
///
/// The use is taken by a conditional update in the caller's transaction:
/// concurrent redemptions wait for each other's update, so they can't take
/// more uses than the limit, and a redemption that fails later rolls its
/// use back with everything else.
pub(super) async fn claim(
    executor: &mut impl Executor,
    token_hash: &str,
    key_hash: &str,
    name: &str,
    now: i64,
) -> Result<Option<Claim>, ApiError> {
    let Some(token): Option<LiveToken> = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    DeploymentTokens::Id,
                    DeploymentTokens::Name,
                    DeploymentTokens::CreatedByUserId,
                    DeploymentTokens::CompanyId,
                ])
                .from(DeploymentTokens::Table)
                .and_where(Expr::col(DeploymentTokens::TokenHash).eq(token_hash))
                .and_where(Expr::col(DeploymentTokens::RevokedAt).is_null())
                .and_where(Expr::col(DeploymentTokens::ExpiresAt).gt(now))
                .to_owned(),
        )
        .await?
    else {
        return Ok(None);
    };
    let device_id = new_id();
    // A computer retrying with its key finds its row taken, by itself.
    let inserted = executor
        .execute(
            &Query::insert()
                .into_table(DeploymentTokenEnrollments::Table)
                .columns([
                    DeploymentTokenEnrollments::TokenId,
                    DeploymentTokenEnrollments::RedemptionKeyHash,
                    DeploymentTokenEnrollments::ComputerName,
                    DeploymentTokenEnrollments::DeviceId,
                    DeploymentTokenEnrollments::CreatedAt,
                    DeploymentTokenEnrollments::RecoverableUntil,
                ])
                .values_panic([
                    token.id.as_str().into(),
                    key_hash.into(),
                    name.into(),
                    device_id.as_str().into(),
                    now.into(),
                    (now + INSTALLER_TTL_MS).into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        DeploymentTokenEnrollments::TokenId,
                        DeploymentTokenEnrollments::RedemptionKeyHash,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    let device_id = if inserted == 1 {
        let taken = executor
            .execute(
                &Query::update()
                    .table(DeploymentTokens::Table)
                    .value(
                        DeploymentTokens::Uses,
                        Expr::col(DeploymentTokens::Uses).add(1),
                    )
                    .value(DeploymentTokens::LastUsedAt, now)
                    .and_where(Expr::col(DeploymentTokens::Id).eq(token.id.as_str()))
                    // Checked again here: it may have been revoked meanwhile.
                    .and_where(Expr::col(DeploymentTokens::RevokedAt).is_null())
                    .and_where(Expr::col(DeploymentTokens::ExpiresAt).gt(now))
                    .and_where(Expr::col(DeploymentTokens::MaxUses).is_null().or(
                        Expr::col(DeploymentTokens::Uses).lt(Expr::col(DeploymentTokens::MaxUses)),
                    ))
                    .to_owned(),
            )
            .await?;
        if taken != 1 {
            // The caller rolls the enrollment row back with the transaction.
            return Ok(None);
        }
        device_id
    } else {
        let earlier: Option<(String,)> = executor
            .fetch_optional(
                &Query::select()
                    .column(DeploymentTokenEnrollments::DeviceId)
                    .from(DeploymentTokenEnrollments::Table)
                    .and_where(Expr::col(DeploymentTokenEnrollments::TokenId).eq(token.id.as_str()))
                    .and_where(
                        Expr::col(DeploymentTokenEnrollments::RedemptionKeyHash).eq(key_hash),
                    )
                    .and_where(Expr::col(DeploymentTokenEnrollments::ComputerName).eq(name))
                    .and_where(Expr::col(DeploymentTokenEnrollments::RecoverableUntil).gt(now))
                    .to_owned(),
            )
            .await?;
        let Some((device_id,)) = earlier else {
            return Ok(None);
        };
        device_id
    };
    Ok(Some(Claim {
        token_id: token.id,
        token_name: token.name,
        created_by_user_id: token.created_by_user_id,
        company_id: token.company_id,
        device_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_follow_revocation_then_expiry_then_the_limit() {
        let now = 1_000;
        assert_eq!(state_of(None, 2_000, None, 50, now), TokenState::Active);
        assert_eq!(state_of(None, 2_000, Some(3), 2, now), TokenState::Active);
        assert_eq!(
            state_of(None, 2_000, Some(3), 3, now),
            TokenState::Exhausted
        );
        assert_eq!(state_of(None, 1_000, Some(3), 3, now), TokenState::Expired);
        assert_eq!(
            state_of(Some(5), 1_000, Some(3), 3, now),
            TokenState::Revoked
        );
        assert_eq!(state_of(Some(5), 2_000, None, 0, now), TokenState::Revoked);
    }
}
