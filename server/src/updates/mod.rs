//! Operating system updates: what each device has pending, scans and
//! installs, and the history of installs. See docs/patch-management.md.
//!
//! Agents keep their own scan schedule. When one connects, the server tells
//! it the instance's scan interval and the last scan it received from the
//! device, and tells every connected Agent again when the interval changes;
//! the Agent scans an interval after its last scan (spread out at random),
//! soon after a restart a scan said was pending, and within the hour when
//! its last scan is too old. Users' **Scan now** and scan jobs ask for a scan
//! at once. A scan replaces the device's pending list, keeping when each
//! update was first seen, unless it lists the same updates as the stored
//! list: then only its status is written. Installs are update runs, asked
//! for by a user on one device or as a device's part of an install job; the
//! Agent reports progress and then each update's outcome, and scans again.
//!
//! The engine's task marks runs whose Agent never reported as lost.
pub mod runs;
pub mod store;

use std::{collections::HashSet, sync::Arc, time::Duration};

use meshrmm_protocol_types::{
    AgentCommand, LastUpdateScan, UpdateInstallResult, UpdateInstallStatus, UpdateScan,
    UpdateScanSchedule,
};
use serde::Serialize;
use tokio::sync::broadcast;

use self::{runs::Run, store::DeviceSummary};
use crate::{
    db,
    http::AppState,
    time::{DAY_MS, now_ms},
};

/// How often the engine looks for lost runs.
const INTERVAL: Duration = Duration::from_secs(60);
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

    /// Whether an event published now would reach anyone: when not, there
    /// is no need to read what it would say. A socket subscribing just after
    /// is the same as one subscribing just after the event.
    fn heard(&self) -> bool {
        self.events.receiver_count() > 0
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

/// One pass of the engine at `now`: lost runs. Agents keep the scan
/// schedule themselves, so there is nothing else to do.
pub async fn pass(state: &AppState, now: i64) -> anyhow::Result<()> {
    sweep(state, now).await
}

/// Marks runs the Agent never reported as lost.
pub async fn sweep(state: &AppState, now: i64) -> anyhow::Result<()> {
    let lost = runs::sweep_lost(&mut &state.database, now).await?;
    for (run_id, device_id, kind) in &lost {
        tracing::info!(run_id, device_id, "an update run never reported");
        state.automation.device_changed(device_id);
        publish_run(state, run_id).await;
        publish_device(state, device_id).await;
        if *kind == runs::Kind::Install {
            crate::alerts::update_install_finished(state, run_id).await;
        }
    }
    Ok(())
}

/// The instance's scan interval in hours, or `None` when devices scan only
/// when asked or by policies.
fn interval_hours(hours: i64) -> Option<u32> {
    u32::try_from(hours).ok().filter(|hours| *hours > 0)
}

/// What a device's Agent is told when it connects: the instance's scan
/// interval, and the last scan the server received from the device, by which
/// the Agent schedules its own scans. Only reads.
pub async fn greeting(state: &AppState, device_id: &str) -> db::Result<AgentCommand> {
    let hours = state
        .settings
        .get(&state.database)
        .await?
        .update_scan_interval_hours;
    let status = store::status(&mut &state.database, device_id).await?;
    let now = now_ms();
    Ok(AgentCommand::SetUpdateScanSchedule {
        schedule: UpdateScanSchedule {
            interval_hours: interval_hours(hours),
            last_scan: status.received_at.map(|received| LastUpdateScan {
                received_ms_ago: now.saturating_sub(received).max(0),
                reboot_pending: status.reboot_pending,
            }),
        },
    })
}

/// The instance's scan interval was saved as `hours`: every connected Agent
/// is told once. Each keeps the last scan it knows of, so only the interval
/// is sent, without reading any device.
pub fn interval_changed(state: &AppState, hours: i64) {
    let told = state
        .agents
        .send_to_all(AgentCommand::SetUpdateScanSchedule {
            schedule: UpdateScanSchedule {
                interval_hours: interval_hours(hours),
                last_scan: None,
            },
        });
    tracing::info!(
        hours,
        told,
        "told the connected Agents the new update scan interval"
    );
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
    let stored = match store::store_scan(&state.database, device_id, scan, now).await {
        Ok(store::Stored::NotEnrolled) => return,
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not store a device's update scan");
            return;
        }
    };
    tracing::info!(device_id, error, ?stored, "stored a device's update scan");
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
        state.automation.device_changed(device_id);
    }
    // The website shows when the device last scanned, so it hears of every
    // scan.
    publish_device(state, device_id).await;
    // Pending updates and restarts are what some alert rules watch; when
    // neither changed, there is nothing new to judge.
    if !matches!(
        stored,
        store::Stored::Unchanged {
            reboot_changed: false
        }
    ) {
        state.alerts.device_reported(device_id);
    }
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
    state.automation.device_changed(device_id);
    if failed {
        crate::alerts::update_install_finished(state, run_id).await;
    }
}

/// Tells event sockets about a run.
pub async fn publish_run(state: &AppState, run_id: &str) {
    if !state.updates.heard() {
        return;
    }
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
    if !state.updates.heard() {
        return;
    }
    // The summary asks only whether this device is connected.
    let connected = if state.agents.is_connected(device_id) {
        HashSet::from([device_id.to_owned()])
    } else {
        HashSet::new()
    };
    match store::summaries(&mut &state.database, Some(device_id), &connected).await {
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
