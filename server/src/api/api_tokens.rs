//! Personal API tokens: users make and revoke their own on the website, and
//! holders of `api_tokens.manage` see and revoke everyone's.
use axum::{
    Json,
    extract::{Path, Query as QueryParams, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{double_option, ensure_within};
use crate::{
    api_tokens::{
        self, MAX_LIFETIME_DAYS, MAX_NAME_LENGTH, MAX_TOKENS_PER_USER, PermissionChoice, Token,
    },
    audit::{self, Target},
    auth::Authorized,
    db::tables::ApiTokens,
    http::{ApiError, AppState, JsonBody},
    rbac::{Permission, Permissions},
    secrets::token_hash,
    settings::{self, Settings},
    time::{DAY_MS, now_ms},
    users::new_id,
};

/// What the server allows, so the website offers only lifetimes it accepts.
#[derive(Debug, Serialize)]
pub struct Policy {
    enabled: bool,
    /// `None`: no limit, and tokens may never expire.
    max_lifetime_days: Option<i64>,
}

impl From<&Settings> for Policy {
    fn from(settings: &Settings) -> Self {
        Self {
            enabled: settings.api_tokens_enabled,
            max_lifetime_days: settings.api_token_max_lifetime_days,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TokenList {
    tokens: Vec<Token>,
    policy: Policy,
    /// The permissions the caller may limit a new token to: their own.
    permissions: Vec<PermissionChoice>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Every user's tokens, for `api_tokens.manage`.
    #[serde(default)]
    all: bool,
    /// One user's tokens: the caller's own, or anyone's with
    /// `api_tokens.manage`.
    #[serde(default)]
    user_id: Option<String>,
}

/// `GET /v1/api-tokens`: the caller's tokens, or with `all=true` or
/// `user_id`, other users' too.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParams(query): QueryParams<ListQuery>,
) -> Result<Json<TokenList>, ApiError> {
    let user_id = match (query.all, query.user_id) {
        (true, Some(_)) => {
            return Err(ApiError::bad_request("pass all or user_id, not both"));
        }
        (true, None) => None,
        (false, user_id) => Some(user_id.unwrap_or_else(|| actor.user.id.clone())),
    };
    if user_id.as_deref() != Some(actor.user.id.as_str()) {
        actor.require(Permission::ApiTokensManage)?;
    }
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    Ok(Json(TokenList {
        tokens: api_tokens::list(&mut database, user_id.as_deref()).await?,
        policy: Policy::from(&settings),
        permissions: api_tokens::choices(&actor.permissions),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    name: String,
    /// Days until it expires; `null` for never, if the server allows it.
    /// Required, so a token never lasts forever by omission.
    #[serde(default, deserialize_with = "double_option")]
    expires_in_days: Option<Option<u32>>,
    /// The permissions to limit it to; missing or `null` for all of the
    /// user's.
    #[serde(default)]
    permissions: Option<Permissions>,
}

#[derive(Debug, Serialize)]
pub struct CreatedToken {
    #[serde(flatten)]
    token: Token,
    /// The bearer token. Shown once; only its hash is kept.
    secret: String,
}

/// `POST /v1/api-tokens`: makes a token for the signed-in user. Only the
/// website can, so a leaked token can't make more of itself.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<NewToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    actor.require_website()?;
    let name = request.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the token name must be 1 to {MAX_NAME_LENGTH} characters"
        )));
    }
    let Some(lifetime_days) = request.expires_in_days else {
        return Err(ApiError::bad_request(
            "set expires_in_days, or null for a token that never expires",
        ));
    };
    if let Some(permissions) = &request.permissions {
        if permissions.is_empty() {
            return Err(ApiError::bad_request(
                "choose at least one permission, or null for all of yours",
            ));
        }
        ensure_within(&actor, permissions, "limit a token to these permissions")?;
    }
    let mut transaction = state.database.begin().await?;
    let settings = settings::load(&mut transaction).await?;
    if !settings.api_tokens_enabled {
        return Err(
            ApiError::forbidden("API tokens are turned off on this server")
                .with_code("api_tokens_disabled"),
        );
    }
    check_lifetime(lifetime_days, settings.api_token_max_lifetime_days)?;
    if api_tokens::list(&mut transaction, Some(&actor.user.id))
        .await?
        .len()
        >= MAX_TOKENS_PER_USER
    {
        return Err(ApiError::conflict(format!(
            "you can have at most {MAX_TOKENS_PER_USER} API tokens; revoke one first"
        ))
        .with_code("too_many_tokens"));
    }
    let secret = api_tokens::new_secret();
    let id = new_id();
    let now = now_ms();
    let expires_at = lifetime_days.map(|days| now + i64::from(days) * DAY_MS);
    transaction
        .execute(
            &Query::insert()
                .into_table(ApiTokens::Table)
                .columns([
                    ApiTokens::Id,
                    ApiTokens::UserId,
                    ApiTokens::Name,
                    ApiTokens::TokenHash,
                    ApiTokens::PermissionsJson,
                    ApiTokens::CreatedAt,
                    ApiTokens::ExpiresAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    actor.user.id.as_str().into(),
                    name.into(),
                    token_hash(&secret).into(),
                    request
                        .permissions
                        .as_ref()
                        .map(api_tokens::scope_json)
                        .into(),
                    now.into(),
                    expires_at.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "api_token.create",
        Target::api_token(&id),
        json!({
            "name": name,
            "permissions": request.permissions,
            "expires_at": expires_at,
        }),
    )
    .await?;
    let token = api_tokens::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(CreatedToken { token, secret })))
}

fn check_lifetime(days: Option<u32>, max_days: Option<i64>) -> Result<(), ApiError> {
    match (days, max_days) {
        (Some(days), _) if days == 0 || days > MAX_LIFETIME_DAYS => Err(ApiError::bad_request(
            format!("a token lasts 1 to {MAX_LIFETIME_DAYS} days"),
        )),
        (Some(days), Some(max)) if i64::from(days) > max => Err(ApiError::bad_request(format!(
            "this server allows tokens of at most {max} days"
        ))
        .with_code("lifetime_too_long")),
        (None, Some(max)) => Err(ApiError::bad_request(format!(
            "this server requires tokens to expire within {max} days"
        ))
        .with_code("lifetime_too_long")),
        _ => Ok(()),
    }
}

/// `DELETE /v1/api-tokens/{id}`: revokes one of the caller's tokens, or
/// anyone's with `api_tokens.manage`. It stops working at once.
pub async fn revoke(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let token = api_tokens::by_id(&mut transaction, &id)
        .await?
        .filter(|token| token.user.id == actor.user.id || actor.has(Permission::ApiTokensManage))
        .ok_or_else(|| ApiError::not_found("no such token"))?;
    transaction
        .execute(
            &Query::delete()
                .from_table(ApiTokens::Table)
                .and_where(Expr::col(ApiTokens::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "api_token.revoke",
        Target::api_token(&id),
        json!({
            "name": token.name,
            "user_id": token.user.id,
            "user_email": token.user.email,
        }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct Current {
    token: Token,
    /// What the token may do now: its scope and the user's roles together.
    permissions: Permissions,
}

/// `GET /v1/api-tokens/current`: the token the request carries and what it
/// may do, for checking an integration's setup.
pub async fn current(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Current>, ApiError> {
    let Some(reference) = &actor.api_token else {
        return Err(
            ApiError::bad_request("this request didn't use an API token")
                .with_code("not_an_api_token"),
        );
    };
    let token = api_tokens::by_id(&mut &state.database, &reference.id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such token"))?;
    Ok(Json(Current {
        token,
        permissions: actor.permissions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes_follow_the_cap() {
        assert!(check_lifetime(Some(30), Some(90)).is_ok());
        assert!(check_lifetime(Some(90), Some(90)).is_ok());
        assert!(check_lifetime(Some(91), Some(90)).is_err());
        assert!(check_lifetime(None, Some(90)).is_err());
        assert!(check_lifetime(None, None).is_ok());
        assert!(check_lifetime(Some(0), None).is_err());
        assert!(check_lifetime(Some(MAX_LIFETIME_DAYS + 1), None).is_err());
    }
}
