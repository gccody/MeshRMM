//! Third-party applications: the library, each device's packages and the
//! fleet's outdated ones, and installs, updates and uninstalls on one
//! device. Seeing them and asking for scans needs `applications.view`;
//! installing, updating and uninstalling needs `applications.deploy`;
//! changing the library needs `applications.manage`, and adding or changing
//! a custom installer `scripts.run` as well. Runs on many devices
//! are jobs (`POST /v1/jobs` with `applications`). See
//! docs/application-deployment.md.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use meshrmm_protocol_types::ApplicationOperation;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agents::{self, parse_id},
    applications::{
        self, Start,
        jobs::{self, Request},
        library::{self, Application, ApplicationInput, Usage},
        policies,
        runs::{self, Action, Filter, NewRun, Run},
        store::{self, Matches},
    },
    audit::{self, Target as AuditTarget},
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    users::new_id,
};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 200;

fn application_target(id: &str) -> AuditTarget<'_> {
    AuditTarget {
        kind: "application",
        id,
    }
}

fn not_found() -> ApiError {
    ApiError::not_found("Application not found")
}

/// A library application as lists show it, with how many devices have it.
#[derive(Debug, Serialize)]
pub struct ApplicationView {
    #[serde(flatten)]
    application: Application,
    #[serde(flatten)]
    usage: Usage,
    /// The policies that install or update it.
    policies: Vec<String>,
}

async fn view(
    executor: &mut impl crate::db::Executor,
    application: Application,
) -> Result<ApplicationView, ApiError> {
    let usage = library::usage(executor, std::slice::from_ref(&application))
        .await?
        .remove(&application.id)
        .unwrap_or_default();
    let policies = policies::using_application(executor, &application.id).await?;
    Ok(ApplicationView {
        application,
        usage,
        policies,
    })
}

/// `GET /v1/applications`: the library, by name.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ApplicationsView)?;
    let mut database = &state.database;
    let applications = library::load(&mut database, None).await?;
    let mut usage = library::usage(&mut database, &applications).await?;
    let mut views = Vec::with_capacity(applications.len());
    for application in applications {
        let policies = policies::using_application(&mut database, &application.id).await?;
        views.push(ApplicationView {
            usage: usage.remove(&application.id).unwrap_or_default(),
            application,
            policies,
        });
    }
    Ok(Json(json!({ "applications": views })))
}

/// `GET /v1/applications/{id}`.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ApplicationView>, ApiError> {
    actor.require(Permission::ApplicationsView)?;
    let id = parse_id(&id, "application")?;
    let mut database = &state.database;
    let application = library::get(&mut database, &id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(view(&mut database, application).await?))
}

fn name_taken() -> ApiError {
    ApiError::conflict("another application in the library has this name")
        .with_code("application_name_taken")
}

/// Adding or changing a custom installer needs `scripts.run` too: it runs
/// its file and commands as SYSTEM or root, on every device a policy that
/// names the application installs it on.
fn require_installers(
    actor: &Authorized,
    valid: &library::Valid,
    old: Option<&Application>,
) -> Result<(), ApiError> {
    if valid.changes_installers(old) && !actor.has(Permission::ScriptsRun) {
        return Err(ApiError::forbidden(
            "adding or changing a custom installer needs the scripts.run permission too, as it runs as SYSTEM or root",
        )
        .with_code("permission_denied"));
    }
    Ok(())
}

fn audit_metadata(valid: &library::Valid) -> Value {
    json!({
        "name": valid.name,
        "pinned_version": valid.pinned_version,
        "sources": valid
            .sources
            .iter()
            .map(|source| json!({
                "source": source.source.as_str(),
                "platform": source.platform.as_str(),
                "package_id": source.package_id,
                "file_id": source.file_id,
                "install_arguments": source.install_arguments,
                "uninstall_command": source.uninstall_command,
                "detection": source.detection,
            }))
            .collect::<Vec<_>>(),
    })
}

/// `POST /v1/applications`: adds an application to the library.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<ApplicationInput>,
) -> Result<(StatusCode, Json<ApplicationView>), ApiError> {
    actor.require(Permission::ApplicationsManage)?;
    let valid = input.validate()?;
    require_installers(&actor, &valid, None)?;
    let id = new_id();
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    library::check_files(&mut transaction, &valid).await?;
    if library::name_taken(&mut transaction, &valid.name, None).await? {
        return Err(name_taken());
    }
    library::save(&mut transaction, &id, &valid, &actor.user.id, now, true).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "application.create",
        application_target(&id),
        audit_metadata(&valid),
    )
    .await?;
    let application = library::get(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let view = view(&mut transaction, application).await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// `PUT /v1/applications/{id}`: changes an application. Jobs already
/// started keep what they were started with.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<ApplicationInput>,
) -> Result<Json<ApplicationView>, ApiError> {
    actor.require(Permission::ApplicationsManage)?;
    let id = parse_id(&id, "application")?;
    let valid = input.validate()?;
    let mut transaction = state.database.begin().await?;
    let old = library::get(&mut transaction, &id)
        .await?
        .ok_or_else(not_found)?;
    require_installers(&actor, &valid, Some(&old))?;
    library::check_files(&mut transaction, &valid).await?;
    if library::name_taken(&mut transaction, &valid.name, Some(&id)).await? {
        return Err(name_taken());
    }
    if !library::save(
        &mut transaction,
        &id,
        &valid,
        &actor.user.id,
        now_ms(),
        false,
    )
    .await?
    {
        return Err(not_found());
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "application.edit",
        application_target(&id),
        audit_metadata(&valid),
    )
    .await?;
    let application = library::get(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let view = view(&mut transaction, application).await?;
    transaction.commit().await?;
    Ok(Json(view))
}

/// `DELETE /v1/applications/{id}`. Applications that policies name can't be
/// deleted.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::ApplicationsManage)?;
    let id = parse_id(&id, "application")?;
    let mut transaction = state.database.begin().await?;
    let application = library::get(&mut transaction, &id)
        .await?
        .ok_or_else(not_found)?;
    let used = policies::using_application(&mut transaction, &id).await?;
    if let Some(first) = used.first() {
        let message = if used.len() == 1 {
            format!(
                "The policy \u{201c}{first}\u{201d} uses this application. Change or delete it first."
            )
        } else {
            format!(
                "{} policies use this application, including \u{201c}{first}\u{201d}. Change or delete them first.",
                used.len()
            )
        };
        return Err(ApiError::conflict(message).with_code("application_in_use"));
    }
    if !library::delete(&mut transaction, &id).await? {
        return Err(not_found());
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "application.delete",
        application_target(&id),
        json!({ "name": application.name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The active device named by a path.
async fn device(state: &AppState, id: &str) -> Result<String, ApiError> {
    let id = parse_id(id, "device")?;
    if !agents::is_active(&mut &state.database, &id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    Ok(id)
}

fn running() -> ApiError {
    ApiError::conflict(
        "the device is already installing, updating or uninstalling applications; wait for it to finish",
    )
    .with_code("application_run_running")
}

fn offline() -> ApiError {
    ApiError::conflict("the device is offline; try again when it is online")
        .with_code("device_offline")
}

#[derive(Debug, Serialize)]
pub struct DevicePackages {
    device_id: String,
    /// Its last scan's package managers and times; null before one.
    status: Option<store::Status>,
    /// The scan the server is waiting for, if any.
    scanning: Option<Run>,
    /// The install, update or uninstall in progress, if any.
    running: Option<Run>,
    packages: Vec<store::Package>,
}

/// `GET /v1/agents/{id}/packages`: the device's package managers and the
/// packages they have installed, outdated first.
pub async fn device_packages(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<DevicePackages>, ApiError> {
    actor.require(Permission::ApplicationsView)?;
    let id = device(&state, &id).await?;
    let mut database = &state.database;
    let library = library::load(&mut database, None).await?;
    let packages = store::packages(&mut database, &id, &Matches::new(&library)).await?;
    let scanning = match runs::pending(&mut database, &id, true).await? {
        Some(run_id) => runs::get(&mut database, &run_id, false).await?,
        None => None,
    };
    let running = match runs::pending(&mut database, &id, false).await? {
        Some(run_id) => runs::get(&mut database, &run_id, false).await?,
        None => None,
    };
    Ok(Json(DevicePackages {
        status: store::status(&mut database, &id).await?,
        device_id: id,
        scanning,
        running,
        packages,
    }))
}

/// `POST /v1/agents/{id}/packages/scan`: asks the online Agent to scan now.
/// A scan already asked for is returned instead.
pub async fn scan(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Run>), ApiError> {
    actor.require(Permission::ApplicationsView)?;
    let id = device(&state, &id).await?;
    if !state.agents.is_connected(&id) {
        return Err(offline());
    }
    let mut database = &state.database;
    if let Some(run_id) = runs::pending(&mut database, &id, true).await?
        && let Some(run) = runs::get(&mut database, &run_id, false).await?
    {
        return Ok((StatusCode::ACCEPTED, Json(run)));
    }
    let run_id = new_id();
    let mut transaction = state.database.begin().await?;
    let inserted = runs::insert(
        &mut transaction,
        &NewRun {
            id: &run_id,
            device_id: &id,
            summary: "Scan for applications",
            operation: None,
            requested_by_user_id: &actor.user.id,
            policy_name: None,
            job_id: None,
            created_at: now_ms(),
        },
    )
    .await;
    match inserted {
        Ok(()) => {}
        // Another request asked for one since `pending` looked: that one.
        Err(error) if runs::is_already_pending(&error) => {
            transaction.rollback().await?;
            let run = match runs::pending(&mut database, &id, true).await? {
                Some(run_id) => runs::get(&mut database, &run_id, false).await?,
                None => None,
            };
            return run
                .map(|run| (StatusCode::ACCEPTED, Json(run)))
                .ok_or_else(ApiError::internal);
        }
        Err(error) => return Err(error.into()),
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        Action::Scan.audit_kind(),
        AuditTarget::device(&id),
        json!({ "run_id": run_id }),
    )
    .await?;
    transaction.commit().await?;
    send(&state, &id, &run_id, None).await?;
    let run = runs::get(&mut &state.database, &run_id, false)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

/// Hands a recorded run to the online Agent; a run it never got is removed.
async fn send(
    state: &AppState,
    device_id: &str,
    run_id: &str,
    operation: Option<&ApplicationOperation>,
) -> Result<(), ApiError> {
    if state
        .agents
        .send(device_id, runs::command(run_id, operation))
    {
        return Ok(());
    }
    runs::discard(&mut &state.database, run_id).await?;
    Err(offline())
}

/// `POST /v1/agents/{id}/application-runs`: asks the online Agent to
/// install, update or uninstall applications.
pub async fn start_run(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<Request>,
) -> Result<(StatusCode, Json<Run>), ApiError> {
    actor.require(Permission::ApplicationsDeploy)?;
    let id = device(&state, &id).await?;
    if !state.agents.is_connected(&id) {
        return Err(offline());
    }
    let mut transaction = state.database.begin().await?;
    if applications::busy(&mut transaction, &id).await? {
        return Err(running());
    }
    let spec = jobs::build(&mut transaction, request).await?;
    if let Some(reason) = spec.all_pinned() {
        return Err(jobs::pinned(reason));
    }
    let platform = applications::platform(&mut transaction, &id)
        .await?
        .ok_or_else(|| jobs::invalid("the device's operating system is unknown"))?;
    let installed = store::packages(&mut transaction, &id, &Matches::new(&[])).await?;
    let operation = spec
        .operation(platform, &installed)
        .map_err(|reason| jobs::invalid(reason).with_code("not_applicable"))?;
    if let Some(reason) = applications::unusable(&mut transaction, &id, &spec).await? {
        return Err(jobs::invalid(reason).with_code("not_applicable"));
    }
    let run_id = new_id();
    let command = applications::record_run(
        &mut transaction,
        &Start {
            run_id: &run_id,
            device_id: &id,
            requested_by_user_id: &actor.user.id,
            policy_name: None,
            job_id: None,
            now: now_ms(),
        },
        &spec,
    )
    .await
    // Another request started one since `busy` looked.
    .map_err(|error| {
        if runs::is_already_pending(&error) {
            running()
        } else {
            error.into()
        }
    })?;
    let mut metadata = spec.audit();
    metadata["run_id"] = run_id.as_str().into();
    audit::record(
        &mut transaction,
        &actor.actor(),
        spec.action.action().audit_kind(),
        AuditTarget::device(&id),
        metadata,
    )
    .await?;
    transaction.commit().await?;
    if command.is_some() {
        send(&state, &id, &run_id, Some(&operation)).await?;
    }
    let run = runs::get(&mut &state.database, &run_id, true)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFilter {
    device_id: Option<String>,
    action: Option<Action>,
    limit: Option<u64>,
}

/// `GET /v1/application-runs?device_id=&action=&limit=`: runs, newest
/// first, with each application's outcome.
pub async fn list_runs(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(filter): QueryParameters<RunFilter>,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ApplicationsView)?;
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
            action: filter.action,
        },
        limit,
    )
    .await?;
    Ok(Json(json!({ "runs": runs })))
}

/// `GET /v1/application-runs/{id}`: a run, with each application's output.
pub async fn get_run(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    actor.require(Permission::ApplicationsView)?;
    let id = parse_id(&id, "application run")?;
    runs::get(&mut &state.database, &id, true)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("Application run not found"))
}

/// `GET /v1/packages/outdated`: every outdated package in the fleet, with
/// the devices it is outdated on.
pub async fn outdated(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ApplicationsView)?;
    let mut database = &state.database;
    let library = library::load(&mut database, None).await?;
    let packages = store::outdated(&mut database, &Matches::new(&library)).await?;
    Ok(Json(json!({ "packages": packages })))
}

/// `GET /v1/agents/{id}/application-runs/{run_id}/files/{file_id}`: the
/// Agent downloads a custom installer of a run it was asked for, while the
/// run is pending.
pub async fn installer(
    State(state): State<AppState>,
    Path((device_id, run_id, file_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let agent = agents::authenticate(&state, &headers, &device_id).await?;
    let run_id = parse_id(&run_id, "run")?;
    let file_id = parse_id(&file_id, "file")?;
    let gone = || ApiError::not_found("no pending run installs this file");
    let operation = runs::pending_operation(&mut &state.database, &agent.device_id, &run_id)
        .await?
        .ok_or_else(gone)?;
    let ApplicationOperation::Install { items } = operation else {
        return Err(gone());
    };
    let named = items.iter().flat_map(|item| &item.sources).any(|source| {
        source
            .custom
            .as_ref()
            .is_some_and(|custom| custom.file_id == file_id)
    });
    if !named {
        return Err(gone());
    }
    super::agent::file_download(&state, &file_id)
        .await?
        .ok_or_else(|| ApiError::not_found("the file is no longer in the toolbox"))
}
