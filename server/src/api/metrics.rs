//! `GET /v1/agents/{id}/metrics`: a device's resource usage over a range.
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};

use crate::{
    agents::{self, parse_id},
    auth::Authorized,
    http::{ApiError, AppState},
    rbac::Permission,
    realtime::metrics::{Point, Range, Reading},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsQuery {
    #[serde(default = "live")]
    range: Range,
}

fn live() -> Range {
    Range::Live
}

#[derive(Debug, Serialize)]
pub struct DeviceMetrics {
    range: Range,
    /// How long each point covers.
    step_ms: i64,
    /// The latest reading, with the device's volumes, while it is online.
    latest: Option<Reading>,
    /// Oldest first.
    points: Vec<Point>,
}

pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Result<Json<DeviceMetrics>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let Query(query) =
        query.map_err(|_| ApiError::bad_request("range must be live, hour, day or week"))?;
    let id = parse_id(&id, "device")?;
    if !agents::is_active(&mut &state.database, &id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    let points = state.metrics.history(&id, query.range).await?;
    Ok(Json(DeviceMetrics {
        range: query.range,
        step_ms: query.range.step_ms(),
        latest: state.metrics.latest(&id),
        points,
    }))
}
