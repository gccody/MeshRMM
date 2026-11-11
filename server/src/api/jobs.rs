//! Jobs from the website: a toolbox script or file, an update scan or
//! install, or applications to install, update or uninstall, on many
//! devices at once.
//! Users see the jobs they started; users who manage automation see
//! policies' jobs; users who may read the audit log see everyone's.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State, rejection::QueryRejection},
    http::StatusCode,
};
use meshrmm_protocol_types::{
    FileDeliveryDestination, MAX_REBOOT_DELAY_MINUTES, RebootBehavior, RunAs, UpdateCategory,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::toolbox::toolbox_user;
use crate::{
    agents::parse_id,
    applications::jobs::RunAction,
    audit::{self, Target as AuditTarget},
    auth::Authorized,
    automation::jobs::{
        self, DEFAULT_WAIT_HOURS, DevicePage, Job, JobDevice, MAX_WAIT_HOURS, NewJob, Offline,
        Outcome, Payload, Target, TargetKind, Viewer,
    },
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    targets::{Choice, TagMatch},
    time::{HOUR_MS, now_ms},
    toolbox,
};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 200;

fn default_wait_hours() -> i64 {
    DEFAULT_WAIT_HOURS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdatesAction {
    Scan,
    Install,
}

/// An update scan, or an install of the pending updates in `categories` (or
/// every one), on each device.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatesJob {
    action: UpdatesAction,
    #[serde(default)]
    categories: Vec<UpdateCategory>,
    #[serde(default)]
    reboot: Option<RebootBehavior>,
    #[serde(default)]
    reboot_delay_minutes: Option<u32>,
}

/// A job as the website starts it: a script and the account it runs as, or
/// a file and where it goes; the devices; and what happens to offline ones.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartJob {
    #[serde(default)]
    script_id: Option<String>,
    #[serde(default)]
    run_as: Option<RunAs>,
    #[serde(default)]
    file_id: Option<String>,
    #[serde(default)]
    destination: Option<FileDeliveryDestination>,
    #[serde(default)]
    updates: Option<UpdatesJob>,
    /// Installs, updates or uninstalls applications.
    #[serde(default)]
    applications: Option<crate::applications::jobs::Request>,
    target: TargetKind,
    #[serde(default)]
    device_ids: Vec<String>,
    #[serde(default)]
    company_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
    #[serde(default)]
    tag_match: TagMatch,
    /// For `outdated`: the packages, as `source:id`, whose devices it runs
    /// on; none for the devices with any package outdated.
    #[serde(default)]
    packages: Vec<String>,
    offline: Offline,
    /// How long offline devices are waited for.
    #[serde(default = "default_wait_hours")]
    wait_hours: i64,
}

fn viewer(actor: &Authorized) -> Result<Viewer, ApiError> {
    if !Viewer::may_list(&actor.permissions) {
        return Err(actor
            .require(Permission::ScriptsRun)
            .expect_err("users without the permission are refused"));
    }
    Ok(Viewer::new(&actor.user.id, &actor.permissions))
}

/// `POST /v1/jobs`: runs one of the user's scripts, sends one of their
/// files, scans for or installs updates, or installs, updates or uninstalls
/// applications, on chosen devices, whole companies, the devices with chosen
/// tags, or every device; an update of applications also on the devices
/// where chosen packages, or any, are outdated as the job is created.
pub async fn start(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<StartJob>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let user = toolbox_user(&actor);
    let mut target = Target::from_request(
        request.target,
        Choice {
            device_ids: request.device_ids,
            company_ids: request.company_ids,
            tag_ids: request.tag_ids,
            tag_match: request.tag_match,
            packages: request.packages,
        },
    )
    .map_err(|error| error.with_code("invalid_job"))?;
    if target.kind().is_wide() {
        actor.require(Permission::JobsRunWide)?;
    }
    if request.offline == Offline::Wait && !(1..=MAX_WAIT_HOURS).contains(&request.wait_hours) {
        return Err(jobs::invalid(format!(
            "offline devices can be waited for 1 to {MAX_WAIT_HOURS} hours"
        )));
    }
    // Everything but the job's own rows is read before the transaction, so
    // SQLite's writer is held only while the job is inserted.
    let mut database = &state.database;
    let payload = match (
        request.script_id,
        request.file_id,
        request.updates,
        request.applications,
    ) {
        (None, None, None, Some(applications)) => {
            actor.require(Permission::ApplicationsDeploy)?;
            let spec = crate::applications::jobs::build(&mut database, applications).await?;
            if let Some(reason) = spec.all_pinned() {
                return Err(crate::applications::jobs::pinned(reason));
            }
            if let Target::Outdated { packages, except } = &mut target {
                // The devices with any package outdated leave out those
                // whose only outdated packages are held back.
                if packages.is_empty() {
                    *except = spec.held.iter().map(|held| held.package.clone()).collect();
                }
            }
            Payload::Applications(Box::new(spec))
        }
        (None, None, Some(updates), None) => match updates.action {
            UpdatesAction::Scan => {
                actor.require(Permission::UpdatesView)?;
                Payload::UpdateScan
            }
            UpdatesAction::Install => {
                actor.require(Permission::UpdatesInstall)?;
                let reboot_delay_minutes = updates
                    .reboot_delay_minutes
                    .unwrap_or(crate::automation::policies::DEFAULT_REBOOT_DELAY_MINUTES);
                if !(1..=MAX_REBOOT_DELAY_MINUTES).contains(&reboot_delay_minutes) {
                    return Err(jobs::invalid(format!(
                        "the restart can wait 1 to {MAX_REBOOT_DELAY_MINUTES} minutes"
                    )));
                }
                let mut categories = updates.categories;
                categories.sort();
                categories.dedup();
                Payload::UpdateInstall {
                    categories,
                    reboot: updates
                        .reboot
                        .ok_or_else(|| jobs::invalid("choose when the computers restart"))?,
                    reboot_delay_minutes,
                }
            }
        },
        (Some(script_id), None, None, None) => {
            user.require(Permission::ScriptsRun)?;
            let run_as = request
                .run_as
                .ok_or_else(|| jobs::invalid("choose the account the script runs as"))?;
            let script =
                toolbox::visible_script(&mut database, &user, &parse_id(&script_id, "script")?)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Script not found"))?;
            Payload::Script {
                language: script.language()?,
                script_id: script.id,
                name: script.name,
                body: script.body,
                timeout_seconds: script.timeout_seconds,
                run_as,
            }
        }
        (None, Some(file_id), None, None) => {
            user.require(Permission::FilesDeliver)?;
            let destination = request
                .destination
                .ok_or_else(|| jobs::invalid("choose where the file goes"))?;
            let file = toolbox::visible_file(&mut database, &user, &parse_id(&file_id, "file")?)
                .await?
                .ok_or_else(|| ApiError::not_found("File not found"))?;
            Payload::File {
                file_id: file.id,
                name: file.name,
                destination,
            }
        }
        _ => {
            return Err(jobs::invalid(
                "choose a script, a file, an update scan or install, or applications",
            ));
        }
    };
    if target.kind() == TargetKind::Outdated
        && !matches!(&payload, Payload::Applications(spec) if spec.action == RunAction::Update)
    {
        return Err(jobs::invalid(
            "only an update of applications runs where packages are outdated",
        ));
    }
    let plan = jobs::plan(
        &mut database,
        &state.agents,
        NewJob {
            payload,
            target,
            offline: request.offline,
            wait_ms: request.wait_hours * HOUR_MS,
            requested_by_user_id: user.user_id.clone(),
            policy: None,
            actor: user.actor.clone(),
        },
    )
    .await?;
    // In several short transactions for a job on many devices; it is shown
    // and started only once the last one ends, and removed if one fails.
    let created = jobs::create(&state, &plan, now_ms()).await?;
    state.automation.job_changed(&created.id);
    let detail = detail(&state, &actor, &created.id).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListFilter {
    policy_id: Option<String>,
    limit: Option<u64>,
}

/// `GET /v1/jobs?policy_id=&limit=`: recent jobs, newest first.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(filter): QueryParameters<ListFilter>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let viewer = viewer(&actor)?;
    let policy_id = filter
        .policy_id
        .as_deref()
        .map(|id| parse_id(id, "policy"))
        .transpose()?;
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let jobs: Vec<Job> = jobs::list(
        &mut &state.database,
        &viewer,
        policy_id.as_deref(),
        limit,
        now_ms(),
    )
    .await?;
    let views = jobs.iter().map(|job| viewer.view(job)).collect::<Vec<_>>();
    Ok(Json(json!({ "jobs": views })))
}

async fn detail(
    state: &AppState,
    actor: &Authorized,
    id: &str,
) -> Result<serde_json::Value, ApiError> {
    let (viewer, job) = visible(state, actor, id).await?;
    Ok(json!(viewer.view(&job)))
}

/// The job, with its counts, if the user may see it.
async fn visible(
    state: &AppState,
    actor: &Authorized,
    id: &str,
) -> Result<(Viewer, Job), ApiError> {
    let viewer = viewer(actor)?;
    let job = jobs::get(&mut &state.database, id, now_ms())
        .await?
        .filter(|job| viewer.sees(job))
        .ok_or_else(|| ApiError::not_found("Job not found"))?;
    Ok((viewer, job))
}

/// `GET /v1/jobs/{id}`: the job and its counts. Its devices are read a
/// page at a time from `GET /v1/jobs/{id}/devices`.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = parse_id(&id, "job")?;
    Ok(Json(detail(&state, &actor, &id).await?))
}

const DEFAULT_DEVICES_LIMIT: u64 = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicesParameters {
    /// Only the devices with this outcome, as the counts name it.
    outcome: Option<Outcome>,
    /// Only the devices after this one in the page's order: the last one
    /// of the page before.
    after: Option<String>,
    offset: Option<u64>,
    /// 0 for only how many match.
    limit: Option<u64>,
}

/// A page of a job's devices.
#[derive(Debug, Serialize)]
pub struct DevicesPage {
    devices: Vec<JobDevice>,
    /// Devices of the job with the outcome asked for, or all of them, on
    /// this page or not.
    matching: i64,
}

/// `GET /v1/jobs/{id}/devices?outcome=&after=&offset=&limit=`: a page of
/// the job's devices, those with an outcome or every one, in device ID
/// order, with how many match.
pub async fn devices(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    parameters: Result<QueryParameters<DevicesParameters>, QueryRejection>,
) -> Result<Json<DevicesPage>, ApiError> {
    let id = parse_id(&id, "job")?;
    // Refused, like any other mistake, as an error the website can show.
    let QueryParameters(parameters) =
        parameters.map_err(|rejection| ApiError::bad_request(rejection.body_text()))?;
    let (_, job) = visible(&state, &actor, &id).await?;
    let page = DevicePage {
        outcome: parameters.outcome,
        after: parameters
            .after
            .as_deref()
            .map(|after| parse_id(after, "device"))
            .transpose()?,
        offset: parameters.offset.unwrap_or(0),
        limit: parameters
            .limit
            .unwrap_or(DEFAULT_DEVICES_LIMIT)
            .min(MAX_LIMIT),
    };
    let devices = jobs::devices(&mut &state.database, &id, &page, now_ms()).await?;
    Ok(Json(DevicesPage {
        devices,
        // The counts the job was just read with: no second read of its
        // devices.
        matching: job.counts.of(page.outcome),
    }))
}

/// `POST /v1/jobs/{id}/cancel`: devices that haven't started don't. Runs
/// already on a device finish.
pub async fn cancel(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = parse_id(&id, "job")?;
    let viewer = viewer(&actor)?;
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    let job = jobs::get(&mut transaction, &id, now)
        .await?
        .filter(|job| viewer.sees(job))
        .ok_or_else(|| ApiError::not_found("Job not found"))?;
    if !viewer.may_cancel(&job) {
        return Err(ApiError::forbidden(
            "only the user who started a job, or for a policy's job users who manage automation, can cancel it",
        )
        .with_code("permission_denied"));
    }
    let cancelled = jobs::cancel(&mut transaction, &id, now).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "job.cancel",
        AuditTarget::job(&id),
        json!({ "item_name": job.item_name, "cancelled": cancelled }),
    )
    .await?;
    transaction.commit().await?;
    state.automation.job_changed(&id);
    Ok(Json(detail(&state, &actor, &id).await?))
}
