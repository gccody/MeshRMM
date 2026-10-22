//! Automation policies: a shared script the server runs by itself, on a
//! schedule or on each newly enrolled device, as a job.
//!
//! A policy runs with the access of the user who last saved it: when it
//! fires, that user must still be enabled and hold `automation.manage` and
//! `scripts.run`, and its script must still be shared. Otherwise it doesn't
//! fire and says why. Policies use shared scripts only, so everyone who
//! manages policies can see what each one runs.
//!
//! Firing claims the due time in the same transaction that creates the job,
//! so a restart (or a second pass) can't fire it twice. A firing missed while
//! the server was down runs when it starts if it is at most
//! [`MISSED_GRACE_MS`] late; later than that, interval schedules still run
//! once, and times of day are skipped until their next time. Missed times
//! never run more than once.
use std::collections::HashMap;

use meshrmm_protocol_types::{RunAs, ScriptLanguage};
use sea_query::{Alias, Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    jobs::{self, DeviceState, NewJob, Offline, Payload, Target, TargetKind},
    schedule::{self, MAX_INTERVAL_MINUTES, MIN_INTERVAL_MINUTES, Trigger, Weekdays},
};
use crate::{
    audit::{self, Actor, Target as AuditTarget},
    db::{
        self, Executor,
        tables::{
            Agents, AutomationPolicies, AutomationPolicyChannels, AutomationPolicyCompanies,
            AutomationPolicyDevices, Companies, JobDevices, Jobs, NotificationChannels,
            ToolboxScripts, Users,
        },
    },
    http::{ApiError, AppState},
    rbac::{self, Permission},
    time::HOUR_MS,
    users,
};

pub const MAX_NAME_LENGTH: usize = 120;
/// A firing this late (the server was down) still runs.
pub const MISSED_GRACE_MS: i64 = HOUR_MS;
pub const DEFAULT_CATCH_UP_HOURS: i64 = 24;

const SUPERSEDED: &str = "The policy ran again before the device connected";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Interval,
    Daily,
    Weekly,
    Once,
    Enroll,
}

fn enabled_by_default() -> bool {
    true
}

fn default_catch_up_hours() -> i64 {
    DEFAULT_CATCH_UP_HOURS
}

/// A policy as the website sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyInput {
    pub name: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub script_id: String,
    pub run_as: RunAs,
    pub target: TargetKind,
    #[serde(default)]
    pub device_ids: Vec<String>,
    #[serde(default)]
    pub company_ids: Vec<String>,
    pub trigger: TriggerKind,
    #[serde(default)]
    pub interval_minutes: Option<i64>,
    /// `HH:MM`, for daily and weekly policies.
    #[serde(default)]
    pub time: Option<String>,
    /// ISO day numbers, Monday = 1, for weekly policies.
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// `YYYY-MM-DDTHH:MM`, for a policy that runs once.
    #[serde(default)]
    pub run_at: Option<String>,
    /// An IANA time zone, for daily, weekly and once policies.
    #[serde(default)]
    pub time_zone: Option<String>,
    pub offline: Offline,
    #[serde(default = "default_catch_up_hours")]
    pub catch_up_hours: i64,
    #[serde(default)]
    pub channel_ids: Vec<String>,
}

/// A checked policy, ready to save.
#[derive(Debug, Clone)]
pub struct Valid {
    pub name: String,
    pub enabled: bool,
    pub script_id: String,
    pub run_as: RunAs,
    pub target: Target,
    pub trigger: Trigger,
    pub offline: Offline,
    pub catch_up_hours: i64,
    pub channel_ids: Vec<String>,
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_policy")
}

impl PolicyInput {
    pub fn validate(self) -> Result<Valid, ApiError> {
        let name = self.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_NAME_LENGTH
            || name.chars().any(char::is_control)
        {
            return Err(invalid(format!(
                "the policy name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
            )));
        }
        crate::agents::parse_id(&self.script_id, "script")?;
        let zone = || {
            self.time_zone
                .as_deref()
                .and_then(schedule::parse_zone)
                .ok_or_else(|| invalid("choose a time zone such as Europe/London"))
        };
        let time = || {
            self.time
                .as_deref()
                .and_then(schedule::parse_time_of_day)
                .ok_or_else(|| invalid("the time must be HH:MM, from 00:00 to 23:59"))
        };
        let trigger = match self.trigger {
            TriggerKind::Interval => match self.interval_minutes {
                Some(minutes)
                    if (MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&minutes) =>
                {
                    Trigger::Interval { minutes }
                }
                _ => {
                    return Err(invalid(format!(
                        "the interval must be {MIN_INTERVAL_MINUTES} minutes to {} days",
                        MAX_INTERVAL_MINUTES / (24 * 60)
                    )));
                }
            },
            TriggerKind::Daily => Trigger::Daily {
                time: time()?,
                zone: zone()?,
            },
            TriggerKind::Weekly => Trigger::Weekly {
                days: Weekdays::from_days(&self.weekdays)
                    .ok_or_else(|| invalid("choose the days of the week it runs on"))?,
                time: time()?,
                zone: zone()?,
            },
            TriggerKind::Once => Trigger::Once {
                at: self
                    .run_at
                    .as_deref()
                    .and_then(schedule::parse_local_date_time)
                    .ok_or_else(|| invalid("the date and time must be YYYY-MM-DDTHH:MM"))?,
                zone: zone()?,
            },
            TriggerKind::Enroll => Trigger::Enroll,
        };
        let target = Target::from_request(self.target, self.device_ids, self.company_ids)
            .map_err(|_| invalid("choose the devices or companies it runs on"))?;
        if trigger == Trigger::Enroll && matches!(target, Target::Devices(_)) {
            return Err(invalid(
                "a policy for newly enrolled devices runs on every device or on companies",
            ));
        }
        if !(1..=jobs::MAX_WAIT_HOURS).contains(&self.catch_up_hours) {
            return Err(invalid(format!(
                "offline devices can be caught up for 1 to {} hours",
                jobs::MAX_WAIT_HOURS
            )));
        }
        let mut channel_ids = self.channel_ids;
        for id in &channel_ids {
            crate::agents::parse_id(id, "notification channel")?;
        }
        channel_ids.sort();
        channel_ids.dedup();
        Ok(Valid {
            name: name.to_owned(),
            enabled: self.enabled,
            script_id: self.script_id,
            run_as: self.run_as,
            target,
            trigger,
            offline: self.offline,
            catch_up_hours: self.catch_up_hours,
            channel_ids,
        })
    }
}

impl Valid {
    /// Checks that the script is shared and the companies, devices and
    /// channels exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        let script: Option<(bool,)> = executor
            .fetch_optional(
                &Query::select()
                    .column(ToolboxScripts::Shared)
                    .from(ToolboxScripts::Table)
                    .and_where(Expr::col(ToolboxScripts::Id).eq(self.script_id.as_str()))
                    .to_owned(),
            )
            .await?;
        match script {
            None => return Err(ApiError::not_found("Script not found")),
            Some((false,)) => {
                return Err(ApiError::bad_request(
                    "policies run shared scripts only; share the script first",
                )
                .with_code("script_not_shared"));
            }
            Some((true,)) => {}
        }
        let count = |found: usize, wanted: usize, what: &'static str| {
            if found == wanted {
                Ok(())
            } else {
                Err(ApiError::not_found(format!(
                    "a chosen {what} no longer exists"
                )))
            }
        };
        let companies = self.target.company_ids();
        if !companies.is_empty() {
            let found: Vec<(String,)> = executor
                .fetch_all(
                    &Query::select()
                        .column(Companies::Id)
                        .from(Companies::Table)
                        .and_where(Expr::col(Companies::Id).is_in(companies.iter().cloned()))
                        .to_owned(),
                )
                .await?;
            count(found.len(), companies.len(), "company")?;
        }
        let devices = self.target.device_ids();
        for chunk in devices.chunks(500) {
            let found: Vec<(String,)> = executor
                .fetch_all(
                    &crate::agents::active()
                        .and_where(Expr::col(Agents::Id).is_in(chunk.iter().cloned()))
                        .to_owned(),
                )
                .await?;
            count(found.len(), chunk.len(), "device")?;
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
            count(found.len(), self.channel_ids.len(), "notification channel")?;
        }
        Ok(())
    }
}

/// A policy as stored, with its script's name.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub script_id: String,
    /// `None` once the script is gone.
    pub script_name: Option<String>,
    pub script_language: Option<ScriptLanguage>,
    pub script_shared: bool,
    pub run_as: RunAs,
    pub target: TargetKind,
    pub device_ids: Vec<String>,
    pub company_ids: Vec<String>,
    pub trigger: Trigger,
    pub offline: Offline,
    pub catch_up_hours: i64,
    pub channel_ids: Vec<String>,
    pub owner_user_id: String,
    pub owner: String,
    pub next_run_at: Option<i64>,
    pub interval_anchor_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_job_id: Option<String>,
    pub last_problem: Option<String>,
    pub last_problem_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Policy {
    /// Whether the policy covers a device in `company_id`.
    pub fn covers(&self, device_id: &str, company_id: Option<&str>) -> bool {
        match self.target {
            TargetKind::All => true,
            TargetKind::Companies => {
                company_id.is_some_and(|company| self.company_ids.iter().any(|id| id == company))
            }
            TargetKind::Devices => self.device_ids.iter().any(|id| id == device_id),
        }
    }

    fn job_target(&self) -> Target {
        match self.target {
            TargetKind::All => Target::All,
            TargetKind::Companies => Target::Companies(self.company_ids.clone()),
            TargetKind::Devices => Target::Devices(self.device_ids.clone()),
        }
    }

    /// Recorded in the audit log as starting the policy's jobs.
    pub fn actor(&self) -> Actor {
        Actor {
            user_id: None,
            label: format!("policy \u{201c}{}\u{201d}", self.name),
            ip: None,
        }
    }
}

/// A policy as the website shows it.
#[derive(Debug, Serialize)]
pub struct PolicyView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub script_id: String,
    pub script_name: Option<String>,
    pub script_language: Option<ScriptLanguage>,
    pub script_shared: bool,
    pub run_as: RunAs,
    pub target: TargetKind,
    pub device_ids: Vec<String>,
    pub company_ids: Vec<String>,
    pub trigger: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_minutes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    pub offline: Offline,
    pub catch_up_hours: i64,
    pub channel_ids: Vec<String>,
    /// The user whose access it runs with.
    pub owner: String,
    pub owned_by_you: bool,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_job_id: Option<String>,
    pub last_problem: Option<String>,
    pub last_problem_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Policy {
    pub fn view(self, user_id: &str) -> PolicyView {
        let (interval_minutes, time, weekdays, run_at, time_zone) = match self.trigger {
            Trigger::Interval { minutes } => (Some(minutes), None, None, None, None),
            Trigger::Daily { time, zone } => (
                None,
                Some(schedule::format_time_of_day(time)),
                None,
                None,
                Some(zone.name().to_owned()),
            ),
            Trigger::Weekly { days, time, zone } => (
                None,
                Some(schedule::format_time_of_day(time)),
                Some(days.days()),
                None,
                Some(zone.name().to_owned()),
            ),
            Trigger::Once { at, zone } => (
                None,
                None,
                None,
                Some(schedule::format_local_date_time(at)),
                Some(zone.name().to_owned()),
            ),
            Trigger::Enroll => (None, None, None, None, None),
        };
        PolicyView {
            owned_by_you: self.owner_user_id == user_id,
            trigger: self.trigger.kind(),
            id: self.id,
            name: self.name,
            enabled: self.enabled,
            script_id: self.script_id,
            script_name: self.script_name,
            script_language: self.script_language,
            script_shared: self.script_shared,
            run_as: self.run_as,
            target: self.target,
            device_ids: self.device_ids,
            company_ids: self.company_ids,
            interval_minutes,
            time,
            weekdays,
            run_at,
            time_zone,
            offline: self.offline,
            catch_up_hours: self.catch_up_hours,
            channel_ids: self.channel_ids,
            owner: self.owner,
            next_run_at: self.next_run_at,
            last_run_at: self.last_run_at,
            last_job_id: self.last_job_id,
            last_problem: self.last_problem,
            last_problem_at: self.last_problem_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct PolicyRow {
    id: String,
    name: String,
    enabled: bool,
    script_id: String,
    script_name: Option<String>,
    script_language: Option<String>,
    script_shared: Option<bool>,
    run_as: String,
    target: String,
    trigger_type: String,
    interval_minutes: Option<i64>,
    time_of_day: Option<String>,
    weekdays: Option<i64>,
    run_at: Option<String>,
    time_zone: Option<String>,
    offline: String,
    catch_up_hours: i64,
    owner_user_id: String,
    owner: Option<String>,
    next_run_at: Option<i64>,
    interval_anchor_at: Option<i64>,
    last_run_at: Option<i64>,
    last_job_id: Option<String>,
    last_problem: Option<String>,
    last_problem_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

fn trigger_of(row: &PolicyRow) -> Option<Trigger> {
    let zone = || row.time_zone.as_deref().and_then(schedule::parse_zone);
    let time = || {
        row.time_of_day
            .as_deref()
            .and_then(schedule::parse_time_of_day)
    };
    Some(match row.trigger_type.as_str() {
        "interval" => Trigger::Interval {
            minutes: row.interval_minutes?,
        },
        "daily" => Trigger::Daily {
            time: time()?,
            zone: zone()?,
        },
        "weekly" => Trigger::Weekly {
            days: Weekdays::from_bits(row.weekdays?)?,
            time: time()?,
            zone: zone()?,
        },
        "once" => Trigger::Once {
            at: schedule::parse_local_date_time(row.run_at.as_deref()?)?,
            zone: zone()?,
        },
        "enroll" => Trigger::Enroll,
        _ => return None,
    })
}

/// Every policy, by name, or only `id`'s.
pub async fn load(executor: &mut impl Executor, id: Option<&str>) -> db::Result<Vec<Policy>> {
    let mut select = Query::select();
    select
        .columns([
            (AutomationPolicies::Table, AutomationPolicies::Id),
            (AutomationPolicies::Table, AutomationPolicies::Name),
            (AutomationPolicies::Table, AutomationPolicies::Enabled),
            (AutomationPolicies::Table, AutomationPolicies::ScriptId),
        ])
        .expr_as(
            Expr::col((ToolboxScripts::Table, ToolboxScripts::Name)),
            "script_name",
        )
        .expr_as(
            Expr::col((ToolboxScripts::Table, ToolboxScripts::Language)),
            "script_language",
        )
        .expr_as(
            Expr::col((ToolboxScripts::Table, ToolboxScripts::Shared)),
            "script_shared",
        )
        .columns([
            (AutomationPolicies::Table, AutomationPolicies::RunAs),
            (AutomationPolicies::Table, AutomationPolicies::Target),
            (AutomationPolicies::Table, AutomationPolicies::TriggerType),
            (
                AutomationPolicies::Table,
                AutomationPolicies::IntervalMinutes,
            ),
            (AutomationPolicies::Table, AutomationPolicies::TimeOfDay),
            (AutomationPolicies::Table, AutomationPolicies::Weekdays),
            (AutomationPolicies::Table, AutomationPolicies::RunAt),
            (AutomationPolicies::Table, AutomationPolicies::TimeZone),
            (AutomationPolicies::Table, AutomationPolicies::Offline),
            (AutomationPolicies::Table, AutomationPolicies::CatchUpHours),
            (AutomationPolicies::Table, AutomationPolicies::OwnerUserId),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "owner",
        )
        .columns([
            (AutomationPolicies::Table, AutomationPolicies::NextRunAt),
            (
                AutomationPolicies::Table,
                AutomationPolicies::IntervalAnchorAt,
            ),
            (AutomationPolicies::Table, AutomationPolicies::LastRunAt),
            (AutomationPolicies::Table, AutomationPolicies::LastJobId),
            (AutomationPolicies::Table, AutomationPolicies::LastProblem),
            (AutomationPolicies::Table, AutomationPolicies::LastProblemAt),
            (AutomationPolicies::Table, AutomationPolicies::CreatedAt),
            (AutomationPolicies::Table, AutomationPolicies::UpdatedAt),
        ])
        .from(AutomationPolicies::Table)
        .left_join(
            ToolboxScripts::Table,
            Expr::col((ToolboxScripts::Table, ToolboxScripts::Id))
                .equals((AutomationPolicies::Table, AutomationPolicies::ScriptId)),
        )
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((AutomationPolicies::Table, AutomationPolicies::OwnerUserId)),
        )
        .order_by(
            (AutomationPolicies::Table, AutomationPolicies::Name),
            Order::Asc,
        );
    let mut companies = Query::select();
    companies
        .columns([
            AutomationPolicyCompanies::PolicyId,
            AutomationPolicyCompanies::CompanyId,
        ])
        .from(AutomationPolicyCompanies::Table);
    let mut devices = Query::select();
    devices
        .columns([
            AutomationPolicyDevices::PolicyId,
            AutomationPolicyDevices::DeviceId,
        ])
        .from(AutomationPolicyDevices::Table);
    let mut channels = Query::select();
    channels
        .columns([
            AutomationPolicyChannels::PolicyId,
            AutomationPolicyChannels::ChannelId,
        ])
        .from(AutomationPolicyChannels::Table);
    if let Some(id) = id {
        select.and_where(Expr::col((AutomationPolicies::Table, AutomationPolicies::Id)).eq(id));
        companies.and_where(Expr::col(AutomationPolicyCompanies::PolicyId).eq(id));
        devices.and_where(Expr::col(AutomationPolicyDevices::PolicyId).eq(id));
        channels.and_where(Expr::col(AutomationPolicyChannels::PolicyId).eq(id));
    }
    let rows: Vec<PolicyRow> = executor.fetch_all(&select).await?;
    let companies = grouped(executor.fetch_all(&companies).await?);
    let devices = grouped(executor.fetch_all(&devices).await?);
    let channels = grouped(executor.fetch_all(&channels).await?);
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let (Some(trigger), Some(run_as), Some(target), Some(offline)) = (
                trigger_of(&row),
                RunAs::parse(&row.run_as),
                TargetKind::parse(&row.target),
                Offline::parse(&row.offline),
            ) else {
                tracing::error!(policy_id = row.id, "stored automation policy is invalid");
                return None;
            };
            Some(Policy {
                company_ids: companies.get(&row.id).cloned().unwrap_or_default(),
                device_ids: devices.get(&row.id).cloned().unwrap_or_default(),
                channel_ids: channels.get(&row.id).cloned().unwrap_or_default(),
                script_language: row
                    .script_language
                    .as_deref()
                    .and_then(ScriptLanguage::parse),
                script_shared: row.script_shared.unwrap_or(false),
                owner: row.owner.unwrap_or_else(|| "A removed user".to_owned()),
                id: row.id,
                name: row.name,
                enabled: row.enabled,
                script_id: row.script_id,
                script_name: row.script_name,
                run_as,
                target,
                trigger,
                offline,
                catch_up_hours: row.catch_up_hours,
                owner_user_id: row.owner_user_id,
                next_run_at: row.next_run_at,
                interval_anchor_at: row.interval_anchor_at,
                last_run_at: row.last_run_at,
                last_job_id: row.last_job_id,
                last_problem: row.last_problem,
                last_problem_at: row.last_problem_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

fn grouped(pairs: Vec<(String, String)>) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (policy_id, other) in pairs {
        map.entry(policy_id).or_default().push(other);
    }
    for values in map.values_mut() {
        values.sort();
    }
    map
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<Policy>> {
    Ok(load(executor, Some(id)).await?.into_iter().next())
}

/// The policies that run `script_id`, by name.
pub async fn using_script(
    executor: &mut impl Executor,
    script_id: &str,
) -> db::Result<Vec<String>> {
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(AutomationPolicies::Name)
                .from(AutomationPolicies::Table)
                .and_where(Expr::col(AutomationPolicies::ScriptId).eq(script_id))
                .order_by(AutomationPolicies::Name, Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// When a saved policy fires next, counting intervals from `now`: `None`
/// when it is off or fires on enrollment.
pub fn next_run(policy: &Valid, now: i64) -> Option<i64> {
    if !policy.enabled {
        return None;
    }
    policy.trigger.next_after(now, now)
}

/// Writes the policy `id` from `policy`, owned by `owner_user_id`: a new
/// one, or over the existing one. Its schedule starts over from `now`.
/// The tables that link a policy to its companies, devices and channels,
/// and the column each links to.
const LINK_TABLES: [&str; 3] = [
    "automation_policy_companies",
    "automation_policy_devices",
    "automation_policy_channels",
];
const LINK_COLUMNS: [&str; 3] = ["company_id", "device_id", "channel_id"];

pub async fn save(
    executor: &mut impl Executor,
    id: &str,
    policy: &Valid,
    owner_user_id: &str,
    now: i64,
    create: bool,
) -> db::Result<()> {
    let (interval_minutes, time_of_day, weekdays, run_at, time_zone) = match policy.trigger {
        Trigger::Interval { minutes } => (Some(minutes), None, None, None, None),
        Trigger::Daily { time, zone } => (
            None,
            Some(schedule::format_time_of_day(time)),
            None,
            None,
            Some(zone.name().to_owned()),
        ),
        Trigger::Weekly { days, time, zone } => (
            None,
            Some(schedule::format_time_of_day(time)),
            Some(days.bits()),
            None,
            Some(zone.name().to_owned()),
        ),
        Trigger::Once { at, zone } => (
            None,
            None,
            None,
            Some(schedule::format_local_date_time(at)),
            Some(zone.name().to_owned()),
        ),
        Trigger::Enroll => (None, None, None, None, None),
    };
    let values: [(AutomationPolicies, sea_query::Value); 17] = [
        (AutomationPolicies::Name, policy.name.as_str().into()),
        (AutomationPolicies::Enabled, policy.enabled.into()),
        (
            AutomationPolicies::ScriptId,
            policy.script_id.as_str().into(),
        ),
        (AutomationPolicies::RunAs, policy.run_as.as_str().into()),
        (
            AutomationPolicies::Target,
            policy.target.kind().as_str().into(),
        ),
        (
            AutomationPolicies::TriggerType,
            policy.trigger.kind().into(),
        ),
        (AutomationPolicies::IntervalMinutes, interval_minutes.into()),
        (AutomationPolicies::TimeOfDay, time_of_day.into()),
        (AutomationPolicies::Weekdays, weekdays.into()),
        (AutomationPolicies::RunAt, run_at.into()),
        (AutomationPolicies::TimeZone, time_zone.into()),
        (AutomationPolicies::Offline, policy.offline.as_str().into()),
        (
            AutomationPolicies::CatchUpHours,
            policy.catch_up_hours.into(),
        ),
        (AutomationPolicies::OwnerUserId, owner_user_id.into()),
        (AutomationPolicies::NextRunAt, next_run(policy, now).into()),
        (AutomationPolicies::IntervalAnchorAt, now.into()),
        (AutomationPolicies::UpdatedAt, now.into()),
    ];
    if create {
        let (mut columns, mut row): (Vec<_>, Vec<_>) = values
            .into_iter()
            .map(|(column, value)| (column, Expr::val(value)))
            .unzip();
        columns.extend([AutomationPolicies::Id, AutomationPolicies::CreatedAt]);
        row.extend([Expr::val(id), Expr::val(now)]);
        executor
            .execute(
                &Query::insert()
                    .into_table(AutomationPolicies::Table)
                    .columns(columns)
                    .values_panic(row)
                    .to_owned(),
            )
            .await?;
    } else {
        executor
            .execute(
                &Query::update()
                    .table(AutomationPolicies::Table)
                    .values(values.map(|(column, value)| (column, Expr::val(value))))
                    // Saving clears an old problem; firing says if it remains.
                    .value(AutomationPolicies::LastProblem, Option::<String>::None)
                    .value(AutomationPolicies::LastProblemAt, Option::<i64>::None)
                    .and_where(Expr::col(AutomationPolicies::Id).eq(id))
                    .to_owned(),
            )
            .await?;
        for table in LINK_TABLES {
            executor
                .execute(
                    &Query::delete()
                        .from_table(Alias::new(table))
                        .and_where(Expr::col(Alias::new("policy_id")).eq(id))
                        .to_owned(),
                )
                .await?;
        }
    }
    let links = [
        policy.target.company_ids(),
        policy.target.device_ids(),
        policy.channel_ids.as_slice(),
    ];
    for ((table, column), ids) in LINK_TABLES.into_iter().zip(LINK_COLUMNS).zip(links) {
        for chunk in ids.chunks(400) {
            let mut insert = Query::insert();
            insert
                .into_table(Alias::new(table))
                .columns([Alias::new("policy_id"), Alias::new(column)]);
            for other in chunk {
                insert.values_panic([id.into(), other.as_str().into()]);
            }
            executor.execute(&insert).await?;
        }
    }
    Ok(())
}

/// Why the policy can't start a job now, if it can't: its owner lost the
/// access it runs with, or its script was deleted or unshared.
async fn problem(executor: &mut impl Executor, policy: &Policy) -> db::Result<Option<String>> {
    if !policy.script_shared || policy.script_name.is_none() {
        return Ok(Some(
            "Its script was deleted or is no longer shared".to_owned(),
        ));
    }
    let owner = users::by_id(executor, &policy.owner_user_id).await?;
    let Some(owner) = owner.filter(|owner| !owner.disabled) else {
        return Ok(Some(format!(
            "{}, whose access it runs with, was disabled or removed; save the policy to run it with your access",
            policy.owner
        )));
    };
    let permissions = rbac::permissions_of(&rbac::user_roles(executor, &owner.id).await?);
    if !permissions.contains(&Permission::AutomationManage)
        || !permissions.contains(&Permission::ScriptsRun)
    {
        return Ok(Some(format!(
            "{}, whose access it runs with, no longer has the automation.manage and scripts.run permissions; save the policy to run it with your access",
            policy.owner
        )));
    }
    Ok(None)
}

/// The script the policy runs, as a job's payload.
async fn payload(executor: &mut impl Executor, policy: &Policy) -> db::Result<Option<Payload>> {
    let script: Option<(String, String, String, i64)> = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    ToolboxScripts::Name,
                    ToolboxScripts::Language,
                    ToolboxScripts::Body,
                    ToolboxScripts::TimeoutSeconds,
                ])
                .from(ToolboxScripts::Table)
                .and_where(Expr::col(ToolboxScripts::Id).eq(policy.script_id.as_str()))
                .and_where(Expr::col(ToolboxScripts::Shared).eq(true))
                .to_owned(),
        )
        .await?;
    Ok(script.and_then(|(name, language, body, timeout_seconds)| {
        Some(Payload::Script {
            script_id: policy.script_id.clone(),
            name,
            language: ScriptLanguage::parse(&language)?,
            body,
            timeout_seconds,
            run_as: policy.run_as,
        })
    }))
}

/// Who a policy's job is started by.
pub enum Starter<'a> {
    /// The policy itself, with its owner's access.
    Policy,
    /// A user who chose Run now.
    User { user_id: &'a str, actor: Actor },
}

/// Starts a job of the policy on `target`, after misses the waiting devices
/// of its earlier jobs if `supersede`. Returns the job's ID, or why it
/// didn't start.
async fn start_job(
    executor: &mut impl Executor,
    state: &AppState,
    policy: &Policy,
    target: Target,
    starter: Starter<'_>,
    supersede: bool,
    now: i64,
) -> Result<Result<String, String>, ApiError> {
    if matches!(starter, Starter::Policy)
        && let Some(problem) = problem(executor, policy).await?
    {
        return Ok(Err(problem));
    }
    let Some(payload) = payload(executor, policy).await? else {
        return Ok(Err(
            "Its script was deleted or is no longer shared".to_owned()
        ));
    };
    if supersede {
        executor
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, DeviceState::Missed.as_str())
                    .value(JobDevices::Detail, SUPERSEDED)
                    .value(JobDevices::UpdatedAt, now)
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                    .and_where(
                        Expr::col(JobDevices::JobId).in_subquery(
                            Query::select()
                                .column(Jobs::Id)
                                .from(Jobs::Table)
                                .and_where(Expr::col(Jobs::PolicyId).eq(policy.id.as_str()))
                                .and_where(Expr::col(Jobs::FinishedAt).is_null())
                                .to_owned(),
                        ),
                    )
                    .to_owned(),
            )
            .await?;
    }
    let (requested_by_user_id, actor) = match starter {
        Starter::Policy => (policy.owner_user_id.clone(), policy.actor()),
        Starter::User { user_id, actor } => (user_id.to_owned(), actor),
    };
    let job = NewJob {
        payload,
        target,
        offline: policy.offline,
        wait_ms: policy.catch_up_hours * HOUR_MS,
        requested_by_user_id,
        policy: Some((policy.id.clone(), policy.name.clone())),
        actor,
    };
    match jobs::create(executor, &job, &state.agents.connected(), now).await {
        Ok(created) => {
            executor
                .execute(
                    &Query::update()
                        .table(AutomationPolicies::Table)
                        .value(AutomationPolicies::LastRunAt, now)
                        .value(AutomationPolicies::LastJobId, created.id.as_str())
                        .value(AutomationPolicies::LastProblem, Option::<String>::None)
                        .value(AutomationPolicies::LastProblemAt, Option::<i64>::None)
                        .and_where(Expr::col(AutomationPolicies::Id).eq(policy.id.as_str()))
                        .to_owned(),
                )
                .await?;
            Ok(Ok(created.id))
        }
        Err(error) if error.code() == Some("no_devices") => {
            Ok(Err("There were no devices to run on".to_owned()))
        }
        Err(error) => Err(error),
    }
}

/// An error starting a job where nobody waits for an HTTP response.
fn failed(error: ApiError) -> anyhow::Error {
    anyhow::anyhow!("{}", error.message())
}

/// Notes why a policy didn't fire, and records it in the audit log.
async fn record_problem(
    executor: &mut impl Executor,
    policy: &Policy,
    problem: &str,
    due_at: Option<i64>,
    now: i64,
) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(AutomationPolicies::Table)
                .value(AutomationPolicies::LastProblem, problem)
                .value(AutomationPolicies::LastProblemAt, now)
                .and_where(Expr::col(AutomationPolicies::Id).eq(policy.id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        executor,
        &policy.actor(),
        "automation_policy.skip",
        AuditTarget::automation_policy(&policy.id),
        json!({ "name": policy.name, "reason": problem, "due_at": due_at }),
    )
    .await?;
    tracing::info!(
        policy_id = policy.id,
        problem,
        "an automation policy didn't run"
    );
    Ok(())
}

/// Fires every enabled policy whose time has come.
pub async fn fire_due(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let due: Vec<(String, i64)> = state
        .database
        .fetch_all(
            &Query::select()
                .columns([AutomationPolicies::Id, AutomationPolicies::NextRunAt])
                .from(AutomationPolicies::Table)
                .and_where(Expr::col(AutomationPolicies::Enabled).eq(true))
                .and_where(Expr::col(AutomationPolicies::NextRunAt).lte(now))
                .to_owned(),
        )
        .await?;
    let mut fired = false;
    for (id, due_at) in due {
        match fire(state, &id, due_at, now).await {
            Ok(started) => fired |= started,
            Err(error) => tracing::warn!(
                policy_id = id,
                error = format!("{error:#}"),
                "could not run an automation policy"
            ),
        }
    }
    Ok(fired)
}

/// Fires the policy due at `due_at`, once: claiming the time and starting
/// the job happen together. Returns whether a job started.
async fn fire(state: &AppState, id: &str, due_at: i64, now: i64) -> anyhow::Result<bool> {
    let mut transaction = state.database.begin().await?;
    let Some(policy) = by_id(&mut transaction, id).await? else {
        return Ok(false);
    };
    if !policy.enabled || policy.next_run_at != Some(due_at) {
        return Ok(false);
    }
    let anchor = policy.interval_anchor_at.unwrap_or(due_at);
    let next = policy
        .trigger
        .next_after(std::cmp::max(now, due_at), anchor);
    let claimed = transaction
        .execute(
            &Query::update()
                .table(AutomationPolicies::Table)
                .value(AutomationPolicies::NextRunAt, next)
                .and_where(Expr::col(AutomationPolicies::Id).eq(id))
                .and_where(Expr::col(AutomationPolicies::NextRunAt).eq(due_at))
                .and_where(Expr::col(AutomationPolicies::Enabled).eq(true))
                .to_owned(),
        )
        .await?;
    if claimed != 1 {
        return Ok(false);
    }
    if now - due_at > MISSED_GRACE_MS && !policy.trigger.catches_up_late() {
        let problem = format!(
            "Missed its run at {} because the server wasn't running",
            crate::alerts::format_time(due_at)
        );
        record_problem(&mut transaction, &policy, &problem, Some(due_at), now).await?;
        transaction.commit().await?;
        return Ok(false);
    }
    let target = policy.job_target();
    let started = match start_job(
        &mut transaction,
        state,
        &policy,
        target,
        Starter::Policy,
        true,
        now,
    )
    .await
    .map_err(failed)?
    {
        Ok(job_id) => {
            tracing::info!(policy_id = id, job_id, "an automation policy started a job");
            true
        }
        Err(problem) => {
            record_problem(&mut transaction, &policy, &problem, Some(due_at), now).await?;
            false
        }
    };
    transaction.commit().await?;
    Ok(started)
}

/// Starts a job of the policy now, as `user_id`. The schedule is unchanged.
pub async fn run_now(
    state: &AppState,
    policy: &Policy,
    user_id: &str,
    actor: Actor,
    now: i64,
) -> Result<String, ApiError> {
    let mut transaction = state.database.begin().await?;
    let started = start_job(
        &mut transaction,
        state,
        policy,
        policy.job_target(),
        Starter::User { user_id, actor },
        policy.trigger.scheduled(),
        now,
    )
    .await?;
    match started {
        Ok(job_id) => {
            transaction.commit().await?;
            Ok(job_id)
        }
        Err(problem) => Err(ApiError::conflict(problem).with_code("policy_cannot_run")),
    }
}

/// Starts the enabled enrollment policies that cover a newly enrolled device
/// on it. Returns how many started.
pub async fn device_enrolled(state: &AppState, device_id: &str, now: i64) -> anyhow::Result<usize> {
    let mut database = &state.database;
    let company: Option<(Option<String>,)> = database
        .fetch_optional(
            &Query::select()
                .column(Agents::CompanyId)
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await?;
    let Some((company_id,)) = company else {
        return Ok(0);
    };
    let policies = load(&mut database, None).await?;
    let mut started = 0;
    for policy in policies.iter().filter(|policy| {
        policy.enabled
            && policy.trigger == Trigger::Enroll
            && policy.covers(device_id, company_id.as_deref())
    }) {
        let mut transaction = state.database.begin().await?;
        let mut enrolling = policy.clone();
        // A device that just enrolled connects in a moment.
        enrolling.offline = Offline::Wait;
        match start_job(
            &mut transaction,
            state,
            &enrolling,
            Target::Devices(vec![device_id.to_owned()]),
            Starter::Policy,
            false,
            now,
        )
        .await
        .map_err(failed)?
        {
            Ok(job_id) => {
                tracing::info!(
                    policy_id = policy.id,
                    device_id,
                    job_id,
                    "an automation policy started a job on a new device"
                );
                started += 1;
            }
            Err(problem) => {
                record_problem(&mut transaction, policy, &problem, None, now).await?;
            }
        }
        transaction.commit().await?;
    }
    Ok(started)
}

/// Why a script can't be deleted or unshared: the policies that run it.
pub fn script_used_message(policies: &[String]) -> Option<String> {
    let first = policies.first()?;
    Some(if policies.len() == 1 {
        format!("The policy \u{201c}{first}\u{201d} runs this script. Change or delete it first.")
    } else {
        format!(
            "{} policies run this script, including \u{201c}{first}\u{201d}. Change or delete them first.",
            policies.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn input(value: serde_json::Value) -> Result<Valid, ApiError> {
        serde_json::from_value::<PolicyInput>(value)
            .unwrap()
            .validate()
    }

    const SCRIPT: &str = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a";

    #[test]
    fn policies_check_their_schedule() {
        let base = json!({
            "name": " Nightly ", "script_id": SCRIPT, "run_as": "system",
            "target": "all", "offline": "wait",
        });
        let with = |extra: serde_json::Value| {
            let mut value = base.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            input(value)
        };
        let daily =
            with(json!({ "trigger": "daily", "time": "03:00", "time_zone": "Europe/Paris" }))
                .unwrap();
        assert_eq!(daily.name, "Nightly");
        assert_eq!(daily.catch_up_hours, DEFAULT_CATCH_UP_HOURS);
        assert!(matches!(daily.trigger, Trigger::Daily { .. }));
        for bad in [
            json!({ "trigger": "daily", "time": "3:00", "time_zone": "Europe/Paris" }),
            json!({ "trigger": "daily", "time": "03:00", "time_zone": "Paris" }),
            json!({ "trigger": "daily", "time": "03:00" }),
            json!({ "trigger": "weekly", "time": "03:00", "time_zone": "UTC" }),
            json!({ "trigger": "weekly", "time": "03:00", "time_zone": "UTC", "weekdays": [0] }),
            json!({ "trigger": "interval", "interval_minutes": 4 }),
            json!({ "trigger": "interval" }),
            json!({ "trigger": "once", "run_at": "2026-13-01T00:00", "time_zone": "UTC" }),
            json!({ "trigger": "interval", "interval_minutes": 60, "catch_up_hours": 0 }),
            json!({ "trigger": "interval", "interval_minutes": 60, "catch_up_hours": 169 }),
        ] {
            assert!(with(bad.clone()).is_err(), "{bad}");
        }
        assert!(with(json!({ "trigger": "interval", "interval_minutes": 5 })).is_ok());
        let weekly = with(
            json!({ "trigger": "weekly", "time": "08:00", "time_zone": "UTC", "weekdays": [5, 1] }),
        )
        .unwrap();
        assert!(
            matches!(weekly.trigger, Trigger::Weekly { days, .. } if days.days() == vec![1, 5])
        );
    }

    #[test]
    fn enrollment_policies_cover_companies_or_everything() {
        let device = json!({
            "name": "Onboard", "script_id": SCRIPT, "run_as": "system", "offline": "wait",
            "trigger": "enroll", "target": "devices", "device_ids": [SCRIPT],
        });
        assert!(input(device).is_err());
        let company = json!({
            "name": "Onboard", "script_id": SCRIPT, "run_as": "system", "offline": "wait",
            "trigger": "enroll", "target": "companies", "company_ids": [SCRIPT],
        });
        assert!(input(company).is_ok());
        let none = json!({
            "name": "Onboard", "script_id": SCRIPT, "run_as": "system", "offline": "wait",
            "trigger": "enroll", "target": "companies",
        });
        assert!(input(none).is_err());
    }

    #[test]
    fn scripts_in_use_name_their_policies() {
        assert_eq!(script_used_message(&[]), None);
        assert!(
            script_used_message(&["A".into()])
                .unwrap()
                .contains("\u{201c}A\u{201d}")
        );
        assert!(
            script_used_message(&["A".into(), "B".into()])
                .unwrap()
                .starts_with("2 policies")
        );
    }
}
