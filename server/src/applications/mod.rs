//! Third-party applications: the library, what each device's package
//! managers have installed and outdated, and installs, updates and
//! uninstalls. See docs/application-deployment.md.
//!
//! Agents scan for applications whenever they scan for operating system
//! updates (see [`crate::updates`]), and when asked: by a user, and before
//! and after every run. A scan replaces the device's packages, keeping when
//! each was first found outdated. Runs are asked for by a user on one device
//! or as a device's part of a job; the Agent reports progress, then each
//! application's outcome. A device runs one at a time.
//!
//! The engine's task marks runs whose Agent never reported as lost, and
//! removes runs older than [`HISTORY_MS`].
pub mod jobs;
pub mod library;
pub mod policies;
pub mod runs;
pub mod store;
pub mod tables;

use std::time::{Duration, Instant};

use meshrmm_protocol_types::{
    AgentCommand, ApplicationRunResult, ApplicationScan, DevicePlatform, RunStatus,
};
use sea_query::{Expr, ExprTrait, Query};

use self::runs::{Action, NewRun};
use crate::{
    db::{Executor, tables::Agents},
    http::AppState,
    time::{DAY_MS, HOUR_MS, now_ms},
};

/// How often the engine looks for lost runs.
const INTERVAL: Duration = Duration::from_secs(60);
/// How often it removes old runs.
const PURGE_INTERVAL: Duration = Duration::from_millis(HOUR_MS as u64);
/// How long runs are kept: the application history, for a year.
pub const HISTORY_MS: i64 = 365 * DAY_MS;

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut purged: Option<Instant> = None;
        loop {
            interval.tick().await;
            let now = now_ms();
            if let Err(error) = sweep(&state, now).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not look for lost application runs"
                );
            }
            if purged.is_none_or(|at| at.elapsed() >= PURGE_INTERVAL) {
                match runs::purge(&mut &state.database, now - HISTORY_MS).await {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(removed, "removed old application runs");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "could not remove old application runs"),
                }
                purged = Some(Instant::now());
            }
        }
    })
}

/// Marks runs the Agent never reported as lost.
pub async fn sweep(state: &AppState, now: i64) -> anyhow::Result<()> {
    let lost = runs::sweep_lost(&mut &state.database, now).await?;
    for (run_id, device_id, action) in &lost {
        tracing::info!(run_id, device_id, "an application run never reported");
        if matches!(action, Action::Install | Action::Update) {
            crate::alerts::applications::run_finished(state, run_id).await;
        }
    }
    if !lost.is_empty() {
        state.automation.wake();
    }
    Ok(())
}

/// The device's Agent connected at `connected_at` and said which of the
/// server's scans and runs it still has: the device's other pending ones
/// from before then are lost at once, rather than when their report window
/// closes, so the device is free for the next.
pub async fn activity_reported(
    state: &AppState,
    device_id: &str,
    run_ids: &[String],
    connected_at: i64,
) {
    let lost = match runs::interrupted(
        &mut &state.database,
        device_id,
        run_ids,
        connected_at,
        now_ms(),
    )
    .await
    {
        Ok(lost) => lost,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not settle a device's interrupted application runs");
            return;
        }
    };
    for (run_id, action) in &lost {
        tracing::info!(run_id, device_id, "an application run was interrupted");
        if matches!(action, Action::Install | Action::Update) {
            crate::alerts::applications::run_finished(state, run_id).await;
        }
    }
    if !lost.is_empty() {
        state.automation.wake();
        state.alerts.wake();
    }
}

/// Stores a scan the device's Agent reported and finishes the scan runs it
/// answers.
pub async fn scan_reported(
    state: &AppState,
    device_id: &str,
    run_ids: Vec<String>,
    scan: ApplicationScan,
) {
    let now = now_ms();
    match store::store_scan(&state.database, device_id, scan, now).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not store a device's application scan");
            return;
        }
    }
    tracing::debug!(device_id, "stored a device's application scan");
    match runs::finish_scans(&mut &state.database, device_id, &run_ids, now).await {
        Ok(finished) if !finished.is_empty() => state.automation.wake(),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(device_id, %error, "could not finish a device's application scans");
        }
    }
    // Outdated packages are what some alert rules watch.
    state.alerts.wake();
}

/// Notes a run's progress.
pub async fn progress_reported(state: &AppState, device_id: &str, run_id: &str, message: &str) {
    if let Err(error) = runs::progress(&mut &state.database, device_id, run_id, message).await {
        tracing::warn!(device_id, run_id, %error, "could not note an application run's progress");
    }
}

/// Records how a run went, and checks a failed install or update against
/// the alert rules.
pub async fn result_reported(
    state: &AppState,
    device_id: &str,
    run_id: &str,
    result: ApplicationRunResult,
) {
    // Skipped applications weren't for the device: not failures.
    let failed = matches!(result.status, RunStatus::Failed | RunStatus::Partial);
    let mut transaction = match state.database.begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::warn!(device_id, run_id, %error, "could not record an application run");
            return;
        }
    };
    let recorded = match runs::finish(&mut transaction, device_id, run_id, result, now_ms()).await {
        Ok(recorded) => recorded,
        Err(error) => {
            tracing::warn!(device_id, run_id, %error, "could not record an application run");
            return;
        }
    };
    if let Err(error) = transaction.commit().await {
        tracing::warn!(device_id, run_id, %error, "could not record an application run");
        return;
    }
    if !recorded {
        return;
    }
    tracing::info!(device_id, run_id, failed, "recorded an application run");
    // A job's counts changed, and the device may take its next run.
    state.automation.wake();
    if failed {
        crate::alerts::applications::run_finished(state, run_id).await;
    }
}

/// The device's platform, if it is enrolled.
pub async fn platform(
    executor: &mut impl Executor,
    device_id: &str,
) -> crate::db::Result<Option<DevicePlatform>> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(Agents::Platform)
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(row.and_then(|(platform,)| DevicePlatform::parse(&platform)))
}

/// A run to start on one device, by a user or as its part of a job.
pub struct Start<'a> {
    pub run_id: &'a str,
    pub device_id: &'a str,
    pub requested_by_user_id: &'a str,
    pub policy_name: Option<&'a str>,
    pub job_id: Option<&'a str>,
    pub now: i64,
}

/// Records the run of `spec` on the device and returns the command for its
/// Agent; or records it failed, returning `None`, when there is nothing
/// for the device to do.
pub async fn record_run(
    executor: &mut impl Executor,
    start: &Start<'_>,
    spec: &jobs::Spec,
) -> crate::db::Result<Option<AgentCommand>> {
    let platform = platform(executor, start.device_id).await?;
    let installed = store::packages(executor, start.device_id, &store::Matches::new(&[])).await?;
    let operation = match platform {
        Some(platform) => spec.operation(platform, &installed),
        None => Err("The device's operating system is unknown".to_owned()),
    };
    let run = NewRun {
        id: start.run_id,
        device_id: start.device_id,
        summary: &spec.summary,
        operation: operation.as_ref().ok(),
        requested_by_user_id: start.requested_by_user_id,
        policy_name: start.policy_name,
        job_id: start.job_id,
        created_at: start.now,
    };
    runs::insert(executor, &run).await?;
    match operation {
        Ok(operation) => Ok(Some(runs::command(start.run_id, Some(&operation)))),
        Err(reason) => {
            runs::fail(executor, start.run_id, &reason, start.now).await?;
            Ok(None)
        }
    }
}

/// How a job's device starts.
pub enum JobStart {
    /// The run is recorded; the command for its Agent, if it has one.
    Run(Option<AgentCommand>),
    /// Nothing in the job is for the device, and why: no run.
    Skip(String),
}

/// Why the device can't use anything `spec` installs, from the package
/// managers its last scan found; `None` if it can, or it never scanned.
pub async fn unusable(
    executor: &mut impl Executor,
    device_id: &str,
    spec: &jobs::Spec,
) -> crate::db::Result<Option<String>> {
    let Some(platform) = platform(executor, device_id).await? else {
        return Ok(None);
    };
    Ok(store::managers(executor, device_id)
        .await?
        .and_then(|managers| spec.unusable_with(platform, &managers)))
}

/// Starts a job's run on a queued device, for the dispatcher: skipped when
/// nothing in it is for the device; a run without a command when the device
/// has nothing to do, which is recorded as a failed run.
pub async fn start_job_run(
    executor: &mut impl Executor,
    start: &Start<'_>,
) -> crate::db::Result<JobStart> {
    let spec = match start.job_id {
        Some(job_id) => jobs::load(executor, job_id).await?,
        None => None,
    };
    let Some(spec) = spec else {
        // Only a damaged database gets here; the device's part fails.
        let run = NewRun {
            id: start.run_id,
            device_id: start.device_id,
            summary: "Applications",
            operation: None,
            requested_by_user_id: start.requested_by_user_id,
            policy_name: start.policy_name,
            job_id: start.job_id,
            created_at: start.now,
        };
        runs::insert(executor, &run).await?;
        runs::fail(
            executor,
            start.run_id,
            "The job's applications are missing",
            start.now,
        )
        .await?;
        return Ok(JobStart::Run(None));
    };
    if let Some(reason) = unusable(executor, start.device_id, &spec).await? {
        return Ok(JobStart::Skip(reason));
    }
    Ok(JobStart::Run(record_run(executor, start, &spec).await?))
}

/// Whether the device is installing, updating or uninstalling.
pub async fn busy(executor: &mut impl Executor, device_id: &str) -> crate::db::Result<bool> {
    Ok(runs::pending(executor, device_id, false).await?.is_some())
}

/// The device was deleted: its scan goes with it; its runs stay in the
/// history.
pub async fn device_removed(state: &AppState, device_id: &str) {
    if let Err(error) = store::forget(&mut &state.database, device_id).await {
        tracing::warn!(device_id, %error, "could not remove a deleted device's applications");
    }
}
