//! Monitors: checks an Agent runs on its own schedule for the alert rules
//! that cover its device, and the self-healing actions the server asks it to
//! take. See docs/monitors.md.
//!
//! The server sends an Agent every monitor that applies to it
//! ([`crate::AgentCommand::SetMonitors`]) when it connects and whenever that
//! set changes; on connecting it also says what it holds of each monitor's
//! latest result ([`KnownMonitorResult`]). The Agent runs each one every
//! `interval_seconds`, off its control connection, and reports results
//! ([`crate::AgentStatusMessage::MonitorResults`]): at once when one fails,
//! starts or stops passing, is the first the server would hold, or was asked
//! for; and the latest of the rest together every
//! [`MONITOR_REFRESH_INTERVAL_SECONDS`]. On connecting it sends only the
//! results the server lacks or holds differently. The server decides from
//! the number of consecutive failures whether to raise an alert.
//!
//! The validators here are shared: the server checks a rule with them before
//! saving it, and the Agent checks what it is sent again before it hands
//! a name or host to the operating system.

use serde::{Deserialize, Serialize};

use crate::{DevicePlatform, RunAs, ScriptLanguage};

/// How often a monitor may run, at most and at least.
pub const MIN_MONITOR_INTERVAL_SECONDS: u32 = 30;
pub const MAX_MONITOR_INTERVAL_SECONDS: u32 = 24 * 60 * 60;
pub const DEFAULT_MONITOR_INTERVAL_SECONDS: u32 = 5 * 60;
/// How long one check may take. A script check's timeout follows the
/// toolbox's limits instead.
pub const MIN_MONITOR_TIMEOUT_SECONDS: u32 = 1;
pub const MAX_MONITOR_TIMEOUT_SECONDS: u32 = 300;
pub const DEFAULT_MONITOR_TIMEOUT_SECONDS: u32 = 30;
/// The most consecutive failures a rule may wait for before alerting.
pub const MAX_MONITOR_FAILURES: u32 = 100;
/// How often the Agent sends the latest results of monitors that keep
/// passing, which it doesn't send as they come. Each connection starts at a
/// random point in the interval, so a fleet's refreshes spread out.
pub const MONITOR_REFRESH_INTERVAL_SECONDS: u64 = 15 * 60;
/// The most monitors one device runs, and results one report carries.
pub const MAX_MONITORS: usize = 256;
/// A result's detail, in characters, and its output, in bytes.
pub const MAX_MONITOR_DETAIL_CHARS: usize = 500;
pub const MAX_MONITOR_OUTPUT_BYTES: usize = 16 * 1024;
/// Names, hosts, sources, messages and keywords, in characters.
pub const MAX_MONITOR_TEXT_CHARS: usize = 256;
pub const MAX_MONITOR_URL_CHARS: usize = 2048;
pub const MAX_EVENT_IDS: usize = 32;
/// The most days ahead a certificate check looks.
pub const MAX_CERTIFICATE_DAYS: u32 = 365;
/// The longest a process's load must last before it counts.
pub const MAX_PROCESS_MINUTES: u32 = 24 * 60;
pub const MAX_PROCESS_MEGABYTES: u64 = 1024 * 1024;

/// One monitor as the Agent runs it. `id` is its alert rule's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDefinition {
    pub id: String,
    pub interval_seconds: u32,
    pub timeout_seconds: u32,
    pub check: MonitorCheck,
}

/// What a process monitor alerts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessAlert {
    /// No process of that name is running.
    NotRunning,
    /// One is running, when it shouldn't be.
    Running,
    /// Together they use more than `percent` of the computer's CPU for
    /// `minutes`.
    CpuAbove,
    /// Together they use more than `megabytes` of memory for `minutes`.
    MemoryAbove,
}

impl ProcessAlert {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRunning => "not_running",
            Self::Running => "running",
            Self::CpuAbove => "cpu_above",
            Self::MemoryAbove => "memory_above",
        }
    }
}

/// An event's level, as the Windows Event Log names them. journald's
/// priorities and the macOS unified log's types map onto them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventLevel {
    Critical,
    Error,
    Warning,
    Information,
}

impl EventLevel {
    pub const ALL: [Self; 4] = [
        Self::Critical,
        Self::Error,
        Self::Warning,
        Self::Information,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Critical => "Critical",
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Information => "Information",
        }
    }

    /// The Windows Event Log's level numbers: Information is 4, or 0 for
    /// events logged whatever the level.
    pub fn windows_levels(self) -> &'static [u8] {
        match self {
            Self::Critical => &[1],
            Self::Error => &[2],
            Self::Warning => &[3],
            Self::Information => &[0, 4],
        }
    }

    /// journald's (syslog's) priorities: emergency, alert and critical are
    /// critical; notice and info are information. Debug is never matched.
    pub fn journal_priorities(self) -> &'static [u8] {
        match self {
            Self::Critical => &[0, 1, 2],
            Self::Error => &[3],
            Self::Warning => &[4],
            Self::Information => &[5, 6],
        }
    }

    /// The level of a journald priority, if a monitor can match it.
    pub fn from_journal_priority(priority: u8) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|level| level.journal_priorities().contains(&priority))
    }
}

/// What a monitor checks. Its kind is its alert rule's condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorCheck {
    /// A Windows service, systemd unit or launchd job (in the system domain)
    /// isn't running, or, with `automatic`, isn't set to start by itself.
    Service {
        name: String,
        #[serde(default)]
        automatic: bool,
    },
    /// A process by name: see [`ProcessAlert`]. `percent` and `minutes` go
    /// with `cpu_above`, `megabytes` and `minutes` with `memory_above`.
    Process {
        name: String,
        alert_when: ProcessAlert,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        percent: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        megabytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
    },
    /// Events logged since the last check: in the Windows Event Log `log`,
    /// journald, or the macOS unified log. Every filter that is set must
    /// match: `source` (the Windows provider, a systemd unit or syslog
    /// identifier, a macOS subsystem or process), `event_ids` (Windows),
    /// `levels`, and `message`, a substring of the message, ignoring case.
    EventLog {
        #[serde(default)]
        log: String,
        #[serde(default)]
        source: String,
        #[serde(default)]
        event_ids: Vec<u32>,
        #[serde(default)]
        levels: Vec<EventLevel>,
        #[serde(default)]
        message: String,
    },
    /// The host answers an ICMP echo.
    Ping { host: String },
    /// A TCP connection to the host's port opens.
    TcpPort { host: String, port: u16 },
    /// A `GET` of `url` (without following redirects) answers with
    /// `status`, and its body contains `keyword` if one is set.
    Http {
        url: String,
        #[serde(default = "default_status")]
        status: u16,
        #[serde(default)]
        keyword: String,
        #[serde(default = "crate::default_enabled")]
        verify_tls: bool,
    },
    /// The certificate the host presents on `port` expires more than
    /// `days` from now, whether or not the device trusts it.
    Certificate {
        host: String,
        #[serde(default = "default_tls_port")]
        port: u16,
        days: u32,
    },
    /// A shared toolbox script exits with 0 within the timeout. The rule
    /// keeps `script_id`; the server fills in the script's `language` and
    /// `body` when it sends the monitor.
    ScriptCheck {
        script_id: String,
        run_as: RunAs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<ScriptLanguage>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        body: String,
    },
}

fn default_status() -> u16 {
    200
}

fn default_tls_port() -> u16 {
    443
}

impl MonitorCheck {
    /// The check's kind, which is its alert rule's condition.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Service { .. } => "service",
            Self::Process { .. } => "process",
            Self::EventLog { .. } => "event_log",
            Self::Ping { .. } => "ping",
            Self::TcpPort { .. } => "tcp_port",
            Self::Http { .. } => "http",
            Self::Certificate { .. } => "certificate",
            Self::ScriptCheck { .. } => "script_check",
        }
    }

    /// Whether an Agent on `platform` can run the check: a script needs its
    /// interpreter, a Windows event log check names its log, and a service
    /// is named as the platform names them ([`valid_unit_name`] on Linux,
    /// [`valid_launchd_label`] on a Mac).
    pub fn runs_on(&self, platform: DevicePlatform) -> bool {
        match self {
            Self::ScriptCheck {
                language: Some(language),
                ..
            } => language.runs_on(platform),
            Self::ScriptCheck { language: None, .. } => false,
            Self::EventLog { log, .. } => platform != DevicePlatform::Windows || !log.is_empty(),
            Self::Service { name, .. } => match platform {
                DevicePlatform::Linux => valid_unit_name(name),
                DevicePlatform::Macos => valid_launchd_label(name),
                DevicePlatform::Windows => true,
            },
            _ => true,
        }
    }

    /// Checks the fields, trimming text. The message says what's wrong.
    pub fn validate(&mut self) -> Result<(), String> {
        match self {
            Self::Service { name, .. } => {
                trim(name);
                check_name(name, "service")
            }
            Self::Process {
                name,
                alert_when,
                percent,
                megabytes,
                minutes,
            } => {
                trim(name);
                check_name(name, "process")?;
                if name.contains(['/', '\\']) {
                    return Err("give the process's file name, without a folder".into());
                }
                let needs_minutes = match alert_when {
                    ProcessAlert::NotRunning | ProcessAlert::Running => {
                        (*percent, *megabytes, *minutes) = (None, None, None);
                        false
                    }
                    ProcessAlert::CpuAbove => {
                        *megabytes = None;
                        match percent {
                            Some(value) if (1..100).contains(value) => true,
                            _ => return Err("the CPU threshold must be 1 to 99 percent".into()),
                        }
                    }
                    ProcessAlert::MemoryAbove => {
                        *percent = None;
                        match megabytes {
                            Some(value) if (1..=MAX_PROCESS_MEGABYTES).contains(value) => true,
                            _ => {
                                return Err(format!(
                                    "the memory threshold must be 1 to {MAX_PROCESS_MEGABYTES} MB"
                                ));
                            }
                        }
                    }
                };
                if needs_minutes
                    && !minutes.is_some_and(|value| (1..=MAX_PROCESS_MINUTES).contains(&value))
                {
                    return Err(format!(
                        "the load must last 1 to {MAX_PROCESS_MINUTES} minutes"
                    ));
                }
                Ok(())
            }
            Self::EventLog {
                log,
                source,
                event_ids,
                levels,
                message,
            } => {
                for text in [&mut *log, &mut *source, &mut *message] {
                    trim(text);
                    check_text(text, "event log filter")?;
                }
                if source.contains(['\'', '"', '<', '>', '&']) {
                    return Err("the source can't contain quotes, < > or &".into());
                }
                if log.contains(['\'', '"', '<', '>', '&', '*', '?']) {
                    return Err("the log name can't contain quotes, wildcards, < > or &".into());
                }
                event_ids.sort_unstable();
                event_ids.dedup();
                if event_ids.len() > MAX_EVENT_IDS || event_ids.iter().any(|id| *id > 65_535) {
                    return Err(format!(
                        "choose at most {MAX_EVENT_IDS} event IDs, each 0 to 65535"
                    ));
                }
                levels.sort_unstable();
                levels.dedup();
                Ok(())
            }
            Self::Ping { host } => check_host(host),
            Self::TcpPort { host, port } => {
                check_host(host)?;
                check_port(*port)
            }
            Self::Http {
                url,
                status,
                keyword,
                ..
            } => {
                trim(url);
                check_url(url)?;
                if !(100..=599).contains(status) {
                    return Err("the expected status must be 100 to 599".into());
                }
                check_text(keyword, "keyword")
            }
            Self::Certificate { host, port, days } => {
                check_host(host)?;
                check_port(*port)?;
                if !(1..=MAX_CERTIFICATE_DAYS).contains(days) {
                    return Err(format!(
                        "the warning must be 1 to {MAX_CERTIFICATE_DAYS} days ahead"
                    ));
                }
                Ok(())
            }
            Self::ScriptCheck { script_id, .. } => {
                if script_id.is_empty()
                    || script_id.len() > 64
                    || !script_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    return Err("choose a script".into());
                }
                Ok(())
            }
        }
    }
}

fn trim(text: &mut String) {
    let trimmed = text.trim();
    if trimmed.len() != text.len() {
        *text = trimmed.to_owned();
    }
}

fn check_text(text: &str, what: &str) -> Result<(), String> {
    if text.chars().count() > MAX_MONITOR_TEXT_CHARS || text.chars().any(char::is_control) {
        return Err(format!(
            "the {what} must be at most {MAX_MONITOR_TEXT_CHARS} characters with no control characters"
        ));
    }
    Ok(())
}

/// A service or process name: not empty, and not something a tool could
/// read as an option.
fn check_name(name: &str, what: &str) -> Result<(), String> {
    check_text(name, &format!("{what} name"))?;
    if name.is_empty() || name.starts_with('-') {
        return Err(format!("give the {what}'s name"));
    }
    Ok(())
}

fn check_port(port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("the port must be 1 to 65535".into());
    }
    Ok(())
}

/// Trims `host`, and checks it is a host name or an IP address, without
/// brackets around an IPv6 address.
pub fn check_host(host: &mut String) -> Result<(), String> {
    trim(host);
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        *host = inner.to_owned();
    }
    if valid_host(host) {
        Ok(())
    } else {
        Err("give a host name or IP address".into())
    }
}

/// Whether `name` is a systemd unit name: letters, digits and `:-_.@\`,
/// not starting with `-`.
pub fn valid_unit_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b":-_.@\\".contains(&byte))
}

/// Whether `label` is a launchd job label: letters, digits and `.-_`, not
/// starting with `-`.
pub fn valid_launchd_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 256
        && !label.starts_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
}

/// Whether `host` is a host name, an IPv4 address or an IPv6 address
/// (with an optional `%zone`).
pub fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if let Some((address, zone)) = host.split_once('%') {
        return address.parse::<std::net::Ipv6Addr>().is_ok()
            && !zone.is_empty()
            && zone.len() <= 32
            && zone
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

/// Checks that `url` is an `http` or `https` URL with a host.
pub fn check_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let valid = url.len() <= MAX_MONITOR_URL_CHARS
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && rest.is_some_and(|rest| {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            let host = if let Some(bracketed) = host.strip_prefix('[') {
                bracketed.split(']').next().unwrap_or_default()
            } else {
                host.split(':').next().unwrap_or_default()
            };
            valid_host(host)
        });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "give an http:// or https:// URL of at most {MAX_MONITOR_URL_CHARS} characters"
        ))
    }
}

/// How one check went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorResult {
    pub monitor_id: String,
    pub checked_at_unix_ms: u64,
    pub passing: bool,
    /// Consecutive failed checks, counting this one; 0 when it passed.
    pub failures: u32,
    /// What was measured: milliseconds for network checks, days left for a
    /// certificate, a percentage or megabytes for a process, matching events
    /// for a log, an exit code for a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// In words, such as `Spooler is stopped`.
    pub detail: String,
    /// A script's output, or the matching events, up to
    /// [`MAX_MONITOR_OUTPUT_BYTES`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// What the server holds of a monitor's latest result on the device. It
/// tells the Agent when it connects, so the Agent sends only what is news.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownMonitorResult {
    pub monitor_id: String,
    pub checked_at_unix_ms: u64,
    pub passing: bool,
    pub failures: u32,
}

/// A self-healing action the server asks the Agent to take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorAction {
    /// The server's record of the attempt, which the result names.
    pub action_id: String,
    /// The monitor to check again once the action is done.
    pub monitor_id: String,
    pub action: RemediationAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemediationAction {
    /// Restarts the service, or starts it if it is stopped.
    RestartService { name: String },
    /// Ends every process with the name.
    EndProcess { name: String },
}

/// How a [`MonitorAction`] went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorActionResult {
    pub action_id: String,
    pub succeeded: bool,
    /// What happened, such as `Restarted Spooler` or why it couldn't.
    pub detail: String,
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn truncate_text(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(value: &str) -> MonitorCheck {
        serde_json::from_str(value).unwrap()
    }

    #[test]
    fn checks_read_their_kind_and_defaults() {
        let http = check(r#"{"kind":"http","url":"https://intranet.example/health"}"#);
        assert_eq!(
            http,
            MonitorCheck::Http {
                url: "https://intranet.example/health".into(),
                status: 200,
                keyword: String::new(),
                verify_tls: true,
            }
        );
        assert_eq!(http.kind(), "http");
        assert_eq!(
            check(r#"{"kind":"certificate","host":"mail.example","days":14}"#),
            MonitorCheck::Certificate {
                host: "mail.example".into(),
                port: 443,
                days: 14
            }
        );
        let script = MonitorCheck::ScriptCheck {
            script_id: "s".into(),
            run_as: RunAs::System,
            language: None,
            body: String::new(),
        };
        assert_eq!(
            serde_json::to_value(&script).unwrap(),
            serde_json::json!({ "kind": "script_check", "script_id": "s", "run_as": "system" })
        );
        assert!(!script.runs_on(DevicePlatform::Windows));
    }

    #[test]
    fn checks_validate_and_trim_their_fields() {
        let mut service = check(r#"{"kind":"service","name":"  Spooler "}"#);
        service.validate().unwrap();
        assert_eq!(
            service,
            MonitorCheck::Service {
                name: "Spooler".into(),
                automatic: false
            }
        );
        assert!(
            check(r#"{"kind":"service","name":"--now"}"#)
                .validate()
                .is_err()
        );
        assert!(
            check(r#"{"kind":"service","name":"a\nb"}"#)
                .validate()
                .is_err()
        );

        let mut process = check(
            r#"{"kind":"process","name":"sqlservr.exe","alert_when":"not_running","percent":5}"#,
        );
        process.validate().unwrap();
        assert!(matches!(
            process,
            MonitorCheck::Process { percent: None, .. }
        ));
        assert!(
            check(r#"{"kind":"process","name":"x","alert_when":"cpu_above","percent":90}"#)
                .validate()
                .is_err(),
            "a CPU threshold needs minutes"
        );
        assert!(
            check(r#"{"kind":"process","name":"C:\\x.exe","alert_when":"running"}"#)
                .validate()
                .is_err()
        );

        let mut log = check(
            r#"{"kind":"event_log","log":"System","source":"Service Control Manager","event_ids":[7034,7031,7034],"levels":["warning","error"]}"#,
        );
        log.validate().unwrap();
        let MonitorCheck::EventLog {
            event_ids, levels, ..
        } = &log
        else {
            unreachable!()
        };
        assert_eq!(event_ids, &[7031, 7034]);
        assert_eq!(levels, &[EventLevel::Error, EventLevel::Warning]);
        assert!(log.runs_on(DevicePlatform::Linux));
        assert!(
            check(r#"{"kind":"event_log","source":"x' or '1'='1"}"#)
                .validate()
                .is_err(),
            "a source can't break out of the query"
        );
        assert!(!check(r#"{"kind":"event_log"}"#).runs_on(DevicePlatform::Windows));

        let mut ping = check(r#"{"kind":"ping","host":" [fe80::1%eth0] "}"#);
        ping.validate().unwrap();
        assert_eq!(
            ping,
            MonitorCheck::Ping {
                host: "fe80::1%eth0".into()
            }
        );
        for host in [
            "-c1",
            "a b",
            "evil;rm",
            "",
            "a..b",
            "x".repeat(254).as_str(),
        ] {
            assert!(
                MonitorCheck::Ping { host: host.into() }.validate().is_err(),
                "{host}"
            );
        }
        assert!(
            check(r#"{"kind":"tcp_port","host":"db01","port":0}"#)
                .validate()
                .is_err()
        );
        assert!(
            check(r#"{"kind":"http","url":"ftp://x"}"#)
                .validate()
                .is_err()
        );
        assert!(
            check(r#"{"kind":"http","url":"https://user@[::1]:8443/x?y"}"#)
                .validate()
                .is_ok()
        );
        assert!(
            check(r#"{"kind":"http","url":"https://a b/"}"#)
                .validate()
                .is_err()
        );
        assert!(
            check(r#"{"kind":"certificate","host":"x","days":0}"#)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn services_are_named_as_their_platform_names_them() {
        let service = |name: &str| check(&format!(r#"{{"kind":"service","name":"{name}"}}"#));
        let spooler = service("Print Spooler");
        assert!(spooler.runs_on(DevicePlatform::Windows));
        assert!(!spooler.runs_on(DevicePlatform::Linux));
        assert!(!spooler.runs_on(DevicePlatform::Macos));
        let cron = service("cron.service");
        assert!(cron.runs_on(DevicePlatform::Linux));
        assert!(cron.runs_on(DevicePlatform::Macos));
        assert!(service("user@1000.service").runs_on(DevicePlatform::Linux));
        assert!(!service("user@1000.service").runs_on(DevicePlatform::Macos));
        assert!(valid_unit_name("dev-disk-by\\x2duuid.device"));
        assert!(!valid_unit_name("cron;reboot"));
        assert!(!valid_launchd_label("system/com.example"));
        assert!(valid_launchd_label("com.example.backup"));
    }

    #[test]
    fn levels_map_to_journal_priorities() {
        assert_eq!(
            EventLevel::from_journal_priority(2),
            Some(EventLevel::Critical)
        );
        assert_eq!(
            EventLevel::from_journal_priority(6),
            Some(EventLevel::Information)
        );
        assert_eq!(EventLevel::from_journal_priority(7), None);
        assert_eq!(truncate_text("añb", 2), "a");
    }
}
