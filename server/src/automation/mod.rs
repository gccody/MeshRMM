//! Automation: jobs that run a script or send a file on many devices, and
//! policies that start jobs on a schedule or when devices enroll. See
//! docs/automation.md.
//!
//! The engine's task makes a pass every few seconds, every second while
//! devices are queued, and at once when woken: it fires the policies that are
//! due, misses the waiting devices of expired jobs, queues waiting devices
//! whose Agent connected, starts queued devices within the dispatcher's
//! limits, and then marks the jobs with nothing left to do as finished. The
//! website's event socket gets each job whose counts changed. Everything the
//! engine decides is in the database, so a restart picks up where it left
//! off.
pub mod dispatch;
pub mod jobs;
pub mod policies;
pub mod schedule;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Map, Value};
use tokio::sync::{Notify, broadcast};

use self::jobs::{Counts, Job};
use crate::{
    db::tables::{AutomationPolicyChannels, Jobs},
    http::AppState,
    notifications::{self, Notification},
    settings,
    time::now_ms,
};

/// How often the engine looks for due policies and connected devices when
/// nothing wakes it.
const IDLE_INTERVAL: Duration = Duration::from_secs(5);
/// How soon it makes another pass while devices are queued.
const BUSY_INTERVAL: Duration = Duration::from_secs(1);
/// Changes a subscriber may fall behind by before it misses some.
const EVENT_BACKLOG: usize = 256;
/// Devices a failure notification names.
const NOTIFIED_DEVICES: usize = 20;

#[derive(Debug, Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    wake: Notify,
    events: broadcast::Sender<Arc<Job>>,
    /// The counts last published for each unfinished job.
    published: Mutex<HashMap<String, Counts>>,
    /// One pass at a time.
    passing: tokio::sync::Mutex<()>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                wake: Notify::new(),
                events: broadcast::channel(EVENT_BACKLOG).0,
                published: Mutex::default(),
                passing: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

impl Engine {
    /// Makes a pass soon, such as after a job was created, a device
    /// connected or a run was reported.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Jobs whose counts changed. The event socket shows each to the users
    /// who may see it.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Job>> {
        self.inner.events.subscribe()
    }

    fn published(&self) -> std::sync::MutexGuard<'_, HashMap<String, Counts>> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let busy = match pass(&state, now_ms()).await {
                Ok(busy) => busy,
                Err(error) => {
                    tracing::warn!(error = format!("{error:#}"), "automation pass failed");
                    false
                }
            };
            let wait = if busy { BUSY_INTERVAL } else { IDLE_INTERVAL };
            tokio::select! {
                () = state.automation.inner.wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    })
}

/// One pass of the engine at `now`. Returns whether queued devices remain.
pub async fn pass(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let _one_at_a_time = state.automation.inner.passing.lock().await;
    policies::fire_due(state, now).await?;
    dispatch::expire(state, now).await?;
    dispatch::queue_connected(state, now).await?;
    let busy = dispatch::dispatch(state, now).await?;
    report(state, now).await?;
    Ok(busy)
}

/// Publishes the unfinished jobs whose counts changed, and finishes those
/// with nothing left to do, notifying a policy's channels of failures.
async fn report(state: &AppState, now: i64) -> anyhow::Result<()> {
    let mut database = &state.database;
    let ids = jobs::unfinished(&mut database).await?;
    let unfinished = jobs::by_ids(&mut database, &ids, now).await?;
    for mut job in unfinished {
        if job.counts.active() == 0 {
            let finished = database
                .execute(
                    &Query::update()
                        .table(Jobs::Table)
                        .value(Jobs::FinishedAt, now)
                        .and_where(Expr::col(Jobs::Id).eq(job.id.as_str()))
                        .and_where(Expr::col(Jobs::FinishedAt).is_null())
                        .to_owned(),
                )
                .await?;
            if finished == 1 {
                job.finished_at_unix_ms = Some(now);
                tracing::info!(job_id = job.id, counts = ?job.counts, "a job finished");
                if let Err(error) = notify_failures(state, &job, now).await {
                    tracing::warn!(
                        job_id = job.id,
                        error = format!("{error:#}"),
                        "could not notify a policy's channels of a job's failures"
                    );
                }
            }
        }
        let changed = {
            let mut published = state.automation.published();
            if job.finished_at_unix_ms.is_some() {
                published.remove(&job.id);
                true
            } else {
                published.insert(job.id.clone(), job.counts) != Some(job.counts)
            }
        };
        if changed {
            publish(state, job);
        }
    }
    let unfinished = ids.into_iter().collect::<std::collections::HashSet<_>>();
    state
        .automation
        .published()
        .retain(|id, _| unfinished.contains(id));
    Ok(())
}

/// Shows event sockets a job that was created or changed.
pub fn publish(state: &AppState, job: Job) {
    // Nobody listening is fine.
    let _ = state.automation.inner.events.send(Arc::new(job));
}

/// Tells the channels of the policy that started `job` that devices failed.
async fn notify_failures(state: &AppState, job: &Job, now: i64) -> anyhow::Result<()> {
    let Some(policy_id) = &job.policy_id else {
        return Ok(());
    };
    if job.counts.failures() == 0 {
        return Ok(());
    }
    let mut database = &state.database;
    let channels: Vec<(String,)> = database
        .fetch_all(
            &Query::select()
                .column(AutomationPolicyChannels::ChannelId)
                .from(AutomationPolicyChannels::Table)
                .and_where(Expr::col(AutomationPolicyChannels::PolicyId).eq(policy_id.as_str()))
                .to_owned(),
        )
        .await?;
    if channels.is_empty() {
        return Ok(());
    }
    let devices = jobs::devices(&mut database, &job.id, now).await?;
    let settings = settings::load(&mut database).await?;
    notifications::dispatch(
        state,
        channels.into_iter().map(|(id,)| id).collect(),
        failure_notification(
            job,
            &devices,
            &settings.instance_name,
            &state.config.public_origin(),
        ),
    );
    Ok(())
}

/// The email and webhook for a policy's job that finished with failures.
fn failure_notification(
    job: &Job,
    devices: &[jobs::JobDevice],
    instance_name: &str,
    origin: &str,
) -> Notification {
    let policy = job.policy_name.as_deref().unwrap_or("a policy");
    let counts = &job.counts;
    let summary = format!(
        "\u{201c}{policy}\u{201d}: {} of {} devices failed",
        counts.failures(),
        counts.total
    );
    let mut body = format!(
        "{instance_name} ran \u{201c}{}\u{201d} on {} for the policy \u{201c}{policy}\u{201d}.\n\n",
        job.item_name, job.target_label
    );
    body.push_str(&format!(
        "Succeeded: {}\nFailed: {}\nTimed out: {}\nNo result: {}\nSkipped: {}\nMissed: {}\n\n",
        counts.succeeded,
        counts.failed,
        counts.timed_out,
        counts.no_result,
        counts.skipped,
        counts.missed,
    ));
    let failed = devices
        .iter()
        .filter(|device| device.outcome.failure())
        .collect::<Vec<_>>();
    for device in failed.iter().take(NOTIFIED_DEVICES) {
        let name = device.device_name.as_deref().unwrap_or(&device.device_id);
        let why = match (device.exit_code, &device.detail) {
            (Some(code), _) if code != 0 => format!("exit code {code}"),
            (_, Some(detail)) => detail.clone(),
            _ => device.outcome.label().to_lowercase(),
        };
        body.push_str(&format!("- {name}: {why}\n"));
    }
    if failed.len() > NOTIFIED_DEVICES {
        body.push_str(&format!("- and {} more\n", failed.len() - NOTIFIED_DEVICES));
    }
    body.push_str(&format!("\nSee the job: {origin}/job?id={}\n", job.id));
    let mut data = Map::new();
    data.insert(
        "job".into(),
        serde_json::to_value(job).unwrap_or(Value::Null),
    );
    Notification {
        event: "job.failed",
        summary,
        body,
        data,
    }
}

/// A device enrolled: starts the policies for new devices that cover it.
pub async fn device_enrolled(state: &AppState, device_id: &str) {
    match policies::device_enrolled(state, device_id, now_ms()).await {
        Ok(0) => {}
        Ok(_) => state.automation.wake(),
        Err(error) => tracing::warn!(
            device_id,
            error = format!("{error:#}"),
            "could not start the automation policies for a new device"
        ),
    }
}

/// A device was deleted: it misses the jobs it hasn't started.
pub async fn device_removed(state: &AppState, device_id: &str) {
    match dispatch::device_removed(&mut &state.database, device_id, now_ms()).await {
        Ok(0) => {}
        Ok(_) => state.automation.wake(),
        Err(error) => tracing::warn!(
            device_id,
            %error,
            "could not cancel a deleted device's jobs"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{jobs::*, *};

    fn job(counts: Counts) -> Job {
        Job {
            id: "j".into(),
            kind: Kind::Script,
            item_id: "s".into(),
            item_name: "Clean up".into(),
            language: None,
            run_as: None,
            destination: None,
            categories: None,
            reboot: None,
            reboot_delay_minutes: None,
            target: TargetKind::All,
            target_label: "All devices".into(),
            offline: Offline::Skip,
            expires_at_unix_ms: 0,
            requested_by_user_id: "u".into(),
            requested_by: "Ada".into(),
            policy_id: Some("p".into()),
            policy_name: Some("Nightly".into()),
            created_at_unix_ms: 0,
            finished_at_unix_ms: Some(1),
            counts,
        }
    }

    fn device(name: &str, outcome: Outcome, exit_code: Option<i64>) -> JobDevice {
        JobDevice {
            device_id: name.to_lowercase(),
            device_name: Some(name.into()),
            platform: None,
            outcome,
            detail: None,
            run_id: None,
            delivery_id: None,
            update_run_id: None,
            wake_id: None,
            exit_code,
            path: None,
            started_at_unix_ms: None,
            completed_at_unix_ms: None,
        }
    }

    #[test]
    fn failure_notifications_name_the_failed_devices() {
        let mut counts = Counts::default();
        counts.add(Outcome::Succeeded, 1);
        counts.add(Outcome::Failed, 1);
        counts.add(Outcome::TimedOut, 1);
        let notification = failure_notification(
            &job(counts),
            &[
                device("A", Outcome::Succeeded, Some(0)),
                device("B", Outcome::Failed, Some(3)),
                device("C", Outcome::TimedOut, None),
            ],
            "Acme RMM",
            "https://rmm.example.com",
        );
        assert_eq!(notification.event, "job.failed");
        assert_eq!(
            notification.summary,
            "\u{201c}Nightly\u{201d}: 2 of 3 devices failed"
        );
        assert!(notification.body.contains("- B: exit code 3\n"));
        assert!(notification.body.contains("- C: timed out\n"));
        assert!(!notification.body.contains("- A"));
        assert!(
            notification
                .body
                .contains("https://rmm.example.com/job?id=j")
        );
        assert_eq!(notification.data["job"]["counts"]["failed"], 1);
    }
}
