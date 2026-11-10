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
//! monitor's timeout. A check due while the limits are reached waits,
//! without the scheduler waking for it, until a check finishes or overruns;
//! then the one due longest starts. A check that overruns counts as
//! failing, and the monitor doesn't run again until it finishes. A check
//! that breaks (panics), or whose thread can't start, counts as failing
//! too, and the monitor runs again at its next interval.
//!
//! Results reach the coordinator through [`Monitors::next`], measured
//! against what the server holds of each monitor (what it was last sent, or
//! what it said it holds when the Agent connected). A failure, a change
//! between passing and failing, the first result the server would hold, a
//! result Check now asked for and every self-healing action's go at once. A
//! pass the server already holds as passing waits: the latest results of
//! such monitors go together every [`MONITOR_REFRESH_INTERVAL_SECONDS`], from
//! a random point in the interval for each connection. On connecting the
//! Agent waits for the server's monitors and what it holds of them, then
//! sends only the results it lacks or holds with a different outcome or
//! count of failures.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{
    AgentStatusMessage, KnownMonitorResult, MAX_MONITOR_DETAIL_CHARS, MAX_MONITOR_FAILURES,
    MAX_MONITOR_OUTPUT_BYTES, MAX_MONITORS, MONITOR_REFRESH_INTERVAL_SECONDS, MonitorAction,
    MonitorActionResult, MonitorCheck, MonitorDefinition, MonitorResult, truncate_text,
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
/// The most text, in bytes of details and outputs, one message carries:
/// even escaped, well within what the server takes in one message. A
/// refresh of passing monitors usually fits in one, however many there are.
const MESSAGE_TEXT_BYTES: usize = 512 * 1024;
/// How long a new connection waits for the server's monitors before sending
/// what it has, if they don't come.
const GREETING_WAIT: Duration = Duration::from_secs(10);
/// How long messages the socket refused wait before they go again.
const RETRY_WAIT: Duration = Duration::from_secs(5);

fn refresh_interval() -> Duration {
    Duration::from_secs(MONITOR_REFRESH_INTERVAL_SECONDS)
}

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

/// What the server holds of a monitor's latest result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Held {
    checked_at_unix_ms: u64,
    passing: bool,
    failures: u32,
}

impl Held {
    fn of(result: &MonitorResult) -> Self {
        Self {
            checked_at_unix_ms: result.checked_at_unix_ms,
            passing: result.passing,
            failures: result.failures,
        }
    }

    /// Whether `result` says something this doesn't: another outcome or
    /// count of failures. A later pass of a passing monitor doesn't.
    fn differs(self, result: &MonitorResult) -> bool {
        self.passing != result.passing || self.failures != result.failures
    }
}

/// The latest results, waiting for the coordinator.
#[derive(Default)]
struct Pending {
    latest: HashMap<String, MonitorResult>,
    /// What the server holds of each monitor's latest result, as far as the
    /// Agent knows: what it was last sent on this connection, or what the
    /// server said it holds when the connection began. Unknown monitors are
    /// missing.
    held: HashMap<String, Held>,
    /// Monitors whose latest result goes at once.
    changed: BTreeSet<String>,
    actions: Vec<MonitorActionResult>,
    /// When the latest results of monitors that kept passing go next; none
    /// until the Agent connects.
    refresh_at: Option<Instant>,
    /// Nothing goes before this: a new connection waits for the server to
    /// say what it holds, and refused messages wait to go again.
    quiet_until: Option<Instant>,
}

impl Pending {
    /// Marks every monitor whose latest result the server doesn't hold to
    /// go, for a refresh.
    fn refresh(&mut self) {
        let behind = self
            .latest
            .iter()
            .filter(|(id, result)| {
                self.held
                    .get(*id)
                    .is_none_or(|held| held.checked_at_unix_ms != result.checked_at_unix_ms)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.changed.extend(behind);
    }

    /// The messages to send at `now`, or when to look again (`None` for
    /// when something changes).
    fn poll(&mut self, now: Instant) -> Result<Vec<AgentStatusMessage>, Option<Instant>> {
        if let Some(at) = self.refresh_at
            && at <= now
        {
            self.refresh_at = Some(now + refresh_interval());
            self.refresh();
        }
        let quiet = self.quiet_until.filter(|until| *until > now);
        if quiet.is_none() && (!self.changed.is_empty() || !self.actions.is_empty()) {
            return Ok(take(self));
        }
        Err(quiet.into_iter().chain(self.refresh_at).min())
    }
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

    /// Keeps a monitor's latest result, sending it at once unless it is a
    /// pass the server holds the monitor passing already, or `asked`.
    fn result(&self, result: MonitorResult, asked: bool) {
        let mut pending = self.lock();
        let steady = result.passing
            && pending
                .held
                .get(&result.monitor_id)
                .is_some_and(|held| held.passing);
        let news = asked || !steady;
        if news {
            pending.changed.insert(result.monitor_id.clone());
        }
        pending.latest.insert(result.monitor_id.clone(), result);
        drop(pending);
        if news {
            self.ready.notify_one();
        }
    }

    fn action(&self, result: MonitorActionResult) {
        self.lock().actions.push(result);
        self.ready.notify_one();
    }

    /// A new connection: what the server holds is unknown until it says,
    /// and the first refresh comes `phase` into the interval.
    fn connected(&self, now: Instant, phase: Duration) {
        let mut pending = self.lock();
        pending.held.clear();
        pending.quiet_until = Some(now + GREETING_WAIT);
        pending.refresh_at = Some(now + phase);
    }

    /// The server said what it holds: the latest results that differ, or
    /// that it lacks, go at once.
    fn known(&self, known: Vec<KnownMonitorResult>) {
        let mut pending = self.lock();
        pending.held = known
            .into_iter()
            .map(|known| {
                let held = Held {
                    checked_at_unix_ms: known.checked_at_unix_ms,
                    passing: known.passing,
                    failures: known.failures,
                };
                (known.monitor_id, held)
            })
            .collect();
        let news = pending
            .latest
            .iter()
            .filter(|(id, result)| {
                pending
                    .held
                    .get(*id)
                    .is_none_or(|held| held.differs(result))
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        pending.changed.extend(news);
        pending.quiet_until = None;
        drop(pending);
        self.ready.notify_one();
    }

    /// Forgets what a monitor found, as when its definition changed: its
    /// next result is the first.
    fn forget(&self, id: &str) {
        let mut pending = self.lock();
        pending.latest.remove(id);
        pending.held.remove(id);
        pending.changed.remove(id);
    }

    /// Forgets the results of monitors the device no longer runs.
    fn retain(&self, ids: &BTreeSet<String>) {
        let mut pending = self.lock();
        pending.latest.retain(|id, _| ids.contains(id));
        pending.held.retain(|id, _| ids.contains(id));
        pending.changed.retain(|id| ids.contains(id));
    }
}

enum Command {
    Set {
        definitions: Vec<MonitorDefinition>,
        known: Option<Vec<KnownMonitorResult>>,
    },
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
            spawn: spawn_check,
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

    /// Runs `monitors` from now on, in place of the ones before. `known` is
    /// what the server holds of their results, when a connection begins.
    pub(crate) fn set(
        &self,
        monitors: Vec<MonitorDefinition>,
        known: Option<Vec<KnownMonitorResult>>,
    ) {
        let _ = self.commands.send(Command::Set {
            definitions: monitors,
            known,
        });
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

    /// A new connection began. Results wait until the server says what it
    /// holds (or [`GREETING_WAIT`] passes), and refreshes start at a random
    /// point in their interval, so a fleet that reconnects together doesn't
    /// refresh together.
    pub(crate) fn connected(&self) {
        let phase = Random::new().up_to(refresh_interval());
        self.outbox.connected(Instant::now(), phase);
        self.outbox.ready.notify_one();
    }

    /// Completes with the messages to send the server. Cancel safe.
    pub(crate) async fn next(&self) -> Vec<AgentStatusMessage> {
        loop {
            let now = Instant::now();
            let wait = match self.outbox.lock().poll(now) {
                Ok(messages) => return messages,
                Err(at) => at.map_or(refresh_interval(), |at| at.saturating_duration_since(now)),
            };
            tokio::select! {
                () = self.outbox.ready.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Messages [`Self::next`] gave that couldn't be sent go again, after
    /// [`RETRY_WAIT`]. The server may not hold their results.
    pub(crate) fn unsent(&self, messages: Vec<AgentStatusMessage>) {
        let mut pending = self.outbox.lock();
        for message in messages {
            match message {
                AgentStatusMessage::MonitorResults { results } => {
                    for result in results {
                        pending.held.remove(&result.monitor_id);
                        if pending.latest.contains_key(&result.monitor_id) {
                            pending.changed.insert(result.monitor_id);
                        }
                    }
                }
                AgentStatusMessage::MonitorActionDone { result } => pending.actions.push(result),
                _ => {}
            }
        }
        pending.quiet_until = Some(Instant::now() + RETRY_WAIT);
    }
}

/// The marked results and every action, as messages; the server holds the
/// results from then on, as far as the Agent knows.
fn take(pending: &mut Pending) -> Vec<AgentStatusMessage> {
    let mut messages = Vec::new();
    let mut results = Vec::new();
    let mut text = 0;
    for id in std::mem::take(&mut pending.changed) {
        let Some(result) = pending.latest.get(&id) else {
            continue;
        };
        pending.held.insert(id, Held::of(result));
        let size = result.monitor_id.len() + result.detail.len() + result.output.len();
        if !results.is_empty()
            && (results.len() >= MAX_MONITORS || text + size > MESSAGE_TEXT_BYTES)
        {
            messages.push(AgentStatusMessage::MonitorResults {
                results: std::mem::take(&mut results),
            });
            text = 0;
        }
        text += size;
        results.push(result.clone());
    }
    if !results.is_empty() {
        messages.push(AgentStatusMessage::MonitorResults { results });
    }
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
    /// Whether Check now asked for its next result, which then goes at once.
    asked: bool,
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

/// A check to run, and where its result goes when it finishes.
struct Job {
    run: u64,
    check: MonitorCheck,
    context: Context,
    memory: Memory,
    commands: mpsc::Sender<Command>,
}

impl Job {
    /// Runs the check with `check` and sends [`Command::Done`] with what it
    /// found. A check that panics fails, saying it broke, and keeps the
    /// memory from before it: it may have stopped halfway through changing
    /// it, and an event log check must look again from its last finished
    /// check, not from this one. Either way the monitor is free to run
    /// again, rather than marked running until the Agent restarts.
    fn finish(self, check: impl FnOnce(&MonitorCheck, &Context, &mut Memory) -> Outcome) {
        let Job {
            run,
            check: monitor,
            context,
            memory: before,
            commands,
        } = self;
        let mut memory = before.clone();
        // Unwind safe: the check only reads `monitor` and `context`, and
        // the memory it may have left half changed is dropped on a panic.
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check(&monitor, &context, &mut memory)
        }));
        let (outcome, memory) = match checked {
            Ok(outcome) => {
                memory.checked_at_ms = Some(context.now_ms);
                (outcome, memory)
            }
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
                tracing::error!(panic = message, "a monitor's check panicked");
                let detail = match message {
                    Some(message) => format!("The check itself broke: {message}"),
                    None => "The check itself broke.".to_owned(),
                };
                (Outcome::failing(detail), before)
            }
        };
        let _ = commands.send(Command::Done {
            run,
            checked_at_ms: context.now_ms,
            outcome,
            memory,
        });
    }
}

/// Runs a check on its own thread, which sends [`Command::Done`] when it
/// finishes, even if the check panics.
fn spawn_check(job: Job) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("meshrmm-monitor-check".into())
        .spawn(move || job.finish(checks::run))
        .map(drop)
}

/// The longest the scheduler waits when nothing comes sooner.
const IDLE_WAIT: Duration = Duration::from_secs(60);

fn is_script(entry: &Entry) -> bool {
    matches!(entry.definition.check, MonitorCheck::ScriptCheck { .. })
}

struct Scheduler {
    mode: ExecutionMode,
    entries: BTreeMap<String, Entry>,
    runs: HashMap<u64, Run>,
    next_run: u64,
    random: Random,
    outbox: Arc<Outbox>,
    commands: mpsc::Sender<Command>,
    /// Starts a check, which sends [`Command::Done`] when it finishes.
    /// Tests start none, and finish them themselves.
    spawn: fn(Job) -> std::io::Result<()>,
}

impl Scheduler {
    fn run(mut self, receiver: &mpsc::Receiver<Command>) {
        loop {
            let command = match receiver.recv_timeout(self.wait(Instant::now())) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            self.step(command, Instant::now());
        }
    }

    /// One turn of the loop: the command that came, if any, then the
    /// overruns and the checks due at `now`.
    fn step(&mut self, command: Option<Command>, now: Instant) {
        if let Some(command) = command {
            self.handle(command);
        }
        self.expire(now);
        self.start_due(now);
    }

    /// How many checks run, and how many of them are scripts. An overrun no
    /// longer counts: it was reported failing, and its thread is left to
    /// finish.
    fn running(&self) -> (usize, usize) {
        self.runs
            .values()
            .filter(|run| !run.abandoned)
            .fold((0, 0), |(checks, scripts), run| {
                (checks + 1, scripts + usize::from(run.script))
            })
    }

    /// Whether a limit keeps `entry` from starting while `running` checks,
    /// and scripts among them, run.
    fn held_back(entry: &Entry, (checks, scripts): (usize, usize)) -> bool {
        checks >= MAX_RUNNING_CHECKS || (is_script(entry) && scripts >= MAX_RUNNING_SCRIPTS)
    }

    /// How long the loop waits for a command before it looks again: until
    /// the next monitor is due or the next check overruns. A monitor due
    /// but held back by a limit doesn't count, or the loop would spin until
    /// a check finished: a check finishing comes as a command, and an
    /// overrun frees its place at its deadline, so either wakes the loop to
    /// start it.
    fn wait(&self, now: Instant) -> Duration {
        let running = self.running();
        let next = self
            .entries
            .values()
            .filter(|entry| entry.running.is_none())
            .filter(|entry| entry.due > now || !Self::held_back(entry, running))
            .map(|entry| entry.due)
            .chain(
                self.runs
                    .values()
                    .filter(|run| !run.abandoned)
                    .map(|run| run.deadline),
            )
            .min();
        next.map_or(IDLE_WAIT, |next| {
            next.saturating_duration_since(now).min(IDLE_WAIT)
        })
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Set { definitions, known } => self.set(definitions, known),
            Command::CheckNow(ids) => {
                let now = Instant::now();
                for (id, entry) in &mut self.entries {
                    if ids.is_empty() || ids.contains(id) {
                        entry.asked = true;
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

    fn set(&mut self, definitions: Vec<MonitorDefinition>, known: Option<Vec<KnownMonitorResult>>) {
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
            // What it found before says nothing about the new definition.
            self.outbox.forget(&id);
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
                    asked: false,
                },
            );
        }
        let ids = self.entries.keys().cloned().collect();
        self.outbox.retain(&ids);
        if let Some(known) = known {
            self.outbox.known(known);
        }
        tracing::info!(
            monitors = self.entries.len(),
            "the server set the device's monitors"
        );
    }

    /// Checks that overran their timeout count as failing.
    fn expire(&mut self, now: Instant) {
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

    /// Starts the checks due at `now` that the limits let run, the one due
    /// longest first. A script held back doesn't hold back other checks.
    fn start_due(&mut self, now: Instant) {
        let mut due = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.running.is_none() && entry.due <= now)
            .map(|(id, entry)| (entry.due, id.clone()))
            .collect::<Vec<_>>();
        due.sort();
        for (_, id) in due {
            let running = self.running();
            if running.0 >= MAX_RUNNING_CHECKS {
                break;
            }
            if Self::held_back(&self.entries[&id], running) {
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
        let script = is_script(entry);
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
        let job = Job {
            run,
            check: entry.definition.check.clone(),
            context: Context {
                mode: self.mode,
                interval,
                timeout: entry.timeout(),
                now_ms: checked_at_ms,
            },
            memory: entry.memory.clone(),
            commands: self.commands.clone(),
        };
        if let Err(error) = (self.spawn)(job) {
            // No run is left to finish, so the monitor is free to start
            // again when it is next due.
            self.runs.remove(&run);
            entry.running = None;
            let memory = entry.memory.clone();
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
        entry.failures = if outcome.passing {
            0
        } else {
            (entry.failures + 1).min(MAX_MONITOR_FAILURES)
        };
        entry.memory = memory;
        if entry.running.is_none() && entry.due <= Instant::now() {
            entry.due = Instant::now() + entry.interval();
        }
        let asked = std::mem::take(&mut entry.asked);
        let result = MonitorResult {
            monitor_id: id.to_owned(),
            checked_at_unix_ms: checked_at_ms.max(0) as u64,
            passing: outcome.passing,
            failures: entry.failures,
            value: outcome.value,
            detail: truncate_text(&outcome.detail, MAX_MONITOR_DETAIL_CHARS).to_owned(),
            output: truncate_text(&outcome.output, MAX_MONITOR_OUTPUT_BYTES).to_owned(),
        };
        self.outbox.result(result, asked);
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
                spawn: spawn_check,
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

    fn results_in(messages: Vec<AgentStatusMessage>) -> Vec<MonitorResult> {
        messages
            .into_iter()
            .flat_map(|message| match message {
                AgentStatusMessage::MonitorResults { results } => results,
                _ => vec![],
            })
            .collect()
    }

    /// The results marked to go, taken as the coordinator would.
    fn results(outbox: &Outbox) -> Vec<MonitorResult> {
        results_in(take(&mut outbox.lock()))
    }

    /// The IDs of the results that go at `now`, sorted; none when nothing
    /// goes yet.
    fn sent_at(outbox: &Outbox, now: Instant) -> Vec<String> {
        let Ok(messages) = outbox.lock().poll(now) else {
            return Vec::new();
        };
        let mut ids = results_in(messages)
            .into_iter()
            .map(|result| result.monitor_id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn known(
        id: &str,
        checked_at_unix_ms: u64,
        passing: bool,
        failures: u32,
    ) -> KnownMonitorResult {
        KnownMonitorResult {
            monitor_id: id.into(),
            checked_at_unix_ms,
            passing,
            failures,
        }
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
        scheduler.set(vec![port_monitor("one", 1)], None);
        let first = scheduler.entries["one"].due;
        assert!(first <= Instant::now() + MAX_JITTER);
        let now = Instant::now();

        // The first result goes at once, even passing; the next pass waits.
        scheduler.record("one", 1, Outcome::passing("open"), Memory::default());
        assert_eq!(sent_at(&outbox, now), ["one"], "a first result");
        scheduler.record("one", 2, Outcome::passing("open"), Memory::default());
        assert!(sent_at(&outbox, now).is_empty(), "a steady pass waits");

        scheduler.record("one", 3, Outcome::failing("closed"), Memory::default());
        scheduler.record("one", 4, Outcome::failing("closed"), Memory::default());
        let sent = results(&outbox);
        assert_eq!(sent.len(), 1, "only the latest result is kept");
        assert_eq!((sent[0].failures, sent[0].passing), (2, false));
        // Every failure goes, with its count.
        scheduler.record("one", 5, Outcome::failing("closed"), Memory::default());
        assert_eq!(results(&outbox)[0].failures, 3);

        scheduler.record(
            "one",
            6,
            Outcome::passing("open").value(1.5),
            Memory::default(),
        );
        let sent = results(&outbox);
        assert_eq!(sent.len(), 1, "recovering goes at once");
        assert_eq!((sent[0].failures, sent[0].value), (0, Some(1.5)));

        scheduler.record("one", 7, Outcome::passing("open"), Memory::default());
        assert!(sent_at(&outbox, now).is_empty(), "a steady pass waits");

        // Check now's result goes at once, passing or not.
        scheduler.handle(Command::CheckNow(vec![]));
        scheduler.record("one", 8, Outcome::passing("open"), Memory::default());
        assert_eq!(sent_at(&outbox, now), ["one"], "an asked-for result");
    }

    #[test]
    fn steady_passes_wait_for_the_refresh() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        let ids = (0..50).map(|n| format!("m{n}")).collect::<Vec<_>>();
        let definitions = ids.iter().map(|id| port_monitor(id, 1)).collect::<Vec<_>>();
        let start = Instant::now();
        let phase = Duration::from_secs(60);
        outbox.connected(start, phase);
        // The server holds every one passing.
        let held = ids.iter().map(|id| known(id, 1, true, 0)).collect();
        scheduler.set(definitions, Some(held));

        for round in 2..10 {
            for id in &ids {
                scheduler.record(id, round, Outcome::passing("open"), Memory::default());
            }
        }
        assert!(
            matches!(outbox.lock().poll(start + phase - Duration::from_secs(1)), Err(Some(at)) if at == start + phase),
            "nothing goes before the refresh"
        );

        // The refresh carries every latest result, together.
        let messages = outbox.lock().poll(start + phase).unwrap();
        assert_eq!(messages.len(), 1, "one message");
        let refreshed = results_in(messages);
        assert_eq!(refreshed.len(), ids.len());
        assert!(
            refreshed
                .iter()
                .all(|result| result.checked_at_unix_ms == 9)
        );

        // The next refresh carries only what the server doesn't hold yet.
        let next = start + phase + refresh_interval();
        assert!(matches!(outbox.lock().poll(start + phase), Err(Some(at)) if at == next));
        scheduler.record("m7", 10, Outcome::passing("open"), Memory::default());
        assert_eq!(sent_at(&outbox, next), ["m7"]);
        assert!(sent_at(&outbox, next + refresh_interval()).is_empty());
    }

    #[test]
    fn a_reconnect_sends_only_what_the_server_lacks_or_holds_differently() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        let definitions = ["a", "b", "c", "d"]
            .into_iter()
            .map(|id| port_monitor(id, 1))
            .collect::<Vec<_>>();
        let start = Instant::now();
        outbox.connected(start, Duration::from_secs(600));
        scheduler.set(definitions.clone(), Some(Vec::new()));
        for id in ["a", "b", "d"] {
            scheduler.record(id, 1, Outcome::passing("open"), Memory::default());
        }
        scheduler.record("c", 1, Outcome::failing("closed"), Memory::default());
        assert_eq!(sent_at(&outbox, start), ["a", "b", "c", "d"]);

        // Another connection whose server holds the same: nothing goes.
        let again = start + Duration::from_secs(1);
        outbox.connected(again, Duration::from_secs(600));
        scheduler.set(
            definitions.clone(),
            Some(vec![
                known("a", 1, true, 0),
                known("b", 1, true, 0),
                known("c", 1, false, 1),
                known("d", 1, true, 0),
            ]),
        );
        assert!(sent_at(&outbox, again).is_empty());
        scheduler.record("a", 2, Outcome::passing("open"), Memory::default());
        assert!(sent_at(&outbox, again).is_empty(), "a steady pass waits");

        // One whose server lacks one and holds another differently: those
        // go, and only once the server has said.
        let third = again + Duration::from_secs(1);
        outbox.connected(third, Duration::from_secs(600));
        assert!(
            sent_at(&outbox, third).is_empty(),
            "it waits for the server"
        );
        scheduler.set(
            definitions,
            Some(vec![
                known("a", 1, true, 0),
                known("b", 1, false, 4),
                known("c", 1, false, 1),
            ]),
        );
        assert_eq!(sent_at(&outbox, third), ["b", "d"]);

        // Without word from the server, news goes once the wait is over.
        let fourth = third + Duration::from_secs(1);
        outbox.connected(fourth, Duration::from_secs(600));
        scheduler.record("c", 2, Outcome::failing("closed"), Memory::default());
        assert!(sent_at(&outbox, fourth).is_empty());
        assert_eq!(sent_at(&outbox, fourth + GREETING_WAIT), ["c"]);
    }

    #[test]
    fn a_changed_definition_starts_over_and_its_first_result_goes() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        let now = Instant::now();
        scheduler.set(vec![port_monitor("one", 1)], None);
        scheduler.record("one", 1, Outcome::passing("open"), Memory::default());
        assert_eq!(sent_at(&outbox, now), ["one"]);
        scheduler.set(vec![port_monitor("one", 2)], None);
        scheduler.record("one", 2, Outcome::passing("open"), Memory::default());
        assert_eq!(sent_at(&outbox, now), ["one"]);
    }

    #[test]
    fn unchanged_monitors_keep_their_state_and_removed_ones_go() {
        let outbox = outbox();
        let (mut scheduler, _receiver) = scheduler(&outbox);
        scheduler.set(vec![port_monitor("one", 1), port_monitor("two", 2)], None);
        scheduler.record("one", 1, Outcome::failing("closed"), Memory::default());
        scheduler.record("two", 1, Outcome::failing("closed"), Memory::default());
        scheduler.set(vec![port_monitor("one", 1), port_monitor("two", 3)], None);
        assert_eq!(scheduler.entries["one"].failures, 1);
        assert_eq!(
            scheduler.entries["two"].failures, 0,
            "a changed monitor starts over"
        );
        scheduler.set(vec![port_monitor("two", 3)], None);
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
        scheduler.set(vec![monitor], None);
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
        scheduler.set(vec![port_monitor("open", port)], None);
        scheduler.handle(Command::CheckNow(vec![]));
        scheduler.start_due(Instant::now());
        assert_eq!(scheduler.runs.len(), 1);
        let done = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        scheduler.handle(done);
        let sent = results(&outbox);
        assert!(sent[0].passing, "{}", sent[0].detail);
        assert!(sent[0].value.is_some());

        // A run past its deadline is reported failing, and its late result
        // is dropped.
        scheduler.handle(Command::CheckNow(vec![]));
        scheduler.start_due(Instant::now());
        let run = *scheduler.runs.keys().next().unwrap();
        scheduler.runs.get_mut(&run).unwrap().deadline = Instant::now();
        scheduler.expire(Instant::now());
        let sent = results(&outbox);
        assert!(!sent[0].passing);
        assert!(sent[0].detail.contains("didn't finish"));
        assert!(
            scheduler.entries["open"].running.is_some(),
            "it doesn't run twice"
        );
        scheduler.start_due(Instant::now());
        assert_eq!(scheduler.runs.len(), 1);
        let late = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        scheduler.handle(late);
        assert!(results(&outbox).is_empty());
        assert!(scheduler.entries["open"].running.is_none());
    }

    /// Starts nothing: the test finishes each check itself, with [`done`].
    fn hold(_job: Job) -> std::io::Result<()> {
        Ok(())
    }

    fn done(run: u64) -> Command {
        Command::Done {
            run,
            checked_at_ms: 1,
            outcome: Outcome::passing("done"),
            memory: Memory::default(),
        }
    }

    fn script_monitor(id: &str) -> MonitorDefinition {
        MonitorDefinition {
            id: id.into(),
            interval_seconds: 60,
            timeout_seconds: 30,
            check: MonitorCheck::ScriptCheck {
                script_id: "script-1".into(),
                run_as: meshrmm_protocol::RunAs::System,
                language: Some(meshrmm_protocol::ScriptLanguage::Shell),
                body: "exit 0".into(),
            },
        }
    }

    /// A scheduler that starts no real checks, running `definitions`, each
    /// due at `now`.
    fn due_at(
        outbox: &Arc<Outbox>,
        definitions: Vec<MonitorDefinition>,
        now: Instant,
    ) -> (Scheduler, mpsc::Receiver<Command>) {
        let (mut scheduler, receiver) = scheduler(outbox);
        scheduler.spawn = hold;
        scheduler.set(definitions, None);
        for entry in scheduler.entries.values_mut() {
            entry.due = now;
        }
        (scheduler, receiver)
    }

    /// The monitors running, by ID.
    fn running(scheduler: &Scheduler) -> Vec<&str> {
        scheduler
            .entries
            .iter()
            .filter(|(_, entry)| entry.running.is_some())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    fn run_of(scheduler: &Scheduler, id: &str) -> u64 {
        scheduler.entries[id].running.unwrap()
    }

    #[test]
    fn a_script_held_back_waits_for_a_script_slot() {
        let outbox = outbox();
        let now = Instant::now();
        let scripts = ["a", "b", "c"].map(script_monitor).into();
        let (mut scheduler, _receiver) = due_at(&outbox, scripts, now);
        scheduler.step(None, now);
        assert_eq!(running(&scheduler), ["a", "b"]);

        // c is due but held back: the wait runs to the first overrun.
        let overrun = Duration::from_secs(30) + OVERRUN_GRACE;
        assert_eq!(scheduler.wait(now), overrun);
        // A port monitor due sooner than that wakes the loop first.
        let mut port = port_monitor("d", 1);
        port.interval_seconds = 600;
        scheduler.set(
            vec![
                script_monitor("a"),
                script_monitor("b"),
                script_monitor("c"),
                port,
            ],
            None,
        );
        scheduler.entries.get_mut("d").unwrap().due = now + Duration::from_secs(20);
        assert_eq!(scheduler.wait(now), Duration::from_secs(20));

        // A script finishing frees a slot, and c starts on that step.
        let later = now + Duration::from_secs(3);
        scheduler.step(Some(done(run_of(&scheduler, "a"))), later);
        assert_eq!(running(&scheduler), ["b", "c"]);
        assert_eq!(
            scheduler.wait(later),
            Duration::from_secs(17),
            "a's next check is a minute off; d's sooner"
        );
    }

    #[test]
    fn a_ninth_check_waits_for_one_of_eight_to_finish() {
        let outbox = outbox();
        let now = Instant::now();
        let mut definitions = (0..9)
            .map(|n| port_monitor(&format!("m{n}"), 1))
            .collect::<Vec<_>>();
        // Due after the others, though first by name.
        definitions.push(port_monitor("a-late", 1));
        let (mut scheduler, _receiver) = due_at(&outbox, definitions, now);
        scheduler.entries.get_mut("a-late").unwrap().due = now + Duration::from_secs(1);
        scheduler.step(None, now);
        assert_eq!(scheduler.running(), (MAX_RUNNING_CHECKS, 0));
        assert!(scheduler.entries["m8"].running.is_none());

        // m8 is due but held back; a-late's time is the next thing to come.
        assert_eq!(scheduler.wait(now), Duration::from_secs(1));
        let due = now + Duration::from_secs(1);
        scheduler.step(None, due);
        assert!(scheduler.entries["a-late"].running.is_none());
        // Now both are held back: the wait runs to the first overrun.
        let overrun = Duration::from_secs(5) + OVERRUN_GRACE;
        assert_eq!(scheduler.wait(due), overrun - Duration::from_secs(1));

        // When one finishes, the one due longest starts.
        let later = now + Duration::from_secs(2);
        scheduler.step(Some(done(run_of(&scheduler, "m3"))), later);
        assert!(scheduler.entries["m8"].running.is_some());
        assert!(scheduler.entries["a-late"].running.is_none());
        assert_eq!(scheduler.running(), (MAX_RUNNING_CHECKS, 0));
    }

    #[test]
    fn a_monitor_not_yet_due_wakes_the_loop_on_time() {
        let outbox = outbox();
        let now = Instant::now();
        let (mut scheduler, _receiver) = due_at(&outbox, vec![port_monitor("one", 1)], now);
        let due = now + Duration::from_secs(7);
        scheduler.entries.get_mut("one").unwrap().due = due;
        assert_eq!(scheduler.wait(now), Duration::from_secs(7));
        scheduler.step(None, due - Duration::from_millis(1));
        assert!(running(&scheduler).is_empty());
        scheduler.step(None, due);
        assert_eq!(running(&scheduler), ["one"]);
        assert_eq!(
            scheduler.wait(due),
            Duration::from_secs(5) + OVERRUN_GRACE,
            "then it waits for the check"
        );

        // An overrun is due at once, and only once.
        let late = due + Duration::from_secs(3600);
        assert_eq!(scheduler.wait(late), Duration::ZERO);
        scheduler.step(None, late);
        assert_eq!(scheduler.wait(late), IDLE_WAIT);
        let (idle, _receiver) = due_at(&outbox, Vec::new(), now);
        assert_eq!(idle.wait(now), IDLE_WAIT);
    }

    /// The loop as [`Scheduler::run`] turns it, on a clock of the test's
    /// own: three scripts due together, the first finishing after 200 of
    /// its 300 seconds.
    #[test]
    fn the_loop_sleeps_while_a_script_waits_for_a_slot() {
        let outbox = outbox();
        let start = Instant::now();
        let scripts = ["a", "b", "c"]
            .map(|id| MonitorDefinition {
                timeout_seconds: 300,
                ..script_monitor(id)
            })
            .into();
        let (mut scheduler, _receiver) = due_at(&outbox, scripts, start);
        scheduler.step(None, start);
        let first = run_of(&scheduler, "a");
        let finishes = start + Duration::from_secs(200);
        let mut now = start;
        let mut turns = 0;
        while scheduler.entries["c"].running.is_none() {
            turns += 1;
            assert!(
                turns <= 10,
                "the loop turned {turns} times in {:?}",
                now - start
            );
            // It wakes when its wait is over or the check finishes, which
            // comes first.
            let wake = now + scheduler.wait(now);
            if wake >= finishes && scheduler.runs.contains_key(&first) {
                now = finishes;
                scheduler.step(Some(done(first)), now);
            } else {
                now = wake;
                scheduler.step(None, now);
            }
        }
        assert_eq!(now, finishes, "c starts as a finishes");
    }

    thread_local! {
        /// The checks [`keep`] started, for the test to finish.
        static STARTED: std::cell::RefCell<Vec<Job>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Starts nothing, keeping the check for the test to finish with
    /// [`Job::finish`], as its thread would.
    fn keep(job: Job) -> std::io::Result<()> {
        STARTED.with_borrow_mut(|started| started.push(job));
        Ok(())
    }

    /// The one check started since the last call.
    fn started() -> Job {
        let mut started = STARTED.with_borrow_mut(std::mem::take);
        assert_eq!(started.len(), 1);
        started.remove(0)
    }

    #[test]
    fn a_check_that_panics_fails_once_and_runs_again_with_its_memory() {
        let outbox = outbox();
        let now = Instant::now();
        let (mut scheduler, receiver) = due_at(&outbox, vec![port_monitor("one", 1)], now);
        scheduler.spawn = keep;
        let remembered = Memory {
            checked_at_ms: Some(5),
            over_since_ms: Some(3),
        };
        scheduler.entries.get_mut("one").unwrap().memory = remembered.clone();
        scheduler.step(None, now);
        started().finish(|_, _, memory| {
            // Halfway through changing what it remembers.
            memory.checked_at_ms = Some(99);
            memory.over_since_ms = None;
            panic!("byte index 1 is not a char boundary");
        });
        scheduler.step(Some(receiver.try_recv().unwrap()), now);
        let sent = results(&outbox);
        assert_eq!(sent.len(), 1, "it is reported once");
        assert!(!sent[0].passing);
        assert_eq!(sent[0].failures, 1);
        assert_eq!(
            sent[0].detail,
            "The check itself broke: byte index 1 is not a char boundary"
        );
        let entry = &scheduler.entries["one"];
        assert!(entry.running.is_none() && scheduler.runs.is_empty());
        assert_eq!(entry.memory, remembered, "nothing it changed is kept");

        // Nothing more comes of it, not even an overrun, until it is next
        // due; then it runs again, with what it remembered before.
        let next = entry.due;
        scheduler.step(None, next - Duration::from_millis(1));
        assert!(running(&scheduler).is_empty());
        assert!(results(&outbox).is_empty());
        scheduler.step(None, next);
        assert_eq!(running(&scheduler), ["one"]);
        let job = started();
        assert_eq!(job.memory, remembered);
        // A panic that isn't a message says only that it broke.
        job.finish(|_, _, _| std::panic::panic_any(7));
        scheduler.step(Some(receiver.try_recv().unwrap()), next);
        let sent = results(&outbox);
        assert_eq!(
            (sent[0].detail.as_str(), sent[0].failures),
            ("The check itself broke.", 2)
        );
        assert_eq!(scheduler.entries["one"].memory, remembered);

        // A check that finishes keeps what it remembers, and its time.
        let again = scheduler.entries["one"].due;
        scheduler.step(None, again);
        let job = started();
        let checked_at = job.context.now_ms;
        job.finish(|_, _, memory| {
            memory.over_since_ms = Some(42);
            Outcome::passing("open")
        });
        scheduler.step(Some(receiver.try_recv().unwrap()), again);
        assert!(results(&outbox)[0].passing);
        assert_eq!(
            scheduler.entries["one"].memory,
            Memory {
                checked_at_ms: Some(checked_at),
                over_since_ms: Some(42),
            }
        );
    }

    fn refuse(_job: Job) -> std::io::Result<()> {
        Err(std::io::Error::other("no threads left"))
    }

    #[test]
    fn a_check_whose_thread_cant_start_fails_and_is_due_again() {
        let outbox = outbox();
        let now = Instant::now();
        let (mut scheduler, _receiver) = due_at(&outbox, vec![port_monitor("one", 1)], now);
        scheduler.spawn = refuse;
        scheduler.step(None, now);
        let sent = results(&outbox);
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].passing);
        assert_eq!(sent[0].detail, "Could not start the check: no threads left");
        assert!(running(&scheduler).is_empty() && scheduler.runs.is_empty());

        // The loop wakes for its next time, and tries again then.
        let next = scheduler.entries["one"].due;
        assert!(next > now);
        assert_eq!(scheduler.wait(now), (next - now).min(IDLE_WAIT));
        scheduler.step(None, next - Duration::from_millis(1));
        assert!(results(&outbox).is_empty(), "not before it is due");
        scheduler.step(None, next);
        let sent = results(&outbox);
        assert_eq!((sent.len(), sent[0].failures), (1, 2), "it tried again");

        // Once a thread starts, the check runs.
        scheduler.spawn = keep;
        let again = scheduler.entries["one"].due;
        scheduler.step(None, again);
        assert_eq!(running(&scheduler), ["one"]);
        started();
    }

    #[tokio::test]
    async fn the_coordinator_hears_news_once_the_server_says_and_refused_results_again() {
        let monitors = Monitors::new(ExecutionMode::Console);
        monitors.connected();
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
            false,
        );
        // The server holds nothing, so the failure is news.
        monitors.outbox.known(Vec::new());
        let messages = tokio::time::timeout(Duration::from_secs(5), monitors.next())
            .await
            .unwrap();
        assert_eq!(results_in(messages.clone()).len(), 1);

        // Messages the socket refused go again, after a wait.
        monitors.unsent(messages);
        let now = Instant::now();
        assert!(sent_at(&monitors.outbox, now).is_empty());
        assert_eq!(sent_at(&monitors.outbox, now + RETRY_WAIT), ["one"]);
    }
}
