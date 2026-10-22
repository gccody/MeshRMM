//! Jobs from the website: a toolbox script or file, or an update scan or
//! install, on many devices at once.
//! Users see the jobs they started; users who manage automation see
//! policies' jobs; users who may read the audit log see everyone's.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
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
    audit::{self, Target as AuditTarget},
    auth::Authorized,
    automation::jobs::{
        self, DEFAULT_WAIT_HOURS, Job, JobDevice, MAX_WAIT_HOURS, NewJob, Offline, Payload, Target,
        TargetKind, Viewer,
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
    target: TargetKind,
    #[serde(default)]
    device_ids: Vec<String>,
    #[serde(default)]
    company_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
    #[serde(default)]
    tag_match: TagMatch,
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
/// files, or scans for or installs updates, on chosen devices, whole
/// companies, the devices with chosen tags, or every device.
pub async fn start(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<StartJob>,
) -> Result<(StatusCode, Json<JobDetail>), ApiError> {
    let user = toolbox_user(&actor);
    let target = Target::from_request(
        request.target,
        Choice {
            device_ids: request.device_ids,
            company_ids: request.company_ids,
            tag_ids: request.tag_ids,
            tag_match: request.tag_match,
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
    let mut transaction = state.database.begin().await?;
    let payload = match (request.script_id, request.file_id, request.updates) {
        (None, None, Some(updates)) => match updates.action {
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
        (Some(script_id), None, None) => {
            user.require(Permission::ScriptsRun)?;
            let run_as = request
                .run_as
                .ok_or_else(|| jobs::invalid("choose the account the script runs as"))?;
            let script =
                toolbox::visible_script(&mut transaction, &user, &parse_id(&script_id, "script")?)
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
        (None, Some(file_id), None) => {
            user.require(Permission::FilesDeliver)?;
            let destination = request
                .destination
                .ok_or_else(|| jobs::invalid("choose where the file goes"))?;
            let file = toolbox::visible_file(&mut transaction, &user, &parse_id(&file_id, "file")?)
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
                "choose a script, a file, or an update scan or install",
            ));
        }
    };
    let now = now_ms();
    let created = jobs::create(
        &mut transaction,
        &NewJob {
            payload,
            target,
            offline: request.offline,
            wait_ms: request.wait_hours * HOUR_MS,
            requested_by_user_id: user.user_id.clone(),
            policy: None,
            actor: user.actor.clone(),
        },
        &state.agents.connected(),
        now,
    )
    .await?;
    transaction.commit().await?;
    state.automation.wake();
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

/// A job with each of its devices.
#[derive(Debug, Serialize)]
pub struct JobDetail {
    #[serde(flatten)]
    job: serde_json::Value,
    devices: Vec<JobDevice>,
}

async fn detail(state: &AppState, actor: &Authorized, id: &str) -> Result<JobDetail, ApiError> {
    let viewer = viewer(actor)?;
    let now = now_ms();
    let mut database = &state.database;
    let job = jobs::get(&mut database, id, now)
        .await?
        .filter(|job| viewer.sees(job))
        .ok_or_else(|| ApiError::not_found("Job not found"))?;
    let devices = jobs::devices(&mut database, id, now).await?;
    Ok(JobDetail {
        job: json!(viewer.view(&job)),
        devices,
    })
}

/// `GET /v1/jobs/{id}`, with its devices.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<JobDetail>, ApiError> {
    let id = parse_id(&id, "job")?;
    Ok(Json(detail(&state, &actor, &id).await?))
}

/// `POST /v1/jobs/{id}/cancel`: devices that haven't started don't. Runs
/// already on a device finish.
pub async fn cancel(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<JobDetail>, ApiError> {
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
    state.automation.wake();
    Ok(Json(detail(&state, &actor, &id).await?))
}
