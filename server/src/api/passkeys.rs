//! Passkeys: adding and removing them on the account page, and signing in
//! with one instead of a password.
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;
use webauthn_rs::prelude::{
    CreationChallengeResponse, DiscoverableKey, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Uuid,
};

use webauthn_rs_proto::ResidentKeyRequirement;

use super::{confirm_password, second_factor_removed, signed_in_response};
use crate::{
    audit::{self, Actor, Target},
    auth::{
        Authorized, PendingRegistration, SignedIn,
        limits::ip_key,
        passkeys::{self, MAX_PER_USER, PasskeyView},
        second_factor,
        session::{self, AuthMethod, Client},
    },
    db::tables::{UserPasskeys, UserSessions},
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    settings,
    time::now_ms,
    users,
};

/// `GET /v1/account/passkeys`
pub async fn list(
    State(state): State<AppState>,
    signed_in: SignedIn,
) -> Result<Json<Vec<PasskeyView>>, ApiError> {
    let stored = passkeys::for_user(&mut &state.database, &signed_in.user.id).await?;
    Ok(Json(stored.iter().map(PasskeyView::from).collect()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordConfirmation {
    password: String,
}

/// A WebAuthn prompt for the browser, and the token that answers it.
#[derive(Debug, Serialize)]
pub struct Prompt<T> {
    ceremony: String,
    options: T,
}

/// `POST /v1/account/passkeys/options`: starts adding a passkey. Open to a
/// user who must still set up two-factor authentication, since a passkey
/// counts.
pub async fn registration_options(
    State(state): State<AppState>,
    signed_in: SignedIn,
    JsonBody(request): JsonBody<PasswordConfirmation>,
) -> Result<Json<Prompt<CreationChallengeResponse>>, ApiError> {
    confirm_password(&state, &signed_in.user, &request.password).await?;
    let mut database = &state.database;
    let existing = passkeys::for_user(&mut database, &signed_in.user.id).await?;
    if existing.len() >= MAX_PER_USER {
        return Err(ApiError::conflict(format!(
            "you can have at most {MAX_PER_USER} passkeys; remove one first"
        )));
    }
    let settings = settings::load(&mut database).await?;
    let relying_party = passkeys::relying_party(&state.config, &settings.instance_name)?;
    let user_handle =
        Uuid::parse_str(&signed_in.user.id).map_err(|_| anyhow::anyhow!("user IDs are UUIDs"))?;
    let (mut options, registration) = relying_party
        .start_passkey_registration(
            user_handle,
            &signed_in.user.email,
            &signed_in.user.display_name,
            Some(
                existing
                    .iter()
                    .map(|stored| stored.passkey.cred_id().clone())
                    .collect(),
            ),
        )
        .map_err(|error| anyhow::anyhow!("could not start a passkey registration: {error}"))?;
    // Ask for a discoverable credential, so the passkey can sign in without
    // an email address. An authenticator that can't still works as a second
    // factor.
    if let Some(selection) = options.public_key.authenticator_selection.as_mut() {
        selection.resident_key = Some(ResidentKeyRequirement::Preferred);
    }
    let ceremony = state.auth.passkey_registrations.start(PendingRegistration {
        user_id: signed_in.user.id.clone(),
        state: registration,
    });
    Ok(Json(Prompt { ceremony, options }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPasskey {
    ceremony: String,
    name: String,
    credential: RegisterPublicKeyCredential,
}

#[derive(Debug, Serialize)]
pub struct AddedPasskey {
    passkey: PasskeyView,
    /// New recovery codes, when this passkey turned two-factor
    /// authentication on. Shown once.
    recovery_codes: Option<Vec<String>>,
}

/// `POST /v1/account/passkeys`: finishes adding a passkey.
pub async fn register(
    State(state): State<AppState>,
    signed_in: SignedIn,
    JsonBody(request): JsonBody<NewPasskey>,
) -> Result<(StatusCode, Json<AddedPasskey>), ApiError> {
    let name = passkeys::validate_name(&request.name)?;
    let expired = || {
        ApiError::bad_request("the passkey prompt expired; start again")
            .with_code("ceremony_expired")
    };
    let pending = state
        .auth
        .passkey_registrations
        .take(&request.ceremony)
        .filter(|pending| pending.user_id == signed_in.user.id)
        .ok_or_else(expired)?;
    let mut database = &state.database;
    // Browsers skip authenticators in `excludeCredentials`, but one that
    // doesn't would otherwise get a confusing verification error.
    if passkeys::by_credential_id(&mut database, request.credential.raw_id.as_slice())
        .await?
        .is_some()
    {
        return Err(
            ApiError::conflict("this passkey is already registered").with_code("passkey_exists")
        );
    }
    let settings = settings::load(&mut database).await?;
    let relying_party = passkeys::relying_party(&state.config, &settings.instance_name)?;
    let passkey = relying_party
        .finish_passkey_registration(&request.credential, &pending.state)
        .map_err(|error| {
            tracing::info!(%error, user_id = signed_in.user.id, "a passkey registration was refused");
            ApiError::bad_request("the passkey couldn't be verified; try again")
                .with_code("invalid_passkey")
        })?;
    let mut transaction = state.database.begin().await?;
    let had_second_factor = users::second_factors(&mut transaction, &signed_in.user.id)
        .await?
        .any();
    let stored = passkeys::insert(&mut transaction, &signed_in.user.id, &name, passkey).await?;
    let recovery_codes = if had_second_factor {
        None
    } else {
        Some(second_factor::replace_recovery_codes(&mut transaction, &signed_in.user.id).await?)
    };
    // The user just verified with the passkey.
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
        "account.passkey_add",
        Target::user(&signed_in.user.id),
        json!({ "passkey_id": stored.id, "name": stored.name }),
    )
    .await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(AddedPasskey {
            passkey: PasskeyView::from(&stored),
            recovery_codes,
        }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeyRename {
    name: String,
}

/// `PATCH /v1/account/passkeys/{id}`: renames a passkey.
pub async fn rename(
    State(state): State<AppState>,
    signed_in: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<PasskeyRename>,
) -> Result<StatusCode, ApiError> {
    let name = passkeys::validate_name(&request.name)?;
    let mut transaction = state.database.begin().await?;
    let renamed = transaction
        .execute(
            &Query::update()
                .table(UserPasskeys::Table)
                .value(UserPasskeys::Name, name.as_str())
                .and_where(Expr::col(UserPasskeys::Id).eq(id.as_str()))
                .and_where(Expr::col(UserPasskeys::UserId).eq(signed_in.user.id.as_str()))
                .to_owned(),
        )
        .await?;
    if renamed == 0 {
        return Err(ApiError::not_found("no such passkey"));
    }
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.passkey_rename",
        Target::user(&signed_in.user.id),
        json!({ "passkey_id": id, "name": name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/account/passkeys/{id}/remove`: removes a passkey, unless it is
/// the last second factor and the instance requires one.
pub async fn remove(
    State(state): State<AppState>,
    signed_in: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<PasswordConfirmation>,
) -> Result<StatusCode, ApiError> {
    confirm_password(&state, &signed_in.user, &request.password).await?;
    let mut transaction = state.database.begin().await?;
    users::lock(&mut transaction, &signed_in.user.id).await?;
    let removed = transaction
        .execute(
            &Query::delete()
                .from_table(UserPasskeys::Table)
                .and_where(Expr::col(UserPasskeys::Id).eq(id.as_str()))
                .and_where(Expr::col(UserPasskeys::UserId).eq(signed_in.user.id.as_str()))
                .to_owned(),
        )
        .await?;
    if removed == 0 {
        return Err(ApiError::not_found("no such passkey"));
    }
    second_factor_removed(&mut transaction, &signed_in.user.id).await?;
    audit::record(
        &mut transaction,
        &signed_in.actor(),
        "account.passkey_remove",
        Target::user(&signed_in.user.id),
        json!({ "passkey_id": id }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/auth/passkey/options`: a prompt for any passkey registered
/// here, for signing in without an email address or password.
pub async fn sign_in_options(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
) -> Result<Json<Prompt<RequestChallengeResponse>>, ApiError> {
    state
        .auth
        .ceremony_starts
        .hit(&ip_key(ip))
        .map_err(ApiError::rate_limited)?;
    let settings = settings::load(&mut &state.database).await?;
    let relying_party = passkeys::relying_party(&state.config, &settings.instance_name)?;
    let (mut options, authentication) = relying_party
        .start_discoverable_authentication()
        .map_err(|error| anyhow::anyhow!("could not start a passkey prompt: {error}"))?;
    // A modal prompt after the user asks for one, not autofill.
    options.mediation = None;
    let ceremony = state.auth.passkey_sign_ins.start(authentication);
    Ok(Json(Prompt { ceremony, options }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeySignIn {
    ceremony: String,
    credential: PublicKeyCredential,
}

fn invalid_passkey() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "that passkey isn't registered here or couldn't be verified",
    )
    .with_code("invalid_passkey")
}

/// `POST /v1/auth/passkey`: signs in with a passkey.
pub async fn sign_in(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(request): JsonBody<PasskeySignIn>,
) -> Result<Response, ApiError> {
    let ip_key = ip_key(ip);
    state
        .auth
        .sign_in_by_ip
        .hit(&ip_key)
        .map_err(ApiError::rate_limited)?;
    let authentication = state
        .auth
        .passkey_sign_ins
        .take(&request.ceremony)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "the passkey prompt expired; try again",
            )
            .with_code("ceremony_expired")
        })?;
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    let relying_party = passkeys::relying_party(&state.config, &settings.instance_name)?;
    let (user_handle, credential_id) = relying_party
        .identify_discoverable_authentication(&request.credential)
        .map_err(|_| invalid_passkey())?;
    let Some(mut stored) = passkeys::by_credential_id(&mut database, credential_id)
        .await?
        .filter(|stored| stored.user_id == user_handle.to_string())
    else {
        return Err(invalid_passkey());
    };
    let result = match relying_party.finish_discoverable_authentication(
        &request.credential,
        authentication,
        &[DiscoverableKey::from(&stored.passkey)],
    ) {
        Ok(result) => result,
        Err(error) => {
            tracing::info!(%error, user_id = stored.user_id, "a passkey sign-in was refused");
            audit::record(
                &mut database,
                &Actor::anonymous(ip),
                "auth.sign_in_failed",
                Target::user(&stored.user_id),
                json!({ "reason": "passkey" }),
            )
            .await?;
            return Err(invalid_passkey());
        }
    };
    state.auth.sign_in_by_ip.refund(&ip_key);
    let user = users::by_id(&mut database, &stored.user_id)
        .await?
        .ok_or_else(invalid_passkey)?;
    if user.disabled {
        return Err(ApiError::forbidden("this account is disabled").with_code("account_disabled"));
    }
    let mut transaction = state.database.begin().await?;
    passkeys::record_use(&mut transaction, &mut stored, &result).await?;
    let client = Client::new(ip, &headers);
    let cookie = session::start(
        &mut transaction,
        &user.id,
        AuthMethod::Passkey,
        true,
        &client,
    )
    .await?;
    audit::record(
        &mut transaction,
        &Actor::user(&user.id, &user.email, Some(ip)),
        "auth.sign_in",
        Target::user(&user.id),
        json!({ "method": "passkey", "passkey_id": stored.id }),
    )
    .await?;
    transaction.commit().await?;
    state.auth.sign_in_by_account.clear(&user.email);
    Ok(signed_in_response(StatusCode::OK, cookie, false))
}
