//! Operating system updates: what each device has pending, scans and
//! installs, and the history of installs. See docs/patch-management.md.
//!
//! Agents scan when the server asks: when one connects with a scan older
//! than the instance's scan interval (or a restart pending, to see it
//! clear), every interval while it stays connected, when a user asks, and as
//! a device's part of a scan job. A scan replaces the device's pending list,
//! keeping when each update was first seen. Installs are update runs, asked
//! for by a user on one device or as a device's part of an install job; the
//! Agent reports progress and then each update's outcome, and scans again.
//!
//! The engine's task marks runs whose Agent never reported as lost, and asks
//! connected devices with old scans for a new one, a few at a time.
pub mod runs;
pub mod store;

use std::{sync::Arc, time::Duration};

use meshrmm_protocol_types::{AgentCommand, UpdateInstallResult, UpdateInstallStatus, UpdateScan};
use sea_query::{Expr, ExprTrait, Query};
use serde::Serialize;
use tokio::sync::broadcast;

use self::{runs::Run, store::DeviceSummary};
use crate::{
    db::{
        self,
        tables::{Agents, DeviceUpdateStatus, UpdateRuns},
    },
    http::AppState,
    settings,
    time::{DAY_MS, HOUR_MS, now_ms},
};

/// How often the engine looks for lost runs and old scans.
const INTERVAL: Duration = Duration::from_secs(60);
/// The most devices asked to scan on their schedule per pass, so a fleet's
/// package mirrors aren't all hit at once.
const SCHEDULED_SCANS_PER_PASS: usize = 50;
/// A device asked to scan on its schedule isn't asked again for this long,
/// in case its Agent doesn't answer.
const SCHEDULED_SCAN_RETRY_MS: i64 = HOUR_MS;
/// Changes a subscriber may fall behind by before it misses some.
const EVENT_BACKLOG: usize = 256;

/// What the website's event socket sends about updates, to users who may
/// see them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpdatesEvent {
    /// A device's update status, pending updates or runs changed.
    DeviceUpdates { device: Box<DeviceSummary> },
    /// A run was created, reported progress or finished.
    UpdateRun { run: Box<Run> },
}

#[derive(Debug, Clone)]
pub struct Engine {
    events: broadcast::Sender<Arc<UpdatesEvent>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            events: broadcast::channel(EVENT_BACKLOG).0,
        }
    }
}

impl Engine {
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<UpdatesEvent>> {
        self.events.subscribe()
    }

    fn publish(&self, event: UpdatesEvent) {
        // Nobody listening is fine.
        let _ = self.events.send(Arc::new(event));
    }
}

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(error) = pass(&state, now_ms()).await {
                tracing::warn!(error = format!("{error:#}"), "update pass failed");
            }
        }
    })
}

/// One pass of the engine at `now`: lost runs, then scheduled scans.
pub async fn pass(state: &AppState, now: i64) -> anyhow::Result<()> {
    sweep(state, now).await?;
    scan_scheduled(state, now).await?;
    Ok(())
}

/// Marks runs the Agent never reported as lost.
pub async fn sweep(state: &AppState, now: i64) -> anyhow::Result<()> {
    let lost = runs::sweep_lost(&mut &state.database, now).await?;
    for (run_id, device_id, kind) in &lost {
        tracing::info!(run_id, device_id, "an update run never reported");
        publish_run(state, run_id).await;
        publish_device(state, device_id).await;
        if *kind == runs::Kind::Install {
            crate::alerts::update_install_finished(state, run_id).await;
        }
    }
    if !lost.is_empty() {
        state.automation.wake();
    }
    Ok(())
}

/// The instance's scan interval, or `None` when devices scan only when
/// asked or by policies.
async fn scan_interval_ms(state: &AppState) -> db::Result<Option<i64>> {
    let hours = settings::load(&mut &state.database)
        .await?
        .update_scan_interval_hours;
    Ok((hours > 0).then_some(hours * HOUR_MS))
}

/// Asks connected devices whose last scan is older than the scan interval
/// for a new one.
async fn scan_scheduled(state: &AppState, now: i64) -> anyhow::Result<()> {
    let Some(interval) = scan_interval_ms(state).await? else {
        return Ok(());
    };
    let connected = state.agents.connected();
    if connected.is_empty() {
        return Ok(());
    }
    let mut database = &state.database;
    let rows: Vec<(String, Option<i64>, Option<i64>)> = database
        .fetch_all(
            &Query::select()
                .column((Agents::Table, Agents::Id))
                .columns([
                    (DeviceUpdateStatus::Table, DeviceUpdateStatus::ReceivedAt),
                    (
                        DeviceUpdateStatus::Table,
                        DeviceUpdateStatus::ScanRequestedAt,
                    ),
                ])
                .from(Agents::Table)
                .left_join(
                    DeviceUpdateStatus::Table,
                    Expr::col((DeviceUpdateStatus::Table, DeviceUpdateStatus::DeviceId))
                        .equals((Agents::Table, Agents::Id)),
                )
                .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
                .and_where(
                    Expr::col((Agents::Table, Agents::Id)).not_in_subquery(
                        Query::select()
                            .column(UpdateRuns::DeviceId)
                            .from(UpdateRuns::Table)
                            .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                            .to_owned(),
                    ),
                )
                .to_owned(),
        )
        .await?;
    let due = rows
        .into_iter()
        .filter(|(id, received, requested)| {
            connected.contains(id)
                && received.is_none_or(|received| now - received > interval)
                && requested.is_none_or(|requested| now - requested > SCHEDULED_SCAN_RETRY_MS)
        })
        .map(|(id, _, _)| id)
        .take(SCHEDULED_SCANS_PER_PASS)
        .collect::<Vec<_>>();
    for device_id in due {
        store::scan_requested(&mut database, &device_id, now).await?;
        if state
            .agents
            .send(&device_id, AgentCommand::ScanUpdates { run_id: None })
        {
            tracing::debug!(device_id, "asked a device for its scheduled update scan");
        }
    }
    Ok(())
}

/// A device's Agent connected: whether to ask it for a scan, because its
/// last one is older than the scan interval, or because it had to restart
/// (to see the restart done). Notes the request.
pub async fn connected(state: &AppState, device_id: &str) -> db::Result<bool> {
    let mut database = &state.database;
    let status = store::status(&mut database, device_id).await?;
    let now = now_ms();
    let stale = match scan_interval_ms(state).await? {
        Some(interval) => status
            .received_at
            .is_none_or(|received| now - received > interval),
        None => false,
    };
    let scan = stale || status.reboot_pending;
    if scan {
        store::scan_requested(&mut database, device_id, now).await?;
    }
    Ok(scan)
}

/// Stores a scan the device's Agent reported, finishes the runs it answers
/// and tells the website.
pub async fn scan_reported(
    state: &AppState,
    device_id: &str,
    run_ids: Vec<String>,
    scan: UpdateScan,
) {
    let now = now_ms();
    let error = scan.error.clone();
    match store::store_scan(&state.database, device_id, scan, now).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not store a device's update scan");
            return;
        }
    }
    tracing::info!(device_id, error, "stored a device's update scan");
    let finished = match runs::finish_scans(
        &mut &state.database,
        device_id,
        &run_ids,
        error.as_deref(),
        now,
    )
    .await
    {
        Ok(finished) => finished,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not finish a device's update scans");
            Vec::new()
        }
    };
    for run_id in &finished {
        publish_run(state, run_id).await;
    }
    if !finished.is_empty() {
        state.automation.wake();
    }
    publish_device(state, device_id).await;
    // Pending updates and restarts are what some alert rules watch.
    state.alerts.wake();
}

/// Notes an install's progress and tells the website.
pub async fn progress_reported(
    state: &AppState,
    device_id: &str,
    run_id: &str,
    message: &str,
    percent: Option<u8>,
) {
    match runs::progress(&mut &state.database, device_id, run_id, message, percent).await {
        Ok(true) => publish_run(state, run_id).await,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(device_id, run_id, %error, "could not note an install's progress")
        }
    }
}

/// Records how an install went, tells the website, and checks a failed one
/// against the alert rules.
pub async fn install_reported(
    state: &AppState,
    device_id: &str,
    run_id: &str,
    result: UpdateInstallResult,
) {
    let failed = result.status != UpdateInstallStatus::Succeeded;
    let mut transaction = match state.database.begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::warn!(device_id, run_id, %error, "could not record an install");
            return;
        }
    };
    let recorded =
        match runs::finish_install(&mut transaction, device_id, run_id, result, now_ms()).await {
            Ok(recorded) => recorded,
            Err(error) => {
                tracing::warn!(device_id, run_id, %error, "could not record an install");
                return;
            }
        };
    if let Err(error) = transaction.commit().await {
        tracing::warn!(device_id, run_id, %error, "could not record an install");
        return;
    }
    if !recorded {
        return;
    }
    tracing::info!(device_id, run_id, failed, "recorded an update install");
    publish_run(state, run_id).await;
    publish_device(state, device_id).await;
    state.automation.wake();
    if failed {
        crate::alerts::update_install_finished(state, run_id).await;
    }
}

/// Tells event sockets about a run.
pub async fn publish_run(state: &AppState, run_id: &str) {
    match runs::get(&mut &state.database, run_id, false).await {
        Ok(Some(run)) => state
            .updates
            .publish(UpdatesEvent::UpdateRun { run: Box::new(run) }),
        Ok(None) => {}
        Err(error) => tracing::warn!(run_id, %error, "could not read an update run"),
    }
}

/// Tells event sockets about a device's updates.
pub async fn publish_device(state: &AppState, device_id: &str) {
    match store::summaries(
        &mut &state.database,
        Some(device_id),
        &state.agents.connected(),
    )
    .await
    {
        Ok(summaries) => {
            if let Some(device) = summaries.into_iter().next() {
                state.updates.publish(UpdatesEvent::DeviceUpdates {
                    device: Box::new(device),
                });
            }
        }
        Err(error) => tracing::warn!(device_id, %error, "could not summarize a device's updates"),
    }
}

/// The device was deleted: its status and pending updates go with it.
pub async fn device_removed(state: &AppState, device_id: &str) {
    if let Err(error) = store::forget(&state.database, device_id).await {
        tracing::warn!(device_id, %error, "could not remove a deleted device's updates");
    }
}

/// How long update runs are kept: installs, the update history, for a
/// year; scans like jobs.
pub const INSTALL_HISTORY_MS: i64 = 365 * DAY_MS;
pub const SCAN_HISTORY_MS: i64 = 30 * DAY_MS;
