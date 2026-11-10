//! Signing in with a password and second factor, and signing out.
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use webauthn_rs::prelude::{PasskeyAuthentication, PublicKeyCredential, RequestChallengeResponse};

use super::{AccessUnchanged, SignInResult, signed_in_response};
use crate::{
    audit::{self, Actor, Target},
    auth::{
        devices, passkeys, password,
        second_factor::{self, TotpState},
        session::{self, AuthMethod, Client},
    },
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    settings,
    users::{self, User},
};

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
    let email = users::normalize_email(&request.email).ok();
    let device = email
        .as_deref()
        .and_then(|email| devices::device(&state.instance_key, &headers, email));
    let attempt = state
        .auth
        .count_password_attempt(ip, email.as_deref(), device.as_deref())
        .map_err(ApiError::rate_limited)?;
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
    state.auth.password_accepted(&attempt);
    let user = user.expect("a password matched, so the account exists");
    // Only someone who knows the password learns the account is disabled.
    if user.disabled {
        return Err(ApiError::forbidden("this account is disabled").with_code("account_disabled"));
    }
    let factors = users::second_factors(&mut database, &user.id).await?;
    if factors.any() {
        let (passkey, passkey_state) = if factors.passkeys > 0 {
            passkey_prompt(&state, &user).await?
        } else {
            (None, None)
        };
        let mut methods = Vec::new();
        if factors.totp {
            methods.push("totp");
        }
        if passkey.is_some() {
            methods.push("passkey");
        }
        methods.push("recovery_code");
        let challenge = state.auth.challenges.issue(&user.id, passkey_state);
        return Ok(Json(SignInResult::SecondFactorRequired {
            challenge,
            methods,
            passkey: passkey.map(Box::new),
        })
        .into_response());
    }
    finish(&state, &user, None, Client::new(ip, &headers)).await
}

/// A prompt for one of the user's passkeys, unless passkeys can't work on
/// this server.
async fn passkey_prompt(
    state: &AppState,
    user: &User,
) -> Result<
    (
        Option<RequestChallengeResponse>,
        Option<PasskeyAuthentication>,
    ),
    ApiError,
> {
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    let Ok(relying_party) = passkeys::relying_party(&state.config, &settings.instance_name) else {
        return Ok((None, None));
    };
    let stored = passkeys::for_user(&mut database, &user.id).await?;
    let credentials = stored
        .into_iter()
        .map(|stored| stored.passkey)
        .collect::<Vec<_>>();
    if credentials.is_empty() {
        return Ok((None, None));
    }
    let (options, state) = relying_party
        .start_passkey_authentication(&credentials)
        .map_err(|error| anyhow::anyhow!("could not start a passkey prompt: {error}"))?;
    Ok((Some(options), Some(state)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecondFactorSignIn {
    challenge: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
    /// The browser's answer to the passkey prompt.
    #[serde(default)]
    passkey: Option<PublicKeyCredential>,
}

/// `POST /v1/auth/sign-in/second-factor`: completes a sign-in with an
/// authenticator code, a passkey or a recovery code.
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
    let attempt = state
        .auth
        .challenges
        .attempt(&request.challenge, request.passkey.is_some())
        .ok_or_else(expired)?;
    let mut database = &state.database;
    let user = users::by_id(&mut database, &attempt.user_id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(expired)?;
    // Wrong codes count against the user, so new challenges don't buy more
    // guesses. Only someone with the password gets here, so a stranger's
    // wrong passwords don't use these up.
    state
        .auth
        .second_factor_by_user
        .hit(&user.id)
        .map_err(ApiError::rate_limited)?;
    let (method, accepted) = match (&request.code, &request.recovery_code, &request.passkey) {
        (Some(code), None, None) => (
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
        (None, Some(code), None) => (
            "recovery_code",
            second_factor::use_recovery_code(&mut database, &user.id, code).await?,
        ),
        (None, None, Some(credential)) => {
            let prompt = attempt.passkey.ok_or_else(|| {
                ApiError::bad_request(
                    "this sign-in has no passkey prompt; enter your password again",
                )
                .with_code("challenge_expired")
            })?;
            (
                "passkey",
                check_passkey(&state, &user, credential, &prompt).await?,
            )
        }
        _ => {
            return Err(ApiError::bad_request(
                "use your authenticator app, a passkey or a recovery code",
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
    state.auth.second_factor_by_user.refund(&user.id);
    state.auth.challenges.complete(&request.challenge);
    finish(&state, &user, Some(method), Client::new(ip, &headers)).await
}

/// Checks the browser's answer to a passkey prompt for `user`.
async fn check_passkey(
    state: &AppState,
    user: &User,
    credential: &PublicKeyCredential,
    prompt: &PasskeyAuthentication,
) -> Result<bool, ApiError> {
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    let relying_party = passkeys::relying_party(&state.config, &settings.instance_name)?;
    let result = match relying_party.finish_passkey_authentication(credential, prompt) {
        Ok(result) => result,
        Err(error) => {
            tracing::info!(%error, user_id = user.id, "a passkey was refused");
            return Ok(false);
        }
    };
    let Some(mut stored) = passkeys::by_credential_id(&mut database, result.cred_id())
        .await?
        .filter(|stored| stored.user_id == user.id)
    else {
        return Ok(false);
    };
    passkeys::record_use(&mut database, &mut stored, &result).await?;
    Ok(true)
}

/// Starts the session, records the sign-in and gives the browser a device
/// cookie for the account.
async fn finish(
    state: &AppState,
    user: &User,
    second_factor: Option<&str>,
    client: Client,
) -> Result<Response, ApiError> {
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
    let mut response = signed_in_response(
        StatusCode::OK,
        cookie,
        settings.require_two_factor && second_factor.is_none(),
    );
    // Stored emails are normalized, as the cookie's account must be.
    response.headers_mut().append(
        header::SET_COOKIE,
        devices::cookie(&state.instance_key, &user.email),
    );
    Ok(response)
}

/// `POST /v1/auth/sign-out`: ends this browser's session, if it has one.
pub async fn sign_out(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mut database = &state.database;
    let ended = session::end(&mut database, &headers).await?;
    if let Some(user_id) = &ended
        && let Some(user) = users::by_id(&mut database, user_id).await?
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
    let mut response = (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::clear_cookie())],
    )
        .into_response();
    if ended.is_none() {
        response.extensions_mut().insert(AccessUnchanged);
    }
    Ok(response)
}
