//! Automation: jobs that run a script or send a file on many devices, and
//! policies that start jobs on a schedule or when devices enroll. See
//! docs/automation.md.
//!
//! The engine's task steps every few seconds, every second while queued
//! devices could start, and at once when woken. A step costs as much as what changed:
//! it queues the waiting parts of the devices whose Agent connected, fires
//! the policies that are due and starts queued devices within the
//! dispatcher's limits. At most once per [`SETTLE_SPACING_MS`] it also
//! misses the waiting devices of jobs that expired since it last looked and
//! recounts the jobs whose devices changed, marking those with nothing left
//! to do as finished. Every [`RECONCILE_INTERVAL_MS`], after a restart and
//! when a policy fires, it makes a full pass instead, which looks at every
//! waiting device and recounts every unfinished job, and first removes the
//! jobs whose creation stopped part-way: those still marked as being created
//! that this server isn't creating, which after a restart is all of them.
//! The website's event socket gets each job whose counts changed.
//! Everything the engine decides is in the database, so a restart picks up
//! where it left off.
pub mod dispatch;
pub mod jobs;
pub mod policies;
pub mod schedule;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Map, Value};
use tokio::sync::{Notify, broadcast};

use self::jobs::{Counts, Job, Outcome};
use crate::{
    db::tables::{AutomationPolicyChannels, Jobs},
    http::AppState,
    notifications::{self, Notification},
    settings,
    time::{MINUTE_MS, SECOND_MS, now_ms},
};

/// How often the engine looks for due policies and expired jobs when
/// nothing wakes it.
const IDLE_INTERVAL: Duration = Duration::from_secs(5);
/// How soon it steps again while queued devices could start: the last pass
/// stopped at the dispatcher's batch, not at a ceiling only a report frees.
const BUSY_INTERVAL: Duration = Duration::from_secs(1);
/// The least time between two recounts, however often the engine is woken:
/// while thousands of Agents reconnect, their devices start as they connect
/// and their jobs' counts follow once a second.
pub const SETTLE_SPACING_MS: i64 = SECOND_MS;
/// How often a full pass looks at every waiting device and unfinished job.
/// Steps act on every change announced to the engine, so this only catches
/// what wasn't announced: an Agent that connected while a policy was
/// creating its job, a job whose creation stopped part-way, a change by a
/// server that stopped before announcing it, and runs that passed their
/// report deadline unreported. Those wait at most a minute, and the full
/// pass, whose cost grows with the fleet, runs once a minute rather than
/// once per Agent.
pub const RECONCILE_INTERVAL_MS: i64 = MINUTE_MS;
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
    /// What changed since the engine last looked.
    pending: Mutex<Pending>,
    /// Full passes made and jobs recounted, which tests read to measure the
    /// engine's work.
    full_passes: AtomicU64,
    recounted: AtomicU64,
    /// Wake jobs' queued devices whose wake requests couldn't be recorded,
    /// by job, with when that first happened. Kept in memory: a restart
    /// gives them their retries again.
    wake_failures: Mutex<HashMap<String, HashMap<String, i64>>>,
    /// The jobs this server is creating: a job marked as being created that
    /// isn't one of them stopped part-way, and is removed.
    creating: Mutex<HashSet<String>>,
    /// Where creations pause after each transaction of devices, for tests.
    pause: Mutex<Option<Pause>>,
}

#[derive(Debug, Default)]
struct Pending {
    /// Devices whose Agent connected: their waiting parts are queued.
    connected: HashSet<String>,
    /// Jobs created or cancelled: their waiting parts on connected devices
    /// are queued, and they are recounted.
    changed_jobs: HashSet<String>,
    /// Devices whose part in a job changed: their unfinished jobs are
    /// recounted.
    devices: HashSet<String>,
    /// Jobs to recount.
    jobs: HashSet<String>,
    /// Whether a full pass was asked for.
    reconcile: bool,
    /// When the engine last recounted, and last made a full pass.
    settled_at: Option<i64>,
    reconciled_at: Option<i64>,
}

impl Pending {
    fn settle_due(&self, now: i64) -> bool {
        self.settled_at
            .is_none_or(|at| !(0..SETTLE_SPACING_MS).contains(&(now - at)))
    }

    fn reconcile_due(&self, now: i64) -> bool {
        let Some(at) = self.reconciled_at else {
            return true;
        };
        let since = now - at;
        let spacing = if self.reconcile {
            SETTLE_SPACING_MS
        } else {
            RECONCILE_INTERVAL_MS
        };
        !(0..spacing).contains(&since)
    }

    /// How long until spaced work is due, in milliseconds.
    fn next_due(&self, now: i64) -> Option<i64> {
        let reconciled_at = self.reconciled_at?;
        let mut due = reconciled_at
            + if self.reconcile {
                SETTLE_SPACING_MS
            } else {
                RECONCILE_INTERVAL_MS
            };
        if let Some(settled_at) = self.settled_at
            && !(self.devices.is_empty() && self.jobs.is_empty())
        {
            due = std::cmp::min(due, settled_at + SETTLE_SPACING_MS);
        }
        Some(std::cmp::max(due - now, 0))
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                wake: Notify::new(),
                events: broadcast::channel(EVENT_BACKLOG).0,
                published: Mutex::default(),
                passing: tokio::sync::Mutex::new(()),
                pending: Mutex::default(),
                full_passes: AtomicU64::new(0),
                recounted: AtomicU64::new(0),
                wake_failures: Mutex::default(),
                creating: Mutex::default(),
                pause: Mutex::default(),
            }),
        }
    }
}

impl Engine {
    /// Steps soon: fires the policies that are due and starts queued
    /// devices, such as after a policy was saved.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// The device's Agent connected: its waiting parts are queued and start
    /// soon. Agents that connect together are queued together.
    pub fn device_connected(&self, device_id: &str) {
        self.pending().connected.insert(device_id.to_owned());
        self.wake();
    }

    /// The device's part in a job changed, such as when its run or delivery
    /// reported: its unfinished jobs are recounted soon, and it may take its
    /// next run.
    pub fn device_changed(&self, device_id: &str) {
        self.pending().devices.insert(device_id.to_owned());
        self.wake();
    }

    /// A job was created or cancelled: its waiting devices whose Agent is
    /// connected are queued, and it is recounted soon.
    pub fn job_changed(&self, job_id: &str) {
        self.pending().changed_jobs.insert(job_id.to_owned());
        self.wake();
    }

    /// Something changed that could touch any job, such as a policy's run
    /// replacing its older jobs' waits: a full pass soon.
    pub fn reconcile(&self) {
        self.pending().reconcile = true;
        self.wake();
    }

    /// Recounts the job when the engine next does.
    fn recount(&self, job_id: &str) {
        self.pending().jobs.insert(job_id.to_owned());
    }

    /// How many full passes the engine has made.
    pub fn full_passes(&self) -> u64 {
        self.inner.full_passes.load(Ordering::Relaxed)
    }

    /// How many times it has recounted a job.
    pub fn jobs_recounted(&self) -> u64 {
        self.inner.recounted.load(Ordering::Relaxed)
    }

    /// Notes that the wake requests of the job's `devices` couldn't be
    /// recorded at `now`. Returns those that have failed for `retry_ms`.
    fn wakes_failed(
        &self,
        job_id: &str,
        devices: &[String],
        now: i64,
        retry_ms: i64,
    ) -> Vec<String> {
        let mut failures = self.wake_failures();
        let job = failures.entry(job_id.to_owned()).or_default();
        devices
            .iter()
            .filter(|device_id| now - *job.entry((*device_id).clone()).or_insert(now) >= retry_ms)
            .cloned()
            .collect()
    }

    /// Forgets the failures of the job's `devices`, whose wakes were
    /// recorded or have failed.
    fn wakes_settled(&self, job_id: &str, devices: &[String]) {
        let mut failures = self.wake_failures();
        let Some(job) = failures.get_mut(job_id) else {
            return;
        };
        for device_id in devices {
            job.remove(device_id);
        }
        if job.is_empty() {
            failures.remove(job_id);
        }
    }

    fn wake_failures(&self) -> std::sync::MutexGuard<'_, HashMap<String, HashMap<String, i64>>> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .wake_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

impl Engine {
    /// Notes that this server is creating the job `job_id` until the
    /// returned guard is dropped.
    pub(crate) fn hold_creation(&self, job_id: &str) -> Creating {
        self.creating().insert(job_id.to_owned());
        Creating {
            engine: self.clone(),
            job_id: job_id.to_owned(),
        }
    }

    /// Whether this server is creating the job.
    pub(crate) fn is_creating(&self, job_id: &str) -> bool {
        self.creating().contains(job_id)
    }

    fn creating(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .creating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Pauses every job created in several transactions after each
    /// transaction of its devices, until [`CreationPause::resume`], while
    /// the returned pause lives: tests look at a job half created with it.
    pub fn pause_creation(&self) -> CreationPause {
        let (sender, batches) = tokio::sync::mpsc::unbounded_channel();
        let resume = Arc::new(tokio::sync::Semaphore::new(0));
        *self.pause_slot() = Some(Pause {
            batches: sender,
            resume: Arc::clone(&resume),
        });
        CreationPause {
            engine: self.clone(),
            batches,
            resume,
        }
    }

    /// A transaction of `devices` of the job's devices was committed: waits
    /// for a test's pause, if there is one.
    pub(crate) async fn devices_written(&self, job_id: &str, devices: usize) {
        let Some(pause) = self.pause_slot().clone() else {
            return;
        };
        let batch = Batch {
            job_id: job_id.to_owned(),
            devices,
        };
        if pause.batches.send(batch).is_ok() {
            // Closed when the pause ends, which lets it go on.
            if let Ok(permit) = pause.resume.acquire().await {
                permit.forget();
            }
        }
    }

    fn pause_slot(&self) -> std::sync::MutexGuard<'_, Option<Pause>> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// This server creating a job, until it is dropped.
#[derive(Debug)]
pub struct Creating {
    engine: Engine,
    job_id: String,
}

impl Drop for Creating {
    fn drop(&mut self) {
        self.engine.creating().remove(&self.job_id);
    }
}

#[derive(Debug, Clone)]
struct Pause {
    batches: tokio::sync::mpsc::UnboundedSender<Batch>,
    resume: Arc<tokio::sync::Semaphore>,
}

/// A transaction of a job's devices, committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub job_id: String,
    /// The planned devices it covered: at most
    /// [`jobs::DEVICES_PER_TRANSACTION`]. Those deleted since planning were
    /// left out.
    pub devices: usize,
}

/// Job creations paused after each transaction of devices
/// ([`Engine::pause_creation`]). Dropping it lets them all go on.
#[derive(Debug)]
pub struct CreationPause {
    engine: Engine,
    batches: tokio::sync::mpsc::UnboundedReceiver<Batch>,
    resume: Arc<tokio::sync::Semaphore>,
}

impl CreationPause {
    /// The next transaction of devices committed; its creation waits for
    /// [`resume`](Self::resume).
    pub async fn next(&mut self) -> Batch {
        self.batches
            .recv()
            .await
            .expect("the engine keeps the pause")
    }

    /// Lets one paused creation write its next transaction.
    pub fn resume(&self) {
        self.resume.add_permits(1);
    }
}

impl Drop for CreationPause {
    fn drop(&mut self) {
        self.engine.pause_slot().take();
        self.resume.close();
    }
}

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let wait = match step(&state, now_ms()).await {
                Ok(wait) => wait,
                Err(error) => {
                    tracing::warn!(error = format!("{error:#}"), "automation pass failed");
                    IDLE_INTERVAL
                }
            };
            tokio::select! {
                () = state.automation.inner.wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    })
}

/// What the engine does at `now` when woken or when it has waited: the work
/// for what changed, or a full pass when one is due. Returns how soon it
/// wants to step again.
pub async fn step(state: &AppState, now: i64) -> anyhow::Result<Duration> {
    let engine = &state.automation;
    let _one_at_a_time = engine.inner.passing.lock().await;
    let (full, settle) = {
        let pending = engine.pending();
        (pending.reconcile_due(now), pending.settle_due(now))
    };
    let busy = if full {
        full_pass(state, now).await?
    } else {
        let busy = quick_pass(state, now).await?;
        if settle {
            settle_pass(state, now).await?;
        }
        busy
    };
    let wait = if busy { BUSY_INTERVAL } else { IDLE_INTERVAL };
    Ok(engine.pending().next_due(now).map_or(wait, |ms| {
        wait.min(Duration::from_millis(u64::try_from(ms).unwrap_or(0)))
    }))
}

/// A full pass of the engine at `now`, whatever was announced. Returns
/// whether queued devices could start straight away.
pub async fn pass(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let _one_at_a_time = state.automation.inner.passing.lock().await;
    full_pass(state, now).await
}

async fn full_pass(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let engine = &state.automation;
    engine.inner.full_passes.fetch_add(1, Ordering::Relaxed);
    {
        // It covers everything announced before it starts.
        let mut pending = engine.pending();
        pending.connected.clear();
        pending.changed_jobs.clear();
        pending.devices.clear();
        pending.jobs.clear();
        pending.reconcile = false;
        pending.settled_at = Some(now);
        pending.reconciled_at = Some(now);
    }
    let passed = async {
        // First, so a policy whose firing stopped part-way fires again now.
        jobs::discard_abandoned(state).await?;
        policies::fire_due(state, now).await?;
        dispatch::expire(state, None, now).await?;
        dispatch::miss_deleted(state, now).await?;
        dispatch::queue_connected(state, now).await?;
        let busy = dispatch::dispatch(state, now).await?;
        report(state, now, None).await?;
        anyhow::Ok(busy)
    }
    .await;
    if passed.is_err() {
        engine.pending().reconcile = true;
    }
    passed
}

/// Queues the waiting parts of the devices that connected and of the
/// connected devices of changed jobs, fires due policies and starts queued
/// devices. Returns whether queued devices could start straight away.
async fn quick_pass(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let engine = &state.automation;
    let (connected, changed_jobs) = {
        let mut pending = engine.pending();
        (
            std::mem::take(&mut pending.connected),
            std::mem::take(&mut pending.changed_jobs),
        )
    };
    let queued = async {
        let mut online = connected
            .iter()
            .filter(|device_id| state.agents.is_connected(device_id))
            .cloned()
            .collect::<Vec<_>>();
        if !changed_jobs.is_empty() {
            // An Agent that connected while its job was being created.
            let ids = changed_jobs.iter().cloned().collect::<Vec<_>>();
            online.extend(
                dispatch::waiting_devices(state, &ids)
                    .await?
                    .into_iter()
                    .filter(|device_id| state.agents.is_connected(device_id)),
            );
        }
        online.sort();
        online.dedup();
        dispatch::queue_devices(state, &online, now).await
    }
    .await;
    {
        let mut pending = engine.pending();
        match &queued {
            Ok(devices) => pending.devices.extend(devices.iter().cloned()),
            // Tried again on the next step.
            Err(_) => {
                pending.connected.extend(connected);
                pending.changed_jobs.extend(changed_jobs.iter().cloned());
            }
        }
        pending.jobs.extend(changed_jobs);
    }
    queued?;
    if policies::fire_due(state, now).await? {
        // A full pass counts the new job and the older ones whose waits it
        // replaced.
        engine.pending().reconcile = true;
    }
    dispatch::dispatch(state, now).await
}

/// Misses the waiting devices of the jobs that expired since the last
/// recount, and recounts the jobs that changed.
async fn settle_pass(state: &AppState, now: i64) -> anyhow::Result<()> {
    let engine = &state.automation;
    let (devices, mut ids, since) = {
        let mut pending = engine.pending();
        let since = pending.settled_at.replace(now);
        (
            std::mem::take(&mut pending.devices),
            std::mem::take(&mut pending.jobs),
            since,
        )
    };
    let settled = async {
        let devices = devices.iter().cloned().collect::<Vec<_>>();
        ids.extend(jobs::unfinished_of_devices(&mut &state.database, &devices).await?);
        ids.extend(dispatch::expire(state, since, now).await?);
        report(state, now, Some(&ids)).await
    }
    .await;
    if settled.is_err() {
        let mut pending = engine.pending();
        pending.devices.extend(devices);
        pending.jobs.extend(ids);
        pending.settled_at = since;
    }
    settled
}

/// Publishes the unfinished jobs whose counts changed, and finishes those
/// with nothing left to do, notifying a policy's channels of failures: the
/// jobs of `ids`, or every unfinished job.
async fn report(state: &AppState, now: i64, ids: Option<&HashSet<String>>) -> anyhow::Result<()> {
    let mut database = &state.database;
    let every = ids.is_none();
    let ids = match ids {
        Some(ids) if ids.is_empty() => return Ok(()),
        Some(ids) => ids.iter().cloned().collect(),
        None => jobs::unfinished(&mut database).await?,
    };
    let unfinished = jobs::by_ids(&mut database, &ids, now)
        .await?
        .into_iter()
        .filter(|job| job.finished_at_unix_ms.is_none())
        .collect::<Vec<_>>();
    state
        .automation
        .inner
        .recounted
        .fetch_add(unfinished.len() as u64, Ordering::Relaxed);
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
    if every {
        let unfinished = ids.into_iter().collect::<HashSet<_>>();
        state
            .automation
            .published()
            .retain(|id, _| unfinished.contains(id));
    }
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
    // Only the devices the notification names, a page of each failing
    // outcome, however many devices failed.
    let mut devices = Vec::new();
    for outcome in [Outcome::Failed, Outcome::TimedOut, Outcome::NoResult] {
        let limit = NOTIFIED_DEVICES.saturating_sub(devices.len());
        if limit == 0 {
            break;
        }
        let page = jobs::DevicePage {
            outcome: Some(outcome),
            limit: limit as u64,
            ..Default::default()
        };
        devices.extend(jobs::devices(&mut database, &job.id, &page, now).await?);
    }
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

/// The email and webhook for a policy's job that finished with failures,
/// naming the failed ones of `devices`; the counts say how many more failed.
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
    let named = failed.len().min(NOTIFIED_DEVICES) as i64;
    if counts.failures() > named {
        body.push_str(&format!("- and {} more\n", counts.failures() - named));
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
        Ok(_) => state.automation.device_changed(device_id),
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
        Ok(job_ids) if job_ids.is_empty() => {}
        Ok(job_ids) => {
            for job_id in &job_ids {
                state.automation.recount(job_id);
            }
            state.automation.wake();
        }
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
            maintenance_during_install: None,
            scheduled: true,
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
            applications: Vec::new(),
            device_id: name.to_lowercase(),
            device_name: Some(name.into()),
            platform: None,
            outcome,
            detail: None,
            run_id: None,
            delivery_id: None,
            update_run_id: None,
            wake_id: None,
            application_run_id: None,
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

    #[test]
    fn failure_notifications_count_the_failed_devices_they_dont_name() {
        let mut counts = Counts::default();
        counts.add(Outcome::Failed, 30);
        counts.add(Outcome::NoResult, 5);
        let named = (0..NOTIFIED_DEVICES)
            .map(|index| device(&format!("D{index}"), Outcome::Failed, Some(1)))
            .collect::<Vec<_>>();
        let notification =
            failure_notification(&job(counts), &named, "Acme RMM", "https://rmm.example.com");
        assert!(notification.body.contains(
            "- D19: exit code 1
"
        ));
        assert!(
            notification.body.contains(
                "- and 15 more
"
            ),
            "{}",
            notification.body
        );
    }
}
