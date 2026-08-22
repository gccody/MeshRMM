use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::remote::config::Config;

pub const SERVICE_NAME: &str = "PulseRMMAgent";
const NO_ACTIVE_SESSION: u32 = u32::MAX;
static SERVICE_CONFIG: OnceLock<Config> = OnceLock::new();

pub fn run(config: Config) -> anyhow::Result<()> {
    SERVICE_CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("the Agent service configuration was already initialized"))?;
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("failed to connect the Agent to the Windows Service Control Manager")
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        tracing::error!(error = ?error, "PulseRMM Agent service stopped with an error");
    }
}

enum Control {
    Stop,
    DesktopChanged,
}

fn run_service() -> anyhow::Result<()> {
    let config = SERVICE_CONFIG
        .get()
        .context("the Agent service configuration was not initialized")?;
    let (control_tx, control_rx) = mpsc::channel();
    let event_handler = move |event| -> ServiceControlHandlerResult {
        match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = control_tx.send(Control::Stop);
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
    let status = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("failed to register the Agent service control handler")?;
    status.set_service_status(service_status(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
    ))?;

    let mut worker: Option<WorkerProcess> = None;
    let mut next_update_check = Instant::now();
    let result = loop {
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        let needs_worker = session_id != NO_ACTIVE_SESSION
            && worker
                .as_ref()
                .is_none_or(|process| process.session_id != session_id || !process.is_running());
        if needs_worker {
            if let Some(mut process) = worker.take() {
                process.stop();
            }
            match WorkerProcess::launch(session_id, &config.config_path) {
                Ok(process) => {
                    tracing::info!(
                        session_id,
                        "started SYSTEM Agent worker in active desktop session"
                    );
                    worker = Some(process);
                }
                Err(error) => {
                    tracing::warn!(session_id, error = ?error, "could not start Agent worker in active desktop session");
                }
            }
        } else if session_id == NO_ACTIVE_SESSION
            && let Some(mut process) = worker.take()
        {
            process.stop();
        }

        if Instant::now() >= next_update_check {
            next_update_check = Instant::now() + Duration::from_secs(6 * 60 * 60);
            match crate::updater::check_and_schedule(config) {
                Ok(true) => {
                    tracing::info!("staged an Agent update; stopping the service for replacement");
                    break Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(error = ?error, "automatic Agent update check failed");
                }
            }
        }

        match control_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
            Ok(Control::DesktopChanged) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };

    status.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;
    if let Some(mut process) = worker {
        process.stop();
    }
    status.set_service_status(service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    ))?;
    result
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

struct WorkerProcess {
    process: OwnedHandle,
    session_id: u32,
}

impl WorkerProcess {
    fn launch(session_id: u32, config_path: &Path) -> anyhow::Result<Self> {
        let executable =
            std::env::current_exe().context("could not locate the Agent executable")?;
        let working_directory = executable
            .parent()
            .context("Agent executable has no parent directory")?;

        let mut process_token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut process_token) }
            .context("failed to open the LocalSystem service token")?;
        let process_token = OwnedHandle(process_token);

        let mut session_token = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                process_token.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut session_token,
            )
        }
        .context("failed to duplicate the LocalSystem service token")?;
        let session_token = OwnedHandle(session_token);
        unsafe {
            SetTokenInformation(
                session_token.0,
                TokenSessionId,
                (&session_id as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        }
        .context("failed to move the LocalSystem worker token into the active session")?;

        let executable_wide = wide(executable.as_os_str());
        let working_directory_wide = wide(working_directory.as_os_str());
        let mut command_line = wide(OsStr::new(&format!(
            "\"{}\" --worker --config \"{}\"",
            executable.display(),
            config_path.display()
        )));
        let mut desktop = wide(OsStr::new("winsta0\\default"));
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process_info = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(session_token.0),
                PCWSTR(executable_wide.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR(working_directory_wide.as_ptr()),
                &startup,
                &mut process_info,
            )
        }
        .context("failed to launch the SYSTEM Agent worker on winsta0\\default")?;
        let _thread = OwnedHandle(process_info.hThread);
        Ok(Self {
            process: OwnedHandle(process_info.hProcess),
            session_id,
        })
    }

    fn is_running(&self) -> bool {
        (unsafe { WaitForSingleObject(self.process.0, 0) }) == WAIT_TIMEOUT
    }

    fn stop(&mut self) {
        if self.is_running() {
            if let Err(error) = unsafe { TerminateProcess(self.process.0, 0) } {
                tracing::warn!(error = %error, "failed to stop the Agent desktop worker");
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
