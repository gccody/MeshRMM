//! The remote tools' API: a device's processes and services. Listing needs
//! `tools.processes.view` or `tools.services.view`, changes
//! `tools.processes.manage` or `tools.services.manage`. Every change is
//! audited on the device with its target and result; lists aren't. API
//! tokens may use every route, within their permissions. See
//! [`crate::tools`] and docs/remote-tools.md.
use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use meshrmm_protocol_types::{
    DevicePlatform, MAX_ENDED_PROCESSES, ProcessList, ProcessTarget, ProcessesEnded, ServiceAction,
    ServiceInfo, ServiceList, ServiceStartup, ToolOperation, ToolReply, valid_service_name,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agents::{self, parse_id},
    audit::{self, Target},
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    tools,
};

/// The most processes an audit event lists.
const AUDITED_PROCESSES: usize = 100;

/// The device `id` that `actor` may use with `permission`, and its
/// platform. Every route here authorizes its device through this, and only
/// this: a device that isn't enrolled, or that the actor may not reach, is
/// not found.
pub(super) async fn authorized_device(
    state: &AppState,
    actor: &Authorized,
    id: &str,
    permission: Permission,
) -> Result<(String, DevicePlatform), ApiError> {
    actor.require(permission)?;
    let device_id = parse_id(id, "device")?;
    // A device in a company the permission doesn't reach is as missing as one
    // that isn't enrolled.
    actor
        .require_device(&mut &state.database, permission, &device_id)
        .await?;
    let platform = agents::active_platform(&mut &state.database, &device_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    Ok((device_id, platform))
}

pub(super) fn unexpected() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "the device answered with something unexpected",
    )
}

/// Records `action` on the device; a failure only logs.
pub(super) async fn record(
    state: &AppState,
    actor: &Authorized,
    device_id: &str,
    action: &str,
    metadata: Value,
) {
    if let Err(error) = audit::record(
        &mut &state.database,
        &actor.actor(),
        action,
        Target::device(device_id),
        metadata,
    )
    .await
    {
        tracing::warn!(%error, device_id, action, "could not record a tool's change in the audit log");
    }
}

/// `metadata` with the result of a change the device refused or never
/// confirmed: `failed` with the device's code, or `unconfirmed` when it
/// didn't answer in time or disconnected, so may have made it.
pub(super) fn failure(mut metadata: Value, error: &ApiError) -> Value {
    metadata["result"] = if tools::unconfirmed(error) {
        "unconfirmed"
    } else {
        "failed"
    }
    .into();
    metadata["error"] = error.code().unwrap_or("failed").into();
    metadata
}

/// `GET /v1/agents/{id}/processes`: every process, with its CPU share since
/// the Agent's previous reading.
pub async fn list_processes(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ProcessList>, ApiError> {
    let (device_id, _) =
        authorized_device(&state, &actor, &id, Permission::ToolsProcessesView).await?;
    match tools::ask(&state, &device_id, ToolOperation::ListProcesses).await? {
        ToolReply::Processes(list) => Ok(Json(list)),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndProcesses {
    processes: Vec<ProcessTarget>,
    /// The processes are a tree the user chose to end, for the audit log.
    #[serde(default)]
    tree: bool,
}

/// The audit log's view of what ending processes did.
fn ended_metadata(ended: &ProcessesEnded) -> Value {
    json!({
        "processes": ended.ended.iter().take(AUDITED_PROCESSES).map(|process| json!({
            "pid": process.pid,
            "process": process.name,
        })).collect::<Vec<_>>(),
        "count": ended.ended.len(),
        "failed": ended.failed.iter().take(AUDITED_PROCESSES).map(|failure| json!({
            "pid": failure.pid,
            "process": failure.name,
            "code": failure.error.code.as_str(),
        })).collect::<Vec<_>>(),
    })
}

/// `POST /v1/agents/{id}/processes/end {processes: [{pid, started_at}], tree}`:
/// ends each process that still is the one listed, children first; says
/// which ended and why others didn't.
pub async fn end_processes(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<EndProcesses>,
) -> Result<Json<ProcessesEnded>, ApiError> {
    let (device_id, _) =
        authorized_device(&state, &actor, &id, Permission::ToolsProcessesManage).await?;
    let mut seen = HashSet::new();
    let processes = request
        .processes
        .into_iter()
        .filter(|process| seen.insert(process.pid))
        .collect::<Vec<_>>();
    if processes.is_empty() || processes.len() > MAX_ENDED_PROCESSES {
        return Err(ApiError::bad_request(format!(
            "choose 1 to {MAX_ENDED_PROCESSES} processes"
        )));
    }
    let asked = json!({
        "pids": processes.iter().take(AUDITED_PROCESSES).map(|process| process.pid).collect::<Vec<_>>(),
        "count": processes.len(),
        "tree": request.tree,
    });
    let answer = tools::ask(
        &state,
        &device_id,
        ToolOperation::EndProcesses { processes },
    )
    .await;
    let ended = match answer {
        Ok(ToolReply::ProcessesEnded(ended)) => ended,
        Ok(_) => {
            let error = unexpected();
            record(
                &state,
                &actor,
                &device_id,
                "process.end",
                failure(asked, &error),
            )
            .await;
            return Err(error);
        }
        Err(error) => {
            if !tools::never_sent(&error) {
                record(
                    &state,
                    &actor,
                    &device_id,
                    "process.end",
                    failure(asked, &error),
                )
                .await;
            }
            return Err(error);
        }
    };
    let mut metadata = ended_metadata(&ended);
    metadata["tree"] = request.tree.into();
    metadata["result"] = match (ended.ended.is_empty(), ended.failed.is_empty()) {
        (_, true) => "completed",
        (true, false) => "failed",
        (false, false) => "partial",
    }
    .into();
    record(&state, &actor, &device_id, "process.end", metadata).await;
    Ok(Json(ended))
}

/// `GET /v1/agents/{id}/services`: every service the device's service
/// manager knows.
pub async fn list_services(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ServiceList>, ApiError> {
    let (device_id, _) =
        authorized_device(&state, &actor, &id, Permission::ToolsServicesView).await?;
    match tools::ask(&state, &device_id, ToolOperation::ListServices).await? {
        ToolReply::Services(list) => Ok(Json(list)),
        _ => Err(unexpected()),
    }
}

fn check_service_name(platform: DevicePlatform, name: &str) -> Result<(), ApiError> {
    if valid_service_name(platform, name) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "that isn't a service name a {} device has",
            platform.label()
        ))
        .with_code("invalid"))
    }
}

/// Asks the device for a change to a service and records it as `action`,
/// with `asked` and the service's state afterwards, or why it failed.
async fn change_service(
    state: &AppState,
    actor: &Authorized,
    device_id: &str,
    operation: ToolOperation,
    action: &str,
    asked: Value,
) -> Result<Json<ServiceInfo>, ApiError> {
    let answer = tools::ask(state, device_id, operation).await;
    match answer {
        Ok(ToolReply::Service(service)) => {
            let mut metadata = asked;
            metadata["display_name"] = service.display_name.clone().into();
            metadata["state"] = serde_json::to_value(service.state).unwrap_or_default();
            if let Some(startup) = service.startup {
                metadata["startup_now"] = startup.as_str().into();
            }
            metadata["result"] = "completed".into();
            record(state, actor, device_id, action, metadata).await;
            Ok(Json(service))
        }
        Ok(_) => {
            let error = unexpected();
            record(state, actor, device_id, action, failure(asked, &error)).await;
            Err(error)
        }
        Err(error) => {
            if !tools::never_sent(&error) {
                record(state, actor, device_id, action, failure(asked, &error)).await;
            }
            Err(error)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlService {
    name: String,
    action: ServiceAction,
}

/// `POST /v1/agents/{id}/services/control {name, action}`: starts, stops or
/// restarts a service; returns it as it is afterwards. Audited as
/// `service.start`, `service.stop` or `service.restart`.
pub async fn control_service(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<ControlService>,
) -> Result<Json<ServiceInfo>, ApiError> {
    let (device_id, platform) =
        authorized_device(&state, &actor, &id, Permission::ToolsServicesManage).await?;
    check_service_name(platform, &request.name)?;
    let action = format!("service.{}", request.action.as_str());
    // Not `name`, which the audit log reads as the target's.
    let asked = json!({ "service": request.name });
    let operation = ToolOperation::ControlService {
        name: request.name,
        action: request.action,
    };
    change_service(&state, &actor, &device_id, operation, &action, asked).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetStartup {
    name: String,
    startup: ServiceStartup,
}

/// `POST /v1/agents/{id}/services/startup {name, startup}`: sets how a
/// service starts; returns it as it is afterwards. Audited as
/// `service.set_startup`.
pub async fn set_service_startup(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<SetStartup>,
) -> Result<Json<ServiceInfo>, ApiError> {
    let (device_id, platform) =
        authorized_device(&state, &actor, &id, Permission::ToolsServicesManage).await?;
    check_service_name(platform, &request.name)?;
    if request.startup == ServiceStartup::Static
        || (request.startup == ServiceStartup::AutomaticDelayed
            && platform != DevicePlatform::Windows)
    {
        return Err(ApiError::bad_request(format!(
            "a {} service can't be set to start that way",
            platform.label()
        ))
        .with_code("invalid"));
    }
    let asked = json!({ "service": request.name, "startup": request.startup.as_str() });
    let operation = ToolOperation::SetServiceStartup {
        name: request.name,
        startup: request.startup,
    };
    change_service(
        &state,
        &actor,
        &device_id,
        operation,
        "service.set_startup",
        asked,
    )
    .await
}
