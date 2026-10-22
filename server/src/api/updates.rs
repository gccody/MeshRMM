//! Operating system updates: each device's pending updates and status,
//! asking it to scan or install, the history of scans and installs, and the
//! fleet's summary. Seeing them and asking for scans needs `updates.view`;
//! installing needs `updates.install`. Installs and scans on many devices
//! are jobs (`POST /v1/jobs`).
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
    http::StatusCode,
};
use meshrmm_protocol_types::{MAX_REBOOT_DELAY_MINUTES, RebootBehavior, UpdateCategory};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agents::{self, parse_id},
    audit::{self, Target as AuditTarget},
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    updates::{
        self,
        runs::{self, Filter, Install, Kind, NewRun, Run},
        store::{self, Pending},
    },
    users::new_id,
};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 200;
/// The most updates an install may choose one by one.
const MAX_CHOSEN_UPDATES: usize = 5_000;

/// The active device named by a path.
async fn device(state: &AppState, id: &str) -> Result<String, ApiError> {
    let id = parse_id(id, "device")?;
    if !agents::is_active(&mut &state.database, &id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    Ok(id)
}

#[derive(Debug, Serialize)]
pub struct DeviceUpdates {
    device_id: String,
    /// The device's summary, with `scanning` the scan the server is waiting
    /// for and `installing` the install in progress, or null.
    status: Value,
    updates: Vec<Pending>,
}

/// `GET /v1/agents/{id}/updates`: the device's update status and pending
/// updates.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<DeviceUpdates>, ApiError> {
    actor.require(Permission::UpdatesView)?;
    let id = device(&state, &id).await?;
    let mut database = &state.database;
    let summary = store::summaries(&mut database, Some(&id), &state.agents.connected())
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    let scanning = match runs::pending(&mut database, &id, Kind::Scan).await? {
        Some(run_id) => runs::get(&mut database, &run_id, false)
            .await?
            .map(|run| json!({ "run_id": run.id, "created_at": run.created_at })),
        None => None,
    };
    let installing = match runs::pending(&mut database, &id, Kind::Install).await? {
        Some(run_id) => runs::get(&mut database, &run_id, false).await?,
        None => None,
    };
    let mut status = json!(summary);
    status["scanning"] = json!(scanning);
    status["installing"] = json!(installing);
    let updates = store::pending(&mut database, &id).await?;
    Ok(Json(DeviceUpdates {
        device_id: id,
        status,
        updates,
    }))
}

fn offline() -> ApiError {
    ApiError::conflict("the device is offline; try again when it is online")
        .with_code("device_offline")
}

/// Records a run and asks the online Agent for it. A run the Agent never
/// got is removed again.
async fn start(
    state: &AppState,
    actor: &Authorized,
    device_id: &str,
    kind: Kind,
    install: Option<&Install>,
) -> Result<Run, ApiError> {
    let id = new_id();
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    runs::insert(
        &mut transaction,
        &NewRun {
            id: &id,
            device_id,
            kind,
            requested_by_user_id: &actor.user.id,
            policy_name: None,
            job_id: None,
            install,
            created_at: now,
        },
    )
    .await?;
    let mut metadata = json!({ "run_id": id });
    if let Some(install) = install {
        metadata["updates"] = json!(install.update_ids);
        metadata["categories"] = json!(install.categories);
        metadata["what"] = Install::describe(&install.update_ids, &install.categories).into();
        metadata["reboot"] = install.reboot.as_str().into();
        metadata["reboot_delay_minutes"] = install.reboot_delay_minutes.into();
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        match kind {
            Kind::Scan => "update.scan",
            Kind::Install => "update.install",
        },
        AuditTarget::device(device_id),
        metadata,
    )
    .await?;
    transaction.commit().await?;
    if !state
        .agents
        .send(device_id, runs::command(&id, kind, install))
    {
        let mut database = &state.database;
        runs::discard(&mut database, &id).await?;
        return Err(offline());
    }
    updates::publish_run(state, &id).await;
    updates::publish_device(state, device_id).await;
    runs::get(&mut &state.database, &id, true)
        .await?
        .ok_or_else(ApiError::internal)
}

/// `POST /v1/agents/{id}/updates/scan`: asks the online Agent to scan now.
/// A scan already asked for is returned instead.
pub async fn scan(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Run>), ApiError> {
    actor.require(Permission::UpdatesView)?;
    let id = device(&state, &id).await?;
    if !state.agents.is_connected(&id) {
        return Err(offline());
    }
    let mut database = &state.database;
    if let Some(run_id) = runs::pending(&mut database, &id, Kind::Scan).await?
        && let Some(run) = runs::get(&mut database, &run_id, false).await?
    {
        return Ok((StatusCode::ACCEPTED, Json(run)));
    }
    let run = start(&state, &actor, &id, Kind::Scan, None).await?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

fn default_reboot_delay() -> u32 {
    crate::automation::policies::DEFAULT_REBOOT_DELAY_MINUTES
}

/// An install as the website asks for it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    /// Chosen updates; empty for `categories`.
    #[serde(default)]
    update_ids: Vec<String>,
    /// Chosen categories; both empty for every pending update.
    #[serde(default)]
    categories: Vec<UpdateCategory>,
    reboot: RebootBehavior,
    #[serde(default = "default_reboot_delay")]
    reboot_delay_minutes: u32,
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_install")
}

/// `POST /v1/agents/{id}/updates/install`: asks the online Agent to install
/// the chosen updates, then restart as asked.
pub async fn install(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<InstallRequest>,
) -> Result<(StatusCode, Json<Run>), ApiError> {
    actor.require(Permission::UpdatesInstall)?;
    let id = device(&state, &id).await?;
    if !(1..=MAX_REBOOT_DELAY_MINUTES).contains(&request.reboot_delay_minutes) {
        return Err(invalid(format!(
            "the restart can wait 1 to {MAX_REBOOT_DELAY_MINUTES} minutes"
        )));
    }
    let mut update_ids = request.update_ids;
    update_ids.sort();
    update_ids.dedup();
    if update_ids.len() > MAX_CHOSEN_UPDATES
        || update_ids.iter().any(|update| {
            update.is_empty()
                || update.chars().count() > meshrmm_protocol_types::MAX_UPDATE_ID_CHARS
        })
    {
        return Err(invalid("the chosen updates aren't valid"));
    }
    let mut categories = request.categories;
    categories.sort();
    categories.dedup();
    let mut database = &state.database;
    let status = store::status(&mut database, &id).await?;
    if status.scanned_at.is_none() {
        return Err(
            ApiError::bad_request("the device hasn't reported a scan yet; scan it first")
                .with_code("not_scanned"),
        );
    }
    if !status.supported {
        return Err(ApiError::bad_request(
            "the device has no update service or package manager MeshRMM supports",
        )
        .with_code("unsupported"));
    }
    if !status.can_select() && (!update_ids.is_empty() || !categories.is_empty()) {
        let manager = status
            .manager()
            .map_or("This device's package manager", |manager| manager.label());
        return Err(ApiError::bad_request(format!(
            "{manager} installs every pending update at once; install all of them"
        ))
        .with_code("selection_unsupported"));
    }
    if !state.agents.is_connected(&id) {
        return Err(offline());
    }
    if runs::pending(&mut database, &id, Kind::Install)
        .await?
        .is_some()
    {
        return Err(ApiError::conflict(
            "the device is already installing updates; wait for it to finish",
        )
        .with_code("install_running"));
    }
    let install = Install {
        update_ids,
        categories,
        reboot: request.reboot,
        reboot_delay_minutes: request.reboot_delay_minutes,
    };
    let run = start(&state, &actor, &id, Kind::Install, Some(&install)).await?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFilter {
    device_id: Option<String>,
    kind: Option<Kind>,
    limit: Option<u64>,
}

/// `GET /v1/update-runs?device_id=&kind=&limit=`: scans and installs, newest
/// first, installs with each update's outcome.
pub async fn list_runs(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(filter): QueryParameters<RunFilter>,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::UpdatesView)?;
    let device_id = filter
        .device_id
        .as_deref()
        .map(|id| parse_id(id, "device"))
        .transpose()?;
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let runs = runs::list(
        &mut &state.database,
        &Filter {
            device_id,
            kind: filter.kind,
        },
        limit,
    )
    .await?;
    Ok(Json(json!({ "runs": runs })))
}

/// `GET /v1/update-runs/{id}`.
pub async fn get_run(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    actor.require(Permission::UpdatesView)?;
    let id = parse_id(&id, "update run")?;
    runs::get(&mut &state.database, &id, true)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("Update run not found"))
}

/// `GET /v1/updates/devices`: every device's updates at a glance.
pub async fn devices(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::UpdatesView)?;
    let devices = store::summaries(&mut &state.database, None, &state.agents.connected()).await?;
    Ok(Json(json!({ "devices": devices })))
}
