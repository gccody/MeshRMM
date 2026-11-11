//! Automation policies: a shared script, an update scan or an update install
//! the server runs by itself, on a schedule or on each newly enrolled
//! device, as a job.
//!
//! A policy runs with the access of the user who last saved it: when it
//! fires, that user must still be enabled and hold `automation.manage` and
//! what its action needs (`scripts.run`, `updates.view` or
//! `updates.install`), and a script policy's script must still be shared.
//! Otherwise it doesn't fire and says why. Policies use shared scripts only,
//! so everyone who manages policies can see what each one runs.
//!
//! Firing claims the due time in the transaction that ends the job's
//! creation, the only one for a job on few devices, so a restart (or a
//! second pass) can't fire it twice, and a firing that stops while a job on
//! many devices is written fires again on the next pass, once its half-made
//! job is removed. The job is planned before that, through the database's
//! readers; the claim holds only if the policy hasn't been saved since, and
//! otherwise the job is removed and the firing planned again from what was
//! saved. A firing missed while
//! the server was down runs when it starts if it is at most
//! [`MISSED_GRACE_MS`] late; later than that, interval schedules still run
//! once, and times of day are skipped until their next time. Missed times
//! never run more than once. A run replaces its earlier jobs' waits for
//! offline devices, but is skipped, and says why, while those jobs still
//! have online devices queued, so a policy can't pile up jobs faster than
//! the dispatcher starts them.
use std::collections::HashMap;

use meshrmm_protocol_types::{
    MAX_REBOOT_DELAY_MINUTES, RebootBehavior, RunAs, ScriptLanguage, UpdateCategory,
};
use sea_query::{Alias, Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    dispatch::QUEUE_CHUNK,
    jobs::{self, DeviceState, NewJob, Offline, Payload, Target, TargetKind},
    schedule::{self, MAX_INTERVAL_MINUTES, MIN_INTERVAL_MINUTES, Trigger, Weekdays},
};
use crate::{
    applications::policies::PolicySpec,
    audit::{self, Actor, Target as AuditTarget},
    db::{
        self, Executor, Transaction,
        tables::{
            AutomationPolicies, AutomationPolicyChannels, AutomationPolicyCompanies,
            AutomationPolicyDevices, AutomationPolicyTags, JobDevices, Jobs, NotificationChannels,
            ToolboxScripts, Users,
        },
    },
    http::{ApiError, AppState},
    rbac::{self, Permission},
    targets::{Choice, TagMatch},
    time::HOUR_MS,
    updates::runs::Install,
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

pub const DEFAULT_REBOOT_DELAY_MINUTES: u32 = 5;

/// What a policy does when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    #[default]
    Script,
    UpdateScan,
    UpdateInstall,
    Wake,
    AppInstall,
    AppUpdate,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::UpdateScan => "update_scan",
            Self::UpdateInstall => "update_install",
            Self::Wake => "wake",
            Self::AppInstall => "app_install",
            Self::AppUpdate => "app_update",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "script" => Some(Self::Script),
            "update_scan" => Some(Self::UpdateScan),
            "update_install" => Some(Self::UpdateInstall),
            "wake" => Some(Self::Wake),
            "app_install" => Some(Self::AppInstall),
            "app_update" => Some(Self::AppUpdate),
            _ => None,
        }
    }

    /// The permission, besides `automation.manage`, whoever saves the policy
    /// (and so whose access it runs with) must hold.
    pub fn permission(self) -> Permission {
        match self {
            Self::Script => Permission::ScriptsRun,
            Self::UpdateScan => Permission::UpdatesView,
            Self::UpdateInstall => Permission::UpdatesInstall,
            Self::Wake => Permission::DevicesWake,
            Self::AppInstall | Self::AppUpdate => Permission::ApplicationsDeploy,
        }
    }
}

/// What a policy does, with what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Script {
        script_id: String,
        run_as: RunAs,
    },
    UpdateScan,
    /// Installs the pending updates in `categories`, or every one.
    UpdateInstall {
        categories: Vec<UpdateCategory>,
        reboot: RebootBehavior,
        reboot_delay_minutes: u32,
    },
    /// Wakes the devices that are offline.
    Wake,
    /// Installs (`install`) or updates applications.
    Applications {
        install: bool,
        spec: PolicySpec,
    },
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Script { .. } => ActionKind::Script,
            Self::UpdateScan => ActionKind::UpdateScan,
            Self::UpdateInstall { .. } => ActionKind::UpdateInstall,
            Self::Wake => ActionKind::Wake,
            Self::Applications { install: true, .. } => ActionKind::AppInstall,
            Self::Applications { install: false, .. } => ActionKind::AppUpdate,
        }
    }

    /// Whether it keeps applications installed, which newly enrolled
    /// devices it covers get whatever its trigger.
    pub fn installs_applications(&self) -> bool {
        matches!(self, Self::Applications { install: true, .. })
    }

    pub fn script_id(&self) -> Option<&str> {
        match self {
            Self::Script { script_id, .. } => Some(script_id),
            _ => None,
        }
    }
}

/// A policy as the website sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyInput {
    pub name: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub action: ActionKind,
    /// For a script policy.
    #[serde(default)]
    pub script_id: Option<String>,
    #[serde(default)]
    pub run_as: Option<RunAs>,
    /// For an install policy: none for every pending update.
    #[serde(default)]
    pub update_categories: Vec<UpdateCategory>,
    #[serde(default)]
    pub reboot: Option<RebootBehavior>,
    #[serde(default)]
    pub reboot_delay_minutes: Option<i64>,
    /// For an application policy.
    #[serde(default)]
    pub applications: Option<PolicySpec>,
    pub target: TargetKind,
    #[serde(default)]
    pub device_ids: Vec<String>,
    #[serde(default)]
    pub company_ids: Vec<String>,
    #[serde(default)]
    pub tag_ids: Vec<String>,
    #[serde(default)]
    pub tag_match: TagMatch,
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
    pub action: Action,
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
        let action = match self.action {
            ActionKind::Script => {
                let script_id = self
                    .script_id
                    .ok_or_else(|| invalid("choose the script the policy runs"))?;
                crate::agents::parse_id(&script_id, "script")?;
                Action::Script {
                    script_id,
                    run_as: self
                        .run_as
                        .ok_or_else(|| invalid("choose the account the script runs as"))?,
                }
            }
            ActionKind::UpdateScan => Action::UpdateScan,
            ActionKind::Wake => Action::Wake,
            ActionKind::AppInstall | ActionKind::AppUpdate => {
                let install = self.action == ActionKind::AppInstall;
                Action::Applications {
                    install,
                    spec: PolicySpec::validate(self.applications, install)?,
                }
            }
            ActionKind::UpdateInstall => {
                let reboot_delay_minutes = match self.reboot_delay_minutes {
                    None => DEFAULT_REBOOT_DELAY_MINUTES,
                    Some(minutes) => u32::try_from(minutes)
                        .ok()
                        .filter(|minutes| (1..=MAX_REBOOT_DELAY_MINUTES).contains(minutes))
                        .ok_or_else(|| {
                            invalid(format!(
                                "the restart can wait 1 to {MAX_REBOOT_DELAY_MINUTES} minutes"
                            ))
                        })?,
                };
                let mut categories = self.update_categories;
                categories.sort();
                categories.dedup();
                Action::UpdateInstall {
                    categories,
                    reboot: self
                        .reboot
                        .ok_or_else(|| invalid("choose when the computer restarts"))?,
                    reboot_delay_minutes,
                }
            }
        };
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
        // Outdated packages' devices are for one job's update: an update
        // policy already updates only what is outdated on its devices.
        if self.target == TargetKind::Outdated {
            return Err(invalid(
                "a policy runs on chosen devices, companies, tags or every device",
            ));
        }
        let target = Target::from_request(
            self.target,
            Choice {
                device_ids: self.device_ids,
                company_ids: self.company_ids,
                tag_ids: self.tag_ids,
                tag_match: self.tag_match,
                packages: Vec::new(),
            },
        )
        .map_err(|_| invalid("choose the devices, companies or tags it runs on"))?;
        if trigger == Trigger::Enroll && action == Action::Wake {
            return Err(invalid(
                "a policy that wakes devices runs on a schedule: newly enrolled devices are online",
            ));
        }
        // A device that just enrolled has no tags yet.
        if trigger == Trigger::Enroll && matches!(target, Target::Devices(_) | Target::Tags { .. })
        {
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
            action,
            target,
            trigger,
            offline: self.offline,
            catch_up_hours: self.catch_up_hours,
            channel_ids,
        })
    }
}

impl Valid {
    /// Checks that the script, if any, is shared and the companies, devices
    /// and channels exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        if let Some(script_id) = self.action.script_id() {
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
                        "policies run shared scripts only; share the script first",
                    )
                    .with_code("script_not_shared"));
                }
                Some((true,)) => {}
            }
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
        if let Action::Applications { spec, .. } = &self.action {
            spec.check_references(executor).await?;
        }
        self.target.check_references(executor).await?;
        let mut found = 0;
        for chunk in self.channel_ids.chunks(QUEUE_CHUNK) {
            let rows: Vec<(String,)> = executor
                .fetch_all(
                    &Query::select()
                        .column(NotificationChannels::Id)
                        .from(NotificationChannels::Table)
                        .and_where(Expr::col(NotificationChannels::Id).is_in(chunk.iter().cloned()))
                        .to_owned(),
                )
                .await?;
            found += rows.len();
        }
        count(found, self.channel_ids.len(), "notification channel")?;
        Ok(())
    }
}

/// A policy as stored, with its script's name.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub action: Action,
    /// `None` once the script is gone, or for an update policy.
    pub script_name: Option<String>,
    pub script_language: Option<ScriptLanguage>,
    pub script_shared: bool,
    pub target: Target,
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
    /// Recorded in the audit log as starting the policy's jobs.
    pub fn actor(&self) -> Actor {
        Actor {
            user_id: None,
            label: format!("policy \u{201c}{}\u{201d}", self.name),
            ip: None,
            api_token: None,
        }
    }
}

/// A policy as the website shows it.
#[derive(Debug, Serialize)]
pub struct PolicyView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub action: ActionKind,
    pub script_id: Option<String>,
    pub script_name: Option<String>,
    pub script_language: Option<ScriptLanguage>,
    pub script_shared: bool,
    pub run_as: Option<RunAs>,
    pub update_categories: Option<Vec<UpdateCategory>>,
    pub reboot: Option<RebootBehavior>,
    pub reboot_delay_minutes: Option<u32>,
    pub applications: Option<PolicySpec>,
    #[serde(flatten)]
    pub target: Target,
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
        let (script_id, run_as) = match &self.action {
            Action::Script { script_id, run_as } => (Some(script_id.clone()), Some(*run_as)),
            _ => (None, None),
        };
        let (update_categories, reboot, reboot_delay_minutes) = match &self.action {
            Action::UpdateInstall {
                categories,
                reboot,
                reboot_delay_minutes,
            } => (
                Some(categories.clone()),
                Some(*reboot),
                Some(*reboot_delay_minutes),
            ),
            _ => (None, None, None),
        };
        let applications = match &self.action {
            Action::Applications { spec, .. } => Some(spec.clone()),
            _ => None,
        };
        PolicyView {
            owned_by_you: self.owner_user_id == user_id,
            trigger: self.trigger.kind(),
            action: self.action.kind(),
            id: self.id,
            name: self.name,
            enabled: self.enabled,
            script_id,
            script_name: self.script_name,
            script_language: self.script_language,
            script_shared: self.script_shared,
            run_as,
            update_categories,
            reboot,
            reboot_delay_minutes,
            applications,
            target: self.target,
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
    action: String,
    script_id: Option<String>,
    script_name: Option<String>,
    script_language: Option<String>,
    script_shared: Option<bool>,
    run_as: Option<String>,
    update_categories: Option<String>,
    reboot: Option<String>,
    reboot_delay_minutes: Option<i64>,
    target: String,
    tag_match: String,
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

fn action_of(row: &PolicyRow, applications: Option<&PolicySpec>) -> Option<Action> {
    Some(match ActionKind::parse(&row.action)? {
        ActionKind::Script => Action::Script {
            script_id: row.script_id.clone()?,
            run_as: RunAs::parse(row.run_as.as_deref()?)?,
        },
        ActionKind::UpdateScan => Action::UpdateScan,
        ActionKind::Wake => Action::Wake,
        ActionKind::UpdateInstall => Action::UpdateInstall {
            categories: Install::parse_categories(row.update_categories.as_deref()),
            reboot: RebootBehavior::parse(row.reboot.as_deref()?)?,
            reboot_delay_minutes: u32::try_from(row.reboot_delay_minutes?).ok()?,
        },
        kind @ (ActionKind::AppInstall | ActionKind::AppUpdate) => Action::Applications {
            install: kind == ActionKind::AppInstall,
            spec: applications?.clone(),
        },
    })
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
            (AutomationPolicies::Table, AutomationPolicies::Action),
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
            (
                AutomationPolicies::Table,
                AutomationPolicies::UpdateCategories,
            ),
            (AutomationPolicies::Table, AutomationPolicies::Reboot),
            (
                AutomationPolicies::Table,
                AutomationPolicies::RebootDelayMinutes,
            ),
            (AutomationPolicies::Table, AutomationPolicies::Target),
            (AutomationPolicies::Table, AutomationPolicies::TagMatch),
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
    let mut tags = Query::select();
    tags.columns([AutomationPolicyTags::PolicyId, AutomationPolicyTags::TagId])
        .from(AutomationPolicyTags::Table);
    let mut channels = Query::select();
    channels
        .columns([
            AutomationPolicyChannels::PolicyId,
            AutomationPolicyChannels::ChannelId,
        ])
        .from(AutomationPolicyChannels::Table);
    if let Some(id) = id {
        tags.and_where(Expr::col(AutomationPolicyTags::PolicyId).eq(id));
        select.and_where(Expr::col((AutomationPolicies::Table, AutomationPolicies::Id)).eq(id));
        companies.and_where(Expr::col(AutomationPolicyCompanies::PolicyId).eq(id));
        devices.and_where(Expr::col(AutomationPolicyDevices::PolicyId).eq(id));
        channels.and_where(Expr::col(AutomationPolicyChannels::PolicyId).eq(id));
    }
    let rows: Vec<PolicyRow> = executor.fetch_all(&select).await?;
    let companies = grouped(executor.fetch_all(&companies).await?);
    let devices = grouped(executor.fetch_all(&devices).await?);
    let tags = grouped(executor.fetch_all(&tags).await?);
    let channels = grouped(executor.fetch_all(&channels).await?);
    let applications = crate::applications::policies::load(executor, id).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let choice = Choice {
                device_ids: devices.get(&row.id).cloned().unwrap_or_default(),
                company_ids: companies.get(&row.id).cloned().unwrap_or_default(),
                tag_ids: tags.get(&row.id).cloned().unwrap_or_default(),
                ..Choice::default()
            };
            let (Some(trigger), Some(action), Some(target), Some(offline)) = (
                trigger_of(&row),
                action_of(&row, applications.get(&row.id)),
                Target::from_stored(&row.target, &row.tag_match, choice),
                Offline::parse(&row.offline),
            ) else {
                tracing::error!(policy_id = row.id, "stored automation policy is invalid");
                return None;
            };
            Some(Policy {
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
                action,
                script_name: row.script_name,
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
/// The tables that link a policy to its companies, devices, tags and
/// channels, and the column each links to.
const LINK_TABLES: [&str; 4] = [
    "automation_policy_companies",
    "automation_policy_devices",
    "automation_policy_tags",
    "automation_policy_channels",
];
const LINK_COLUMNS: [&str; 4] = ["company_id", "device_id", "tag_id", "channel_id"];

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
    let (script_id, run_as) = match &policy.action {
        Action::Script { script_id, run_as } => (Some(script_id.as_str()), Some(run_as.as_str())),
        _ => (None, None),
    };
    let (categories, reboot, reboot_delay_minutes) = match &policy.action {
        Action::UpdateInstall {
            categories,
            reboot,
            reboot_delay_minutes,
        } => (
            Install::categories_text(categories),
            Some(reboot.as_str()),
            Some(i64::from(*reboot_delay_minutes)),
        ),
        _ => (None, None, None),
    };
    let values: [(AutomationPolicies, sea_query::Value); 21] = [
        (AutomationPolicies::Name, policy.name.as_str().into()),
        (AutomationPolicies::Enabled, policy.enabled.into()),
        (
            AutomationPolicies::Action,
            policy.action.kind().as_str().into(),
        ),
        (AutomationPolicies::ScriptId, script_id.into()),
        (AutomationPolicies::RunAs, run_as.into()),
        (AutomationPolicies::UpdateCategories, categories.into()),
        (AutomationPolicies::Reboot, reboot.into()),
        (
            AutomationPolicies::RebootDelayMinutes,
            reboot_delay_minutes.into(),
        ),
        (
            AutomationPolicies::Target,
            policy.target.kind().as_str().into(),
        ),
        (
            AutomationPolicies::TagMatch,
            policy.target.tag_match().as_str().into(),
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
    ];
    if create {
        let (mut columns, mut row): (Vec<_>, Vec<_>) = values
            .into_iter()
            .map(|(column, value)| (column, Expr::val(value)))
            .unzip();
        columns.extend([
            AutomationPolicies::Id,
            AutomationPolicies::CreatedAt,
            AutomationPolicies::UpdatedAt,
        ]);
        row.extend([Expr::val(id), Expr::val(now), Expr::val(now)]);
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
                    // Later than the last save even within its millisecond,
                    // so a firing planned before this save can tell.
                    .value(
                        AutomationPolicies::UpdatedAt,
                        Expr::case(
                            Expr::col(AutomationPolicies::UpdatedAt).gte(now),
                            Expr::col(AutomationPolicies::UpdatedAt).add(1),
                        )
                        .finally(now),
                    )
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
        policy.target.tag_ids(),
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
    let applications = match &policy.action {
        Action::Applications { spec, .. } => Some(spec),
        _ => None,
    };
    crate::applications::policies::save(executor, id, applications).await?;
    Ok(())
}

/// Why the policy can't start a job now, if it can't: its owner lost the
/// access it runs with, or its script was deleted or unshared.
async fn problem(executor: &mut impl Executor, policy: &Policy) -> db::Result<Option<String>> {
    if matches!(policy.action, Action::Script { .. })
        && (!policy.script_shared || policy.script_name.is_none())
    {
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
    let needed = policy.action.kind().permission();
    if !permissions.contains(&Permission::AutomationManage) || !permissions.contains(&needed) {
        return Ok(Some(format!(
            "{}, whose access it runs with, no longer has the automation.manage and {needed} permissions; save the policy to run it with your access",
            policy.owner
        )));
    }
    Ok(None)
}

/// What the policy does, as a job's payload: `None` when its script is gone.
async fn payload(executor: &mut impl Executor, policy: &Policy) -> db::Result<Option<Payload>> {
    let (script_id, run_as) = match &policy.action {
        Action::Script { script_id, run_as } => (script_id, *run_as),
        Action::UpdateScan => return Ok(Some(Payload::UpdateScan)),
        Action::Wake => return Ok(Some(Payload::Wake)),
        Action::Applications { install, spec } => {
            return Ok(match spec.spec(executor, *install).await {
                Ok(spec) => Some(Payload::Applications(Box::new(spec))),
                Err(error) => {
                    tracing::warn!(
                        policy_id = policy.id,
                        error = error.message(),
                        "an application policy's applications are unusable"
                    );
                    None
                }
            });
        }
        Action::UpdateInstall {
            categories,
            reboot,
            reboot_delay_minutes,
        } => {
            return Ok(Some(Payload::UpdateInstall {
                categories: categories.clone(),
                reboot: *reboot,
                reboot_delay_minutes: *reboot_delay_minutes,
            }));
        }
    };
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
                .and_where(Expr::col(ToolboxScripts::Id).eq(script_id.as_str()))
                .and_where(Expr::col(ToolboxScripts::Shared).eq(true))
                .to_owned(),
        )
        .await?;
    Ok(script.and_then(|(name, language, body, timeout_seconds)| {
        Some(Payload::Script {
            script_id: script_id.clone(),
            name,
            language: ScriptLanguage::parse(&language)?,
            body,
            timeout_seconds,
            run_as,
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

const NO_DEVICES: &str = "There were no devices to run on";

/// A job of a policy as planned through the readers, or why it can't start.
#[derive(Debug)]
enum Prepared {
    Job(Box<jobs::Plan>),
    /// Its target covers no devices. Its earlier jobs still stop waiting.
    NoDevices,
    /// Its owner lost the access it runs with, or its script or
    /// applications are gone.
    Problem(String),
}

/// Plans a job of the policy on `target` through the readers, outside any
/// transaction: checks that the policy can run (for its own runs, that its
/// owner still has the access it runs with), builds what the job sends, and
/// plans its devices. A role, script or library change that lands after
/// this is as one that lands just after the job is created.
async fn prepare(
    state: &AppState,
    policy: &Policy,
    target: Target,
    starter: Starter<'_>,
) -> Result<Prepared, ApiError> {
    let mut database = &state.database;
    if matches!(starter, Starter::Policy)
        && let Some(problem) = problem(&mut database, policy).await?
    {
        return Ok(Prepared::Problem(problem));
    }
    let Some(payload) = payload(&mut database, policy).await? else {
        return Ok(Prepared::Problem(
            if matches!(policy.action, Action::Applications { .. }) {
                "Its applications are no longer in the library".to_owned()
            } else {
                "Its script was deleted or is no longer shared".to_owned()
            },
        ));
    };
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
    match jobs::plan(&mut database, &state.agents, job).await {
        Ok(plan) => Ok(Prepared::Job(Box::new(plan))),
        Err(error) if error.code() == Some("no_devices") => Ok(Prepared::NoDevices),
        Err(error) => Err(error),
    }
}

/// What starting a policy's job writes when the job can't start.
#[derive(Debug, Clone, Copy)]
enum OnProblem {
    /// Why, as the policy's last problem, for the firing due then if any.
    Record(Option<i64>),
    /// Nothing: the user who chose Run now is told.
    Refuse,
}

/// A firing's claim on its due time: the policy's next run moves on, as long
/// as it is still enabled, due then and not saved since planning.
struct Claim<'a> {
    policy: &'a Policy,
    due_at: i64,
    next: Option<i64>,
}

impl Claim<'_> {
    /// The policy, while the claim holds.
    fn policy_row(&self) -> sea_query::Condition {
        sea_query::Cond::all()
            .add(Expr::col(AutomationPolicies::Id).eq(self.policy.id.as_str()))
            .add(Expr::col(AutomationPolicies::NextRunAt).eq(self.due_at))
            .add(Expr::col(AutomationPolicies::Enabled).eq(true))
            .add(Expr::col(AutomationPolicies::UpdatedAt).eq(self.policy.updated_at))
    }

    /// Takes the claim. Returns whether it held.
    async fn take(&self, executor: &mut impl Executor) -> db::Result<bool> {
        let claimed = executor
            .execute(
                &Query::update()
                    .table(AutomationPolicies::Table)
                    .value(AutomationPolicies::NextRunAt, self.next)
                    .cond_where(self.policy_row())
                    .to_owned(),
            )
            .await?;
        Ok(claimed == 1)
    }

    /// Whether it would hold now, without taking it.
    async fn holds(&self, executor: &mut impl Executor) -> db::Result<bool> {
        let row: Option<(String,)> = executor
            .fetch_optional(
                &Query::select()
                    .column(AutomationPolicies::Id)
                    .from(AutomationPolicies::Table)
                    .cond_where(self.policy_row())
                    .to_owned(),
            )
            .await?;
        Ok(row.is_some())
    }
}

/// Misses the waiting devices of the policy's earlier unfinished jobs, which
/// its new run replaces.
async fn supersede_waits(
    executor: &mut impl Executor,
    policy: &Policy,
    now: i64,
) -> db::Result<()> {
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
                            // Not the new one, nor another still being created.
                            .and_where(jobs::created())
                            .to_owned(),
                    ),
                )
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Starts a prepared job of the policy in `transaction`, which may already
/// hold the caller's checks: missing the waiting devices of its earlier
/// jobs if `supersede`, and noting it as the policy's last run. A job on
/// many devices is stored with `transaction`, marked as being created, and
/// its devices are written in transactions of their own before another
/// ends its creation ([`jobs::Draft::write_devices`]). The firing's
/// `claim`, if any, is taken in the transaction that ends the job's
/// creation, with the waits the job replaces, so a job starts only with its
/// claim: if another pass took it first, or the policy was saved, turned
/// off or deleted meanwhile, the job is removed and this returns `None`.
/// Otherwise it returns the job's ID, or why it didn't start, after writing
/// what `on_problem` says.
#[allow(clippy::too_many_arguments)]
async fn start_job(
    state: &AppState,
    mut transaction: Transaction,
    policy: &Policy,
    prepared: &Prepared,
    supersede: bool,
    claim: Option<&Claim<'_>>,
    on_problem: OnProblem,
    now: i64,
) -> Result<Option<Result<String, String>>, ApiError> {
    let mut draft = None;
    let mut batched = false;
    if let Prepared::Job(plan) = prepared {
        batched = plan.batched();
        // Many devices written for a firing it can't claim would be for nothing.
        if batched
            && let Some(claim) = claim
            && !claim.holds(&mut transaction).await?
        {
            transaction.rollback().await?;
            return Ok(None);
        }
        let mut written = jobs::draft(&mut transaction, &state.automation, plan, now).await?;
        transaction = written.write_devices(state, transaction).await?;
        draft = Some(written);
    }
    // A job stored before this transaction is removed if it doesn't start.
    let stored = batched
        .then(|| draft.as_ref().map(|draft| draft.id().to_owned()))
        .flatten();
    if let Some(claim) = claim
        && !claim.take(&mut transaction).await?
    {
        transaction.rollback().await?;
        if let Some(job_id) = &stored {
            jobs::abandon(state, job_id).await;
        }
        return Ok(None);
    }
    // With no devices, its earlier jobs still stop waiting.
    if supersede && !matches!(prepared, Prepared::Problem(_)) {
        supersede_waits(&mut transaction, policy, now).await?;
    }
    let started = match (prepared, draft) {
        (Prepared::Problem(problem), _) => Err(problem.clone()),
        (_, Some(draft)) => match draft.finish(&mut transaction).await {
            Ok(created) => Ok(created.id),
            // Every planned device was deleted before its batch.
            Err(error) if error.code() == Some("no_devices") => Err(NO_DEVICES.to_owned()),
            Err(error) => return Err(error),
        },
        (_, None) => Err(NO_DEVICES.to_owned()),
    };
    match (&started, on_problem) {
        (Ok(job_id), _) => {
            transaction
                .execute(
                    &Query::update()
                        .table(AutomationPolicies::Table)
                        .value(AutomationPolicies::LastRunAt, now)
                        .value(AutomationPolicies::LastJobId, job_id.as_str())
                        .value(AutomationPolicies::LastProblem, Option::<String>::None)
                        .value(AutomationPolicies::LastProblemAt, Option::<i64>::None)
                        .and_where(Expr::col(AutomationPolicies::Id).eq(policy.id.as_str()))
                        .to_owned(),
                )
                .await?;
        }
        (Err(problem), OnProblem::Record(due_at)) => {
            record_problem(&mut transaction, policy, problem, due_at, now).await?;
        }
        (Err(_), OnProblem::Refuse) => {
            transaction.rollback().await?;
            if let Some(job_id) = &stored {
                jobs::abandon(state, job_id).await;
            }
            return Ok(Some(started));
        }
    }
    transaction.commit().await?;
    Ok(Some(started))
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

/// How many devices are queued in the policy's unfinished jobs: online, and
/// not started yet. A job still being created isn't counted: one whose creation
/// stopped would otherwise hold the policy back until it is removed.
async fn queued_devices(executor: &mut impl Executor, policy_id: &str) -> db::Result<i64> {
    let (count,): (i64,) = executor
        .fetch_one(
            &Query::select()
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(JobDevices::Table)
                .and_where(Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()))
                .and_where(
                    Expr::col(JobDevices::JobId).in_subquery(
                        Query::select()
                            .column(Jobs::Id)
                            .from(Jobs::Table)
                            .and_where(Expr::col(Jobs::PolicyId).eq(policy_id))
                            .and_where(Expr::col(Jobs::FinishedAt).is_null())
                            .and_where(jobs::created())
                            .to_owned(),
                    ),
                )
                .to_owned(),
        )
        .await?;
    Ok(count)
}

/// How many times a firing is planned again when its policy is saved, or its
/// last run's devices start, between planning and claiming. Past that, the
/// next pass tries again.
const FIRE_ATTEMPTS: usize = 3;

/// A policy's firing as planned through the readers: what claiming its due
/// time will do. [`write_firing`] claims the time and does it.
pub struct Firing {
    policy: Policy,
    due_at: i64,
    now: i64,
    step: Step,
}

enum Step {
    /// Later than a schedule that doesn't catch up allows: say so.
    Missed,
    /// Its last run still has devices queued: skip, if they still are.
    Busy,
    Start(Prepared),
}

/// How claiming a planned firing went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fired {
    /// The due time is claimed; whether a job started.
    Claimed { started: bool },
    /// Another pass claimed it first, or the policy was deleted, turned off
    /// or given another time.
    NotDue,
    /// The policy was saved, or its last run's queued devices started, since
    /// planning: nothing was written, and the firing is to be planned again.
    Changed,
}

/// Plans the firing of the policy `id` due at `due_at`, through the readers:
/// its job, unless the time was missed or its last run's devices are still
/// queued. `None` when the policy isn't due then.
pub async fn plan_firing(
    state: &AppState,
    id: &str,
    due_at: i64,
    now: i64,
) -> anyhow::Result<Option<Firing>> {
    let Some(policy) = by_id(&mut &state.database, id).await? else {
        return Ok(None);
    };
    if !policy.enabled || policy.next_run_at != Some(due_at) {
        return Ok(None);
    }
    let step = if now - due_at > MISSED_GRACE_MS && !policy.trigger.catches_up_late() {
        Step::Missed
    } else if queued_devices(&mut &state.database, &policy.id).await? > 0 {
        Step::Busy
    } else {
        Step::Start(
            prepare(state, &policy, policy.target.clone(), Starter::Policy)
                .await
                .map_err(failed)?,
        )
    };
    Ok(Some(Firing {
        policy,
        due_at,
        now,
        step,
    }))
}

/// Claims a planned firing's due time and does what was planned, together,
/// so a firing is claimed once and only with its job: in one transaction,
/// or for a job on many devices in the one that ends its creation
/// ([`start_job`]). The claim holds only while the policy is enabled, still
/// due then and not saved since planning (its `updated_at` is unchanged),
/// so a job never starts from an old definition; whether the last run's
/// devices are still queued is checked again inside.
pub async fn write_firing(state: &AppState, firing: &Firing) -> anyhow::Result<Fired> {
    let Firing {
        policy,
        due_at,
        now,
        step,
    } = firing;
    let (due_at, now) = (*due_at, *now);
    let anchor = policy.interval_anchor_at.unwrap_or(due_at);
    let claim = Claim {
        policy,
        due_at,
        next: policy
            .trigger
            .next_after(std::cmp::max(now, due_at), anchor),
    };
    let mut transaction = state.database.begin().await?;
    let problem = if matches!(step, Step::Missed) {
        Some(format!(
            "Missed its run at {} because the server wasn't running",
            crate::alerts::format_time(due_at)
        ))
    } else {
        // Online devices still waiting their turn from its last run: a new
        // job would only queue them again behind it, and on a big fleet with
        // a short interval the queue would grow faster than it drains.
        let queued = queued_devices(&mut transaction, &policy.id).await?;
        (queued > 0).then(|| {
            format!(
                "Skipped its run at {} because {} from its last run hadn't started yet",
                crate::alerts::format_time(due_at),
                if queued == 1 {
                    "1 device".to_owned()
                } else {
                    format!("{queued} devices")
                }
            )
        })
    };
    if let Some(problem) = problem {
        if !claim.take(&mut transaction).await? {
            transaction.rollback().await?;
            return Ok(still_due(state, &policy.id, due_at).await?);
        }
        record_problem(&mut transaction, policy, &problem, Some(due_at), now).await?;
        transaction.commit().await?;
        return Ok(Fired::Claimed { started: false });
    }
    let Step::Start(prepared) = step else {
        // They started since planning, so the policy runs after all.
        transaction.rollback().await?;
        return Ok(Fired::Changed);
    };
    match start_job(
        state,
        transaction,
        policy,
        prepared,
        true,
        Some(&claim),
        OnProblem::Record(Some(due_at)),
        now,
    )
    .await
    .map_err(failed)?
    {
        None => Ok(still_due(state, &policy.id, due_at).await?),
        Some(Ok(job_id)) => {
            tracing::info!(
                policy_id = policy.id,
                job_id,
                "an automation policy started a job"
            );
            Ok(Fired::Claimed { started: true })
        }
        Some(Err(_)) => Ok(Fired::Claimed { started: false }),
    }
}

/// Why a firing's claim found nothing to claim: if the policy is still
/// enabled and due then, only a save since planning can have stopped it.
async fn still_due(state: &AppState, id: &str, due_at: i64) -> db::Result<Fired> {
    let row: Option<(bool, Option<i64>)> = state
        .database
        .fetch_optional(
            &Query::select()
                .columns([AutomationPolicies::Enabled, AutomationPolicies::NextRunAt])
                .from(AutomationPolicies::Table)
                .and_where(Expr::col(AutomationPolicies::Id).eq(id))
                .to_owned(),
        )
        .await?;
    Ok(match row {
        Some((true, Some(next_run_at))) if next_run_at == due_at => Fired::Changed,
        _ => Fired::NotDue,
    })
}

/// Fires the policy due at `due_at`, once: its job is planned through the
/// readers, then the job written and the time claimed together, and
/// planned again if the policy changed in between. A policy whose last run
/// still has queued devices skips this run and says why. Returns whether a
/// job started.
async fn fire(state: &AppState, id: &str, due_at: i64, now: i64) -> anyhow::Result<bool> {
    for _ in 0..FIRE_ATTEMPTS {
        let Some(firing) = plan_firing(state, id, due_at, now).await? else {
            return Ok(false);
        };
        match write_firing(state, &firing).await? {
            Fired::Claimed { started } => return Ok(started),
            Fired::NotDue => return Ok(false),
            Fired::Changed => {}
        }
    }
    Ok(false)
}

/// Starts a job of the policy now, as `user_id`. The schedule is unchanged.
pub async fn run_now(
    state: &AppState,
    policy: &Policy,
    user_id: &str,
    actor: Actor,
    now: i64,
) -> Result<String, ApiError> {
    let cannot_run =
        |problem: &str| ApiError::conflict(problem.to_owned()).with_code("policy_cannot_run");
    let prepared = prepare(
        state,
        policy,
        policy.target.clone(),
        Starter::User { user_id, actor },
    )
    .await?;
    // Nothing is written for a job that can't start.
    match &prepared {
        Prepared::Job(_) => {}
        Prepared::NoDevices => return Err(cannot_run(NO_DEVICES)),
        Prepared::Problem(problem) => return Err(cannot_run(problem)),
    }
    let transaction = state.database.begin().await?;
    match start_job(
        state,
        transaction,
        policy,
        &prepared,
        policy.trigger.scheduled(),
        None,
        OnProblem::Refuse,
        now,
    )
    .await?
    {
        Some(Ok(job_id)) => Ok(job_id),
        Some(Err(problem)) => Err(cannot_run(&problem)),
        // Only a firing has a claim to lose.
        None => Err(ApiError::internal()),
    }
}

/// Starts the enabled enrollment policies that cover a newly enrolled device
/// on it. Returns how many started.
pub async fn device_enrolled(state: &AppState, device_id: &str, now: i64) -> anyhow::Result<usize> {
    let mut database = &state.database;
    let Some(device) = crate::targets::device(&mut database, device_id).await? else {
        return Ok(0);
    };
    let policies = load(&mut database, None).await?;
    let mut started = 0;
    for policy in policies.iter().filter(|policy| {
        policy.enabled
            && (policy.trigger == Trigger::Enroll || policy.action.installs_applications())
            && policy.target.covers(&device)
    }) {
        let mut enrolling = policy.clone();
        // A device that just enrolled connects in a moment.
        enrolling.offline = Offline::Wait;
        let prepared = prepare(
            state,
            &enrolling,
            Target::Devices(vec![device_id.to_owned()]),
            Starter::Policy,
        )
        .await
        .map_err(failed)?;
        let transaction = state.database.begin().await?;
        if let Some(Ok(job_id)) = start_job(
            state,
            transaction,
            &enrolling,
            &prepared,
            false,
            None,
            OnProblem::Record(None),
            now,
        )
        .await
        .map_err(failed)?
        {
            tracing::info!(
                policy_id = policy.id,
                device_id,
                job_id,
                "an automation policy started a job on a new device"
            );
            started += 1;
        }
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
