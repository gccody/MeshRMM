//! Password reset links: requested by the user (emailed, if email is set up)
//! or made by an administrator or the admin CLI.
use axum::{Json, extract::State, http::StatusCode};
use sea_query::{Expr, ExprTrait, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    audit::{self, Actor, Target},
    auth::limits::ip_key,
    auth::password,
    db::{self, Executor, tables::PasswordResets},
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    mail,
    secrets::{new_token, token_hash},
    settings,
    time::{HOUR_MS, now_ms},
    users::{self, User, new_id},
};

/// A link the user asked for by email.
pub const SELF_SERVICE_RESET_TTL_MS: i64 = HOUR_MS;
/// A link an administrator passes on, which may take a while to reach the user.
pub const ADMIN_RESET_TTL_MS: i64 = 24 * HOUR_MS;

/// Stores a new reset for `user_id`, returning its token and expiry.
pub async fn create_reset(
    executor: &mut impl Executor,
    user_id: &str,
    created_by_user_id: Option<&str>,
    ttl_ms: i64,
) -> db::Result<(String, i64)> {
    let token = new_token();
    let now = now_ms();
    executor
        .execute(
            &Query::insert()
                .into_table(PasswordResets::Table)
                .columns([
                    PasswordResets::Id,
                    PasswordResets::TokenHash,
                    PasswordResets::UserId,
                    PasswordResets::CreatedByUserId,
                    PasswordResets::CreatedAt,
                    PasswordResets::ExpiresAt,
                ])
                .values_panic([
                    new_id().into(),
                    token_hash(&token).into(),
                    user_id.into(),
                    created_by_user_id.map(str::to_owned).into(),
                    now.into(),
                    (now + ttl_ms).into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok((token, now + ttl_ms))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetRequest {
    email: String,
}

/// `POST /v1/auth/password-reset`: emails a reset link if the account exists
/// and email is set up. The answer is the same either way, so it doesn't
/// reveal which addresses have accounts.
pub async fn request(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(request): JsonBody<ResetRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .auth
        .reset_requests
        .hit(&ip_key(ip))
        .map_err(ApiError::rate_limited)?;
    let Ok(email) = users::normalize_email(&request.email) else {
        return Ok(StatusCode::ACCEPTED);
    };
    // Sending takes a variable time, so it happens after the response.
    tokio::spawn(async move {
        if let Err(error) = email_reset(&state, &email, ip).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not send a password reset email"
            );
        }
    });
    Ok(StatusCode::ACCEPTED)
}

async fn email_reset(state: &AppState, email: &str, ip: std::net::IpAddr) -> anyhow::Result<()> {
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    let Some(smtp) = mail::Smtp::from_settings(&settings, &state.instance_key)? else {
        return Ok(());
    };
    let Some(user) = users::by_email(&mut database, email)
        .await?
        .filter(|user| !user.disabled)
    else {
        return Ok(());
    };
    let mut transaction = state.database.begin().await?;
    let (token, _) =
        create_reset(&mut transaction, &user.id, None, SELF_SERVICE_RESET_TTL_MS).await?;
    audit::record(
        &mut transaction,
        &Actor::anonymous(ip),
        "user.password_reset_request",
        Target::user(&user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    let link = super::link(state, "reset", &token);
    let (subject, body) = mail::password_reset_message(
        &settings.instance_name,
        &link,
        SELF_SERVICE_RESET_TTL_MS / HOUR_MS,
    );
    smtp.send(&mail::hello_name(state), &user.email, &subject, body)
        .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetToken {
    token: String,
}

#[derive(Debug, Serialize)]
pub struct ResetDetails {
    email: String,
    expires_at: i64,
}

#[derive(sqlx::FromRow)]
struct ResetRow {
    id: String,
    user_id: String,
    expires_at: i64,
}

fn invalid_link() -> ApiError {
    ApiError::not_found("this password reset link is invalid, used or expired")
        .with_code("invalid_token")
}

/// The unused, unexpired reset `token` names, with its enabled user.
async fn find(executor: &mut impl Executor, token: &str) -> Result<(ResetRow, User), ApiError> {
    let reset: ResetRow = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    PasswordResets::Id,
                    PasswordResets::UserId,
                    PasswordResets::ExpiresAt,
                ])
                .from(PasswordResets::Table)
                .and_where(Expr::col(PasswordResets::TokenHash).eq(token_hash(token)))
                .and_where(Expr::col(PasswordResets::UsedAt).is_null())
                .and_where(Expr::col(PasswordResets::ExpiresAt).gt(now_ms()))
                .to_owned(),
        )
        .await?
        .ok_or_else(invalid_link)?;
    let user = users::by_id(executor, &reset.user_id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(invalid_link)?;
    Ok((reset, user))
}

/// `POST /v1/auth/password-reset/lookup`: whose password a link resets.
pub async fn lookup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(request): JsonBody<ResetToken>,
) -> Result<Json<ResetDetails>, ApiError> {
    super::limit_token_attempts(&state, ip)?;
    let (reset, user) = find(&mut &state.database, &request.token).await?;
    Ok(Json(ResetDetails {
        email: user.email,
        expires_at: reset.expires_at,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCompletion {
    token: String,
    password: String,
}

/// `POST /v1/auth/password-reset/complete`: sets the new password and signs
/// the user out everywhere. They then sign in normally, second factor
/// included.
pub async fn complete(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(request): JsonBody<ResetCompletion>,
) -> Result<StatusCode, ApiError> {
    let settings = settings::load(&mut &state.database).await?;
    password::check_policy(&request.password, settings.password_min_length)?;
    // The token is checked before the costly hash, so guessing is limited.
    super::limit_token_attempts(&state, ip)?;
    find(&mut &state.database, &request.token).await?;
    let hash = password::hash(&request.password).await?;
    let mut transaction = state.database.begin().await?;
    let (reset, user) = find(&mut transaction, &request.token).await?;
    let claimed = transaction
        .execute(
            &Query::update()
                .table(PasswordResets::Table)
                .value(PasswordResets::UsedAt, now_ms())
                .and_where(Expr::col(PasswordResets::Id).eq(reset.id.as_str()))
                .and_where(Expr::col(PasswordResets::UsedAt).is_null())
                .to_owned(),
        )
        .await?;
    if claimed != 1 {
        return Err(invalid_link());
    }
    users::set_password(&mut transaction, &user.id, &hash, None, now_ms()).await?;
    audit::record(
        &mut transaction,
        &Actor::user(&user.id, &user.email, Some(ip)),
        "user.password_reset_complete",
        Target::user(&user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
