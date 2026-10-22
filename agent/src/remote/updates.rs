//! Operating system updates: scans for pending updates and installs them
//! when the server asks, then reports on the control connection. See
//! docs/patch-management.md.
//!
//! The coordinator runs them as root or SYSTEM, like the system information
//! collection. Each scan or install runs on its own thread, one at a time,
//! so the control connection never waits for a package manager. A scan asked
//! for while one runs is answered by the running one; one asked for during an
//! install is answered by the scan that follows every install. Results wait
//! in a queue that outlives reconnects, so an install that finishes while the
//! Agent is offline is still reported.
//!
//! An install scans first, picks the chosen updates from what is pending
//! then, installs them, scans again and, as the request asks, sets the
//! computer to restart after a delay that warns its user.
use std::sync::Mutex;
use std::time::Duration;

use meshrmm_protocol::{
    AgentStatusMessage, PendingUpdate, RebootBehavior, UpdateInstallItem, UpdateInstallRequest,
    UpdateInstallResult, UpdateInstallStatus, UpdateItemOutcome, UpdateManager, UpdateScan,
};
use tokio::sync::mpsc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod process;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

/// One scan or install at a time: package managers lock their databases,
/// and Windows Update runs one installer at a time.
static OPERATION: Mutex<()> = Mutex::new(());

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
pub(crate) struct Progress {
    run_id: String,
    sender: mpsc::UnboundedSender<AgentStatusMessage>,
}

impl Progress {
    pub(crate) fn report(&self, message: impl Into<String>, percent: Option<u8>) {
        let message = meshrmm_protocol::clean_text(&message.into());
        tracing::info!(run_id = %self.run_id, message, ?percent, "update install progress");
        let _ = self.sender.send(AgentStatusMessage::UpdateProgress {
            run_id: self.run_id.clone(),
            message,
            percent: percent.map(|percent| percent.min(100)),
        });
    }
}

/// The coordinator's handle on update scans and installs. Created once per
/// Agent process, so results outlive reconnects.
pub struct Updates {
    sender: mpsc::UnboundedSender<AgentStatusMessage>,
    receiver: mpsc::UnboundedReceiver<AgentStatusMessage>,
    /// A report the connection failed to send, sent first on the next one.
    unsent: Option<AgentStatusMessage>,
    waiting: std::sync::Arc<Mutex<Waiting>>,
}

impl Updates {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            unsent: None,
            waiting: std::sync::Arc::default(),
        }
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, Waiting> {
        lock(&self.waiting)
    }

    /// The server asked for a scan.
    pub fn scan(&mut self, run_id: Option<String>) {
        let mut waiting = self.waiting();
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
        let sender = self.sender.clone();
        let shared = std::sync::Arc::clone(&self.waiting);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-update-scan".into())
            .spawn(move || {
                let scan = {
                    let _one_at_a_time = lock(&OPERATION);
                    scan_now()
                };
                let run_ids = lock(&shared).scan.take().unwrap_or_default();
                let _ = sender.send(AgentStatusMessage::UpdateScan {
                    run_ids,
                    scan: Box::new(scan),
                });
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for an update scan");
            self.waiting().scan = None;
        }
    }

    /// The server asked for an install.
    pub fn install(&mut self, request: UpdateInstallRequest) {
        let mut waiting = self.waiting();
        if let Some((running, _)) = &waiting.install {
            let result = UpdateInstallResult::failed(format!(
                "The device is already installing updates (run {running}). Try again when it finishes."
            ));
            let _ = self.sender.send(AgentStatusMessage::UpdateInstall {
                run_id: request.run_id,
                result: Box::new(result),
            });
            return;
        }
        waiting.install = Some((request.run_id.clone(), Vec::new()));
        drop(waiting);
        let sender = self.sender.clone();
        let shared = std::sync::Arc::clone(&self.waiting);
        let run_id = request.run_id.clone();
        let spawned = std::thread::Builder::new()
            .name("meshrmm-update-install".into())
            .spawn(move || {
                let progress = Progress {
                    run_id: request.run_id.clone(),
                    sender: sender.clone(),
                };
                let (result, scan) = {
                    let _one_at_a_time = lock(&OPERATION);
                    install_now(&request, &progress)
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
                let run_ids = lock(&shared)
                    .install
                    .take()
                    .map(|(_, runs)| runs)
                    .unwrap_or_default();
                let _ = sender.send(AgentStatusMessage::UpdateInstall {
                    run_id: request.run_id.clone(),
                    result: Box::new(result.sanitized()),
                });
                if let Some(scan) = scan {
                    let _ = sender.send(AgentStatusMessage::UpdateScan {
                        run_ids,
                        scan: Box::new(scan),
                    });
                }
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for an update install");
            self.waiting().install = None;
            let _ = self.sender.send(AgentStatusMessage::UpdateInstall {
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
