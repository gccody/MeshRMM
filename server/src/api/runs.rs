//! Toolbox script runs and file deliveries started from the website. Users
//! see what they started; users who may read the audit log see everyone's.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
    http::StatusCode,
};
use meshrmm_protocol_types::{FileDeliveryDestination, RunAs};
use sea_query::{Expr, ExprTrait};
use serde::{Deserialize, Serialize};

use super::{require_any, toolbox::toolbox_user};
use crate::{
    agents::parse_id,
    auth::Authorized,
    db::tables::{FileDeliveries, ScriptRuns},
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    toolbox::{self, DeliveryRow, DeliveryView, RunRow, RunView, Source},
};

/// How many recent runs or deliveries a list returns.
const LIST_LIMIT: u64 = 50;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRun {
    script_id: String,
    run_as: RunAs,
}

/// `POST /v1/agents/{id}/script-runs`: runs one of the user's scripts on the
/// device.
pub async fn start_run(
    State(state): State<AppState>,
    actor: Authorized,
    Path(device_id): Path<String>,
    JsonBody(request): JsonBody<StartRun>,
) -> Result<(StatusCode, Json<RunView>), ApiError> {
    let device_id = parse_id(&device_id, "device")?;
    let script_id = parse_id(&request.script_id, "script")?;
    let run = toolbox::start_script_run(
        &state,
        &toolbox_user(&actor),
        &device_id,
        &script_id,
        request.run_as,
        Source::Dashboard,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(RunView {
            run,
            source: Source::Dashboard.as_str().to_owned(),
            requested_by_you: true,
        }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListFilter {
    device_id: Option<String>,
}

/// Users see their own runs and deliveries, or everyone's with `audit.view`.
fn require_history(actor: &Authorized, permission: Permission) -> Result<bool, ApiError> {
    require_any(actor, &[permission, Permission::AuditView])?;
    Ok(actor.has(Permission::AuditView))
}

#[derive(Debug, Serialize)]
pub struct RunList {
    runs: Vec<RunView>,
}

/// `GET /v1/script-runs?device_id=`: recent runs, newest first, without
/// their output.
pub async fn list_runs(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(filter): QueryParameters<ListFilter>,
) -> Result<Json<RunList>, ApiError> {
    let everyone = require_history(&actor, Permission::ScriptsRun)?;
    let mut select = toolbox::run_select(false);
    select.limit(LIST_LIMIT);
    if let Some(device_id) = &filter.device_id {
        select.and_where(Expr::col(ScriptRuns::DeviceId).eq(parse_id(device_id, "device")?));
    }
    if !everyone {
        select.and_where(Expr::col(ScriptRuns::RequestedByUserId).eq(actor.user.id.as_str()));
    }
    let rows: Vec<RunRow> = state.database.fetch_all(&select).await?;
    let now = now_ms();
    let runs = rows
        .into_iter()
        .map(|row| row.into_view(&actor.user.id, now))
        .collect::<Result<_, _>>()?;
    Ok(Json(RunList { runs }))
}

/// `GET /v1/script-runs/{id}`, with the run's output.
pub async fn get_run(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<RunView>, ApiError> {
    let everyone = require_history(&actor, Permission::ScriptsRun)?;
    let id = parse_id(&id, "run")?;
    let row: Option<RunRow> = state
        .database
        .fetch_optional(
            &toolbox::run_select(true)
                .and_where(Expr::col(ScriptRuns::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    let row = row
        .filter(|row| everyone || row.requested_by_user_id == actor.user.id)
        .ok_or_else(|| ApiError::not_found("Run not found"))?;
    Ok(Json(row.into_view(&actor.user.id, now_ms())?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartDelivery {
    file_id: String,
    /// `user` saves to the signed-in user's Downloads folder, `public` to
    /// the shared Public Documents folder.
    destination: FileDeliveryDestination,
}

/// `POST /v1/agents/{id}/file-deliveries`: sends one of the user's library
/// files to the device.
pub async fn start_delivery(
    State(state): State<AppState>,
    actor: Authorized,
    Path(device_id): Path<String>,
    JsonBody(request): JsonBody<StartDelivery>,
) -> Result<(StatusCode, Json<DeliveryView>), ApiError> {
    let device_id = parse_id(&device_id, "device")?;
    let file_id = parse_id(&request.file_id, "file")?;
    let delivery = toolbox::start_file_delivery(
        &state,
        &toolbox_user(&actor),
        &device_id,
        &file_id,
        request.destination,
        Source::Dashboard,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(DeliveryView {
            delivery,
            destination: request.destination.as_str().to_owned(),
            requested_by_you: true,
        }),
    ))
}

#[derive(Debug, Serialize)]
pub struct DeliveryList {
    deliveries: Vec<DeliveryView>,
}

/// `GET /v1/file-deliveries?device_id=`: recent deliveries, newest first.
pub async fn list_deliveries(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(filter): QueryParameters<ListFilter>,
) -> Result<Json<DeliveryList>, ApiError> {
    let everyone = require_history(&actor, Permission::FilesDeliver)?;
    let mut select = toolbox::delivery_select();
    select.limit(LIST_LIMIT);
    if let Some(device_id) = &filter.device_id {
        select.and_where(Expr::col(FileDeliveries::DeviceId).eq(parse_id(device_id, "device")?));
    }
    if !everyone {
        select.and_where(Expr::col(FileDeliveries::RequestedByUserId).eq(actor.user.id.as_str()));
    }
    let rows: Vec<DeliveryRow> = state.database.fetch_all(&select).await?;
    let now = now_ms();
    let deliveries = rows
        .into_iter()
        .map(|row| row.into_view(&actor.user.id, now))
        .collect::<Result<_, _>>()?;
    Ok(Json(DeliveryList { deliveries }))
}

/// `GET /v1/file-deliveries/{id}`.
pub async fn get_delivery(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<DeliveryView>, ApiError> {
    let everyone = require_history(&actor, Permission::FilesDeliver)?;
    let id = parse_id(&id, "delivery")?;
    let row: Option<DeliveryRow> = state
        .database
        .fetch_optional(
            &toolbox::delivery_select()
                .and_where(Expr::col(FileDeliveries::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    let row = row
        .filter(|row| everyone || row.requested_by_user_id == actor.user.id)
        .ok_or_else(|| ApiError::not_found("Delivery not found"))?;
    Ok(Json(row.into_view(&actor.user.id, now_ms())?))
}
