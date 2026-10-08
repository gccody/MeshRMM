//! The administrator's side of SCIM: tokens for the identity provider, and
//! which role each provider group grants.
//!
//! A SCIM token can disable any account and put anyone in a group that
//! grants the Administrator role, so only administrators manage SCIM.
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Func, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{double_option, require_administrator};
use crate::{
    audit::{self, Target},
    auth::Authorized,
    db::{
        Executor,
        tables::{ScimGroupMembers, ScimGroups, ScimTokens},
    },
    http::{ApiError, AppState, JsonBody},
    rbac, scim,
    secrets::{new_token, token_hash},
    time::now_ms,
    users::new_id,
};

const MAX_TOKEN_NAME_LENGTH: usize = 80;
const MAX_TOKENS: i64 = 20;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TokenView {
    id: String,
    name: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GroupView {
    id: String,
    display_name: String,
    external_id: Option<String>,
    role_id: Option<String>,
    member_count: i64,
}

#[derive(Debug, Serialize)]
pub struct ScimSettings {
    /// The SCIM base URL to give the identity provider.
    base_url: String,
    tokens: Vec<TokenView>,
    groups: Vec<GroupView>,
}

async fn tokens(executor: &mut impl Executor) -> crate::db::Result<Vec<TokenView>> {
    executor
        .fetch_all(
            &Query::select()
                .columns([
                    ScimTokens::Id,
                    ScimTokens::Name,
                    ScimTokens::CreatedAt,
                    ScimTokens::LastUsedAt,
                ])
                .from(ScimTokens::Table)
                .and_where(Expr::col(ScimTokens::RevokedAt).is_null())
                .order_by(ScimTokens::CreatedAt, Order::Asc)
                .to_owned(),
        )
        .await
}

async fn groups(executor: &mut impl Executor) -> crate::db::Result<Vec<GroupView>> {
    executor
        .fetch_all(
            &Query::select()
                .column((ScimGroups::Table, ScimGroups::Id))
                .column((ScimGroups::Table, ScimGroups::DisplayName))
                .column((ScimGroups::Table, ScimGroups::ExternalId))
                .column((ScimGroups::Table, ScimGroups::RoleId))
                .expr_as(
                    Func::count(Expr::col((
                        ScimGroupMembers::Table,
                        ScimGroupMembers::UserId,
                    ))),
                    "member_count",
                )
                .from(ScimGroups::Table)
                .left_join(
                    ScimGroupMembers::Table,
                    Expr::col((ScimGroupMembers::Table, ScimGroupMembers::GroupId))
                        .equals((ScimGroups::Table, ScimGroups::Id)),
                )
                .group_by_columns([
                    (ScimGroups::Table, ScimGroups::Id),
                    (ScimGroups::Table, ScimGroups::DisplayName),
                    (ScimGroups::Table, ScimGroups::ExternalId),
                    (ScimGroups::Table, ScimGroups::RoleId),
                ])
                .order_by((ScimGroups::Table, ScimGroups::DisplayName), Order::Asc)
                .to_owned(),
        )
        .await
}

/// `GET /v1/settings/scim`
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<ScimSettings>, ApiError> {
    require_administrator(&actor)?;
    let mut database = &state.database;
    Ok(Json(ScimSettings {
        base_url: scim::base_url(&state),
        tokens: tokens(&mut database).await?,
        groups: groups(&mut database).await?,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    name: String,
}

#[derive(Debug, Serialize)]
pub struct CreatedToken {
    #[serde(flatten)]
    token: TokenView,
    /// The bearer token. Shown once; only its hash is kept.
    secret: String,
}

/// `POST /v1/settings/scim/tokens`
pub async fn create_token(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<NewToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    require_administrator(&actor)?;
    let name = request.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_TOKEN_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(
            "the token name must be 1 to 80 characters",
        ));
    }
    let mut transaction = state.database.begin().await?;
    if tokens(&mut transaction).await?.len() as i64 >= MAX_TOKENS {
        return Err(ApiError::conflict(format!(
            "there can be at most {MAX_TOKENS} SCIM tokens; revoke one first"
        )));
    }
    let secret = new_token();
    let token = TokenView {
        id: new_id(),
        name: name.to_owned(),
        created_at: now_ms(),
        last_used_at: None,
    };
    transaction
        .execute(
            &Query::insert()
                .into_table(ScimTokens::Table)
                .columns([
                    ScimTokens::Id,
                    ScimTokens::Name,
                    ScimTokens::TokenHash,
                    ScimTokens::CreatedByUserId,
                    ScimTokens::CreatedAt,
                ])
                .values_panic([
                    token.id.as_str().into(),
                    name.into(),
                    token_hash(&secret).into(),
                    actor.user.id.as_str().into(),
                    token.created_at.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "scim.token_create",
        Target::scim_token(&token.id),
        json!({ "name": name }),
    )
    .await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedToken { token, secret })))
}

/// `DELETE /v1/settings/scim/tokens/{id}`: revokes a token at once.
pub async fn revoke_token(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_administrator(&actor)?;
    let mut transaction = state.database.begin().await?;
    let revoked = transaction
        .execute(
            &Query::update()
                .table(ScimTokens::Table)
                .value(ScimTokens::RevokedAt, now_ms())
                .and_where(Expr::col(ScimTokens::Id).eq(id.as_str()))
                .and_where(Expr::col(ScimTokens::RevokedAt).is_null())
                .to_owned(),
        )
        .await?;
    if revoked == 0 {
        return Err(ApiError::not_found("no such token"));
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "scim.token_revoke",
        Target::scim_token(&id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupUpdate {
    /// `null` stops the group granting a role.
    #[serde(default, deserialize_with = "double_option")]
    role_id: Option<Option<String>>,
}

/// `PATCH /v1/settings/scim/groups/{id}`: sets the role the group grants.
pub async fn update_group(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<GroupUpdate>,
) -> Result<Json<GroupView>, ApiError> {
    require_administrator(&actor)?;
    let Some(role_id) = request.role_id else {
        return Err(ApiError::bad_request("set role_id, or null for no role"));
    };
    let mut transaction = state.database.begin().await?;
    if let Some(role_id) = &role_id
        && rbac::load_roles(&mut transaction, Some(std::slice::from_ref(role_id)))
            .await?
            .is_empty()
    {
        return Err(ApiError::bad_request("the role doesn't exist"));
    }
    let updated = transaction
        .execute(
            &Query::update()
                .table(ScimGroups::Table)
                .value(ScimGroups::RoleId, role_id.clone())
                .and_where(Expr::col(ScimGroups::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    if updated == 0 {
        return Err(ApiError::not_found("no such group"));
    }
    super::ensure_administrator_remains(&mut transaction).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "scim.group_role_update",
        Target::scim_group(&id),
        json!({ "role_id": role_id }),
    )
    .await?;
    let view = groups(&mut transaction)
        .await?
        .into_iter()
        .find(|group| group.id == id)
        .ok_or_else(|| ApiError::not_found("no such group"))?;
    transaction.commit().await?;
    state.presence.recheck_access();
    Ok(Json(view))
}
