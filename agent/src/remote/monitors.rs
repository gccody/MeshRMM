//! Monitors: checks the server asks the Agent to run on its own schedule,
//! and the self-healing actions it asks for when one fails. See
//! docs/monitors.md.
//!
//! The server sends the device's monitors when the Agent connects and when
//! they change. A scheduler thread runs each one every interval, starting
//! within its first interval (at most 30 seconds in) and then a little late
//! each time, so a fleet's devices don't all check at once. Each check runs
//! on its own thread, at most [`MAX_RUNNING_CHECKS`] at a time and
//! [`MAX_RUNNING_SCRIPTS`] of them script checks, and is bounded by the
//! monitor's timeout. A check that overruns counts as failing, and the
//! monitor doesn't run again until it finishes.
//!
//! Results reach the coordinator through [`Monitors::next`]: a failure or a
//! change from passing to failing or back at once, the rest together every
//! [`MONITOR_STATUS_INTERVAL_SECONDS`], and every monitor's latest result
//! again when the Agent reconnects.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{
    AgentStatusMessage, MAX_MONITOR_DETAIL_CHARS, MAX_MONITOR_FAILURES, MAX_MONITOR_OUTPUT_BYTES,
    MAX_MONITORS, MONITOR_STATUS_INTERVAL_SECONDS, MonitorAction, MonitorActionResult,
    MonitorCheck, MonitorDefinition, MonitorResult, truncate_text,
};

use super::config::ExecutionMode;

mod checks;
#[cfg(unix)]
mod command;
mod network;
// Each operating system reads only its own tools' output.
#[cfg_attr(not(test), allow(dead_code))]
mod parse;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

const MAX_RUNNING_CHECKS: usize = 8;
const MAX_RUNNING_SCRIPTS: usize = 2;
const MAX_RUNNING_ACTIONS: usize = 4;
/// The most a first check waits, and the most a later one runs late.
const MAX_JITTER: Duration = Duration::from_secs(30);
/// How long past its timeout a check may take before it counts as failing.
const OVERRUN_GRACE: Duration = Duration::from_secs(5);
/// Results sent in one message.
const RESULTS_PER_MESSAGE: usize = 32;

/// What a check found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) passing: bool,
    pub(crate) value: Option<f64>,
    pub(crate) detail: String,
    pub(crate) output: String,
}

impl Outcome {
    pub(crate) fn passing(detail: impl Into<String>) -> Self {
        Self {
            passing: true,
            value: None,
            detail: detail.into(),
            output: String::new(),
        }
    }

    pub(crate) fn failing(detail: impl Into<String>) -> Self {
        Self {
            passing: false,
            ..Self::passing(detail)
        }
    }

    pub(crate) fn when(passing: bool, detail: impl Into<String>) -> Self {
        Self {
            passing,
            ..Self::passing(detail)
        }
    }

    pub(crate) fn value(mut self, value: f64) -> Self {
        self.value = value.is_finite().then_some(value);
        self
    }

    pub(crate) fn output(mut self, output: impl Into<String>) -> Self {
        self.output = output.into();
        self
    }
}

/// What a monitor remembers from one check to the next.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Memory {
    /// When it last checked, in Unix milliseconds.
    pub(crate) checked_at_ms: Option<i64>,
    /// Since when a process has been over its threshold.
    pub(crate) over_since_ms: Option<i64>,
}

/// What a check runs with.
pub(crate) struct Context {
    pub(crate) mode: ExecutionMode,
    pub(crate) interval: Duration,
    pub(crate) timeout: Duration,
    pub(crate) now_ms: i64,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// The latest results, waiting for the coordinator.
#[derive(Default)]
struct Pending {
    latest: HashMap<String, MonitorResult>,
    /// Monitors whose latest result the server hasn't been sent.
    changed: BTreeSet<String>,
    actions: Vec<MonitorActionResult>,
    /// Whether to send now rather than with the next status.
    urgent: bool,
    flushed: Option<Instant>,
}

struct Outbox {
    pending: Mutex<Pending>,
    ready: tokio::sync::Notify,
}

impl Outbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn result(&self, result: MonitorResult, urgent: bool) {
        let mut pending = self.lock();
        pending.changed.insert(result.monitor_id.clone());
        pending.latest.insert(result.monitor_id.clone(), result);
        pending.urgent |= urgent;
        drop(pending);
        if urgent {
            self.ready.notify_one();
        }
    }

    fn action(&self, result: MonitorActionResult) {
        let mut pending = self.lock();
        pending.actions.push(result);
        pending.urgent = true;
        drop(pending);
        self.ready.notify_one();
    }

    /// Forgets the results of monitors the device no longer runs.
    fn retain(&self, ids: &BTreeSet<String>) {
        let mut pending = self.lock();
        pending.latest.retain(|id, _| ids.contains(id));
        pending.changed.retain(|id| ids.contains(id));
    }
}

enum Command {
    Set(Vec<MonitorDefinition>),
    CheckNow(Vec<String>),
    Done {
        run: u64,
        checked_at_ms: i64,
        outcome: Outcome,
        memory: Memory,
    },
}

/// The device's monitors, as the coordinator sees them.
pub(crate) struct Monitors {
    commands: mpsc::Sender<Command>,
    outbox: Arc<Outbox>,
    actions: Arc<std::sync::atomic::AtomicUsize>,
}

impl Monitors {
    pub(crate) fn new(mode: ExecutionMode) -> Self {
        let (commands, receiver) = mpsc::channel();
        let outbox = Arc::new(Outbox {
            pending: Mutex::default(),
            ready: tokio::sync::Notify::new(),
        });
        let scheduler = Scheduler {
            mode,
            entries: BTreeMap::new(),
            runs: HashMap::new(),
            next_run: 0,
            random: Random::new(),
            outbox: Arc::clone(&outbox),
            commands: commands.clone(),
        };
        if let Err(error) = std::thread::Builder::new()
            .name("meshrmm-monitors".into())
            .spawn(move || scheduler.run(&receiver))
        {
            tracing::error!(%error, "could not start the monitor scheduler");
        }
        Self {
            commands,
            outbox,
            actions: Arc::default(),
        }
    }

    /// Runs `monitors` from now on, in place of the ones before.
    pub(crate) fn set(&self, monitors: Vec<MonitorDefinition>) {
        let _ = self.commands.send(Command::Set(monitors));
    }

    /// Runs the monitors in `ids`, or every one when it is empty, now.
    pub(crate) fn check_now(&self, ids: Vec<String>) {
        let _ = self.commands.send(Command::CheckNow(ids));
    }

    /// Runs a self-healing action on its own thread.
    pub(crate) fn act(&self, action: MonitorAction) {
        use std::sync::atomic::Ordering;
        let outbox = Arc::clone(&self.outbox);
        let running = Arc::clone(&self.actions);
        let action_id = action.action_id.clone();
        if running
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_RUNNING_ACTIONS).then_some(count + 1)
            })
            .is_err()
        {
            outbox.action(MonitorActionResult {
                action_id,
                succeeded: false,
                detail: format!(
                    "The device is already running {MAX_RUNNING_ACTIONS} self-healing actions."
                ),
            });
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("meshrmm-monitor-action".into())
            .spawn(move || {
                let result = checks::act(&action.action);
                running.fetch_sub(1, Ordering::AcqRel);
                let (succeeded, detail) = match result {
                    Ok(detail) => (true, detail),
                    Err(error) => (false, format!("{error:#}")),
                };
                tracing::info!(
                    action_id = %action.action_id,
                    monitor_id = %action.monitor_id,
                    succeeded,
                    detail,
                    "self-healing action finished"
                );
                outbox.action(MonitorActionResult {
                    action_id: action.action_id,
                    succeeded,
                    detail: truncate_text(&detail, MAX_MONITOR_DETAIL_CHARS).to_owned(),
                });
            });
        if let Err(error) = spawned {
            self.actions.fetch_sub(1, Ordering::AcqRel);
            self.outbox.action(MonitorActionResult {
                action_id,
                succeeded: false,
                detail: format!("Could not start the action: {error}"),
            });
        }
    }

    /// The server hears every monitor's latest result again: it may have
    /// missed some while the Agent was away.
    pub(crate) fn connected(&self) {
        let mut pending = self.outbox.lock();
        pending.changed = pending.latest.keys().cloned().collect();
        pending.urgent = true;
        drop(pending);
        self.outbox.ready.notify_one();
    }

    /// Completes with the messages to send the server. Cancel safe.
    pub(crate) async fn next(&self) -> Vec<AgentStatusMessage> {
        let status = Duration::from_secs(MONITOR_STATUS_INTERVAL_SECONDS);
        loop {
            let wait = {
                let mut pending = self.outbox.lock();
                let since = pending.flushed.map_or(status, |flushed| flushed.elapsed());
                let waiting = !pending.changed.is_empty() || !pending.actions.is_empty();
                if waiting && (pending.urgent || since >= status) {
                    return take(&mut pending);
                }
                status.saturating_sub(since).max(Duration::from_secs(1))
            };
            tokio::select! {
                () = self.outbox.ready.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Messages [`Self::next`] gave that couldn't be sent go again.
    pub(crate) fn unsent(&self, messages: Vec<AgentStatusMessage>) {
        let mut pending = self.outbox.lock();
        for message in messages {
            match message {
                AgentStatusMessage::MonitorResults { results } => {
                    for result in results {
                        if pending.latest.contains_key(&result.monitor_id) {
                            pending.changed.insert(result.monitor_id);
                        }
                    }
                }
                AgentStatusMessage::MonitorActionDone { result } => pending.actions.push(result),
                _ => {}
            }
        }
    }
}

fn take(pending: &mut Pending) -> Vec<AgentStatusMessage> {
    pending.urgent = false;
    pending.flushed = Some(Instant::now());
    let results = std::mem::take(&mut pending.changed)
        .into_iter()
        .filter_map(|id| pending.latest.get(&id).cloned())
        .collect::<Vec<_>>();
    let mut messages = results
        .chunks(RESULTS_PER_MESSAGE)
        .map(|chunk| AgentStatusMessage::MonitorResults {
            results: chunk.to_vec(),
        })
        .collect::<Vec<_>>();
    messages.extend(
        std::mem::take(&mut pending.actions)
            .into_iter()
            .map(|result| AgentStatusMessage::MonitorActionDone { result }),
    );
    messages
}

/// A small pseudo-random generator for jitter.
struct Random(u64);

impl Random {
    fn new() -> Self {
        let seed = uuid::Uuid::new_v4().as_u64_pair().0;
        Self(seed | 1)
    }

    /// A duration from zero up to `max`.
    fn up_to(&mut self, max: Duration) -> Duration {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let millis = max.as_millis() as u64;
        Duration::from_millis(if millis == 0 {
            0
        } else {
            self.0 % (millis + 1)
        })
    }
}

struct Entry {
    definition: MonitorDefinition,
    /// Why the Agent won't run it, when the definition is invalid.
    invalid: Option<String>,
    due: Instant,
    running: Option<u64>,
    memory: Memory,
    failures: u32,
    passing: Option<bool>,
    /// Whether its next result goes at once.
    urgent: bool,
}

impl Entry {
    fn interval(&self) -> Duration {
        Duration::from_secs(self.definition.interval_seconds.max(1).into())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.definition.timeout_seconds.max(1).into())
    }
}

struct Run {
    monitor_id: String,
    script: bool,
    deadline: Instant,
    /// Whether it overran and was reported failing already.
    abandoned: bool,
}

struct Scheduler {
    mode: ExecutionMode,
    entries: BTreeMap<String, Entry>,
    runs: HashMap<u64, Run>,
    next_run: u64,
    random: Random,
    outbox: Arc<Outbox>,
    commands: mpsc::Sender<Command>,
}

impl Scheduler {
    fn run(mut self, receiver: &mpsc::Receiver<Command>) {
        loop {
            let now = Instant::now();
            let next = self
                .entries
                .values()
                .filter(|entry| entry.running.is_none())
                .map(|entry| entry.due)
                .chain(
                    self.runs
                        .values()
                        .filter(|run| !run.abandoned)
                        .map(|run| run.deadline),
                )
                .min();
            let wait = next.map_or(Duration::from_secs(60), |next| {
                next.saturating_duration_since(now)
                    .min(Duration::from_secs(60))
            });
            match receiver.recv_timeout(wait) {
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            self.expire();
            self.start_due();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Set(definitions) => self.set(definitions),
            Command::CheckNow(ids) => {
                let now = Instant::now();
                for (id, entry) in &mut self.entries {
                    if ids.is_empty() || ids.contains(id) {
                        entry.urgent = true;
                        if entry.running.is_none() {
                            entry.due = now;
                        }
                    }
                }
            }
            Command::Done {
                run,
                checked_at_ms,
                outcome,
                memory,
            } => self.finished(run, checked_at_ms, outcome, memory),
        }
    }

    fn set(&mut self, definitions: Vec<MonitorDefinition>) {
        let now = Instant::now();
        let mut previous = std::mem::take(&mut self.entries);
        for definition in definitions.into_iter().take(MAX_MONITORS) {
            let id = definition.id.clone();
            if let Some(entry) = previous
                .remove(&id)
                .filter(|entry| entry.definition == definition)
            {
                self.entries.insert(id, entry);
                continue;
            }
            let mut check = definition.check.clone();
            let invalid = check
                .validate()
                .err()
                .map(|problem| format!("The monitor is invalid: {problem}."));
            let first = self
                .random
                .up_to(Duration::from_secs(definition.interval_seconds.into()).min(MAX_JITTER));
            self.entries.insert(
                id,
                Entry {
                    definition: MonitorDefinition {
                        check,
                        ..definition
                    },
                    invalid,
                    due: now + first,
                    running: None,
                    memory: Memory::default(),
                    failures: 0,
                    passing: None,
                    urgent: false,
                },
            );
        }
        let ids = self.entries.keys().cloned().collect();
        self.outbox.retain(&ids);
        tracing::info!(
            monitors = self.entries.len(),
            "the server set the device's monitors"
        );
    }

    /// Checks that overran their timeout count as failing.
    fn expire(&mut self) {
        let now = Instant::now();
        let overrun = self
            .runs
            .iter_mut()
            .filter(|(_, run)| !run.abandoned && run.deadline <= now)
            .map(|(id, run)| {
                run.abandoned = true;
                (*id, run.monitor_id.clone())
            })
            .collect::<Vec<_>>();
        for (run, monitor_id) in overrun {
            let Some(entry) = self.entries.get(&monitor_id) else {
                continue;
            };
            if entry.running != Some(run) {
                continue;
            }
            let outcome = Outcome::failing(format!(
                "The check didn't finish within {} seconds.",
                entry.definition.timeout_seconds
            ));
            let memory = entry.memory.clone();
            tracing::warn!(monitor_id, "a monitor's check overran its timeout");
            self.record(&monitor_id, now_ms(), outcome, memory);
        }
    }

    fn start_due(&mut self) {
        let now = Instant::now();
        let mut due = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.running.is_none() && entry.due <= now)
            .map(|(id, entry)| (entry.due, id.clone()))
            .collect::<Vec<_>>();
        due.sort();
        for (_, id) in due {
            let active = self.runs.values().filter(|run| !run.abandoned);
            let (checks, scripts) = active.fold((0, 0), |(checks, scripts), run| {
                (checks + 1, scripts + usize::from(run.script))
            });
            if checks >= MAX_RUNNING_CHECKS {
                break;
            }
            let script = matches!(
                self.entries[&id].definition.check,
                MonitorCheck::ScriptCheck { .. }
            );
            if script && scripts >= MAX_RUNNING_SCRIPTS {
                continue;
            }
            self.start(&id, now);
        }
    }

    fn start(&mut self, id: &str, now: Instant) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        let interval = entry.interval();
        let late = self.random.up_to((interval / 10).min(MAX_JITTER));
        entry.due = now + interval + late;
        let checked_at_ms = now_ms();
        if let Some(problem) = entry.invalid.clone() {
            let memory = entry.memory.clone();
            self.record(id, checked_at_ms, Outcome::failing(problem), memory);
            return;
        }
        let run = self.next_run;
        self.next_run += 1;
        entry.running = Some(run);
        let script = matches!(entry.definition.check, MonitorCheck::ScriptCheck { .. });
        // A script check's own timeout stops it; this only catches a hang.
        self.runs.insert(
            run,
            Run {
                monitor_id: id.to_owned(),
                script,
                deadline: now + entry.timeout() + OVERRUN_GRACE,
                abandoned: false,
            },
        );
        let check = entry.definition.check.clone();
        let mut memory = entry.memory.clone();
        let context = Context {
            mode: self.mode,
            interval,
            timeout: entry.timeout(),
            now_ms: checked_at_ms,
        };
        let commands = self.commands.clone();
        let spawned = std::thread::Builder::new()
            .name("meshrmm-monitor-check".into())
            .spawn(move || {
                let outcome = checks::run(&check, &context, &mut memory);
                memory.checked_at_ms = Some(context.now_ms);
                let _ = commands.send(Command::Done {
                    run,
                    checked_at_ms: context.now_ms,
                    outcome,
                    memory,
                });
            });
        if let Err(error) = spawned {
            self.runs.remove(&run);
            let memory = self.entries[id].memory.clone();
            self.record(
                id,
                checked_at_ms,
                Outcome::failing(format!("Could not start the check: {error}")),
                memory,
            );
        }
    }

    fn finished(&mut self, run: u64, checked_at_ms: i64, outcome: Outcome, memory: Memory) {
        let Some(finished) = self.runs.remove(&run) else {
            return;
        };
        let Some(entry) = self.entries.get_mut(&finished.monitor_id) else {
            return;
        };
        if entry.running != Some(run) {
            return;
        }
        entry.running = None;
        // An overrun was reported when it overran.
        if finished.abandoned {
            entry.memory.checked_at_ms = memory.checked_at_ms;
            return;
        }
        self.record(&finished.monitor_id, checked_at_ms, outcome, memory);
    }

    /// Keeps a result and hands it to the coordinator.
    fn record(&mut self, id: &str, checked_at_ms: i64, outcome: Outcome, memory: Memory) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        let changed = entry.passing != Some(outcome.passing);
        entry.passing = Some(outcome.passing);
        entry.failures = if outcome.passing {
            0
        } else {
            (entry.failures + 1).min(MAX_MONITOR_FAILURES)
        };
        entry.memory = memory;
        if entry.running.is_none() && entry.due <= Instant::now() {
            entry.due = Instant::now() + entry.interval();
        }
        let urgent = changed || !outcome.passing || std::mem::take(&mut entry.urgent);
        let result = MonitorResult {
            monitor_id: id.to_owned(),
            checked_at_unix_ms: checked_at_ms.max(0) as u64,
            passing: outcome.passing,
            failures: entry.failures,
            value: outcome.value,
            detail: truncate_text(&outcome.detail, MAX_MONITOR_DETAIL_CHARS).to_owned(),
            output: truncate_text(&outcome.output, MAX_MONITOR_OUTPUT_BYTES).to_owned(),
        };
        self.outbox.result(result, urgent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox() -> Arc<Outbox> {
        Arc::new(Outbox {
            pending: Mutex::default(),
            ready: tokio::sync::Notify::new(),
        })
    }

    fn scheduler(outbox: &Arc<Outbox>) -> (Scheduler, mpsc::Receiver<Command>) {
        let (commands, receiver) = mpsc::channel();
        (
            Scheduler {
                mode: ExecutionMode::Console,
                entries: BTreeMap::new(),
                runs: HashMap::new(),
                next_run: 0,
                random: Random(7),
                outbox: Arc::clone(outbox),
                commands,
            },
            receiver,
        )
    }

    fn port_monitor(id: &str, port: u16) -> MonitorDefinition {
        MonitorDefinition {
            id: id.into(),
            interval_seconds: 60,
            timeout_seconds: 5,
            check: MonitorCheck::TcpPort {
                host: "127.0.0.1".into(),
                port,
            },
        }
    }

    fn results(outbox: &Outbox) -> Vec<MonitorResult> {
        take(&mut outbox.lock())
            .into_iter()
            .flat_map(|message| match message {
                AgentStatusMessage::MonitorResults { results } => results,
                _ => vec![],
            })
            .collect()
    }

    #[test]
    fn jitter_stays_within_its_bound() {
        let mut random = Random(99);
        for _ in 0..1000 {
            assert!(random.up_to(Duration::from_secs(30)) <= Duration::from_secs(30));
        }
        assert_eq!(random.up_to(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn failures_count_up_and_reset_and_changes_go_at_once() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        scheduler.set(vec![port_monitor("one", 1)]);
        let first = scheduler.entries["one"].due;
        assert!(first <= Instant::now() + MAX_JITTER);

        scheduler.record("one", 1, Outcome::failing("closed"), Memory::default());
        scheduler.record("one", 2, Outcome::failing("closed"), Memory::default());
        assert!(outbox.lock().urgent);
        let sent = results(&outbox);
        assert_eq!(sent.len(), 1, "only the latest result is kept");
        assert_eq!((sent[0].failures, sent[0].passing), (2, false));

        scheduler.record(
            "one",
            3,
            Outcome::passing("open").value(1.5),
            Memory::default(),
        );
        assert!(outbox.lock().urgent, "recovering is a change");
        let sent = results(&outbox);
        assert_eq!((sent[0].failures, sent[0].value), (0, Some(1.5)));

        scheduler.record("one", 4, Outcome::passing("open"), Memory::default());
        assert!(!outbox.lock().urgent, "a steady pass waits for the status");
    }

    #[test]
    fn unchanged_monitors_keep_their_state_and_removed_ones_go() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        scheduler.set(vec![port_monitor("one", 1), port_monitor("two", 2)]);
        scheduler.record("one", 1, Outcome::failing("closed"), Memory::default());
        scheduler.record("two", 1, Outcome::failing("closed"), Memory::default());
        scheduler.set(vec![port_monitor("one", 1), port_monitor("two", 3)]);
        assert_eq!(scheduler.entries["one"].failures, 1);
        assert_eq!(
            scheduler.entries["two"].failures, 0,
            "a changed monitor starts over"
        );
        scheduler.set(vec![port_monitor("two", 3)]);
        let sent = results(&outbox);
        assert!(sent.iter().all(|result| result.monitor_id == "two"));
    }

    #[test]
    fn invalid_monitors_fail_with_the_reason() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        let mut monitor = port_monitor("bad", 1);
        monitor.check = MonitorCheck::Service {
            name: "-x".into(),
            automatic: false,
        };
        scheduler.set(vec![monitor]);
        scheduler.start("bad", Instant::now());
        let sent = results(&outbox);
        assert!(!sent[0].passing);
        assert!(
            sent[0].detail.starts_with("The monitor is invalid"),
            "{}",
            sent[0].detail
        );
        assert!(scheduler.runs.is_empty());
    }

    #[test]
    fn checks_run_on_their_threads_and_overruns_fail() {
        let outbox = outbox();
        let (mut scheduler, receiver) = scheduler(&outbox);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        scheduler.set(vec![port_monitor("open", port)]);
        scheduler.handle(Command::CheckNow(vec![]));
        scheduler.start_due();
        assert_eq!(scheduler.runs.len(), 1);
        let done = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        scheduler.handle(done);
        let sent = results(&outbox);
        assert!(sent[0].passing, "{}", sent[0].detail);
        assert!(sent[0].value.is_some());

        // A run past its deadline is reported failing, and its late result
        // is dropped.
        scheduler.handle(Command::CheckNow(vec![]));
        scheduler.start_due();
        let run = *scheduler.runs.keys().next().unwrap();
        scheduler.runs.get_mut(&run).unwrap().deadline = Instant::now();
        scheduler.expire();
        let sent = results(&outbox);
        assert!(!sent[0].passing);
        assert!(sent[0].detail.contains("didn't finish"));
        assert!(
            scheduler.entries["open"].running.is_some(),
            "it doesn't run twice"
        );
        scheduler.start_due();
        assert_eq!(scheduler.runs.len(), 1);
        let late = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        scheduler.handle(late);
        assert!(results(&outbox).is_empty());
        assert!(scheduler.entries["open"].running.is_none());
    }

    #[tokio::test]
    async fn the_coordinator_hears_changes_at_once_and_everything_on_connect() {
        let monitors = Monitors::new(ExecutionMode::Console);
        monitors.outbox.result(
            MonitorResult {
                monitor_id: "one".into(),
                checked_at_unix_ms: 1,
                passing: false,
                failures: 1,
                value: None,
                detail: "down".into(),
                output: String::new(),
            },
            true,
        );
        let messages = tokio::time::timeout(Duration::from_secs(5), monitors.next())
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        monitors.unsent(messages);
        assert_eq!(monitors.outbox.lock().changed.len(), 1);
        monitors.outbox.lock().changed.clear();
        monitors.connected();
        let messages = tokio::time::timeout(Duration::from_secs(5), monitors.next())
            .await
            .unwrap();
        assert!(matches!(
            &messages[..],
            [AgentStatusMessage::MonitorResults { results }] if results.len() == 1
        ));
    }
}
