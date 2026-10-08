//! The signed-in user's own account: profile, password, two-factor
//! authentication and sessions.
use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{RoleRef, confirm_password};
use crate::{
    audit::{self, Target},
    auth::{
        Authorized, SignedIn, password,
        second_factor::{self, TotpState},
        session,
    },
    db::tables::{UserSessions, Users},
    http::{ApiError, AppState, JsonBody},
    rbac::Permissions,
    settings,
    time::now_ms,
    users,
};

#[derive(Debug, Serialize)]
pub struct Account {
    user: AccountUser,
    two_factor: TwoFactorStatus,
    roles: Vec<RoleRef>,
    permissions: Permissions,
    is_administrator: bool,
    session: CurrentSession,
    /// Sign the browser out after this long without activity.
    idle_timeout_minutes: i64,
    /// Device users approve each connection, so the website asks for a
    /// reason first. Policy is otherwise only for `settings.manage`.
    connection_approval: bool,
}

#[derive(Debug, Serialize)]
struct AccountUser {
    id: String,
    email: String,
    display_name: String,
    has_password: bool,
    created_at: i64,
    last_sign_in_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct TwoFactorStatus {
    enabled: bool,
    /// The instance requires it of password sign-ins.
    required: bool,
    /// Set up is required before anything else works.
    enrollment_required: bool,
    recovery_codes_remaining: i64,
}

#[derive(Debug, Serialize)]
struct CurrentSession {
    id: String,
    auth_method: String,
    created_at: i64,
    expires_at: i64,
}

/// `GET /v1/account`
pub async fn get(
    State(state): State<AppState>,
    signed_in: SignedIn,
) -> Result<Json<Account>, ApiError> {
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    let recovery_codes_remaining = if signed_in.two_factor_enabled {
        second_factor::recovery_codes_remaining(&mut database, &signed_in.user.id).await?
    } else {
        0
    };
    Ok(Json(Account {
        two_factor: TwoFactorStatus {
            enabled: signed_in.two_factor_enabled,
            required: settings.require_two_factor,
            enrollment_required: signed_in.must_enroll_two_factor,
            recovery_codes_remaining,
        },
        roles: signed_in.roles.iter().map(RoleRef::from).collect(),
        is_administrator: signed_in.is_administrator(),
        session: CurrentSession {
            id: signed_in.session_id.clone(),
            auth_method: signed_in.auth_method.clone(),
            created_at: signed_in.session_created_at,
            expires_at: signed_in.session_expires_at,
        },
        idle_timeout_minutes: signed_in.idle_timeout_minutes,
        connection_approval: settings.connection_approval,
        permissions: signed_in.permissions,
        user: AccountUser {
            has_password: signed_in.user.password_hash.is_some(),
            id: signed_in.user.id,
            email: signed_in.user.email,
            display_name: signed_in.user.display_name,
            created_at: signed_in.user.created_at,
            last_sign_in_at: signed_in.user.last_sign_in_at,
        },
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountUpdate {
    display_name: String,
}

/// `PATCH /v1/account`: changes the display name.
pub async fn update(
    State(state): State<AppState>,
    signed_in: Authorized,
    JsonBody(request): JsonBody<AccountUpdate>,
) -> Result<StatusCode, ApiError> {
    let display_name = users::validate_display_name(&request.display_name)?;
    let mut transaction = state.database.begin().await?;
    transaction
        .execute(
            &Query::update()
                .table(Users::Table)
                .values([
                    (Users::DisplayName, display_name.as_str().into()),
                    (Users::UpdatedAt, now_ms().into()),
                ])
                .and_where(Expr::col(Users::Id).eq(signed_in.user.id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.update",
        Target::user(&signed_in.user.id),
        json!({ "display_name": display_name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// `POST /v1/account/password`: changes the password and ends the user's
/// other sessions.
pub async fn change_password(
    State(state): State<AppState>,
    signed_in: SignedIn,
    JsonBody(request): JsonBody<PasswordChange>,
) -> Result<StatusCode, ApiError> {
    confirm_password(&state, &signed_in.user, &request.current_password).await?;
    let settings = settings::load(&mut &state.database).await?;
    password::check_policy(&request.new_password, settings.password_min_length)?;
    let hash = password::hash(&request.new_password).await?;
    let mut transaction = state.database.begin().await?;
    users::set_password(
        &mut transaction,
        &signed_in.user.id,
        &hash,
        Some(&signed_in.session_id),
        now_ms(),
    )
    .await?;
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.password_change",
        Target::user(&signed_in.user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordConfirmation {
    password: String,
}

#[derive(Debug, Serialize)]
pub struct TotpSetup {
    /// For typing into the authenticator app.
    secret: String,
    /// For a QR code.
    otpauth_uri: String,
}

/// `POST /v1/account/two-factor/totp`: starts setting up an authenticator
/// app. It takes effect once confirmed with a code.
pub async fn start_totp(
    State(state): State<AppState>,
    signed_in: SignedIn,
    JsonBody(request): JsonBody<PasswordConfirmation>,
) -> Result<Json<TotpSetup>, ApiError> {
    if signed_in.two_factor_enabled {
        return Err(ApiError::conflict(
            "two-factor authentication is already on; turn it off first to replace the authenticator",
        ));
    }
    confirm_password(&state, &signed_in.user, &request.password).await?;
    let settings = settings::load(&mut &state.database).await?;
    let secret = second_factor::new_totp_secret();
    second_factor::store_pending_totp(
        &mut &state.database,
        &state.instance_key,
        &signed_in.user.id,
        &secret,
    )
    .await?;
    let (secret, otpauth_uri) =
        second_factor::provisioning(&secret, &settings.instance_name, &signed_in.user.email);
    Ok(Json(TotpSetup {
        secret,
        otpauth_uri,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TotpConfirmation {
    code: String,
}

#[derive(Debug, Serialize)]
pub struct RecoveryCodes {
    /// Shown once; only their hashes are kept.
    recovery_codes: Vec<String>,
}

/// `POST /v1/account/two-factor/totp/confirm`: turns two-factor
/// authentication on once the app produces a valid code, and returns the
/// recovery codes.
pub async fn confirm_totp(
    State(state): State<AppState>,
    signed_in: SignedIn,
    JsonBody(request): JsonBody<TotpConfirmation>,
) -> Result<Json<RecoveryCodes>, ApiError> {
    if signed_in.two_factor_enabled {
        return Err(ApiError::conflict(
            "two-factor authentication is already on",
        ));
    }
    state
        .auth
        .password_confirmations
        .hit(&signed_in.user.id)
        .map_err(ApiError::rate_limited)?;
    let mut transaction = state.database.begin().await?;
    let accepted = second_factor::verify_totp(
        &mut transaction,
        &state.instance_key,
        &signed_in.user.id,
        TotpState::Pending,
        &request.code,
    )
    .await?;
    if !accepted {
        return Err(ApiError::bad_request(
            "the code is incorrect; check the app's clock, or start again if you haven't added the account yet",
        )
        .with_code("invalid_code"));
    }
    state.auth.password_confirmations.refund(&signed_in.user.id);
    second_factor::confirm_totp(&mut transaction, &signed_in.user.id).await?;
    let recovery_codes =
        second_factor::replace_recovery_codes(&mut transaction, &signed_in.user.id).await?;
    transaction
        .execute(
            &Query::update()
                .table(UserSessions::Table)
                .value(UserSessions::VerifiedAt, now_ms())
                .and_where(Expr::col(UserSessions::Id).eq(signed_in.session_id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.two_factor_enable",
        Target::user(&signed_in.user.id),
        json!({ "method": "totp" }),
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(RecoveryCodes { recovery_codes }))
}

/// `POST /v1/account/two-factor/totp/disable`: turns two-factor
/// authentication off, unless the instance requires it.
pub async fn disable_totp(
    State(state): State<AppState>,
    signed_in: Authorized,
    JsonBody(request): JsonBody<PasswordConfirmation>,
) -> Result<StatusCode, ApiError> {
    let settings = settings::load(&mut &state.database).await?;
    if settings.require_two_factor {
        return Err(ApiError::forbidden(
            "this server requires two-factor authentication, so it can't be turned off",
        )
        .with_code("two_factor_required"));
    }
    confirm_password(&state, &signed_in.user, &request.password).await?;
    let mut transaction = state.database.begin().await?;
    users::remove_two_factor(&mut transaction, &signed_in.user.id).await?;
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.two_factor_disable",
        Target::user(&signed_in.user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/account/two-factor/recovery-codes`: replaces the recovery codes.
pub async fn regenerate_recovery_codes(
    State(state): State<AppState>,
    signed_in: Authorized,
    JsonBody(request): JsonBody<PasswordConfirmation>,
) -> Result<Json<RecoveryCodes>, ApiError> {
    if !signed_in.two_factor_enabled {
        return Err(ApiError::conflict(
            "turn on two-factor authentication to get recovery codes",
        ));
    }
    confirm_password(&state, &signed_in.user, &request.password).await?;
    let mut transaction = state.database.begin().await?;
    let recovery_codes =
        second_factor::replace_recovery_codes(&mut transaction, &signed_in.user.id).await?;
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.recovery_codes_replace",
        Target::user(&signed_in.user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(RecoveryCodes { recovery_codes }))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SessionView {
    id: String,
    auth_method: String,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
    ip: Option<String>,
    user_agent: Option<String>,
    #[sqlx(skip)]
    current: bool,
}

/// `GET /v1/account/sessions`: where the user is signed in.
pub async fn sessions(
    State(state): State<AppState>,
    signed_in: SignedIn,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    let mut sessions: Vec<SessionView> = state
        .database
        .fetch_all(
            &Query::select()
                .columns([
                    UserSessions::Id,
                    UserSessions::AuthMethod,
                    UserSessions::CreatedAt,
                    UserSessions::LastSeenAt,
                    UserSessions::ExpiresAt,
                    UserSessions::Ip,
                    UserSessions::UserAgent,
                ])
                .from(UserSessions::Table)
                .and_where(Expr::col(UserSessions::UserId).eq(signed_in.user.id.as_str()))
                .and_where(Expr::col(UserSessions::ExpiresAt).gt(now_ms()))
                .order_by(UserSessions::LastSeenAt, Order::Desc)
                .to_owned(),
        )
        .await?;
    for session in &mut sessions {
        session.current = session.id == signed_in.session_id;
    }
    Ok(Json(sessions))
}

/// `DELETE /v1/account/sessions/{id}`: signs one of the user's sessions out.
pub async fn end_session(
    State(state): State<AppState>,
    signed_in: SignedIn,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let mut transaction = state.database.begin().await?;
    let ended = transaction
        .execute(
            &Query::delete()
                .from_table(UserSessions::Table)
                .and_where(Expr::col(UserSessions::Id).eq(id.as_str()))
                .and_where(Expr::col(UserSessions::UserId).eq(signed_in.user.id.as_str()))
                .to_owned(),
        )
        .await?;
    if ended == 0 {
        return Err(ApiError::not_found("no such session"));
    }
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.session_end",
        Target::user(&signed_in.user.id),
        json!({ "session_id": id }),
    )
    .await?;
    transaction.commit().await?;
    if id == signed_in.session_id {
        return Ok((
            StatusCode::NO_CONTENT,
            [(header::SET_COOKIE, session::clear_cookie())],
        )
            .into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
