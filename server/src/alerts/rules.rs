//! Alert rules: what raises alerts, how severe they are, which devices they
//! cover and which notification channels they notify.
use std::collections::HashMap;

use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        self, Executor,
        tables::{
            AlertRuleChannels, AlertRuleCompanies, AlertRules, Companies, NotificationChannels,
        },
    },
    http::ApiError,
};

pub const MAX_NAME_LENGTH: usize = 120;
/// The longest a device may be offline before a rule raises an alert.
pub const MAX_OFFLINE_MINUTES: i64 = 7 * 24 * 60;
/// The longest a load must be sustained; the engine keeps this much of each
/// device's readings.
pub const MAX_SUSTAINED_MINUTES: i64 = 60;
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
}

impl Condition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeviceOffline => "device_offline",
            Self::DiskSpace => "disk_space",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::ScriptFailed => "script_failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "device_offline" => Some(Self::DeviceOffline),
            "disk_space" => Some(Self::DiskSpace),
            "cpu" => Some(Self::Cpu),
            "memory" => Some(Self::Memory),
            "script_failed" => Some(Self::ScriptFailed),
            _ => None,
        }
    }

    /// Whether the server resolves the alert once the condition passes;
    /// script failures stay until a person resolves them.
    pub fn clears_itself(self) -> bool {
        self != Self::ScriptFailed
    }
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

/// A rule, with the companies it covers and the channels it notifies.
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
    pub all_companies: bool,
    pub company_ids: Vec<String>,
    pub channel_ids: Vec<String>,
    pub notify_on_resolve: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Rule {
    /// Whether the rule covers a device in `company_id`.
    pub fn covers(&self, company_id: Option<&str>) -> bool {
        self.all_companies
            || company_id.is_some_and(|company| self.company_ids.iter().any(|id| id == company))
    }

    /// The condition in words, such as "CPU above 90% for 5 minutes".
    pub fn describe(&self) -> String {
        let threshold = format_number(self.threshold.unwrap_or_default());
        let minutes = plural(self.duration_minutes.unwrap_or_default(), "minute");
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
    #[serde(default = "enabled_by_default")]
    pub all_companies: bool,
    #[serde(default)]
    pub company_ids: Vec<String>,
    #[serde(default)]
    pub channel_ids: Vec<String>,
    #[serde(default)]
    pub notify_on_resolve: bool,
}

fn enabled_by_default() -> bool {
    true
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
            Condition::ScriptFailed => (None, None, None),
        };
        self.threshold = threshold;
        self.threshold_unit = unit;
        self.duration_minutes = duration;
        if self.condition != Condition::ScriptFailed {
            self.nonzero_exit_fails = false;
        }
        if self.all_companies {
            self.company_ids.clear();
        } else if self.company_ids.is_empty() {
            return Err(invalid("choose at least one company, or every device"));
        }
        self.company_ids.sort();
        self.company_ids.dedup();
        self.channel_ids.sort();
        self.channel_ids.dedup();
        Ok(self)
    }

    /// Checks that the companies and channels named exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        for id in self.company_ids.iter().chain(&self.channel_ids) {
            crate::agents::parse_id(id, "company or channel")?;
        }
        if !self.company_ids.is_empty() {
            let found: Vec<(String,)> = executor
                .fetch_all(
                    &Query::select()
                        .column(Companies::Id)
                        .from(Companies::Table)
                        .and_where(Expr::col(Companies::Id).is_in(self.company_ids.clone()))
                        .to_owned(),
                )
                .await?;
            if found.len() != self.company_ids.len() {
                return Err(ApiError::not_found("a chosen company no longer exists"));
            }
        }
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
    all_companies: bool,
    notify_on_resolve: bool,
    created_at: i64,
    updated_at: i64,
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
            AlertRules::AllCompanies,
            AlertRules::NotifyOnResolve,
            AlertRules::CreatedAt,
            AlertRules::UpdatedAt,
        ])
        .from(AlertRules::Table)
        .order_by(AlertRules::Name, Order::Asc);
    let mut companies = Query::select();
    companies
        .columns([AlertRuleCompanies::RuleId, AlertRuleCompanies::CompanyId])
        .from(AlertRuleCompanies::Table);
    let mut channels = Query::select();
    channels
        .columns([AlertRuleChannels::RuleId, AlertRuleChannels::ChannelId])
        .from(AlertRuleChannels::Table);
    if let Some(id) = id {
        select.and_where(Expr::col(AlertRules::Id).eq(id));
        companies.and_where(Expr::col(AlertRuleCompanies::RuleId).eq(id));
        channels.and_where(Expr::col(AlertRuleChannels::RuleId).eq(id));
    }
    let rows: Vec<RuleRow> = executor.fetch_all(&select).await?;
    let companies = grouped(executor.fetch_all(&companies).await?);
    let channels = grouped(executor.fetch_all(&channels).await?);
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let (Some(severity), Some(condition)) = (
                Severity::parse(&row.severity),
                Condition::parse(&row.condition),
            ) else {
                tracing::error!(rule_id = row.id, "stored alert rule is invalid");
                return None;
            };
            Some(Rule {
                company_ids: companies.get(&row.id).cloned().unwrap_or_default(),
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
                all_companies: row.all_companies,
                notify_on_resolve: row.notify_on_resolve,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
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
    let values: [(AlertRules, sea_query::Value); 12] = [
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
        (AlertRules::AllCompanies, input.all_companies.into()),
        (AlertRules::NotifyOnResolve, input.notify_on_resolve.into()),
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
        assert!(rule.all_companies && rule.company_ids.is_empty());

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
            input(json!({ "name": "R", "severity": "info", "condition": "script_failed", "all_companies": false }))
                .is_err(),
            "a rule for chosen companies needs one"
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
            all_companies: false,
            company_ids: vec!["acme".into()],
            channel_ids: vec![],
            notify_on_resolve: false,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(rule.describe(), "CPU above 90.5% for 1 minute");
        assert!(rule.covers(Some("acme")));
        assert!(!rule.covers(Some("other")));
        assert!(!rule.covers(None));
        assert!(
            Rule {
                all_companies: true,
                ..rule.clone()
            }
            .covers(None)
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
