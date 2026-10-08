//! Website sessions: an HttpOnly cookie holding a random token whose hash is
//! a `user_sessions` row.
use std::net::IpAddr;

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
};
use sea_query::{Expr, ExprTrait, Query};

use crate::{
    audit::Actor,
    db::{
        Executor,
        tables::{UserSessions, Users},
    },
    http::{ApiError, AppState, client_ip::ClientIp},
    rbac::{self, Permission, Permissions, Role},
    secrets::{new_token, token_hash},
    settings,
    time::{HOUR_MS, MINUTE_MS, SECOND_MS, now_ms},
    users::{self, User, new_id},
};

/// `__Host-` makes browsers insist on Secure, Path=/ and no Domain, so no
/// other host or path can set or read it.
pub const COOKIE_NAME: &str = "__Host-meshrmm-session";
/// `last_seen_at` is written at most this often.
const TOUCH_INTERVAL_MS: i64 = MINUTE_MS;
const MAX_USER_AGENT_LENGTH: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    Passkey,
    Oidc,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Passkey => "passkey",
            Self::Oidc => "oidc",
        }
    }
}

/// Where a sign-in came from, recorded on the session.
#[derive(Debug, Clone)]
pub struct Client {
    pub ip: IpAddr,
    pub user_agent: Option<String>,
}

impl Client {
    pub fn new(ip: IpAddr, headers: &HeaderMap) -> Self {
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(|agent| agent.chars().take(MAX_USER_AGENT_LENGTH).collect());
        Self { ip, user_agent }
    }
}

/// Starts a session for `user_id` and returns its `Set-Cookie` value.
/// `verified` records that a second factor (or passkey) was just used.
pub async fn start(
    executor: &mut impl Executor,
    user_id: &str,
    method: AuthMethod,
    verified: bool,
    client: &Client,
) -> Result<HeaderValue, ApiError> {
    let settings = settings::load(executor).await?;
    let now = now_ms();
    let lifetime_ms = settings.session_lifetime_hours * HOUR_MS;
    let token = new_token();
    executor
        .execute(
            &Query::insert()
                .into_table(UserSessions::Table)
                .columns([
                    UserSessions::Id,
                    UserSessions::TokenHash,
                    UserSessions::UserId,
                    UserSessions::AuthMethod,
                    UserSessions::CreatedAt,
                    UserSessions::LastSeenAt,
                    UserSessions::ExpiresAt,
                    UserSessions::VerifiedAt,
                    UserSessions::Ip,
                    UserSessions::UserAgent,
                ])
                .values_panic([
                    new_id().into(),
                    token_hash(&token).into(),
                    user_id.into(),
                    method.as_str().into(),
                    now.into(),
                    now.into(),
                    (now + lifetime_ms).into(),
                    verified.then_some(now).into(),
                    client.ip.to_string().into(),
                    client.user_agent.clone().into(),
                ])
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::update()
                .table(Users::Table)
                .value(Users::LastSignInAt, now)
                .and_where(Expr::col(Users::Id).eq(user_id))
                .to_owned(),
        )
        .await?;
    Ok(cookie(&token, lifetime_ms / SECOND_MS))
}

fn cookie(token: &str, max_age_seconds: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={max_age_seconds}; HttpOnly; Secure; SameSite=Lax"
    ))
    .expect("a hex token is a valid header value")
}

/// A `Set-Cookie` value that removes the session cookie.
pub fn clear_cookie() -> HeaderValue {
    cookie("", 0)
}

/// The session token from the request's cookies.
pub fn token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
}

/// Ends the session the request's cookie names, if any. Returns its user.
pub async fn end(
    executor: &mut impl Executor,
    headers: &HeaderMap,
) -> Result<Option<String>, ApiError> {
    let Some(token) = token(headers) else {
        return Ok(None);
    };
    let hash = token_hash(&token);
    let session: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(UserSessions::UserId)
                .from(UserSessions::Table)
                .and_where(Expr::col(UserSessions::TokenHash).eq(hash.as_str()))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(UserSessions::Table)
                .and_where(Expr::col(UserSessions::TokenHash).eq(hash))
                .to_owned(),
        )
        .await?;
    Ok(session.map(|(user_id,)| user_id))
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    user_id: String,
    auth_method: String,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
}

/// A request with a live session, even one whose user must still set up a
/// second factor. Most routes take [`Authorized`] instead.
#[derive(Debug, Clone)]
pub struct SignedIn {
    pub session_id: String,
    pub auth_method: String,
    pub session_created_at: i64,
    pub session_expires_at: i64,
    pub user: User,
    pub roles: Vec<Role>,
    pub permissions: Permissions,
    pub two_factor_enabled: bool,
    /// The instance requires a second factor and this password session's
    /// user has none: only account security routes are open to it.
    pub must_enroll_two_factor: bool,
    pub idle_timeout_minutes: i64,
    pub ip: IpAddr,
}

impl SignedIn {
    pub fn actor(&self) -> Actor {
        Actor::user(&self.user.id, &self.user.email, Some(self.ip))
    }

    pub fn has(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    pub fn is_administrator(&self) -> bool {
        self.roles.iter().any(Role::is_administrator)
    }
}

fn unauthenticated() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "sign in to continue").with_code("unauthenticated")
}

impl FromRequestParts<AppState> for SignedIn {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = token(&parts.headers).ok_or_else(unauthenticated)?;
        let ClientIp(ip) = ClientIp::from_request_parts(parts, state)
            .await
            .unwrap_or_else(|never| match never {});
        load(state, &token, ip, true).await
    }
}

/// The live session `token` names. `touch` counts the request as activity,
/// which a check that the user is still signed in should not.
pub async fn load(
    state: &AppState,
    token: &str,
    ip: IpAddr,
    touch: bool,
) -> Result<SignedIn, ApiError> {
    let mut database = &state.database;
    let session: SessionRow = database
        .fetch_optional(
            &Query::select()
                .columns([
                    UserSessions::Id,
                    UserSessions::UserId,
                    UserSessions::AuthMethod,
                    UserSessions::CreatedAt,
                    UserSessions::LastSeenAt,
                    UserSessions::ExpiresAt,
                ])
                .from(UserSessions::Table)
                .and_where(Expr::col(UserSessions::TokenHash).eq(token_hash(token)))
                .to_owned(),
        )
        .await?
        .ok_or_else(unauthenticated)?;
    let settings = settings::load(&mut database).await?;
    let now = now_ms();
    let idle_ms = settings.dashboard_idle_timeout_minutes * MINUTE_MS;
    if now >= session.expires_at || now >= session.last_seen_at + idle_ms {
        database
            .execute(
                &Query::delete()
                    .from_table(UserSessions::Table)
                    .and_where(Expr::col(UserSessions::Id).eq(session.id.as_str()))
                    .to_owned(),
            )
            .await?;
        return Err(unauthenticated());
    }
    let user = users::by_id(&mut database, &session.user_id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(unauthenticated)?;
    if touch && now - session.last_seen_at >= TOUCH_INTERVAL_MS {
        database
            .execute(
                &Query::update()
                    .table(UserSessions::Table)
                    .value(UserSessions::LastSeenAt, now)
                    .and_where(Expr::col(UserSessions::Id).eq(session.id.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    let roles = rbac::user_roles(&mut database, &user.id).await?;
    let two_factor_enabled = users::has_two_factor(&mut database, &user.id).await?;
    Ok(SignedIn {
        must_enroll_two_factor: settings.require_two_factor
            && session.auth_method == AuthMethod::Password.as_str()
            && !two_factor_enabled,
        session_id: session.id,
        auth_method: session.auth_method,
        session_created_at: session.created_at,
        session_expires_at: session.expires_at,
        permissions: rbac::permissions_of(&roles),
        roles,
        user,
        two_factor_enabled,
        idle_timeout_minutes: settings.dashboard_idle_timeout_minutes,
        ip,
    })
}

/// A signed-in user with full access to the routes their permissions allow.
#[derive(Debug, Clone)]
pub struct Authorized(pub SignedIn);

impl std::ops::Deref for Authorized {
    type Target = SignedIn;

    fn deref(&self) -> &SignedIn {
        &self.0
    }
}

impl Authorized {
    /// Full access, unless the user must still set up a second factor.
    pub fn new(signed_in: SignedIn) -> Result<Self, ApiError> {
        if signed_in.must_enroll_two_factor {
            return Err(
                ApiError::forbidden("set up two-factor authentication to continue")
                    .with_code("two_factor_enrollment_required"),
            );
        }
        Ok(Self(signed_in))
    }

    pub fn require(&self, permission: Permission) -> Result<(), ApiError> {
        if self.has(permission) {
            Ok(())
        } else {
            Err(
                ApiError::forbidden(format!("you don't have the {permission} permission"))
                    .with_code("permission_denied"),
            )
        }
    }
}

impl FromRequestParts<AppState> for Authorized {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        Self::new(SignedIn::from_request_parts(parts, state).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("theme=dark"));
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("a=b; __Host-meshrmm-session=abc123; c=d"),
        );
        assert_eq!(token(&headers).as_deref(), Some("abc123"));
        assert_eq!(token(&HeaderMap::new()), None);
        let mut empty = HeaderMap::new();
        empty.insert(
            header::COOKIE,
            HeaderValue::from_static("__Host-meshrmm-session="),
        );
        assert_eq!(token(&empty), None);
    }

    #[test]
    fn cookies_are_host_only_secure_and_http_only() {
        let value = cookie("abc", 3600);
        assert_eq!(
            value,
            "__Host-meshrmm-session=abc; Path=/; Max-Age=3600; HttpOnly; Secure; SameSite=Lax"
        );
        assert!(clear_cookie().to_str().unwrap().contains("Max-Age=0"));
    }
}
