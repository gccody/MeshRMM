//! Operating system updates: scans for pending updates on the schedule the
//! server sets ([`schedule`]) and when it asks, installs them when it asks,
//! and reports on the control connection. See docs/patch-management.md.
//!
//! The coordinator runs them as root or SYSTEM, like the system information
//! collection. Each scan or install runs on its own thread, one at a time,
//! so the control connection never waits for a package manager. A scan asked
//! for while one runs is answered by the running one; one asked for during an
//! install is answered by the scan that follows every install. Results wait
//! in a queue that outlives reconnects, so an install that finishes while the
//! Agent is offline is still reported.
//!
//! A scan has [`UPDATE_SCAN_TIMEOUT_SECONDS`] and an install, with the scans
//! around it, [`UPDATE_INSTALL_TIMEOUT_SECONDS`]: every program it runs, and
//! every search, download and install Windows Update does for it, is stopped
//! by then. One that still hasn't returned a few minutes later is given up
//! on: it's reported failed, and the next scan or install can start (see
//! [`Gate`]). Both are well inside the time the server waits before it marks
//! a run lost.
//!
//! An install scans first, picks the chosen updates from what is pending
//! then, installs them, scans again and, as the request asks, sets the
//! computer to restart after a delay that warns its user.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use meshrmm_protocol::{
    AgentStatusMessage, PendingUpdate, RebootBehavior, UPDATE_INSTALL_TIMEOUT_SECONDS,
    UPDATE_SCAN_TIMEOUT_SECONDS, UpdateInstallItem, UpdateInstallRequest, UpdateInstallResult,
    UpdateInstallStatus, UpdateItemOutcome, UpdateManager, UpdateScan,
};
use tokio::sync::mpsc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod process;
mod schedule;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
pub(crate) use self::schedule::Schedule;
#[cfg(windows)]
use self::windows as platform;

/// How long a scan, or an install with its scans, may take, and how much
/// longer it has to return before it's given up on: long enough for what it
/// runs to be stopped and for Windows Update to stop what it was asked to.
#[derive(Debug, Clone, Copy)]
struct Limits {
    scan: Duration,
    install: Duration,
    grace: Duration,
}

impl Limits {
    /// The server marks a scan lost 15 minutes after the scan's limit and an
    /// install an hour after the install's, so the Agent says first.
    const AGENT: Self = Self {
        scan: Duration::from_secs(UPDATE_SCAN_TIMEOUT_SECONDS),
        install: Duration::from_secs(UPDATE_INSTALL_TIMEOUT_SECONDS),
        grace: Duration::from_secs(5 * 60),
    };
}

/// What scans and installs: the computer's update service or package
/// manager, or a stand-in in tests.
trait Runner: Send + Sync + 'static {
    fn scan(&self) -> UpdateScan;
    fn install(
        &self,
        request: &UpdateInstallRequest,
        progress: &Progress,
    ) -> (UpdateInstallResult, Option<UpdateScan>);
}

/// This computer's update service or package manager.
struct Computer;

impl Runner for Computer {
    fn scan(&self) -> UpdateScan {
        scan_now()
    }

    fn install(
        &self,
        request: &UpdateInstallRequest,
        progress: &Progress,
    ) -> (UpdateInstallResult, Option<UpdateScan>) {
        install_now(request, progress)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Scan,
    Install,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Install => "install",
        }
    }
}

/// One scan or install at a time: package managers lock their databases,
/// and Windows Update runs one installer at a time.
///
/// One given up on lets go of its turn, so others can run, but until it
/// returns no other of its kind starts: no search or install is sent after
/// one that's stuck, and nothing installs while an install given up on
/// might still be changing the computer. A scan may run beside an install
/// given up on, since it changes nothing, and the package managers' locks
/// and Windows Update's own keep it from clashing with one. So at most one
/// scan and one install are ever left behind.
#[derive(Debug, Default)]
struct Gate {
    /// A scan or install has its turn.
    busy: bool,
    /// A scan given up on hasn't returned.
    stray_scan: bool,
    /// An install given up on hasn't returned.
    stray_install: bool,
}

impl Gate {
    fn stray(&mut self, kind: Kind) -> &mut bool {
        match kind {
            Kind::Scan => &mut self.stray_scan,
            Kind::Install => &mut self.stray_install,
        }
    }
}

/// Where a scan's or install's work is, changed only with the gate held.
const WAITING: u8 = 0;
const WORKING: u8 = 1;
const GIVEN_UP: u8 = 2;
/// It let go of its turn and answered.
const DONE: u8 = 3;

/// What the scans and installs running now owe the server.
#[derive(Debug, Default)]
struct Waiting {
    /// A scan is running, and these runs wait for it.
    scan: Option<Vec<String>>,
    /// The install running, and the scans asked for during it, which the
    /// scan after it answers.
    install: Option<(String, Vec<String>)>,
}

/// Tells the server how far an install has got.
#[derive(Clone)]
pub(crate) struct Progress {
    run_id: String,
    sender: mpsc::UnboundedSender<AgentStatusMessage>,
    /// The install was given up on and reported, so it says nothing more.
    given_up: Arc<AtomicBool>,
}

impl Progress {
    pub(crate) fn report(&self, message: impl Into<String>, percent: Option<u8>) {
        if self.given_up.load(Ordering::Relaxed) {
            return;
        }
        let message = meshrmm_protocol::clean_text(&message.into());
        tracing::info!(run_id = %self.run_id, message, ?percent, "update install progress");
        let _ = self.sender.send(AgentStatusMessage::UpdateProgress {
            run_id: self.run_id.clone(),
            message,
            percent: percent.map(|percent| percent.min(100)),
        });
    }
}

/// What the coordinator's handle and the scan and install threads share.
struct Shared {
    sender: mpsc::UnboundedSender<AgentStatusMessage>,
    waiting: Mutex<Waiting>,
    gate: Mutex<Gate>,
    /// Signalled when a turn is let go of or a stray returns.
    turn: Condvar,
    runner: Box<dyn Runner>,
    limits: Limits,
}

/// The coordinator's handle on update scans and installs. Created once per
/// Agent process, so results outlive reconnects.
pub struct Updates {
    receiver: mpsc::UnboundedReceiver<AgentStatusMessage>,
    /// A report the connection failed to send, sent first on the next one.
    unsent: Option<AgentStatusMessage>,
    shared: Arc<Shared>,
}

impl Updates {
    pub fn new() -> Self {
        Self::with(Box::new(Computer), Limits::AGENT)
    }

    fn with(runner: Box<dyn Runner>, limits: Limits) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            receiver,
            unsent: None,
            shared: Arc::new(Shared {
                sender,
                waiting: Mutex::default(),
                gate: Mutex::default(),
                turn: Condvar::new(),
                runner,
                limits,
            }),
        }
    }

    /// Scans: for the server's run `run_id`, or on the schedule (none).
    pub fn scan(&mut self, run_id: Option<String>) {
        let mut waiting = lock(&self.shared.waiting);
        if let Some((_, runs)) = &mut waiting.install {
            runs.extend(run_id);
            return;
        }
        if let Some(runs) = &mut waiting.scan {
            runs.extend(run_id);
            return;
        }
        waiting.scan = Some(run_id.into_iter().collect());
        drop(waiting);
        self.shared.start_scan();
    }

    /// The server asked for an install.
    pub fn install(&mut self, request: UpdateInstallRequest) {
        let shared = &self.shared;
        let mut waiting = lock(&shared.waiting);
        if let Some((running, _)) = &waiting.install {
            let result = UpdateInstallResult::failed(format!(
                "The device is already installing updates (run {running}). Try again when it finishes."
            ));
            let _ = shared.sender.send(AgentStatusMessage::UpdateInstall {
                run_id: request.run_id,
                result: Box::new(result),
            });
            return;
        }
        waiting.install = Some((request.run_id.clone(), Vec::new()));
        drop(waiting);
        let run_id = request.run_id.clone();
        let progress = Progress {
            run_id: request.run_id.clone(),
            sender: shared.sender.clone(),
            given_up: Arc::default(),
        };
        let thread = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-update-install".into())
            .spawn(move || thread.install(request, progress));
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for an update install");
            lock(&shared.waiting).install = None;
            let _ = shared.sender.send(AgentStatusMessage::UpdateInstall {
                run_id,
                result: Box::new(UpdateInstallResult::failed(
                    "The Agent could not start the install.",
                )),
            });
        }
    }

    /// The next report for the server. Cancel safe.
    pub async fn next(&mut self) -> AgentStatusMessage {
        if let Some(unsent) = self.unsent.take() {
            return unsent;
        }
        // The sender lives in `self`, so the channel never closes.
        self.receiver
            .recv()
            .await
            .expect("the update channel stays open")
    }

    /// The connection couldn't send `report`; the next one sends it first.
    /// Progress is dropped, since a newer one or the result follows.
    pub fn unsent(&mut self, report: AgentStatusMessage) {
        if !matches!(report, AgentStatusMessage::UpdateProgress { .. }) {
            self.unsent = Some(report);
        }
    }
}

impl Shared {
    /// Starts the scan `waiting.scan` was set for.
    fn start_scan(self: &Arc<Self>) {
        let thread = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-update-scan".into())
            .spawn(move || {
                let scan = thread
                    .work(Kind::Scan, |runner| runner.scan())
                    .unwrap_or_else(|error| UpdateScan {
                        scanned_at_unix_ms: now_unix_ms(),
                        error: Some(error),
                        ..Default::default()
                    });
                let run_ids = lock(&thread.waiting).scan.take().unwrap_or_default();
                let _ = thread.sender.send(AgentStatusMessage::UpdateScan {
                    run_ids,
                    scan: Box::new(scan),
                });
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for an update scan");
            lock(&self.waiting).scan = None;
        }
    }

    /// Installs, on the install's thread, and reports.
    fn install(self: &Arc<Self>, request: UpdateInstallRequest, progress: Progress) {
        let given_up = Arc::clone(&progress.given_up);
        let worker = (request.clone(), progress);
        let outcome = self.work(Kind::Install, move |runner| {
            let (request, progress) = worker;
            runner.install(&request, &progress)
        });
        let (result, scan) = match outcome {
            Ok(done) => done,
            Err(error) => {
                given_up.store(true, Ordering::Relaxed);
                (UpdateInstallResult::failed(error), None)
            }
        };
        tracing::info!(
            run_id = %request.run_id,
            status = result.status.as_str(),
            installed = result.items.iter().filter(|item| item.outcome == UpdateItemOutcome::Installed).count(),
            failed = result.items.iter().filter(|item| item.outcome == UpdateItemOutcome::Failed).count(),
            error = ?result.error,
            reboot_required = result.reboot_required,
            reboot_scheduled_at = ?result.reboot_scheduled_at_unix_ms,
            "update install finished"
        );
        let mut waiting = lock(&self.waiting);
        let run_ids = waiting
            .install
            .take()
            .map(|(_, runs)| runs)
            .unwrap_or_default();
        let _ = self.sender.send(AgentStatusMessage::UpdateInstall {
            run_id: request.run_id.clone(),
            result: Box::new(result.sanitized()),
        });
        match scan {
            Some(scan) => {
                drop(waiting);
                let _ = self.sender.send(AgentStatusMessage::UpdateScan {
                    run_ids,
                    scan: Box::new(scan),
                });
            }
            // It never got to the scan after it, so a scan of its own
            // answers the ones asked for meanwhile.
            None => match &mut waiting.scan {
                Some(runs) => runs.extend(run_ids),
                None => {
                    waiting.scan = Some(run_ids);
                    drop(waiting);
                    self.start_scan();
                }
            },
        }
    }

    /// Runs `job` on a thread of its own, in its turn and by its kind's
    /// deadline, and waits for it. One that hasn't returned a little after
    /// the deadline is given up on: its thread is left to finish, and what
    /// it returns is dropped. Errs with why, for the server, when it was
    /// given up on or couldn't start.
    fn work<T: Send + 'static>(
        self: &Arc<Self>,
        kind: Kind,
        job: impl FnOnce(&dyn Runner) -> T + Send + 'static,
    ) -> Result<T, String> {
        let allowed = match kind {
            Kind::Scan => self.limits.scan,
            Kind::Install => self.limits.install,
        };
        let deadline = Instant::now() + allowed;
        let stage = Arc::new(AtomicU8::new(WAITING));
        let (done, result) = std::sync::mpsc::sync_channel(1);
        let worker = Arc::clone(self);
        let worker_stage = Arc::clone(&stage);
        let spawned = std::thread::Builder::new()
            .name(format!("meshrmm-update-{}-work", kind.word()))
            .spawn(move || {
                if let Err(refused) = worker.enter(kind, deadline, &worker_stage) {
                    let _ = done.send(Err(refused));
                    return;
                }
                // Every program it runs, and Windows Update's work, stops
                // by the deadline. A panic, such as a parser's, ends the scan
                // or install like an error, and its turn is let go of as on
                // any return. Asserting unwind safety is sound: after a
                // panic nothing the job touched is used again but the
                // runner, which keeps no state, the progress channel and
                // atomics; the gate and waiting locks aren't held while it
                // runs, and `by` restores the thread's deadline as it
                // unwinds.
                let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::remote::process::by(deadline, || job(worker.runner.as_ref()))
                }))
                .map_err(|panic| {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "no message".to_owned());
                    tracing::error!(kind = kind.word(), message, "update work panicked");
                    format!(
                        "The {} stopped on an error in the Agent: {message}",
                        kind.word()
                    )
                });
                let mut gate = lock(&worker.gate);
                if worker_stage.load(Ordering::Relaxed) == GIVEN_UP {
                    *gate.stray(kind) = false;
                    tracing::warn!(
                        kind = kind.word(),
                        "update work the Agent gave up on returned"
                    );
                } else {
                    gate.busy = false;
                    worker_stage.store(DONE, Ordering::Relaxed);
                    let _ = done.send(value);
                }
                drop(gate);
                worker.turn.notify_all();
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for update work");
            return Err(format!("The Agent could not start the {}.", kind.word()));
        }
        let give_up = deadline + self.limits.grace;
        match result.recv_timeout(give_up.saturating_duration_since(Instant::now())) {
            Ok(outcome) => return outcome,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // The thread ended without answering, which nothing above
            // should let happen. It's gone, so it isn't left behind: if it
            // still had its turn, that's let go of, and nothing waits for it.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let mut gate = lock(&self.gate);
                if stage.swap(DONE, Ordering::Relaxed) == WORKING {
                    gate.busy = false;
                }
                drop(gate);
                self.turn.notify_all();
                tracing::error!(kind = kind.word(), "update work ended without answering");
                return Err(format!(
                    "The {} ended without a result because of an error in the Agent.",
                    kind.word()
                ));
            }
        }
        let mut gate = lock(&self.gate);
        // It may have returned just now.
        if let Ok(outcome) = result.try_recv() {
            return outcome;
        }
        if stage.swap(GIVEN_UP, Ordering::Relaxed) == WORKING {
            gate.busy = false;
            *gate.stray(kind) = true;
        }
        drop(gate);
        self.turn.notify_all();
        tracing::error!(
            kind = kind.word(),
            allowed_s = allowed.as_secs(),
            "gave up on update work that did not return"
        );
        Err(format!(
            "{} did not answer within {}.",
            platform::SERVICE,
            span(allowed)
        ))
    }

    /// Waits for `kind`'s turn until `deadline`; see [`Gate`].
    fn enter(&self, kind: Kind, deadline: Instant, stage: &AtomicU8) -> Result<(), String> {
        let mut gate = lock(&self.gate);
        loop {
            if stage.load(Ordering::Relaxed) == GIVEN_UP {
                return Err(format!("The {} was given up on.", kind.word()));
            }
            let stray = *gate.stray(kind);
            if !gate.busy && !stray {
                gate.busy = true;
                stage.store(WORKING, Ordering::Relaxed);
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(if stray {
                    format!(
                        "{} still hasn't finished an earlier {} the Agent gave up on; restarting the computer ends it.",
                        platform::SERVICE,
                        kind.word()
                    )
                } else {
                    format!(
                        "The {} waited too long for another scan or install to finish.",
                        kind.word()
                    )
                });
            }
            gate = self
                .turn
                .wait_timeout(gate, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// A time allowed, in words: "30 minutes", "4 hours".
pub(crate) fn span(time: Duration) -> String {
    let minutes = time.as_secs().div_ceil(60).max(1);
    match minutes {
        1 => "1 minute".to_owned(),
        60 => "1 hour".to_owned(),
        _ if minutes >= 120 && minutes.is_multiple_of(60) => format!("{} hours", minutes / 60),
        _ => format!("{minutes} minutes"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Nothing panics while holding these locks, so a poisoned one is whole.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Unix milliseconds now.
pub(crate) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Scans, logging how it went.
fn scan_now() -> UpdateScan {
    let started = std::time::Instant::now();
    let mut scan = platform::scan();
    scan.scanned_at_unix_ms = now_unix_ms();
    let scan = scan.sanitized();
    tracing::info!(
        manager = scan.manager.map(UpdateManager::as_str),
        pending = scan.updates.len(),
        reboot_pending = scan.reboot_pending,
        error = ?scan.error,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "scanned for updates"
    );
    scan
}

/// What an install of `request` installs from `scan`: the chosen pending
/// updates, and the chosen IDs that are no longer pending.
fn choose<'a>(
    request: &UpdateInstallRequest,
    scan: &'a UpdateScan,
) -> (Vec<&'a PendingUpdate>, Vec<String>) {
    let chosen = scan
        .updates
        .iter()
        .filter(|update| request.selection.covers(update))
        .collect::<Vec<_>>();
    let gone = request
        .selection
        .ids
        .iter()
        .filter(|id| !scan.updates.iter().any(|update| update.id == **id))
        .cloned()
        .collect();
    (chosen, gone)
}

/// Installs what `request` chooses, and returns how it went and the scan
/// after it, if it got that far.
fn install_now(
    request: &UpdateInstallRequest,
    progress: &Progress,
) -> (UpdateInstallResult, Option<UpdateScan>) {
    progress.report("Looking for updates", None);
    let before = scan_now();
    if let Some(error) = &before.error {
        return (
            UpdateInstallResult::failed(format!("Could not look for updates: {error}")),
            Some(before),
        );
    }
    let Some(manager) = before.manager else {
        return (
            UpdateInstallResult::failed(
                "The device has no update service or package manager the Agent supports.",
            ),
            Some(before),
        );
    };
    let (chosen, gone) = choose(request, &before);
    let everything = chosen.len() == before.updates.len();
    if !manager.installs_selected() && !chosen.is_empty() && !everything {
        return (
            UpdateInstallResult::failed(format!(
                "{} installs every pending update at once; choose all of them.",
                manager.label()
            )),
            Some(before),
        );
    }
    let mut items = gone
        .into_iter()
        .map(|id| UpdateInstallItem {
            title: id.clone(),
            id,
            kb: None,
            category: meshrmm_protocol::UpdateCategory::Other,
            outcome: UpdateItemOutcome::Failed,
            error: Some("It is no longer pending.".into()),
            reboot_required: false,
        })
        .collect::<Vec<_>>();
    let mut error = None;
    if !chosen.is_empty() {
        match platform::install(manager, &chosen, everything, progress) {
            Ok(installed) => items.extend(installed),
            Err(failure) => {
                error = Some(format!("{failure:#}"));
                items.extend(chosen.iter().map(|update| UpdateInstallItem {
                    id: update.id.clone(),
                    title: update.title.clone(),
                    kb: update.kb.clone(),
                    category: update.category,
                    outcome: UpdateItemOutcome::Failed,
                    error: None,
                    reboot_required: false,
                }));
            }
        }
    }
    progress.report("Checking what is still pending", None);
    let after = scan_now();
    let reboot_required = items.iter().any(|item| item.reboot_required) || after.reboot_pending;
    let mut result = UpdateInstallResult {
        status: if error.is_some() {
            UpdateInstallStatus::Failed
        } else {
            UpdateInstallStatus::of(&items)
        },
        error,
        items,
        reboot_required,
        reboot_scheduled_at_unix_ms: None,
        reboot_error: None,
    };
    let restart = match request.reboot {
        RebootBehavior::Never => false,
        RebootBehavior::IfRequired => reboot_required,
        RebootBehavior::Always => true,
    };
    if restart {
        let delay = Duration::from_secs(u64::from(request.reboot_delay_minutes.max(1)) * 60);
        match crate::power::schedule_restart(delay, RESTART_MESSAGE) {
            Ok(()) => {
                result.reboot_scheduled_at_unix_ms =
                    Some(now_unix_ms() + i64::try_from(delay.as_millis()).unwrap_or(0));
            }
            Err(failure) => {
                tracing::warn!(error = ?failure, "could not set the computer to restart after updates");
                result.reboot_error = Some(format!("{failure:#}"));
            }
        }
    }
    (result, Some(after))
}

/// What the signed-in user is told before the computer restarts.
pub(crate) const RESTART_MESSAGE: &str = "Your IT administrator installed updates. This computer will restart to finish installing them. Save your work.";

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{UpdateCategory, UpdateSelection};

    use super::*;

    fn update(id: &str, category: UpdateCategory) -> PendingUpdate {
        PendingUpdate {
            id: id.into(),
            title: id.to_uppercase(),
            kb: None,
            version: None,
            current_version: None,
            category,
            severity: None,
            size_bytes: None,
            reboot_required: false,
            released_at_unix_ms: None,
            source: None,
        }
    }

    fn request(selection: UpdateSelection) -> UpdateInstallRequest {
        UpdateInstallRequest {
            run_id: "run".into(),
            selection,
            reboot: RebootBehavior::Never,
            reboot_delay_minutes: 5,
        }
    }

    /// Scans and installs that take no time, except the first few, which
    /// hang until let go or panic.
    #[derive(Default)]
    struct Fake {
        hanging_scans: usize,
        hanging_installs: usize,
        panicking_scans: usize,
        panicking_installs: usize,
        scans: std::sync::atomic::AtomicUsize,
        installs: std::sync::atomic::AtomicUsize,
        let_go: Mutex<bool>,
        going: Condvar,
    }

    impl Fake {
        fn hang(&self) {
            let mut let_go = lock(&self.let_go);
            while !*let_go {
                let_go = self.going.wait(let_go).unwrap();
            }
        }

        fn let_go(&self) {
            *lock(&self.let_go) = true;
            self.going.notify_all();
        }

        fn scans(&self) -> usize {
            self.scans.load(Ordering::SeqCst)
        }

        fn installs(&self) -> usize {
            self.installs.load(Ordering::SeqCst)
        }
    }

    impl Runner for Arc<Fake> {
        fn scan(&self) -> UpdateScan {
            let scan = self.scans.fetch_add(1, Ordering::SeqCst);
            if scan < self.panicking_scans {
                panic!("a parser met output it didn't expect");
            }
            if scan < self.hanging_scans {
                self.hang();
            }
            UpdateScan {
                manager: Some(UpdateManager::Apt),
                ..Default::default()
            }
        }

        fn install(
            &self,
            _: &UpdateInstallRequest,
            _: &Progress,
        ) -> (UpdateInstallResult, Option<UpdateScan>) {
            let install = self.installs.fetch_add(1, Ordering::SeqCst);
            if install < self.panicking_installs {
                panic!("{}", String::from("an installer's result was unreadable"));
            }
            if install < self.hanging_installs {
                self.hang();
            }
            let result = UpdateInstallResult {
                status: UpdateInstallStatus::Succeeded,
                ..UpdateInstallResult::failed("")
            };
            (result, Some(UpdateScan::default()))
        }
    }

    const LIMITS: Limits = Limits {
        scan: Duration::from_secs(1),
        install: Duration::from_secs(1),
        grace: Duration::from_millis(500),
    };

    fn updates(fake: &Arc<Fake>) -> Updates {
        Updates::with(Box::new(Arc::clone(fake)), LIMITS)
    }

    fn install_of(run_id: &str) -> UpdateInstallRequest {
        UpdateInstallRequest {
            run_id: run_id.into(),
            ..request(UpdateSelection::default())
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// The next report, or `None` if none comes within `wait`.
    fn report_within(updates: &mut Updates, wait: Duration) -> Option<AgentStatusMessage> {
        runtime()
            .block_on(async { tokio::time::timeout(wait, updates.next()).await })
            .ok()
    }

    fn next_report(updates: &mut Updates) -> AgentStatusMessage {
        report_within(updates, Duration::from_secs(30)).expect("a report")
    }

    #[track_caller]
    fn install_result(report: AgentStatusMessage) -> (String, UpdateInstallResult) {
        match report {
            AgentStatusMessage::UpdateInstall { run_id, result } => (run_id, *result),
            other => panic!("not an install result: {other:?}"),
        }
    }

    #[track_caller]
    fn scan_result(report: AgentStatusMessage) -> (Vec<String>, UpdateScan) {
        match report {
            AgentStatusMessage::UpdateScan { run_ids, scan } => (run_ids, *scan),
            other => panic!("not a scan: {other:?}"),
        }
    }

    #[test]
    fn an_install_that_never_returns_is_given_up_on_and_the_next_runs() {
        let fake = Arc::new(Fake {
            hanging_installs: 1,
            ..Default::default()
        });
        let mut updates = updates(&fake);
        let started = Instant::now();
        updates.install(install_of("stuck"));
        updates.scan(Some("asked meanwhile".into()));
        updates.install(install_of("refused"));
        let (run_id, result) = install_result(next_report(&mut updates));
        assert_eq!(run_id, "refused");
        assert!(result.error.unwrap().contains("already installing"));

        let (run_id, result) = install_result(next_report(&mut updates));
        assert!(started.elapsed() >= LIMITS.install + LIMITS.grace);
        assert_eq!(run_id, "stuck");
        assert_eq!(result.status, UpdateInstallStatus::Failed);
        let error = result.error.unwrap();
        assert_eq!(
            error,
            format!("{} did not answer within 1 minute.", platform::SERVICE)
        );
        // The scan asked for meanwhile runs now.
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert_eq!(run_ids, ["asked meanwhile"]);
        assert_eq!(scan.error, None);
        assert_eq!(fake.scans(), 1);

        // An install is accepted, and starts once the stuck one returns.
        updates.install(install_of("next"));
        fake.let_go();
        let (run_id, result) = install_result(next_report(&mut updates));
        assert_eq!(run_id, "next");
        assert_eq!(result.status, UpdateInstallStatus::Succeeded);
        assert_eq!(fake.installs(), 2);
        let (run_ids, _) = scan_result(next_report(&mut updates));
        assert!(run_ids.is_empty());
        // The stuck install's own result, when it came, wasn't reported.
        assert!(report_within(&mut updates, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn nothing_installs_while_an_install_given_up_on_might_still_be_running() {
        let fake = Arc::new(Fake {
            hanging_installs: 1,
            ..Default::default()
        });
        let mut updates = updates(&fake);
        updates.install(install_of("stuck"));
        let (_, result) = install_result(next_report(&mut updates));
        assert!(result.error.unwrap().contains("did not answer"));
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert!(run_ids.is_empty());
        assert_eq!(scan.error, None);

        updates.install(install_of("next"));
        let (run_id, result) = install_result(next_report(&mut updates));
        assert_eq!(run_id, "next");
        assert_eq!(
            result.error.unwrap(),
            format!(
                "{} still hasn't finished an earlier install the Agent gave up on; restarting the computer ends it.",
                platform::SERVICE
            )
        );
        assert_eq!(fake.installs(), 1, "the second install didn't start");
        let _ = scan_result(next_report(&mut updates));
        // Scans still run.
        updates.scan(Some("scan".into()));
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert_eq!(run_ids, ["scan"]);
        assert_eq!(scan.error, None);
        fake.let_go();
    }

    #[test]
    fn a_scan_that_never_returns_is_given_up_on() {
        let fake = Arc::new(Fake {
            hanging_scans: 1,
            ..Default::default()
        });
        let mut updates = updates(&fake);
        updates.scan(Some("first".into()));
        updates.scan(Some("second".into()));
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert_eq!(run_ids, ["first", "second"]);
        assert_eq!(
            scan.error.unwrap(),
            format!("{} did not answer within 1 minute.", platform::SERVICE)
        );

        // An install doesn't wait for it.
        updates.install(install_of("install"));
        let (run_id, result) = install_result(next_report(&mut updates));
        assert_eq!(run_id, "install");
        assert_eq!(result.status, UpdateInstallStatus::Succeeded);
        let _ = scan_result(next_report(&mut updates));

        // Another scan waits for it, and runs once it returns.
        updates.scan(Some("third".into()));
        fake.let_go();
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert_eq!(run_ids, ["third"]);
        assert_eq!(scan.error, None);
        assert_eq!(fake.scans(), 2);
        assert!(report_within(&mut updates, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn a_scan_that_panics_is_reported_and_the_next_runs() {
        let fake = Arc::new(Fake {
            panicking_scans: 1,
            ..Default::default()
        });
        let mut updates = updates(&fake);
        updates.scan(Some("first".into()));
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert_eq!(run_ids, ["first"]);
        assert_eq!(
            scan.error.unwrap(),
            "The scan stopped on an error in the Agent: a parser met output it didn't expect"
        );
        assert!(report_within(&mut updates, Duration::from_millis(300)).is_none());

        // Its turn was let go of: the next scan runs at once.
        let started = Instant::now();
        updates.scan(Some("second".into()));
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert!(started.elapsed() < LIMITS.scan);
        assert_eq!(run_ids, ["second"]);
        assert_eq!(scan.error, None);
        assert_eq!(fake.scans(), 2);
    }

    #[test]
    fn an_install_that_panics_is_reported_failed_and_the_next_is_accepted() {
        let fake = Arc::new(Fake {
            panicking_installs: 1,
            ..Default::default()
        });
        let mut updates = updates(&fake);
        updates.install(install_of("panics"));
        updates.scan(Some("asked meanwhile".into()));
        let (run_id, result) = install_result(next_report(&mut updates));
        assert_eq!(run_id, "panics");
        assert_eq!(result.status, UpdateInstallStatus::Failed);
        assert_eq!(
            result.error.unwrap(),
            "The install stopped on an error in the Agent: an installer's result was unreadable"
        );
        let (run_ids, scan) = scan_result(next_report(&mut updates));
        assert_eq!(run_ids, ["asked meanwhile"]);
        assert_eq!(scan.error, None);

        let started = Instant::now();
        updates.install(install_of("next"));
        let (run_id, result) = install_result(next_report(&mut updates));
        assert!(started.elapsed() < LIMITS.install);
        assert_eq!(run_id, "next");
        assert_eq!(result.status, UpdateInstallStatus::Succeeded);
        assert_eq!(fake.installs(), 2);
        let _ = scan_result(next_report(&mut updates));
        assert!(report_within(&mut updates, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn times_allowed_read_as_people_say_them() {
        assert_eq!(span(Limits::AGENT.scan), "30 minutes");
        assert_eq!(span(Limits::AGENT.install), "4 hours");
        assert_eq!(span(Duration::from_secs(60 * 60)), "1 hour");
        assert_eq!(span(Duration::from_secs(90 * 60)), "90 minutes");
        assert_eq!(span(Duration::from_millis(1)), "1 minute");
    }

    #[test]
    fn installs_choose_pending_updates_and_name_the_missing_ones() {
        let scan = UpdateScan {
            updates: vec![
                update("a", UpdateCategory::Security),
                update("b", UpdateCategory::Other),
            ],
            ..Default::default()
        };
        let (chosen, gone) = choose(&request(UpdateSelection::default()), &scan);
        assert_eq!(chosen.len(), 2);
        assert!(gone.is_empty());
        let (chosen, gone) = choose(
            &request(UpdateSelection {
                ids: vec!["b".into(), "c".into()],
                categories: vec![],
            }),
            &scan,
        );
        assert_eq!(
            chosen.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(gone, ["c"]);
        let (chosen, _) = choose(
            &request(UpdateSelection {
                ids: vec![],
                categories: vec![UpdateCategory::Security],
            }),
            &scan,
        );
        assert_eq!(
            chosen.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
    }
}
