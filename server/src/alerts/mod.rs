//! Alerts: rules that watch devices, and the alerts they raise. See
//! docs/alerts.md.
//!
//! The engine's task judges the rules about a device's state (offline, disk
//! space, CPU and memory) every [`EVALUATION_INTERVAL`], and at once when
//! rules change, from which Agents are connected and the resource usage they
//! report. It raises an alert when a rule's condition holds for a device the
//! rule has no unresolved alert about, and resolves the alert when the
//! condition passes, or when the rule is turned off or stops covering the
//! device. Script failure rules are checked when a run finishes
//! ([`script_run_finished`]) or is found lost; their alerts stay until a
//! person resolves them. Raising an alert notifies the rule's channels, and
//! so does resolving one if the rule asks.
//!
//! Unresolved alerts live in the database, so a restart neither forgets them
//! nor notifies again. What the engine keeps in memory starts over: a device
//! counts as offline from the first evaluation after a start, so a restart
//! delays offline alerts instead of raising one for every device, and loads
//! need a full window of new readings.
pub mod applications;
mod evaluate;
pub mod rules;
pub mod store;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use meshrmm_protocol_types::{ScriptRunStatus, SystemMetrics};
use sea_query::{Expr, ExprTrait, Query};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{Notify, broadcast};

use self::{
    evaluate::{DeviceState, Tracker, UpdateFacts, Verdict, judge},
    rules::{Condition, Rule},
    store::{Alert, Counts, Raise, Resolution},
};
use crate::{
    audit::Actor,
    db::tables::{ScriptRuns, UpdateRuns},
    http::AppState,
    notifications::{self, Notification},
    realtime::Metrics,
    settings,
    time::{DAY_MS, now_ms},
    toolbox::run_status,
};

/// How often device-state rules are judged.
pub const EVALUATION_INTERVAL: Duration = Duration::from_secs(15);
/// How often the engine takes in new resource usage readings: as often as
/// Agents report.
const FEED_INTERVAL: Duration =
    Duration::from_secs(meshrmm_protocol_types::METRICS_INTERVAL_SECONDS);
/// How long resolved alerts are kept.
pub const RESOLVED_RETENTION_MS: i64 = 90 * DAY_MS;
/// Changes a subscriber may fall behind by before it gets the counts again.
const EVENT_BACKLOG: usize = 256;

/// What the website's event socket sends about alerts, to users who may see
/// them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertEvent {
    /// An alert was raised or changed.
    AlertUpsert { alert: Box<Alert>, counts: Counts },
    /// How many alerts are unresolved: when the socket opens, and after
    /// changes to many alerts at once, after which lists should be read again.
    AlertCounts { counts: Counts },
    /// A device's monitors reported: their latest results.
    MonitorUpdate {
        device_id: String,
        states: Vec<crate::monitors::store::MonitorState>,
    },
}

#[derive(Debug, Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    tracker: Mutex<Tracker>,
    events: broadcast::Sender<Arc<AlertEvent>>,
    wake: Notify,
    /// One evaluation at a time.
    evaluating: tokio::sync::Mutex<()>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                tracker: Mutex::default(),
                events: broadcast::channel(EVENT_BACKLOG).0,
                wake: Notify::new(),
                evaluating: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

impl Engine {
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AlertEvent>> {
        self.inner.events.subscribe()
    }

    /// Judges the rules soon, such as after they changed.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Takes in a reading the device's Agent reported at `at`.
    pub fn observe(&self, device_id: &str, at: i64, metrics: &SystemMetrics) {
        self.tracker().observe(device_id, at, metrics);
    }

    fn tracker(&self) -> std::sync::MutexGuard<'_, Tracker> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes in the readings reported after `sequence`, and returns the
    /// sequence to ask from next.
    fn feed(&self, metrics: &Metrics, sequence: u64) -> u64 {
        let (readings, next) = metrics.since(sequence);
        let mut tracker = self.tracker();
        for reading in readings {
            tracker.observe(&reading.device_id, reading.at, &reading.metrics);
        }
        next
    }

    pub(crate) fn publish(&self, event: AlertEvent) {
        // Nobody listening is fine.
        let _ = self.inner.events.send(Arc::new(event));
    }
}

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut feed = tokio::time::interval(FEED_INTERVAL);
        feed.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sequence = 0;
        let mut evaluated: Option<Instant> = None;
        loop {
            let woken = tokio::select! {
                _ = feed.tick() => false,
                () = state.alerts.inner.wake.notified() => true,
            };
            sequence = state.alerts.feed(&state.metrics, sequence);
            if woken || evaluated.is_none_or(|at| at.elapsed() >= EVALUATION_INTERVAL) {
                if let Err(error) = evaluate(&state, now_ms()).await {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "could not evaluate alert rules"
                    );
                }
                evaluated = Some(Instant::now());
            }
        }
    })
}

/// Judges every device-state rule for every device it covers at `now`,
/// raising and resolving alerts, and marks runs the Agent never reported
/// as lost.
pub async fn evaluate(state: &AppState, now: i64) -> anyhow::Result<()> {
    let _one_at_a_time = state.alerts.inner.evaluating.lock().await;
    sweep_lost_runs(state, now).await?;
    crate::monitors::remediation::tick(state, now).await?;
    let mut database = &state.database;
    let rules = rules::load(&mut database, None).await?;
    let devices = crate::targets::devices(&mut database, None).await?;
    let unresolved = store::unresolved(&mut database).await?;
    let connected = state.agents.connected();
    let updating = state.presence.updating_devices().await;
    let latest = connected
        .iter()
        .filter_map(|id| {
            state
                .metrics
                .latest(id)
                .map(|reading| (id.clone(), reading))
        })
        .collect::<HashMap<_, _>>();
    let watches_updates = rules.iter().any(|rule| {
        rule.enabled
            && matches!(
                rule.condition,
                Condition::UpdatesPending | Condition::RebootPending
            )
    });
    let (pending_updates, reboots, scanned) = if watches_updates {
        (
            crate::updates::store::pending_ages(&mut database).await?,
            crate::updates::store::reboots_pending(&mut database).await?,
            crate::updates::store::scanned_devices(&mut database).await?,
        )
    } else {
        Default::default()
    };
    let outdated_packages = applications::outdated(&mut database, &rules).await?;

    // Which rule and device pairs have a verdict, and the alerts it wants.
    let mut judged = HashSet::new();
    let mut wanted = Vec::new();
    {
        let mut tracker = state.alerts.tracker();
        let ids = devices
            .iter()
            .map(|device| device.id.clone())
            .collect::<Vec<_>>();
        tracker.presence(&ids, &connected, now);
        for rule in rules
            .iter()
            .filter(|rule| rule.enabled && rule.condition.clears_itself())
        {
            for covered in &devices {
                if !rule.covers(covered) {
                    continue;
                }
                let device_id = &covered.id;
                let device = DeviceState {
                    id: device_id,
                    connected: connected.contains(device_id),
                    updating: updating.contains(device_id),
                    latest: latest.get(device_id).map(|reading| &reading.metrics),
                    updates: scanned.contains(device_id).then(|| UpdateFacts {
                        pending: pending_updates.get(device_id).map_or(&[], Vec::as_slice),
                        reboot_pending_since: reboots.get(device_id).copied(),
                    }),
                };
                let verdict = if rule.condition == Condition::AppUpdatesPending {
                    applications::judge(rule, outdated_packages.get(device_id), now)
                } else {
                    judge(rule, device, &tracker, now)
                };
                if let Verdict::Known(breaches) = verdict {
                    judged.insert((rule.id.as_str(), device_id.as_str()));
                    wanted.extend(breaches.into_iter().map(|breach| (rule, device_id, breach)));
                }
            }
        }
    }

    let rules_by_id = rules
        .iter()
        .map(|rule| (rule.id.as_str(), rule))
        .collect::<HashMap<_, _>>();
    let devices_by_id = crate::targets::by_id(&devices);
    let wanted_keys = wanted
        .iter()
        .map(|(rule, device_id, breach)| {
            (
                rule.id.as_str(),
                device_id.as_str(),
                breach.subject.as_str(),
            )
        })
        .collect::<HashSet<_>>();
    // Monitor rules whose check devices no longer get, because it runs with
    // an owner who lost that access: their alerts can't update or clear.
    let mut unchecked = HashMap::new();
    for alert in &unresolved {
        let Some(rule) = rules_by_id.get(alert.rule_id.as_str()) else {
            continue;
        };
        let mut applies = rule.enabled
            && devices_by_id
                .get(alert.device_id.as_str())
                .is_some_and(|device| rule.covers(device));
        if applies && rule.monitor.is_some() && rule.condition.runs_with_owner_access() {
            let blocked = match unchecked.get(rule.id.as_str()) {
                Some(blocked) => *blocked,
                None => {
                    let blocked = crate::monitors::check_problem(&mut database, rule)
                        .await?
                        .is_some();
                    if blocked {
                        // Devices may still have the check: take it off.
                        state.monitors.wake();
                    }
                    unchecked.insert(rule.id.as_str(), blocked);
                    blocked
                }
            };
            applies = !blocked;
        }
        let key = (
            alert.rule_id.as_str(),
            alert.device_id.as_str(),
            alert.subject.as_str(),
        );
        let resolution = if !applies {
            Some(Resolution::NotApplicable)
        } else if judged.contains(&(key.0, key.1)) && !wanted_keys.contains(&key) {
            Some(Resolution::Cleared)
        } else {
            None
        };
        if let Some(resolution) = resolution {
            resolve(state, &alert.id, rule, resolution, None, now).await?;
        }
    }

    let open = unresolved
        .iter()
        .map(|alert| {
            (
                alert.rule_id.as_str(),
                alert.device_id.as_str(),
                alert.subject.as_str(),
            )
        })
        .collect::<HashSet<_>>();
    for (rule, device_id, breach) in &wanted {
        if open.contains(&(
            rule.id.as_str(),
            device_id.as_str(),
            breach.subject.as_str(),
        )) {
            continue;
        }
        let raise = Raise {
            rule_id: &rule.id,
            device_id,
            subject: &breach.subject,
            value: Some(breach.value),
            detail: &breach.detail,
            script_run_id: None,
            update_run_id: None,
            output: "",
        };
        if let Some(id) = store::raise(&mut database, &raise, now).await? {
            tracing::info!(
                rule_id = rule.id,
                device_id,
                alert_id = id,
                "raised an alert"
            );
            raised(state, rule, &id, now).await?;
        }
    }
    Ok(())
}

/// Announces an alert `rule` just raised: its channels hear of it unless the
/// rule waits for self-healing to fail, which starts now.
pub(crate) async fn raised(
    state: &AppState,
    rule: &Rule,
    id: &str,
    now: i64,
) -> anyhow::Result<()> {
    let notify = rule.notifies_at_once();
    if notify {
        store::mark_notified(&mut &state.database, id).await?;
    }
    // Self-healing that stops at once is told as one notice, which carries
    // the alert, rather than a raise and a failure that might arrive in
    // either order.
    let stopped = rule.remediation.is_some()
        && crate::monitors::remediation::begin(state, rule, id, now).await?;
    let notice = if stopped {
        Some(Notice::RemediationFailed)
    } else {
        notify.then_some(Notice::Raised)
    };
    announce(state, rule, id, notice).await
}

/// Resolves an unresolved alert of `rule`, as `actor` or (with `None`) the
/// server, and notifies if the rule asks and its channels heard of the
/// alert. An alert that clears just after a self-healing attempt succeeded
/// is `remediated` ([`crate::monitors::remediation::healed`]). Returns
/// whether it was unresolved.
pub async fn resolve(
    state: &AppState,
    id: &str,
    rule: &Rule,
    resolution: Resolution,
    actor: Option<&Actor>,
    now: i64,
) -> anyhow::Result<bool> {
    let mut database = &state.database;
    let Some(alert) = store::get(&mut database, id).await? else {
        return Ok(false);
    };
    let resolution = if resolution == Resolution::Cleared
        && crate::monitors::remediation::healed(&mut database, &alert, rule, now).await?
    {
        Resolution::Remediated
    } else {
        resolution
    };
    if !store::resolve(&mut database, id, resolution, actor, now).await? {
        return Ok(false);
    }
    if alert.remediation_state.is_some() {
        crate::monitors::remediation::resolved(state, id, resolution == Resolution::Remediated)
            .await?;
    }
    tracing::info!(
        alert_id = id,
        resolution = resolution.as_str(),
        "resolved an alert"
    );
    let notify = rule.notify_on_resolve && alert.notified;
    announce(state, rule, id, notify.then_some(Notice::Resolved)).await?;
    Ok(true)
}

/// What a rule's channels are told about an alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Notice {
    Raised,
    Resolved,
    /// Self-healing gave up on it, for a rule that waits for that.
    RemediationFailed,
}

/// Tells event sockets about the alert, and its rule's channels `notice`.
pub(crate) async fn announce(
    state: &AppState,
    rule: &Rule,
    id: &str,
    notice: Option<Notice>,
) -> anyhow::Result<()> {
    let mut database = &state.database;
    let Some(alert) = store::get(&mut database, id).await? else {
        return Ok(());
    };
    if let Some(notice) = notice
        && !rule.channel_ids.is_empty()
    {
        let settings = settings::load(&mut database).await?;
        let last = if notice == Notice::RemediationFailed {
            crate::monitors::remediation::last_attempt(&mut database, id).await?
        } else {
            None
        };
        notifications::dispatch(
            state,
            rule.channel_ids.clone(),
            notification(
                &alert,
                rule,
                notice,
                last.as_ref(),
                &settings.instance_name,
                &state.config.public_origin(),
            ),
        );
    }
    let counts = store::counts(&mut database).await?;
    state.alerts.publish(AlertEvent::AlertUpsert {
        alert: Box::new(alert),
        counts,
    });
    Ok(())
}

/// Tells event sockets the counts after a change to many alerts at once.
pub async fn announce_counts(state: &AppState) {
    match store::counts(&mut &state.database).await {
        Ok(counts) => state.alerts.publish(AlertEvent::AlertCounts { counts }),
        Err(error) => tracing::warn!(%error, "could not count unresolved alerts"),
    }
}

/// Publishes an alert a user changed.
pub async fn changed(state: &AppState, id: &str) -> anyhow::Result<()> {
    let mut database = &state.database;
    if let Some(alert) = store::get(&mut database, id).await? {
        let counts = store::counts(&mut database).await?;
        state.alerts.publish(AlertEvent::AlertUpsert {
            alert: Box::new(alert),
            counts,
        });
    }
    Ok(())
}

/// The device was deleted: its alerts go with it.
pub async fn device_removed(state: &AppState, device_id: &str) {
    state.alerts.tracker().forget(device_id);
    if let Err(error) =
        crate::monitors::store::delete_for_device(&mut &state.database, device_id).await
    {
        tracing::warn!(device_id, %error, "could not delete a deleted device's monitor results");
    }
    match store::delete_for_device(&mut &state.database, device_id).await {
        Ok(0) => {}
        Ok(_) => announce_counts(state).await,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not delete a deleted device's alerts")
        }
    }
}

/// The email and webhook for a raised or resolved alert. `last` is the
/// last self-healing attempt, for a notice that self-healing failed.
fn notification(
    alert: &Alert,
    rule: &Rule,
    notice: Notice,
    last: Option<&crate::monitors::remediation::Attempt>,
    instance_name: &str,
    origin: &str,
) -> Notification {
    let device = match &alert.company_name {
        Some(company) => format!("{} ({company})", alert.device_name),
        None => alert.device_name.clone(),
    };
    let summary = match notice {
        Notice::Resolved => format!("Resolved: {} on {}", alert.rule_name, alert.device_name),
        Notice::Raised => format!(
            "{}: {} on {}",
            rule.severity.label(),
            alert.rule_name,
            alert.device_name
        ),
        Notice::RemediationFailed => format!(
            "{}: {} on {} (self-healing failed)",
            rule.severity.label(),
            alert.rule_name,
            alert.device_name
        ),
    };
    let mut body = match notice {
        Notice::Resolved => format!(
            "The alert \"{}\" on {device} is resolved{}.\n\n",
            alert.rule_name,
            match alert.resolution.as_deref() {
                Some("cleared") => ": the condition passed",
                Some("remediated") => ": the condition passed after self-healing",
                Some("not_applicable") => ": the rule no longer covers the device",
                _ => "",
            }
        ),
        Notice::Raised => format!(
            "{instance_name} raised a {} alert on {device}.\n\n",
            rule.severity.as_str()
        ),
        Notice::RemediationFailed => match last.and_then(|last| last.skipped_because()) {
            Some(why) => format!(
                "{instance_name} raised a {} alert on {device}, and self-healing stopped. {why}.\n\n",
                rule.severity.as_str(),
            ),
            None => format!(
                "{instance_name} raised a {} alert on {device}, and self-healing didn't fix it after {}.\n\n",
                rule.severity.as_str(),
                rules::plural(alert.remediation_attempts, "attempt")
            ),
        },
    };
    body.push_str(&format!(
        "{}\nRule: {} ({})\nRaised: {}\n",
        alert.detail,
        alert.rule_name,
        rule.describe(),
        format_time(alert.raised_at)
    ));
    if let Some(remediation) = rule.describe_remediation() {
        body.push_str(&format!("Self-healing: {remediation}\n"));
    }
    if let Some(last) = last.filter(|_| notice == Notice::RemediationFailed) {
        body.push_str(&format!("Last attempt: {}\n", last.describe()));
    }
    if let Some(at) = alert.resolved_at {
        body.push_str(&format!("Resolved: {}\n", format_time(at)));
    }
    body.push_str(&format!(
        "\nSee the device's alerts: {origin}/alerts?device={}\n",
        alert.device_id
    ));
    let mut data = Map::new();
    data.insert(
        "alert".into(),
        serde_json::to_value(alert).unwrap_or(Value::Null),
    );
    if let Some(last) = last.filter(|_| notice == Notice::RemediationFailed) {
        if let Some(why) = last.skipped_because() {
            data.insert("remediation_stopped".into(), Value::String(why.to_owned()));
        }
        data.insert(
            "last_attempt".into(),
            serde_json::to_value(last).unwrap_or(Value::Null),
        );
    }
    Notification {
        event: match notice {
            Notice::Raised => "alert.raised",
            Notice::Resolved => "alert.resolved",
            Notice::RemediationFailed => "alert.remediation_failed",
        },
        summary,
        body,
        data,
    }
}

pub fn format_time(unix_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct FinishedRun {
    device_id: String,
    script_name: String,
    status: String,
    exit_code: Option<i64>,
    error: Option<String>,
}

/// Checks a run that just finished, failed to start or was found lost
/// against the script failure rules.
pub async fn script_run_finished(state: &AppState, run_id: &str) {
    crate::monitors::remediation::run_finished(state, run_id).await;
    if let Err(error) = check_run(state, run_id, now_ms()).await {
        tracing::warn!(
            run_id,
            error = format!("{error:#}"),
            "could not check a script run against the alert rules"
        );
    }
}

/// How a run failed: the alert's value (its exit code) and detail, and
/// whether only its exit code says so.
fn run_failure(run: &FinishedRun) -> Option<(Option<f64>, String, bool)> {
    let name = &run.script_name;
    match ScriptRunStatus::parse(&run.status)? {
        ScriptRunStatus::Failed => Some((
            None,
            match &run.error {
                Some(error) => format!("\u{201c}{name}\u{201d} could not run: {error}"),
                None => format!("\u{201c}{name}\u{201d} could not run"),
            },
            false,
        )),
        ScriptRunStatus::TimedOut => Some((
            None,
            format!("\u{201c}{name}\u{201d} timed out and was stopped"),
            false,
        )),
        ScriptRunStatus::Lost => Some((
            None,
            format!("\u{201c}{name}\u{201d} never reported a result"),
            false,
        )),
        ScriptRunStatus::Completed => match run.exit_code {
            Some(code) if code != 0 => Some((
                Some(code as f64),
                format!("\u{201c}{name}\u{201d} exited with code {code}"),
                true,
            )),
            _ => None,
        },
        ScriptRunStatus::Pending => None,
    }
}

async fn check_run(state: &AppState, run_id: &str, now: i64) -> anyhow::Result<()> {
    let mut database = &state.database;
    let Some(run): Option<FinishedRun> = database
        .fetch_optional(
            &Query::select()
                .columns([
                    ScriptRuns::DeviceId,
                    ScriptRuns::ScriptName,
                    ScriptRuns::Status,
                    ScriptRuns::ExitCode,
                    ScriptRuns::Error,
                ])
                .from(ScriptRuns::Table)
                .and_where(Expr::col(ScriptRuns::Id).eq(run_id))
                .to_owned(),
        )
        .await?
    else {
        return Ok(());
    };
    let Some((value, detail, by_exit_code)) = run_failure(&run) else {
        return Ok(());
    };
    let Some(device) = crate::targets::device(&mut database, &run.device_id).await? else {
        return Ok(());
    };
    for rule in rules::load(&mut database, None).await? {
        if !rule.enabled
            || rule.condition != Condition::ScriptFailed
            || !rule.covers(&device)
            || (by_exit_code && !rule.nonzero_exit_fails)
        {
            continue;
        }
        let raise = Raise {
            rule_id: &rule.id,
            device_id: &run.device_id,
            subject: "",
            value,
            detail: &detail,
            script_run_id: Some(run_id),
            update_run_id: None,
            output: "",
        };
        if let Some(id) = store::raise(&mut database, &raise, now).await? {
            tracing::info!(
                rule_id = rule.id,
                run_id,
                alert_id = id,
                "raised a script failure alert"
            );
            raised(state, &rule, &id, now).await?;
        } else if let Some(id) = store::recur(&mut database, &raise, now).await? {
            announce(state, &rule, &id, None).await?;
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct FinishedInstall {
    device_id: String,
    status: String,
    error: Option<String>,
    installed: i64,
    failed: i64,
}

/// How an install failed, in words, or `None` if it didn't.
fn install_failure(install: &FinishedInstall) -> Option<String> {
    let total = install.installed + install.failed;
    match install.status.as_str() {
        "partial" => Some(format!(
            "{} of {total} updates failed to install",
            install.failed
        )),
        "failed" => Some(match &install.error {
            Some(error) => format!("Installing updates failed: {error}"),
            None if total == 1 => "The update failed to install".to_owned(),
            None if total > 0 => format!("All {total} updates failed to install"),
            None => "Installing updates failed".to_owned(),
        }),
        "lost" => Some("An update install never reported how it went".to_owned()),
        _ => None,
    }
}

/// Checks an install that failed, partly or entirely, or was found lost
/// against the install failure rules.
pub async fn update_install_finished(state: &AppState, run_id: &str) {
    if let Err(error) = check_install(state, run_id, now_ms()).await {
        tracing::warn!(
            run_id,
            error = format!("{error:#}"),
            "could not check an update install against the alert rules"
        );
    }
}

async fn check_install(state: &AppState, run_id: &str, now: i64) -> anyhow::Result<()> {
    let mut database = &state.database;
    let Some(install): Option<FinishedInstall> = database
        .fetch_optional(
            &Query::select()
                .columns([
                    UpdateRuns::DeviceId,
                    UpdateRuns::Status,
                    UpdateRuns::Error,
                    UpdateRuns::Installed,
                    UpdateRuns::Failed,
                ])
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::Id).eq(run_id))
                .and_where(Expr::col(UpdateRuns::Kind).eq("install"))
                .to_owned(),
        )
        .await?
    else {
        return Ok(());
    };
    let Some(detail) = install_failure(&install) else {
        return Ok(());
    };
    let Some(device) = crate::targets::device(&mut database, &install.device_id).await? else {
        return Ok(());
    };
    for rule in rules::load(&mut database, None).await? {
        if !rule.enabled
            || rule.condition != Condition::UpdateInstallFailed
            || !rule.covers(&device)
        {
            continue;
        }
        let raise = Raise {
            rule_id: &rule.id,
            device_id: &install.device_id,
            subject: "",
            value: Some(install.failed as f64),
            detail: &detail,
            script_run_id: None,
            update_run_id: Some(run_id),
            output: "",
        };
        if let Some(id) = store::raise(&mut database, &raise, now).await? {
            tracing::info!(
                rule_id = rule.id,
                run_id,
                alert_id = id,
                "raised an update install failure alert"
            );
            raised(state, &rule, &id, now).await?;
        } else if let Some(id) = store::recur(&mut database, &raise, now).await? {
            announce(state, &rule, &id, None).await?;
        }
    }
    Ok(())
}

/// Marks runs whose Agent never reported back long after their timeout as
/// lost, and checks them against the script failure rules.
async fn sweep_lost_runs(state: &AppState, now: i64) -> anyhow::Result<()> {
    let database = &state.database;
    let pending: Vec<(String, i64, i64)> = database
        .fetch_all(
            &Query::select()
                .columns([
                    ScriptRuns::Id,
                    ScriptRuns::CreatedAt,
                    ScriptRuns::TimeoutSeconds,
                ])
                .from(ScriptRuns::Table)
                .and_where(Expr::col(ScriptRuns::Status).eq(ScriptRunStatus::Pending.as_str()))
                .to_owned(),
        )
        .await?;
    for (id, created_at, timeout_seconds) in pending {
        if run_status(ScriptRunStatus::Pending, created_at, timeout_seconds, now)
            != ScriptRunStatus::Lost
        {
            continue;
        }
        let marked = database
            .execute(
                &Query::update()
                    .table(ScriptRuns::Table)
                    .value(ScriptRuns::Status, ScriptRunStatus::Lost.as_str())
                    .value(ScriptRuns::CompletedAt, now)
                    .and_where(Expr::col(ScriptRuns::Id).eq(id.as_str()))
                    .and_where(Expr::col(ScriptRuns::Status).eq(ScriptRunStatus::Pending.as_str()))
                    .to_owned(),
            )
            .await?;
        if marked == 1 {
            // A self-healing script's attempt is lost with it.
            crate::monitors::remediation::run_finished(state, &id).await;
            check_run(state, &id, now).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, exit_code: Option<i64>, error: Option<&str>) -> FinishedRun {
        FinishedRun {
            device_id: "d".into(),
            script_name: "Backup".into(),
            status: status.into(),
            exit_code,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn runs_fail_by_status_or_exit_code() {
        assert_eq!(run_failure(&run("completed", Some(0), None)), None);
        assert_eq!(run_failure(&run("pending", None, None)), None);
        assert_eq!(
            run_failure(&run("completed", Some(3), None)),
            Some((
                Some(3.0),
                "\u{201c}Backup\u{201d} exited with code 3".into(),
                true
            ))
        );
        assert_eq!(
            run_failure(&run("failed", None, Some("no shell"))),
            Some((
                None,
                "\u{201c}Backup\u{201d} could not run: no shell".into(),
                false
            ))
        );
        assert!(run_failure(&run("timed_out", None, None)).is_some_and(|(_, _, code)| !code));
        assert!(run_failure(&run("lost", None, None)).is_some());
    }
}
