#[path = "service_tray.rs"]
mod trays;

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::remote::config::Config;
use crate::remote::service_link::{SESSION_ACTIVE, SESSION_IDLE};

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// An update check that falls due during a remote session is retried this often until the
/// session ends, so the update does not interrupt it.
const DEFERRED_UPDATE_RETRY: Duration = Duration::from_secs(5 * 60);
/// A session that never ends does not keep the Agent from updating for longer than this.
const MAX_UPDATE_DEFERRAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Covers the coordinator's session close helpers, which may each take 10 seconds.
const COORDINATOR_STOP_TIMEOUT: Duration = Duration::from_secs(25);

pub const SERVICE_NAME: &str = "MeshRMMAgent";
pub const LEGACY_SERVICE_NAME: &str = "PulseRMMAgent";
static SERVICE_CONFIG: OnceLock<Config> = OnceLock::new();

pub fn run(config: Config) -> anyhow::Result<()> {
    SERVICE_CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("the Agent service configuration was already initialized"))?;
    service_dispatcher::start(active_service_name(), ffi_service_main)
        .context("failed to connect the Agent to the Windows Service Control Manager")
}

pub fn active_service_name() -> &'static str {
    std::env::current_exe()
        .ok()
        .as_deref()
        .map(service_name_for_path)
        .unwrap_or(SERVICE_NAME)
}

pub fn service_name_for_path(path: &Path) -> &'static str {
    if path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("PulseRMM")
    }) {
        LEGACY_SERVICE_NAME
    } else {
        SERVICE_NAME
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        tracing::error!(error = ?error, "MeshRMM Agent service stopped with an error");
    }
}

enum Control {
    Stop,
    Shutdown,
    DesktopChanged,
}

fn run_service() -> anyhow::Result<()> {
    let config = SERVICE_CONFIG
        .get()
        .context("the Agent service configuration was not initialized")?;
    let (control_tx, control_rx) = mpsc::channel();
    let event_handler = move |event| -> ServiceControlHandlerResult {
        match event {
            ServiceControl::Stop => {
                let _ = control_tx.send(Control::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Shutdown => {
                let _ = control_tx.send(Control::Shutdown);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(_) => {
                let _ = control_tx.send(Control::DesktopChanged);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status = service_control_handler::register(active_service_name(), event_handler)
        .context("failed to register the Agent service control handler")?;
    status.set_service_status(service_status(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
    ))?;

    if let Some(config_directory) = config.config_path.parent() {
        crate::updater::remove_stale_files(config_directory);
    }

    let mut worker: Option<Coordinator> = None;
    let mut trays = trays::Trays::default();
    let mut updates = UpdateSchedule::new(Instant::now());
    // Whether the coordinator gets to end its remote sessions and run their close actions.
    let graceful = loop {
        // The authenticated coordinator deliberately remains in the service's
        // non-interactive Session 0. Desktop-bound helpers are launched by the
        // worker into the active console session without receiving Agent
        // credentials. Keeping this process stable preserves signaling and an
        // active WebRTC connection across lock, logoff, and user switching.
        let needs_worker = worker
            .as_ref()
            .is_none_or(|process| !process.process.is_running());
        if needs_worker {
            if let Some(mut process) = worker.take() {
                process.process.stop();
            }
            match Coordinator::launch(&config.config_path) {
                Ok(process) => {
                    tracing::info!("started persistent SYSTEM Agent coordinator in Session 0");
                    worker = Some(process);
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "could not start persistent Agent coordinator");
                }
            }
        }

        if let Err(error) = trays.refresh() {
            tracing::warn!(?error, "could not refresh Agent tray sessions");
        }

        let session_active = worker.as_ref().is_some_and(Coordinator::session_active);
        if updates.due(Instant::now(), session_active) {
            match crate::updater::check_and_schedule(config) {
                Ok(true) => {
                    // Stop cleanly: the helper restarts the service on every path, while a
                    // failure exit would let SCM recovery start this executable again while
                    // the helper is replacing it.
                    tracing::info!("staged an Agent update; stopping the service for replacement");
                    break true;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(error = ?error, "automatic Agent update check failed");
                }
            }
        }

        match control_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            // Signed-in sessions end with Windows, and shutdown leaves little time.
            Ok(Control::Shutdown) => break false,
            Ok(Control::DesktopChanged) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };

    let mut checkpoint = 1;
    let stop_pending = |checkpoint| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    };
    status.set_service_status(stop_pending(checkpoint))?;
    if graceful && let Some(process) = worker.as_mut() {
        process.request_stop();
    }
    drop(trays);
    if let Some(process) = worker {
        if graceful {
            process.wait_for_exit(COORDINATOR_STOP_TIMEOUT, || {
                checkpoint += 1;
                let _ = status.set_service_status(stop_pending(checkpoint));
            });
        } else {
            drop(process);
        }
    }
    status.set_service_status(service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;
    Ok(())
}

/// Schedules automatic update checks and postpones them while a remote session is live.
struct UpdateSchedule {
    next_check: Instant,
    deferred_since: Option<Instant>,
}

impl UpdateSchedule {
    fn new(now: Instant) -> Self {
        Self {
            next_check: now,
            deferred_since: None,
        }
    }

    /// Whether to check for an update now. Staging an update stops the service and ends the
    /// session, so a check waits for the session to end, but not beyond the maximum deferral.
    fn due(&mut self, now: Instant, session_active: bool) -> bool {
        if now < self.next_check {
            return false;
        }
        if session_active {
            let since = *self.deferred_since.get_or_insert(now);
            let deferred = now.duration_since(since);
            if deferred < MAX_UPDATE_DEFERRAL {
                if deferred.is_zero() {
                    tracing::info!(
                        "postponing the automatic Agent update check until the remote session ends"
                    );
                }
                self.next_check = now + DEFERRED_UPDATE_RETRY;
                return false;
            }
            tracing::warn!(
                deferred_hours = deferred.as_secs() / 3600,
                "checking for an Agent update although a remote session is still active"
            );
        } else if self.deferred_since.is_some() {
            tracing::info!("the remote session ended; running the postponed Agent update check");
        }
        self.deferred_since = None;
        self.next_check = now + UPDATE_CHECK_INTERVAL;
        true
    }
}

fn service_status(state: ServiceState, accepted: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// The persistent coordinator and the pipes it reports on. Closing its stdin asks it to stop;
/// its stdout reports whether a remote session is live.
struct Coordinator {
    process: WorkerProcess,
    control: Option<ChildStdin>,
    session_active: Arc<AtomicBool>,
}

impl Coordinator {
    fn launch(config_path: &Path) -> anyhow::Result<Self> {
        let executable =
            std::env::current_exe().context("could not locate the Agent executable")?;
        let working_directory = executable
            .parent()
            .context("Agent executable has no parent directory")?;
        let mut child = Command::new(&executable)
            .arg("--worker")
            .arg("--config")
            .arg(config_path)
            .current_dir(working_directory)
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to launch the persistent SYSTEM Agent coordinator")?;
        let control = child.stdin.take();
        let output = child.stdout.take();
        let coordinator = Self {
            process: WorkerProcess {
                process: OwnedHandle(HANDLE(child.into_raw_handle())),
            },
            control,
            session_active: Arc::default(),
        };
        if let Some(output) = output {
            follow_session_activity(output, Arc::clone(&coordinator.session_active));
        }
        Ok(coordinator)
    }

    fn session_active(&self) -> bool {
        self.process.is_running() && self.session_active.load(Ordering::Relaxed)
    }

    /// Asks the coordinator to end its remote sessions, run their close actions, and exit.
    fn request_stop(&mut self) {
        self.control = None;
    }

    /// Waits for a coordinator asked to stop, reporting progress about once a second, and
    /// terminates it if it does not exit in time.
    fn wait_for_exit(mut self, timeout: Duration, mut progress: impl FnMut()) {
        let deadline = Instant::now() + timeout;
        while self.process.is_running() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    timeout_seconds = timeout.as_secs(),
                    "the Agent coordinator did not stop in time; terminating it"
                );
                break;
            }
            let _ = unsafe { WaitForSingleObject(self.process.process.0, 1_000) };
            progress();
        }
        if !self.process.is_running() {
            tracing::info!("the Agent coordinator stopped");
        }
        self.process.stop();
    }
}

fn follow_session_activity(output: ChildStdout, active: Arc<AtomicBool>) {
    let reader = std::thread::Builder::new()
        .name("meshrmm-coordinator-activity".into())
        .spawn(move || {
            for line in BufReader::new(output).split(b'\n') {
                let Ok(line) = line else {
                    break;
                };
                if let Some(session_active) = session_activity(&line)
                    && active.swap(session_active, Ordering::Relaxed) != session_active
                {
                    tracing::info!(
                        session_active,
                        "the Agent coordinator reported remote session activity"
                    );
                }
            }
            active.store(false, Ordering::Relaxed);
        });
    if let Err(error) = reader {
        tracing::warn!(%error, "could not follow remote session activity; updates will not wait for sessions");
    }
}

fn session_activity(line: &[u8]) -> Option<bool> {
    match line.trim_ascii() {
        line if line == SESSION_ACTIVE.as_bytes() => Some(true),
        line if line == SESSION_IDLE.as_bytes() => Some(false),
        _ => None,
    }
}

struct WorkerProcess {
    process: OwnedHandle,
}

impl WorkerProcess {
    fn is_running(&self) -> bool {
        (unsafe { WaitForSingleObject(self.process.0, 0) }) == WAIT_TIMEOUT
    }

    fn stop(&mut self) {
        if self.is_running() {
            if let Err(error) = unsafe { TerminateProcess(self.process.0, 0) } {
                tracing::warn!(error = %error, "failed to stop the Agent coordinator");
            } else {
                let _ = unsafe { WaitForSingleObject(self.process.0, 5_000) };
            }
        }
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_session_activity_reports() {
        assert_eq!(session_activity(b"session-active"), Some(true));
        assert_eq!(session_activity(b"session-idle\r"), Some(false));
        assert_eq!(session_activity(b""), None);
        assert_eq!(session_activity(b"session-active-ish"), None);
    }

    #[test]
    fn checks_for_updates_on_schedule_without_a_session() {
        let start = Instant::now();
        let mut updates = UpdateSchedule::new(start);
        assert!(updates.due(start, false));
        assert!(!updates.due(start + UPDATE_CHECK_INTERVAL / 2, false));
        assert!(updates.due(start + UPDATE_CHECK_INTERVAL, false));
    }

    #[test]
    fn postpones_update_checks_during_a_session_up_to_the_limit() {
        let start = Instant::now();
        let mut updates = UpdateSchedule::new(start);
        assert!(!updates.due(start, true));
        assert!(!updates.due(start + DEFERRED_UPDATE_RETRY / 2, false));
        assert!(!updates.due(start + DEFERRED_UPDATE_RETRY, true));
        // The check runs soon after the session ends, not a full interval later.
        assert!(updates.due(start + DEFERRED_UPDATE_RETRY * 2, false));

        let mut updates = UpdateSchedule::new(start);
        let mut now = start;
        while !updates.due(now, true) {
            now += DEFERRED_UPDATE_RETRY;
            assert!(now <= start + MAX_UPDATE_DEFERRAL);
        }
        assert_eq!(now, start + MAX_UPDATE_DEFERRAL);
        // A later session gets the full deferral again.
        assert!(!updates.due(now + UPDATE_CHECK_INTERVAL, true));
    }

    #[test]
    fn preserves_the_legacy_service_for_in_place_updates() {
        assert_eq!(
            service_name_for_path(Path::new(
                r"C:\Program Files\PulseRMM\Agent\pulsermm-agent.exe"
            )),
            LEGACY_SERVICE_NAME
        );
        assert_eq!(
            service_name_for_path(Path::new(
                r"C:\Program Files\MeshRMM\Agent\meshrmm-agent.exe"
            )),
            SERVICE_NAME
        );
    }
}
