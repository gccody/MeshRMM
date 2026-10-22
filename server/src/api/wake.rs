//! Wake-on-LAN: waking offline devices through online devices on their
//! network, one or many at once, and the wake requests' outcomes. Waking
//! needs `devices.wake`; seeing wake requests needs `devices.view`. Each
//! device woken is recorded in the audit log as `agent.wake`.
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::{
    agents::{self, parse_id},
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    wake::{self, MAX_DEVICES, Requester, Wake},
};

/// How long a request waits for the relays to report, which they do within
/// a second or two.
const RELAY_WAIT: Duration = Duration::from_secs(5);
const DEFAULT_RECENT: u64 = 5;
const MAX_RECENT: u64 = 50;

fn online() -> ApiError {
    ApiError::conflict("the device is online already").with_code("device_online")
}

/// Starts wake requests for `devices`, which are enrolled and offline, and
/// waits a little for their relays.
async fn wake(
    state: &AppState,
    actor: &Authorized,
    devices: &[String],
) -> Result<Vec<Wake>, ApiError> {
    let mut changes = state.wake.subscribe();
    let actor_record = actor.actor();
    let ids = wake::start(
        state,
        devices,
        Requester {
            user_id: &actor.user.id,
            job_id: None,
            actor: Some(&actor_record),
        },
    )
    .await?;
    let ids = devices
        .iter()
        .filter_map(|device| ids.get(device).cloned())
        .collect::<Vec<_>>();
    wake::wait_for_relays(state, &mut changes, &ids, RELAY_WAIT).await?;
    Ok(wake::by_ids(&mut &state.database, &ids).await?)
}

/// `POST /v1/agents/{id}/wake`: wakes the offline device through online
/// devices on its network. 201 with the wake request, once its relays
/// reported or after a few seconds; its status is `no_relay`, with the
/// reason in `detail`, when no device can wake it. 409 `device_online` when
/// it is online.
pub async fn wake_device(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Wake>), ApiError> {
    actor.require(Permission::DevicesWake)?;
    let id = parse_id(&id, "device")?;
    if !agents::is_active(&mut &state.database, &id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    if state.agents.is_connected(&id) {
        return Err(online());
    }
    let wake = wake(&state, &actor, std::slice::from_ref(&id))
        .await?
        .into_iter()
        .next()
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(wake)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakeMany {
    device_ids: Vec<String>,
}

/// A chosen device that wasn't woken.
#[derive(Debug, Serialize)]
pub struct Skipped {
    device_id: String,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct WokenMany {
    wakes: Vec<Wake>,
    /// Chosen devices that are online, or no longer enrolled.
    skipped: Vec<Skipped>,
}

/// `POST /v1/wakes`: wakes the chosen devices that are offline, like
/// `POST /v1/agents/{id}/wake` for each. Online and unknown devices are
/// listed as skipped.
pub async fn wake_many(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<WakeMany>,
) -> Result<(StatusCode, Json<WokenMany>), ApiError> {
    actor.require(Permission::DevicesWake)?;
    let mut ids = request.device_ids;
    if ids.is_empty() || ids.len() > MAX_DEVICES {
        return Err(
            ApiError::bad_request(format!("choose 1 to {MAX_DEVICES} devices to wake"))
                .with_code("invalid_wake"),
        );
    }
    for id in &ids {
        parse_id(id, "device")?;
    }
    ids.sort();
    ids.dedup();
    let mut skipped = Vec::new();
    let mut offline = Vec::new();
    for id in ids {
        if !agents::is_active(&mut &state.database, &id).await? {
            skipped.push(Skipped {
                device_id: id,
                reason: "not_found",
            });
        } else if state.agents.is_connected(&id) {
            skipped.push(Skipped {
                device_id: id,
                reason: "online",
            });
        } else {
            offline.push(id);
        }
    }
    let wakes = if offline.is_empty() {
        Vec::new()
    } else {
        wake(&state, &actor, &offline).await?
    };
    Ok((StatusCode::CREATED, Json(WokenMany { wakes, skipped })))
}

fn parameters<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(parameters)| parameters)
        .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
}

#[derive(Debug, Deserialize)]
pub struct ByIds {
    /// Comma-separated wake request IDs.
    ids: String,
}

#[derive(Debug, Serialize)]
pub struct WakeList {
    wakes: Vec<Wake>,
}

/// `GET /v1/wakes?ids=…`: wake requests by ID, as their relays report and
/// their devices come online.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    query: Result<Query<ByIds>, QueryRejection>,
) -> Result<Json<WakeList>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let ByIds { ids } = parameters(query)?;
    let ids = ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| parse_id(id, "wake request"))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.len() > MAX_DEVICES {
        return Err(ApiError::bad_request(format!(
            "ask for at most {MAX_DEVICES} wake requests"
        )));
    }
    Ok(Json(WakeList {
        wakes: wake::by_ids(&mut &state.database, &ids).await?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct Recent {
    limit: Option<u64>,
}

/// `GET /v1/agents/{id}/wakes?limit=`: the device's latest wake requests,
/// newest first.
pub async fn recent(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<Recent>, QueryRejection>,
) -> Result<Json<WakeList>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let Recent { limit } = parameters(query)?;
    let id = parse_id(&id, "device")?;
    if !agents::is_active(&mut &state.database, &id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    let limit = limit.unwrap_or(DEFAULT_RECENT).clamp(1, MAX_RECENT);
    Ok(Json(WakeList {
        wakes: wake::recent(&mut &state.database, &id, limit).await?,
    }))
}
