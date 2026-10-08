//! Signing in with a password and second factor, and signing out.
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use super::{SignInResult, signed_in_response};
use crate::{
    audit::{self, Actor, Target},
    auth::{
        limits::ip_key,
        password,
        second_factor::{self, TotpState},
        session::{self, AuthMethod, Client},
    },
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    settings,
    users::{self, User},
};

const SECOND_FACTOR_METHODS: &[&str] = &["totp", "recovery_code"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordSignIn {
    email: String,
    password: String,
}

fn invalid_credentials() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "the email or password is incorrect",
    )
    .with_code("invalid_credentials")
}

/// `POST /v1/auth/sign-in`: checks the password, then either signs in or
/// asks for a second factor.
pub async fn password(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(request): JsonBody<PasswordSignIn>,
) -> Result<Response, ApiError> {
    let ip_key = ip_key(ip);
    let limits = &state.auth;
    limits
        .sign_in_by_ip
        .hit(&ip_key)
        .map_err(ApiError::rate_limited)?;
    let email = users::normalize_email(&request.email).ok();
    if let Some(email) = &email
        && let Err(seconds) = limits.sign_in_by_account.hit(email)
    {
        limits.sign_in_by_ip.refund(&ip_key);
        return Err(ApiError::rate_limited(seconds));
    }
    let mut database = &state.database;
    let user = match &email {
        Some(email) => users::by_email(&mut database, email).await?,
        None => None,
    };
    let hash = user.as_ref().and_then(|user| user.password_hash.as_deref());
    if !password::verify(&request.password, hash).await {
        // Written after the response, so its time doesn't reveal that the
        // account exists.
        if let Some(user) = user {
            let database = state.database.clone();
            tokio::spawn(async move {
                let recorded = audit::record(
                    &mut &database,
                    &Actor::anonymous(ip),
                    "auth.sign_in_failed",
                    Target::user(&user.id),
                    json!({ "reason": "password" }),
                )
                .await;
                if let Err(error) = recorded {
                    tracing::error!(%error, "could not record a failed sign-in");
                }
            });
        }
        return Err(invalid_credentials());
    }
    // A right password isn't a guess; only failures count.
    limits.sign_in_by_ip.refund(&ip_key);
    if let Some(email) = &email {
        limits.sign_in_by_account.refund(email);
    }
    let user = user.expect("a password matched, so the account exists");
    // Only someone who knows the password learns the account is disabled.
    if user.disabled {
        return Err(ApiError::forbidden("this account is disabled").with_code("account_disabled"));
    }
    if users::has_two_factor(&mut database, &user.id).await? {
        let challenge = state.auth.challenges.issue(&user.id);
        return Ok(Json(SignInResult::SecondFactorRequired {
            challenge,
            methods: SECOND_FACTOR_METHODS,
        })
        .into_response());
    }
    finish(&state, &user, None, Client::new(ip, &headers)).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecondFactorSignIn {
    challenge: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// `POST /v1/auth/sign-in/second-factor`: completes a sign-in with an
/// authenticator code or a recovery code.
pub async fn second_factor(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(request): JsonBody<SecondFactorSignIn>,
) -> Result<Response, ApiError> {
    let expired = || {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "the sign-in expired or had too many wrong codes; enter your password again",
        )
        .with_code("challenge_expired")
    };
    let user_id = state
        .auth
        .challenges
        .attempt(&request.challenge)
        .ok_or_else(expired)?;
    let mut database = &state.database;
    let user = users::by_id(&mut database, &user_id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(expired)?;
    // Wrong codes count against the account like wrong passwords, so new
    // challenges don't buy more guesses.
    state
        .auth
        .sign_in_by_account
        .hit(&user.email)
        .map_err(ApiError::rate_limited)?;
    let (method, accepted) = match (&request.code, &request.recovery_code) {
        (Some(code), None) => (
            "totp",
            second_factor::verify_totp(
                &mut database,
                &state.instance_key,
                &user.id,
                TotpState::Confirmed,
                code,
            )
            .await?,
        ),
        (None, Some(code)) => (
            "recovery_code",
            second_factor::use_recovery_code(&mut database, &user.id, code).await?,
        ),
        _ => {
            return Err(ApiError::bad_request(
                "enter a code from your authenticator app or a recovery code",
            ));
        }
    };
    if !accepted {
        audit::record(
            &mut database,
            &Actor::anonymous(ip),
            "auth.sign_in_failed",
            Target::user(&user.id),
            json!({ "reason": method }),
        )
        .await?;
        return Err(
            ApiError::new(StatusCode::UNAUTHORIZED, "the code is incorrect")
                .with_code("invalid_code"),
        );
    }
    state.auth.challenges.complete(&request.challenge);
    finish(&state, &user, Some(method), Client::new(ip, &headers)).await
}

/// Starts the session and records the sign-in.
async fn finish(
    state: &AppState,
    user: &User,
    second_factor: Option<&str>,
    client: Client,
) -> Result<Response, ApiError> {
    // Stored emails are normalized, so this is the key failures used.
    state.auth.sign_in_by_account.clear(&user.email);
    let mut transaction = state.database.begin().await?;
    let settings = settings::load(&mut transaction).await?;
    let cookie = session::start(
        &mut transaction,
        &user.id,
        AuthMethod::Password,
        second_factor.is_some(),
        &client,
    )
    .await?;
    audit::record(
        &mut transaction,
        &Actor::user(&user.id, &user.email, Some(client.ip)),
        "auth.sign_in",
        Target::user(&user.id),
        json!({ "method": "password", "second_factor": second_factor }),
    )
    .await?;
    transaction.commit().await?;
    Ok(signed_in_response(
        StatusCode::OK,
        cookie,
        settings.require_two_factor && second_factor.is_none(),
    ))
}

/// `POST /v1/auth/sign-out`: ends this browser's session, if it has one.
pub async fn sign_out(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mut database = &state.database;
    if let Some(user_id) = session::end(&mut database, &headers).await?
        && let Some(user) = users::by_id(&mut database, &user_id).await?
    {
        audit::record(
            &mut database,
            &Actor::user(&user.id, &user.email, Some(ip)),
            "auth.sign_out",
            Target::user(&user.id),
            json!({}),
        )
        .await?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::clear_cookie())],
    )
        .into_response())
}
