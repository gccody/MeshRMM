//! Monitors: alert rules whose condition the device's Agent checks, and
//! self-healing. See docs/monitors.md.
//!
//! The engine's task keeps each connected Agent's monitors current: it sends
//! a device the monitors that apply to it when it connects
//! ([`greeting`]), and again whenever that set changes, because a rule, its
//! targets, the device's company or tags, a script or a rule owner's access
//! changed. It looks again on every presence change, when woken after a rule
//! changes, and every [`RESYNC_INTERVAL`]. Agents report results
//! ([`reported`]); the server keeps the latest per monitor and device, and
//! raises and resolves the rule's alerts through the alert engine. An offline
//! device reports nothing, so its alerts stay as they are.
pub mod remediation;
pub mod settings;
pub mod store;

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use meshrmm_protocol_types::{
    AgentCommand, DevicePlatform, MAX_MONITOR_DETAIL_CHARS, MAX_MONITOR_OUTPUT_BYTES, MAX_MONITORS,
    MonitorCheck, MonitorDefinition, MonitorResult, ScriptLanguage, truncate_text,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::Serialize;
use tokio::sync::Notify;

use self::store::MonitorState;
use crate::{
    alerts::{
        self, AlertEvent,
        rules::{self, Condition, Rule, Severity},
        store::{self as alert_store, Raise, Resolution},
    },
    db::{
        self, Executor,
        tables::{Agents, ToolboxScripts},
    },
    http::AppState,
    rbac::{self, Permission},
    targets,
    time::now_ms,
    users,
};

/// How often every connected device's monitors are compared with what it
/// was sent, catching changes nothing announced, such as a role's.
pub const RESYNC_INTERVAL: Duration = Duration::from_secs(60);
/// Changes this close together are handled together.
const SETTLE: Duration = Duration::from_millis(250);
/// How long the rules read for one pass or report serve the next ones.
/// Waking the engine forgets them at once.
const RULES_FRESH: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default)]
pub struct Engine {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    wake: Notify,
    /// Devices that get their monitors at the next pass even if unchanged.
    forced: Mutex<HashSet<String>>,
    /// What each connected device was last sent.
    sent: Mutex<HashMap<String, u64>>,
    rules: Mutex<Option<(Instant, Arc<RuleSet>)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Nothing panics while holding these locks, so a poisoned one is whole.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Engine {
    /// Compares every connected device's monitors with what it was sent,
    /// soon, such as after a rule changed.
    pub fn wake(&self) {
        *lock(&self.inner.rules) = None;
        self.inner.wake.notify_one();
    }

    /// The monitor rules, read again if those at hand are old.
    async fn rules(&self, executor: &mut impl Executor) -> db::Result<Arc<RuleSet>> {
        if let Some((read, rules)) = &*lock(&self.inner.rules)
            && read.elapsed() < RULES_FRESH
        {
            return Ok(Arc::clone(rules));
        }
        let rules = Arc::new(RuleSet::load(executor).await?);
        *lock(&self.inner.rules) = Some((Instant::now(), Arc::clone(&rules)));
        Ok(rules)
    }

    /// Sends the device its monitors at the next pass, changed or not.
    fn resend(&self, device_id: &str) {
        lock(&self.inner.forced).insert(device_id.to_owned());
        self.wake();
    }
}

fn fingerprint(monitors: &[MonitorDefinition]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(monitors)
        .expect("monitors serialize")
        .hash(&mut hasher);
    hasher.finish()
}

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut presence = state.presence.subscribe();
        let mut resync = tokio::time::interval(RESYNC_INTERVAL);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = resync.tick() => {}
                () = state.monitors.inner.wake.notified() => {}
                event = presence.recv() => {
                    if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return;
                    }
                }
            }
            tokio::time::sleep(SETTLE).await;
            // What arrived meanwhile is covered by this pass.
            while presence.try_recv().is_ok() {}
            if let Err(error) = sync(&state).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not update the devices' monitors"
                );
            }
        }
    })
}

/// Sends each connected device whose monitors changed its new set.
pub async fn sync(state: &AppState) -> anyhow::Result<()> {
    let connected = state.agents.connected();
    if connected.is_empty() {
        lock(&state.monitors.inner.sent).clear();
        return Ok(());
    }
    let catalog = Catalog::load(state, None).await?;
    let forced = std::mem::take(&mut *lock(&state.monitors.inner.forced));
    let empty = fingerprint(&[]);
    for device_id in &connected {
        let monitors = catalog.definitions(device_id);
        let print = fingerprint(&monitors);
        let previous = lock(&state.monitors.inner.sent)
            .get(device_id)
            .copied()
            .unwrap_or(empty);
        if print == previous && !forced.contains(device_id) {
            continue;
        }
        let count = monitors.len();
        if state
            .agents
            .send(device_id, AgentCommand::SetMonitors { monitors })
        {
            tracing::info!(device_id, monitors = count, "sent the device its monitors");
            lock(&state.monitors.inner.sent).insert(device_id.clone(), print);
        }
    }
    lock(&state.monitors.inner.sent).retain(|device_id, _| connected.contains(device_id));
    Ok(())
}

/// The monitors to send an Agent that just connected, if it has any.
pub async fn greeting(state: &AppState, device_id: &str) -> anyhow::Result<Option<AgentCommand>> {
    let catalog = Catalog::load(state, Some(device_id)).await?;
    let monitors = catalog.definitions(device_id);
    lock(&state.monitors.inner.sent).insert(device_id.to_owned(), fingerprint(&monitors));
    Ok((!monitors.is_empty()).then_some(AgentCommand::SetMonitors { monitors }))
}

/// A shared script a monitor runs.
#[derive(Debug, Clone)]
struct Script {
    language: Option<ScriptLanguage>,
    body: String,
}

/// The enabled monitor rules, with why devices can't check each if they
/// can't ([`check_problem`]), and their scripts.
#[derive(Debug)]
struct RuleSet {
    rules: Vec<(Rule, Option<String>)>,
    scripts: HashMap<String, Script>,
}

impl RuleSet {
    async fn load(executor: &mut impl Executor) -> db::Result<Self> {
        let mut rules = Vec::new();
        for rule in rules::load(executor, None).await? {
            if rule.enabled && rule.monitor.is_some() {
                let problem = check_problem(executor, &rule).await?;
                rules.push((rule, problem));
            }
        }
        let script_ids = rules
            .iter()
            .filter_map(|(rule, _)| rule.monitor.as_ref()?.script_id().map(str::to_owned))
            .collect::<Vec<_>>();
        let scripts = if script_ids.is_empty() {
            HashMap::new()
        } else {
            executor
                .fetch_all::<(String, String, String), _>(
                    &Query::select()
                        .columns([
                            ToolboxScripts::Id,
                            ToolboxScripts::Language,
                            ToolboxScripts::Body,
                        ])
                        .from(ToolboxScripts::Table)
                        .and_where(Expr::col(ToolboxScripts::Id).is_in(script_ids))
                        .and_where(Expr::col(ToolboxScripts::Shared).eq(true))
                        .to_owned(),
                )
                .await?
                .into_iter()
                .map(|(id, language, body)| {
                    (
                        id,
                        Script {
                            language: ScriptLanguage::parse(&language),
                            body,
                        },
                    )
                })
                .collect()
        };
        Ok(Self { rules, scripts })
    }
}

/// What deciding a device's monitors needs.
struct Catalog {
    rules: Arc<RuleSet>,
    devices: HashMap<String, targets::Device>,
    platforms: HashMap<String, DevicePlatform>,
}

impl Catalog {
    /// Everything for every device, or only for `device_id`.
    async fn load(state: &AppState, device_id: Option<&str>) -> db::Result<Self> {
        let executor = &mut &state.database;
        let rules = state.monitors.rules(executor).await?;
        let devices = targets::devices(executor, device_id)
            .await?
            .into_iter()
            .map(|device| (device.id.clone(), device))
            .collect();
        let mut select = Query::select();
        select
            .columns([Agents::Id, Agents::Platform])
            .from(Agents::Table)
            .and_where(Expr::col(Agents::DeletionRequestedAt).is_null());
        if let Some(id) = device_id {
            select.and_where(Expr::col(Agents::Id).eq(id));
        }
        let platforms = executor
            .fetch_all::<(String, String), _>(&select)
            .await?
            .into_iter()
            .filter_map(|(id, platform)| Some((id, DevicePlatform::parse(&platform)?)))
            .collect();
        Ok(Self {
            rules,
            devices,
            platforms,
        })
    }

    /// The monitor rules that cover the device, each with its definition or
    /// why the device doesn't run it.
    fn applicable(&self, device_id: &str) -> Vec<(&Rule, Result<MonitorDefinition, String>)> {
        let (Some(device), Some(platform)) =
            (self.devices.get(device_id), self.platforms.get(device_id))
        else {
            return Vec::new();
        };
        self.rules
            .rules
            .iter()
            .filter(|(rule, _)| rule.covers(device))
            .take(MAX_MONITORS)
            .map(|(rule, problem)| (rule, self.definition(rule, problem.as_deref(), *platform)))
            .collect()
    }

    fn definition(
        &self,
        rule: &Rule,
        problem: Option<&str>,
        platform: DevicePlatform,
    ) -> Result<MonitorDefinition, String> {
        let monitor = rule.monitor.as_ref().ok_or_else(String::new)?;
        // Only checks that run with the owner's access have a problem.
        if let Some(problem) = problem {
            return Err(problem.to_owned());
        }
        let mut check = monitor.check.clone();
        if let MonitorCheck::ScriptCheck {
            script_id,
            language,
            body,
            ..
        } = &mut check
        {
            let script = self
                .rules
                .scripts
                .get(script_id.as_str())
                .ok_or("Its script was deleted or is no longer shared")?;
            *language = script.language;
            body.clone_from(&script.body);
        }
        if !check.runs_on(platform) {
            return Err(match &check {
                MonitorCheck::ScriptCheck {
                    language: Some(language),
                    ..
                } => format!(
                    "{} scripts don't run on {} devices",
                    language.label(),
                    platform.label()
                ),
                MonitorCheck::EventLog { .. } => {
                    "It names no Windows event log, so Windows devices don't check it".to_owned()
                }
                MonitorCheck::Service { name, .. } => format!(
                    "\u{201c}{name}\u{201d} isn't a {}, so {} devices don't check it",
                    if platform == DevicePlatform::Macos {
                        "launchd job label"
                    } else {
                        "systemd unit name"
                    },
                    platform.label()
                ),
                _ => format!("{} devices can't run it", platform.label()),
            });
        }
        Ok(MonitorDefinition {
            id: rule.id.clone(),
            interval_seconds: monitor.interval_seconds,
            timeout_seconds: monitor.timeout_seconds,
            check,
        })
    }

    fn definitions(&self, device_id: &str) -> Vec<MonitorDefinition> {
        self.applicable(device_id)
            .into_iter()
            .filter_map(|(_, definition)| definition.ok())
            .collect()
    }
}

/// Why a rule can't act on devices with its owner's access, if it can't:
/// a script it runs is gone or no longer shared, or its owner lost the
/// access it runs with. Rules that only watch have no problem.
pub async fn problem(executor: &mut impl Executor, rule: &Rule) -> db::Result<Option<String>> {
    if !rule.acts_on_devices() {
        return Ok(None);
    }
    let scripts_gone = [
        rule.monitor
            .as_ref()
            .and_then(|monitor| monitor.script_id())
            .map(|_| &rule.check_script),
        rule.remediation
            .as_ref()
            .and_then(|remediation| remediation.script_id.as_ref())
            .map(|_| &rule.remediation_script),
    ]
    .into_iter()
    .flatten()
    .any(|script| !script.as_ref().is_some_and(|script| script.shared));
    if scripts_gone {
        return Ok(Some(SCRIPT_GONE.to_owned()));
    }
    owner_problem(executor, rule).await
}

const SCRIPT_GONE: &str = "A script it runs was deleted or is no longer shared";

/// Why devices can't check the rule's monitor, if its check runs with its
/// owner's access ([`Condition::runs_with_owner_access`]) and can't: its
/// script is gone or its owner lost that access. Such a check isn't sent,
/// and its alerts resolve as no longer applying. Other checks only look at
/// the device, so nothing stops them.
pub async fn check_problem(
    executor: &mut impl Executor,
    rule: &Rule,
) -> db::Result<Option<String>> {
    if rule.monitor.is_none() || !rule.condition.runs_with_owner_access() {
        return Ok(None);
    }
    if rule
        .monitor
        .as_ref()
        .and_then(|monitor| monitor.script_id())
        .is_some()
        && !rule
            .check_script
            .as_ref()
            .is_some_and(|script| script.shared)
    {
        return Ok(Some(SCRIPT_GONE.to_owned()));
    }
    owner_problem(executor, rule).await
}

/// Why the rule's owner can't lend it their access, if they can't: they
/// were disabled or removed, or lost `alerts.manage` or `scripts.run`.
async fn owner_problem(executor: &mut impl Executor, rule: &Rule) -> db::Result<Option<String>> {
    let owner = match &rule.owner_user_id {
        Some(id) => users::by_id(executor, id).await?,
        None => None,
    };
    let Some(owner) = owner.filter(|owner| !owner.disabled) else {
        return Ok(Some(
            "The user whose access it runs with was disabled or removed; save the rule to run it with your access".to_owned(),
        ));
    };
    let permissions = rbac::permissions_of(&rbac::user_roles(executor, &owner.id).await?);
    let missing = [Permission::AlertsManage, Permission::ScriptsRun]
        .into_iter()
        .filter(|permission| !permissions.contains(permission))
        .collect::<Vec<_>>();
    Ok(lacks(&owner_label(&owner), &missing))
}

/// Why an owner can't lend a rule their access, if they miss any of the
/// permissions it needs: naming only those.
fn lacks(owner: &str, missing: &[Permission]) -> Option<String> {
    let what = match missing {
        [] => return None,
        [one] => format!("the {one} permission"),
        [first, second] => format!("the {first} and {second} permissions"),
        _ => {
            let names = missing.iter().map(ToString::to_string).collect::<Vec<_>>();
            format!("the {} permissions", names.join(", "))
        }
    };
    Some(format!(
        "{owner}, whose access it runs with, no longer has {what}; save the rule to run it with your access"
    ))
}

/// How a rule's owner is named: by their display name, else their email.
pub fn owner_label(owner: &users::User) -> String {
    if owner.display_name.is_empty() {
        owner.email.clone()
    } else {
        owner.display_name.clone()
    }
}

/// A storable detail or output: no NUL characters, and within its limit.
fn storable(text: &str, max_bytes: usize) -> String {
    truncate_text(&text.replace('\0', "\u{fffd}"), max_bytes).to_owned()
}

/// Takes in the results a device's Agent reported: keeps each one, and
/// raises or resolves its rule's alert. Results for monitors that no longer
/// apply make the server send the device its monitors again.
pub async fn reported(state: &AppState, device_id: &str, results: Vec<MonitorResult>) {
    if let Err(error) = take_results(state, device_id, results, now_ms()).await {
        tracing::warn!(
            device_id,
            error = format!("{error:#}"),
            "could not take in monitor results"
        );
    }
}

async fn take_results(
    state: &AppState,
    device_id: &str,
    results: Vec<MonitorResult>,
    now: i64,
) -> anyhow::Result<()> {
    let mut database = &state.database;
    let catalog = Catalog::load(state, Some(device_id)).await?;
    let applicable = catalog
        .applicable(device_id)
        .into_iter()
        .filter(|(_, definition)| definition.is_ok())
        .map(|(rule, _)| (rule.id.as_str(), rule))
        .collect::<HashMap<_, _>>();
    let mut stale = false;
    let mut states = Vec::new();
    for result in results.into_iter().take(MAX_MONITORS) {
        let Some(rule) = applicable.get(result.monitor_id.as_str()) else {
            stale = true;
            continue;
        };
        let checked_at = Ord::min(i64::try_from(result.checked_at_unix_ms).unwrap_or(now), now);
        let detail: String = result
            .detail
            .replace('\0', "\u{fffd}")
            .chars()
            .take(MAX_MONITOR_DETAIL_CHARS)
            .collect();
        let monitor_state = MonitorState {
            rule_id: rule.id.clone(),
            device_id: device_id.to_owned(),
            checked_at,
            received_at: now,
            passing: result.passing,
            failures: i64::from(result.failures),
            value: result.value.filter(|value| value.is_finite()),
            detail,
            output: storable(&result.output, MAX_MONITOR_OUTPUT_BYTES),
        };
        store::upsert(&mut database, &monitor_state).await?;
        judge(state, rule, &monitor_state, now).await?;
        states.push(monitor_state);
    }
    if stale {
        state.monitors.resend(device_id);
    }
    if !states.is_empty() {
        state.alerts.publish(AlertEvent::MonitorUpdate {
            device_id: device_id.to_owned(),
            states,
        });
    }
    Ok(())
}

/// Raises the rule's alert once enough checks in a row failed, keeps it
/// describing the latest failure, and resolves it once a check passes.
async fn judge(
    state: &AppState,
    rule: &Rule,
    result: &MonitorState,
    now: i64,
) -> anyhow::Result<()> {
    let threshold = rule.monitor.as_ref().map_or(1, |monitor| monitor.failures);
    let alerting = !result.passing && result.failures >= i64::from(threshold);
    let mut database = &state.database;
    let open = alert_store::unresolved_for(&mut database, &rule.id, &result.device_id, "").await?;
    match open {
        None if alerting => {
            let raise = Raise {
                rule_id: &rule.id,
                device_id: &result.device_id,
                subject: "",
                value: result.value,
                detail: &result.detail,
                script_run_id: None,
                update_run_id: None,
                output: &result.output,
            };
            if let Some(id) = alert_store::raise(&mut database, &raise, now).await? {
                tracing::info!(
                    rule_id = rule.id,
                    device_id = result.device_id,
                    alert_id = id,
                    "a monitor raised an alert"
                );
                alerts::raised(state, rule, &id, now).await?;
            }
        }
        Some(alert) if alerting => {
            if (alert.detail != result.detail
                || alert.output != result.output
                || alert.value != result.value)
                && alert_store::refresh(
                    &mut database,
                    &alert.id,
                    result.value,
                    &result.detail,
                    &result.output,
                    now,
                )
                .await?
            {
                alerts::announce(state, rule, &alert.id, None).await?;
            }
        }
        Some(alert) if result.passing => {
            alerts::resolve(state, &alert.id, rule, Resolution::Cleared, None, now).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Asks the device to run the monitors (all of them when `rule_ids` is
/// empty) now. Returns whether its Agent is connected.
pub fn check_now(state: &AppState, device_id: &str, rule_ids: Vec<String>) -> bool {
    state.agents.send(
        device_id,
        AgentCommand::CheckMonitors {
            monitor_ids: rule_ids,
        },
    )
}

/// A monitor rule as a device's page shows it.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceMonitor {
    pub rule_id: String,
    pub rule_name: String,
    pub condition: Condition,
    pub severity: Severity,
    /// What it alerts on, in words.
    pub description: String,
    pub interval_seconds: u32,
    /// Failed checks in a row that raise its alert.
    pub failures_before_alert: u32,
    /// What self-healing does, in words.
    pub remediation: Option<String>,
    /// Why the device doesn't run it, if it doesn't.
    pub problem: Option<String>,
    pub state: Option<MonitorState>,
    /// Its unresolved alert on the device.
    pub alert_id: Option<String>,
    pub alert_state: Option<String>,
    pub remediation_state: Option<String>,
}

/// The monitors that apply to the device, by name.
pub async fn for_device(state: &AppState, device_id: &str) -> db::Result<Vec<DeviceMonitor>> {
    let executor = &mut &state.database;
    let catalog = Catalog::load(state, Some(device_id)).await?;
    let mut states = store::for_device(executor, device_id)
        .await?
        .into_iter()
        .map(|state| (state.rule_id.clone(), state))
        .collect::<HashMap<_, _>>();
    let mut monitors = Vec::new();
    for (rule, definition) in catalog.applicable(device_id) {
        let alert = alert_store::unresolved_for(executor, &rule.id, device_id, "").await?;
        let monitor = rule.monitor.as_ref();
        monitors.push(DeviceMonitor {
            rule_id: rule.id.clone(),
            rule_name: rule.name.clone(),
            condition: rule.condition,
            severity: rule.severity,
            description: rule.describe(),
            interval_seconds: monitor.map_or(0, |monitor| monitor.interval_seconds),
            failures_before_alert: monitor.map_or(1, |monitor| monitor.failures),
            remediation: rule.describe_remediation(),
            problem: definition.err(),
            state: states.remove(&rule.id),
            alert_id: alert.as_ref().map(|alert| alert.id.clone()),
            alert_state: alert.as_ref().map(|alert| alert.state.clone()),
            remediation_state: alert.and_then(|alert| alert.remediation_state),
        });
    }
    Ok(monitors)
}

/// Whether the rule's monitor would be checked by the device. Used by the
/// API to refuse a check of a monitor that doesn't apply.
pub async fn applies(state: &AppState, device_id: &str, rule_id: &str) -> db::Result<bool> {
    let catalog = Catalog::load(state, Some(device_id)).await?;
    Ok(catalog
        .applicable(device_id)
        .iter()
        .any(|(rule, definition)| rule.id == rule_id && definition.is_ok()))
}

/// A rule's monitor changed what it checks: its old results no longer say
/// anything about the new check.
pub async fn rule_changed(state: &AppState, previous: &Rule, rule: &Rule) {
    let changed = previous.monitor.as_ref().map(|monitor| &monitor.check)
        != rule.monitor.as_ref().map(|monitor| &monitor.check);
    if changed && let Err(error) = store::delete_for_rule(&mut &state.database, &rule.id).await {
        tracing::warn!(rule_id = rule.id, %error, "could not forget a changed monitor's results");
    }
    state.monitors.wake();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_follow_the_monitors() {
        let monitor = |interval| MonitorDefinition {
            id: "r".into(),
            interval_seconds: interval,
            timeout_seconds: 10,
            check: MonitorCheck::Ping { host: "gw".into() },
        };
        assert_eq!(fingerprint(&[monitor(60)]), fingerprint(&[monitor(60)]));
        assert_ne!(fingerprint(&[monitor(60)]), fingerprint(&[monitor(120)]));
        assert_ne!(fingerprint(&[]), fingerprint(&[monitor(60)]));
        assert_eq!(storable("a\0b", 2), "a");
    }

    #[test]
    fn owners_are_told_only_what_they_lack() {
        assert_eq!(lacks("Ada", &[]), None);
        assert_eq!(
            lacks("Ada", &[Permission::ScriptsRun]).unwrap(),
            "Ada, whose access it runs with, no longer has the scripts.run permission; save the rule to run it with your access"
        );
        assert!(
            lacks("Ada", &[Permission::AlertsManage, Permission::ScriptsRun])
                .unwrap()
                .contains("no longer has the alerts.manage and scripts.run permissions;")
        );
    }
}
