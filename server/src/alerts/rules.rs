//! Alert rules: what raises alerts, how severe they are, which devices they
//! cover and which notification channels they notify.
use std::collections::HashMap;

use meshrmm_protocol_types::UpdateCategory;
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        self, Executor,
        tables::{
            AlertRuleChannels, AlertRuleCompanies, AlertRuleTags, AlertRules, NotificationChannels,
            ToolboxScripts,
        },
    },
    http::ApiError,
    monitors::settings::{Monitor, NotifyWhen, Remediation, RemediationKind},
    targets::{self, Choice, TagMatch, Target, TargetKind},
};

pub const MAX_NAME_LENGTH: usize = 120;
/// The longest a device may be offline before a rule raises an alert.
pub const MAX_OFFLINE_MINUTES: i64 = 7 * 24 * 60;
/// The longest a load must be sustained; the engine keeps this much of each
/// device's readings.
pub const MAX_SUSTAINED_MINUTES: i64 = 60;
/// The longest updates or a restart may be pending before a rule raises an
/// alert.
pub const MAX_PENDING_DAYS: i64 = 365;
/// An updates rule's category that counts every pending update.
pub const ANY_CATEGORY: &str = "any";
const MAX_GB: f64 = 1_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Critical => "Critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// The device has been offline for `duration_minutes`.
    DeviceOffline,
    /// A volume's free space is below `threshold` percent or gigabytes.
    DiskSpace,
    /// CPU load averaged above `threshold` percent over `duration_minutes`.
    Cpu,
    /// Memory use averaged above `threshold` percent over `duration_minutes`.
    Memory,
    /// A toolbox script run failed, timed out or never reported back.
    ScriptFailed,
    /// Updates of `update_category` have been pending for `days`.
    UpdatesPending,
    /// An update install failed, partly or entirely, or never reported back.
    UpdateInstallFailed,
    /// The device has had to restart to finish installing updates for `days`.
    RebootPending,
    /// An application install or update failed, or never reported back.
    AppInstallFailed,
    /// Application updates have been pending for `days`.
    AppUpdatesPending,
    /// Monitors, which the device's Agent checks: see docs/monitors.md.
    Service,
    Process,
    EventLog,
    Ping,
    TcpPort,
    Http,
    Certificate,
    ScriptCheck,
}

impl Condition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeviceOffline => "device_offline",
            Self::DiskSpace => "disk_space",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::ScriptFailed => "script_failed",
            Self::UpdatesPending => "updates_pending",
            Self::UpdateInstallFailed => "update_install_failed",
            Self::RebootPending => "reboot_pending",
            Self::AppInstallFailed => "app_install_failed",
            Self::AppUpdatesPending => "app_updates_pending",
            Self::Service => "service",
            Self::Process => "process",
            Self::EventLog => "event_log",
            Self::Ping => "ping",
            Self::TcpPort => "tcp_port",
            Self::Http => "http",
            Self::Certificate => "certificate",
            Self::ScriptCheck => "script_check",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "device_offline" => Some(Self::DeviceOffline),
            "disk_space" => Some(Self::DiskSpace),
            "cpu" => Some(Self::Cpu),
            "memory" => Some(Self::Memory),
            "script_failed" => Some(Self::ScriptFailed),
            "updates_pending" => Some(Self::UpdatesPending),
            "update_install_failed" => Some(Self::UpdateInstallFailed),
            "reboot_pending" => Some(Self::RebootPending),
            "app_install_failed" => Some(Self::AppInstallFailed),
            "app_updates_pending" => Some(Self::AppUpdatesPending),
            "service" => Some(Self::Service),
            "process" => Some(Self::Process),
            "event_log" => Some(Self::EventLog),
            "ping" => Some(Self::Ping),
            "tcp_port" => Some(Self::TcpPort),
            "http" => Some(Self::Http),
            "certificate" => Some(Self::Certificate),
            "script_check" => Some(Self::ScriptCheck),
            _ => None,
        }
    }

    /// Whether the server resolves the alert once the condition passes;
    /// script and install failures stay until a person resolves them.
    pub fn clears_itself(self) -> bool {
        !matches!(
            self,
            Self::ScriptFailed | Self::UpdateInstallFailed | Self::AppInstallFailed
        )
    }

    /// Whether the device's Agent checks the condition, as a monitor. The
    /// server raises and resolves its alerts from what the Agent reports.
    pub fn is_monitor(self) -> bool {
        matches!(
            self,
            Self::Service
                | Self::Process
                | Self::EventLog
                | Self::Ping
                | Self::TcpPort
                | Self::Http
                | Self::Certificate
                | Self::ScriptCheck
        )
    }

    /// Whether the Agent checks the condition with the access of the rule's
    /// owner, which needs `scripts.run`: it runs a script, reaches hosts
    /// from inside the device's network, or reads the device's logs.
    /// Service and process checks only look at the device itself.
    pub fn runs_with_owner_access(self) -> bool {
        matches!(
            self,
            Self::EventLog
                | Self::Ping
                | Self::TcpPort
                | Self::Http
                | Self::Certificate
                | Self::ScriptCheck
        )
    }
}

/// A toolbox script a rule runs, as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptRef {
    pub name: String,
    pub shared: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdUnit {
    Percent,
    Gb,
}

impl ThresholdUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Percent => "percent",
            Self::Gb => "gb",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "percent" => Some(Self::Percent),
            "gb" => Some(Self::Gb),
            _ => None,
        }
    }
}

/// A rule, with the devices it covers and the channels it notifies.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub severity: Severity,
    pub enabled: bool,
    pub condition: Condition,
    pub threshold: Option<f64>,
    pub threshold_unit: Option<ThresholdUnit>,
    pub duration_minutes: Option<i64>,
    pub nonzero_exit_fails: bool,
    /// For updates_pending: `any`, or the category that counts.
    pub update_category: Option<String>,
    /// For updates_pending and reboot_pending.
    pub days: Option<i64>,
    /// Every device, chosen companies, or the devices with chosen tags.
    #[serde(flatten)]
    pub target: Target,
    pub channel_ids: Vec<String>,
    pub notify_on_resolve: bool,
    /// What a monitor rule's Agent checks.
    pub monitor: Option<Monitor>,
    /// What the rule does to heal the alerts it raises.
    pub remediation: Option<Remediation>,
    pub notify_when: NotifyWhen,
    /// Who last saved the rule: its script checks and self-healing run with
    /// their access.
    pub owner_user_id: Option<String>,
    /// The scripts it runs, as they are now; `None` once deleted.
    pub check_script: Option<ScriptRef>,
    pub remediation_script: Option<ScriptRef>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Rule {
    /// Whether the rule runs with its owner's access, which needs
    /// `scripts.run`: it heals itself, or its check runs a script, reaches
    /// hosts from inside the network or reads the device's logs.
    pub fn acts_on_devices(&self) -> bool {
        self.remediation.is_some() || self.condition.runs_with_owner_access()
    }

    /// Whether channels hear of an alert as soon as it is raised.
    pub fn notifies_at_once(&self) -> bool {
        self.remediation.is_none() || self.notify_when == NotifyWhen::Raised
    }

    /// What self-healing does, in words.
    pub fn describe_remediation(&self) -> Option<String> {
        self.remediation.as_ref().map(|remediation| {
            remediation.describe(
                self.remediation_script
                    .as_ref()
                    .map(|script| script.name.as_str()),
            )
        })
    }

    /// The category an updates rule counts; `None` for every one.
    pub fn category(&self) -> Option<UpdateCategory> {
        self.update_category
            .as_deref()
            .and_then(UpdateCategory::parse)
    }

    /// Whether the rule covers `device`.
    pub fn covers(&self, device: &targets::Device) -> bool {
        self.target.covers(device)
    }

    /// The condition in words, such as "CPU above 90% for 5 minutes".
    pub fn describe(&self) -> String {
        let threshold = format_number(self.threshold.unwrap_or_default());
        let minutes = plural(self.duration_minutes.unwrap_or_default(), "minute");
        if let Some(monitor) = &self.monitor {
            return monitor.describe(
                self.check_script
                    .as_ref()
                    .map(|script| script.name.as_str()),
            );
        }
        match self.condition {
            Condition::DeviceOffline => format!("Offline for {minutes}"),
            Condition::DiskSpace => match self.threshold_unit {
                Some(ThresholdUnit::Gb) => format!("Free disk space below {threshold} GB"),
                _ => format!("Free disk space below {threshold}%"),
            },
            Condition::Cpu => format!("CPU above {threshold}% for {minutes}"),
            Condition::Memory => format!("Memory above {threshold}% for {minutes}"),
            Condition::ScriptFailed if self.nonzero_exit_fails => {
                "A script fails or exits with an error code".to_owned()
            }
            Condition::ScriptFailed => "A script fails, times out or doesn't report".to_owned(),
            Condition::UpdatesPending => {
                let days = plural(self.days.unwrap_or_default(), "day");
                match self.category() {
                    Some(category) => format!("{} updates pending for {days}", category.label()),
                    None => format!("Updates pending for {days}"),
                }
            }
            Condition::UpdateInstallFailed => "An update install fails".to_owned(),
            Condition::RebootPending => format!(
                "Restart pending for {}",
                plural(self.days.unwrap_or_default(), "day")
            ),
            Condition::AppInstallFailed => "An application install or update fails".to_owned(),
            Condition::AppUpdatesPending => format!(
                "Application updates pending for {}",
                plural(self.days.unwrap_or_default(), "day")
            ),
            _ => "A monitor fails".to_owned(),
        }
    }
}

/// `12`, or `12.5`: at most two decimals and no trailing zeros.
pub fn format_number(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

pub fn plural(count: i64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// A rule as the website sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleInput {
    pub name: String,
    pub severity: Severity,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub condition: Condition,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub threshold_unit: Option<ThresholdUnit>,
    #[serde(default)]
    pub duration_minutes: Option<i64>,
    #[serde(default)]
    pub nonzero_exit_fails: bool,
    #[serde(default)]
    pub update_category: Option<String>,
    #[serde(default)]
    pub days: Option<i64>,
    #[serde(default = "every_device")]
    pub target: TargetKind,
    #[serde(default)]
    pub company_ids: Vec<String>,
    #[serde(default)]
    pub tag_ids: Vec<String>,
    #[serde(default)]
    pub tag_match: TagMatch,
    #[serde(default)]
    pub channel_ids: Vec<String>,
    #[serde(default)]
    pub notify_on_resolve: bool,
    #[serde(default)]
    pub monitor: Option<Monitor>,
    #[serde(default)]
    pub remediation: Option<Remediation>,
    #[serde(default)]
    pub notify_when: NotifyWhen,
}

fn enabled_by_default() -> bool {
    true
}

fn every_device() -> TargetKind {
    TargetKind::All
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_rule")
}

impl RuleInput {
    /// Checks the fields the condition uses and clears the others.
    pub fn validate(mut self) -> Result<Self, ApiError> {
        let name = self.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_NAME_LENGTH
            || name.chars().any(char::is_control)
        {
            return Err(invalid(format!(
                "the rule name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
            )));
        }
        self.name = name.to_owned();
        let percent = |threshold: Option<f64>, what: &str| match threshold {
            Some(value) if value.is_finite() && value > 0.0 && value < 100.0 => Ok(value),
            _ => Err(invalid(format!(
                "the {what} threshold must be a percentage above 0 and below 100"
            ))),
        };
        let minutes = |duration: Option<i64>, max: i64| match duration {
            Some(value) if (1..=max).contains(&value) => Ok(value),
            _ => Err(invalid(format!("the duration must be 1 to {max} minutes"))),
        };
        let (threshold, unit, duration) = match self.condition {
            Condition::DeviceOffline => (
                None,
                None,
                Some(minutes(self.duration_minutes, MAX_OFFLINE_MINUTES)?),
            ),
            Condition::DiskSpace => match self.threshold_unit {
                Some(ThresholdUnit::Percent) => (
                    Some(percent(self.threshold, "free space")?),
                    Some(ThresholdUnit::Percent),
                    None,
                ),
                Some(ThresholdUnit::Gb) => match self.threshold {
                    Some(value) if value.is_finite() && value > 0.0 && value <= MAX_GB => {
                        (Some(value), Some(ThresholdUnit::Gb), None)
                    }
                    _ => {
                        return Err(invalid(
                            "the free space threshold must be above 0 and at most 1,000,000 GB",
                        ));
                    }
                },
                None => return Err(invalid("choose percent or gb for the free space threshold")),
            },
            Condition::Cpu | Condition::Memory => (
                Some(percent(self.threshold, "load")?),
                None,
                Some(minutes(self.duration_minutes, MAX_SUSTAINED_MINUTES)?),
            ),
            Condition::ScriptFailed
            | Condition::UpdatesPending
            | Condition::UpdateInstallFailed
            | Condition::RebootPending
            | Condition::AppInstallFailed
            | Condition::AppUpdatesPending
            | Condition::Service
            | Condition::Process
            | Condition::EventLog
            | Condition::Ping
            | Condition::TcpPort
            | Condition::Http
            | Condition::Certificate
            | Condition::ScriptCheck => (None, None, None),
        };
        self.threshold = threshold;
        self.threshold_unit = unit;
        self.duration_minutes = duration;
        if self.condition != Condition::ScriptFailed {
            self.nonzero_exit_fails = false;
        }
        let days = |days: Option<i64>| match days {
            Some(days) if (1..=MAX_PENDING_DAYS).contains(&days) => Ok(days),
            _ => Err(invalid(format!(
                "the number of days must be 1 to {MAX_PENDING_DAYS}"
            ))),
        };
        (self.update_category, self.days) = match self.condition {
            Condition::UpdatesPending => {
                let category = self
                    .update_category
                    .as_deref()
                    .map(str::trim)
                    .filter(|category| {
                        *category == ANY_CATEGORY || UpdateCategory::parse(category).is_some()
                    })
                    .ok_or_else(|| invalid("choose which updates count: any, or a category"))?
                    .to_owned();
                (Some(category), Some(days(self.days)?))
            }
            Condition::RebootPending | Condition::AppUpdatesPending => {
                (None, Some(days(self.days)?))
            }
            _ => (None, None),
        };
        if self.target == TargetKind::Devices {
            return Err(invalid(
                "a rule covers every device, chosen companies or the devices with chosen tags",
            ));
        }
        let target = Target::from_request(
            self.target,
            Choice {
                device_ids: Vec::new(),
                company_ids: std::mem::take(&mut self.company_ids),
                tag_ids: std::mem::take(&mut self.tag_ids),
                tag_match: self.tag_match,
            },
        )
        .map_err(|error| error.with_code("invalid_rule"))?;
        self.company_ids = target.company_ids().to_vec();
        self.tag_ids = target.tag_ids().to_vec();
        self.tag_match = target.tag_match();
        self.channel_ids.sort();
        self.channel_ids.dedup();
        self.monitor = match (self.condition.is_monitor(), self.monitor.take()) {
            (true, Some(monitor)) => Some(monitor.validate(self.condition)?),
            (true, None) => return Err(invalid("say what the monitor checks")),
            (false, _) => None,
        };
        self.remediation = self
            .remediation
            .take()
            .map(|remediation| remediation.validate(self.condition, self.monitor.as_ref()))
            .transpose()?;
        if self.remediation.is_none() {
            self.notify_when = NotifyWhen::Raised;
        }
        Ok(self)
    }

    /// The toolbox scripts the rule runs.
    pub fn script_ids(&self) -> Vec<&str> {
        let check = self.monitor.as_ref().and_then(Monitor::script_id);
        let remediation = self
            .remediation
            .as_ref()
            .and_then(|remediation| remediation.script_id.as_deref());
        check.into_iter().chain(remediation).collect()
    }

    /// Whether saving the rule needs `scripts.run`: it heals itself, or its
    /// check runs with the saver's access
    /// ([`Condition::runs_with_owner_access`]).
    pub fn acts_on_devices(&self) -> bool {
        self.remediation.is_some() || self.condition.runs_with_owner_access()
    }

    /// The devices a validated rule covers.
    pub fn scope(&self) -> Target {
        match self.target {
            TargetKind::Companies => Target::Companies(self.company_ids.clone()),
            TargetKind::Tags => Target::Tags {
                ids: self.tag_ids.clone(),
                matching: self.tag_match,
            },
            TargetKind::All | TargetKind::Devices => Target::All,
        }
    }

    /// Checks that the companies, tags and channels named exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        for script_id in self.script_ids() {
            let script: Option<(bool,)> = executor
                .fetch_optional(
                    &Query::select()
                        .column(ToolboxScripts::Shared)
                        .from(ToolboxScripts::Table)
                        .and_where(Expr::col(ToolboxScripts::Id).eq(script_id))
                        .to_owned(),
                )
                .await?;
            match script {
                None => return Err(ApiError::not_found("Script not found")),
                Some((false,)) => {
                    return Err(ApiError::bad_request(
                        "rules run shared scripts only; share the script first",
                    )
                    .with_code("script_not_shared"));
                }
                Some((true,)) => {}
            }
        }
        for id in &self.channel_ids {
            crate::agents::parse_id(id, "notification channel")?;
        }
        self.scope().check_references(executor).await?;
        if !self.channel_ids.is_empty() {
            let found: Vec<(String,)> = executor
                .fetch_all(
                    &Query::select()
                        .column(NotificationChannels::Id)
                        .from(NotificationChannels::Table)
                        .and_where(
                            Expr::col(NotificationChannels::Id).is_in(self.channel_ids.clone()),
                        )
                        .to_owned(),
                )
                .await?;
            if found.len() != self.channel_ids.len() {
                return Err(ApiError::not_found(
                    "a chosen notification channel no longer exists",
                ));
            }
        }
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: String,
    name: String,
    severity: String,
    enabled: bool,
    condition: String,
    threshold: Option<f64>,
    threshold_unit: Option<String>,
    duration_minutes: Option<i64>,
    nonzero_exit_fails: bool,
    update_category: Option<String>,
    days: Option<i64>,
    target: String,
    tag_match: String,
    notify_on_resolve: bool,
    monitor_check: Option<String>,
    monitor_interval_seconds: Option<i64>,
    monitor_timeout_seconds: Option<i64>,
    monitor_failures: Option<i64>,
    remediation_action: Option<String>,
    remediation_script_id: Option<String>,
    remediation_run_as: Option<String>,
    remediation_attempts: Option<i64>,
    remediation_retry_minutes: Option<i64>,
    notify_when: String,
    updated_by_user_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl RuleRow {
    /// The stored monitor; `None` when the row's isn't valid.
    fn monitor(&self) -> Option<Option<Monitor>> {
        let Some(check) = &self.monitor_check else {
            return Some(None);
        };
        Some(Some(Monitor {
            interval_seconds: u32::try_from(self.monitor_interval_seconds?).ok()?,
            timeout_seconds: u32::try_from(self.monitor_timeout_seconds?).ok()?,
            failures: u32::try_from(self.monitor_failures?).ok()?,
            check: serde_json::from_str(check).ok()?,
        }))
    }

    /// The stored self-healing; `None` when the row's isn't valid.
    fn remediation(&self) -> Option<Option<Remediation>> {
        let Some(action) = &self.remediation_action else {
            return Some(None);
        };
        Some(Some(Remediation {
            action: RemediationKind::parse(action)?,
            script_id: self.remediation_script_id.clone(),
            run_as: self
                .remediation_run_as
                .as_deref()
                .and_then(meshrmm_protocol_types::RunAs::parse),
            attempts: u32::try_from(self.remediation_attempts?).ok()?,
            retry_minutes: u32::try_from(self.remediation_retry_minutes?).ok()?,
        }))
    }

    fn check_script_id(&self) -> Option<String> {
        let check = serde_json::from_str(self.monitor_check.as_deref()?).ok()?;
        match check {
            meshrmm_protocol_types::MonitorCheck::ScriptCheck { script_id, .. } => Some(script_id),
            _ => None,
        }
    }
}

/// Every rule, by name, or only `id`'s.
pub async fn load(executor: &mut impl Executor, id: Option<&str>) -> db::Result<Vec<Rule>> {
    let mut select = Query::select();
    select
        .columns([
            AlertRules::Id,
            AlertRules::Name,
            AlertRules::Severity,
            AlertRules::Enabled,
            AlertRules::Condition,
            AlertRules::Threshold,
            AlertRules::ThresholdUnit,
            AlertRules::DurationMinutes,
            AlertRules::NonzeroExitFails,
            AlertRules::UpdateCategory,
            AlertRules::Days,
            AlertRules::Target,
            AlertRules::TagMatch,
            AlertRules::NotifyOnResolve,
            AlertRules::MonitorCheck,
            AlertRules::MonitorIntervalSeconds,
            AlertRules::MonitorTimeoutSeconds,
            AlertRules::MonitorFailures,
            AlertRules::RemediationAction,
            AlertRules::RemediationScriptId,
            AlertRules::RemediationRunAs,
            AlertRules::RemediationAttempts,
            AlertRules::RemediationRetryMinutes,
            AlertRules::NotifyWhen,
            AlertRules::UpdatedByUserId,
            AlertRules::CreatedAt,
            AlertRules::UpdatedAt,
        ])
        .from(AlertRules::Table)
        .order_by(AlertRules::Name, Order::Asc);
    let mut companies = Query::select();
    companies
        .columns([AlertRuleCompanies::RuleId, AlertRuleCompanies::CompanyId])
        .from(AlertRuleCompanies::Table);
    let mut tags = Query::select();
    tags.columns([AlertRuleTags::RuleId, AlertRuleTags::TagId])
        .from(AlertRuleTags::Table);
    let mut channels = Query::select();
    channels
        .columns([AlertRuleChannels::RuleId, AlertRuleChannels::ChannelId])
        .from(AlertRuleChannels::Table);
    if let Some(id) = id {
        select.and_where(Expr::col(AlertRules::Id).eq(id));
        companies.and_where(Expr::col(AlertRuleCompanies::RuleId).eq(id));
        tags.and_where(Expr::col(AlertRuleTags::RuleId).eq(id));
        channels.and_where(Expr::col(AlertRuleChannels::RuleId).eq(id));
    }
    let rows: Vec<RuleRow> = executor.fetch_all(&select).await?;
    let companies = grouped(executor.fetch_all(&companies).await?);
    let tags = grouped(executor.fetch_all(&tags).await?);
    let channels = grouped(executor.fetch_all(&channels).await?);
    let scripts = scripts(executor, &rows).await?;
    let script = |id: Option<&str>| id.and_then(|id| scripts.get(id).cloned());
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let choice = Choice {
                company_ids: companies.get(&row.id).cloned().unwrap_or_default(),
                tag_ids: tags.get(&row.id).cloned().unwrap_or_default(),
                ..Choice::default()
            };
            let (
                Some(severity),
                Some(condition),
                Some(target),
                Some(monitor),
                Some(remediation),
                Some(notify_when),
            ) = (
                Severity::parse(&row.severity),
                Condition::parse(&row.condition),
                Target::from_stored(&row.target, &row.tag_match, choice),
                row.monitor(),
                row.remediation(),
                NotifyWhen::parse(&row.notify_when),
            )
            else {
                tracing::error!(rule_id = row.id, "stored alert rule is invalid");
                return None;
            };
            Some(Rule {
                check_script: script(monitor.as_ref().and_then(Monitor::script_id)),
                remediation_script: script(
                    remediation
                        .as_ref()
                        .and_then(|remediation| remediation.script_id.as_deref()),
                ),
                monitor,
                remediation,
                notify_when,
                owner_user_id: row.updated_by_user_id,
                target,
                channel_ids: channels.get(&row.id).cloned().unwrap_or_default(),
                threshold_unit: row.threshold_unit.as_deref().and_then(ThresholdUnit::parse),
                id: row.id,
                name: row.name,
                severity,
                enabled: row.enabled,
                condition,
                threshold: row.threshold,
                duration_minutes: row.duration_minutes,
                nonzero_exit_fails: row.nonzero_exit_fails,
                update_category: row.update_category,
                days: row.days,
                notify_on_resolve: row.notify_on_resolve,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

/// The scripts the rules run, by ID.
async fn scripts(
    executor: &mut impl Executor,
    rows: &[RuleRow],
) -> db::Result<HashMap<String, ScriptRef>> {
    let mut ids = rows
        .iter()
        .filter_map(RuleRow::check_script_id)
        .chain(
            rows.iter()
                .filter_map(|row| row.remediation_script_id.clone()),
        )
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let found: Vec<(String, String, bool)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    ToolboxScripts::Id,
                    ToolboxScripts::Name,
                    ToolboxScripts::Shared,
                ])
                .from(ToolboxScripts::Table)
                .and_where(Expr::col(ToolboxScripts::Id).is_in(ids))
                .to_owned(),
        )
        .await?;
    Ok(found
        .into_iter()
        .map(|(id, name, shared)| (id, ScriptRef { name, shared }))
        .collect())
}

/// The names of the rules that run `script_id`.
pub async fn using_script(
    executor: &mut impl Executor,
    script_id: &str,
) -> db::Result<Vec<String>> {
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(AlertRules::Name)
                .from(AlertRules::Table)
                .cond_where(
                    sea_query::Cond::any()
                        .add(Expr::col(AlertRules::CheckScriptId).eq(script_id))
                        .add(Expr::col(AlertRules::RemediationScriptId).eq(script_id)),
                )
                .order_by(AlertRules::Name, Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

fn grouped(pairs: Vec<(String, String)>) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (rule_id, other) in pairs {
        map.entry(rule_id).or_default().push(other);
    }
    for values in map.values_mut() {
        values.sort();
    }
    map
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<Rule>> {
    Ok(load(executor, Some(id)).await?.into_iter().next())
}

/// Writes the rule `id` from `input`: a new one, or over the existing one.
pub async fn save(
    executor: &mut impl Executor,
    id: &str,
    input: &RuleInput,
    user_id: &str,
    now: i64,
    create: bool,
) -> db::Result<()> {
    let monitor = input.monitor.as_ref();
    let remediation = input.remediation.as_ref();
    let values: [(AlertRules, sea_query::Value); 26] = [
        (AlertRules::Name, input.name.as_str().into()),
        (AlertRules::Severity, input.severity.as_str().into()),
        (AlertRules::Enabled, input.enabled.into()),
        (AlertRules::Condition, input.condition.as_str().into()),
        (AlertRules::Threshold, input.threshold.into()),
        (
            AlertRules::ThresholdUnit,
            input.threshold_unit.map(ThresholdUnit::as_str).into(),
        ),
        (AlertRules::DurationMinutes, input.duration_minutes.into()),
        (
            AlertRules::NonzeroExitFails,
            input.nonzero_exit_fails.into(),
        ),
        (
            AlertRules::UpdateCategory,
            input.update_category.clone().into(),
        ),
        (AlertRules::Days, input.days.into()),
        (AlertRules::Target, input.target.as_str().into()),
        (AlertRules::TagMatch, input.tag_match.as_str().into()),
        (AlertRules::NotifyOnResolve, input.notify_on_resolve.into()),
        (
            AlertRules::MonitorCheck,
            monitor
                .map(|monitor| serde_json::to_string(&monitor.check).expect("checks serialize"))
                .into(),
        ),
        (
            AlertRules::MonitorIntervalSeconds,
            monitor
                .map(|monitor| i64::from(monitor.interval_seconds))
                .into(),
        ),
        (
            AlertRules::MonitorTimeoutSeconds,
            monitor
                .map(|monitor| i64::from(monitor.timeout_seconds))
                .into(),
        ),
        (
            AlertRules::MonitorFailures,
            monitor.map(|monitor| i64::from(monitor.failures)).into(),
        ),
        (
            AlertRules::CheckScriptId,
            monitor
                .and_then(Monitor::script_id)
                .map(str::to_owned)
                .into(),
        ),
        (
            AlertRules::RemediationAction,
            remediation
                .map(|remediation| remediation.action.as_str())
                .into(),
        ),
        (
            AlertRules::RemediationScriptId,
            remediation
                .and_then(|remediation| remediation.script_id.clone())
                .into(),
        ),
        (
            AlertRules::RemediationRunAs,
            remediation
                .and_then(|remediation| remediation.run_as)
                .map(meshrmm_protocol_types::RunAs::as_str)
                .into(),
        ),
        (
            AlertRules::RemediationAttempts,
            remediation
                .map(|remediation| i64::from(remediation.attempts))
                .into(),
        ),
        (
            AlertRules::RemediationRetryMinutes,
            remediation
                .map(|remediation| i64::from(remediation.retry_minutes))
                .into(),
        ),
        (AlertRules::NotifyWhen, input.notify_when.as_str().into()),
        (AlertRules::UpdatedAt, now.into()),
        (AlertRules::UpdatedByUserId, user_id.into()),
    ];
    if create {
        let (mut columns, mut row): (Vec<_>, Vec<_>) = values
            .into_iter()
            .map(|(column, value)| (column, Expr::val(value)))
            .unzip();
        columns.extend([AlertRules::Id, AlertRules::CreatedAt]);
        row.extend([Expr::val(id), Expr::val(now)]);
        executor
            .execute(
                &Query::insert()
                    .into_table(AlertRules::Table)
                    .columns(columns)
                    .values_panic(row)
                    .to_owned(),
            )
            .await?;
    } else {
        executor
            .execute(
                &Query::update()
                    .table(AlertRules::Table)
                    .values(values.map(|(column, value)| (column, Expr::val(value))))
                    .and_where(Expr::col(AlertRules::Id).eq(id))
                    .to_owned(),
            )
            .await?;
        executor
            .execute(
                &Query::delete()
                    .from_table(AlertRuleCompanies::Table)
                    .and_where(Expr::col(AlertRuleCompanies::RuleId).eq(id))
                    .to_owned(),
            )
            .await?;
        executor
            .execute(
                &Query::delete()
                    .from_table(AlertRuleTags::Table)
                    .and_where(Expr::col(AlertRuleTags::RuleId).eq(id))
                    .to_owned(),
            )
            .await?;
        executor
            .execute(
                &Query::delete()
                    .from_table(AlertRuleChannels::Table)
                    .and_where(Expr::col(AlertRuleChannels::RuleId).eq(id))
                    .to_owned(),
            )
            .await?;
    }
    if !input.company_ids.is_empty() {
        let mut insert = Query::insert();
        insert
            .into_table(AlertRuleCompanies::Table)
            .columns([AlertRuleCompanies::RuleId, AlertRuleCompanies::CompanyId]);
        for company_id in &input.company_ids {
            insert.values_panic([id.into(), company_id.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    if !input.tag_ids.is_empty() {
        let mut insert = Query::insert();
        insert
            .into_table(AlertRuleTags::Table)
            .columns([AlertRuleTags::RuleId, AlertRuleTags::TagId]);
        for tag_id in &input.tag_ids {
            insert.values_panic([id.into(), tag_id.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    if !input.channel_ids.is_empty() {
        let mut insert = Query::insert();
        insert
            .into_table(AlertRuleChannels::Table)
            .columns([AlertRuleChannels::RuleId, AlertRuleChannels::ChannelId]);
        for channel_id in &input.channel_ids {
            insert.values_panic([id.into(), channel_id.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn input(value: serde_json::Value) -> Result<RuleInput, ApiError> {
        serde_json::from_value::<RuleInput>(value)
            .unwrap()
            .validate()
    }

    #[test]
    fn rules_keep_only_the_fields_their_condition_uses() {
        let rule = input(json!({
            "name": "  Offline  ",
            "severity": "critical",
            "condition": "device_offline",
            "threshold": 50,
            "threshold_unit": "percent",
            "duration_minutes": 30,
            "nonzero_exit_fails": true,
            "company_ids": ["x"],
        }))
        .unwrap();
        assert_eq!(rule.name, "Offline");
        assert_eq!(rule.threshold, None);
        assert_eq!(rule.threshold_unit, None);
        assert_eq!(rule.duration_minutes, Some(30));
        assert!(!rule.nonzero_exit_fails);
        assert_eq!(rule.scope(), Target::All);
        assert!(rule.company_ids.is_empty());

        let disk = input(json!({
            "name": "Disk", "severity": "warning", "condition": "disk_space",
            "threshold": 10, "threshold_unit": "gb", "duration_minutes": 5,
        }))
        .unwrap();
        assert_eq!(disk.threshold, Some(10.0));
        assert_eq!(disk.duration_minutes, None);
    }

    #[test]
    fn thresholds_and_durations_are_checked() {
        let base = |condition: &str, threshold: serde_json::Value, minutes: serde_json::Value| {
            input(json!({
                "name": "R", "severity": "info", "condition": condition,
                "threshold": threshold, "threshold_unit": "percent", "duration_minutes": minutes,
            }))
        };
        assert!(base("cpu", json!(90), json!(5)).is_ok());
        assert!(base("cpu", json!(100), json!(5)).is_err());
        assert!(base("cpu", json!(0), json!(5)).is_err());
        assert!(base("cpu", json!(90), json!(61)).is_err());
        assert!(base("memory", json!(90), json!(null)).is_err());
        assert!(base("device_offline", json!(null), json!(0)).is_err());
        assert!(base("device_offline", json!(null), json!(10080)).is_ok());
        assert!(base("disk_space", json!(5), json!(null)).is_ok());
        assert!(
            input(json!({ "name": "R", "severity": "info", "condition": "disk_space", "threshold": 5 }))
                .is_err(),
            "a disk rule needs a unit"
        );
        assert!(
            input(json!({ "name": "R", "severity": "info", "condition": "script_failed", "target": "companies" }))
                .is_err(),
            "a rule for chosen companies needs one"
        );
        assert!(
            input(json!({ "name": "R", "severity": "info", "condition": "script_failed", "target": "devices" }))
                .is_err(),
            "rules don't cover chosen devices"
        );
        let tagged = input(json!({
            "name": "R", "severity": "info", "condition": "script_failed", "target": "tags",
            "tag_ids": ["00000000-0000-4000-8000-00000000000b", "00000000-0000-4000-8000-00000000000a"],
            "company_ids": ["00000000-0000-4000-8000-00000000000c"], "tag_match": "all",
        }))
        .unwrap();
        assert!(tagged.company_ids.is_empty());
        assert_eq!(
            tagged.scope(),
            Target::Tags {
                ids: vec![
                    "00000000-0000-4000-8000-00000000000a".into(),
                    "00000000-0000-4000-8000-00000000000b".into()
                ],
                matching: TagMatch::All
            }
        );
        assert!(
            input(json!({ "name": "", "severity": "info", "condition": "script_failed" })).is_err()
        );
    }

    #[test]
    fn rules_describe_themselves_and_cover_their_companies() {
        let rule = Rule {
            id: "r".into(),
            name: "CPU".into(),
            severity: Severity::Warning,
            enabled: true,
            condition: Condition::Cpu,
            threshold: Some(90.5),
            threshold_unit: None,
            duration_minutes: Some(1),
            nonzero_exit_fails: false,
            update_category: None,
            days: None,
            target: Target::Companies(vec!["acme".into()]),
            channel_ids: vec![],
            notify_on_resolve: false,
            monitor: None,
            remediation: None,
            notify_when: NotifyWhen::Raised,
            owner_user_id: None,
            check_script: None,
            remediation_script: None,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(rule.describe(), "CPU above 90.5% for 1 minute");
        let device = |company: Option<&str>| targets::Device {
            id: "d".into(),
            company_id: company.map(Into::into),
            tag_ids: vec![],
        };
        assert!(rule.covers(&device(Some("acme"))));
        assert!(!rule.covers(&device(Some("other"))));
        assert!(!rule.covers(&device(None)));
        assert!(
            Rule {
                target: Target::All,
                ..rule.clone()
            }
            .covers(&device(None))
        );
        let disk = Rule {
            condition: Condition::DiskSpace,
            threshold: Some(10.0),
            threshold_unit: Some(ThresholdUnit::Gb),
            ..rule
        };
        assert_eq!(disk.describe(), "Free disk space below 10 GB");
        assert_eq!(format_number(12.0), "12");
        assert_eq!(format_number(0.25), "0.25");
    }
}
