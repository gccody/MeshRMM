//! Starting jobs' runs, deliveries and update runs on their devices, a few
//! at a time.
//!
//! Queued devices are started in the order their jobs were created, at most
//! [`DISPATCH_BATCH`] per pass, while fewer than [`MAX_IN_FLIGHT`] of all
//! jobs' runs and deliveries await a report, and while the device has fewer
//! than [`MAX_PER_DEVICE`] of its own. So a job on a thousand devices reaches
//! them over a minute or so instead of all at once, and its results trickle
//! in rather than arriving together. Waiting devices are queued when their
//! Agent connects, and missed when the job expires first. A wake job's
//! queued devices are offline ones: they are woken together, through the
//! relays the wake requests pick, and don't count against the limits.
use std::collections::{BTreeMap, HashMap, HashSet};

use meshrmm_protocol_types::{
    AgentCommand, FileDeliveryDestination, FileDeliveryRequest, RebootBehavior, RunAs,
    ScriptLanguage, ScriptRunRequest,
};
use sea_query::{Expr, ExprTrait, Order, Query};

use super::jobs::{DeviceState, Kind, Offline};
use crate::{
    db::{
        self, Executor,
        tables::{
            AutomationPolicyDevices, FileDeliveries, JobDevices, Jobs, ScriptRuns, UpdateRuns,
        },
    },
    http::AppState,
    time::SECOND_MS,
    toolbox::{self, DELIVERY_WINDOW_MS, NewDelivery, NewRun, RUN_REPORT_GRACE_MS, Source},
    updates::runs::{
        self as update_runs, INSTALL_REPORT_WINDOW_MS, Install, Kind as UpdateKind,
        SCAN_REPORT_WINDOW_MS,
    },
    users::new_id,
};

/// The most devices started per pass.
pub const DISPATCH_BATCH: u64 = 25;
/// The most jobs' runs and deliveries awaiting a report at once.
pub const MAX_IN_FLIGHT: i64 = 500;
/// The most of a device's job runs and deliveries awaiting a report at once,
/// well under the eight scripts an Agent runs at a time.
pub const MAX_PER_DEVICE: i64 = 2;

const EXPIRED: &str = "The device didn't connect before the job expired";
const DELETED: &str = "The device was deleted";
const FILE_GONE: &str = "The file was deleted from the toolbox before it was sent";

/// Misses the waiting devices of jobs that expired by `now`, and of devices
/// that were deleted. Returns how many it missed.
pub async fn expire(state: &AppState, now: i64) -> db::Result<u64> {
    let database = &state.database;
    let expired = database
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Missed.as_str())
                .value(JobDevices::Detail, EXPIRED)
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                .and_where(
                    Expr::col(JobDevices::JobId).in_subquery(
                        Query::select()
                            .column(Jobs::Id)
                            .from(Jobs::Table)
                            .and_where(Expr::col(Jobs::ExpiresAt).lte(now))
                            .and_where(Expr::col(Jobs::FinishedAt).is_null())
                            .to_owned(),
                    ),
                )
                .to_owned(),
        )
        .await?;
    let deleted = database
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Missed.as_str())
                .value(JobDevices::Detail, DELETED)
                .value(JobDevices::UpdatedAt, now)
                .and_where(
                    Expr::col(JobDevices::State)
                        .is_in([DeviceState::Waiting.as_str(), DeviceState::Queued.as_str()]),
                )
                .and_where(Expr::col(JobDevices::DeviceId).not_in_subquery(crate::agents::active()))
                .to_owned(),
        )
        .await?;
    Ok(expired + deleted)
}

/// Queues the waiting devices whose Agent is connected. Returns how many.
pub async fn queue_connected(state: &AppState, now: i64) -> db::Result<u64> {
    let database = &state.database;
    let waiting: Vec<(String,)> = database
        .fetch_all(
            &Query::select()
                .distinct()
                .column(JobDevices::DeviceId)
                .from(JobDevices::Table)
                .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                .to_owned(),
        )
        .await?;
    let connected = state.agents.connected();
    let online = waiting
        .into_iter()
        .map(|(id,)| id)
        .filter(|id| connected.contains(id))
        .collect::<Vec<_>>();
    let mut queued = 0;
    for chunk in online.chunks(500) {
        queued += database
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, DeviceState::Queued.as_str())
                    .value(JobDevices::UpdatedAt, now)
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                    .and_where(Expr::col(JobDevices::DeviceId).is_in(chunk.iter().cloned()))
                    .to_owned(),
            )
            .await?;
    }
    Ok(queued)
}

/// The SQL that is true of a job's run or delivery still awaiting a report.
fn awaiting_run(now: i64) -> sea_query::SimpleExpr {
    Expr::col(ScriptRuns::JobId)
        .is_not_null()
        .and(
            Expr::col(ScriptRuns::Status)
                .eq("pending")
                .and(Expr::cust(format!(
                    "created_at + timeout_seconds * {SECOND_MS} + {RUN_REPORT_GRACE_MS} >= {now}"
                ))),
        )
}

fn awaiting_delivery(now: i64) -> sea_query::SimpleExpr {
    Expr::col(FileDeliveries::JobId).is_not_null().and(
        Expr::col(FileDeliveries::Status)
            .eq("pending")
            .and(Expr::col(FileDeliveries::CreatedAt).gte(now - DELIVERY_WINDOW_MS)),
    )
}

fn awaiting_update_run(now: i64) -> sea_query::SimpleExpr {
    Expr::col(UpdateRuns::JobId)
        .is_not_null()
        .and(
            Expr::col(UpdateRuns::Status)
                .eq("pending")
                .and(Expr::cust(format!(
                    "created_at + CASE kind WHEN 'scan' THEN {SCAN_REPORT_WINDOW_MS} \
                 ELSE {INSTALL_REPORT_WINDOW_MS} END >= {now}"
                ))),
        )
}

/// How many of the jobs' runs, deliveries and update runs await a report,
/// in all and on each of `devices`.
async fn in_flight(
    state: &AppState,
    devices: &[String],
    now: i64,
) -> db::Result<(i64, HashMap<String, i64>)> {
    let database = &state.database;
    let mut total = 0;
    let mut per_device: HashMap<String, i64> = HashMap::new();
    let runs: Vec<(String, i64)> = database
        .fetch_all(
            &Query::select()
                .column(ScriptRuns::DeviceId)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(ScriptRuns::Table)
                .and_where(awaiting_run(now))
                .group_by_col(ScriptRuns::DeviceId)
                .to_owned(),
        )
        .await?;
    let deliveries: Vec<(String, i64)> = database
        .fetch_all(
            &Query::select()
                .column(FileDeliveries::DeviceId)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(FileDeliveries::Table)
                .and_where(awaiting_delivery(now))
                .group_by_col(FileDeliveries::DeviceId)
                .to_owned(),
        )
        .await?;
    let updates: Vec<(String, i64)> = database
        .fetch_all(
            &Query::select()
                .column(UpdateRuns::DeviceId)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(UpdateRuns::Table)
                .and_where(awaiting_update_run(now))
                .group_by_col(UpdateRuns::DeviceId)
                .to_owned(),
        )
        .await?;
    let wanted = devices.iter().collect::<HashSet<_>>();
    for (device_id, count) in runs.into_iter().chain(deliveries).chain(updates) {
        total += count;
        if wanted.contains(&device_id) {
            *per_device.entry(device_id).or_default() += count;
        }
    }
    Ok((total, per_device))
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct QueuedRow {
    job_id: String,
    device_id: String,
}

/// What the dispatcher needs of a job.
#[derive(Debug, Clone, sqlx::FromRow)]
struct JobRow {
    id: String,
    kind: String,
    item_id: String,
    item_name: String,
    language: Option<String>,
    body: Option<String>,
    timeout_seconds: Option<i64>,
    run_as: Option<String>,
    destination: Option<String>,
    update_categories: Option<String>,
    reboot: Option<String>,
    reboot_delay_minutes: Option<i64>,
    offline: String,
    expires_at: i64,
    requested_by_user_id: String,
    policy_name: Option<String>,
}

/// Starts queued devices' runs and deliveries, within the limits. Returns
/// whether queued devices remain that it could start soon.
pub async fn dispatch(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let database = &state.database;
    let queued: Vec<QueuedRow> = database
        .fetch_all(
            &Query::select()
                .columns([
                    (JobDevices::Table, JobDevices::JobId),
                    (JobDevices::Table, JobDevices::DeviceId),
                ])
                .from(JobDevices::Table)
                .inner_join(
                    Jobs::Table,
                    Expr::col((Jobs::Table, Jobs::Id))
                        .equals((JobDevices::Table, JobDevices::JobId)),
                )
                .and_where(
                    Expr::col((JobDevices::Table, JobDevices::State))
                        .eq(DeviceState::Queued.as_str()),
                )
                .order_by((Jobs::Table, Jobs::CreatedAt), Order::Asc)
                .order_by((JobDevices::Table, JobDevices::JobId), Order::Asc)
                .order_by((JobDevices::Table, JobDevices::DeviceId), Order::Asc)
                .limit(DISPATCH_BATCH * 4)
                .to_owned(),
        )
        .await?;
    if queued.is_empty() {
        return Ok(false);
    }
    let devices = queued
        .iter()
        .map(|row| row.device_id.clone())
        .collect::<Vec<_>>();
    let (mut total, mut per_device) = in_flight(state, &devices, now).await?;
    let job_ids = queued
        .iter()
        .map(|row| row.job_id.clone())
        .collect::<HashSet<_>>();
    let jobs: Vec<JobRow> = database
        .fetch_all(
            &Query::select()
                .columns([
                    Jobs::Id,
                    Jobs::Kind,
                    Jobs::ItemId,
                    Jobs::ItemName,
                    Jobs::Language,
                    Jobs::Body,
                    Jobs::TimeoutSeconds,
                    Jobs::RunAs,
                    Jobs::Destination,
                    Jobs::UpdateCategories,
                    Jobs::Reboot,
                    Jobs::RebootDelayMinutes,
                    Jobs::Offline,
                    Jobs::ExpiresAt,
                    Jobs::RequestedByUserId,
                    Jobs::PolicyName,
                ])
                .from(Jobs::Table)
                .and_where(Expr::col(Jobs::Id).is_in(job_ids))
                .to_owned(),
        )
        .await?;
    let jobs = jobs
        .into_iter()
        .map(|job| (job.id.clone(), job))
        .collect::<HashMap<_, _>>();
    let mut started = 0;
    let mut held_back = false;
    let mut full = false;
    let mut wakes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for row in &queued {
        if started >= DISPATCH_BATCH || total >= MAX_IN_FLIGHT {
            full = true;
            break;
        }
        let Some(job) = jobs.get(&row.job_id) else {
            continue;
        };
        if Kind::parse(&job.kind) == Some(Kind::Wake) {
            wakes
                .entry(&job.id)
                .or_default()
                .push(row.device_id.clone());
            started += 1;
            continue;
        }
        let busy = per_device.entry(row.device_id.clone()).or_default();
        if *busy >= MAX_PER_DEVICE {
            held_back = true;
            continue;
        }
        if start(state, job, &row.device_id, now).await? {
            started += 1;
            total += 1;
            *busy += 1;
        }
    }
    for (job_id, devices) in wakes {
        if let Some(job) = jobs.get(job_id) {
            start_wakes(state, job, &devices, now).await?;
        }
    }
    // Devices held back wait for their runs to finish; anything past the
    // batch is next.
    Ok(full || queued.len() as u64 >= DISPATCH_BATCH * 4 || (held_back && started > 0))
}

/// Wakes a wake job's queued devices that are still offline, and skips
/// those that came online meanwhile.
async fn start_wakes(
    state: &AppState,
    job: &JobRow,
    devices: &[String],
    now: i64,
) -> anyhow::Result<()> {
    let connected = state.agents.connected();
    let mut claimed = Vec::new();
    let mut transaction = state.database.begin().await?;
    for device_id in devices {
        let online = connected.contains(device_id);
        let (next, detail) = if online {
            (DeviceState::Skipped, Some("The device is online already"))
        } else {
            (DeviceState::Started, None)
        };
        let updated = transaction
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, next.as_str())
                    .value(JobDevices::Detail, detail)
                    .value(JobDevices::UpdatedAt, now)
                    .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                    .and_where(Expr::col(JobDevices::DeviceId).eq(device_id.as_str()))
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()))
                    .to_owned(),
            )
            .await?;
        if updated == 1 && !online {
            claimed.push(device_id.clone());
        }
    }
    transaction.commit().await?;
    if !claimed.is_empty() {
        crate::wake::start(
            state,
            &claimed,
            crate::wake::Requester {
                user_id: &job.requested_by_user_id,
                job_id: Some(&job.id),
                actor: None,
            },
        )
        .await?;
    }
    Ok(())
}

/// Starts the job on a queued device: records the run or delivery and
/// hands it to the Agent. A device that went offline waits again, or is
/// missed. Returns whether it started.
async fn start(state: &AppState, job: &JobRow, device_id: &str, now: i64) -> anyhow::Result<bool> {
    if !state.agents.is_connected(device_id) {
        unqueue(
            &mut &state.database,
            job,
            device_id,
            DeviceState::Queued,
            now,
        )
        .await?;
        return Ok(false);
    }
    let Some(kind) = Kind::parse(&job.kind) else {
        tracing::error!(job_id = job.id, "stored job is invalid");
        return Ok(false);
    };
    // A device installs one thing at a time; this waits for the install it
    // is running.
    if kind == Kind::UpdateInstall
        && update_runs::pending(&mut &state.database, device_id, UpdateKind::Install)
            .await?
            .is_some()
    {
        return Ok(false);
    }
    let id = new_id();
    let mut transaction = state.database.begin().await?;
    let command = match kind {
        Kind::Script => {
            let (Some(language), Some(body), Some(timeout_seconds), Some(run_as)) = (
                job.language.as_deref().and_then(ScriptLanguage::parse),
                &job.body,
                job.timeout_seconds,
                job.run_as.as_deref().and_then(RunAs::parse),
            ) else {
                tracing::error!(job_id = job.id, "stored script job is invalid");
                return Ok(false);
            };
            toolbox::insert_run(
                &mut transaction,
                NewRun {
                    id: &id,
                    device_id,
                    script_id: &job.item_id,
                    script_name: &job.item_name,
                    language,
                    requested_by_user_id: &job.requested_by_user_id,
                    source: Source::Job,
                    run_as,
                    timeout_seconds,
                    job_id: Some(&job.id),
                    created_at: now,
                },
            )
            .await?;
            Some(AgentCommand::RunScript {
                run: ScriptRunRequest {
                    run_id: id.clone(),
                    language,
                    body: body.clone(),
                    run_as,
                    timeout_seconds: u32::try_from(timeout_seconds)?,
                },
            })
        }
        Kind::File => {
            let Some(destination) = job
                .destination
                .as_deref()
                .and_then(FileDeliveryDestination::parse)
            else {
                tracing::error!(job_id = job.id, "stored file job is invalid");
                return Ok(false);
            };
            let file = toolbox::file_by_id(&mut transaction, &job.item_id).await?;
            toolbox::insert_delivery(
                &mut transaction,
                NewDelivery {
                    id: &id,
                    device_id,
                    file_id: &job.item_id,
                    file_name: file.as_ref().map_or(&job.item_name, |file| &file.name),
                    size_bytes: file.as_ref().map_or(0, |file| file.size_bytes),
                    requested_by_user_id: &job.requested_by_user_id,
                    destination,
                    job_id: Some(&job.id),
                    created_at: now,
                },
            )
            .await?;
            match file {
                Some(file) => Some(AgentCommand::DeliverFile {
                    delivery: FileDeliveryRequest {
                        delivery_id: id.clone(),
                        file_name: file.name.clone(),
                        size_bytes: file.size_bytes(),
                        sha256: file.sha256.clone(),
                        destination,
                    },
                }),
                None => {
                    transaction
                        .execute(
                            &Query::update()
                                .table(FileDeliveries::Table)
                                .value(FileDeliveries::Status, "failed")
                                .value(FileDeliveries::Error, FILE_GONE)
                                .value(FileDeliveries::CompletedAt, now)
                                .and_where(Expr::col(FileDeliveries::Id).eq(id.as_str()))
                                .to_owned(),
                        )
                        .await?;
                    None
                }
            }
        }
        Kind::UpdateScan | Kind::UpdateInstall => {
            let (update_kind, install) = if kind == Kind::UpdateInstall {
                let Some(reboot) = job.reboot.as_deref().and_then(RebootBehavior::parse) else {
                    tracing::error!(job_id = job.id, "stored update job is invalid");
                    return Ok(false);
                };
                (
                    UpdateKind::Install,
                    Some(Install {
                        update_ids: Vec::new(),
                        categories: Install::parse_categories(job.update_categories.as_deref()),
                        reboot,
                        reboot_delay_minutes: u32::try_from(job.reboot_delay_minutes.unwrap_or(5))
                            .unwrap_or(5),
                    }),
                )
            } else {
                (UpdateKind::Scan, None)
            };
            update_runs::insert(
                &mut transaction,
                &update_runs::NewRun {
                    id: &id,
                    device_id,
                    kind: update_kind,
                    requested_by_user_id: &job.requested_by_user_id,
                    policy_name: job.policy_name.as_deref(),
                    job_id: Some(&job.id),
                    install: install.as_ref(),
                    created_at: now,
                },
            )
            .await?;
            Some(update_runs::command(&id, update_kind, install.as_ref()))
        }
        // Started together by `start_wakes`.
        Kind::Wake => return Ok(false),
    };
    let claimed = transaction
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Started.as_str())
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()))
                .to_owned(),
        )
        .await?;
    if claimed == 0 {
        // Cancelled meanwhile.
        transaction.rollback().await?;
        return Ok(false);
    }
    transaction.commit().await?;
    let Some(command) = command else {
        return Ok(true);
    };
    if state.agents.send(device_id, command) {
        if matches!(kind, Kind::UpdateScan | Kind::UpdateInstall) {
            crate::updates::publish_run(state, &id).await;
            crate::updates::publish_device(state, device_id).await;
        }
        return Ok(true);
    }
    // The Agent went away before it got it: the run never existed.
    let mut transaction = state.database.begin().await?;
    match kind {
        Kind::Script => {
            transaction
                .execute(
                    &Query::delete()
                        .from_table(ScriptRuns::Table)
                        .and_where(Expr::col(ScriptRuns::Id).eq(id.as_str()))
                        .and_where(Expr::col(ScriptRuns::Status).eq("pending"))
                        .to_owned(),
                )
                .await?;
        }
        Kind::File => {
            transaction
                .execute(
                    &Query::delete()
                        .from_table(FileDeliveries::Table)
                        .and_where(Expr::col(FileDeliveries::Id).eq(id.as_str()))
                        .and_where(Expr::col(FileDeliveries::Status).eq("pending"))
                        .to_owned(),
                )
                .await?;
        }
        Kind::UpdateScan | Kind::UpdateInstall => {
            update_runs::discard(&mut transaction, &id).await?;
        }
        Kind::Wake => {}
    }
    unqueue(&mut transaction, job, device_id, DeviceState::Started, now).await?;
    transaction.commit().await?;
    Ok(false)
}

/// A device in state `from` turned out to be offline: it waits again, or is
/// missed.
async fn unqueue(
    executor: &mut impl Executor,
    job: &JobRow,
    device_id: &str,
    from: DeviceState,
    now: i64,
) -> db::Result<()> {
    let waits = Offline::parse(&job.offline) == Some(Offline::Wait) && job.expires_at > now;
    let (next, detail) = if waits {
        (DeviceState::Waiting, None)
    } else {
        (DeviceState::Missed, Some("The device was offline"))
    };
    executor
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, next.as_str())
                .value(JobDevices::Detail, detail)
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(Expr::col(JobDevices::State).eq(from.as_str()))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Misses the device's queued and waiting parts of jobs, now it is deleted,
/// and takes it out of the policies that chose it.
pub async fn device_removed(
    executor: &mut impl Executor,
    device_id: &str,
    now: i64,
) -> db::Result<u64> {
    let missed = executor
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Missed.as_str())
                .value(JobDevices::Detail, DELETED)
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(
                    Expr::col(JobDevices::State)
                        .is_in([DeviceState::Waiting.as_str(), DeviceState::Queued.as_str()]),
                )
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(AutomationPolicyDevices::Table)
                .and_where(Expr::col(AutomationPolicyDevices::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(missed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_leave_room_on_each_device() {
        // An Agent refuses a ninth script; jobs leave room for runs by hand.
        const { assert!(MAX_PER_DEVICE < 8) };
        const { assert!(DISPATCH_BATCH as i64 <= MAX_IN_FLIGHT) };
    }
}
