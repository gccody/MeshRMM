//! Monitors' results and self-healing attempts. See docs/monitors.md.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::parse_id,
    alerts::store as alert_store,
    audit::{self, Target},
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    monitors::{self, DeviceMonitor, remediation, store::MonitorState},
    rbac::Permission,
};

/// The active device a path names, if `permission` reaches it.
async fn active_device(
    state: &AppState,
    actor: &Authorized,
    permission: Permission,
    id: &str,
) -> Result<String, ApiError> {
    let id = parse_id(id, "device")?;
    actor
        .require_device(&mut &state.database, permission, &id)
        .await?;
    Ok(id)
}

/// `GET /v1/agents/{id}/monitors`: the monitors that apply to the device,
/// with their latest results.
pub async fn device_monitors(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Vec<DeviceMonitor>>, ApiError> {
    actor.require(Permission::DevicesView)?;
    actor.require(Permission::AlertsView)?;
    let id = active_device(&state, &actor, Permission::AlertsView, &id).await?;
    actor
        .require_device(&mut &state.database, Permission::DevicesView, &id)
        .await?;
    Ok(Json(monitors::for_device(&state, &id).await?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    /// The rules whose monitors to run; every one when empty.
    #[serde(default)]
    rule_ids: Vec<String>,
}

/// `POST /v1/agents/{id}/monitors/check`: runs the device's monitors now.
/// It runs only what the device already checks on its schedule.
pub async fn check(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<CheckRequest>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::DevicesView)?;
    actor.require(Permission::AlertsRespond)?;
    let id = active_device(&state, &actor, Permission::AlertsRespond, &id).await?;
    actor
        .require_device(&mut &state.database, Permission::DevicesView, &id)
        .await?;
    let mut rule_ids = Vec::new();
    for rule_id in &request.rule_ids {
        let rule_id = parse_id(rule_id, "rule")?;
        if !monitors::applies(&state, &id, &rule_id).await? {
            return Err(ApiError::not_found("The device doesn't run that monitor"));
        }
        rule_ids.push(rule_id);
    }
    monitors::check_now(&state, &id, rule_ids.clone()).map_err(|unsent| {
        unsent.answer(ApiError::conflict("The device is offline").with_code("device_offline"))
    })?;
    audit::record(
        &mut &state.database,
        &actor.actor(),
        "monitor.check",
        Target::device(&id),
        json!({ "rule_ids": rule_ids }),
    )
    .await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatesParameters {
    rule: String,
}

#[derive(Debug, Serialize)]
pub struct RuleStates {
    states: Vec<MonitorState>,
}

/// `GET /v1/monitor-states?rule=`: a monitor's latest result on every device
/// that reported one, failing first.
pub async fn rule_states(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(parameters): QueryParameters<StatesParameters>,
) -> Result<Json<RuleStates>, ApiError> {
    actor.require(Permission::AlertsView)?;
    let rule_id = parse_id(&parameters.rule, "rule")?;
    Ok(Json(RuleStates {
        states: monitors::store::for_rule(
            &mut &state.database,
            &rule_id,
            &actor.companies(Permission::AlertsView),
        )
        .await?,
    }))
}

/// `GET /v1/alerts/{id}/remediations`: the alert's self-healing attempts.
pub async fn remediations(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Vec<remediation::Attempt>>, ApiError> {
    actor.require(Permission::AlertsView)?;
    let id = parse_id(&id, "alert")?;
    let mut database = &state.database;
    let alert = alert_store::get(&mut database, &id).await?;
    if !alert.is_some_and(|alert| actor.covers(Permission::AlertsView, alert.company_id.as_deref()))
    {
        return Err(ApiError::not_found("Alert not found"));
    }
    Ok(Json(remediation::attempts(&mut database, &id).await?))
}
