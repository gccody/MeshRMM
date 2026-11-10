#[path = "service_tray.rs"]
mod trays;

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
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
use crate::remote::service_link::{BUSY, IDLE, UPDATING_PREFIX};
use crate::update_policy::{UpdateSchedule, random_bits, unix_seconds};
use crate::win32::{OwnedHandle, wide};

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

/// Whether `name` is the Agent's Windows service, under its current or legacy name.
pub fn is_agent_service(name: &str) -> bool {
    [SERVICE_NAME, LEGACY_SERVICE_NAME]
        .iter()
        .any(|service| name.eq_ignore_ascii_case(service))
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
    crate::logging::flush();
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
    crate::power::finish_safe_mode_restart();

    let mut worker: Option<Coordinator> = None;
    let mut trays = trays::Trays::default();
    let config_directory = config.config_path.parent();
    let mut updates = UpdateSchedule::new(
        Instant::now(),
        unix_seconds(),
        config_directory
            .and_then(crate::updater::scheduled_check)
            .as_ref(),
        random_bits(),
    );
    // A release a check found, waiting for the coordinator to be free to stop.
    let mut staged: Option<crate::updater::StagedUpdate> = None;
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

        let busy = worker.as_ref().is_some_and(Coordinator::busy);
        if staged.is_none() && updates.due(Instant::now(), busy) {
            match crate::updater::check_and_stage(config) {
                Ok(Some(update)) => staged = Some(update),
                Ok(None) => {
                    let next = updates.checked(Instant::now(), unix_seconds(), true, random_bits());
                    if let (Some(directory), Some(next)) = (config_directory, next) {
                        crate::updater::record_next_check(directory, &next);
                    }
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "automatic Agent update check failed");
                    updates.checked(Instant::now(), unix_seconds(), false, random_bits());
                }
            }
        }
        // Asked again, since the download may have taken minutes.
        let busy = worker.as_ref().is_some_and(Coordinator::busy);
        if let Some(update) = staged.take_if(|_| updates.install_now(Instant::now(), busy)) {
            match update.start() {
                Ok(version) => {
                    // Stop cleanly: the helper restarts the service on every path, while a
                    // failure exit would let SCM recovery start this executable again while
                    // the helper is replacing it.
                    tracing::info!("staged an Agent update; stopping the service for replacement");
                    if let Some(process) = worker.as_mut() {
                        process.announce_update(&version);
                    }
                    break true;
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "could not start the Agent update");
                    updates.checked(Instant::now(), unix_seconds(), false, random_bits());
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
    tracing::info!(graceful, "the Agent service stopped");
    // Windows may end the process once it reports Stopped.
    crate::logging::flush();
    status.set_service_status(service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;
    Ok(())
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

/// The persistent coordinator and the pipes it reports on. Closing its stdin asks it to stop;
/// its stdout reports whether it is busy with a remote session or a job an update must not cut
/// off.
struct Coordinator {
    process: WorkerProcess,
    control: Option<ChildStdin>,
    busy: Arc<AtomicBool>,
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
            busy: Arc::default(),
        };
        if let Some(output) = output {
            follow_activity(output, Arc::clone(&coordinator.busy));
        }
        Ok(coordinator)
    }

    fn busy(&self) -> bool {
        self.process.is_running() && self.busy.load(Ordering::Relaxed)
    }

    /// Tells the coordinator the stop that follows installs `version`, which it reports to the
    /// server before disconnecting.
    fn announce_update(&mut self, version: &str) {
        let Some(control) = self.control.as_mut() else {
            return;
        };
        if let Err(error) =
            writeln!(control, "{UPDATING_PREFIX}{version}").and_then(|()| control.flush())
        {
            tracing::warn!(%error, "could not tell the Agent coordinator about the update");
        }
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

fn follow_activity(output: ChildStdout, busy: Arc<AtomicBool>) {
    let reader = std::thread::Builder::new()
        .name("meshrmm-coordinator-activity".into())
        .spawn(move || {
            for line in BufReader::new(output).split(b'\n') {
                let Ok(line) = line else {
                    break;
                };
                if let Some(reported) = activity(&line)
                    && busy.swap(reported, Ordering::Relaxed) != reported
                {
                    tracing::info!(
                        busy = reported,
                        "the Agent coordinator reported whether an update would interrupt it"
                    );
                }
            }
            busy.store(false, Ordering::Relaxed);
        });
    if let Err(error) = reader {
        tracing::warn!(%error, "could not follow the coordinator's activity; updates will not wait for sessions or jobs");
    }
}

fn activity(line: &[u8]) -> Option<bool> {
    match line.trim_ascii() {
        line if line == BUSY.as_bytes() => Some(true),
        line if line == IDLE.as_bytes() => Some(false),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_activity_reports() {
        assert_eq!(activity(b"busy"), Some(true));
        assert_eq!(activity(b"idle\r"), Some(false));
        assert_eq!(activity(b""), None);
        assert_eq!(activity(b"busy-ish"), None);
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
