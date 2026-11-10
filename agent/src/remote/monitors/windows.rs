//! Monitors on Windows: the Service Control Manager for services, a
//! process snapshot for processes, the Event Log API for events, and the
//! ICMP API for ping, so nothing depends on a tool's localized output.
use std::collections::HashMap;
use std::ffi::OsStr;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, ERROR_NO_MORE_ITEMS,
    ERROR_TIMEOUT, FILETIME, HANDLE,
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

use super::network;
use super::parse::{self, EventFilter, LogEvent, LogRead, ProcessSample, ServiceStatus};

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

/// The events of a query's result set, newest first.
fn query(log: &str, query: &str) -> anyhow::Result<Event> {
    // SAFETY: the result set is closed on drop.
    let results = unsafe {
        EvtQuery(
            None,
            &HSTRING::from(log),
            &HSTRING::from(query),
            EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
        )
    }
    .map_err(|error| anyhow::anyhow!("could not read the “{log}” log: {}", error.message()))?;
    Ok(Event(results))
}

/// What one call for a result set's next events got.
enum Batch {
    Events(Vec<Event>),
    End,
    OutOfTime,
}

/// Up to `count` more events of `results`, waiting at most `left`.
fn next_batch(results: &Event, count: usize, left: Duration) -> anyhow::Result<Batch> {
    let mut handles = vec![0isize; count];
    let mut returned = 0;
    // SAFETY: the handles returned are each closed on drop, below.
    let next = unsafe {
        EvtNext(
            results.0,
            &mut handles,
            left.as_millis().clamp(1, u128::from(u32::MAX)) as u32,
            0,
            &mut returned,
        )
    };
    match next {
        Ok(()) => Ok(Batch::Events(
            handles
                .iter()
                .take(returned as usize)
                .map(|handle| Event(EVT_HANDLE(*handle)))
                .collect(),
        )),
        Err(error) if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() => Ok(Batch::End),
        Err(error) if error.code() == ERROR_TIMEOUT.to_hresult() => Ok(Batch::OutOfTime),
        Err(error) => anyhow::bail!("could not read the log: {}", error.message()),
    }
}

/// What reading a log needs of the Event Log, so that the reading can be
/// tested without one.
trait Records {
    type Record;
    /// The number of the log's newest record; `None` when it has none.
    fn newest(&mut self) -> anyhow::Result<Option<u64>>;
    /// Starts reading the records the filter's query selects, newest first.
    fn select(&mut self, filter: &EventFilter, records: parse::Records) -> anyhow::Result<()>;
    /// How long the check has left.
    fn left(&self) -> Duration;
    /// The next record selected, waiting at most `left`.
    fn next(&mut self, left: Duration) -> anyhow::Result<Next<Self::Record>>;
    /// The record's event, with the message its own XML holds: the rendered
    /// one, else its data.
    fn event(&self, record: &Self::Record) -> Option<LogEvent>;
    /// The record's message as Event Viewer shows it, from its publisher,
    /// which is slow; `None` when its XML holds it already or the publisher
    /// can't say.
    fn message(&mut self, record: &Self::Record) -> Option<String>;
}

enum Next<R> {
    Record(R),
    End,
    OutOfTime,
}

/// Reads the records written to the log since `log_record`, the newest one
/// an earlier check read up to; with none, or when the log was cleared
/// since (it numbers its records from 1 again), those logged since
/// `since_ms`. Newest first, until the check's time is up: what it found by
/// then counts, and the read says it ran out of time. `log_record` becomes
/// the newest record when it started, so the next check reads only what
/// was written after, and a log with nothing new costs one record's read.
fn read_records<R: Records>(
    records: &mut R,
    filter: &EventFilter,
    since_ms: i64,
    now_ms: i64,
    log_record: &mut Option<u64>,
) -> anyhow::Result<LogRead> {
    let Some(newest) = records.newest()? else {
        *log_record = None;
        return Ok(LogRead::default());
    };
    let after = log_record.filter(|after| *after <= newest);
    if after == Some(newest) {
        return Ok(LogRead::default());
    }
    records.select(
        filter,
        parse::Records {
            after,
            through: newest,
            age_ms: now_ms.saturating_sub(since_ms).max(1000) as u64,
        },
    )?;
    // The query has the source, event IDs and levels; checking them again
    // costs nothing, and spares a message for an event they don't match.
    let unworded = EventFilter {
        message: String::new(),
        ..filter.clone()
    };
    let mut read = LogRead::default();
    while read.events.len() < MAX_EVENTS {
        let left = records.left();
        if left.is_zero() {
            read.out_of_time = true;
            break;
        }
        let record = match records.next(left)? {
            Next::Record(record) => record,
            Next::End => break,
            Next::OutOfTime => {
                read.out_of_time = true;
                break;
            }
        };
        let Some(mut event) = records.event(&record) else {
            continue;
        };
        if !unworded.matches(&event) {
            continue;
        }
        if (!filter.message.is_empty() || read.events.len() < MESSAGES_LISTED)
            && let Some(message) = records.message(&record)
        {
            event.message = message.trim().to_owned();
        }
        if filter.matches(&event) {
            read.events.push(event);
        }
    }
    read.truncated = read.events.len() >= MAX_EVENTS;
    // Reverse direction is record order, which isn't strictly time order:
    // an event can be written with an earlier time than the one before it.
    read.events
        .sort_by_key(|event| std::cmp::Reverse(event.at_ms));
    *log_record = Some(newest);
    Ok(read)
}

/// A log, read through the Event Log API until `deadline`.
struct EventLog<'a> {
    log: &'a str,
    deadline: Instant,
    results: Option<Event>,
    batch: std::vec::IntoIter<Event>,
    publishers: HashMap<String, Option<Event>>,
}

/// An event and its XML.
struct Rendered {
    event: Event,
    xml: String,
}

impl Records for EventLog<'_> {
    type Record = Rendered;

    fn newest(&mut self) -> anyhow::Result<Option<u64>> {
        let results = query(self.log, "*")?;
        let event = match next_batch(&results, 1, self.left())? {
            Batch::Events(events) => events.into_iter().next(),
            Batch::End => None,
            Batch::OutOfTime => anyhow::bail!("reading the log took too long"),
        };
        let Some(event) = event else {
            return Ok(None);
        };
        let xml = render(&event)?;
        parse::windows_record(&xml)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("the log's newest record has no number"))
    }

    fn select(&mut self, filter: &EventFilter, records: parse::Records) -> anyhow::Result<()> {
        self.results = Some(query(self.log, &parse::windows_query(filter, records))?);
        Ok(())
    }

    fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn next(&mut self, left: Duration) -> anyhow::Result<Next<Rendered>> {
        loop {
            for event in self.batch.by_ref() {
                if let Ok(xml) = render(&event) {
                    return Ok(Next::Record(Rendered { event, xml }));
                }
            }
            let Some(results) = &self.results else {
                return Ok(Next::End);
            };
            match next_batch(results, 64, left)? {
                Batch::Events(events) => self.batch = events.into_iter(),
                Batch::End => return Ok(Next::End),
                Batch::OutOfTime => return Ok(Next::OutOfTime),
            }
        }
    }

    fn event(&self, record: &Rendered) -> Option<LogEvent> {
        parse::windows_event(&record.xml, None)
    }

    fn message(&mut self, record: &Rendered) -> Option<String> {
        if record.xml.contains("<RenderingInfo") {
            return None;
        }
        let provider = parse::windows_provider(&record.xml)?;
        let publisher = self.publishers.entry(provider.clone()).or_insert_with(|| {
            // SAFETY: the metadata handle is closed on drop.
            unsafe {
                EvtOpenPublisherMetadata(None, &HSTRING::from(provider), PCWSTR::null(), 0, 0)
            }
            .ok()
            .map(Event)
        });
        message(publisher.as_ref()?, &record.event)
    }
}

pub(super) fn events(
    log: &str,
    filter: &EventFilter,
    since_ms: i64,
    now_ms: i64,
    timeout: Duration,
    log_record: &mut Option<u64>,
) -> anyhow::Result<LogRead> {
    let mut records = EventLog {
        log,
        deadline: Instant::now() + timeout,
        results: None,
        batch: Vec::new().into_iter(),
        publishers: HashMap::new(),
    };
    read_records(&mut records, filter, since_ms, now_ms, log_record)
}

pub(super) fn ping(host: &str, timeout: Duration) -> anyhow::Result<Option<f64>> {
    // The lookup counts against the timeout, which the echo has what's left of.
    let deadline = Instant::now() + timeout;
    let address = network::resolve(host, 0, deadline)?[0];
    let left = deadline.saturating_duration_since(Instant::now());
    let timeout_ms = left.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
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
        let mut log_record = None;
        let found = events(
            "System",
            &filter,
            since,
            super::super::now_ms(),
            Duration::from_secs(30),
            &mut log_record,
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
        assert!(log_record.is_some(), "the System log is never empty");
        assert!(
            events(
                "No Such Log",
                &filter,
                since,
                super::super::now_ms(),
                Duration::from_secs(5),
                &mut None,
            )
            .is_err()
        );
    }

    /// Windows refuses a query that nests deeper than 23, which 23 event
    /// IDs in a plain list and a time did; a rule may have 32.
    #[test]
    fn the_event_log_takes_a_query_with_every_filter_and_32_event_ids() {
        let filter = EventFilter {
            source: "MeshRMM test source".into(),
            event_ids: (0..32).map(|id| 1000 + id * 7).collect(),
            levels: meshrmm_protocol::EventLevel::ALL.to_vec(),
            message: "no such message".into(),
        };
        let now = super::super::now_ms();
        let mut log_record = None;
        // The first read selects by time, ...
        let first = events(
            "Application",
            &filter,
            now - 60 * 60 * 1000,
            now,
            Duration::from_secs(30),
            &mut log_record,
        )
        .unwrap();
        assert!(first.events.is_empty() && !first.out_of_time);
        // ... later ones by record number.
        let newest = log_record.expect("the Application log is never empty");
        log_record = Some(newest.saturating_sub(500));
        let later = events(
            "Application",
            &filter,
            now,
            now,
            Duration::from_secs(30),
            &mut log_record,
        )
        .unwrap();
        assert!(later.events.is_empty());
        assert!(log_record >= Some(newest));
    }

    /// A log in memory: its records, oldest first, the time the check has
    /// left, and how much of it each message takes.
    struct Fake {
        records: Vec<(u64, LogEvent)>,
        now_ms: i64,
        selected: Vec<(u64, LogEvent)>,
        left_ms: u64,
        message_ms: u64,
        selects: usize,
        returned: Vec<u64>,
        messages: usize,
    }

    impl Fake {
        fn new(now_ms: i64) -> Self {
            Self {
                records: Vec::new(),
                now_ms,
                selected: Vec::new(),
                left_ms: 0,
                message_ms: 10,
                selects: 0,
                returned: Vec::new(),
                messages: 0,
            }
        }

        /// Writes an event of `level` at `at_ms`, whose message is `text`.
        fn write(&mut self, at_ms: i64, level: meshrmm_protocol::EventLevel, text: &str) {
            let number = self.records.last().map_or(1, |(number, _)| number + 1);
            self.records.push((
                number,
                LogEvent {
                    at_ms,
                    level: Some(level),
                    sources: vec!["Disk".into()],
                    event_id: Some(7),
                    message: text.into(),
                },
            ));
        }

        /// A check with `left_ms` to read in.
        fn check(
            &mut self,
            filter: &EventFilter,
            since_ms: i64,
            left_ms: u64,
            log_record: &mut Option<u64>,
        ) -> LogRead {
            self.left_ms = left_ms;
            (self.selects, self.messages) = (0, 0);
            self.returned.clear();
            let now = self.now_ms;
            read_records(self, filter, since_ms, now, log_record).unwrap()
        }
    }

    impl Records for Fake {
        type Record = (u64, LogEvent);

        fn newest(&mut self) -> anyhow::Result<Option<u64>> {
            Ok(self.records.last().map(|(number, _)| *number))
        }

        /// Selects by number or time; the rest of the filter is left to
        /// the reading.
        fn select(&mut self, _: &EventFilter, records: parse::Records) -> anyhow::Result<()> {
            self.selects += 1;
            let since = self.now_ms - records.age_ms as i64;
            self.selected = self
                .records
                .iter()
                .rev()
                .filter(|(number, event)| {
                    *number <= records.through
                        && records
                            .after
                            .map_or(event.at_ms >= since, |after| *number > after)
                })
                .cloned()
                .collect();
            Ok(())
        }

        fn left(&self) -> Duration {
            Duration::from_millis(self.left_ms)
        }

        fn next(&mut self, _: Duration) -> anyhow::Result<Next<(u64, LogEvent)>> {
            if self.selected.is_empty() {
                return Ok(Next::End);
            }
            let record = self.selected.remove(0);
            self.returned.push(record.0);
            Ok(Next::Record(record))
        }

        /// Its message only from its publisher.
        fn event(&self, (_, event): &(u64, LogEvent)) -> Option<LogEvent> {
            Some(LogEvent {
                message: String::new(),
                ..event.clone()
            })
        }

        fn message(&mut self, (_, event): &(u64, LogEvent)) -> Option<String> {
            self.messages += 1;
            self.left_ms = self.left_ms.saturating_sub(self.message_ms);
            Some(event.message.clone())
        }
    }

    #[test]
    fn a_log_too_busy_to_search_in_time_keeps_what_it_found_and_reads_on() {
        use meshrmm_protocol::EventLevel::{Error, Warning};
        let now = 1_000_000_000;
        let mut log = Fake::new(now);
        for number in 1..=100 {
            let text = if number % 10 == 0 {
                "Disk D: failed"
            } else {
                "All well"
            };
            log.write(now - 1000 + number, Error, text);
        }
        let filter = EventFilter {
            levels: vec![Error],
            message: "disk d".into(),
            ..EventFilter::default()
        };
        let mut log_record = None;

        // Time for the newest 30 messages: what they hold counts.
        let read = log.check(&filter, now - 60_000, 300, &mut log_record);
        assert!(read.out_of_time && !read.truncated);
        assert_eq!(
            read.events
                .iter()
                .map(|event| event.at_ms)
                .collect::<Vec<_>>(),
            [now - 900, now - 910, now - 920],
            "newest first"
        );
        assert_eq!(log.messages, 30);
        assert_eq!(log_record, Some(100));

        // The next check reads only what was written since, whatever the
        // events' times say, and none of the levels it doesn't ask for
        // costs a message.
        log.write(now - 5000, Error, "Disk D: failed again");
        log.write(now, Warning, "Disk D: slow");
        log.write(now, Error, "All well");
        let read = log.check(&filter, now - 1, 1000, &mut log_record);
        assert_eq!(log.returned, [103, 102, 101], "nothing read twice");
        assert_eq!(log.messages, 2);
        assert!(!read.out_of_time);
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].message, "Disk D: failed again");
        assert_eq!(log_record, Some(103));

        // Nothing new: nothing is selected or formatted.
        let read = log.check(&filter, now - 1, 1000, &mut log_record);
        assert_eq!(read, LogRead::default());
        assert_eq!((log.selects, log.messages), (0, 0));
        assert_eq!(log_record, Some(103));

        // A check with no time left says so, with nothing found.
        log.write(now, Error, "Disk D: failed");
        let read = log.check(&filter, now - 1, 0, &mut log_record);
        assert!(read.out_of_time && read.events.is_empty());
        assert_eq!(log_record, Some(104), "the next one starts after it");
    }

    #[test]
    fn a_read_is_bounded_by_its_window_and_formats_only_what_it_lists() {
        use meshrmm_protocol::EventLevel::Error;
        let now = 1_000_000_000;
        let mut log = Fake::new(now);
        log.write(now - 120_000, Error, "Too old");
        for number in 0..50 {
            log.write(now - 1000 + number, Error, "Disk D: failed");
        }
        // The first check reads the window back from now, and without a
        // message to search for, formats only the messages a result lists.
        let mut log_record = None;
        let read = log.check(
            &EventFilter::default(),
            now - 60_000,
            10_000,
            &mut log_record,
        );
        assert_eq!(read.events.len(), 50);
        assert_eq!(log.messages, MESSAGES_LISTED);
        assert!(!read.out_of_time && !read.truncated);
        assert_eq!(log_record, Some(51));

        // A cleared log numbers its records from 1 again: it is read by
        // time again, not skipped until its numbers pass the last one.
        log.records.clear();
        log.write(now - 120_000, Error, "Too old");
        log.write(now - 10, Error, "Disk D: failed");
        let read = log.check(
            &EventFilter::default(),
            now - 60_000,
            10_000,
            &mut log_record,
        );
        assert_eq!(log.returned, [2]);
        assert_eq!(read.events.len(), 1);
        assert_eq!(log_record, Some(2));

        // An empty log has nothing to read.
        log.records.clear();
        let read = log.check(
            &EventFilter::default(),
            now - 60_000,
            10_000,
            &mut log_record,
        );
        assert_eq!((read.events.len(), log.selects), (0, 0));
        assert_eq!(log_record, None);
    }

    #[test]
    fn localhost_answers_pings() {
        assert!(ping("127.0.0.1", Duration::from_secs(2)).unwrap().is_some());
        assert!(ping("::1", Duration::from_secs(2)).unwrap().is_some());
    }
}
