//! What a monitor rule checks, and what a rule does to heal itself, as the
//! rule stores them, with their validation and their descriptions.
use meshrmm_protocol_types::{
    DEFAULT_MONITOR_INTERVAL_SECONDS, DEFAULT_MONITOR_TIMEOUT_SECONDS, MAX_MONITOR_FAILURES,
    MAX_MONITOR_INTERVAL_SECONDS, MAX_MONITOR_TIMEOUT_SECONDS, MAX_SCRIPT_TIMEOUT_SECONDS,
    MIN_MONITOR_INTERVAL_SECONDS, MIN_MONITOR_TIMEOUT_SECONDS, MIN_SCRIPT_TIMEOUT_SECONDS,
    MonitorCheck, ProcessAlert, RunAs,
};
use serde::{Deserialize, Serialize};

use crate::{
    alerts::rules::{Condition, format_number, plural},
    http::ApiError,
};

/// The most self-healing attempts on one alert.
pub const MAX_REMEDIATION_ATTEMPTS: u32 = 10;
/// The longest wait between attempts, in minutes.
pub const MAX_RETRY_MINUTES: u32 = 24 * 60;

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_rule")
}

fn default_interval() -> u32 {
    DEFAULT_MONITOR_INTERVAL_SECONDS
}

fn default_timeout() -> u32 {
    DEFAULT_MONITOR_TIMEOUT_SECONDS
}

fn one() -> u32 {
    1
}

/// A monitor rule's check, and how often the Agent runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    #[serde(default = "default_interval")]
    pub interval_seconds: u32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
    /// Failed checks in a row that raise an alert.
    #[serde(default = "one")]
    pub failures: u32,
    pub check: MonitorCheck,
}

impl Monitor {
    /// Checks the monitor for a rule with `condition`, and clears what the
    /// server fills in itself.
    pub fn validate(mut self, condition: Condition) -> Result<Self, ApiError> {
        if self.check.kind() != condition.as_str() {
            return Err(invalid(format!(
                "a {} rule checks a {} monitor",
                condition.as_str(),
                condition.as_str()
            )));
        }
        self.check.validate().map_err(invalid)?;
        if let MonitorCheck::ScriptCheck { language, body, .. } = &mut self.check {
            *language = None;
            body.clear();
        }
        if !(MIN_MONITOR_INTERVAL_SECONDS..=MAX_MONITOR_INTERVAL_SECONDS)
            .contains(&self.interval_seconds)
        {
            return Err(invalid(format!(
                "a monitor runs every {MIN_MONITOR_INTERVAL_SECONDS} seconds to {} hours",
                MAX_MONITOR_INTERVAL_SECONDS / 3600
            )));
        }
        let (min, max) = if condition == Condition::ScriptCheck {
            (MIN_SCRIPT_TIMEOUT_SECONDS, MAX_SCRIPT_TIMEOUT_SECONDS)
        } else {
            (MIN_MONITOR_TIMEOUT_SECONDS, MAX_MONITOR_TIMEOUT_SECONDS)
        };
        if !(min..=max).contains(&self.timeout_seconds) {
            return Err(invalid(format!(
                "the timeout must be {min} to {max} seconds"
            )));
        }
        if !(1..=MAX_MONITOR_FAILURES).contains(&self.failures) {
            return Err(invalid(format!(
                "alert after 1 to {MAX_MONITOR_FAILURES} failed checks in a row"
            )));
        }
        Ok(self)
    }

    /// The toolbox script a script check runs.
    pub fn script_id(&self) -> Option<&str> {
        match &self.check {
            MonitorCheck::ScriptCheck { script_id, .. } => Some(script_id),
            _ => None,
        }
    }

    /// What the check alerts on, such as `“Spooler” isn't running`.
    /// `script_name` names a script check's script.
    pub fn describe(&self, script_name: Option<&str>) -> String {
        let quoted = |text: &str| format!("\u{201c}{text}\u{201d}");
        match &self.check {
            MonitorCheck::Service { name, automatic } => {
                if *automatic {
                    format!(
                        "{} isn't running or isn't set to start automatically",
                        quoted(name)
                    )
                } else {
                    format!("{} isn't running", quoted(name))
                }
            }
            MonitorCheck::Process {
                name,
                alert_when,
                percent,
                megabytes,
                minutes,
            } => {
                let minutes = plural(i64::from(minutes.unwrap_or(1)), "minute");
                match alert_when {
                    ProcessAlert::NotRunning => format!("{} isn't running", quoted(name)),
                    ProcessAlert::Running => format!("{} is running", quoted(name)),
                    ProcessAlert::CpuAbove => format!(
                        "{} uses more than {}% CPU for {minutes}",
                        quoted(name),
                        percent.unwrap_or_default()
                    ),
                    ProcessAlert::MemoryAbove => format!(
                        "{} uses more than {} MB of memory for {minutes}",
                        quoted(name),
                        format_number(megabytes.unwrap_or_default() as f64)
                    ),
                }
            }
            MonitorCheck::EventLog {
                log,
                source,
                event_ids,
                levels,
                message,
            } => {
                let mut text = if levels.is_empty() {
                    "Events".to_owned()
                } else {
                    let names = levels
                        .iter()
                        .map(|level| level.label().to_lowercase())
                        .collect::<Vec<_>>();
                    let mut names = join_or(&names);
                    if let Some(first) = names.get_mut(..1) {
                        first.make_ascii_uppercase();
                    }
                    format!("{names} events")
                };
                if !log.is_empty() {
                    text.push_str(&format!(" in {}", quoted(log)));
                }
                if !source.is_empty() {
                    text.push_str(&format!(" from {}", quoted(source)));
                }
                if !event_ids.is_empty() {
                    let ids = event_ids.iter().map(u32::to_string).collect::<Vec<_>>();
                    text.push_str(&format!(" with ID {}", join_or(&ids)));
                }
                if !message.is_empty() {
                    text.push_str(&format!(" mentioning {}", quoted(message)));
                }
                text
            }
            MonitorCheck::Ping { host } => format!("{} doesn't answer pings", quoted(host)),
            MonitorCheck::TcpPort { host, port } => {
                format!("Port {port} on {} doesn't accept connections", quoted(host))
            }
            MonitorCheck::Http {
                url,
                status,
                keyword,
                ..
            } => {
                if keyword.is_empty() {
                    format!("{} doesn't answer {status}", quoted(url))
                } else {
                    format!(
                        "{} doesn't answer {status} with {} in its page",
                        quoted(url),
                        quoted(keyword)
                    )
                }
            }
            MonitorCheck::Certificate { host, port, days } => format!(
                "The certificate of {} expires within {}",
                quoted(&if *port == 443 {
                    host.clone()
                } else {
                    format!("{host}:{port}")
                }),
                plural(i64::from(*days), "day")
            ),
            MonitorCheck::ScriptCheck { .. } => format!(
                "{} fails or times out",
                quoted(script_name.unwrap_or("A deleted script"))
            ),
        }
    }
}

fn join_or(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// What self-healing does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemediationKind {
    /// Restarts a service monitor's service.
    RestartService,
    /// Ends a process monitor's processes.
    EndProcess,
    /// Runs a shared toolbox script.
    Script,
}

impl RemediationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RestartService => "restart_service",
            Self::EndProcess => "end_process",
            Self::Script => "script",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "restart_service" => Some(Self::RestartService),
            "end_process" => Some(Self::EndProcess),
            "script" => Some(Self::Script),
            _ => None,
        }
    }
}

fn three() -> u32 {
    3
}

fn five() -> u32 {
    5
}

/// What a rule does when it raises an alert, until the condition passes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remediation {
    pub action: RemediationKind,
    #[serde(default)]
    pub script_id: Option<String>,
    #[serde(default)]
    pub run_as: Option<RunAs>,
    /// At most this many attempts per alert.
    #[serde(default = "three")]
    pub attempts: u32,
    /// Minutes between an attempt finishing and the next, and after the last
    /// before self-healing counts as failed.
    #[serde(default = "five")]
    pub retry_minutes: u32,
}

impl Remediation {
    pub fn validate(
        mut self,
        condition: Condition,
        monitor: Option<&Monitor>,
    ) -> Result<Self, ApiError> {
        if !condition.clears_itself() || condition == Condition::DeviceOffline {
            return Err(invalid(
                "self-healing needs a condition that can pass while the device is online",
            ));
        }
        match self.action {
            RemediationKind::RestartService => {
                if !matches!(
                    monitor.map(|monitor| &monitor.check),
                    Some(MonitorCheck::Service { .. })
                ) {
                    return Err(invalid("only a service monitor can restart its service"));
                }
                (self.script_id, self.run_as) = (None, None);
            }
            RemediationKind::EndProcess => {
                if !matches!(
                    monitor.map(|monitor| &monitor.check),
                    Some(MonitorCheck::Process { alert_when, .. }) if *alert_when != ProcessAlert::NotRunning
                ) {
                    return Err(invalid(
                        "only a process monitor for a running process can end it",
                    ));
                }
                (self.script_id, self.run_as) = (None, None);
            }
            RemediationKind::Script => {
                let Some(script_id) = self.script_id.as_deref() else {
                    return Err(invalid("choose the script self-healing runs"));
                };
                crate::agents::parse_id(script_id, "script")
                    .map_err(|_| invalid("choose the script self-healing runs"))?;
                self.run_as.get_or_insert(RunAs::System);
            }
        }
        if !(1..=MAX_REMEDIATION_ATTEMPTS).contains(&self.attempts) {
            return Err(invalid(format!(
                "self-healing tries 1 to {MAX_REMEDIATION_ATTEMPTS} times"
            )));
        }
        if !(1..=MAX_RETRY_MINUTES).contains(&self.retry_minutes) {
            return Err(invalid(format!(
                "wait 1 to {MAX_RETRY_MINUTES} minutes between attempts"
            )));
        }
        Ok(self)
    }

    /// The protocol's action for a built-in kind, with the monitor's name.
    pub fn agent_action(
        &self,
        monitor: Option<&Monitor>,
    ) -> Option<meshrmm_protocol_types::RemediationAction> {
        use meshrmm_protocol_types::RemediationAction;
        match (self.action, monitor.map(|monitor| &monitor.check)) {
            (RemediationKind::RestartService, Some(MonitorCheck::Service { name, .. })) => {
                Some(RemediationAction::RestartService { name: name.clone() })
            }
            (RemediationKind::EndProcess, Some(MonitorCheck::Process { name, .. })) => {
                Some(RemediationAction::EndProcess { name: name.clone() })
            }
            _ => None,
        }
    }

    /// What it does, such as `Restart the service, up to 3 times`.
    pub fn describe(&self, script_name: Option<&str>) -> String {
        let what = match self.action {
            RemediationKind::RestartService => "Restart the service".to_owned(),
            RemediationKind::EndProcess => "End the process".to_owned(),
            RemediationKind::Script => format!(
                "Run \u{201c}{}\u{201d} as {}",
                script_name.unwrap_or("a deleted script"),
                match self.run_as.unwrap_or(RunAs::System) {
                    RunAs::System => "SYSTEM or root",
                    RunAs::User => "the signed-in user",
                }
            ),
        };
        if self.attempts == 1 {
            return format!("{what}, once");
        }
        format!(
            "{what}, up to {} {} apart",
            plural(i64::from(self.attempts), "time"),
            plural(i64::from(self.retry_minutes), "minute")
        )
    }
}

/// When a rule's channels hear of an alert.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyWhen {
    /// As soon as it is raised.
    #[default]
    Raised,
    /// Only once self-healing has given up, so problems it fixes stay quiet.
    RemediationFailed,
}

impl NotifyWhen {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::RemediationFailed => "remediation_failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "raised" => Some(Self::Raised),
            "remediation_failed" => Some(Self::RemediationFailed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn monitor(value: serde_json::Value) -> Monitor {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn monitors_match_their_condition_and_keep_their_limits() {
        let service = monitor(json!({ "check": { "kind": "service", "name": "Spooler" } }));
        assert!(service.clone().validate(Condition::Process).is_err());
        let service = service.validate(Condition::Service).unwrap();
        assert_eq!(service.interval_seconds, 300);
        assert_eq!(service.failures, 1);
        assert_eq!(
            service.describe(None),
            "\u{201c}Spooler\u{201d} isn't running"
        );
        for (interval, timeout, failures) in [(29, 30, 1), (60, 0, 1), (60, 301, 1), (60, 30, 0)] {
            assert!(
                monitor(json!({
                    "interval_seconds": interval, "timeout_seconds": timeout, "failures": failures,
                    "check": { "kind": "ping", "host": "gw" },
                }))
                .validate(Condition::Ping)
                .is_err(),
                "{interval} {timeout} {failures}"
            );
        }
        let script = monitor(json!({
            "timeout_seconds": 600,
            "check": { "kind": "script_check", "script_id": "00000000-0000-4000-8000-000000000001", "run_as": "user", "body": "rm -rf /", "language": "shell" },
        }))
        .validate(Condition::ScriptCheck)
        .unwrap();
        assert!(
            matches!(&script.check, MonitorCheck::ScriptCheck { body, language: None, .. } if body.is_empty()),
            "the server fills in a script's body itself"
        );
        assert_eq!(
            script.describe(Some("Backups fresh")),
            "\u{201c}Backups fresh\u{201d} fails or times out"
        );
    }

    #[test]
    fn monitors_describe_themselves() {
        let describe = |check: serde_json::Value| monitor(json!({ "check": check })).describe(None);
        assert_eq!(
            describe(
                json!({ "kind": "process", "name": "sqlservr.exe", "alert_when": "cpu_above", "percent": 90, "minutes": 5 })
            ),
            "\u{201c}sqlservr.exe\u{201d} uses more than 90% CPU for 5 minutes"
        );
        assert_eq!(
            describe(
                json!({ "kind": "event_log", "log": "System", "source": "Disk", "event_ids": [7, 11, 51], "levels": ["critical", "error"], "message": "bad block" })
            ),
            "Critical or error events in \u{201c}System\u{201d} from \u{201c}Disk\u{201d} with ID 7, 11 or 51 mentioning \u{201c}bad block\u{201d}"
        );
        assert_eq!(
            describe(
                json!({ "kind": "certificate", "host": "mail.example", "port": 993, "days": 1 })
            ),
            "The certificate of \u{201c}mail.example:993\u{201d} expires within 1 day"
        );
        assert_eq!(
            describe(
                json!({ "kind": "http", "url": "https://x/health", "status": 204, "keyword": "ok" })
            ),
            "\u{201c}https://x/health\u{201d} doesn't answer 204 with \u{201c}ok\u{201d} in its page"
        );
    }

    #[test]
    fn remediation_fits_its_rule() {
        let service = monitor(json!({ "check": { "kind": "service", "name": "Spooler" } }));
        let process = monitor(
            json!({ "check": { "kind": "process", "name": "x", "alert_when": "not_running" } }),
        );
        let restart: Remediation =
            serde_json::from_value(json!({ "action": "restart_service", "script_id": "x" }))
                .unwrap();
        let restart = restart
            .validate(Condition::Service, Some(&service))
            .unwrap();
        assert_eq!(restart.script_id, None);
        assert_eq!((restart.attempts, restart.retry_minutes), (3, 5));
        assert_eq!(
            restart.describe(None),
            "Restart the service, up to 3 times 5 minutes apart"
        );
        assert!(
            restart
                .clone()
                .validate(Condition::Process, Some(&process))
                .is_err()
        );
        let end: Remediation = serde_json::from_value(json!({ "action": "end_process" })).unwrap();
        assert!(
            end.validate(Condition::Process, Some(&process)).is_err(),
            "there is nothing running to end"
        );
        let script: Remediation = serde_json::from_value(json!({ "action": "script", "script_id": "00000000-0000-4000-8000-000000000001", "attempts": 1 })).unwrap();
        let script = script.validate(Condition::DiskSpace, None).unwrap();
        assert_eq!(script.run_as, Some(RunAs::System));
        for condition in [
            Condition::DeviceOffline,
            Condition::ScriptFailed,
            Condition::UpdateInstallFailed,
        ] {
            assert!(
                script.clone().validate(condition, None).is_err(),
                "{condition:?}"
            );
        }
        let missing: Remediation = serde_json::from_value(json!({ "action": "script" })).unwrap();
        assert!(missing.validate(Condition::Cpu, None).is_err());
        let many: Remediation = serde_json::from_value(json!({ "action": "script", "script_id": "00000000-0000-4000-8000-000000000001", "attempts": 11 })).unwrap();
        assert!(many.validate(Condition::Cpu, None).is_err());
    }
}
