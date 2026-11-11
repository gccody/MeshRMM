//! Self-healing: what a rule does about the alerts it raises, until their
//! condition passes. See docs/monitors.md#self-healing.
//!
//! An alert of a rule with self-healing starts `waiting`, due at once. When
//! it is due, the engine makes an attempt: it restarts the service or ends
//! the process through the device's Agent, or runs a shared toolbox script
//! as an ordinary run (source `remediation`). Every attempt runs with the
//! access of the user who last saved the rule, who must still be enabled and
//! hold `alerts.manage` and `scripts.run`, and is recorded on the alert and
//! in the audit log. Once it finishes, the device checks the monitor again
//! (other rules are judged again), and the next attempt is due after the
//! rule's wait. After the last attempt and one more wait, an alert that is
//! still unresolved has `failed` self-healing; a rule that notifies only
//! then tells its channels. An alert that clears within the wait after an
//! attempt that succeeded resolves as `remediated`; one that clears
//! otherwise is `cleared`, as without self-healing.
//!
//! A problem self-healing keeps fixing keeps coming back: once it fixed a
//! rule's alerts on a device [`REPEAT_FIXES`] times in [`REPEAT_WINDOW_MS`],
//! the next alert isn't healed, and its channels hear that self-healing
//! stopped.
//!
//! Each change of an alert's self-healing is one transaction with what it
//! depends on: claiming an attempt with recording it (and creating its
//! script's run), recording how it ended with the alert waiting again, and
//! giving up with marking the channels told. So the server stopping or a
//! write failing part way leaves the alert as it was, for the next pass. An
//! attempt that never ends on its own is ended by [`tick`]: a built-in
//! action unanswered past [`ACTION_GRACE_MS`] is lost, and a script attempt
//! follows its run, which is lost as any run is, or is lost if the run is
//! gone. Should an alert still be found running with no attempt running,
//! [`tick`] moves it on as after a failed attempt.
use meshrmm_protocol_types::{
    AgentCommand, MAX_MONITOR_OUTPUT_BYTES, MonitorAction, MonitorActionResult, RunAs,
    ScriptLanguage, ScriptRunRequest, ScriptRunStatus, truncate_text,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::Serialize;
use serde_json::json;

use super::settings::RemediationKind;
use crate::{
    agents,
    alerts::{
        self, Notice,
        rules::{self, Rule},
        store::{self as alert_store, Alert, RemediationState, Resolution},
    },
    audit::{self, Actor, Target},
    db::{
        self, Executor,
        tables::{AlertRemediations, Alerts, ScriptRuns, ToolboxScripts},
    },
    http::AppState,
    realtime::Unsent,
    time::{DAY_MS, MINUTE_MS, SECOND_MS, now_ms},
    toolbox::{self, NewRun, Source},
    users::new_id,
};

/// A built-in action the Agent hasn't answered in this long is lost. The
/// Agent's slowest takes about two minutes: a Windows service restart waits
/// up to a minute for the service to stop and another to start, and
/// `systemctl` and `launchctl` get two. A late answer that it succeeded is
/// still recorded.
pub const ACTION_GRACE_MS: i64 = 5 * MINUTE_MS;
/// Once self-healing fixed a rule's alerts on a device this many times in
/// [`REPEAT_WINDOW_MS`], it stops healing the next ones there.
pub const REPEAT_FIXES: usize = 3;
pub const REPEAT_WINDOW_MS: i64 = DAY_MS;
/// How long past the rule's wait a condition that clears after an attempt
/// succeeded still counts as fixed by it: evaluations come every 15
/// seconds.
const HEALED_SLACK_MS: i64 = MINUTE_MS;
/// An attempt due while the device is offline waits this long and looks
/// again; it doesn't count as an attempt.
const OFFLINE_RETRY_MS: i64 = MINUTE_MS;
/// An attempt due while the device is connected but too far behind to take
/// it waits for the next evaluation, and doesn't count as an attempt either.
const BUSY_RETRY_MS: i64 = 15 * SECOND_MS;
const OFFLINE: &str = "The device went offline before it could act";
/// A restart or an end for a rule whose monitor has no service or process.
const NOTHING_TO_ACT_ON: &str = "Its monitor has no service or process to act on";

/// How an attempt went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
    /// The device never reported.
    Lost,
    /// It couldn't start, such as because the owner lost their access.
    Skipped,
}

impl AttemptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Lost => "lost",
            Self::Skipped => "skipped",
        }
    }
}

/// One attempt, as the website shows it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Attempt {
    pub id: String,
    pub alert_id: String,
    pub attempt: i64,
    pub action: String,
    /// The service, the process, or the script's name.
    pub target: String,
    pub run_as: Option<String>,
    pub script_run_id: Option<String>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub detail: String,
    pub output: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// An alert's attempts, first first.
pub async fn attempts(executor: &mut impl Executor, alert_id: &str) -> db::Result<Vec<Attempt>> {
    executor
        .fetch_all(
            &Query::select()
                .columns([
                    AlertRemediations::Id,
                    AlertRemediations::AlertId,
                    AlertRemediations::Attempt,
                    AlertRemediations::Action,
                    AlertRemediations::Target,
                    AlertRemediations::RunAs,
                    AlertRemediations::ScriptRunId,
                    AlertRemediations::Status,
                    AlertRemediations::ExitCode,
                    AlertRemediations::Detail,
                    AlertRemediations::Output,
                    AlertRemediations::StartedAt,
                    AlertRemediations::FinishedAt,
                ])
                .from(AlertRemediations::Table)
                .and_where(Expr::col(AlertRemediations::AlertId).eq(alert_id))
                .order_by(AlertRemediations::Attempt, Order::Asc)
                .to_owned(),
        )
        .await
}

/// Recorded in the audit log as making the rule's attempts.
fn actor(rule: &Rule) -> Actor {
    Actor {
        user_id: None,
        label: format!("rule \u{201c}{}\u{201d}", rule.name),
        ip: None,
        api_token: None,
    }
}

async fn set_state(
    executor: &mut impl Executor,
    alert_id: &str,
    state: Option<RemediationState>,
    next_at: Option<i64>,
    only_if: Option<RemediationState>,
) -> db::Result<bool> {
    let mut update = Query::update();
    update
        .table(Alerts::Table)
        .value(
            Alerts::RemediationState,
            state.map(RemediationState::as_str),
        )
        .value(Alerts::NextRemediationAt, next_at)
        .and_where(Expr::col(Alerts::Id).eq(alert_id));
    if let Some(only_if) = only_if {
        update.and_where(Expr::col(Alerts::RemediationState).eq(only_if.as_str()));
    }
    Ok(executor.execute(&update).await? == 1)
}

/// The rule just raised the alert: its first attempt is due now, unless
/// self-healing keeps fixing the same problem on the device. Then it stops
/// at once, and returns `true`: the caller tells the channels, in one
/// `alert.remediation_failed`, so none hears of a failure before the alert.
pub async fn begin(
    state: &AppState,
    rule: &Rule,
    alert_id: &str,
    now: i64,
) -> anyhow::Result<bool> {
    let mut database = &state.database;
    set_state(
        &mut database,
        alert_id,
        Some(RemediationState::Waiting),
        Some(now),
        None,
    )
    .await?;
    let Some(alert) = alert_store::get(&mut database, alert_id).await? else {
        return Ok(false);
    };
    let fixes = recent_fixes(&mut database, &alert, now).await?;
    if let Some(remediation) = &rule.remediation
        && fixes >= REPEAT_FIXES
    {
        let why = format!(
            "Self-healing fixed this {fixes} times in the past {} hours and the problem came back each time",
            REPEAT_WINDOW_MS / (60 * MINUTE_MS)
        );
        let stopped = skip(state, rule, &alert, remediation.action, &why, now)
            .await?
            .is_some();
        if stopped {
            tracing::info!(
                alert_id,
                "self-healing stopped: the problem keeps coming back"
            );
        }
        return Ok(stopped);
    }
    due(state, alert_id, now).await?;
    Ok(false)
}

/// How many of the rule's alerts on the device self-healing fixed in the
/// past [`REPEAT_WINDOW_MS`].
async fn recent_fixes(executor: &mut impl Executor, alert: &Alert, now: i64) -> db::Result<usize> {
    let fixed: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(Alerts::Id)
                .from(Alerts::Table)
                .and_where(Expr::col(Alerts::RuleId).eq(alert.rule_id.as_str()))
                .and_where(Expr::col(Alerts::DeviceId).eq(alert.device_id.as_str()))
                .and_where(Expr::col(Alerts::Resolution).eq(Resolution::Remediated.as_str()))
                .and_where(Expr::col(Alerts::ResolvedAt).gte(now - REPEAT_WINDOW_MS))
                .to_owned(),
        )
        .await?;
    Ok(fixed.len())
}

/// Whether self-healing fixed the alert, whose condition just passed: it is
/// waiting after an attempt that succeeded, which finished within the
/// rule's wait. Skipped, failed and lost attempts fix nothing, and an alert
/// whose self-healing gave up stays given up.
pub async fn healed(
    executor: &mut impl Executor,
    alert: &Alert,
    rule: &Rule,
    now: i64,
) -> db::Result<bool> {
    if alert.remediation_state.as_deref() != Some(RemediationState::Waiting.as_str()) {
        return Ok(false);
    }
    let latest: Option<(String, Option<i64>)> = executor
        .fetch_optional(
            &Query::select()
                .columns([AlertRemediations::Status, AlertRemediations::FinishedAt])
                .from(AlertRemediations::Table)
                .and_where(Expr::col(AlertRemediations::AlertId).eq(alert.id.as_str()))
                .order_by(AlertRemediations::Attempt, Order::Desc)
                .limit(1)
                .to_owned(),
        )
        .await?;
    let wait = rule
        .remediation
        .as_ref()
        .map_or(5, |remediation| i64::from(remediation.retry_minutes))
        * MINUTE_MS;
    Ok(matches!(
        latest,
        Some((status, Some(finished)))
            if status == AttemptStatus::Succeeded.as_str()
                && finished >= now - wait - HEALED_SLACK_MS
    ))
}

/// The alert resolved: self-healing succeeded if it fixed it, and
/// otherwise stops, leaving no attempt due. Failed and succeeded stay.
pub async fn resolved(state: &AppState, alert_id: &str, fixed: bool) -> anyhow::Result<()> {
    state
        .database
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(
                    Alerts::RemediationState,
                    fixed.then_some(RemediationState::Succeeded.as_str()),
                )
                .value(Alerts::NextRemediationAt, Option::<i64>::None)
                .and_where(Expr::col(Alerts::Id).eq(alert_id))
                .and_where(Expr::col(Alerts::RemediationState).is_in([
                    RemediationState::Waiting.as_str(),
                    RemediationState::Running.as_str(),
                ]))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// The alert's last attempt, which says how self-healing ended.
pub async fn last_attempt(
    executor: &mut impl Executor,
    alert_id: &str,
) -> db::Result<Option<Attempt>> {
    Ok(attempts(executor, alert_id).await?.pop())
}

impl Attempt {
    /// Why self-healing stopped, if this attempt couldn't start.
    pub fn skipped_because(&self) -> Option<&str> {
        (self.status == AttemptStatus::Skipped.as_str()).then_some(self.detail.as_str())
    }

    /// The attempt in words, for a notification, such as
    /// `2. Restart “nginx”: failed (Job for nginx.service failed.)`.
    pub fn describe(&self) -> String {
        let what = match RemediationKind::parse(&self.action) {
            Some(RemediationKind::RestartService) => "Restart",
            Some(RemediationKind::EndProcess) => "End",
            _ => "Run",
        };
        let mut text = if self.target.is_empty() {
            format!("{}. {what}", self.attempt)
        } else {
            format!("{}. {what} \u{201c}{}\u{201d}", self.attempt, self.target)
        };
        text.push_str(&format!(": {}", self.status));
        let mut why = self.detail.trim().to_owned();
        if let Some(code) = self.exit_code
            && !why.contains(&code.to_string())
        {
            if !why.is_empty() {
                why.push_str("; ");
            }
            why.push_str(&format!("exit code {code}"));
        }
        if !why.is_empty() {
            text.push_str(&format!(" ({why})"));
        }
        text
    }
}

/// The rule was saved: alerts that waited for self-healing to fail before
/// notifying, but no longer do, notify now, and alerts waiting for an
/// attempt the rule no longer makes stop waiting.
pub async fn rule_saved(state: &AppState, rule: &Rule) -> anyhow::Result<()> {
    if !rule.enabled || (rule.remediation.is_some() && !rule.notifies_at_once()) {
        return Ok(());
    }
    let mut database = &state.database;
    let open = alert_store::unresolved(&mut database)
        .await?
        .into_iter()
        .filter(|alert| alert.rule_id == rule.id);
    for alert in open {
        // Its self-healing stopping and its channels hearing of it change
        // together, so neither is left half done.
        let mut transaction = state.database.begin().await?;
        let mut changed = false;
        if rule.remediation.is_none() {
            changed = set_state(
                &mut transaction,
                &alert.id,
                None,
                None,
                Some(RemediationState::Waiting),
            )
            .await?;
        }
        let notice = (rule.notifies_at_once()
            && alert_store::mark_notified(&mut transaction, &alert.id).await?)
            .then_some(Notice::Raised);
        transaction.commit().await?;
        if changed || notice.is_some() {
            alerts::announce(state, rule, &alert.id, notice).await?;
        }
    }
    Ok(())
}

/// Marks built-in actions that were never answered as lost, records script
/// attempts whose run ended or vanished unrecorded, moves on alerts left
/// running without a running attempt, and makes the attempts that are due.
/// The alert engine calls it with each evaluation.
pub async fn tick(state: &AppState, now: i64) -> anyhow::Result<()> {
    let database = &state.database;
    let scripts: Vec<(String,)> = database
        .fetch_all(
            &Query::select()
                .column(AlertRemediations::ScriptRunId)
                .from(AlertRemediations::Table)
                .and_where(Expr::col(AlertRemediations::Status).eq(AttemptStatus::Running.as_str()))
                .and_where(Expr::col(AlertRemediations::ScriptRunId).is_not_null())
                .to_owned(),
        )
        .await?;
    for (run_id,) in scripts {
        take_run(state, &run_id, now).await?;
    }
    let lost: Vec<(String,)> = database
        .fetch_all(
            &Query::select()
                .column(AlertRemediations::Id)
                .from(AlertRemediations::Table)
                .and_where(Expr::col(AlertRemediations::Status).eq(AttemptStatus::Running.as_str()))
                .and_where(Expr::col(AlertRemediations::ScriptRunId).is_null())
                .and_where(Expr::col(AlertRemediations::StartedAt).lt(now - ACTION_GRACE_MS))
                .to_owned(),
        )
        .await?;
    for (id,) in lost {
        finish(
            state,
            &id,
            Finished {
                status: AttemptStatus::Lost,
                exit_code: None,
                detail: "The device never said how it went".to_owned(),
                output: String::new(),
            },
            now,
        )
        .await?;
    }
    let interrupted: Vec<(String,)> = database.fetch_all(&interrupted_alerts()).await?;
    for (id,) in interrupted {
        interrupted_attempt(state, &id, now).await?;
    }
    let due_alerts: Vec<(String,)> = database
        .fetch_all(
            &Query::select()
                .column(Alerts::Id)
                .from(Alerts::Table)
                .and_where(Expr::col(Alerts::State).ne(alert_store::State::Resolved.as_str()))
                .and_where(
                    Expr::col(Alerts::RemediationState).eq(RemediationState::Waiting.as_str()),
                )
                .and_where(Expr::col(Alerts::NextRemediationAt).lte(now))
                .to_owned(),
        )
        .await?;
    for (id,) in due_alerts {
        due(state, &id, now).await?;
    }
    Ok(())
}

/// Makes the alert's next attempt, or gives up after the last.
async fn due(state: &AppState, alert_id: &str, now: i64) -> anyhow::Result<()> {
    let mut database = &state.database;
    let Some(alert) = alert_store::get(&mut database, alert_id).await? else {
        return Ok(());
    };
    if alert.state == alert_store::State::Resolved.as_str()
        || alert.remediation_state.as_deref() != Some(RemediationState::Waiting.as_str())
        || alert.next_remediation_at.is_none_or(|at| at > now)
    {
        return Ok(());
    }
    // Self-healing waits while the device is in maintenance, someone being
    // at work on it.
    if state
        .maintenance_windows
        .in_maintenance(&alert.device_id, now)
    {
        return Ok(());
    }
    let Some(rule) = rules::by_id(&mut database, &alert.rule_id).await? else {
        return Ok(());
    };
    if !rule.enabled {
        // Its alerts resolve as no longer applying.
        return Ok(());
    }
    let Some(remediation) = rule.remediation.clone() else {
        // The rule no longer heals itself: its channels hear of the alert
        // now if they waited for self-healing to fail, so both change
        // together.
        let mut transaction = state.database.begin().await?;
        set_state(&mut transaction, alert_id, None, None, None).await?;
        let notice = alert_store::mark_notified(&mut transaction, alert_id)
            .await?
            .then_some(Notice::Raised);
        transaction.commit().await?;
        return alerts::announce(state, &rule, alert_id, notice).await;
    };
    if alert.remediation_attempts >= i64::from(remediation.attempts) {
        return give_up(state, &rule, &alert).await;
    }
    // What the attempt runs is read before its transaction, which holds
    // SQLite's one writer.
    let start = match super::problem(&mut database, &rule).await? {
        Some(problem) => Err(problem),
        None if remediation.action == RemediationKind::Script => {
            let script_id = remediation.script_id.as_deref().unwrap_or_default();
            runnable(&mut database, script_id, &alert.device_id)
                .await?
                .map(|(script, language)| Start::Script { script, language })
        }
        None => remediation
            .agent_action(rule.monitor.as_ref())
            .map(Start::Action)
            .ok_or_else(|| NOTHING_TO_ACT_ON.to_owned()),
    };
    let start = match start {
        Ok(start) => start,
        Err(problem) => {
            // Every attempt would be skipped the same way.
            let Some(unheard) =
                skip(state, &rule, &alert, remediation.action, &problem, now).await?
            else {
                return Ok(());
            };
            tracing::info!(alert_id, "self-healing gave up");
            return alerts::announce(
                state,
                &rule,
                alert_id,
                unheard.then_some(Notice::RemediationFailed),
            )
            .await;
        }
    };
    // A place for the attempt is held before it is claimed and recorded, so
    // a device that can't take it now is tried again later and isn't
    // charged an attempt.
    let place = match state.agents.reserve(&alert.device_id) {
        Ok(place) => place,
        Err(unsent) => {
            let wait = match unsent {
                Unsent::Offline => OFFLINE_RETRY_MS,
                Unsent::Full => BUSY_RETRY_MS,
            };
            set_state(
                &mut database,
                alert_id,
                Some(RemediationState::Waiting),
                Some(now + wait),
                Some(RemediationState::Waiting),
            )
            .await?;
            return Ok(());
        }
    };
    let Some(started) = start_attempt(state, &rule, &alert, &remediation, start, now).await? else {
        return Ok(());
    };
    if !place.send(started.command) {
        settle(
            state,
            &started.attempt_id,
            Finished {
                status: AttemptStatus::Failed,
                exit_code: None,
                detail: OFFLINE.to_owned(),
                output: String::new(),
            },
            started.run_id.as_deref(),
            now,
        )
        .await?;
    }
    tracing::info!(
        alert_id,
        attempt = alert.remediation_attempts + 1,
        action = remediation.action.as_str(),
        "started a self-healing attempt"
    );
    alerts::announce(state, &rule, alert_id, None).await
}

/// What an attempt runs on the device.
enum Start {
    Script {
        script: RunnableScript,
        language: ScriptLanguage,
    },
    Action(meshrmm_protocol_types::RemediationAction),
}

/// An attempt that was recorded, and what to send the device to make it.
struct Started {
    attempt_id: String,
    /// A script attempt's run.
    run_id: Option<String>,
    command: AgentCommand,
}

/// Claims the alert's next attempt and records it, with a script's run and
/// in the audit log, in one transaction: a failure part way through, or the
/// server stopping, leaves the alert waiting for the next pass rather than
/// running an attempt nothing records. `None` if another pass claimed it
/// first, or the alert resolved.
async fn start_attempt(
    state: &AppState,
    rule: &Rule,
    alert: &Alert,
    remediation: &super::settings::Remediation,
    start: Start,
    now: i64,
) -> anyhow::Result<Option<Started>> {
    let attempt = alert.remediation_attempts + 1;
    let attempt_id = new_id();
    let mut transaction = state.database.begin().await?;
    // Claim the attempt, so two passes can't both make it, nor one make it
    // on an alert that just resolved.
    let claimed = transaction
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(Alerts::RemediationState, RemediationState::Running.as_str())
                .value(Alerts::NextRemediationAt, Option::<i64>::None)
                .value(
                    Alerts::RemediationAttempts,
                    Expr::col(Alerts::RemediationAttempts).add(1),
                )
                .and_where(Expr::col(Alerts::Id).eq(alert.id.as_str()))
                .and_where(Expr::col(Alerts::State).ne(alert_store::State::Resolved.as_str()))
                .and_where(
                    Expr::col(Alerts::RemediationState).eq(RemediationState::Waiting.as_str()),
                )
                .and_where(Expr::col(Alerts::RemediationAttempts).eq(alert.remediation_attempts))
                .to_owned(),
        )
        .await?;
    if claimed != 1 {
        return Ok(None);
    }
    let owner = rule.owner_user_id.as_deref();
    let started = match start {
        Start::Script { script, language } => {
            let run_as = remediation.run_as.unwrap_or(RunAs::System);
            let script_id = remediation.script_id.as_deref().unwrap_or_default();
            let run_id = new_id();
            toolbox::insert_run(
                &mut transaction,
                NewRun {
                    id: &run_id,
                    device_id: &alert.device_id,
                    script_id,
                    script_name: &script.name,
                    language,
                    requested_by_user_id: owner.unwrap_or_default(),
                    source: Source::Remediation,
                    run_as,
                    timeout_seconds: script.timeout_seconds,
                    job_id: None,
                    created_at: now,
                },
            )
            .await?;
            insert(
                &mut transaction,
                &attempt_id,
                alert,
                attempt,
                RemediationKind::Script,
                &script.name,
                Some(run_as),
                Some(&run_id),
                owner,
                now,
            )
            .await?;
            audit::record(
                &mut transaction,
                &actor(rule),
                "script.run",
                Target::device(&alert.device_id),
                json!({
                    "run_id": run_id,
                    "script_id": script_id,
                    "script_name": script.name,
                    "run_as": run_as.as_str(),
                    "source": Source::Remediation.as_str(),
                    "owner_user_id": rule.owner_user_id,
                }),
            )
            .await?;
            record(
                &mut transaction,
                rule,
                alert,
                attempt,
                RemediationKind::Script,
                &script.name,
            )
            .await?;
            Started {
                attempt_id,
                run_id: Some(run_id.clone()),
                command: AgentCommand::RunScript {
                    run: ScriptRunRequest {
                        run_id,
                        language,
                        body: script.body,
                        run_as,
                        timeout_seconds: u32::try_from(script.timeout_seconds).unwrap_or(300),
                    },
                },
            }
        }
        Start::Action(action) => {
            let target = match &action {
                meshrmm_protocol_types::RemediationAction::RestartService { name }
                | meshrmm_protocol_types::RemediationAction::EndProcess { name } => name.clone(),
            };
            insert(
                &mut transaction,
                &attempt_id,
                alert,
                attempt,
                remediation.action,
                &target,
                None,
                None,
                owner,
                now,
            )
            .await?;
            record(
                &mut transaction,
                rule,
                alert,
                attempt,
                remediation.action,
                &target,
            )
            .await?;
            Started {
                attempt_id: attempt_id.clone(),
                run_id: None,
                command: AgentCommand::RunMonitorAction {
                    action: MonitorAction {
                        action_id: attempt_id,
                        monitor_id: rule.id.clone(),
                        action,
                    },
                },
            }
        }
    };
    transaction.commit().await?;
    Ok(Some(started))
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    executor: &mut impl Executor,
    id: &str,
    alert: &Alert,
    attempt: i64,
    kind: RemediationKind,
    target: &str,
    run_as: Option<RunAs>,
    script_run_id: Option<&str>,
    owner_user_id: Option<&str>,
    now: i64,
) -> db::Result<()> {
    executor
        .execute(
            &Query::insert()
                .into_table(AlertRemediations::Table)
                .columns([
                    AlertRemediations::Id,
                    AlertRemediations::AlertId,
                    AlertRemediations::Attempt,
                    AlertRemediations::Action,
                    AlertRemediations::Target,
                    AlertRemediations::RunAs,
                    AlertRemediations::ScriptRunId,
                    AlertRemediations::Status,
                    AlertRemediations::RequestedByUserId,
                    AlertRemediations::StartedAt,
                ])
                .values_panic([
                    id.into(),
                    alert.id.as_str().into(),
                    attempt.into(),
                    kind.as_str().into(),
                    target.into(),
                    run_as.map(RunAs::as_str).into(),
                    script_run_id.into(),
                    AttemptStatus::Running.as_str().into(),
                    owner_user_id.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Records the attempt in the audit log, as the rule, naming its owner.
async fn record(
    executor: &mut impl Executor,
    rule: &Rule,
    alert: &Alert,
    attempt: i64,
    kind: RemediationKind,
    target: &str,
) -> db::Result<()> {
    audit::record(
        executor,
        &actor(rule),
        "alert.remediate",
        Target::alert(&alert.id),
        json!({
            "rule_id": rule.id,
            "rule_name": rule.name,
            "owner_user_id": rule.owner_user_id,
            "device_id": alert.device_id,
            "device_name": alert.device_name,
            "attempt": attempt,
            "action": kind.as_str(),
            "target": target,
        }),
    )
    .await
}

/// What the rule's attempts act on, as they are recorded: the service or
/// process, or the script by name (empty once it is deleted), and how a
/// script runs.
fn attempt_target(rule: &Rule) -> (String, Option<RunAs>) {
    use meshrmm_protocol_types::RemediationAction;
    let Some(remediation) = &rule.remediation else {
        return (String::new(), None);
    };
    match remediation.action {
        RemediationKind::Script => (
            rule.remediation_script
                .as_ref()
                .map(|script| script.name.clone())
                .unwrap_or_default(),
            Some(remediation.run_as.unwrap_or(RunAs::System)),
        ),
        _ => (
            match remediation.agent_action(rule.monitor.as_ref()) {
                Some(
                    RemediationAction::RestartService { name }
                    | RemediationAction::EndProcess { name },
                ) => name,
                None => String::new(),
            },
            None,
        ),
    }
}

/// Records an attempt that couldn't start, counts it, and gives up on the
/// alert's self-healing, together. `None` if the alert had stopped waiting
/// for an attempt; otherwise whether its channels hadn't heard of it yet.
async fn skip(
    state: &AppState,
    rule: &Rule,
    alert: &Alert,
    kind: RemediationKind,
    why: &str,
    now: i64,
) -> anyhow::Result<Option<bool>> {
    let (target, run_as) = attempt_target(rule);
    let attempt = alert.remediation_attempts + 1;
    let id = new_id();
    let mut transaction = state.database.begin().await?;
    insert(
        &mut transaction,
        &id,
        alert,
        attempt,
        kind,
        &target,
        run_as,
        None,
        rule.owner_user_id.as_deref(),
        now,
    )
    .await?;
    transaction
        .execute(
            &Query::update()
                .table(AlertRemediations::Table)
                .value(AlertRemediations::Status, AttemptStatus::Skipped.as_str())
                .value(AlertRemediations::Detail, why)
                .value(AlertRemediations::FinishedAt, now)
                .and_where(Expr::col(AlertRemediations::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(Alerts::RemediationAttempts, attempt)
                .and_where(Expr::col(Alerts::Id).eq(alert.id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor(rule),
        "alert.remediate",
        Target::alert(&alert.id),
        json!({
            "rule_id": rule.id,
            "rule_name": rule.name,
            "owner_user_id": rule.owner_user_id,
            "device_id": alert.device_id,
            "attempt": attempt,
            "action": kind.as_str(),
            "target": target,
            "skipped": why,
        }),
    )
    .await?;
    let Some(unheard) = fail(&mut transaction, &alert.id).await? else {
        return Ok(None);
    };
    transaction.commit().await?;
    tracing::warn!(alert_id = alert.id, why, "skipped a self-healing attempt");
    Ok(Some(unheard))
}

/// Self-healing has given up: channels that haven't heard of the alert yet
/// hear now.
async fn give_up(state: &AppState, rule: &Rule, alert: &Alert) -> anyhow::Result<()> {
    let mut transaction = state.database.begin().await?;
    let Some(unheard) = fail(&mut transaction, &alert.id).await? else {
        return Ok(());
    };
    transaction.commit().await?;
    tracing::info!(alert_id = alert.id, "self-healing gave up");
    alerts::announce(
        state,
        rule,
        &alert.id,
        unheard.then_some(Notice::RemediationFailed),
    )
    .await
}

/// Marks a waiting alert's self-healing failed, and its channels told, in
/// one transaction, so a failure isn't left unannounced. `None` if it
/// wasn't waiting; otherwise whether the channels hadn't heard of the alert
/// yet.
async fn fail(executor: &mut impl Executor, alert_id: &str) -> db::Result<Option<bool>> {
    if !set_state(
        executor,
        alert_id,
        Some(RemediationState::Failed),
        None,
        Some(RemediationState::Waiting),
    )
    .await?
    {
        return Ok(None);
    }
    Ok(Some(alert_store::mark_notified(executor, alert_id).await?))
}

#[derive(sqlx::FromRow)]
struct RunnableScript {
    name: String,
    language: String,
    body: String,
    timeout_seconds: i64,
    shared: bool,
}

/// The shared script, if the device can run it; otherwise why not.
async fn runnable(
    executor: &mut impl Executor,
    script_id: &str,
    device_id: &str,
) -> anyhow::Result<Result<(RunnableScript, ScriptLanguage), String>> {
    let script: Option<RunnableScript> = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    ToolboxScripts::Name,
                    ToolboxScripts::Language,
                    ToolboxScripts::Body,
                    ToolboxScripts::TimeoutSeconds,
                    ToolboxScripts::Shared,
                ])
                .from(ToolboxScripts::Table)
                .and_where(Expr::col(ToolboxScripts::Id).eq(script_id))
                .to_owned(),
        )
        .await?;
    let Some(script) = script.filter(|script| script.shared) else {
        return Ok(Err(
            "Its script was deleted or is no longer shared".to_owned()
        ));
    };
    let Some(language) = ScriptLanguage::parse(&script.language) else {
        return Ok(Err("Its script's interpreter is unknown".to_owned()));
    };
    let platform = agents::active_platform(executor, device_id)
        .await
        .map_err(|_| anyhow::anyhow!("could not read the device's platform"))?;
    match platform {
        Some(platform) if language.runs_on(platform) => Ok(Ok((script, language))),
        Some(platform) => Ok(Err(format!(
            "{} scripts don't run on {} devices",
            language.label(),
            platform.label()
        ))),
        None => Ok(Err("The device was deleted".to_owned())),
    }
}

/// How an attempt ended.
struct Finished {
    status: AttemptStatus,
    exit_code: Option<i64>,
    detail: String,
    output: String,
}

/// Records how the attempt went, has the device check again, and makes the
/// next attempt due after the rule's wait. Returns whether the attempt was
/// still running.
async fn finish(
    state: &AppState,
    attempt_id: &str,
    finished: Finished,
    now: i64,
) -> anyhow::Result<bool> {
    settle(state, attempt_id, finished, None, now).await
}

/// [`finish`], also failing the script run `unsent`, which never reached
/// the device. The attempt's outcome, its alert waiting again and the run
/// are written in one transaction: an attempt that ended always leaves its
/// alert waiting, so its next attempt or giving up follows.
async fn settle(
    state: &AppState,
    attempt_id: &str,
    finished: Finished,
    unsent: Option<&str>,
    now: i64,
) -> anyhow::Result<bool> {
    // Read before the transaction, which holds SQLite's one writer.
    let found = attempt_alert(state, attempt_id).await?;
    let mut transaction = state.database.begin().await?;
    let updated = transaction
        .execute(
            &Query::update()
                .table(AlertRemediations::Table)
                .value(AlertRemediations::Status, finished.status.as_str())
                .value(AlertRemediations::ExitCode, finished.exit_code)
                .value(
                    AlertRemediations::Detail,
                    truncate_text(&finished.detail, 2000),
                )
                .value(
                    AlertRemediations::Output,
                    truncate_text(&finished.output, MAX_MONITOR_OUTPUT_BYTES),
                )
                .value(AlertRemediations::FinishedAt, now)
                .and_where(Expr::col(AlertRemediations::Id).eq(attempt_id))
                .and_where(Expr::col(AlertRemediations::Status).eq(AttemptStatus::Running.as_str()))
                .to_owned(),
        )
        .await?;
    // Another pass, or the device's answer, ended it first.
    if updated != 1 {
        return Ok(false);
    }
    if let Some(run_id) = unsent {
        transaction
            .execute(
                &Query::update()
                    .table(ScriptRuns::Table)
                    .value(ScriptRuns::Status, ScriptRunStatus::Failed.as_str())
                    .value(
                        ScriptRuns::Error,
                        "The device is offline, so the script did not run.",
                    )
                    .value(ScriptRuns::CompletedAt, now)
                    .and_where(Expr::col(ScriptRuns::Id).eq(run_id))
                    .and_where(Expr::col(ScriptRuns::Status).eq(ScriptRunStatus::Pending.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    if let Some((alert, rule)) = &found {
        set_state(
            &mut transaction,
            &alert.id,
            Some(RemediationState::Waiting),
            Some(now + wait(rule)),
            Some(RemediationState::Running),
        )
        .await?;
    }
    transaction.commit().await?;
    let Some((alert, rule)) = found else {
        return Ok(true);
    };
    tracing::info!(
        alert_id = alert.id,
        attempt_id,
        status = finished.status.as_str(),
        "a self-healing attempt finished"
    );
    check_again(state, &alert, &rule);
    alerts::announce(state, &rule, &alert.id, None).await?;
    Ok(true)
}

/// The rule's wait after an attempt finishes.
fn wait(rule: &Rule) -> i64 {
    rule.remediation
        .as_ref()
        .map_or(1, |remediation| i64::from(remediation.retry_minutes))
        * MINUTE_MS
}

/// Alerts left running an attempt with no attempt running: the server
/// stopped, or a write failed, between claiming an attempt and recording
/// it, or between recording how an attempt ended and moving its alert on.
/// They are found through `idx_alerts_remediation`, and each one's attempts
/// through `idx_alert_remediations_alert`.
fn interrupted_alerts() -> sea_query::SelectStatement {
    Query::select()
        .column(Alerts::Id)
        .from(Alerts::Table)
        .and_where(Expr::col(Alerts::RemediationState).eq(RemediationState::Running.as_str()))
        .and_where(Expr::col(Alerts::State).ne(alert_store::State::Resolved.as_str()))
        .and_where(Expr::exists(running_attempt()).not())
        .to_owned()
}

/// A running attempt of the alert in the outer statement.
fn running_attempt() -> sea_query::SelectStatement {
    Query::select()
        .expr(Expr::val(1))
        .from(AlertRemediations::Table)
        .and_where(
            Expr::col((AlertRemediations::Table, AlertRemediations::AlertId))
                .equals((Alerts::Table, Alerts::Id)),
        )
        .and_where(
            Expr::col((AlertRemediations::Table, AlertRemediations::Status))
                .eq(AttemptStatus::Running.as_str()),
        )
        .to_owned()
}

/// Why an attempt that was claimed but never recorded failed.
const NEVER_STARTED: &str = "It never started: the server stopped or failed as it began";

/// Moves on an alert left running with no attempt running, as after any
/// attempt that failed: it waits the rule's wait for its next attempt, or
/// to give up, and the device checks again. An attempt it claimed but never
/// recorded is recorded as failed, saying so. It happens only while the
/// alert is still running with no attempt running, so two passes, or a pass
/// and an attempt's result, move it on once.
async fn interrupted_attempt(state: &AppState, alert_id: &str, now: i64) -> anyhow::Result<()> {
    let mut database = &state.database;
    let Some(alert) = alert_store::get(&mut database, alert_id).await? else {
        return Ok(());
    };
    let Some(rule) = rules::by_id(&mut database, &alert.rule_id).await? else {
        return Ok(());
    };
    let recorded = last_attempt(&mut database, alert_id)
        .await?
        .map_or(0, |last| last.attempt);
    let unrecorded = rule
        .remediation
        .as_ref()
        .filter(|_| alert.remediation_attempts > recorded);
    let mut transaction = state.database.begin().await?;
    let moved = transaction
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(Alerts::RemediationState, RemediationState::Waiting.as_str())
                .value(Alerts::NextRemediationAt, now + wait(&rule))
                .and_where(Expr::col(Alerts::Id).eq(alert_id))
                .and_where(
                    Expr::col(Alerts::RemediationState).eq(RemediationState::Running.as_str()),
                )
                .and_where(Expr::col(Alerts::State).ne(alert_store::State::Resolved.as_str()))
                .and_where(Expr::exists(running_attempt()).not())
                .to_owned(),
        )
        .await?;
    if moved != 1 {
        return Ok(());
    }
    if let Some(remediation) = unrecorded {
        let (target, run_as) = attempt_target(&rule);
        let id = new_id();
        insert(
            &mut transaction,
            &id,
            &alert,
            alert.remediation_attempts,
            remediation.action,
            &target,
            run_as,
            None,
            rule.owner_user_id.as_deref(),
            now,
        )
        .await?;
        transaction
            .execute(
                &Query::update()
                    .table(AlertRemediations::Table)
                    .value(AlertRemediations::Status, AttemptStatus::Failed.as_str())
                    .value(AlertRemediations::Detail, NEVER_STARTED)
                    .value(AlertRemediations::FinishedAt, now)
                    .and_where(Expr::col(AlertRemediations::Id).eq(id.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    transaction.commit().await?;
    tracing::warn!(
        alert_id,
        attempt = alert.remediation_attempts,
        "moved on self-healing left running without a running attempt"
    );
    check_again(state, &alert, &rule);
    alerts::announce(state, &rule, alert_id, None).await
}

/// The attempt's alert and its rule.
async fn attempt_alert(
    state: &AppState,
    attempt_id: &str,
) -> anyhow::Result<Option<(Alert, Rule)>> {
    let mut database = &state.database;
    let Some((alert_id,)) = database
        .fetch_optional::<(String,), _>(
            &Query::select()
                .column(AlertRemediations::AlertId)
                .from(AlertRemediations::Table)
                .and_where(Expr::col(AlertRemediations::Id).eq(attempt_id))
                .to_owned(),
        )
        .await?
    else {
        return Ok(None);
    };
    let Some(alert) = alert_store::get(&mut database, &alert_id).await? else {
        return Ok(None);
    };
    let Some(rule) = rules::by_id(&mut database, &alert.rule_id).await? else {
        return Ok(None);
    };
    Ok(Some((alert, rule)))
}

/// Has the device check an unresolved alert's monitor again, or judges the
/// rules again for other rules.
fn check_again(state: &AppState, alert: &Alert, rule: &Rule) {
    if alert.state == alert_store::State::Resolved.as_str() {
        return;
    }
    if rule.monitor.is_some() {
        // Only sooner than the device's own schedule, which checks the
        // monitor again anyway, so one that can't be asked now isn't.
        let _ = super::check_now(state, &alert.device_id, vec![rule.id.clone()]);
    } else {
        state.alerts.wake();
    }
}

#[derive(sqlx::FromRow)]
struct FinishedRun {
    status: String,
    exit_code: Option<i64>,
    stdout: String,
    stderr: String,
    error: Option<String>,
}

/// A toolbox run finished, failed to start or was found lost: if it was an
/// attempt's script, records how it went.
pub async fn run_finished(state: &AppState, run_id: &str) {
    if let Err(error) = take_run(state, run_id, now_ms()).await {
        tracing::warn!(
            run_id,
            error = format!("{error:#}"),
            "could not record a self-healing script's result"
        );
    }
}

async fn take_run(state: &AppState, run_id: &str, now: i64) -> anyhow::Result<()> {
    let database = &state.database;
    let Some((attempt_id,)) = database
        .fetch_optional::<(String,), _>(
            &Query::select()
                .column(AlertRemediations::Id)
                .from(AlertRemediations::Table)
                .and_where(Expr::col(AlertRemediations::ScriptRunId).eq(run_id))
                .and_where(Expr::col(AlertRemediations::Status).eq(AttemptStatus::Running.as_str()))
                .to_owned(),
        )
        .await?
    else {
        return Ok(());
    };
    let run: Option<FinishedRun> = database
        .fetch_optional(
            &Query::select()
                .columns([
                    ScriptRuns::Status,
                    ScriptRuns::ExitCode,
                    ScriptRuns::Stdout,
                    ScriptRuns::Stderr,
                    ScriptRuns::Error,
                ])
                .from(ScriptRuns::Table)
                .and_where(Expr::col(ScriptRuns::Id).eq(run_id))
                .to_owned(),
        )
        .await?;
    let finished = match run {
        Some(run) => match run_outcome(&run) {
            Some(finished) => finished,
            None => return Ok(()),
        },
        // Deleted with its history.
        None => Finished {
            status: AttemptStatus::Lost,
            exit_code: None,
            detail: "The script's run is gone".to_owned(),
            output: String::new(),
        },
    };
    finish(state, &attempt_id, finished, now).await.map(drop)
}

/// How a finished run went, as an attempt; `None` while it's pending.
fn run_outcome(run: &FinishedRun) -> Option<Finished> {
    let mut output = run.stdout.clone();
    if !run.stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&run.stderr);
    }
    let (status, detail) = match ScriptRunStatus::parse(&run.status)? {
        ScriptRunStatus::Pending => return None,
        ScriptRunStatus::Completed => match run.exit_code {
            Some(0) => (AttemptStatus::Succeeded, "Exited with code 0".to_owned()),
            Some(code) => (AttemptStatus::Failed, format!("Exited with code {code}")),
            None => (
                AttemptStatus::Failed,
                "Ended without an exit code".to_owned(),
            ),
        },
        ScriptRunStatus::Failed => (
            AttemptStatus::Failed,
            run.error
                .clone()
                .unwrap_or_else(|| "The script couldn't run".to_owned()),
        ),
        ScriptRunStatus::TimedOut => (
            AttemptStatus::Failed,
            "Timed out and was stopped".to_owned(),
        ),
        ScriptRunStatus::Lost => (
            AttemptStatus::Lost,
            "The device never said how it went".to_owned(),
        ),
    };
    Some(Finished {
        status,
        exit_code: run.exit_code,
        detail,
        output,
    })
}

/// The Agent answered a built-in action.
pub async fn action_reported(state: &AppState, device_id: &str, result: MonitorActionResult) {
    if let Err(error) = take_action(state, device_id, result, now_ms()).await {
        tracing::warn!(
            device_id,
            error = format!("{error:#}"),
            "could not record a self-healing action's result"
        );
    }
}

async fn take_action(
    state: &AppState,
    device_id: &str,
    result: MonitorActionResult,
    now: i64,
) -> anyhow::Result<()> {
    if crate::agents::parse_id(&result.action_id, "action").is_err() {
        return Ok(());
    }
    // Only the device the action was sent to may answer it.
    let owner: Option<(String,)> = state
        .database
        .fetch_optional(
            &Query::select()
                .column((Alerts::Table, Alerts::DeviceId))
                .from(AlertRemediations::Table)
                .inner_join(
                    Alerts::Table,
                    Expr::col((Alerts::Table, Alerts::Id))
                        .equals((AlertRemediations::Table, AlertRemediations::AlertId)),
                )
                .and_where(
                    Expr::col((AlertRemediations::Table, AlertRemediations::Id))
                        .eq(result.action_id.as_str()),
                )
                .to_owned(),
        )
        .await?;
    if owner.is_none_or(|(owner,)| owner != device_id) {
        return Ok(());
    }
    let detail: String = result
        .detail
        .replace('\0', "\u{fffd}")
        .chars()
        .take(1000)
        .collect();
    let recorded = finish(
        state,
        &result.action_id,
        Finished {
            status: if result.succeeded {
                AttemptStatus::Succeeded
            } else {
                AttemptStatus::Failed
            },
            exit_code: None,
            detail: detail.clone(),
            output: String::new(),
        },
        now,
    )
    .await?;
    if !recorded && result.succeeded {
        late_success(state, &result.action_id, &detail, now).await?;
    }
    Ok(())
}

/// The Agent said an action succeeded after it was marked lost: records
/// that, and has the device check again. The next attempt stays as due,
/// since the alert's self-healing moved on.
async fn late_success(
    state: &AppState,
    attempt_id: &str,
    detail: &str,
    now: i64,
) -> anyhow::Result<()> {
    let updated = state
        .database
        .execute(
            &Query::update()
                .table(AlertRemediations::Table)
                .value(AlertRemediations::Status, AttemptStatus::Succeeded.as_str())
                .value(
                    AlertRemediations::Detail,
                    truncate_text(&format!("{detail} (reported late)"), 2000),
                )
                .value(AlertRemediations::FinishedAt, now)
                .and_where(Expr::col(AlertRemediations::Id).eq(attempt_id))
                .and_where(Expr::col(AlertRemediations::Status).eq(AttemptStatus::Lost.as_str()))
                .and_where(Expr::col(AlertRemediations::ScriptRunId).is_null())
                .to_owned(),
        )
        .await?;
    if updated != 1 {
        return Ok(());
    }
    let Some((alert, rule)) = attempt_alert(state, attempt_id).await? else {
        return Ok(());
    };
    tracing::info!(
        alert_id = alert.id,
        attempt_id,
        "a self-healing action reported success late"
    );
    check_again(state, &alert, &rule);
    alerts::announce(state, &rule, &alert.id, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, exit_code: Option<i64>) -> FinishedRun {
        FinishedRun {
            status: status.into(),
            exit_code,
            stdout: "cleaned 3 GB".into(),
            stderr: "1 file in use".into(),
            error: None,
        }
    }

    fn attempt(
        action: &str,
        target: &str,
        status: &str,
        exit_code: Option<i64>,
        detail: &str,
    ) -> Attempt {
        Attempt {
            id: "a".into(),
            alert_id: "b".into(),
            attempt: 2,
            action: action.into(),
            target: target.into(),
            run_as: None,
            script_run_id: None,
            status: status.into(),
            exit_code,
            detail: detail.into(),
            output: String::new(),
            started_at: 0,
            finished_at: Some(1),
        }
    }

    #[test]
    fn last_attempts_say_how_they_ended() {
        assert_eq!(
            attempt(
                "restart_service",
                "nginx",
                "failed",
                None,
                "Job for nginx.service failed."
            )
            .describe(),
            "2. Restart \u{201c}nginx\u{201d}: failed (Job for nginx.service failed.)"
        );
        assert_eq!(
            attempt(
                "script",
                "Clean up",
                "failed",
                Some(2),
                "Exited with code 2"
            )
            .describe(),
            "2. Run \u{201c}Clean up\u{201d}: failed (Exited with code 2)",
            "the exit code isn't said twice"
        );
        assert_eq!(
            attempt("script", "Clean up", "failed", Some(3), "").describe(),
            "2. Run \u{201c}Clean up\u{201d}: failed (exit code 3)"
        );
        let lost = attempt(
            "end_process",
            "",
            "lost",
            None,
            "The device never said how it went",
        );
        assert_eq!(
            lost.describe(),
            "2. End: lost (The device never said how it went)"
        );
        assert_eq!(lost.skipped_because(), None);
        let skipped = attempt("restart_service", "nginx", "skipped", None, "No access");
        assert_eq!(skipped.skipped_because(), Some("No access"));
    }

    #[test]
    fn runs_become_attempt_results() {
        let done = run_outcome(&run("completed", Some(0))).unwrap();
        assert_eq!(done.status, AttemptStatus::Succeeded);
        assert_eq!(done.output, "cleaned 3 GB\n1 file in use");
        let failed = run_outcome(&run("completed", Some(2))).unwrap();
        assert_eq!(failed.status, AttemptStatus::Failed);
        assert_eq!(failed.detail, "Exited with code 2");
        assert_eq!(
            run_outcome(&run("lost", None)).unwrap().status,
            AttemptStatus::Lost
        );
        assert!(run_outcome(&run("pending", None)).is_none());
    }
}
