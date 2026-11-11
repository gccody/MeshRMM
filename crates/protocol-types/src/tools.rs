//! Remote tools: a device's processes and services, listed and changed from
//! the website without a remote session. See docs/remote-tools.md.
//!
//! Like the file browser's listings, each request is small and goes over
//! the control connection: the server sends
//! [`crate::AgentCommand::ToolRequest`] and the Agent answers with
//! [`crate::AgentStatusMessage::ToolReply`]. Every tool runs as the Agent's
//! own account, SYSTEM or root. A list is sent whole, so the website can
//! sort, filter and draw a tree of it, but bounded ([`ProcessList::bound`],
//! [`ServiceList::bound`]) to well within what one control message carries.
use serde::{Deserialize, Serialize};

use crate::{truncate_text, valid_launchd_label, valid_unit_name};

/// The most processes one list carries.
pub const MAX_LISTED_PROCESSES: usize = 5000;
/// The longest command line a list carries for one process, in bytes.
pub const MAX_COMMAND_LINE_BYTES: usize = 2048;
/// The most bytes of command lines and program paths one list carries in
/// all; past it, the rest are left out.
pub const MAX_PROCESS_TEXT_BYTES: usize = 1024 * 1024;
/// The most processes one request ends.
pub const MAX_ENDED_PROCESSES: usize = 256;
/// The most services one list carries.
pub const MAX_LISTED_SERVICES: usize = 5000;
/// The longest service description a list carries, in bytes.
pub const MAX_SERVICE_DESCRIPTION_BYTES: usize = 512;
/// The longest service name either side accepts, in bytes.
pub const MAX_SERVICE_NAME_BYTES: usize = 256;
/// The longest name, user, account or display name a list carries, in
/// bytes.
const MAX_LABEL_BYTES: usize = 256;
/// The longest program path a list carries, in bytes.
const MAX_PATH_BYTES: usize = 1024;

/// What the server asks the Agent to do with the device's processes or
/// services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub request_id: String,
    /// How long the server waits for the answer, in milliseconds, which the
    /// Agent counts from the request's arrival on its monotonic clock, as
    /// for [`crate::FileRequest::timeout_ms`].
    pub timeout_ms: u64,
    pub operation: ToolOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToolOperation {
    ListProcesses,
    /// Ends each of `processes`, children before their parents. Each is
    /// ended only if a process with its ID still runs that started when it
    /// says, so an ID the system gave another process since is left alone.
    EndProcesses {
        processes: Vec<ProcessTarget>,
    },
    ListServices,
    /// Starts, stops or restarts the service `name`.
    ControlService {
        name: String,
        action: ServiceAction,
    },
    /// Sets how the service `name` starts.
    SetServiceStartup {
        name: String,
        startup: ServiceStartup,
    },
}

impl ToolOperation {
    /// The operation's name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ListProcesses => "list_processes",
            Self::EndProcesses { .. } => "end_processes",
            Self::ListServices => "list_services",
            Self::ControlService { .. } => "control_service",
            Self::SetServiceStartup { .. } => "set_service_startup",
        }
    }

    /// Whether it may take a while on the device: a service waits to start
    /// or stop, and an ended process gets a few seconds to exit.
    pub fn slow(&self) -> bool {
        matches!(
            self,
            Self::EndProcesses { .. }
                | Self::ControlService { .. }
                | Self::SetServiceStartup { .. }
        )
    }
}

/// A process to end, as a list showed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessTarget {
    pub pid: u32,
    /// When it started, in milliseconds since the Unix epoch, as the list
    /// said. `None` ends whatever has the ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    /// The process that started it, if it is known and not the kernel's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pid: Option<u32>,
    /// Its program's file name, such as `svchost.exe` or `sshd`.
    pub name: String,
    /// Its program's full path, where the system says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The account it runs as, such as `NT AUTHORITY\SYSTEM` or `root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Its share of the whole computer's processor time since the Agent's
    /// previous reading, 0 to 100, as Task Manager shows it.
    pub cpu_percent: f64,
    /// Its working set on Windows, its resident memory elsewhere.
    pub memory_bytes: u64,
    /// When it started, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
    /// The command line was cut short, or left out to keep the list small.
    #[serde(default, skip_serializing_if = "is_false")]
    pub command_line_truncated: bool,
    /// Its Windows session: 0 for services, 1 and up for signed-in users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessList {
    pub processes: Vec<ProcessInfo>,
    /// When it was read, in milliseconds since the Unix epoch.
    pub read_at: i64,
    /// The processors the CPU share is out of.
    pub logical_cpus: u32,
    /// The computer's memory, for the website to show each process's share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_bytes: Option<u64>,
    /// More processes run than a list carries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// `text` cut to `max` bytes at a character boundary, without control
/// characters, and whether anything was cut.
fn bounded(text: &str, max: usize) -> (String, bool) {
    let clean: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let kept = truncate_text(&clean, max);
    let cut = kept.len() < clean.len();
    (kept.to_owned(), cut)
}

fn bound_label(text: &mut String) {
    *text = bounded(text, MAX_LABEL_BYTES).0;
}

impl ProcessList {
    /// Keeps the list within one control message: at most
    /// [`MAX_LISTED_PROCESSES`], each command line cut to
    /// [`MAX_COMMAND_LINE_BYTES`], and the command lines and paths of all of
    /// them together within [`MAX_PROCESS_TEXT_BYTES`], in the order listed.
    pub fn bound(&mut self) {
        if self.processes.len() > MAX_LISTED_PROCESSES {
            self.processes.truncate(MAX_LISTED_PROCESSES);
            self.truncated = true;
        }
        let mut budget = MAX_PROCESS_TEXT_BYTES;
        for process in &mut self.processes {
            bound_label(&mut process.name);
            if let Some(user) = &mut process.user {
                bound_label(user);
            }
            if let Some(path) = process.path.take() {
                let (path, _) = bounded(&path, MAX_PATH_BYTES);
                if path.len() <= budget {
                    budget -= path.len();
                    process.path = Some(path);
                }
            }
            if let Some(line) = process.command_line.take() {
                let (line, cut) = bounded(&line, MAX_COMMAND_LINE_BYTES);
                process.command_line_truncated |= cut;
                if line.len() <= budget {
                    budget -= line.len();
                    process.command_line = Some(line);
                } else {
                    process.command_line_truncated = true;
                }
            }
            if !process.cpu_percent.is_finite() {
                process.cpu_percent = 0.0;
            }
            process.cpu_percent = process.cpu_percent.clamp(0.0, 100.0);
        }
    }
}

/// What ending processes did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessesEnded {
    pub ended: Vec<EndedProcess>,
    pub failed: Vec<ProcessFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndedProcess {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessFailure {
    pub pid: u32,
    /// Its name, when it was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub error: ToolError,
}

/// What a service manager says a service is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Running,
    Stopped,
    Starting,
    Stopping,
    Paused,
    /// Stopped after failing: systemd's `failed`, or a launchd job whose
    /// last run exited with an error.
    Failed,
    Other,
}

/// How a service starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStartup {
    /// With the computer: Windows' Automatic, an enabled systemd unit, an
    /// enabled launchd job.
    Automatic,
    /// Windows' Automatic (Delayed Start).
    AutomaticDelayed,
    /// When something asks for it: Windows' Manual, a disabled systemd
    /// unit, which can still be started.
    Manual,
    /// Never: Windows' Disabled, a masked systemd unit, a disabled launchd
    /// job.
    Disabled,
    /// Started only by other units (systemd's `static` and the like), which
    /// can't be changed.
    Static,
}

impl ServiceStartup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::AutomaticDelayed => "automatic_delayed",
            Self::Manual => "manual",
            Self::Disabled => "disabled",
            Self::Static => "static",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl ServiceAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// The name the service manager knows it by: a Windows service's key
    /// name, a systemd unit, a launchd label.
    pub name: String,
    /// Windows' display name; the unit or label elsewhere.
    pub display_name: String,
    pub state: ServiceState,
    /// The state in the manager's own words, such as `active (exited)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<ServiceStartup>,
    /// How it starts in the manager's own words, such as `masked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_detail: Option<String>,
    /// The ways it may be set to start; empty when it can't be changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup_choices: Vec<ServiceStartup>,
    /// The account it runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its main process, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The Agent's own service, which the tools don't stop or change.
    #[serde(default, skip_serializing_if = "is_false")]
    pub protected: bool,
}

impl ServiceInfo {
    fn bound(&mut self) {
        self.name = bounded(&self.name, MAX_SERVICE_NAME_BYTES).0;
        bound_label(&mut self.display_name);
        for text in [
            &mut self.state_detail,
            &mut self.startup_detail,
            &mut self.account,
        ]
        .into_iter()
        .flatten()
        {
            bound_label(text);
        }
        if let Some(description) = &mut self.description {
            *description = bounded(description, MAX_SERVICE_DESCRIPTION_BYTES).0;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceList {
    pub services: Vec<ServiceInfo>,
    pub read_at: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl ServiceList {
    /// Keeps the list within one control message: at most
    /// [`MAX_LISTED_SERVICES`], with descriptions cut to
    /// [`MAX_SERVICE_DESCRIPTION_BYTES`].
    pub fn bound(&mut self) {
        if self.services.len() > MAX_LISTED_SERVICES {
            self.services.truncate(MAX_LISTED_SERVICES);
            self.truncated = true;
        }
        for service in &mut self.services {
            service.bound();
        }
    }
}

/// Whether `name` is a service name a device on `platform` could have: a
/// Windows service key name (no slashes), a systemd unit name or a launchd
/// label.
pub fn valid_service_name(platform: crate::DevicePlatform, name: &str) -> bool {
    match platform {
        crate::DevicePlatform::Windows => {
            !name.is_empty()
                && name.len() <= MAX_SERVICE_NAME_BYTES
                && !name.contains(['/', '\\'])
                && !name.chars().any(char::is_control)
        }
        crate::DevicePlatform::Linux => valid_unit_name(name),
        crate::DevicePlatform::Macos => valid_launchd_label(name),
    }
}

/// Why a tool's request failed, for the website to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    NotFound,
    AccessDenied,
    /// The Agent's own process or service, or one the system can't do
    /// without.
    Protected,
    /// The process with that ID isn't the one the list showed.
    Changed,
    /// The request doesn't apply, such as a startup type this service
    /// can't have.
    Invalid,
    /// Other running services depend on it.
    Dependents,
    /// This device's platform doesn't have the tool.
    Unsupported,
    /// The Agent is running as many tool requests as it runs at once, or
    /// has as many that never returned as it leaves behind.
    Busy,
    /// The Agent ran out of the time the server waits: it didn't start the
    /// request, or the service hadn't finished starting or stopping.
    TimedOut,
    Failed,
}

impl ToolErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::AccessDenied => "access_denied",
            Self::Protected => "protected",
            Self::Changed => "changed",
            Self::Invalid => "invalid",
            Self::Dependents => "dependents",
            Self::Unsupported => "unsupported",
            Self::Busy => "busy",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
        }
    }
}

/// The longest message a [`ToolError`] carries, in characters.
pub const MAX_TOOL_ERROR_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
}

impl ToolError {
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The error with its message cut to [`MAX_TOOL_ERROR_CHARS`] and
    /// without control characters.
    pub fn sanitized(self) -> Self {
        Self {
            code: self.code,
            message: self
                .message
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_TOOL_ERROR_CHARS)
                .collect(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum ToolReply {
    Processes(ProcessList),
    ProcessesEnded(ProcessesEnded),
    Services(ServiceList),
    /// The service after a change.
    Service(ServiceInfo),
    Failed {
        error: ToolError,
    },
}

impl ToolReply {
    pub fn failed(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self::Failed {
            error: ToolError::new(code, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DevicePlatform;

    fn process(pid: u32, command_line: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: None,
            name: "p".into(),
            path: None,
            user: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            started_at: None,
            command_line: Some(command_line.into()),
            command_line_truncated: false,
            session: None,
        }
    }

    #[test]
    fn process_lists_stay_within_a_control_message() {
        let long = "x".repeat(MAX_COMMAND_LINE_BYTES + 10);
        let mut list = ProcessList {
            processes: (0..MAX_LISTED_PROCESSES as u32 + 5)
                .map(|pid| process(pid, &long))
                .collect(),
            read_at: 0,
            logical_cpus: 1,
            memory_total_bytes: None,
            truncated: false,
        };
        list.processes[0].cpu_percent = f64::NAN;
        list.processes[1].cpu_percent = 250.0;
        list.processes[2].command_line = Some("a\0b\nc".into());
        list.bound();
        assert!(list.truncated);
        assert_eq!(list.processes.len(), MAX_LISTED_PROCESSES);
        assert_eq!(list.processes[0].cpu_percent, 0.0);
        assert_eq!(list.processes[1].cpu_percent, 100.0);
        assert_eq!(list.processes[2].command_line.as_deref(), Some("a b c"));
        let first = &list.processes[0];
        assert_eq!(
            first.command_line.as_ref().unwrap().len(),
            MAX_COMMAND_LINE_BYTES
        );
        assert!(first.command_line_truncated);
        let text: usize = list
            .processes
            .iter()
            .filter_map(|process| process.command_line.as_ref())
            .map(String::len)
            .sum();
        assert!(text <= MAX_PROCESS_TEXT_BYTES);
        let last = list.processes.last().unwrap();
        assert!(last.command_line.is_none() && last.command_line_truncated);
        // Even so, it fits the control connection's 4 MiB with room to spare.
        assert!(
            serde_json::to_vec(&ToolReply::Processes(list))
                .unwrap()
                .len()
                < 3 * 1024 * 1024
        );
    }

    #[test]
    fn service_lists_are_bounded() {
        let mut list = ServiceList {
            services: (0..MAX_LISTED_SERVICES + 1)
                .map(|index| ServiceInfo {
                    name: format!("s{index}"),
                    display_name: "é".repeat(400),
                    state: ServiceState::Running,
                    state_detail: None,
                    startup: Some(ServiceStartup::Manual),
                    startup_detail: None,
                    startup_choices: vec![],
                    account: None,
                    description: Some("d".repeat(5000)),
                    pid: None,
                    protected: false,
                })
                .collect(),
            read_at: 0,
            truncated: false,
        };
        list.bound();
        assert!(list.truncated);
        assert_eq!(list.services.len(), MAX_LISTED_SERVICES);
        let service = &list.services[0];
        assert!(service.display_name.len() <= 256);
        assert_eq!(
            service.description.as_ref().unwrap().len(),
            MAX_SERVICE_DESCRIPTION_BYTES
        );
    }

    #[test]
    fn requests_and_replies_keep_their_shape() {
        let request = ToolRequest {
            request_id: "r".into(),
            timeout_ms: 30_000,
            operation: ToolOperation::EndProcesses {
                processes: vec![ProcessTarget {
                    pid: 7,
                    started_at: Some(5),
                }],
            },
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json["operation"],
            serde_json::json!({ "op": "end_processes", "processes": [{ "pid": 7, "started_at": 5 }] })
        );
        assert_eq!(
            serde_json::from_value::<ToolRequest>(json).unwrap(),
            request
        );
        let operation: ToolOperation = serde_json::from_value(serde_json::json!({
            "op": "set_service_startup", "name": "cron", "startup": "automatic_delayed"
        }))
        .unwrap();
        assert!(operation.slow());
        assert!(!ToolOperation::ListServices.slow());
        let reply = ToolReply::failed(ToolErrorCode::Dependents, "no");
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::json!({ "reply": "failed", "error": { "code": "dependents", "message": "no" } })
        );
    }

    #[test]
    fn service_names_follow_each_platform() {
        assert!(valid_service_name(DevicePlatform::Windows, "Spooler"));
        assert!(valid_service_name(
            DevicePlatform::Windows,
            "MSSQL$SQLEXPRESS"
        ));
        assert!(!valid_service_name(DevicePlatform::Windows, "a\\b"));
        assert!(!valid_service_name(DevicePlatform::Windows, ""));
        assert!(valid_service_name(
            DevicePlatform::Linux,
            "getty@tty1.service"
        ));
        assert!(!valid_service_name(DevicePlatform::Linux, "cron; reboot"));
        assert!(valid_service_name(
            DevicePlatform::Macos,
            "com.openssh.sshd"
        ));
        assert!(!valid_service_name(DevicePlatform::Macos, "system/com.x"));
    }

    #[test]
    fn errors_are_sanitized() {
        let error = ToolError::new(ToolErrorCode::Failed, format!("a\u{7}b{}", "c".repeat(600)))
            .sanitized();
        assert!(error.message.starts_with("abc"));
        assert_eq!(error.message.chars().count(), MAX_TOOL_ERROR_CHARS);
    }
}
