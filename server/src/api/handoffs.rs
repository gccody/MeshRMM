//! Remote handoffs: the website asks for a one-time token that the viewer it
//! opens redeems to start a remote session with a device.
use axum::{Json, extract::State};
use meshrmm_protocol_types::valid_connection_reason;
use sea_query::Query;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::{self, parse_id},
    audit::{self, Target},
    auth::Authorized,
    db::tables::RemoteHandoffs,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    secrets::{new_token, token_hash},
    time::{MINUTE_MS, now_ms},
};

/// The viewer must redeem the token this soon; the website opens it at once.
pub const HANDOFF_TTL_MS: i64 = MINUTE_MS;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRequest {
    device_id: String,
    /// Connect to the background desktop, without the user seeing it.
    #[serde(default)]
    start_in_background: bool,
    /// Why the technician is connecting, for the Agent's approval prompt.
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Serialize)]
pub struct Handoff {
    handoff_token: String,
    /// The server the viewer redeems the token with.
    api_url: String,
    expires_at_unix_ms: i64,
    start_in_background: bool,
}

/// `POST /v1/remote/handoffs`.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<HandoffRequest>,
) -> Result<Json<Handoff>, ApiError> {
    actor.require(Permission::SessionsConnect)?;
    if request.start_in_background {
        actor.require(Permission::SessionsConnectBackground)?;
    }
    let device_id = parse_id(&request.device_id, "device")?;
    if !valid_connection_reason(&request.reason) {
        return Err(ApiError::bad_request(
            "the connection reason must be at most 500 bytes, without control characters except line breaks",
        ));
    }
    let reason = request.reason.trim();
    let token = new_token();
    let now = now_ms();
    let expires_at = now + HANDOFF_TTL_MS;
    let mut transaction = state.database.begin().await?;
    if !agents::is_active(&mut transaction, &device_id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    transaction
        .execute(
            &Query::insert()
                .into_table(RemoteHandoffs::Table)
                .columns([
                    RemoteHandoffs::TokenHash,
                    RemoteHandoffs::DeviceId,
                    RemoteHandoffs::UserId,
                    RemoteHandoffs::StartInBackground,
                    RemoteHandoffs::Reason,
                    RemoteHandoffs::CreatedAt,
                    RemoteHandoffs::ExpiresAt,
                ])
                .values_panic([
                    token_hash(&token).into(),
                    device_id.as_str().into(),
                    actor.user.id.as_str().into(),
                    request.start_in_background.into(),
                    reason.into(),
                    now.into(),
                    expires_at.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "remote.handoff_create",
        Target::device(&device_id),
        json!({ "start_in_background": request.start_in_background, "reason": reason }),
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(Handoff {
        handoff_token: token,
        api_url: state.config.public_origin(),
        expires_at_unix_ms: expires_at,
        start_in_background: request.start_in_background,
    }))
}
