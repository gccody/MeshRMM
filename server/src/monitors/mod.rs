//! Monitors: alert rules whose condition the device's Agent checks, and
//! self-healing. See docs/monitors.md.
//!
//! The engine's task keeps each connected Agent's monitors current: it sends
//! a device the monitors that apply to it when it connects
//! ([`greeting`]), and again whenever that set changes, because a rule, its
//! targets, the device's company or tags, a script or a rule owner's access
//! changed. A presence change looks again at the devices it names; a rule
//! change, a deleted tag and every [`RESYNC_INTERVAL`] look at every
//! connected device, at most once per [`FULL_PASS_SPACING`]. The monitors
//! sent on connecting carry what the server holds of their latest results,
//! so the Agent sends only those that differ.
//!
//! Agents report results ([`reported`]): failures and changes at once,
//! passes of passing monitors together every
//! [`meshrmm_protocol_types::MONITOR_REFRESH_INTERVAL_SECONDS`]. The server
//! keeps the latest per monitor and device, and raises and resolves the
//! rule's alerts through the alert engine, a message's results together. An
//! offline device reports nothing, so its alerts stay as they are.
pub mod remediation;
pub mod settings;
pub mod store;

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use meshrmm_protocol_types::{
    AgentCommand, DevicePlatform, MAX_MONITOR_DETAIL_CHARS, MAX_MONITOR_OUTPUT_BYTES, MAX_MONITORS,
    MonitorCheck, MonitorDefinition, MonitorResult, ScriptLanguage, truncate_text,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::Serialize;
use tokio::sync::{
    Notify,
    broadcast::error::{RecvError, TryRecvError},
};

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
    realtime::presence::PresenceEvent,
    targets,
    time::now_ms,
    users,
};

/// How often every connected device's monitors are compared with what it
/// was sent, catching changes nothing announced, such as a role's.
pub const RESYNC_INTERVAL: Duration = Duration::from_secs(60);
/// Changes this close together are handled together.
const SETTLE: Duration = Duration::from_millis(250);
/// Above this many devices named in one settling window, one pass over every
/// connected device is cheaper than reading each: a device costs three small
/// reads, while a full pass reads every device and its tags and hashes every
/// connected device's set.
const MAX_DEVICE_SYNCS: usize = 200;
/// The least time between two passes over every connected device, so a
/// reconnect storm naming more than [`MAX_DEVICE_SYNCS`] devices in every
/// window reads the fleet once per this long rather than back to back.
pub const FULL_PASS_SPACING: Duration = Duration::from_secs(5);
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
    /// Whether the next pass looks at every connected device.
    everyone: AtomicBool,
    /// Devices that get their monitors at the next pass even if unchanged.
    forced: Mutex<HashSet<String>>,
    /// What each connected device was last sent.
    sent: Mutex<HashMap<String, Offer>>,
    rules: Mutex<Option<(Instant, Arc<RuleSet>)>>,
    full_passes: AtomicU64,
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
        self.forget_rules();
        self.inner.everyone.store(true, Ordering::Relaxed);
        self.inner.wake.notify_one();
    }

    /// How many times every connected device's monitors were compared with
    /// what it was sent.
    pub fn full_passes(&self) -> u64 {
        self.inner.full_passes.load(Ordering::Relaxed)
    }

    fn forget_rules(&self) {
        *lock(&self.inner.rules) = None;
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
        self.inner.wake.notify_one();
    }

    /// The IDs of the monitors the connected device was last sent, if it
    /// was sent any on this connection.
    fn offered(&self, device_id: &str) -> Option<Arc<HashSet<String>>> {
        lock(&self.inner.sent)
            .get(device_id)
            .map(|offer| Arc::clone(&offer.monitor_ids))
    }
}

/// The monitors a connected device was last sent.
#[derive(Debug)]
struct Offer {
    fingerprint: u64,
    /// Their IDs: the monitors whose results the device reports.
    monitor_ids: Arc<HashSet<String>>,
}

impl Offer {
    fn of(monitors: &[MonitorDefinition]) -> Self {
        Self {
            fingerprint: fingerprint(monitors),
            monitor_ids: Arc::new(monitors.iter().map(|monitor| monitor.id.clone()).collect()),
        }
    }
}

fn fingerprint(monitors: &[MonitorDefinition]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(monitors)
        .expect("monitors serialize")
        .hash(&mut hasher);
    hasher.finish()
}

/// What the engine has yet to look at.
#[derive(Debug, Default)]
struct Changes {
    /// Every connected device, because something fleet-wide changed.
    everyone: bool,
    /// The rules read last may be out of date.
    stale_rules: bool,
    devices: HashSet<String>,
}

/// What one pass looks at.
#[derive(Debug, PartialEq, Eq)]
enum Pass {
    Everyone,
    /// These devices; none when there is nothing to do.
    Devices(HashSet<String>),
}

impl Changes {
    fn note(&mut self, event: Result<Arc<PresenceEvent>, RecvError>) {
        let Ok(event) = event else {
            // Which devices changed meanwhile is lost, and a tag may have
            // been deleted among them.
            self.everyone = true;
            self.stale_rules = true;
            return;
        };
        match &*event {
            // A device's company, tags and platform are on its presence, so
            // a change to any of them names the device.
            PresenceEvent::AgentUpsert { agent, .. } => {
                self.devices.insert(agent.id.clone());
            }
            PresenceEvent::AgentDeleted { agent_id, .. } => {
                self.devices.insert(agent_id.clone());
            }
            // Deleting a tag also takes it out of the rules targeting it, and
            // a rule that needed all of several tags then covers the devices
            // that have the rest, which no event names.
            PresenceEvent::TagDeleted { .. } => {
                self.everyone = true;
                self.stale_rules = true;
            }
            PresenceEvent::Snapshot { .. } => self.everyone = true,
            // Names and colors decide nothing. A deleted company's devices
            // were moved out of it and are named on their own, and the rules
            // that targeted it covered only those.
            PresenceEvent::TagUpsert { .. }
            | PresenceEvent::CompanyUpsert { .. }
            | PresenceEvent::CompanyDeleted { .. } => {}
        }
    }

    /// What to look at now, given whether a full pass may run yet.
    fn next(&mut self, full_pass_allowed: bool) -> Pass {
        if self.devices.len() > MAX_DEVICE_SYNCS {
            self.everyone = true;
            self.devices.clear();
        }
        if self.everyone && full_pass_allowed {
            self.everyone = false;
            self.devices.clear();
            return Pass::Everyone;
        }
        // Until a full pass may run, a few devices are still looked at one
        // by one.
        Pass::Devices(std::mem::take(&mut self.devices))
    }
}

/// Runs the engine until the returned task is aborted.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut presence = state.presence.subscribe();
        let mut resync = tokio::time::interval(RESYNC_INTERVAL);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut changes = Changes::default();
        let mut last_full_pass: Option<tokio::time::Instant> = None;
        loop {
            // A full pass held back by the spacing runs once it may.
            let held_back = changes.everyone.then(|| {
                last_full_pass.map_or_else(tokio::time::Instant::now, |at| at + FULL_PASS_SPACING)
            });
            tokio::select! {
                _ = resync.tick() => changes.everyone = true,
                () = state.monitors.inner.wake.notified() => {}
                event = presence.recv() => {
                    if matches!(event, Err(RecvError::Closed)) {
                        return;
                    }
                    changes.note(event);
                }
                () = tokio::time::sleep_until(held_back.unwrap_or_else(tokio::time::Instant::now)),
                    if held_back.is_some() => {}
            }
            tokio::time::sleep(SETTLE).await;
            loop {
                match presence.try_recv() {
                    Ok(event) => changes.note(Ok(event)),
                    Err(TryRecvError::Lagged(skipped)) => {
                        changes.note(Err(RecvError::Lagged(skipped)));
                    }
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                }
            }
            if std::mem::take(&mut changes.stale_rules) {
                state.monitors.forget_rules();
            }
            changes.everyone |= state.monitors.inner.everyone.swap(false, Ordering::Relaxed);
            changes
                .devices
                .extend(lock(&state.monitors.inner.forced).iter().cloned());
            let allowed = last_full_pass.is_none_or(|at| at.elapsed() >= FULL_PASS_SPACING);
            let result = match changes.next(allowed) {
                Pass::Everyone => {
                    last_full_pass = Some(tokio::time::Instant::now());
                    sync(&state).await
                }
                Pass::Devices(devices) => {
                    let mut result = Ok(());
                    for device_id in &devices {
                        if let Err(error) = sync_device(&state, device_id).await {
                            result = Err(error);
                        }
                    }
                    result
                }
            };
            if let Err(error) = result {
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
    state
        .monitors
        .inner
        .full_passes
        .fetch_add(1, Ordering::Relaxed);
    let connected = state.agents.connected();
    if connected.is_empty() {
        lock(&state.monitors.inner.sent).clear();
        return Ok(());
    }
    let catalog = Catalog::load(state, None).await?;
    let forced = std::mem::take(&mut *lock(&state.monitors.inner.forced));
    for device_id in &connected {
        offer(state, &catalog, device_id, forced.contains(device_id));
    }
    lock(&state.monitors.inner.sent).retain(|device_id, _| connected.contains(device_id));
    Ok(())
}

/// Sends the device its new set if its monitors changed, reading only what
/// they depend on, or forgets it if its Agent isn't connected.
async fn sync_device(state: &AppState, device_id: &str) -> anyhow::Result<()> {
    if !state.agents.is_connected(device_id) {
        lock(&state.monitors.inner.forced).remove(device_id);
        lock(&state.monitors.inner.sent).remove(device_id);
        return Ok(());
    }
    let catalog = Catalog::load(state, Some(device_id)).await?;
    let forced = lock(&state.monitors.inner.forced).remove(device_id);
    offer(state, &catalog, device_id, forced);
    Ok(())
}

/// Sends the connected device its monitors in `catalog` if they differ from
/// what it was last sent, or if `forced`.
fn offer(state: &AppState, catalog: &Catalog, device_id: &str, forced: bool) {
    let monitors = catalog.definitions(device_id);
    let offer = Offer::of(&monitors);
    let previous = lock(&state.monitors.inner.sent)
        .get(device_id)
        .map_or_else(|| fingerprint(&[]), |offer| offer.fingerprint);
    if offer.fingerprint == previous && !forced {
        return;
    }
    let count = monitors.len();
    // A connected Agent has said what it found already.
    let command = AgentCommand::SetMonitors {
        monitors,
        known: None,
    };
    if state.agents.send(device_id, command) {
        tracing::info!(device_id, monitors = count, "sent the device its monitors");
        lock(&state.monitors.inner.sent).insert(device_id.to_owned(), offer);
    }
}

/// The monitors to send an Agent that just connected, if it has any, with
/// what the server holds of their latest results, so the Agent sends only
/// the results that differ. A device without monitors costs no more reads.
pub async fn greeting(state: &AppState, device_id: &str) -> anyhow::Result<Option<AgentCommand>> {
    let catalog = Catalog::load(state, Some(device_id)).await?;
    let monitors = catalog.definitions(device_id);
    let offer = Offer::of(&monitors);
    let ids = Arc::clone(&offer.monitor_ids);
    lock(&state.monitors.inner.sent).insert(device_id.to_owned(), offer);
    if monitors.is_empty() {
        return Ok(None);
    }
    let known = store::known(&mut &state.database, device_id)
        .await?
        .into_iter()
        .filter(|known| ids.contains(&known.monitor_id))
        .collect();
    Ok(Some(AgentCommand::SetMonitors {
        monitors,
        known: Some(known),
    }))
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
    /// Where each rule is in `rules`, by ID.
    positions: HashMap<String, usize>,
    scripts: HashMap<String, Script>,
}

impl RuleSet {
    fn rule(&self, id: &str) -> Option<&Rule> {
        self.positions
            .get(id)
            .map(|position| &self.rules[*position].0)
    }

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
        let positions = rules
            .iter()
            .enumerate()
            .map(|(position, (rule, _))| (rule.id.clone(), position))
            .collect();
        Ok(Self {
            rules,
            positions,
            scripts,
        })
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
        // Without rules no device has monitors, whatever it is, so most
        // instances' connects read nothing here.
        if rules.rules.is_empty() {
            return Ok(Self {
                rules,
                devices: HashMap::new(),
                platforms: HashMap::new(),
            });
        }
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
///
/// A message's results are taken together: matched against the monitors
/// the device was sent and the rules at hand, kept in one statement, and
/// judged against the device's unresolved alerts of those rules, read in
/// one. A message of passes with no alert open costs those two statements
/// and no alert events, however many results it carries.
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
    let rules = state.monitors.rules(&mut database).await?;
    // What the device was sent on this connection is what it runs; only
    // without that are its row, tags and platform read to decide.
    let offered = match state.monitors.offered(device_id) {
        Some(offered) => offered,
        None => Arc::new(
            Catalog::load(state, Some(device_id))
                .await?
                .definitions(device_id)
                .into_iter()
                .map(|definition| definition.id)
                .collect(),
        ),
    };
    let mut stale = false;
    let mut judged: Vec<(&Rule, MonitorState)> = Vec::new();
    for result in results.into_iter().take(MAX_MONITORS) {
        let rule = offered
            .contains(&result.monitor_id)
            .then(|| rules.rule(&result.monitor_id))
            .flatten();
        let Some(rule) = rule else {
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
        // A monitor's later result in the same message stands for it.
        match judged.iter_mut().find(|(judged, _)| judged.id == rule.id) {
            Some((_, earlier)) => *earlier = monitor_state,
            None => judged.push((rule, monitor_state)),
        }
    }
    if stale {
        state.monitors.resend(device_id);
    }
    if judged.is_empty() {
        return Ok(());
    }
    let states = judged
        .iter()
        .map(|(_, monitor_state)| monitor_state.clone())
        .collect::<Vec<_>>();
    store::upsert(&mut database, &states).await?;
    let rule_ids = judged
        .iter()
        .map(|(rule, _)| rule.id.as_str())
        .collect::<Vec<_>>();
    let mut open = alert_store::unresolved_of_rules(&mut database, device_id, &rule_ids)
        .await?
        .into_iter()
        .map(|alert| (alert.rule_id.clone(), alert))
        .collect::<HashMap<_, _>>();
    for (rule, monitor_state) in &judged {
        judge(state, rule, monitor_state, open.remove(&rule.id), now).await?;
    }
    state.alerts.publish(AlertEvent::MonitorUpdate {
        device_id: device_id.to_owned(),
        states,
    });
    Ok(())
}

/// Raises the rule's alert once enough checks in a row failed, keeps its
/// unresolved alert (`open`) describing the latest failure, and resolves it
/// once a check passes. A pass, or a failure below the threshold, with no
/// alert open costs nothing.
async fn judge(
    state: &AppState,
    rule: &Rule,
    result: &MonitorState,
    open: Option<alert_store::Alert>,
    now: i64,
) -> anyhow::Result<()> {
    let threshold = rule.monitor.as_ref().map_or(1, |monitor| monitor.failures);
    let alerting = !result.passing && result.failures >= i64::from(threshold);
    let mut database = &state.database;
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

    fn upsert(id: &str) -> Result<Arc<PresenceEvent>, RecvError> {
        Ok(Arc::new(PresenceEvent::AgentUpsert {
            revision: 1,
            agent: crate::realtime::presence::PresenceAgent {
                id: id.into(),
                name: id.into(),
                platform: "linux".into(),
                connected: true,
                updating_to: None,
                company_id: None,
                tag_ids: vec![],
                notes_revision: 0,
                created_at: 0,
                system: None,
            },
        }))
    }

    fn devices(ids: &[&str]) -> Pass {
        Pass::Devices(ids.iter().map(|id| (*id).to_owned()).collect())
    }

    #[test]
    fn presence_changes_are_looked_at_device_by_device() {
        let mut changes = Changes::default();
        // A device that flaps is looked at once.
        changes.note(upsert("a"));
        changes.note(upsert("a"));
        changes.note(Ok(Arc::new(PresenceEvent::AgentDeleted {
            revision: 2,
            agent_id: "b".into(),
        })));
        changes.note(Ok(Arc::new(PresenceEvent::CompanyDeleted {
            revision: 3,
            company_id: "c".into(),
        })));
        assert_eq!(changes.next(true), devices(&["a", "b"]));
        assert_eq!(changes.next(true), devices(&[]));

        // A deleted tag changes rules' targets, and a subscriber that fell
        // behind doesn't know what changed: both look at everyone, with the
        // rules read again.
        changes.note(Ok(Arc::new(PresenceEvent::TagDeleted {
            revision: 4,
            tag_id: "t".into(),
        })));
        assert!(changes.stale_rules);
        assert_eq!(changes.next(true), Pass::Everyone);
        changes.stale_rules = false;
        changes.note(Err(RecvError::Lagged(3)));
        assert!(changes.stale_rules);

        // Until the full pass may run, devices are still looked at one by
        // one, and the full pass waits.
        changes.note(upsert("c"));
        assert_eq!(changes.next(false), devices(&["c"]));
        assert!(changes.everyone);
        assert_eq!(changes.next(true), Pass::Everyone);
        assert_eq!(changes.next(true), devices(&[]));

        // So many devices at once make one full pass instead, held back by
        // the spacing like any other.
        for device in 0..=MAX_DEVICE_SYNCS {
            changes.note(upsert(&device.to_string()));
        }
        assert_eq!(changes.next(false), devices(&[]));
        changes.note(upsert("d"));
        assert_eq!(changes.next(false), devices(&["d"]));
        assert_eq!(changes.next(true), Pass::Everyone);
        assert_eq!(changes.next(true), devices(&[]));
    }
}
