//! Personal API tokens: bearer credentials a user makes on the website so
//! scripts and integrations can call the `/v1` API without a browser.
//!
//! A token acts as its user. What it may do is what its scope allows *and*
//! what the user's roles grant now, so a role change applies at once, and a
//! disabled or deleted user's tokens stop working. Only the SHA-256 of a
//! token is stored, as for sessions and Agent credentials: the secret is 256
//! random bits, so a fast hash is enough.
use std::net::IpAddr;

use axum::http::{HeaderMap, StatusCode, header};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::Serialize;

use crate::{
    audit::TokenRef,
    auth::{Authorized, limits::ip_key},
    db::{
        self, Executor,
        tables::{ApiTokens, Users},
    },
    http::{ApiError, AppState},
    rbac::{self, Permission, Permissions},
    secrets::{new_token, token_hash},
    settings,
    time::{DAY_MS, MINUTE_MS, now_ms},
    users,
};

/// Every token starts with this, so secret scanners can recognize one.
pub const PREFIX: &str = "mrmm_pat_";
pub const MAX_NAME_LENGTH: usize = 80;
pub const MAX_TOKENS_PER_USER: usize = 50;
/// The longest lifetime anyone may choose, and the most the cap may be.
pub const MAX_LIFETIME_DAYS: u32 = 3650;
/// Expired tokens stay listed this long, so their users see why a script
/// stopped working, and then maintenance removes them.
pub const EXPIRED_RETENTION_MS: i64 = 30 * DAY_MS;
/// `last_used_at` and `last_used_ip` are written at most this often.
const TOUCH_INTERVAL_MS: i64 = MINUTE_MS;

/// A new token: the prefix and 64 hex digits.
pub fn new_secret() -> String {
    format!("{PREFIX}{}", new_token())
}

/// The personal API token in the request's `Authorization: Bearer` header.
/// Agents, viewers and SCIM clients send bearer credentials too; only one
/// with the prefix is an API token.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .filter(|token| token.starts_with(PREFIX))
}

/// A token as its user and administrators see it. The secret is never
/// part of it.
#[derive(Debug, Clone, Serialize)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub user: TokenUser,
    /// The permissions the token is limited to; `None` for all of its
    /// user's. Either way it never has more than the user holds.
    pub permissions: Option<Permissions>,
    pub created_at: i64,
    /// `None` never expires.
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub last_used_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUser {
    pub id: String,
    pub email: String,
    pub display_name: String,
}

impl Token {
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }

    pub fn reference(&self) -> TokenRef {
        TokenRef {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    name: String,
    user_id: String,
    email: String,
    display_name: String,
    permissions_json: Option<String>,
    created_at: i64,
    expires_at: Option<i64>,
    last_used_at: Option<i64>,
    last_used_ip: Option<String>,
}

impl From<Row> for Token {
    fn from(row: Row) -> Self {
        Self {
            id: row.id,
            name: row.name,
            user: TokenUser {
                id: row.user_id,
                email: row.email,
                display_name: row.display_name,
            },
            permissions: row.permissions_json.as_deref().map(parse_scope),
            created_at: row.created_at,
            expires_at: row.expires_at,
            last_used_at: row.last_used_at,
            last_used_ip: row.last_used_ip,
        }
    }
}

/// A stored scope. A permission a later release removed grants nothing.
fn parse_scope(json: &str) -> Permissions {
    serde_json::from_str::<Vec<String>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|name| name.parse().ok())
        .collect()
}

pub fn scope_json(permissions: &Permissions) -> String {
    serde_json::to_string(permissions).expect("permission names serialize")
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .column((ApiTokens::Table, ApiTokens::Id))
        .column((ApiTokens::Table, ApiTokens::Name))
        .column((ApiTokens::Table, ApiTokens::UserId))
        .column((Users::Table, Users::Email))
        .column((Users::Table, Users::DisplayName))
        .column((ApiTokens::Table, ApiTokens::PermissionsJson))
        .column((ApiTokens::Table, ApiTokens::CreatedAt))
        .column((ApiTokens::Table, ApiTokens::ExpiresAt))
        .column((ApiTokens::Table, ApiTokens::LastUsedAt))
        .column((ApiTokens::Table, ApiTokens::LastUsedIp))
        .from(ApiTokens::Table)
        .inner_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id)).equals((ApiTokens::Table, ApiTokens::UserId)),
        )
        .to_owned()
}

/// Tokens newest first: one user's, or everyone's.
pub async fn list(executor: &mut impl Executor, user_id: Option<&str>) -> db::Result<Vec<Token>> {
    let mut select = select();
    select
        .order_by((ApiTokens::Table, ApiTokens::CreatedAt), Order::Desc)
        .order_by((ApiTokens::Table, ApiTokens::Id), Order::Desc);
    if let Some(user_id) = user_id {
        select.and_where(Expr::col((ApiTokens::Table, ApiTokens::UserId)).eq(user_id));
    }
    let rows: Vec<Row> = executor.fetch_all(&select).await?;
    Ok(rows.into_iter().map(Token::from).collect())
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<Token>> {
    let row: Option<Row> = executor
        .fetch_optional(
            &select()
                .and_where(Expr::col((ApiTokens::Table, ApiTokens::Id)).eq(id))
                .to_owned(),
        )
        .await?;
    Ok(row.map(Token::from))
}

async fn by_secret(executor: &mut impl Executor, secret: &str) -> db::Result<Option<Token>> {
    let row: Option<Row> = executor
        .fetch_optional(
            &select()
                .and_where(
                    Expr::col((ApiTokens::Table, ApiTokens::TokenHash)).eq(token_hash(secret)),
                )
                .to_owned(),
        )
        .await?;
    Ok(row.map(Token::from))
}

/// Shortens every token that would outlive a new lifetime cap, so lowering
/// the cap applies to existing tokens too.
pub async fn apply_lifetime_cap(executor: &mut impl Executor, days: i64) -> db::Result<u64> {
    let lifetime = Expr::col(ApiTokens::CreatedAt).add(days * DAY_MS);
    executor
        .execute(
            &Query::update()
                .table(ApiTokens::Table)
                .value(ApiTokens::ExpiresAt, lifetime.clone())
                .and_where(
                    Expr::col(ApiTokens::ExpiresAt)
                        .is_null()
                        .or(Expr::col(ApiTokens::ExpiresAt).gt(lifetime)),
                )
                .to_owned(),
        )
        .await
}

fn refused(message: &'static str, code: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, message).with_code(code)
}

/// Authenticates a request carrying `secret`. Unknown tokens count against
/// the client address like failed sign-ins; once it has used them all,
/// every token from it is refused until the oldest failure ages out.
pub async fn authenticate(
    state: &AppState,
    secret: &str,
    ip: IpAddr,
) -> Result<Authorized, ApiError> {
    let limits = &state.auth.api_token_failures;
    let key = ip_key(ip);
    limits.check(&key).map_err(ApiError::rate_limited)?;
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    if !settings.api_tokens_enabled {
        return Err(refused(
            "API tokens are turned off on this server",
            "api_tokens_disabled",
        ));
    }
    let Some(token) = by_secret(&mut database, secret).await? else {
        tracing::info!(%ip, "refused an unknown API token");
        limits.hit(&key).map_err(ApiError::rate_limited)?;
        return Err(refused(
            "the API token is invalid or was revoked",
            "invalid_token",
        ));
    };
    let now = now_ms();
    if token.expired(now) {
        return Err(refused("the API token has expired", "token_expired"));
    }
    let user = users::by_id(&mut database, &token.user.id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(|| refused("the API token's user is disabled", "user_disabled"))?;
    if token
        .last_used_at
        .is_none_or(|used| now - used >= TOUCH_INTERVAL_MS)
    {
        database
            .execute(
                &Query::update()
                    .table(ApiTokens::Table)
                    .values([
                        (ApiTokens::LastUsedAt, now.into()),
                        (ApiTokens::LastUsedIp, ip.to_string().into()),
                    ])
                    .and_where(Expr::col(ApiTokens::Id).eq(token.id.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    let roles = rbac::user_roles(&mut database, &user.id).await?;
    let held = rbac::permissions_of(&roles);
    let permissions = match &token.permissions {
        Some(scope) => held.intersection(scope).copied().collect(),
        None => held,
    };
    // A scoped token isn't the Administrator role, which also stands for
    // permissions no list names.
    let administrator =
        token.permissions.is_none() && roles.iter().any(rbac::Role::is_administrator);
    Ok(Authorized::for_token(
        user,
        roles,
        permissions,
        administrator,
        ip,
        token.reference(),
    ))
}

/// What a token may be limited to: the permissions its user holds now.
pub fn choices(permissions: &Permissions) -> Vec<PermissionChoice> {
    Permission::ALL
        .iter()
        .filter(|permission| permissions.contains(permission))
        .map(|permission| PermissionChoice {
            name: *permission,
            description: permission.description(),
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct PermissionChoice {
    name: Permission,
    description: &'static str,
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn tokens_are_prefixed_and_random() {
        let token = new_secret();
        assert!(token.starts_with("mrmm_pat_"));
        assert_eq!(token.len(), PREFIX.len() + 64);
        assert_ne!(token, new_secret());
    }

    #[test]
    fn only_prefixed_bearer_credentials_are_api_tokens() {
        let headers = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
            headers
        };
        assert_eq!(
            bearer(&headers("Bearer mrmm_pat_abc")),
            Some("mrmm_pat_abc")
        );
        assert_eq!(
            bearer(&headers("bearer  mrmm_pat_abc ")),
            Some("mrmm_pat_abc")
        );
        assert_eq!(bearer(&headers("Bearer 0123abcd")), None);
        assert_eq!(bearer(&headers("Basic mrmm_pat_abc")), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn scopes_ignore_permissions_that_no_longer_exist() {
        let scope = parse_scope(r#"["devices.view","devices.everything","audit.view"]"#);
        assert_eq!(
            scope,
            [Permission::DevicesView, Permission::AuditView].into()
        );
        assert_eq!(parse_scope(&scope_json(&scope)), scope);
    }
}
