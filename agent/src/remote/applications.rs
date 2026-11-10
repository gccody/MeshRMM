//! Third-party applications: scans the package managers the device has for
//! installed and outdated applications, and installs, updates and uninstalls
//! them when the server asks, then reports on the control connection. See
//! docs/application-deployment.md.
//!
//! The coordinator runs them as root or SYSTEM, like update scans. Each scan
//! or run gets its own thread, one at a time, so the control connection
//! never waits for a package manager or an installer. A scan asked for while
//! a run works is answered by the scan after it. Results wait in a queue that
//! outlives reconnects, so a run that finishes offline is still reported.
//!
//! A run scans first, so it knows what is installed and outdated, works
//! through its applications one at a time, each with
//! [`APPLICATION_ITEM_TIMEOUT_SECONDS`], and scans again. The update scan's
//! cadence drives application scans too: the coordinator starts one with
//! every update scan the server asks for.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meshrmm_protocol::{
    APPLICATION_ITEM_TIMEOUT_SECONDS, APPLICATION_RUN_TIMEOUT_SECONDS,
    APPLICATION_SCAN_TIMEOUT_SECONDS, APPLICATION_SCAN_TOTAL_SECONDS, AgentStatusMessage,
    ApplicationOperation, ApplicationRunRequest, ApplicationRunResult, ApplicationScan,
    CustomInstaller, DetectionRule, InstallItem, ItemOutcome, ItemResult, MAX_RUN_ITEMS,
    ManagerState, PackageKey, PackageRef, PackageSource, RunStatus, ScannedPackage, UninstallItem,
    UpdateChoice, UpdateMode,
};
use tokio::sync::mpsc;

use crate::remote::config::{Config, ExecutionMode};
use crate::remote::process;

mod download;
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

/// One scan or run at a time: package managers lock their databases, and
/// Windows Installer runs one install at a time.
static OPERATION: Mutex<()> = Mutex::new(());

/// How long one install, update or uninstall may take.
pub(crate) const ITEM_TIMEOUT: Duration = Duration::from_secs(APPLICATION_ITEM_TIMEOUT_SECONDS);

/// How one install, update or uninstall went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub(crate) outcome: ItemOutcome,
    pub(crate) exit_code: Option<i64>,
    pub(crate) error: Option<String>,
    pub(crate) output: String,
    pub(crate) reboot_required: bool,
}

impl Attempt {
    pub(crate) fn failed(error: impl Into<String>) -> Self {
        Self {
            outcome: ItemOutcome::Failed,
            exit_code: None,
            error: Some(error.into()),
            output: String::new(),
            reboot_required: false,
        }
    }

    /// Not for this device, and why.
    pub(crate) fn skipped(reason: impl Into<String>) -> Self {
        Self {
            outcome: ItemOutcome::Skipped,
            exit_code: None,
            error: Some(reason.into()),
            output: String::new(),
            reboot_required: false,
        }
    }

    pub(crate) fn unchanged(note: impl Into<String>) -> Self {
        Self {
            outcome: ItemOutcome::Unchanged,
            exit_code: None,
            error: Some(note.into()),
            output: String::new(),
            reboot_required: false,
        }
    }

    /// How a program's run went: `judge` says what its exit code means.
    pub(crate) fn of(
        ran: anyhow::Result<process::Ran>,
        judge: impl Fn(&process::Ran) -> Verdict,
    ) -> Self {
        let ran = match ran {
            Ok(ran) => ran,
            Err(error) => return Self::failed(format!("{error:#}")),
        };
        let verdict = judge(&ran);
        let (outcome, reboot_required, error) = match verdict {
            Verdict::Done => (ItemOutcome::Succeeded, false, None),
            Verdict::DoneRestart => (
                ItemOutcome::Succeeded,
                true,
                Some("The computer must restart to finish.".to_owned()),
            ),
            Verdict::Unchanged(note) => (ItemOutcome::Unchanged, false, Some(note)),
            Verdict::Failed(reason) => (ItemOutcome::Failed, false, Some(reason)),
        };
        Self {
            outcome,
            exit_code: ran.code,
            error,
            output: ran.output(),
            reboot_required,
        }
    }
}

/// What a program's exit means.
pub(crate) enum Verdict {
    Done,
    /// Done, but it needs a restart to finish.
    DoneRestart,
    /// Nothing needed doing, and why. Only winget's codes say so.
    #[cfg_attr(not(windows), allow(dead_code))]
    Unchanged(String),
    Failed(String),
}

impl Verdict {
    /// The usual meaning: 0 worked, 3010 and 1641 worked but want a restart
    /// (Windows Installer's codes, which many installers share), anything
    /// else failed with what it wrote.
    pub(crate) fn by_code(ran: &process::Ran) -> Self {
        match ran.code {
            Some(0) => Self::Done,
            Some(3010 | 1641) => Self::DoneRestart,
            Some(1618) => Self::Failed(
                "Another installation is in progress; try again when it finishes".to_owned(),
            ),
            _ => Self::Failed(ran.complaint()),
        }
    }
}

/// Tells the server how far a run has got.
struct Progress {
    run_id: String,
    sender: mpsc::UnboundedSender<AgentStatusMessage>,
}

impl Progress {
    fn report(&self, message: impl Into<String>) {
        let message = meshrmm_protocol::clean_text(&message.into());
        tracing::info!(run_id = %self.run_id, message, "application run progress");
        let _ = self.sender.send(AgentStatusMessage::ApplicationProgress {
            run_id: self.run_id.clone(),
            message,
        });
    }
}

/// What the scans and runs going on now owe the server.
#[derive(Debug, Default)]
struct Waiting {
    /// A scan is going on, and these scans wait for it.
    scan: Option<Vec<String>>,
    /// The run going on, and the scans asked for during it.
    run: Option<(String, Vec<String>)>,
}

/// The coordinator's handle on application scans and runs. Created once per
/// Agent process, so results outlive reconnects.
pub struct Applications {
    sender: mpsc::UnboundedSender<AgentStatusMessage>,
    receiver: mpsc::UnboundedReceiver<AgentStatusMessage>,
    unsent: Option<AgentStatusMessage>,
    waiting: Arc<Mutex<Waiting>>,
    /// The server's scans and runs this Agent has yet to report: going on,
    /// or done with the report waiting to be sent. Each connection starts
    /// by telling the server (see [`Applications::activity`]); one the
    /// server waits for that isn't here won't be reported, because the
    /// Agent restarted or never got it.
    owed: Vec<String>,
}

impl Applications {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            unsent: None,
            waiting: Arc::default(),
            owed: Vec::new(),
        }
    }

    /// The first report on a connection: what the server still gets
    /// reports for.
    pub fn activity(&self) -> AgentStatusMessage {
        AgentStatusMessage::ApplicationActivity {
            run_ids: self.owed.clone(),
        }
    }

    /// `report` reached the connection: the server has what it answers.
    pub fn delivered(&mut self, report: &AgentStatusMessage) {
        match report {
            AgentStatusMessage::ApplicationScan { run_ids, .. } => {
                self.owed.retain(|id| !run_ids.contains(id));
            }
            AgentStatusMessage::ApplicationResult { run_id, .. } => {
                self.owed.retain(|id| id != run_id);
            }
            _ => {}
        }
    }

    /// The server asked for a scan, or for an update scan, which brings an
    /// application scan along.
    pub fn scan(&mut self, run_id: Option<String>) {
        self.owed.extend(run_id.clone());
        let mut waiting = lock(&self.waiting);
        if let Some((_, runs)) = &mut waiting.run {
            runs.extend(run_id);
            return;
        }
        if let Some(runs) = &mut waiting.scan {
            runs.extend(run_id);
            return;
        }
        waiting.scan = Some(run_id.into_iter().collect());
        drop(waiting);
        let sender = self.sender.clone();
        let shared = Arc::clone(&self.waiting);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-application-scan".into())
            .spawn(move || {
                let scan = {
                    let _one_at_a_time = lock(&OPERATION);
                    scan_now()
                };
                let run_ids = lock(&shared).scan.take().unwrap_or_default();
                let _ = sender.send(AgentStatusMessage::ApplicationScan {
                    run_ids,
                    scan: Box::new(scan),
                });
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for an application scan");
            let unanswered = lock(&self.waiting).scan.take().unwrap_or_default();
            self.owed.retain(|id| !unanswered.contains(id));
        }
    }

    /// The server asked for a run.
    pub fn run(&mut self, config: &Config, mode: ExecutionMode, request: ApplicationRunRequest) {
        self.owed.push(request.run_id.clone());
        let mut waiting = lock(&self.waiting);
        if let Some((running, _)) = &waiting.run {
            let result = ApplicationRunResult::failed(format!(
                "The device is already installing applications (run {running}). Try again when it finishes."
            ));
            let _ = self.sender.send(AgentStatusMessage::ApplicationResult {
                run_id: request.run_id,
                result: Box::new(result),
            });
            return;
        }
        waiting.run = Some((request.run_id.clone(), Vec::new()));
        drop(waiting);
        let sender = self.sender.clone();
        let shared = Arc::clone(&self.waiting);
        let run_id = request.run_id.clone();
        let config = config.clone();
        let spawned = std::thread::Builder::new()
            .name("meshrmm-application-run".into())
            .spawn(move || {
                let progress = Progress {
                    run_id: request.run_id.clone(),
                    sender: sender.clone(),
                };
                let (result, scan) = {
                    let _one_at_a_time = lock(&OPERATION);
                    run_now(&config, mode, &request, &progress)
                };
                tracing::info!(
                    run_id = %request.run_id,
                    status = result.status.as_str(),
                    items = result.items.len(),
                    failed = result.items.iter().filter(|item| item.outcome == ItemOutcome::Failed).count(),
                    error = ?result.error,
                    "application run finished"
                );
                let run_ids = lock(&shared)
                    .run
                    .take()
                    .map(|(_, runs)| runs)
                    .unwrap_or_default();
                let _ = sender.send(AgentStatusMessage::ApplicationResult {
                    run_id: request.run_id.clone(),
                    result: Box::new(result.sanitized()),
                });
                let _ = sender.send(AgentStatusMessage::ApplicationScan {
                    run_ids,
                    scan: Box::new(scan),
                });
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for an application run");
            lock(&self.waiting).run = None;
            let _ = self.sender.send(AgentStatusMessage::ApplicationResult {
                run_id,
                result: Box::new(ApplicationRunResult::failed(
                    "The Agent could not start the run.",
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
            .expect("the application channel stays open")
    }

    /// The connection couldn't send `report`; the next one sends it first.
    /// Progress is dropped, since a newer one or the result follows.
    pub fn unsent(&mut self, report: AgentStatusMessage) {
        if !matches!(report, AgentStatusMessage::ApplicationProgress { .. }) {
            self.unsent = Some(report);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Nothing panics while holding these locks, so a poisoned one is whole.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Scans every package manager the device has, logging how it went. Each
/// manager has [`APPLICATION_SCAN_TIMEOUT_SECONDS`] for all its commands,
/// and the whole scan [`APPLICATION_SCAN_TOTAL_SECONDS`], which the server's
/// report windows allow for.
fn scan_now() -> ApplicationScan {
    let started = Instant::now();
    let scan_deadline = started + Duration::from_secs(APPLICATION_SCAN_TOTAL_SECONDS);
    let mut managers = Vec::new();
    let mut packages = Vec::new();
    for tool in platform::tools() {
        let manager_deadline =
            Instant::now() + Duration::from_secs(APPLICATION_SCAN_TIMEOUT_SECONDS);
        let scanned = process::by(scan_deadline.min(manager_deadline), || {
            platform::scan(&tool)
        });
        match scanned {
            Ok((version, found)) => {
                managers.push(ManagerState {
                    source: tool.source(),
                    version,
                    error: None,
                });
                packages.extend(found);
            }
            Err(error) => {
                tracing::warn!(
                    source = tool.source().as_str(),
                    error = format!("{error:#}"),
                    "could not scan a package manager"
                );
                managers.push(ManagerState {
                    source: tool.source(),
                    version: None,
                    error: Some(format!("{error:#}")),
                });
            }
        }
    }
    let scan = ApplicationScan {
        scanned_at_unix_ms: crate::remote::updates::now_unix_ms(),
        managers,
        packages,
    }
    .sanitized();
    tracing::info!(
        managers = scan.managers.len(),
        packages = scan.packages.len(),
        outdated = scan
            .packages
            .iter()
            .filter(|package| package.outdated())
            .count(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "scanned for applications"
    );
    scan
}

/// The tool that handles `source`, if the device has it.
fn tool_for(tools: &[platform::Tool], source: PackageSource) -> Option<&platform::Tool> {
    tools.iter().find(|tool| tool.handles(source))
}

/// The package `key` names in `scan`, if it's installed.
fn installed<'a>(scan: &'a ApplicationScan, key: &PackageKey) -> Option<&'a ScannedPackage> {
    scan.packages
        .iter()
        .find(|package| key.is(package.source, &package.id))
}

/// Whether the manager of `source` reported a failed scan, so its packages
/// may be installed without the scan knowing.
fn scan_failed(scan: &ApplicationScan, source: PackageSource) -> bool {
    let manager = match source {
        PackageSource::HomebrewCask => PackageSource::Homebrew,
        source => source,
    };
    scan.managers
        .iter()
        .any(|state| state.source == manager && state.error.is_some())
}

/// Does what `request` asks, and returns how it went and the scan after it.
fn run_now(
    config: &Config,
    mode: ExecutionMode,
    request: &ApplicationRunRequest,
    progress: &Progress,
) -> (ApplicationRunResult, ApplicationScan) {
    progress.report("Looking for installed applications");
    let before = scan_now();
    let tools = platform::tools();
    let deadline = Instant::now() + Duration::from_secs(APPLICATION_RUN_TIMEOUT_SECONDS);
    // Every program an application runs stops by the deadline too, so the
    // last one can't run past it.
    let items = process::by(deadline, || {
        work(config, mode, request, progress, &tools, &before, deadline)
    });
    progress.report("Checking what is installed now");
    let after = scan_now();
    finish(items, after)
}

/// Works through a run's applications, until `deadline`.
fn work(
    config: &Config,
    mode: ExecutionMode,
    request: &ApplicationRunRequest,
    progress: &Progress,
    tools: &[platform::Tool],
    before: &ApplicationScan,
    deadline: Instant,
) -> Vec<ItemResult> {
    let mut items = Vec::new();
    match &request.operation {
        ApplicationOperation::Install { items: wanted } => {
            for (index, item) in wanted.iter().enumerate() {
                progress.report(format!(
                    "Installing {} of {}: {}",
                    index + 1,
                    wanted.len(),
                    item.name
                ));
                let result = if Instant::now() > deadline {
                    overdue(index, &item.name)
                } else {
                    install(config, mode, request, index, item, tools, before)
                };
                items.push(result);
            }
        }
        ApplicationOperation::Update { choice } => {
            let chosen = choose_updates(choice, before);
            for (index, (package, chosen_as)) in chosen.iter().enumerate() {
                let name = &package.name;
                let note = match chosen_as {
                    Chosen::Outdated => None,
                    Chosen::Current => Some(Attempt::unchanged("It is up to date.")),
                    Chosen::Pinned(version) => Some(Attempt::skipped(format!(
                        "It is pinned to {version} in the library."
                    ))),
                };
                if let Some(note) = note {
                    items.push(result(index, name, Some(package.key()), note));
                    continue;
                }
                progress.report(format!(
                    "Updating {} of {}: {name}",
                    index + 1,
                    chosen.len()
                ));
                let attempt = if Instant::now() > deadline {
                    Attempt::failed(TOO_LONG)
                } else {
                    match tool_for(tools, package.source) {
                        Some(tool) => platform::update(tool, package),
                        None => Attempt::failed(format!(
                            "{} is no longer installed",
                            package.source.label()
                        )),
                    }
                };
                items.push(result(index, name, Some(package.key()), attempt));
            }
        }
        ApplicationOperation::Uninstall { items: wanted } => {
            for (index, item) in wanted.iter().enumerate() {
                progress.report(format!(
                    "Uninstalling {} of {}: {}",
                    index + 1,
                    wanted.len(),
                    item.name()
                ));
                let result = if Instant::now() > deadline {
                    overdue(index, item.name())
                } else {
                    uninstall(index, item, tools, before)
                };
                items.push(result);
            }
        }
    }
    items
}

/// The run's result, with the version each application has after it.
fn finish(
    mut items: Vec<ItemResult>,
    after: ApplicationScan,
) -> (ApplicationRunResult, ApplicationScan) {
    for item in &mut items {
        if item.outcome != ItemOutcome::Succeeded {
            continue;
        }
        if let (Some(source), Some(id)) = (item.source, &item.package_id)
            && let Some(package) = installed(
                &after,
                &PackageKey {
                    source,
                    id: id.clone(),
                },
            )
        {
            item.version = package.version.clone();
        }
    }
    let status = RunStatus::of(&items);
    // A run that skipped everything says why, as its job shows.
    let error = (status == RunStatus::Skipped).then(|| skipped_reasons(&items));
    let result = ApplicationRunResult {
        status,
        error,
        items,
    };
    (result, after)
}

/// Why a run's applications were skipped, each reason once.
fn skipped_reasons(items: &[ItemResult]) -> String {
    let mut reasons = Vec::new();
    for item in items {
        let reason = item.error.as_deref().unwrap_or("It isn't for this device.");
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    reasons.join(" ")
}

const TOO_LONG: &str = "The run took too long and was stopped before this application.";

fn overdue(index: usize, name: &str) -> ItemResult {
    result(index, name, None, Attempt::failed(TOO_LONG))
}

fn result(index: usize, name: &str, package: Option<PackageKey>, attempt: Attempt) -> ItemResult {
    let (source, package_id) = match package {
        Some(key) => (Some(key.source), Some(key.id)),
        None => (None, None),
    };
    ItemResult {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        name: name.to_owned(),
        source,
        package_id,
        outcome: attempt.outcome,
        version: None,
        exit_code: attempt.exit_code,
        error: attempt.error,
        output: meshrmm_protocol::clean_output(&attempt.output),
        reboot_required: attempt.reboot_required,
    }
}

/// Why an update chose a package.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chosen {
    /// It is outdated, or named with a version the manager doesn't know.
    Outdated,
    /// Named, and up to date.
    Current,
    /// Named, but the library pins it to this version.
    Pinned(String),
}

/// The packages an update covers: every outdated one it covers, and the
/// ones it names that are installed, so asking for one that is up to date
/// or pinned says so. A named one whose installed version the manager
/// doesn't know (winget's "Unknown") isn't outdated, but is handed to the
/// manager, which knows whether it has a newer one. At most
/// [`MAX_RUN_ITEMS`]: the rest wait for the next update.
fn choose_updates(choice: &UpdateChoice, scan: &ApplicationScan) -> Vec<(ScannedPackage, Chosen)> {
    let mut chosen = scan
        .packages
        .iter()
        .filter(|package| package.outdated() && choice.covers(&package.key()))
        .map(|package| (package.clone(), Chosen::Outdated))
        .collect::<Vec<_>>();
    if choice.mode == UpdateMode::Only {
        for key in &choice.packages {
            let Some(package) = installed(scan, key) else {
                continue;
            };
            if let Some(version) = choice.pinned(key) {
                chosen.push((package.clone(), Chosen::Pinned(version.to_owned())));
            } else if !package.outdated() {
                let why = if package.version.is_none() {
                    Chosen::Outdated
                } else {
                    Chosen::Current
                };
                chosen.push((package.clone(), why));
            }
        }
    }
    if chosen.len() > MAX_RUN_ITEMS {
        tracing::info!(
            outdated = chosen.len(),
            "updating the first {MAX_RUN_ITEMS} outdated applications; the rest wait for the next update"
        );
        chosen.truncate(MAX_RUN_ITEMS);
    }
    chosen
}

/// Why a package from the server can't be handed to its manager, if it
/// can't. The server checks IDs and versions too; checking again keeps an
/// ID that reads as an option off a command line whatever the server sent.
fn refused(source: PackageSource, id: &str, version: Option<&str>) -> Option<Attempt> {
    if !source.valid_id(id) {
        return Some(Attempt::failed(format!(
            "\u{201c}{id}\u{201d} isn't a {} package ID.",
            source.label()
        )));
    }
    if version.is_some_and(|version| !meshrmm_protocol::valid_version(version)) {
        return Some(Attempt::failed("Its version isn't a version."));
    }
    None
}

/// The first of `item`'s sources the device can use: a custom installer,
/// or a package whose manager it has.
fn usable<'a>(item: &'a InstallItem, tools: &[platform::Tool]) -> Result<&'a PackageRef, String> {
    if let Some(source) = item.sources.iter().find(|source| {
        source.source == PackageSource::Custom || tool_for(tools, source.source).is_some()
    }) {
        return Ok(source);
    }
    let missing = item
        .sources
        .iter()
        .map(|source| source.source.label())
        .collect::<Vec<_>>();
    Err(match missing.as_slice() {
        [] => "It has no way to install on this operating system.".to_owned(),
        [one] => format!("{one} isn't installed on this device."),
        _ => format!(
            "None of {} is installed on this device.",
            missing.join(", ")
        ),
    })
}

fn install(
    config: &Config,
    mode: ExecutionMode,
    request: &ApplicationRunRequest,
    index: usize,
    item: &InstallItem,
    tools: &[platform::Tool],
    before: &ApplicationScan,
) -> ItemResult {
    let source = match usable(item, tools) {
        Ok(source) => source,
        // None of its ways to install is for this device: not a failure.
        Err(reason) => return result(index, &item.name, None, Attempt::skipped(reason)),
    };
    if let Some(custom) = &source.custom {
        let attempt = install_custom(config, mode, request, index, custom);
        return result(index, &item.name, None, attempt);
    }
    if let Some(attempt) = refused(source.source, &source.id, source.version.as_deref()) {
        return result(index, &item.name, None, attempt);
    }
    let key = PackageKey {
        source: source.source,
        id: source.id.clone(),
    };
    if let Some(package) = installed(before, &key) {
        let note = match &package.version {
            Some(version) => format!("It is installed already, version {version}."),
            None => "It is installed already.".to_owned(),
        };
        let mut item = result(index, &item.name, Some(key), Attempt::unchanged(note));
        item.version = package.version.clone();
        return item;
    }
    let Some(tool) = tool_for(tools, source.source) else {
        return result(
            index,
            &item.name,
            Some(key),
            Attempt::failed("Its package manager is gone."),
        );
    };
    if scan_failed(before, source.source) {
        tracing::info!(
            source = source.source.as_str(),
            "installing without knowing what is installed"
        );
    }
    result(
        index,
        &item.name,
        Some(key),
        platform::install(tool, source),
    )
}

fn install_custom(
    config: &Config,
    mode: ExecutionMode,
    request: &ApplicationRunRequest,
    index: usize,
    installer: &CustomInstaller,
) -> Attempt {
    if let Some(rule) = &installer.detection
        && detect(rule)
    {
        return Attempt::unchanged("It is installed already: its detection rule finds it.");
    }
    let staged = match download::fetch(config, mode, &request.run_id, index, installer) {
        Ok(staged) => staged,
        Err(error) => {
            return Attempt::failed(format!("Could not download the installer: {error:#}"));
        }
    };
    let mut attempt = platform::run_installer(staged.path(), &installer.arguments);
    drop(staged);
    if attempt.outcome == ItemOutcome::Succeeded
        && let Some(rule) = &installer.detection
        && !detect(rule)
    {
        attempt.outcome = ItemOutcome::Failed;
        attempt.error = Some(format!(
            "The installer finished ({}), but its detection rule doesn't find the application.",
            attempt
                .exit_code
                .map_or("no exit code".to_owned(), process::describe_code)
        ));
    }
    attempt
}

fn uninstall(
    index: usize,
    item: &UninstallItem,
    tools: &[platform::Tool],
    before: &ApplicationScan,
) -> ItemResult {
    match item {
        UninstallItem::Package { name, package } => {
            if let Some(attempt) = refused(package.source, &package.id, None) {
                return result(index, name, None, attempt);
            }
            // The ID as the device lists it: winget's --exact matches case.
            let listed = installed(before, package).map(ScannedPackage::key);
            let package = listed.as_ref().unwrap_or(package);
            if installed(before, package).is_none() && !scan_failed(before, package.source) {
                return result(
                    index,
                    name,
                    Some(package.clone()),
                    Attempt::unchanged("It isn't installed."),
                );
            }
            let attempt = match tool_for(tools, package.source) {
                Some(tool) => platform::uninstall(tool, package),
                None => Attempt::skipped(format!(
                    "{} isn't installed on this device.",
                    package.source.label()
                )),
            };
            result(index, name, Some(package.clone()), attempt)
        }
        UninstallItem::Inventory { name, version } => result(
            index,
            name,
            None,
            platform::uninstall_inventory(name, version.as_deref()),
        ),
        UninstallItem::Command {
            name,
            command,
            detection,
        } => {
            if let Some(rule) = detection
                && !detect(rule)
            {
                return result(
                    index,
                    name,
                    None,
                    Attempt::unchanged("It isn't installed: its detection rule doesn't find it."),
                );
            }
            let mut attempt = platform::run_command(command);
            if attempt.outcome == ItemOutcome::Succeeded
                && let Some(rule) = detection
                && detect(rule)
            {
                attempt.outcome = ItemOutcome::Failed;
                attempt.error = Some("The uninstall command finished, but the detection rule still finds the application.".to_owned());
            }
            result(index, name, None, attempt)
        }
    }
}

/// Whether a detection rule finds its application.
fn detect(rule: &DetectionRule) -> bool {
    match rule {
        DetectionRule::Application { name, min_version } => {
            let system = crate::remote::system_info::collect();
            system.applications.iter().any(|application| {
                meshrmm_protocol::name_matches(name, &application.name)
                    && min_version.as_deref().is_none_or(|minimum| {
                        application.version.as_deref().is_some_and(|version| {
                            meshrmm_protocol::compare_versions(version, minimum)
                                != std::cmp::Ordering::Less
                        })
                    })
            })
        }
        DetectionRule::File { path } => expand(path).exists(),
        DetectionRule::Registry { key, value } => platform::registry_exists(key, value.as_deref()),
    }
}

/// A path with Windows' `%VARIABLE%`s expanded, such as `%ProgramFiles%`.
/// Other platforms' paths are used as given.
fn expand(path: &str) -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from(path);
    }
    let mut expanded = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) if !name.is_empty() => expanded.push_str(&value),
                    _ => {
                        expanded.push('%');
                        expanded.push_str(name);
                        expanded.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                expanded.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    expanded.push_str(rest);
    PathBuf::from(expanded)
}

/// The installer types a platform runs, by extension.
pub(crate) fn extension(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_start_with_the_runs_still_owed() {
        let mut applications = Applications::new();
        assert_eq!(
            applications.activity(),
            AgentStatusMessage::ApplicationActivity { run_ids: vec![] },
            "a new Agent owes nothing"
        );
        applications.owed = vec!["run".into(), "scan-1".into(), "scan-2".into()];
        applications.delivered(&AgentStatusMessage::ApplicationProgress {
            run_id: "run".into(),
            message: "Installing".into(),
        });
        applications.delivered(&AgentStatusMessage::ApplicationResult {
            run_id: "run".into(),
            result: Box::new(ApplicationRunResult::failed("x")),
        });
        applications.delivered(&AgentStatusMessage::ApplicationScan {
            run_ids: vec!["scan-1".into()],
            scan: Box::default(),
        });
        assert_eq!(
            applications.activity(),
            AgentStatusMessage::ApplicationActivity {
                run_ids: vec!["scan-2".into()]
            }
        );
    }

    fn package(id: &str, version: &str, available: Option<&str>) -> ScannedPackage {
        ScannedPackage {
            source: PackageSource::Winget,
            id: id.into(),
            name: id.to_uppercase(),
            version: Some(version.into()),
            available_version: available.map(str::to_owned),
        }
    }

    fn key(id: &str) -> PackageKey {
        PackageKey {
            source: PackageSource::Winget,
            id: id.into(),
        }
    }

    #[test]
    fn updates_choose_outdated_packages_and_say_when_named_ones_are_current() {
        let scan = ApplicationScan {
            packages: vec![
                package("a", "1", Some("2")),
                package("b", "1", None),
                package("c", "1", Some("3")),
            ],
            ..Default::default()
        };
        let ids = |choice: UpdateChoice| {
            choose_updates(&choice, &scan)
                .into_iter()
                .map(|(package, chosen)| (package.id, chosen))
                .collect::<Vec<_>>()
        };
        let pinned = |id: &str| meshrmm_protocol::Held {
            package: key(id),
            version: "2.10".into(),
        };
        assert_eq!(
            ids(UpdateChoice::default()),
            [
                ("a".into(), Chosen::Outdated),
                ("c".into(), Chosen::Outdated)
            ]
        );
        assert_eq!(
            ids(UpdateChoice {
                mode: UpdateMode::Only,
                packages: vec![key("b"), key("c"), key("missing")],
                held: vec![],
            }),
            [
                ("c".into(), Chosen::Outdated),
                ("b".into(), Chosen::Current)
            ]
        );
        assert_eq!(
            ids(UpdateChoice {
                mode: UpdateMode::Except,
                packages: vec![key("a")],
                held: vec![pinned("c")],
            }),
            Vec::<(String, Chosen)>::new()
        );
        // A pinned package asked for by name says so.
        assert_eq!(
            ids(UpdateChoice {
                mode: UpdateMode::Only,
                packages: vec![key("a"), key("c")],
                held: vec![pinned("c")],
            }),
            [
                ("a".into(), Chosen::Outdated),
                ("c".into(), Chosen::Pinned("2.10".into()))
            ]
        );
    }

    #[test]
    fn runs_skip_what_isnt_for_the_device_and_say_why() {
        let skipped = |reason: &str| result(0, "Hello", None, Attempt::skipped(reason));
        let (run, _) = finish(
            vec![
                skipped("Snap isn't installed on this device."),
                skipped("Snap isn't installed on this device."),
            ],
            ApplicationScan::default(),
        );
        assert_eq!(run.status, RunStatus::Skipped);
        assert_eq!(
            run.error.as_deref(),
            Some("Snap isn't installed on this device."),
            "each reason once"
        );
        let (run, _) = finish(
            vec![
                skipped("It is pinned to 2.10 in the library."),
                result(1, "Git", None, Attempt::unchanged("It is up to date.")),
            ],
            ApplicationScan::default(),
        );
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.error, None);
        assert_eq!(run.items[0].outcome, ItemOutcome::Skipped);
    }

    #[test]
    fn updates_leave_unknown_versions_alone_unless_named() {
        // As winget's list reads Roblox: installed version Unknown.
        let unknown = ScannedPackage {
            version: None,
            ..package("Roblox.Roblox", "1", None)
        };
        let scan = ApplicationScan {
            packages: vec![unknown, package("a", "1", Some("2"))],
            ..Default::default()
        };
        let chosen = choose_updates(&UpdateChoice::default(), &scan);
        assert_eq!(chosen.len(), 1, "update all: {chosen:?}");
        let chosen = choose_updates(
            &UpdateChoice {
                mode: UpdateMode::Only,
                packages: vec![key("roblox.roblox")],
                held: vec![],
            },
            &scan,
        );
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].0.id, "Roblox.Roblox", "as the device lists it");
        assert_eq!(chosen[0].1, Chosen::Outdated, "handed to winget");
    }

    #[test]
    fn updates_do_at_most_a_run_of_applications() {
        let scan = ApplicationScan {
            packages: (0..MAX_RUN_ITEMS + 10)
                .map(|n| package(&format!("app.n{n}"), "1", Some("2")))
                .collect(),
            ..Default::default()
        };
        let chosen = choose_updates(&UpdateChoice::default(), &scan);
        assert_eq!(chosen.len(), MAX_RUN_ITEMS);
        assert_eq!(chosen[0].0.id, "app.n0");
    }

    #[test]
    fn verdicts_follow_windows_installer_codes() {
        let ran = |code| process::Ran {
            code: Some(code),
            stdout: "line one\nline two\n".into(),
            stderr: String::new(),
        };
        let attempt = Attempt::of(Ok(ran(3010)), Verdict::by_code);
        assert_eq!(attempt.outcome, ItemOutcome::Succeeded);
        assert!(attempt.reboot_required);
        let attempt = Attempt::of(Ok(ran(1603)), Verdict::by_code);
        assert_eq!(attempt.outcome, ItemOutcome::Failed);
        assert_eq!(attempt.error.as_deref(), Some("line one line two"));
        assert_eq!(attempt.exit_code, Some(1603));
        assert_eq!(
            Attempt::of(Err(anyhow::anyhow!("could not run x")), Verdict::by_code)
                .error
                .as_deref(),
            Some("could not run x")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_expand_their_variables() {
        let windows = std::env::var("SystemRoot").unwrap();
        assert_eq!(
            expand(r"%SystemRoot%\notepad.exe"),
            PathBuf::from(format!(r"{windows}\notepad.exe"))
        );
        assert_eq!(
            expand("%NO_SUCH_VARIABLE_HERE%\\x"),
            PathBuf::from("%NO_SUCH_VARIABLE_HERE%\\x")
        );
        assert_eq!(expand("100%"), PathBuf::from("100%"));
    }

    #[test]
    fn installers_are_told_apart_by_extension() {
        assert_eq!(extension(Path::new("Setup.MSI")), "msi");
        assert_eq!(extension(Path::new("tool")), "");
    }

    #[test]
    fn option_like_packages_never_reach_a_manager() {
        assert!(refused(PackageSource::Winget, "Mozilla.Firefox", Some("128.0")).is_none());
        for (source, id) in [
            (PackageSource::Winget, "--override"),
            (PackageSource::Chocolatey, "-y"),
            (PackageSource::Homebrew, "../evil"),
            (PackageSource::Snap, "code --dangerous"),
        ] {
            let attempt = refused(source, id, None).expect(id);
            assert_eq!(attempt.outcome, ItemOutcome::Failed);
        }
        assert!(refused(PackageSource::Chocolatey, "git", Some("--pre")).is_some());
    }
}
