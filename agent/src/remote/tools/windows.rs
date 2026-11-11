//! The remote tools on Windows: processes from a Toolhelp snapshot and each
//! process's own handle, services from the Service Control Manager.
use std::collections::HashMap;
use std::time::Duration;

use meshrmm_protocol::{
    ProcessInfo, ServiceAction, ServiceInfo, ServiceStartup, ServiceState, ToolError, ToolErrorCode,
};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, FILETIME, HANDLE,
    UNICODE_STRING, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    ENUM_SERVICE_STATUS_PROCESSW, ENUM_SERVICE_TYPE, EnumServicesStatusExW, OpenSCManagerW,
    OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW,
    QueryServiceStatusEx, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SC_STATUS_PROCESS_INFO, SERVICE_AUTO_START,
    SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DEMAND_START,
    SERVICE_DESCRIPTIONW, SERVICE_DISABLED, SERVICE_ERROR, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_START_TYPE, SERVICE_STATE_ALL, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_WIN32,
    StartServiceW,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetProcessTimes, IsProcessCritical, OpenProcess,
    OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{HRESULT, PCWSTR, PWSTR};

use super::{Deadline, Reading, Snapshot, failed, out_of_time, own_service};
use crate::win32::{OwnedHandle, wide};

/// How long an ended process gets to go before it counts as ending anyway.
const EXIT_WAIT: Duration = Duration::from_secs(5);
/// How often a service starting or stopping is looked at.
const SERVICE_POLL: Duration = Duration::from_millis(250);
/// The longest command line read, in bytes.
const MAX_COMMAND_LINE_READ: u32 = 64 * 1024;
/// Between 1601, when FILETIME starts, and 1970.
const FILETIME_UNIX_OFFSET_MS: i64 = 11_644_473_600_000;

fn win32(code: u32) -> HRESULT {
    HRESULT::from_win32(code)
}

/// `error` as the website should see it, about `what`.
fn tool_error(error: &windows::core::Error, what: &str) -> ToolError {
    let code = error.code();
    if code == win32(5) {
        ToolError::new(
            ToolErrorCode::AccessDenied,
            format!("Access to {what} is denied."),
        )
    } else if code == win32(1060) {
        ToolError::new(
            ToolErrorCode::NotFound,
            format!("{what} isn't a service on this computer."),
        )
    } else if code == win32(87) {
        ToolError::new(ToolErrorCode::NotFound, format!("{what} isn't running."))
    } else {
        failed(format!("{what}: {}", error.message()))
    }
}

fn filetime_ms(time: FILETIME) -> u64 {
    ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) / 10_000
}

/// A process's creation time in milliseconds since the Unix epoch.
fn started_at(created: FILETIME) -> Option<i64> {
    let ms = i64::try_from(filetime_ms(created)).ok()?;
    (ms > 0).then(|| ms - FILETIME_UNIX_OFFSET_MS)
}

fn times(handle: HANDLE) -> Option<(Option<i64>, u64)> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: the handle is open and the outputs are sized.
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some((started_at(created), filetime_ms(kernel) + filetime_ms(user)))
}

/// Every process's ID, parent's ID and file name.
fn entries() -> Result<Vec<(u32, u32, String)>, ToolError> {
    // SAFETY: the snapshot handle is closed on drop, and the entry is sized.
    unsafe {
        let snapshot = OwnedHandle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(
            |error| failed(format!("Could not list the processes: {}", error.message())),
        )?);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Vec::new();
        Process32FirstW(snapshot.0, &mut entry).map_err(|error| {
            failed(format!("Could not list the processes: {}", error.message()))
        })?;
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            found.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..end]),
            ));
            if let Err(error) = Process32NextW(snapshot.0, &mut entry) {
                if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    return Err(failed(format!(
                        "Could not list the processes: {}",
                        error.message()
                    )));
                }
                break;
            }
        }
        Ok(found)
    }
}

fn image_path(handle: HANDLE) -> Option<String> {
    let mut path = vec![0u16; 32 * 1024];
    let mut length = path.len() as u32;
    // SAFETY: the buffer holds `length` characters.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&path[..length as usize]))
}

/// The process's command line, which Windows gives a limited handle since
/// Windows 8.1.
fn command_line(handle: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // SAFETY: a zero-length query only reports the size needed.
    let _ = unsafe {
        NtQueryInformationProcess(
            handle,
            ProcessCommandLineInformation,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed < std::mem::size_of::<UNICODE_STRING>() as u32 || needed > MAX_COMMAND_LINE_READ {
        return None;
    }
    // Aligned for the UNICODE_STRING at its start.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer holds `needed` bytes; the string it returns points
    // into the buffer, within the length it says.
    unsafe {
        NtQueryInformationProcess(
            handle,
            ProcessCommandLineInformation,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
        .ok()
        .ok()?;
        let text = &*buffer.as_ptr().cast::<UNICODE_STRING>();
        if text.Buffer.is_null() || text.Length == 0 {
            return None;
        }
        let characters = std::slice::from_raw_parts(text.Buffer.0, usize::from(text.Length) / 2);
        Some(String::from_utf16_lossy(characters))
    }
}

/// The account a process runs as, by its SID, looked up once per SID.
fn owner(handle: HANDLE, names: &mut HashMap<Vec<u8>, Option<String>>) -> Option<String> {
    // SAFETY: the token is closed on drop; the buffers are sized, and the
    // SID read lies within the token information the call wrote.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(handle, TOKEN_QUERY, &mut token).ok()?;
        let token = OwnedHandle(token);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        if size == 0 || size > 64 * 1024 {
            return None;
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .ok()?;
        let sid = (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let key =
            std::slice::from_raw_parts(sid.0.cast::<u8>(), GetLengthSid(sid) as usize).to_vec();
        names
            .entry(key)
            .or_insert_with(|| {
                let (mut name, mut domain) = (vec![0u16; 256], vec![0u16; 256]);
                let (mut name_length, mut domain_length) = (256, 256);
                let mut kind = SID_NAME_USE::default();
                LookupAccountSidW(
                    PCWSTR::null(),
                    sid,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_length,
                    Some(PWSTR(domain.as_mut_ptr())),
                    &mut domain_length,
                    &mut kind,
                )
                .ok()?;
                let name = String::from_utf16_lossy(&name[..name_length as usize]);
                let domain = String::from_utf16_lossy(&domain[..domain_length as usize]);
                Some(if domain.is_empty() {
                    name
                } else {
                    format!("{domain}\\{name}")
                })
            })
            .clone()
    }
}

pub(super) fn read_processes() -> Result<Snapshot, ToolError> {
    let mut names = HashMap::new();
    let mut processes = Vec::new();
    for (pid, parent, name) in entries()? {
        // The System Idle Process is the processors' idle time, not a process.
        if pid == 0 {
            continue;
        }
        let mut info = ProcessInfo {
            pid,
            parent_pid: (parent != 0 && parent != pid).then_some(parent),
            name,
            path: None,
            user: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            started_at: None,
            command_line: None,
            command_line_truncated: false,
            session: None,
        };
        let mut session = 0;
        // SAFETY: the output is sized.
        if unsafe { ProcessIdToSessionId(pid, &mut session) }.is_ok() {
            info.session = Some(session);
        }
        let mut cpu_ms = 0;
        // SAFETY: the handle is closed on drop; the outputs are sized.
        if let Ok(handle) = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            let handle = OwnedHandle(handle);
            if let Some((started, cpu)) = times(handle.0) {
                info.started_at = started;
                cpu_ms = cpu;
            }
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            if unsafe { K32GetProcessMemoryInfo(handle.0, &mut counters, counters.cb) }.as_bool() {
                info.memory_bytes = counters.WorkingSetSize as u64;
            }
            info.path = image_path(handle.0);
            info.command_line = command_line(handle.0);
            info.user = owner(handle.0, &mut names);
        }
        processes.push(Reading { info, cpu_ms });
    }
    let mut memory = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: the structure is sized.
    let memory_total_bytes = unsafe { GlobalMemoryStatusEx(&mut memory) }
        .is_ok()
        .then_some(memory.ullTotalPhys);
    Ok(Snapshot {
        processes,
        // SAFETY: no pointers.
        logical_cpus: unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1),
        memory_total_bytes,
    })
}

/// The Agent's own processes, and those Windows can't do without.
pub(super) fn is_protected(info: &ProcessInfo) -> bool {
    info.pid <= 4
        || crate::remote::monitors::is_protected_process(&info.name)
        || info
            .path
            .as_deref()
            .is_some_and(crate::remote::background_tasks::is_agent_image)
}

fn end_one(info: &ProcessInfo, deadline: &Deadline) -> Result<(), ToolError> {
    if deadline.passed() {
        return Err(out_of_time(info));
    }
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    // SAFETY: the handle is closed on drop; the outputs are sized.
    unsafe {
        let handle = OwnedHandle(
            OpenProcess(access, false, info.pid).map_err(|error| tool_error(&error, &info.name))?,
        );
        if times(handle.0).map(|(started, _)| started) != Some(info.started_at) {
            return Err(ToolError::new(
                ToolErrorCode::Changed,
                format!("{} has already exited.", info.name),
            ));
        }
        let mut critical = windows::core::BOOL(0);
        if IsProcessCritical(handle.0, &mut critical).is_ok() && critical.as_bool() {
            return Err(ToolError::new(
                ToolErrorCode::Protected,
                format!(
                    "Windows marks {} as critical: ending it would stop the computer.",
                    info.name
                ),
            ));
        }
        TerminateProcess(handle.0, 1).map_err(|error| tool_error(&error, &info.name))?;
        let wait = EXIT_WAIT.min(deadline.left());
        // An ended process may take a moment to go; it is ending regardless.
        let _ = WaitForSingleObject(handle.0, wait.as_millis() as u32) == WAIT_OBJECT_0;
    }
    Ok(())
}

pub(super) fn end_processes(
    targets: &[&ProcessInfo],
    deadline: &Deadline,
) -> Vec<Result<(), ToolError>> {
    targets.iter().map(|info| end_one(info, deadline)).collect()
}

/// A handle to the Service Control Manager or a service, closed on drop.
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn manager(access: u32) -> Result<Sc, ToolError> {
    // SAFETY: null names open the local computer's active database.
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map(Sc)
        .map_err(|error| tool_error(&error, "the Service Control Manager"))
}

fn open(manager: &Sc, name: &str, access: u32) -> Result<Sc, ToolError> {
    let wide_name = wide(name);
    // SAFETY: the name is NUL-terminated.
    unsafe { OpenServiceW(manager.0, PCWSTR(wide_name.as_ptr()), access) }
        .map(Sc)
        .map_err(|error| tool_error(&error, name))
}

/// Every Win32 service's key name, display name and status.
fn enumerate(manager: &Sc) -> Result<Vec<(String, String, SERVICE_STATUS_PROCESS)>, ToolError> {
    let (mut needed, mut count, mut resume) = (0, 0, 0);
    // Aligned storage for the structures and strings it returns.
    let mut buffer = vec![0usize; 256 * 1024 / std::mem::size_of::<usize>()];
    let mut rows = Vec::new();
    loop {
        // SAFETY: the byte view covers the buffer, which outlives it; the
        // entries read are those the call says it wrote, whose strings point
        // into the buffer.
        unsafe {
            let bytes = std::slice::from_raw_parts_mut(
                buffer.as_mut_ptr().cast::<u8>(),
                buffer.len() * std::mem::size_of::<usize>(),
            );
            let result = EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                Some(bytes),
                &mut needed,
                &mut count,
                Some(&mut resume),
                PCWSTR::null(),
            );
            for service in std::slice::from_raw_parts(
                buffer.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                count as usize,
            ) {
                rows.push((
                    service.lpServiceName.to_string().unwrap_or_default(),
                    service.lpDisplayName.to_string().unwrap_or_default(),
                    service.ServiceStatusProcess,
                ));
            }
            match result {
                Ok(()) => break,
                Err(error) if error.code() == ERROR_MORE_DATA.to_hresult() => {}
                Err(error) => return Err(tool_error(&error, "the services")),
            }
        }
    }
    Ok(rows)
}

/// A buffer of `needed` bytes, aligned for the structures the
/// configuration calls write.
fn aligned(needed: u32) -> Vec<u64> {
    vec![0u64; (needed as usize).div_ceil(8).max(1)]
}

fn bytes(buffer: &mut [u64]) -> &mut [u8] {
    // SAFETY: the view covers exactly the buffer's bytes.
    unsafe {
        std::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast(), std::mem::size_of_val(buffer))
    }
}

struct Config {
    start: SERVICE_START_TYPE,
    account: Option<String>,
    display_name: Option<String>,
}

fn config(service: &Sc) -> Option<Config> {
    let mut needed = 0;
    // SAFETY: a query without a buffer only reports the size needed; the
    // second writes within the buffer, and its strings point into it.
    unsafe {
        let first = QueryServiceConfigW(service.0, None, 0, &mut needed);
        if first
            .as_ref()
            .is_err_and(|error| error.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult())
        {
            return None;
        }
        let mut buffer = aligned(needed);
        let size = (buffer.len() * 8) as u32;
        QueryServiceConfigW(
            service.0,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut needed,
        )
        .ok()?;
        let config = &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        let text = |text: PWSTR| {
            (!text.is_null())
                .then(|| text.to_string().ok())
                .flatten()
                .filter(|text| !text.is_empty())
        };
        Some(Config {
            start: config.dwStartType,
            account: text(config.lpServiceStartName),
            display_name: text(config.lpDisplayName),
        })
    }
}

/// The optional configuration `level` of `service`: a structure without
/// pointers into the buffer.
fn config2<T: Copy>(
    service: &Sc,
    level: windows::Win32::System::Services::SERVICE_CONFIG,
) -> Option<T> {
    let mut needed = 0;
    // SAFETY: as for `config`; the structure read lies at the buffer's start.
    unsafe {
        let _ = QueryServiceConfig2W(service.0, level, None, &mut needed);
        if needed == 0 || needed > 64 * 1024 {
            return None;
        }
        let mut buffer = aligned(needed);
        QueryServiceConfig2W(service.0, level, Some(bytes(&mut buffer)), &mut needed).ok()?;
        Some(*buffer.as_ptr().cast::<T>())
    }
}

fn description(service: &Sc) -> Option<String> {
    let mut needed = 0;
    // SAFETY: as for `config2`; the string points into the buffer, so it is
    // copied out while the buffer lives.
    unsafe {
        let _ = QueryServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, None, &mut needed);
        if needed == 0 || needed > 64 * 1024 {
            return None;
        }
        let mut buffer = aligned(needed);
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(bytes(&mut buffer)),
            &mut needed,
        )
        .ok()?;
        let text = (*buffer.as_ptr().cast::<SERVICE_DESCRIPTIONW>()).lpDescription;
        (!text.is_null())
            .then(|| text.to_string().ok())
            .flatten()
            .filter(|text| !text.trim().is_empty())
    }
}

fn delayed(service: &Sc) -> bool {
    config2::<SERVICE_DELAYED_AUTO_START_INFO>(service, SERVICE_CONFIG_DELAYED_AUTO_START_INFO)
        .is_some_and(|info| info.fDelayedAutostart.as_bool())
}

fn status(service: &Sc) -> Result<SERVICE_STATUS_PROCESS, ToolError> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    // SAFETY: the view covers the structure, which outlives it.
    unsafe {
        let view = std::slice::from_raw_parts_mut(
            (&raw mut status).cast::<u8>(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
        );
        QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(view), &mut needed)
            .map_err(|error| tool_error(&error, "the service"))?;
    }
    Ok(status)
}

fn state(state: SERVICE_STATUS_CURRENT_STATE) -> (ServiceState, &'static str) {
    match state.0 {
        1 => (ServiceState::Stopped, "stopped"),
        2 => (ServiceState::Starting, "starting"),
        3 => (ServiceState::Stopping, "stopping"),
        4 => (ServiceState::Running, "running"),
        5 => (ServiceState::Starting, "resuming"),
        6 => (ServiceState::Other, "pausing"),
        7 => (ServiceState::Paused, "paused"),
        _ => (ServiceState::Other, "unknown"),
    }
}

fn startup(start: SERVICE_START_TYPE, delayed: bool) -> ServiceStartup {
    match start {
        SERVICE_AUTO_START if delayed => ServiceStartup::AutomaticDelayed,
        SERVICE_AUTO_START => ServiceStartup::Automatic,
        SERVICE_DEMAND_START => ServiceStartup::Manual,
        SERVICE_DISABLED => ServiceStartup::Disabled,
        _ => ServiceStartup::Static,
    }
}

const CHOICES: [ServiceStartup; 4] = [
    ServiceStartup::Automatic,
    ServiceStartup::AutomaticDelayed,
    ServiceStartup::Manual,
    ServiceStartup::Disabled,
];

/// `name` as the website shows it, with its status.
fn describe(
    manager: &Sc,
    name: &str,
    display_name: Option<String>,
    known: Option<SERVICE_STATUS_PROCESS>,
) -> Result<ServiceInfo, ToolError> {
    let service = open(manager, name, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)?;
    let current = match known {
        Some(status) => status,
        None => status(&service)?,
    };
    let config = config(&service);
    let protected = crate::service::is_agent_service(name);
    let startup = config.as_ref().map(|config| {
        let automatic = config.start == SERVICE_AUTO_START;
        startup(config.start, automatic && delayed(&service))
    });
    let (state, detail) = state(current.dwCurrentState);
    Ok(ServiceInfo {
        name: name.to_owned(),
        display_name: display_name
            .or_else(|| {
                config
                    .as_ref()
                    .and_then(|config| config.display_name.clone())
            })
            .unwrap_or_else(|| name.to_owned()),
        state,
        state_detail: Some(detail.to_owned()),
        startup,
        startup_detail: None,
        startup_choices: if protected || startup == Some(ServiceStartup::Static) {
            Vec::new()
        } else {
            CHOICES.to_vec()
        },
        account: config.and_then(|config| config.account),
        description: description(&service),
        pid: (current.dwProcessId != 0).then_some(current.dwProcessId),
        protected,
    })
}

pub(super) fn list_services() -> Result<Vec<ServiceInfo>, ToolError> {
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    Ok(enumerate(&manager)?
        .into_iter()
        .filter_map(|(name, display_name, status)| {
            // A service removed since it was listed is left out.
            describe(&manager, &name, Some(display_name), Some(status)).ok()
        })
        .collect())
}

/// The service's key name, given its key name, so the Agent's own service
/// is known by any spelling.
fn key_name(manager: &Sc, name: &str) -> Result<String, ToolError> {
    open(manager, name, SERVICE_QUERY_STATUS)?;
    Ok(enumerate(manager)?
        .into_iter()
        .map(|(key, ..)| key)
        .find(|key| key.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| name.to_owned()))
}

/// Waits until `service` is `wanted`, or the deadline passes.
fn wait_for(
    service: &Sc,
    name: &str,
    wanted: ServiceState,
    deadline: &Deadline,
) -> Result<(), ToolError> {
    let mut was_starting = false;
    loop {
        let current = status(service)?;
        let (now, detail) = state(current.dwCurrentState);
        if now == wanted {
            return Ok(());
        }
        if wanted == ServiceState::Running && now == ServiceState::Starting {
            was_starting = true;
        }
        if wanted == ServiceState::Running && now == ServiceState::Stopped && was_starting {
            return Err(failed(format!(
                "{name} stopped as it started (exit code {}).",
                current.dwWin32ExitCode
            )));
        }
        if deadline.passed() {
            return Err(ToolError::new(
                ToolErrorCode::TimedOut,
                format!(
                    "{name} was still {detail} when the time ran out; refresh in a moment to see whether it finished."
                ),
            ));
        }
        std::thread::sleep(SERVICE_POLL.min(deadline.left()));
    }
}

fn stop(service: &Sc, name: &str, deadline: &Deadline) -> Result<(), ToolError> {
    if status(service)?.dwCurrentState.0 == 1 {
        return Ok(());
    }
    let mut ignored = SERVICE_STATUS::default();
    // SAFETY: the status is sized.
    if let Err(error) = unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut ignored) } {
        let code = error.code();
        if code == win32(1051) {
            return Err(ToolError::new(
                ToolErrorCode::Dependents,
                format!("Other running services depend on {name}; stop them first."),
            ));
        }
        if code == win32(1052) {
            return Err(ToolError::new(
                ToolErrorCode::Invalid,
                format!("{name} can't be stopped."),
            ));
        }
        // Not running by now.
        if code != win32(1062) {
            return Err(tool_error(&error, name));
        }
    }
    wait_for(service, name, ServiceState::Stopped, deadline)
}

fn start(service: &Sc, name: &str, deadline: &Deadline) -> Result<(), ToolError> {
    // SAFETY: no arguments.
    if let Err(error) = unsafe { StartServiceW(service.0, None) } {
        let code = error.code();
        if code == win32(1058) {
            return Err(ToolError::new(
                ToolErrorCode::Invalid,
                format!("{name} is disabled; set it to start manually or automatically first."),
            ));
        }
        // Already running.
        if code != win32(1056) {
            return Err(tool_error(&error, name));
        }
    }
    wait_for(service, name, ServiceState::Running, deadline)
}

pub(super) fn control_service(
    name: &str,
    action: ServiceAction,
    deadline: &Deadline,
) -> Result<ServiceInfo, ToolError> {
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    let name = key_name(&manager, name)?;
    if action != ServiceAction::Start && crate::service::is_agent_service(&name) {
        return Err(own_service());
    }
    let service = open(
        &manager,
        &name,
        SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS,
    )?;
    match action {
        ServiceAction::Start => start(&service, &name, deadline)?,
        ServiceAction::Stop => stop(&service, &name, deadline)?,
        ServiceAction::Restart => {
            stop(&service, &name, deadline)?;
            start(&service, &name, deadline)?;
        }
    }
    describe(&manager, &name, None, None)
}

pub(super) fn set_service_startup(
    name: &str,
    wanted: ServiceStartup,
    _deadline: &Deadline,
) -> Result<ServiceInfo, ToolError> {
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    let name = key_name(&manager, name)?;
    if crate::service::is_agent_service(&name) {
        return Err(own_service());
    }
    let start = match wanted {
        ServiceStartup::Automatic | ServiceStartup::AutomaticDelayed => SERVICE_AUTO_START,
        ServiceStartup::Manual => SERVICE_DEMAND_START,
        ServiceStartup::Disabled => SERVICE_DISABLED,
        ServiceStartup::Static => {
            return Err(ToolError::new(
                ToolErrorCode::Invalid,
                "A service can't be set to that.",
            ));
        }
    };
    let service = open(
        &manager,
        &name,
        SERVICE_CHANGE_CONFIG | SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
    )?;
    // SAFETY: null strings leave those settings alone; the delayed-start
    // structure outlives the call.
    unsafe {
        ChangeServiceConfigW(
            service.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            start,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
        .map_err(|error| tool_error(&error, &name))?;
        if start == SERVICE_AUTO_START {
            let info = SERVICE_DELAYED_AUTO_START_INFO {
                fDelayedAutostart: (wanted == ServiceStartup::AutomaticDelayed).into(),
            };
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some((&raw const info).cast()),
            )
            .map_err(|error| tool_error(&error, &name))?;
        }
    }
    drop(service);
    describe(&manager, &name, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_times_are_unix_milliseconds() {
        // 2024-01-01T00:00:00Z.
        let ticks: u64 = (1_704_067_200_000 + FILETIME_UNIX_OFFSET_MS as u64) * 10_000;
        let time = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        assert_eq!(started_at(time), Some(1_704_067_200_000));
        assert_eq!(started_at(FILETIME::default()), None);
    }

    #[test]
    fn startup_types_map_both_ways() {
        assert_eq!(
            startup(SERVICE_AUTO_START, true),
            ServiceStartup::AutomaticDelayed
        );
        assert_eq!(
            startup(SERVICE_AUTO_START, false),
            ServiceStartup::Automatic
        );
        assert_eq!(startup(SERVICE_DEMAND_START, false), ServiceStartup::Manual);
        assert_eq!(startup(SERVICE_DISABLED, false), ServiceStartup::Disabled);
        assert_eq!(
            startup(SERVICE_START_TYPE(0), false),
            ServiceStartup::Static
        );
    }

    #[test]
    fn the_agents_own_service_is_refused() {
        let deadline = Deadline::distant();
        // Refused before anything else, whether or not it is installed here.
        for name in ["MeshRMMAgent", "meshrmmagent"] {
            match control_service(name, ServiceAction::Stop, &deadline) {
                Err(error) => assert!(
                    matches!(
                        error.code,
                        ToolErrorCode::Protected | ToolErrorCode::NotFound
                    ),
                    "{error:?}"
                ),
                Ok(service) => panic!("stopped {service:?}"),
            }
        }
    }
}
