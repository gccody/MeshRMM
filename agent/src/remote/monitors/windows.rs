//! Monitors on Windows: the Service Control Manager for services, a
//! process snapshot for processes, the Event Log API for events, and the
//! ICMP API for ping, so nothing depends on a tool's localized output.
use std::collections::HashMap;
use std::ffi::OsStr;
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, ERROR_NO_MORE_ITEMS, FILETIME,
    HANDLE,
};
use windows::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, ICMPV6_ECHO_REPLY_LH, IP_SUCCESS, Icmp6CreateFile, Icmp6ParseReplies,
    Icmp6SendEcho2, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
};
use windows::Win32::Networking::WinSock::{
    AF_INET6, IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN6, SOCKADDR_IN6_0,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::EventLog::{
    EVT_HANDLE, EvtClose, EvtFormatMessage, EvtFormatMessageEvent, EvtNext,
    EvtOpenPublisherMetadata, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender,
    EvtRenderEventXml,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    TerminateProcess,
};
use windows::core::{HSTRING, PCWSTR};
use windows_service::service::{ServiceAccess, ServiceStartType, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::parse::{self, EventFilter, LogRead, ProcessSample, ServiceStatus};

const SERVICE_WAIT: Duration = Duration::from_secs(60);
/// The most events one check reads, newest first.
const MAX_EVENTS: usize = 1000;
/// Events whose message is read when the filter doesn't need every one's:
/// those a failing result lists.
const MESSAGES_LISTED: usize = 20;

fn manager() -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| anyhow::anyhow!("could not open the Service Control Manager: {error}"))
}

fn missing(error: &windows_service::Error) -> bool {
    matches!(error, windows_service::Error::Winapi(error) if error.raw_os_error() == Some(1060))
}

/// The service's key name, given its key name or display name; `None` when
/// there is no such service.
fn service_key(manager: &ServiceManager, name: &str) -> anyhow::Result<Option<String>> {
    match manager.open_service(name, ServiceAccess::QUERY_STATUS) {
        Ok(_) => return Ok(Some(name.to_owned())),
        Err(error) if missing(&error) => {}
        Err(error) => anyhow::bail!("could not open the service: {error}"),
    }
    match manager.service_name_from_display_name(name) {
        Ok(key) => Ok(Some(key.to_string_lossy().into_owned())),
        Err(_) => Ok(None),
    }
}

fn state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "starting",
        ServiceState::StopPending => "stopping",
        ServiceState::Running => "running",
        ServiceState::ContinuePending => "resuming",
        ServiceState::PausePending => "pausing",
        ServiceState::Paused => "paused",
    }
}

pub(super) fn service(name: &str) -> anyhow::Result<ServiceStatus> {
    let manager = manager()?;
    let Some(key) = service_key(&manager, name)? else {
        return Ok(ServiceStatus {
            found: false,
            running: false,
            automatic: None,
            state: "not installed".into(),
        });
    };
    let service = manager
        .open_service(
            &key,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        )
        .map_err(|error| anyhow::anyhow!("could not open the service: {error}"))?;
    let status = service
        .query_status()
        .map_err(|error| anyhow::anyhow!("could not read the service's status: {error}"))?;
    let config = service
        .query_config()
        .map_err(|error| anyhow::anyhow!("could not read the service's configuration: {error}"))?;
    Ok(ServiceStatus {
        found: true,
        running: status.current_state == ServiceState::Running,
        automatic: Some(config.start_type == ServiceStartType::AutoStart),
        state: state_name(status.current_state).into(),
    })
}

fn wait_for(
    service: &windows_service::service::Service,
    wanted: ServiceState,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + SERVICE_WAIT;
    loop {
        let state = service.query_status()?.current_state;
        if state == wanted {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the service was still {} after {} seconds",
            state_name(state),
            SERVICE_WAIT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn restart_service(name: &str) -> anyhow::Result<()> {
    let manager = manager()?;
    let key =
        service_key(&manager, name)?.ok_or_else(|| anyhow::anyhow!("“{name}” isn't installed"))?;
    anyhow::ensure!(
        !crate::service::is_agent_service(&key),
        "The Agent doesn't restart its own service."
    );
    let service = manager
        .open_service(
            &key,
            ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP,
        )
        .map_err(|error| anyhow::anyhow!("could not open the service: {error}"))?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop().map_err(|error| match &error {
            windows_service::Error::Winapi(inner) if inner.raw_os_error() == Some(1051) => {
                anyhow::anyhow!("other running services depend on it")
            }
            _ => anyhow::anyhow!("could not stop the service: {error}"),
        })?;
        wait_for(&service, ServiceState::Stopped)?;
    }
    service
        .start(&[] as &[&OsStr])
        .map_err(|error| anyhow::anyhow!("could not start the service: {error}"))?;
    wait_for(&service, ServiceState::Running)
}

pub(super) fn is_agent_service(name: &str) -> bool {
    crate::service::is_agent_service(name)
        || manager()
            .ok()
            .and_then(|manager| service_key(&manager, name).ok().flatten())
            .is_some_and(|key| crate::service::is_agent_service(&key))
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn filetime_ms(time: FILETIME) -> u64 {
    ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) / 10_000
}

/// Every process's ID and file name.
fn snapshot() -> anyhow::Result<Vec<(u32, String)>> {
    // SAFETY: the snapshot handle is closed on drop, and the entry is sized.
    unsafe {
        let snapshot = Owned(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Vec::new();
        Process32FirstW(snapshot.0, &mut entry)?;
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            found.push((
                entry.th32ProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..end]),
            ));
            if let Err(error) = Process32NextW(snapshot.0, &mut entry) {
                if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                    return Err(error.into());
                }
                break;
            }
        }
        Ok(found)
    }
}

/// The processes called `name`, with or without `.exe`, ignoring case.
/// Protected processes count, with no CPU or memory use.
pub(super) fn processes(name: &str) -> anyhow::Result<Vec<ProcessSample>> {
    Ok(snapshot()?
        .into_iter()
        .filter(|(_, exe)| parse::process_name_matches(exe, name, true))
        .map(|(pid, exe)| {
            let mut sample = ProcessSample {
                pid,
                names: vec![exe],
                cpu_ms: 0,
                memory_bytes: 0,
            };
            // SAFETY: the handle is closed on drop; the outputs are sized.
            unsafe {
                if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    let handle = Owned(handle);
                    let (mut created, mut exited, mut kernel, mut user) = Default::default();
                    if GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user)
                        .is_ok()
                    {
                        sample.cpu_ms = filetime_ms(kernel) + filetime_ms(user);
                    }
                    let mut counters = PROCESS_MEMORY_COUNTERS {
                        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                        ..Default::default()
                    };
                    if K32GetProcessMemoryInfo(handle.0, &mut counters, counters.cb).as_bool() {
                        sample.memory_bytes = counters.WorkingSetSize as u64;
                    }
                }
            }
            sample
        })
        .collect())
}

/// Processes Windows can't do without: ending one stops the computer.
pub(super) fn is_protected_process(name: &str) -> bool {
    [
        "system",
        "smss",
        "csrss",
        "wininit",
        "winlogon",
        "services",
        "lsass",
        "svchost",
        "meshrmm-agent",
        "pulsermm-agent",
    ]
    .iter()
    .any(|protected| parse::process_name_matches(name, protected, true))
}

pub(super) fn end_processes(name: &str) -> anyhow::Result<usize> {
    let own = std::process::id();
    let mut ended = 0;
    let mut failure = None;
    for process in processes(name)? {
        if process.pid == own || process.pid <= 4 {
            continue;
        }
        // SAFETY: the handle is closed on drop.
        let result = unsafe {
            OpenProcess(PROCESS_TERMINATE, false, process.pid)
                .and_then(|handle| TerminateProcess(Owned(handle).0, 1))
        };
        match result {
            Ok(()) => ended += 1,
            Err(error) => failure = Some(error),
        }
    }
    if ended == 0
        && let Some(error) = failure
    {
        anyhow::bail!("could not end it: {}", error.message());
    }
    Ok(ended)
}

struct Event(EVT_HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        let _ = unsafe { EvtClose(self.0) };
    }
}

/// An event's XML.
fn render(event: &Event) -> anyhow::Result<String> {
    let mut buffer = vec![0u16; 4096];
    loop {
        let (mut used, mut properties) = (0, 0);
        // SAFETY: the buffer's size is given in bytes.
        let rendered = unsafe {
            EvtRender(
                None,
                event.0,
                EvtRenderEventXml.0,
                (buffer.len() * 2) as u32,
                Some(buffer.as_mut_ptr().cast()),
                &mut used,
                &mut properties,
            )
        };
        match rendered {
            Ok(()) => {
                let length = (used as usize / 2).min(buffer.len());
                let text = String::from_utf16_lossy(&buffer[..length]);
                return Ok(text.trim_end_matches('\0').to_owned());
            }
            Err(error) if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {
                buffer = vec![0; used as usize / 2 + 1];
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// An event's message, as Event Viewer shows it, from its publisher.
fn message(publisher: &Event, event: &Event) -> Option<String> {
    let mut buffer = vec![0u16; 2048];
    loop {
        let mut used = 0;
        // SAFETY: the buffer's size is given in characters.
        let formatted = unsafe {
            EvtFormatMessage(
                Some(publisher.0),
                Some(event.0),
                0,
                None,
                EvtFormatMessageEvent.0,
                Some(&mut buffer),
                &mut used,
            )
        };
        match formatted {
            Ok(()) => {
                let length = (used as usize).min(buffer.len());
                return Some(
                    String::from_utf16_lossy(&buffer[..length])
                        .trim_end_matches('\0')
                        .to_owned(),
                );
            }
            Err(error)
                if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult()
                    && used as usize > buffer.len() =>
            {
                buffer = vec![0; used as usize + 1];
            }
            Err(_) => return None,
        }
    }
}

fn provider_name(xml: &str) -> Option<String> {
    let start = xml.find("<Provider ")?;
    let tag = &xml[start..start + xml[start..].find('>')?];
    let at = tag.find(" Name=")? + 6;
    let quote = tag[at..].chars().next()?;
    let value = &tag[at + 1..];
    Some(value[..value.find(quote)?].to_owned())
}

pub(super) fn events(
    log: &str,
    filter: &EventFilter,
    since_ms: i64,
    now_ms: i64,
    timeout: Duration,
) -> anyhow::Result<LogRead> {
    let deadline = Instant::now() + timeout;
    let age_ms = now_ms.saturating_sub(since_ms).max(1000) as u64;
    let query = parse::windows_query(filter, age_ms);
    // SAFETY: the result set is closed on drop.
    let results = Event(
        unsafe {
            EvtQuery(
                None,
                &HSTRING::from(log),
                &HSTRING::from(query),
                EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
            )
        }
        .map_err(|error| anyhow::anyhow!("could not read the “{log}” log: {}", error.message()))?,
    );
    let mut publishers: HashMap<String, Option<Event>> = HashMap::new();
    let mut found = Vec::new();
    'batches: while found.len() < MAX_EVENTS {
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!left.is_zero(), "reading the log took too long");
        let mut handles = [0isize; 64];
        let mut returned = 0;
        // SAFETY: the handles returned are each closed below.
        let next = unsafe {
            EvtNext(
                results.0,
                &mut handles,
                left.as_millis().min(u128::from(u32::MAX)) as u32,
                0,
                &mut returned,
            )
        };
        if let Err(error) = next {
            if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() {
                break;
            }
            anyhow::bail!("could not read the log: {}", error.message());
        }
        let events = handles[..returned as usize]
            .iter()
            .map(|handle| Event(EVT_HANDLE(*handle)))
            .collect::<Vec<_>>();
        for event in events {
            if found.len() >= MAX_EVENTS {
                break 'batches;
            }
            let Ok(xml) = render(&event) else { continue };
            let wants_message = !filter.message.is_empty() || found.len() < MESSAGES_LISTED;
            let text = wants_message
                .then(|| {
                    let provider = provider_name(&xml)?;
                    let publisher = publishers.entry(provider.clone()).or_insert_with(|| {
                        // SAFETY: the metadata handle is closed on drop.
                        unsafe {
                            EvtOpenPublisherMetadata(
                                None,
                                &HSTRING::from(provider),
                                PCWSTR::null(),
                                0,
                                0,
                            )
                        }
                        .ok()
                        .map(Event)
                    });
                    message(publisher.as_ref()?, &event)
                })
                .flatten();
            if let Some(parsed) = parse::windows_event(&xml, text)
                && filter.matches(&parsed)
            {
                found.push(parsed);
            }
        }
    }
    Ok(LogRead {
        truncated: found.len() >= MAX_EVENTS,
        events: found,
    })
}

pub(super) fn ping(host: &str, timeout: Duration) -> anyhow::Result<Option<f64>> {
    let address = (host, 0)
        .to_socket_addrs()
        .map_err(|error| anyhow::anyhow!("could not look up “{host}”: {error}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("“{host}” has no address"))?;
    let timeout_ms = timeout.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
    let data = [0x61u8; 32];
    // Room for the reply, its data, and an ICMP error.
    let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 8 + 64];
    // SAFETY: the handles are closed below; the buffers are sized.
    unsafe {
        match address {
            std::net::SocketAddr::V4(v4) => {
                let handle = IcmpCreateFile()?;
                let count = IcmpSendEcho(
                    handle,
                    u32::from_ne_bytes(v4.ip().octets()),
                    data.as_ptr().cast(),
                    data.len() as u16,
                    None,
                    reply.as_mut_ptr().cast(),
                    reply.len() as u32,
                    timeout_ms,
                );
                let _ = IcmpCloseHandle(handle);
                if count == 0 {
                    return Ok(None);
                }
                let echo = reply.as_ptr().cast::<ICMP_ECHO_REPLY>().read_unaligned();
                Ok((echo.Status == IP_SUCCESS).then(|| f64::from(echo.RoundTripTime)))
            }
            std::net::SocketAddr::V6(v6) => {
                let handle = Icmp6CreateFile()?;
                let source = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    ..Default::default()
                };
                let destination = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: v6.ip().octets(),
                        },
                    },
                    Anonymous: SOCKADDR_IN6_0 {
                        sin6_scope_id: v6.scope_id(),
                    },
                    ..Default::default()
                };
                let count = Icmp6SendEcho2(
                    handle,
                    None,
                    None,
                    None,
                    &source,
                    &destination,
                    data.as_ptr().cast(),
                    data.len() as u16,
                    None,
                    reply.as_mut_ptr().cast(),
                    reply.len() as u32,
                    timeout_ms,
                );
                let _ = IcmpCloseHandle(handle);
                if count == 0
                    || Icmp6ParseReplies(reply.as_mut_ptr().cast(), reply.len() as u32) == 0
                {
                    return Ok(None);
                }
                let echo = reply
                    .as_ptr()
                    .cast::<ICMPV6_ECHO_REPLY_LH>()
                    .read_unaligned();
                Ok((echo.Status == IP_SUCCESS).then(|| f64::from(echo.RoundTripTime)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_services_and_processes_are_seen() {
        // Read-only: the Event Log service always runs.
        let status = service("EventLog").unwrap();
        assert!(status.found && status.running, "{status:?}");
        assert_eq!(status.automatic, Some(true));
        let by_display_name = service("Windows Event Log").unwrap();
        assert!(by_display_name.found);
        assert!(!service("NoSuchMeshRmmService").unwrap().found);

        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_string_lossy().into_owned();
        let found = processes(&name).unwrap();
        let mine = found
            .iter()
            .find(|process| process.pid == std::process::id())
            .unwrap();
        assert!(mine.memory_bytes > 0);
        assert!(is_protected_process("LSASS.EXE"));
        assert!(!is_protected_process("notepad"));
        assert!(is_agent_service("MeshRMMAgent"));
    }

    #[test]
    fn the_event_log_is_queried() {
        let now = parse::parse_timestamp("2100-01-01T00:00:00Z").unwrap();
        let filter = EventFilter {
            levels: vec![
                meshrmm_protocol::EventLevel::Information,
                meshrmm_protocol::EventLevel::Error,
            ],
            ..EventFilter::default()
        };
        let since = super::super::now_ms() - 7 * 24 * 60 * 60 * 1000;
        let found = events(
            "System",
            &filter,
            since,
            super::super::now_ms(),
            Duration::from_secs(30),
        )
        .unwrap()
        .events;
        assert!(
            found
                .iter()
                .all(|event| event.at_ms >= since - 1000 && event.at_ms < now)
        );
        assert!(
            found.windows(2).all(|pair| pair[0].at_ms >= pair[1].at_ms),
            "newest first"
        );
        assert!(
            events(
                "No Such Log",
                &filter,
                since,
                super::super::now_ms(),
                Duration::from_secs(5)
            )
            .is_err()
        );
    }

    #[test]
    fn localhost_answers_pings() {
        assert!(ping("127.0.0.1", Duration::from_secs(2)).unwrap().is_some());
        assert!(ping("::1", Duration::from_secs(2)).unwrap().is_some());
    }
}
