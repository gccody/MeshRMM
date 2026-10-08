//! First-run setup: the holder of the setup link from the server log creates
//! the first administrator and names the instance.
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::Deserialize;
use serde_json::json;

use super::signed_in_response;
use crate::{
    audit::{self, Actor, Target},
    auth::limits::ip_key,
    auth::{
        password,
        session::{self, AuthMethod, Client},
    },
    db::tables::Settings as SettingsTable,
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    rbac::ADMINISTRATOR_ROLE_ID,
    settings,
    time::now_ms,
    users::{self, NewUser, new_id},
};

pub const MAX_INSTANCE_NAME_LENGTH: usize = 120;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    token: String,
    instance_name: String,
    email: String,
    display_name: String,
    password: String,
}

fn already_complete() -> ApiError {
    ApiError::conflict("setup is already complete; sign in instead").with_code("setup_complete")
}

pub async fn complete(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(request): JsonBody<SetupRequest>,
) -> Result<Response, ApiError> {
    state
        .auth
        .token_attempts
        .hit(&ip_key(ip))
        .map_err(ApiError::rate_limited)?;
    let mut database = &state.database;
    if users::count(&mut database).await? > 0 {
        return Err(already_complete());
    }
    let instance_name = validate_instance_name(&request.instance_name)?;
    let email = users::normalize_email(&request.email)?;
    let display_name = users::validate_display_name(&request.display_name)?;
    let settings = settings::load(&mut database).await?;
    password::check_policy(&request.password, settings.password_min_length)?;
    let invalid_token = || {
        ApiError::forbidden(
            "this setup link is invalid or was replaced; use the newest link in the server log",
        )
        .with_code("invalid_token")
    };
    // Checked before the costly hash, and taken after it.
    if !state.auth.setup.matches(&request.token) {
        return Err(invalid_token());
    }
    let password_hash = password::hash(&request.password).await?;
    if !state.auth.setup.consume(&request.token) {
        return Err(invalid_token());
    }
    let client = Client::new(ip, &headers);
    let created = async {
        let mut transaction = state.database.begin().await?;
        if users::count(&mut transaction).await? > 0 {
            return Err(already_complete());
        }
        let user_id = new_id();
        let now = now_ms();
        users::insert(
            &mut transaction,
            NewUser {
                id: &user_id,
                email: &email,
                display_name: &display_name,
                password_hash: Some(&password_hash),
                role_ids: &[ADMINISTRATOR_ROLE_ID.to_owned()],
                now_ms: now,
            },
        )
        .await?;
        transaction
            .execute(
                &Query::update()
                    .table(SettingsTable::Table)
                    .values([
                        (SettingsTable::InstanceName, instance_name.as_str().into()),
                        (SettingsTable::UpdatedAt, now.into()),
                        (SettingsTable::UpdatedByUserId, user_id.as_str().into()),
                    ])
                    .and_where(Expr::col(SettingsTable::Id).eq(1))
                    .to_owned(),
            )
            .await?;
        audit::record(
            &mut transaction,
            &Actor::user(&user_id, &email, Some(ip)),
            "setup.complete",
            Target::user(&user_id),
            json!({ "instance_name": instance_name }),
        )
        .await?;
        let cookie = session::start(
            &mut transaction,
            &user_id,
            AuthMethod::Password,
            false,
            &client,
        )
        .await?;
        transaction.commit().await?;
        Ok(cookie)
    }
    .await;
    match created {
        Ok(cookie) => {
            tracing::info!(%email, "setup complete; created the first administrator");
            Ok(signed_in_response(
                StatusCode::CREATED,
                cookie,
                settings.require_two_factor,
            ))
        }
        Err(error) => {
            if error.code() != Some("setup_complete") {
                state.auth.setup.restore(&request.token);
            }
            Err(error)
        }
    }
}

pub fn validate_instance_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_INSTANCE_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the instance name must be 1 to {MAX_INSTANCE_NAME_LENGTH} characters with no control characters"
        )));
    }
    Ok(name.to_owned())
}
