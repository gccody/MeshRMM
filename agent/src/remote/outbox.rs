//! The final results the Agent owes the server, kept until the server
//! acknowledges them: how a toolbox script or file delivery, an update
//! install or an application run ended.
//!
//! Each result is written to a file of its own in the Agent's private state
//! directory, beside its configuration (script output is in these files, so
//! they need the same protection), before it is first sent, and removed only
//! once the server has taken it. The Agent reads them again when it starts,
//! so a result from before a service restart, a reboot or a self-update is
//! still delivered, and a self-update need not wait for one.
//!
//! One worker thread posts them, oldest first, to their routes under
//! `/v1/agents/{device}/` with the Agent's credential:
//!
//! - a success (2xx) is the acknowledgement: the result is removed;
//! - any other 4xx but 401, 408 and 429 means the server will never take it
//!   (it knows no such run, or the report is malformed): it is removed, with
//!   a warning;
//! - a 5xx, a 401, 408 or 429, or a network error is retried, a little later each
//!   time ([`backoff`]), and at once when the control connection comes back,
//!   since the server is back then too. The result that failed goes to the
//!   back of the queue, so one the server keeps failing on doesn't hold up
//!   the others.
//!
//! The server answers a result it already has with a success, so a result
//! sent again after an answer that never arrived changes nothing.
//!
//! The store is bounded by [`MAX_RESULTS`], [`MAX_BYTES`] and [`MAX_AGE_MS`]:
//! when it is full the oldest result goes, with a warning. A file that cannot
//! be read is removed, with a warning, and never holds up the others.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Context;
use meshrmm_protocol::{
    ApplicationRunResult, FileDeliveryReport, ScriptRunReport, UpdateInstallResult,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How long a result is offered to the server. The server keeps toolbox runs
/// for 30 days, its shortest history, and installs and application runs for a
/// year, and takes a result it had already counted as lost, so a result is
/// worth delivering long after its report window. A week covers a server that
/// is down over a long weekend, or a laptop away from the network for a trip;
/// past that, nobody is waiting for it.
pub(crate) const MAX_AGE_MS: i64 = 7 * DAY_MS;
/// How many results are kept. A device runs at most 8 scripts and 4 file
/// deliveries, one install and one application run at a time; this holds
/// days of a busy device's results while the server is away.
pub(crate) const MAX_RESULTS: usize = 1000;
/// How many bytes of results are kept. The largest result is a script's,
/// with both output streams at their 512 KiB limit, a few MiB once escaped as
/// JSON (the server takes up to 8 MiB); most are a few KiB. This holds at
/// least eight of the largest.
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The wait after the first failure, doubling with each to [`LAST_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(5);
const LAST_RETRY: Duration = Duration::from_secs(10 * 60);
/// How long the worker waits with nothing due, so old results still age out.
const IDLE_WAKE: Duration = Duration::from_secs(60 * 60);
const EXTENSION: &str = ".json";
/// What [`crate::installer::replace_file`] writes before it renames.
const UNFINISHED: &str = ".new";

/// A final result, with the server's ID of what it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Final {
    ScriptRun {
        run_id: String,
        report: ScriptRunReport,
    },
    FileDelivery {
        delivery_id: String,
        report: FileDeliveryReport,
    },
    UpdateInstall {
        run_id: String,
        result: UpdateInstallResult,
    },
    ApplicationRun {
        run_id: String,
        result: ApplicationRunResult,
    },
}

impl Final {
    /// The collection its route is under, which also names its file.
    fn collection(&self) -> &'static str {
        match self {
            Self::ScriptRun { .. } => "script-runs",
            Self::FileDelivery { .. } => "file-deliveries",
            Self::UpdateInstall { .. } => "update-runs",
            Self::ApplicationRun { .. } => "application-runs",
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::ScriptRun { run_id, .. }
            | Self::UpdateInstall { run_id, .. }
            | Self::ApplicationRun { run_id, .. } => run_id,
            Self::FileDelivery { delivery_id, .. } => delivery_id,
        }
    }

    /// Where it is posted, under `/v1/agents/{device}/`.
    fn path(&self) -> [&str; 3] {
        [self.collection(), self.id(), "result"]
    }

    fn body(&self) -> serde_json::Result<Vec<u8>> {
        match self {
            Self::ScriptRun { report, .. } => serde_json::to_vec(report),
            Self::FileDelivery { report, .. } => serde_json::to_vec(report),
            Self::UpdateInstall { result, .. } => serde_json::to_vec(result),
            Self::ApplicationRun { result, .. } => serde_json::to_vec(result),
        }
    }

    /// Its file's name: one per result, whatever characters its ID has.
    fn file_name(&self) -> String {
        let digest = Sha256::digest(self.id().as_bytes());
        let hex: String = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{}-{hex}{EXTENSION}", self.collection())
    }
}

/// A result's file.
#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    created_at_unix_ms: i64,
    result: Final,
}

/// What the server answered, or why there was no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sent {
    Status(u16),
    Unreachable(String),
}

/// Posts a result's body to its path under `/v1/agents/{device}/`.
pub(crate) trait Deliver {
    fn post(&self, path: &[&str], body: &[u8]) -> Sent;
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Taken,
    Refused,
    Retry,
}

fn verdict(sent: &Sent) -> Verdict {
    match sent {
        Sent::Status(200..=299) => Verdict::Taken,
        // A post that left with the credential the server has just replaced
        // is refused; the next goes with the new one.
        Sent::Status(401 | 408 | 429) => Verdict::Retry,
        Sent::Status(400..=499) => Verdict::Refused,
        Sent::Status(_) | Sent::Unreachable(_) => Verdict::Retry,
    }
}

/// How long to wait after `failures` retryable failures in a row, before
/// jitter.
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FIRST_RETRY.saturating_mul(1 << doublings).min(LAST_RETRY)
}

/// A result the queue holds.
#[derive(Debug)]
struct Entry {
    name: String,
    created_at: i64,
    bytes: u64,
    collection: &'static str,
    id: String,
    /// The result, when it could not be written to disk: it is kept only
    /// until the Agent stops.
    unstored: Option<Final>,
}

#[derive(Debug, Default)]
struct State {
    /// In the order they are tried.
    queue: VecDeque<Entry>,
    /// Retryable failures in a row.
    failures: u32,
    /// No attempt before this.
    next_attempt: i64,
    /// The worker should look again: a result was added, or the control
    /// connection came back.
    wake: bool,
    /// The control connection came back during the current attempt.
    reconnected: bool,
}

pub(crate) struct Outbox {
    /// Where the results are kept; none when the directory couldn't be made.
    directory: Option<PathBuf>,
    state: Mutex<State>,
    changed: Condvar,
    jitter: fn(Duration) -> Duration,
}

impl Outbox {
    /// The Agent's outbox: its results directory, or one in memory when that
    /// cannot be made.
    pub(crate) fn for_agent(mode: crate::remote::config::ExecutionMode) -> Self {
        let now = crate::remote::updates::now_unix_ms();
        match directory(mode) {
            Ok(directory) => Self::open(directory, now, meshrmm_signaling_client::jittered),
            Err(error) => {
                tracing::error!(
                    error = format!("{error:#}"),
                    "could not prepare the folder for results; results are kept only until the Agent stops"
                );
                Self::new(None, meshrmm_signaling_client::jittered)
            }
        }
    }

    fn new(directory: Option<PathBuf>, jitter: fn(Duration) -> Duration) -> Self {
        Self {
            directory,
            state: Mutex::default(),
            changed: Condvar::new(),
            jitter,
        }
    }

    /// The results kept in `directory` at `now`, which it must already be.
    fn open(directory: PathBuf, now: i64, jitter: fn(Duration) -> Duration) -> Self {
        let mut entries = Vec::new();
        match std::fs::read_dir(&directory) {
            Ok(listing) => {
                for item in listing.flatten() {
                    let path = item.path();
                    let Some(name) = item.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !item.file_type().is_ok_and(|kind| kind.is_file()) {
                        continue;
                    }
                    if name.ends_with(UNFINISHED) {
                        // A write the Agent stopped during; its result was never sent.
                        remove(&path);
                        continue;
                    }
                    if !name.ends_with(EXTENSION) {
                        continue;
                    }
                    match read(&path) {
                        Ok((stored, bytes)) => entries.push(Entry {
                            name,
                            created_at: stored.created_at_unix_ms,
                            bytes,
                            collection: stored.result.collection(),
                            id: stored.result.id().to_owned(),
                            unstored: None,
                        }),
                        Err(error) => {
                            tracing::warn!(
                                file = %path.display(),
                                error = format!("{error:#}"),
                                "removed a stored result the Agent cannot read"
                            );
                            remove(&path);
                        }
                    }
                }
            }
            Err(error) => tracing::warn!(
                directory = %directory.display(),
                %error,
                "could not read the stored results"
            ),
        }
        entries.sort_by(|a, b| (a.created_at, &a.name).cmp(&(b.created_at, &b.name)));
        if !entries.is_empty() {
            tracing::info!(
                results = entries.len(),
                "found results from before the Agent started; delivering them"
            );
        }
        let outbox = Self::new(Some(directory), jitter);
        {
            let mut state = outbox.lock();
            state.queue = entries.into();
            outbox.bound(&mut state, now);
        }
        outbox
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding it, so a poisoned one is whole.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keeps `result`, created at `now`, until the server takes it. It is on
    /// disk when this returns, unless the disk refused it.
    pub(crate) fn add(&self, result: Final, now: i64) {
        let name = result.file_name();
        let (collection, id) = (result.collection(), result.id().to_owned());
        let stored = Stored {
            created_at_unix_ms: now,
            result,
        };
        let bytes = match serde_json::to_vec(&stored) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(collection, id, %error, "could not encode a result; it is lost");
                return;
            }
        };
        let written = match &self.directory {
            Some(directory) => crate::installer::replace_file(&directory.join(&name), &bytes),
            None => Err(anyhow::anyhow!("there is no folder for results")),
        };
        let unstored = match written {
            Ok(()) => None,
            Err(error) => {
                if self.directory.is_some() {
                    tracing::warn!(
                        collection,
                        id,
                        error = format!("{error:#}"),
                        "could not store a result; it is kept only until the Agent stops"
                    );
                }
                Some(stored.result)
            }
        };
        let mut state = self.lock();
        // The same result again replaces the one kept.
        state.queue.retain(|entry| entry.name != name);
        state.queue.push_back(Entry {
            name,
            created_at: now,
            bytes: bytes.len() as u64,
            collection,
            id,
            unstored,
        });
        self.bound(&mut state, now);
        state.wake = true;
        drop(state);
        self.changed.notify_all();
    }

    /// The control connection came back, and the server with it: what waits
    /// for a retry is tried now.
    pub(crate) fn connected(&self) {
        let mut state = self.lock();
        state.next_attempt = 0;
        state.reconnected = true;
        state.wake = true;
        drop(state);
        self.changed.notify_all();
    }

    /// The application runs whose results are kept, which the Agent still
    /// owes the server.
    pub(crate) fn application_runs(&self) -> Vec<String> {
        self.lock()
            .queue
            .iter()
            .filter(|entry| entry.collection == "application-runs")
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Drops what is too old, then the oldest while there is too much.
    fn bound(&self, state: &mut State, now: i64) {
        state.queue.retain(|entry| {
            let expired = now.saturating_sub(entry.created_at) > MAX_AGE_MS;
            if expired {
                tracing::warn!(
                    collection = entry.collection,
                    id = entry.id,
                    "gave up on a result the server did not take within 7 days"
                );
                self.discard(entry);
            }
            !expired
        });
        loop {
            let bytes: u64 = state.queue.iter().map(|entry| entry.bytes).sum();
            if state.queue.len() <= MAX_RESULTS && bytes <= MAX_BYTES {
                break;
            }
            let Some(oldest) = state
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(position, _)| position)
            else {
                break;
            };
            if let Some(entry) = state.queue.remove(oldest) {
                tracing::warn!(
                    collection = entry.collection,
                    id = entry.id,
                    "the Agent holds as many results as it keeps; dropped the oldest"
                );
                self.discard(&entry);
            }
        }
    }

    fn discard(&self, entry: &Entry) {
        if entry.unstored.is_none()
            && let Some(directory) = &self.directory
        {
            remove(&directory.join(&entry.name));
        }
    }

    /// Takes `name` out of the queue and off the disk.
    fn forget(&self, state: &mut State, name: &str) {
        if let Some(position) = state.queue.iter().position(|entry| entry.name == name)
            && let Some(entry) = state.queue.remove(position)
        {
            self.discard(&entry);
        }
    }

    /// The result `entry` names, from memory or its file.
    fn load(&self, name: &str, unstored: Option<Final>) -> anyhow::Result<Final> {
        if let Some(result) = unstored {
            return Ok(result);
        }
        let directory = self.directory.as_ref().context("no folder for results")?;
        Ok(read(&directory.join(name))?.0.result)
    }

    /// Delivers what is due at `now` with `sender`, one at a time, until
    /// nothing is left or a retry must wait. Returns when the next attempt is
    /// due, if anything is left.
    fn deliver(&self, sender: &dyn Deliver, now: i64) -> Option<i64> {
        loop {
            let (name, unstored) = {
                let mut state = self.lock();
                self.bound(&mut state, now);
                let front = state.queue.front()?;
                if now < state.next_attempt {
                    return Some(state.next_attempt);
                }
                let taken = (front.name.clone(), front.unstored.clone());
                state.reconnected = false;
                taken
            };
            let loaded = self
                .load(&name, unstored)
                .and_then(|result| Ok((result.body()?, result)));
            let (body, result) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    tracing::warn!(
                        file = name,
                        error = format!("{error:#}"),
                        "removed a stored result the Agent cannot read"
                    );
                    self.forget(&mut self.lock(), &name);
                    continue;
                }
            };
            let sent = sender.post(&result.path(), &body);
            let (collection, id) = (result.collection(), result.id());
            let mut state = self.lock();
            match verdict(&sent) {
                Verdict::Taken => {
                    tracing::info!(collection, id, "delivered a result");
                    state.failures = 0;
                    self.forget(&mut state, &name);
                }
                Verdict::Refused => {
                    tracing::warn!(
                        collection,
                        id,
                        ?sent,
                        "the server refused a result; it is dropped"
                    );
                    state.failures = 0;
                    self.forget(&mut state, &name);
                }
                Verdict::Retry => {
                    state.failures = state.failures.saturating_add(1);
                    let wait = (self.jitter)(backoff(state.failures));
                    tracing::warn!(
                        collection,
                        id,
                        ?sent,
                        retry_ms = wait.as_millis() as u64,
                        "could not deliver a result; retrying"
                    );
                    if !std::mem::take(&mut state.reconnected) {
                        state.next_attempt =
                            now.saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX));
                    }
                    if let Some(position) = state.queue.iter().position(|entry| entry.name == name)
                        && let Some(entry) = state.queue.remove(position)
                    {
                        state.queue.push_back(entry);
                    }
                }
            }
        }
    }

    /// Delivers with `sender` on a thread of its own, for as long as the
    /// Agent runs.
    pub(crate) fn spawn(self: &Arc<Self>, sender: impl Deliver + Send + 'static) {
        let outbox = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-results".into())
            .spawn(move || {
                loop {
                    let next = outbox.deliver(&sender, crate::remote::updates::now_unix_ms());
                    let mut state = outbox.lock();
                    while !state.wake {
                        let now = crate::remote::updates::now_unix_ms();
                        let wait = next.map_or(IDLE_WAKE, |at| {
                            Duration::from_millis(
                                u64::try_from(at.saturating_sub(now)).unwrap_or(0),
                            )
                        });
                        let wait = wait.min(IDLE_WAKE);
                        if wait.is_zero() {
                            break;
                        }
                        let (guard, waited) = outbox
                            .changed
                            .wait_timeout(state, wait)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state = guard;
                        if waited.timed_out() {
                            break;
                        }
                    }
                    state.wake = false;
                }
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start the thread that delivers results");
        }
    }
}

/// The folder results are kept in, made private if it is new: beside the
/// configuration, or a temporary one for a console Agent.
fn directory(mode: crate::remote::config::ExecutionMode) -> anyhow::Result<PathBuf> {
    if mode == crate::remote::config::ExecutionMode::Console {
        let folder = std::env::temp_dir().join("meshrmm-results");
        std::fs::create_dir_all(&folder)?;
        return Ok(folder);
    }
    let folder = crate::installer::config_directory()?.join("results");
    #[cfg(windows)]
    crate::private_directory::secure(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)?;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(folder)
}

/// Reads a stored result, with its size.
fn read(path: &Path) -> anyhow::Result<(Stored, u64)> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let stored = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a stored result", path.display()))?;
    Ok((stored, bytes.len() as u64))
}

fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(file = %path.display(), %error, "could not remove a stored result");
    }
}

/// Delivers over HTTPS with the Agent's credential, which
/// [`Https::credential`] lets the coordinator replace when it rotates.
pub(crate) struct Https {
    server: String,
    device_id: String,
    token: Arc<Mutex<String>>,
    http: ureq::Agent,
}

impl Https {
    pub(crate) fn new(config: &crate::remote::config::Config) -> Self {
        Self {
            server: config.server.clone(),
            device_id: config.device_id.clone(),
            token: Arc::new(Mutex::new(config.agent_token.clone())),
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(60)))
                .http_status_as_error(false)
                .tls_config(crate::enrollment::https_tls_config())
                .build()
                .new_agent(),
        }
    }

    pub(crate) fn credential(&self) -> Arc<Mutex<String>> {
        Arc::clone(&self.token)
    }
}

impl Deliver for Https {
    fn post(&self, path: &[&str], body: &[u8]) -> Sent {
        let mut segments = vec!["v1", "agents", self.device_id.as_str()];
        segments.extend_from_slice(path);
        let url = match meshrmm_signaling_client::endpoint_url(&self.server, &segments, &[], false)
        {
            Ok(url) => url,
            Err(error) => return Sent::Unreachable(format!("{error:#}")),
        };
        let token = self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match self
            .http
            .post(url.as_str())
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body)
        {
            Ok(response) => Sent::Status(response.status().as_u16()),
            Err(ureq::Error::StatusCode(status)) => Sent::Status(status),
            Err(error) => Sent::Unreachable(error.to_string()),
        }
    }
}

#[cfg(test)]
impl Outbox {
    /// An outbox that keeps results in memory, without jitter.
    pub(crate) fn in_memory() -> Self {
        Self::new(None, |wait| wait)
    }

    /// Waits up to `wait` for a result and takes the oldest, as if the
    /// server had it.
    pub(crate) fn take(&self, wait: Duration) -> Option<Final> {
        let deadline = std::time::Instant::now() + wait;
        let mut state = self.lock();
        loop {
            if let Some(entry) = state.queue.pop_front() {
                drop(state);
                let result = self.load(&entry.name, entry.unstored.clone()).ok();
                self.discard(&entry);
                return result;
            }
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use meshrmm_protocol::{FileDeliveryStatus, ScriptRunStatus};

    use super::*;

    /// A folder of its own, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("meshrmm-outbox-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|item| item.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Answers each post with the next of its answers, then with a success,
    /// and remembers what was posted.
    #[derive(Default)]
    struct Server {
        answers: RefCell<VecDeque<Sent>>,
        posted: RefCell<Vec<String>>,
    }

    impl Server {
        fn answering(answers: impl IntoIterator<Item = Sent>) -> Self {
            Self {
                answers: RefCell::new(answers.into_iter().collect()),
                ..Self::default()
            }
        }

        fn posted(&self) -> Vec<String> {
            self.posted.borrow().clone()
        }
    }

    impl Deliver for Server {
        fn post(&self, path: &[&str], body: &[u8]) -> Sent {
            serde_json::from_slice::<serde_json::Value>(body).expect("a JSON body");
            self.posted.borrow_mut().push(path.join("/"));
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or(Sent::Status(204))
        }
    }

    fn script(run_id: &str) -> Final {
        Final::ScriptRun {
            run_id: run_id.into(),
            report: ScriptRunReport {
                status: ScriptRunStatus::Completed,
                ran_as: "SYSTEM".into(),
                exit_code: Some(0),
                stdout: "done".into(),
                stderr: String::new(),
                output_truncated: false,
                error: None,
            },
        }
    }

    fn with_output(run_id: &str, bytes: usize) -> Final {
        let Final::ScriptRun { run_id, mut report } = script(run_id) else {
            unreachable!()
        };
        report.stdout = "a".repeat(bytes);
        Final::ScriptRun { run_id, report }
    }

    const NOW: i64 = 1_800_000_000_000;

    fn open(scratch: &Scratch, now: i64) -> Outbox {
        Outbox::open(scratch.0.clone(), now, |wait| wait)
    }

    fn ms(wait: Duration) -> i64 {
        wait.as_millis() as i64
    }

    #[test]
    fn results_are_on_disk_until_the_server_takes_them() {
        let scratch = Scratch::new();
        let outbox = open(&scratch, NOW);
        outbox.add(script("run"), NOW);
        assert_eq!(scratch.files().len(), 1, "written before any attempt");
        let server = Server::default();
        assert_eq!(outbox.deliver(&server, NOW), None);
        assert_eq!(server.posted(), ["script-runs/run/result"]);
        assert!(scratch.files().is_empty(), "removed once acknowledged");
    }

    #[test]
    fn server_errors_and_network_errors_are_retried_with_a_growing_wait() {
        let scratch = Scratch::new();
        let outbox = open(&scratch, NOW);
        outbox.add(script("run"), NOW);
        let server = Server::answering([
            Sent::Status(502),
            Sent::Unreachable("connection refused".into()),
            Sent::Status(503),
        ]);
        let first = NOW + ms(FIRST_RETRY);
        assert_eq!(outbox.deliver(&server, NOW), Some(first));
        // Nothing is sent before it is due.
        assert_eq!(outbox.deliver(&server, first - 1), Some(first));
        assert_eq!(server.posted().len(), 1);
        let second = first + ms(FIRST_RETRY * 2);
        assert_eq!(outbox.deliver(&server, first), Some(second));
        let third = second + ms(FIRST_RETRY * 4);
        assert_eq!(outbox.deliver(&server, second), Some(third));
        assert_eq!(scratch.files().len(), 1, "kept while it fails");
        assert_eq!(outbox.deliver(&server, third), None);
        assert_eq!(server.posted().len(), 4);
        assert!(scratch.files().is_empty());
    }

    #[test]
    fn the_wait_between_retries_is_capped() {
        assert_eq!(backoff(1), FIRST_RETRY);
        assert_eq!(backoff(2), FIRST_RETRY * 2);
        assert_eq!(backoff(20), LAST_RETRY);
        assert_eq!(backoff(u32::MAX), LAST_RETRY);
    }

    #[test]
    fn a_refusal_is_final_except_timeouts_and_rate_limits() {
        let scratch = Scratch::new();
        let outbox = open(&scratch, NOW);
        outbox.add(script("unknown"), NOW);
        let server = Server::answering([Sent::Status(404)]);
        assert_eq!(outbox.deliver(&server, NOW), None);
        assert_eq!(server.posted().len(), 1, "not tried again");
        assert!(scratch.files().is_empty(), "dropped");

        let mut now = NOW;
        for status in [401, 408, 429] {
            now += DAY_MS;
            outbox.add(script("busy"), now);
            let server = Server::answering([Sent::Status(status)]);
            let retry = outbox.deliver(&server, now).expect("retried");
            assert_eq!(scratch.files().len(), 1, "{status} keeps it");
            assert_eq!(outbox.deliver(&server, retry), None);
            assert_eq!(server.posted().len(), 2, "{status}");
            assert!(scratch.files().is_empty());
        }
    }

    #[test]
    fn a_reconnect_retries_at_once() {
        let outbox = Outbox::in_memory();
        outbox.add(script("run"), NOW);
        let server = Server::answering([Sent::Status(503), Sent::Status(503)]);
        let retry = outbox.deliver(&server, NOW).unwrap();
        assert_eq!(
            outbox.deliver(&server, retry),
            Some(retry + ms(FIRST_RETRY * 2))
        );
        // A reconnect cuts the wait short.
        outbox.connected();
        assert_eq!(outbox.deliver(&server, retry + 1), None);
        assert_eq!(server.posted().len(), 3);
    }

    #[test]
    fn a_result_the_server_keeps_failing_on_does_not_hold_up_the_others() {
        let outbox = Outbox::in_memory();
        outbox.add(script("poison"), NOW);
        outbox.add(script("fine"), NOW + 1);
        let server = Server::answering([Sent::Status(500)]);
        let retry = outbox.deliver(&server, NOW + 1).unwrap();
        assert_eq!(outbox.deliver(&server, retry), None);
        assert_eq!(
            server.posted(),
            [
                "script-runs/poison/result",
                "script-runs/fine/result",
                "script-runs/poison/result"
            ]
        );
    }

    #[test]
    fn results_from_before_a_restart_are_delivered() {
        let scratch = Scratch::new();
        let before = open(&scratch, NOW);
        before.add(script("script"), NOW);
        before.add(
            Final::FileDelivery {
                delivery_id: "file".into(),
                report: FileDeliveryReport {
                    status: FileDeliveryStatus::Delivered,
                    path: Some("/srv/file".into()),
                    error: None,
                },
            },
            NOW + 1,
        );
        before.add(
            Final::UpdateInstall {
                run_id: "install".into(),
                result: UpdateInstallResult::failed("x"),
            },
            NOW + 2,
        );
        before.add(
            Final::ApplicationRun {
                run_id: "apps".into(),
                result: ApplicationRunResult::failed("x"),
            },
            NOW + 3,
        );
        // The server was down until the Agent stopped.
        let down = Server::answering([Sent::Unreachable("down".into())]);
        assert!(before.deliver(&down, NOW + 3).is_some());
        drop(before);

        let after = open(&scratch, NOW + 60_000);
        assert_eq!(after.application_runs(), ["apps"]);
        let server = Server::default();
        assert_eq!(after.deliver(&server, NOW + 60_000), None);
        assert_eq!(
            server.posted(),
            [
                "script-runs/script/result",
                "file-deliveries/file/result",
                "update-runs/install/result",
                "application-runs/apps/result"
            ],
            "oldest first"
        );
        assert!(scratch.files().is_empty());
        assert!(after.application_runs().is_empty());
    }

    #[test]
    fn an_unreadable_file_is_removed_and_holds_up_nothing() {
        let scratch = Scratch::new();
        let before = open(&scratch, NOW);
        before.add(script("first"), NOW);
        before.add(script("second"), NOW + 1);
        drop(before);
        std::fs::write(scratch.0.join("script-runs-0123.json"), b"{ not json").unwrap();
        std::fs::write(scratch.0.join("script-runs-4567.json.new"), b"{\"half").unwrap();
        std::fs::write(scratch.0.join("notes.txt"), b"not ours").unwrap();
        let after = open(&scratch, NOW + 2);
        assert_eq!(scratch.files().len(), 3, "the broken ones are gone");
        // One that breaks while kept is removed when its turn comes.
        let first = script("first").file_name();
        std::fs::write(scratch.0.join(&first), b"\0\0\0").unwrap();
        let server = Server::default();
        assert_eq!(after.deliver(&server, NOW + 2), None);
        assert_eq!(server.posted(), ["script-runs/second/result"]);
        assert_eq!(scratch.files(), ["notes.txt"]);
    }

    #[test]
    fn results_older_than_the_limit_are_dropped() {
        let scratch = Scratch::new();
        let outbox = open(&scratch, NOW);
        outbox.add(script("old"), NOW);
        outbox.add(script("recent"), NOW + DAY_MS);
        drop(outbox);
        // Still delivered at the limit, even across a restart.
        let outbox = open(&scratch, NOW + MAX_AGE_MS);
        assert_eq!(scratch.files().len(), 2);
        drop(outbox);
        let outbox = open(&scratch, NOW + MAX_AGE_MS + 1);
        assert_eq!(scratch.files(), [script("recent").file_name()]);
        // And while it waits for a retry.
        let server = Server::answering([Sent::Status(503)]);
        assert!(outbox.deliver(&server, NOW + MAX_AGE_MS + 1).is_some());
        assert_eq!(outbox.deliver(&server, NOW + DAY_MS + MAX_AGE_MS + 1), None);
        assert!(scratch.files().is_empty());
        assert_eq!(server.posted().len(), 1);
    }

    #[test]
    fn a_full_outbox_drops_its_oldest_results() {
        let ids = |outbox: &Outbox| -> Vec<String> {
            outbox.lock().queue.iter().map(|e| e.id.clone()).collect()
        };
        let outbox = Outbox::in_memory();
        for index in 0..=MAX_RESULTS as i64 {
            outbox.add(script(&format!("run-{index}")), NOW + index);
        }
        let kept = ids(&outbox);
        assert_eq!(kept.len(), MAX_RESULTS);
        assert_eq!(kept[0], "run-1", "the oldest went");

        let outbox = Outbox::in_memory();
        let large = (MAX_BYTES / 4) as usize;
        for index in 0..4 {
            outbox.add(with_output(&format!("large-{index}"), large), NOW + index);
        }
        assert_eq!(
            ids(&outbox),
            ["large-1", "large-2", "large-3"],
            "by bytes too"
        );
    }

    #[test]
    fn delivery_order_follows_age_after_a_restart_even_with_late_files() {
        // Filenames carry no order; the time inside each file does.
        let scratch = Scratch::new();
        let before = open(&scratch, NOW);
        before.add(script("zzz-newer"), NOW + 5);
        before.add(script("aaa-older"), NOW);
        drop(before);
        let after = open(&scratch, NOW + 10);
        let server = Server::default();
        after.deliver(&server, NOW + 10);
        assert_eq!(
            server.posted(),
            [
                "script-runs/aaa-older/result",
                "script-runs/zzz-newer/result"
            ]
        );
    }

    #[test]
    fn statuses_mean_what_the_server_intends() {
        for status in [200, 201, 204] {
            assert_eq!(verdict(&Sent::Status(status)), Verdict::Taken, "{status}");
        }
        for status in [400, 403, 404, 409, 410, 413, 422] {
            assert_eq!(verdict(&Sent::Status(status)), Verdict::Refused, "{status}");
        }
        for status in [401, 408, 429, 500, 502, 503, 504, 302] {
            assert_eq!(verdict(&Sent::Status(status)), Verdict::Retry, "{status}");
        }
        assert_eq!(
            verdict(&Sent::Unreachable("timed out".into())),
            Verdict::Retry
        );
    }
}
