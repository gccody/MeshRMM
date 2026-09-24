//! Native, read-only sampling. Slow queries never run on the window thread.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, ensure};
use std::sync::Arc;
use windows::Win32::Foundation::*;
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::RemoteDesktop::*;
use windows::Win32::System::Services::*;
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, EnumWindows, GetWindowThreadProcessId, HICON, IsHungAppWindow, IsWindowVisible,
};
use windows::core::{PCWSTR, PWSTR, w};

#[derive(Debug)]
pub struct Icon(pub usize);
impl Drop for Icon {
    fn drop(&mut self) {
        let _ = unsafe { DestroyIcon(HICON(self.0 as *mut _)) };
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn time(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}
pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

#[derive(Clone, Default, Debug)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    pub description: String,
    pub path: String,
    pub icon: Option<Arc<Icon>>,
    pub user: String,
    pub session: Option<u32>,
    pub memory: Option<usize>,
    pub created: Option<u64>,
    pub cpu_time: u64,
    pub cpu: Option<f64>,
    pub io_bytes: Option<u64>,
    pub io_rate: Option<f64>,
    pub disk_bytes: Option<u64>,
    pub network_bytes: Option<u64>,
    pub disk_rate: Option<f64>,
    pub network_rate: Option<f64>,
    pub handles: u32,
    pub threads: u32,
    pub priority: u32,
    pub hung: bool,
}

pub fn creation(process: HANDLE) -> anyhow::Result<u64> {
    let (mut created, mut exit, mut kernel, mut user) = Default::default();
    unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user)? };
    Ok(time(created))
}

pub fn identified_handle(row: &Process, access: PROCESS_ACCESS_RIGHTS) -> anyhow::Result<Handle> {
    let expected = row
        .created
        .context("This process cannot be safely identified. Refresh the list.")?;
    let handle =
        Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | access, false, row.pid)? });
    ensure!(
        creation(handle.0)? == expected,
        "The process has exited. Refresh the list."
    );
    Ok(handle)
}
/// Processes that `pid` descends from, following each parent only while it is older than its
/// child, so a recycled parent PID ends the chain.
pub fn ancestors(processes: &[Process], pid: u32) -> HashSet<u32> {
    let mut result = HashSet::new();
    let mut child = processes.iter().find(|p| p.pid == pid);
    while let Some(current) = child {
        child = processes.iter().find(|parent| {
            parent.pid == current.parent
                && parent.pid != current.pid
                && parent
                    .created
                    .zip(current.created)
                    .is_some_and(|(parent, child)| parent <= child)
        });
        if child.is_some_and(|parent| !result.insert(parent.pid)) {
            break;
        }
    }
    result
}

/// Whether `path` is the Agent executable or a copy of it, such as an update helper. Copies are
/// compared by content, so another program cannot gain this protection by its name or folder.
fn is_agent_image(path: &str) -> bool {
    !path.is_empty()
        && std::env::current_exe()
            .and_then(|agent| same_contents(&agent, Path::new(path)))
            .unwrap_or(false)
}

fn same_contents(first: &Path, second: &Path) -> std::io::Result<bool> {
    use std::io::Read;

    let (mut first, mut second) = (std::fs::File::open(first)?, std::fs::File::open(second)?);
    if first.metadata()?.len() != second.metadata()?.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (vec![0; 64 * 1024], vec![0; 64 * 1024]);
    loop {
        let read = first.read(&mut a)?;
        if read == 0 {
            return Ok(true);
        }
        second.read_exact(&mut b[..read])?;
        if a[..read] != b[..read] {
            return Ok(false);
        }
    }
}

/// Opens `row` for ending it, unless it is this Task Manager, one of the `protected` processes
/// that run it, an Agent process, or a process Windows needs.
pub fn termination_handle(row: &Process, protected: &HashSet<u32>) -> anyhow::Result<Handle> {
    ensure!(
        row.pid != std::process::id(),
        "The process manager cannot end itself."
    );
    ensure!(
        !protected.contains(&row.pid) && !is_agent_image(&row.path),
        "{} is part of the MeshRMM Agent, which runs this remote session; it cannot be ended here.",
        row.name
    );
    let handle = identified_handle(row, PROCESS_TERMINATE)?;
    let mut critical = windows::core::BOOL(0);
    unsafe { IsProcessCritical(handle.0, &mut critical)? };
    ensure!(
        !critical.as_bool(),
        "Windows marks this process as critical; it cannot be ended here."
    );
    Ok(handle)
}

fn owner(process: HANDLE) -> anyhow::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
        let token = Handle(token);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        ensure!(size > 0 && size < 65536, "Invalid token size");
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let (mut name, mut domain) = ([0u16; 512], [0u16; 512]);
        let (mut nc, mut dc) = (512, 512);
        let mut kind = SID_NAME_USE::default();
        LookupAccountSidW(
            PCWSTR::null(),
            user.User.Sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut nc,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut dc,
            &mut kind,
        )?;
        Ok(String::from_utf16_lossy(&name[..nc as usize]))
    }
}

fn description(path: &str) -> Option<String> {
    unsafe {
        let path = wide(path);
        let size = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None);
        if size == 0 || size > 1024 * 1024 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(path.as_ptr()), None, size, bytes.as_mut_ptr().cast()).ok()?;
        let mut translations = std::ptr::null_mut();
        let mut len = 0;
        if !VerQueryValueW(
            bytes.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &mut translations,
            &mut len,
        )
        .as_bool()
            || len < 4
        {
            return None;
        }
        let lang = std::slice::from_raw_parts(translations.cast::<u16>(), 2);
        let query = wide(&format!(
            "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
            lang[0], lang[1]
        ));
        let mut value = std::ptr::null_mut();
        if !VerQueryValueW(
            bytes.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut value,
            &mut len,
        )
        .as_bool()
            || len < 2
        {
            return None;
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(
            value.cast::<u16>(),
            len as usize - 1,
        )))
    }
}

pub fn processes(previous: &[Process]) -> anyhow::Result<Vec<Process>> {
    let prior: HashMap<_, _> = previous.iter().map(|p| ((p.pid, p.created), p)).collect();
    unsafe extern "system" fn hung_window(hwnd: HWND, context: LPARAM) -> windows::core::BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && IsHungAppWindow(hwnd).as_bool() {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                (&mut *(context.0 as *mut std::collections::HashSet<u32>)).insert(pid);
            }
        }
        windows::core::BOOL(1)
    }
    let mut hung = std::collections::HashSet::<u32>::new();
    let _ = unsafe {
        EnumWindows(
            Some(hung_window),
            LPARAM((&mut hung as *mut std::collections::HashSet<u32>) as isize),
        )
    };
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut rows = Vec::new();
    unsafe { Process32FirstW(snapshot.0, &mut entry)? };
    loop {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let mut row = Process {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..end]),
            threads: entry.cntThreads,
            ..Default::default()
        };
        let mut session = 0;
        if unsafe { ProcessIdToSessionId(row.pid, &mut session) }.is_ok() {
            row.session = Some(session);
        }
        if let Ok(handle) =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, row.pid) }
        {
            let handle = Handle(handle);
            let (mut created, mut exit, mut kernel, mut user) = Default::default();
            if unsafe { GetProcessTimes(handle.0, &mut created, &mut exit, &mut kernel, &mut user) }
                .is_ok()
            {
                row.created = Some(time(created));
                row.cpu_time = time(kernel) + time(user);
            }
            if let Some(old) = prior.get(&(row.pid, row.created)) {
                row.path.clone_from(&old.path);
                row.icon.clone_from(&old.icon);
                row.user.clone_from(&old.user);
                row.description.clone_from(&old.description);
            } else {
                let mut path = [0u16; 32768];
                let mut len = path.len() as u32;
                if unsafe {
                    QueryFullProcessImageNameW(
                        handle.0,
                        PROCESS_NAME_WIN32,
                        PWSTR(path.as_mut_ptr()),
                        &mut len,
                    )
                }
                .is_ok()
                {
                    row.path = String::from_utf16_lossy(&path[..len as usize]);
                    row.description = description(&row.path).unwrap_or_else(|| row.name.clone());
                    let path = wide(&row.path);
                    let mut icon = HICON::default();
                    unsafe {
                        ExtractIconExW(PCWSTR(path.as_ptr()), 0, None, Some(&mut icon), 1);
                    }
                    if !icon.is_invalid() {
                        row.icon = Some(Arc::new(Icon(icon.0 as usize)));
                    }
                }
                row.user = owner(handle.0).unwrap_or_default();
            }
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            if unsafe {
                GetProcessMemoryInfo(
                    handle.0,
                    &mut counters,
                    std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                )
            }
            .is_ok()
            {
                row.memory = Some(counters.WorkingSetSize);
            }
            let mut io = IO_COUNTERS::default();
            if unsafe { GetProcessIoCounters(handle.0, &mut io) }.is_ok() {
                row.io_bytes = Some(io.ReadTransferCount.saturating_add(io.WriteTransferCount));
            }
            let _ = unsafe { GetProcessHandleCount(handle.0, &mut row.handles) };
            row.priority = unsafe { GetPriorityClass(handle.0) };
        }
        if row.pid == std::process::id() {
            row.description = "Task Manager".into();
        }
        if row.description.is_empty() {
            row.description.clone_from(&row.name);
        }
        row.hung = hung.contains(&row.pid);
        rows.push(row);
        if let Err(error) = unsafe { Process32NextW(snapshot.0, &mut entry) } {
            if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                return Err(error.into());
            }
            break;
        }
    }
    Ok(rows)
}

#[derive(Clone, Default)]
pub struct Service {
    pub name: String,
    pub description: String,
    pub pid: u32,
    pub state: u32,
}
impl Service {
    pub fn status(&self) -> &'static str {
        match self.state {
            1 => "Stopped",
            2 => "Start pending",
            3 => "Stop pending",
            4 => "Running",
            5 => "Continue pending",
            6 => "Pause pending",
            7 => "Paused",
            _ => "Unknown",
        }
    }
}
fn services() -> anyhow::Result<Vec<Service>> {
    unsafe {
        let manager = ServiceHandle(OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_ENUMERATE_SERVICE,
        )?);
        let (mut needed, mut count, mut resume) = (0, 0, 0);
        // The API requires aligned storage for the returned structures and strings.
        let mut buffer = vec![0usize; 256 * 1024 / std::mem::size_of::<usize>()];
        let mut rows = Vec::new();
        loop {
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
                rows.push(Service {
                    name: service.lpServiceName.to_string()?,
                    description: service.lpDisplayName.to_string()?,
                    pid: service.ServiceStatusProcess.dwProcessId,
                    state: service.ServiceStatusProcess.dwCurrentState.0,
                });
            }
            match result {
                Ok(()) => break,
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(rows)
    }
}
#[derive(Clone, Copy)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}
pub fn service_action(name: &str, action: ServiceAction) -> anyhow::Result<()> {
    // Losing this service also loses the connection needed to finish the action.
    ensure!(
        !crate::service::is_agent_service(name),
        "The remote connection service cannot be changed here."
    );
    unsafe {
        let manager = ServiceHandle(OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT,
        )?);
        let name = wide(name);
        let service = ServiceHandle(OpenServiceW(
            manager.0,
            PCWSTR(name.as_ptr()),
            SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS,
        )?);
        if matches!(action, ServiceAction::Stop | ServiceAction::Restart) {
            let mut status = SERVICE_STATUS::default();
            QueryServiceStatus(service.0, &mut status)?;
            if status.dwCurrentState != SERVICE_STOPPED {
                ControlService(service.0, SERVICE_CONTROL_STOP, &mut status)?;
            }
            if matches!(action, ServiceAction::Restart) {
                let deadline = Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    QueryServiceStatus(service.0, &mut status)?;
                    if status.dwCurrentState == SERVICE_STOPPED {
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "Service did not stop within 30 seconds; restart was not attempted."
                    );
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        if matches!(action, ServiceAction::Start | ServiceAction::Restart) {
            StartServiceW(service.0, None)?;
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct User {
    pub id: u32,
    pub name: String,
    pub status: String,
}
fn users() -> anyhow::Result<Vec<User>> {
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut count = 0;
        WTSEnumerateSessionsW(None, 0, 1, &mut ptr, &mut count)?;
        let mut rows = Vec::new();
        for session in std::slice::from_raw_parts(ptr, count as usize) {
            let mut value = PWSTR::null();
            let mut bytes = 0;
            if WTSQuerySessionInformationW(
                None,
                session.SessionId,
                WTSUserName,
                &mut value,
                &mut bytes,
            )
            .is_ok()
            {
                let name = value.to_string().unwrap_or_default();
                WTSFreeMemory(value.0.cast());
                if !name.is_empty() {
                    rows.push(User {
                        id: session.SessionId,
                        name,
                        status: if session.State == WTSActive {
                            "Active"
                        } else {
                            "Disconnected"
                        }
                        .into(),
                    });
                }
            }
        }
        WTSFreeMemory(ptr.cast());
        Ok(rows)
    }
}

#[derive(Clone, Default)]
pub struct Startup {
    pub name: String,
    pub command: String,
    pub location: String,
    pub enabled: bool,
    pub root: String,
    pub run_key: String,
    pub approval_key: String,
    pub file: Option<(std::path::PathBuf, std::time::SystemTime)>,
}
fn open_key(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> anyhow::Result<Key> {
    let mut key = HKEY::default();
    let path = wide(path);
    unsafe {
        RegOpenKeyExW(root, PCWSTR(path.as_ptr()), None, access, &mut key).ok()?;
    }
    Ok(Key(key))
}
fn approval(root: HKEY, key: &str, name: &str) -> Option<Vec<u8>> {
    let key = wide(key);
    let name = wide(name);
    let mut size = 0;
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        if size > 4096 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(bytes.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        Some(bytes)
    }
}
fn registry_string(root: HKEY, path: &str, name: &str) -> Option<String> {
    let path = wide(path);
    let name = wide(name);
    let mut text = vec![0u16; 32768];
    let mut size = (text.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(path.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
    Some(String::from_utf16_lossy(&text[..end]))
}
fn startups() -> anyhow::Result<Vec<Startup>> {
    let mut roots = vec![("Machine".to_string(), HKEY_LOCAL_MACHINE, String::new())];
    unsafe {
        for index in 0..1024 {
            let mut name = [0u16; 256];
            let mut size = name.len() as u32;
            let result = RegEnumKeyExW(
                HKEY_USERS,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut size,
                None,
                None,
                None,
                None,
            );
            if result == ERROR_NO_MORE_ITEMS {
                break;
            }
            result.ok()?;
            let name = String::from_utf16_lossy(&name[..size as usize]);
            if name.starts_with("S-1-5-21-") && !name.ends_with("_Classes") {
                roots.push((name.clone(), HKEY_USERS, format!("{name}\\")));
            }
        }
    }
    let mut rows = Vec::new();
    for (root_name, root, prefix) in roots {
        // Shell Folders stores the user's expanded path, including redirection.
        // Never expand USERPROFILE against this SYSTEM helper's environment.
        let shell = format!(
            "{prefix}Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders"
        );
        let folder = registry_string(
            root,
            &shell,
            if root_name == "Machine" {
                "Common Startup"
            } else {
                "Startup"
            },
        );
        if let Some(folder) = folder
            && let Ok(entries) = std::fs::read_dir(&folder)
        {
            let approved = format!(
                "{prefix}Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder"
            );
            for entry in entries.take(10000).flatten() {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.eq_ignore_ascii_case("desktop.ini") {
                    continue;
                }
                let Ok(modified) = metadata.modified() else {
                    continue;
                };
                let enabled = approval(root, &approved, &name)
                    .is_none_or(|v| v.first().is_none_or(|s| s & 1 == 0));
                rows.push(Startup {
                    name,
                    command: entry.path().display().to_string(),
                    location: if root_name == "Machine" {
                        "All users".into()
                    } else {
                        root_name.clone()
                    },
                    enabled,
                    root: root_name.clone(),
                    run_key: folder.clone(),
                    approval_key: approved.clone(),
                    file: Some((entry.path(), modified)),
                });
            }
        }
        for (run, approved) in [
            ("Software\\Microsoft\\Windows\\CurrentVersion\\Run", "Run"),
            (
                "Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Run",
                "Run32",
            ),
        ] {
            let path = format!("{prefix}{run}");
            let Ok(key) = open_key(root, &path, KEY_READ) else {
                continue;
            };
            let approved = format!(
                "{prefix}Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\{approved}"
            );
            for index in 0..10000 {
                let mut name = vec![0u16; 16384];
                let mut name_len = name.len() as u32;
                let mut value = vec![0u16; 32768];
                let mut size = (value.len() * 2) as u32;
                let mut kind = 0;
                let result = unsafe {
                    RegEnumValueW(
                        key.0,
                        index,
                        Some(PWSTR(name.as_mut_ptr())),
                        &mut name_len,
                        None,
                        Some(&mut kind),
                        Some(value.as_mut_ptr().cast()),
                        Some(&mut size),
                    )
                };
                if result == ERROR_NO_MORE_ITEMS {
                    break;
                }
                result.ok()?;
                if kind != REG_SZ.0 && kind != REG_EXPAND_SZ.0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&name[..name_len as usize]);
                let count = (size as usize / 2).min(value.len());
                let end = value[..count].iter().position(|c| *c == 0).unwrap_or(count);
                let enabled = approval(root, &approved, &name)
                    .is_none_or(|v| v.first().is_none_or(|s| s & 1 == 0));
                rows.push(Startup {
                    name,
                    command: String::from_utf16_lossy(&value[..end]),
                    location: if root_name == "Machine" {
                        "All users".into()
                    } else {
                        root_name.clone()
                    },
                    enabled,
                    root: root_name.clone(),
                    run_key: path.clone(),
                    approval_key: approved.clone(),
                    file: None,
                });
            }
        }
    }
    Ok(rows)
}
pub fn startup_action(row: &Startup) -> anyhow::Result<()> {
    let root = if row.root == "Machine" {
        HKEY_LOCAL_MACHINE
    } else {
        HKEY_USERS
    };
    set_startup(root, row)
}
fn set_startup(root: HKEY, row: &Startup) -> anyhow::Result<()> {
    let name = wide(&row.name);
    if let Some((path, modified)) = &row.file {
        ensure!(
            path.is_file() && path.metadata()?.modified()? == *modified,
            "The startup file changed. Refresh before changing it."
        );
    } else {
        // Re-read the original command before writing only the approval value.
        let key = wide(&row.run_key);
        let mut command = vec![0u16; 32768];
        let mut size = (command.len() * 2) as u32;
        unsafe {
            RegGetValueW(
                root,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                None,
                Some(command.as_mut_ptr().cast()),
                Some(&mut size),
            )
            .ok()?;
        }
        let end = command
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(command.len());
        ensure!(
            String::from_utf16_lossy(&command[..end]) == row.command,
            "The startup entry changed. Refresh before changing it."
        );
    }
    let enabled = approval(root, &row.approval_key, &row.name)
        .is_none_or(|v| v.first().is_none_or(|s| s & 1 == 0));
    ensure!(
        enabled == row.enabled,
        "Startup status changed. Refresh before changing it."
    );
    let mut value = [0u8; 12];
    value[0] = if row.enabled { 3 } else { 2 };
    if row.enabled {
        value[4..].copy_from_slice(&time(unsafe { GetSystemTimeAsFileTime() }).to_le_bytes());
    }
    let key = wide(&row.approval_key);
    unsafe {
        RegSetKeyValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_BINARY.0,
            Some(value.as_ptr().cast()),
            value.len() as u32,
        )
        .ok()?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Snapshot {
    pub telemetry: Option<super::telemetry::Shared>,
    pub processes: Vec<Process>,
    pub services: Vec<Service>,
    pub users: Vec<User>,
    pub startups: Vec<Startup>,
    pub sampled: Instant,
    pub cpu_total: u64,
    pub cpu_idle: u64,
    pub cpu: Option<f64>,
    pub memory_total: u64,
    pub memory_available: u64,
    pub commit: u64,
    pub commit_limit: u64,
    pub handles: u32,
    pub threads: u32,
    pub uptime: u64,
    pub network_bytes: u64,
    pub network_rate: Option<f64>,
    pub network_capacity: u64,
    pub disk: Option<f64>,
    pub disk_rate: Option<f64>,
    pub errors: Vec<String>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            telemetry: None,
            processes: Vec::new(),
            services: Vec::new(),
            users: Vec::new(),
            startups: Vec::new(),
            sampled: Instant::now(),
            cpu_total: 0,
            cpu_idle: 0,
            cpu: None,
            memory_total: 0,
            memory_available: 0,
            commit: 0,
            commit_limit: 0,
            handles: 0,
            threads: 0,
            uptime: 0,
            network_bytes: 0,
            network_rate: None,
            network_capacity: 0,
            disk: None,
            disk_rate: None,
            errors: Vec::new(),
        }
    }
}
fn percent(delta: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (delta as f64 * 100.0 / total as f64).clamp(0.0, 100.0))
}
pub fn sample(previous: &Snapshot, inventory: bool) -> anyhow::Result<Snapshot> {
    if let Some(counters) = &previous.telemetry {
        super::telemetry::update_threads(counters);
    }
    let mut next = Snapshot {
        telemetry: previous.telemetry.clone(),
        processes: processes(&previous.processes)?,
        ..Default::default()
    };
    unsafe {
        let (mut idle, mut kernel, mut user) = Default::default();
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user))?;
        next.cpu_total = time(kernel) + time(user);
        next.cpu_idle = time(idle);
        let mut memory = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut memory)?;
        next.memory_total = memory.ullTotalPhys;
        next.memory_available = memory.ullAvailPhys;
        let mut performance = PERFORMANCE_INFORMATION::default();
        GetPerformanceInfo(
            &mut performance,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )?;
        next.commit = (performance.CommitTotal * performance.PageSize) as u64;
        next.commit_limit = (performance.CommitLimit * performance.PageSize) as u64;
        next.handles = performance.HandleCount;
        next.threads = performance.ThreadCount;
        next.uptime = GetTickCount64() / 1000;
        let mut table = std::ptr::null_mut();
        if GetIfTable2(&mut table).is_ok() {
            for row in
                std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
            {
                // Hardware adapters only: avoid counting loopback/tunnel traffic twice.
                if row.InterfaceAndOperStatusFlags._bitfield & 1 != 0 {
                    next.network_capacity = next
                        .network_capacity
                        .saturating_add(row.ReceiveLinkSpeed.max(row.TransmitLinkSpeed));
                    next.network_bytes = next
                        .network_bytes
                        .saturating_add(row.InOctets)
                        .saturating_add(row.OutOctets);
                }
            }
            FreeMibTable(table.cast());
        }
    }
    if let Some(shared) = &next.telemetry
        && let Ok(mut counters) = shared.lock()
    {
        if counters.lost {
            next.errors
                .push("Disk/network event loss detected; per-process rates unavailable.".into());
        } else {
            for p in &mut next.processes {
                let (disk, network) = counters.bytes.get(&p.pid).copied().unwrap_or_default();
                p.disk_bytes = Some(disk);
                p.network_bytes = Some(network);
            }
        }
        let alive: std::collections::HashSet<_> = next.processes.iter().map(|p| p.pid).collect();
        counters.bytes.retain(|pid, _| alive.contains(pid));
    }
    (next.disk, next.disk_rate) = disk_sample();
    next.sampled = Instant::now();
    let elapsed = next.sampled.duration_since(previous.sampled).as_secs_f64();
    if previous.cpu_total > 0 && next.cpu_total >= previous.cpu_total {
        let total = next.cpu_total - previous.cpu_total;
        next.cpu = percent(
            total.saturating_sub(next.cpu_idle.saturating_sub(previous.cpu_idle)),
            total,
        );
        if elapsed > 0.0 && next.network_bytes >= previous.network_bytes {
            next.network_rate =
                Some((next.network_bytes - previous.network_bytes) as f64 / elapsed);
        }
        let prior: HashMap<_, _> = previous
            .processes
            .iter()
            .map(|p| ((p.pid, p.created), p))
            .collect();
        for row in &mut next.processes {
            if let Some(old) = prior
                .get(&(row.pid, row.created))
                .filter(|_| row.created.is_some())
            {
                row.cpu = percent(row.cpu_time.saturating_sub(old.cpu_time), total);
                row.disk_rate = row
                    .disk_bytes
                    .zip(old.disk_bytes)
                    .filter(|(n, o)| n >= o && elapsed > 0.0)
                    .map(|(n, o)| (n - o) as f64 / elapsed);
                row.network_rate = row
                    .network_bytes
                    .zip(old.network_bytes)
                    .filter(|(n, o)| n >= o && elapsed > 0.0)
                    .map(|(n, o)| (n - o) as f64 / elapsed);
                row.io_rate = row
                    .io_bytes
                    .zip(old.io_bytes)
                    .filter(|(n, o)| n >= o && elapsed > 0.0)
                    .map(|(n, o)| (n - o) as f64 / elapsed);
            }
        }
    }
    if inventory {
        match services() {
            Ok(v) => next.services = v,
            Err(e) => {
                next.services.clone_from(&previous.services);
                next.errors.push(format!("Services: {e}"));
            }
        }
        match users() {
            Ok(v) => next.users = v,
            Err(e) => {
                next.users.clone_from(&previous.users);
                next.errors.push(format!("Users: {e}"));
            }
        }
        match startups() {
            Ok(v) => next.startups = v,
            Err(e) => {
                next.startups.clone_from(&previous.startups);
                next.errors.push(format!("Startup: {e}"));
            }
        }
    } else {
        next.services.clone_from(&previous.services);
        next.users.clone_from(&previous.users);
        next.startups.clone_from(&previous.startups);
    }
    Ok(next)
}

// PDH owns counter history across sampling threads. Integer handles permit
// transfer under the mutex; no query is accessed concurrently.
struct DiskQuery {
    query: usize,
    idle: usize,
    bytes: usize,
}
impl Drop for DiskQuery {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Performance::PdhCloseQuery(
                windows::Win32::System::Performance::PDH_HQUERY(self.query as *mut _),
            );
        }
    }
}
impl DiskQuery {
    fn new() -> Option<Self> {
        use windows::Win32::System::Performance::*;
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut owner = Self {
                query: query.0 as usize,
                idle: 0,
                bytes: 0,
            };
            for (path, slot) in [
                (w!("\\PhysicalDisk(_Total)\\% Idle Time"), &mut owner.idle),
                (
                    w!("\\PhysicalDisk(_Total)\\Disk Bytes/sec"),
                    &mut owner.bytes,
                ),
            ] {
                let mut counter = PDH_HCOUNTER::default();
                if PdhAddEnglishCounterW(query, path, 0, &mut counter) != 0 {
                    return None;
                }
                *slot = counter.0 as usize;
            }
            PdhCollectQueryData(query);
            Some(owner)
        }
    }
    fn sample(&self) -> (Option<f64>, Option<f64>) {
        use windows::Win32::System::Performance::*;
        unsafe {
            if PdhCollectQueryData(PDH_HQUERY(self.query as *mut _)) != 0 {
                return (None, None);
            }
            fn value(handle: usize) -> Option<f64> {
                use windows::Win32::System::Performance::*;
                unsafe {
                    let mut value = PDH_FMT_COUNTERVALUE::default();
                    if PdhGetFormattedCounterValue(
                        PDH_HCOUNTER(handle as *mut _),
                        PDH_FMT_DOUBLE,
                        None,
                        &mut value,
                    ) != 0
                        || value.CStatus > 1
                    {
                        return None;
                    }
                    Some(value.Anonymous.doubleValue)
                }
            }
            (
                value(self.idle).map(|idle| (100.0 - idle).clamp(0.0, 100.0)),
                value(self.bytes),
            )
        }
    }
}
fn disk_sample() -> (Option<f64>, Option<f64>) {
    static QUERY: std::sync::OnceLock<std::sync::Mutex<Option<DiskQuery>>> =
        std::sync::OnceLock::new();
    let query = QUERY.get_or_init(|| std::sync::Mutex::new(DiskQuery::new()));
    query
        .lock()
        .ok()
        .and_then(|query| query.as_ref().map(DiskQuery::sample))
        .unwrap_or((None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_and_identity_guards() {
        let first = sample(&Snapshot::default(), true).unwrap();
        assert!(first.memory_total > 0);
        assert!(!first.services.is_empty());
        let row = first
            .processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap();
        assert!(row.created.is_some());
        assert!(termination_handle(row, &HashSet::new()).is_err());
        let second = sample(&first, false).unwrap();
        assert!(second.cpu.is_some_and(|n| (0.0..=100.0).contains(&n)));
    }
    #[test]
    fn end_task_rejects_recycled_identity() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut rows = processes(&[]).unwrap();
        let row = rows.iter_mut().find(|p| p.pid == child.id()).unwrap();
        let identity = row.created;
        row.created = identity.map(|v| v.wrapping_add(1));
        assert!(termination_handle(row, &HashSet::new()).is_err());
        row.created = identity;
        // Processes that run the Task Manager, and Agent images, are refused.
        assert!(termination_handle(row, &HashSet::from([child.id()])).is_err());
        let path = std::mem::replace(
            &mut row.path,
            std::env::current_exe().unwrap().display().to_string(),
        );
        let error = termination_handle(row, &HashSet::new()).err().unwrap();
        assert!(error.to_string().contains("part of the MeshRMM Agent"));
        row.path = path;
        let handle = termination_handle(row, &HashSet::new()).unwrap();
        unsafe {
            TerminateProcess(handle.0, 1).unwrap();
        }
        child.wait().unwrap();
    }
    #[test]
    fn ancestors_stop_at_recycled_parents() {
        let process = |pid, parent, created| Process {
            pid,
            parent,
            created: Some(created),
            ..Default::default()
        };
        let rows = [
            process(4, 0, 1),
            process(10, 4, 2),
            process(20, 10, 3),
            process(30, 20, 4),
            // Started after its child, so it reused the real parent's PID.
            process(50, 0, 9),
            process(60, 50, 5),
            // A PID that names itself as its parent.
            process(70, 70, 6),
            process(80, 70, 7),
        ];
        assert_eq!(ancestors(&rows, 30), HashSet::from([20, 10, 4]));
        assert_eq!(ancestors(&rows, 60), HashSet::new());
        assert_eq!(ancestors(&rows, 80), HashSet::from([70]));
        assert_eq!(ancestors(&rows, 99), HashSet::new());
    }
    #[test]
    fn agent_copies_are_recognized_by_content() {
        let agent = std::env::current_exe().unwrap();
        let directory = std::env::temp_dir().join(format!("meshrmm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let copy = directory.join("update-helper-test.exe");
        std::fs::copy(&agent, &copy).unwrap();
        let identical = is_agent_image(&copy.display().to_string());
        let mut bytes = std::fs::read(&copy).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&copy, bytes).unwrap();
        let changed = is_agent_image(&copy.display().to_string());
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(is_agent_image(&agent.display().to_string()));
        assert!(identical);
        assert!(!changed);
        assert!(!is_agent_image(r"C:\Windows\System32\cmd.exe"));
        assert!(!is_agent_image(""));
    }
    #[test]
    fn startup_toggle_preserves_command_and_rejects_stale_state() {
        let path = format!("Software\\MeshRMMTests\\{}", uuid::Uuid::new_v4());
        let key = wide(&path);
        let mut handle = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &mut handle,
                None,
            )
            .ok()
            .unwrap();
        }
        let owner = Key(handle);
        let mut row = Startup {
            name: "Fixture".into(),
            command: "notepad.exe".into(),
            enabled: true,
            run_key: format!("{path}\\Run"),
            approval_key: format!("{path}\\Approved"),
            ..Default::default()
        };
        let run = wide(&row.run_key);
        let name = wide(&row.name);
        let command = wide(&row.command);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(run.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                (command.len() * 2) as u32,
            )
            .ok()
            .unwrap();
        }
        let disabled = set_startup(HKEY_CURRENT_USER, &row);
        let stale = set_startup(HKEY_CURRENT_USER, &row);
        row.enabled = false;
        let enabled = set_startup(HKEY_CURRENT_USER, &row);
        let value = approval(HKEY_CURRENT_USER, &row.approval_key, &row.name);
        row.command = "changed".into();
        let changed = set_startup(HKEY_CURRENT_USER, &row);
        drop(owner);
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()))
                .ok()
                .unwrap();
        }
        disabled.unwrap();
        assert!(stale.is_err());
        enabled.unwrap();
        assert_eq!(value.unwrap()[0], 2);
        assert!(changed.is_err());
    }
    #[test]
    fn connection_service_is_protected() {
        assert!(service_action("MeshRMMAgent", ServiceAction::Stop).is_err());
        assert!(service_action("meshrmmagent", ServiceAction::Restart).is_err());
        assert!(service_action("PulseRMMAgent", ServiceAction::Stop).is_err());
    }
    #[test]
    #[ignore = "Requires a disposable MeshRMMTaskManagerTest service supplied by the validation harness"]
    fn disposable_service_start_restart_stop() {
        let name = std::env::var("MESHRMM_TASK_TEST_SERVICE").unwrap();
        assert!(name.starts_with("MeshRMMTaskManagerTest-"));
        fn wait(name: &str, state: u32) {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if services()
                    .unwrap()
                    .iter()
                    .any(|s| s.name == name && s.state == state)
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Service failed to reach state {state}"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        service_action(&name, ServiceAction::Start).unwrap();
        wait(&name, 4);
        service_action(&name, ServiceAction::Restart).unwrap();
        wait(&name, 4);
        service_action(&name, ServiceAction::Stop).unwrap();
        wait(&name, 1);
    }
    #[test]
    fn cpu_zero_interval_is_unavailable() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(5, 10), Some(50.0));
    }
}
