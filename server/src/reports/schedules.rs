//! Report schedules: a template on a scope, for the period that ends when it
//! fires, made daily, weekly or monthly at a time of day in a time zone, and
//! emailed to chosen users and addresses.
//!
//! Times of day follow [`crate::automation::schedule`]: a daily 07:00 stays
//! 07:00 across daylight saving changes, a time the clocks skip fires at the
//! first minute after the change, and one they repeat fires the first time.
//! Monthly schedules run on day 1 to 28, which every month has. A schedule
//! runs with the access of the user who last saved it. One the server
//! missed while it was down fires once when it starts.
use std::{collections::HashMap, str::FromStr};

use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    DEFAULT_STALE_DAYS, MAX_STALE_DAYS, Template,
    period::PeriodKind,
    scope::{Scope, ScopeInput},
};
use crate::{
    automation::schedule::{self, Trigger, Weekdays},
    db::{
        self, Executor,
        tables::{
            ReportScheduleCompanies, ReportScheduleTags, ReportScheduleUsers, ReportSchedules,
            Users,
        },
    },
    http::ApiError,
    targets::{TagMatch, TargetKind},
};

pub const MAX_NAME_LENGTH: usize = 120;
/// The most users and addresses a schedule emails, together.
pub const MAX_RECIPIENTS: usize = 50;
/// The last day of the month a monthly schedule may run on: every month has
/// it.
pub const MAX_DAY_OF_MONTH: u32 = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrequencyKind {
    Daily,
    Weekly,
    Monthly,
}

impl FrequencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }
}

/// When a schedule fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Daily,
    /// On an ISO weekday, Monday = 1.
    Weekly(u8),
    /// On a day of the month, 1 to [`MAX_DAY_OF_MONTH`].
    Monthly(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct When {
    pub frequency: Frequency,
    pub time: NaiveTime,
    pub zone: Tz,
}

impl When {
    pub fn kind(&self) -> FrequencyKind {
        match self.frequency {
            Frequency::Daily => FrequencyKind::Daily,
            Frequency::Weekly(_) => FrequencyKind::Weekly,
            Frequency::Monthly(_) => FrequencyKind::Monthly,
        }
    }

    /// The first time after `after_ms` it fires.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        match self.frequency {
            Frequency::Daily => Trigger::Daily {
                time: self.time,
                zone: self.zone,
            }
            .next_after(after_ms, after_ms),
            Frequency::Weekly(day) => Trigger::Weekly {
                days: Weekdays::from_days(&[day])?,
                time: self.time,
                zone: self.zone,
            }
            .next_after(after_ms, after_ms),
            Frequency::Monthly(day) => {
                let after = DateTime::<Utc>::from_timestamp_millis(after_ms)?;
                let local = after.with_timezone(&self.zone).date_naive();
                let first = NaiveDate::from_ymd_opt(local.year(), local.month(), 1)?;
                (0..3).find_map(|months| {
                    let month = first.checked_add_months(Months::new(months))?;
                    let date = month.with_day(day)?;
                    schedule::resolve(self.zone, date.and_time(self.time))
                        .filter(|at| *at > after_ms)
                })
            }
        }
    }
}

fn enabled_by_default() -> bool {
    true
}

fn default_stale_days() -> i64 {
    DEFAULT_STALE_DAYS
}

/// A schedule as requests send it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInput {
    pub name: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub template: Template,
    pub target: TargetKind,
    #[serde(default)]
    pub company_ids: Vec<String>,
    #[serde(default)]
    pub tag_ids: Vec<String>,
    #[serde(default)]
    pub tag_match: TagMatch,
    pub period: PeriodKind,
    #[serde(default = "default_stale_days")]
    pub stale_days: i64,
    pub frequency: FrequencyKind,
    /// `HH:MM`.
    pub time: String,
    /// ISO day, Monday = 1, for a weekly schedule.
    #[serde(default)]
    pub weekday: Option<u8>,
    /// For a monthly schedule.
    #[serde(default)]
    pub day_of_month: Option<u32>,
    pub time_zone: String,
    /// Users to email each report to.
    #[serde(default)]
    pub user_ids: Vec<String>,
    /// Other addresses to email each report to.
    #[serde(default)]
    pub emails: Vec<String>,
}

/// A checked schedule, ready to save.
#[derive(Debug, Clone, PartialEq)]
pub struct Valid {
    pub name: String,
    pub enabled: bool,
    pub template: Template,
    pub scope: Scope,
    pub period: PeriodKind,
    pub stale_days: i64,
    pub when: When,
    pub user_ids: Vec<String>,
    pub emails: Vec<String>,
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_schedule")
}

/// An email address as a recipient: trimmed, checked and lowercased, as
/// users' addresses are kept.
pub fn parse_email(text: &str) -> Option<String> {
    let address = lettre::Address::from_str(text.trim()).ok()?;
    Some(address.to_string().to_lowercase())
}

impl ScheduleInput {
    pub fn validate(self) -> Result<Valid, ApiError> {
        let name = self.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_NAME_LENGTH
            || name.chars().any(char::is_control)
        {
            return Err(invalid(format!(
                "the schedule name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
            )));
        }
        if self.period == PeriodKind::Custom {
            return Err(invalid(
                "a schedule covers the last 7 or 30 days or the previous month, which end when it runs",
            ));
        }
        if !(1..=MAX_STALE_DAYS).contains(&self.stale_days) {
            return Err(invalid(format!(
                "devices count as not seen after 1 to {MAX_STALE_DAYS} days"
            )));
        }
        let zone = schedule::parse_zone(&self.time_zone)
            .ok_or_else(|| invalid("choose a time zone such as Europe/London"))?;
        let time = schedule::parse_time_of_day(&self.time)
            .ok_or_else(|| invalid("the time must be HH:MM, from 00:00 to 23:59"))?;
        let frequency = match self.frequency {
            FrequencyKind::Daily => Frequency::Daily,
            FrequencyKind::Weekly => match self.weekday {
                Some(day @ 1..=7) => Frequency::Weekly(day),
                _ => return Err(invalid("choose the day of the week it runs on")),
            },
            FrequencyKind::Monthly => match self.day_of_month {
                Some(day @ 1..=MAX_DAY_OF_MONTH) => Frequency::Monthly(day),
                _ => {
                    return Err(invalid(format!(
                        "choose a day of the month from 1 to {MAX_DAY_OF_MONTH}"
                    )));
                }
            },
        };
        let scope = Scope::from_input(ScopeInput {
            target: self.target,
            company_ids: self.company_ids,
            tag_ids: self.tag_ids,
            tag_match: self.tag_match,
        })
        .map_err(|error| error.with_code("invalid_schedule"))?;
        let mut user_ids = self.user_ids;
        for id in &user_ids {
            crate::agents::parse_id(id, "user")?;
        }
        user_ids.sort();
        user_ids.dedup();
        let mut emails = Vec::with_capacity(self.emails.len());
        for email in &self.emails {
            if email.trim().is_empty() {
                continue;
            }
            emails.push(
                parse_email(email)
                    .ok_or_else(|| invalid(format!("{} isn't an email address", email.trim())))?,
            );
        }
        emails.sort();
        emails.dedup();
        if user_ids.len() + emails.len() > MAX_RECIPIENTS {
            return Err(invalid(format!(
                "a schedule emails at most {MAX_RECIPIENTS} users and addresses"
            )));
        }
        Ok(Valid {
            name: name.to_owned(),
            enabled: self.enabled,
            template: self.template,
            scope,
            period: self.period,
            stale_days: self.stale_days,
            when: When {
                frequency,
                time,
                zone,
            },
            user_ids,
            emails,
        })
    }
}

impl Valid {
    /// Checks that its companies, tags and users exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        self.scope.check_references(executor).await?;
        if !self.user_ids.is_empty() {
            let found: Vec<(String,)> = executor
                .fetch_all(
                    &Query::select()
                        .column(Users::Id)
                        .from(Users::Table)
                        .and_where(Expr::col(Users::Id).is_in(self.user_ids.iter().cloned()))
                        .to_owned(),
                )
                .await?;
            if found.len() != self.user_ids.len() {
                return Err(ApiError::not_found("a chosen user no longer exists"));
            }
        }
        Ok(())
    }

    /// When it fires next after `now`; `None` while it's off.
    pub fn next_run(&self, now: i64) -> Option<i64> {
        self.enabled.then(|| self.when.next_after(now)).flatten()
    }
}

/// A saved schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub valid: Valid,
    pub owner_user_id: String,
    /// The owner's email, or "a removed user".
    pub owner: String,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_report_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Schedule {
    pub fn recipients(&self) -> usize {
        self.valid.user_ids.len() + self.valid.emails.len()
    }

    /// The schedule as the API returns it.
    pub fn view(&self, viewer_id: &str) -> Value {
        let valid = &self.valid;
        let target = valid.scope.target();
        json!({
            "id": self.id,
            "name": valid.name,
            "enabled": valid.enabled,
            "template": valid.template,
            "target": target.kind(),
            "company_ids": target.company_ids(),
            "tag_ids": target.tag_ids(),
            "tag_match": target.tag_match(),
            "period": valid.period,
            "stale_days": valid.stale_days,
            "frequency": valid.when.kind(),
            "time": schedule::format_time_of_day(valid.when.time),
            "weekday": match valid.when.frequency { Frequency::Weekly(day) => Some(day), _ => None },
            "day_of_month": match valid.when.frequency { Frequency::Monthly(day) => Some(day), _ => None },
            "time_zone": valid.when.zone.name(),
            "user_ids": valid.user_ids,
            "emails": valid.emails,
            "owner": self.owner,
            "owned_by_you": self.owner_user_id == viewer_id,
            "next_run_at": self.next_run_at,
            "last_run_at": self.last_run_at,
            "last_report_id": self.last_report_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }

    /// What the audit log keeps of a change.
    pub fn audit_metadata(&self) -> Value {
        let valid = &self.valid;
        json!({
            "name": valid.name,
            "enabled": valid.enabled,
            "template": valid.template,
            "target": valid.scope.target().kind(),
            "companies": valid.scope.target().company_ids().len(),
            "tags": valid.scope.target().tag_ids().len(),
            "period": valid.period,
            "frequency": valid.when.kind(),
            "time": schedule::format_time_of_day(valid.when.time),
            "time_zone": valid.when.zone.name(),
            "users": valid.user_ids,
            "emails": valid.emails,
            "next_run_at": self.next_run_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    name: String,
    enabled: bool,
    template: String,
    target: String,
    tag_match: String,
    period: String,
    stale_days: i64,
    frequency: String,
    time_of_day: String,
    weekday: Option<i64>,
    day_of_month: Option<i64>,
    time_zone: String,
    emails: String,
    owner_user_id: String,
    owner_email: Option<String>,
    next_run_at: Option<i64>,
    last_run_at: Option<i64>,
    last_report_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

/// Every schedule by name, or only `id`. One that can't be read (its zone
/// left the time zone database, say) is left out and logged.
pub async fn load(executor: &mut impl Executor, id: Option<&str>) -> db::Result<Vec<Schedule>> {
    let mut select = Query::select();
    select
        .columns([
            (ReportSchedules::Table, ReportSchedules::Id),
            (ReportSchedules::Table, ReportSchedules::Name),
            (ReportSchedules::Table, ReportSchedules::Enabled),
            (ReportSchedules::Table, ReportSchedules::Template),
            (ReportSchedules::Table, ReportSchedules::Target),
            (ReportSchedules::Table, ReportSchedules::TagMatch),
            (ReportSchedules::Table, ReportSchedules::Period),
            (ReportSchedules::Table, ReportSchedules::StaleDays),
            (ReportSchedules::Table, ReportSchedules::Frequency),
            (ReportSchedules::Table, ReportSchedules::TimeOfDay),
            (ReportSchedules::Table, ReportSchedules::Weekday),
            (ReportSchedules::Table, ReportSchedules::DayOfMonth),
            (ReportSchedules::Table, ReportSchedules::TimeZone),
            (ReportSchedules::Table, ReportSchedules::Emails),
            (ReportSchedules::Table, ReportSchedules::OwnerUserId),
        ])
        .expr_as(
            Expr::col((Users::Table, Users::Email)),
            sea_query::Alias::new("owner_email"),
        )
        .columns([
            (ReportSchedules::Table, ReportSchedules::NextRunAt),
            (ReportSchedules::Table, ReportSchedules::LastRunAt),
            (ReportSchedules::Table, ReportSchedules::LastReportId),
            (ReportSchedules::Table, ReportSchedules::CreatedAt),
            (ReportSchedules::Table, ReportSchedules::UpdatedAt),
        ])
        .from(ReportSchedules::Table)
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((ReportSchedules::Table, ReportSchedules::OwnerUserId)),
        )
        .order_by((ReportSchedules::Table, ReportSchedules::Name), Order::Asc);
    if let Some(id) = id {
        select.and_where(Expr::col((ReportSchedules::Table, ReportSchedules::Id)).eq(id));
    }
    let rows: Vec<Row> = executor.fetch_all(&select).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let links = |table: sea_query::DynIden, column: sea_query::DynIden| {
        let mut select = Query::select();
        select
            .column(sea_query::Alias::new("schedule_id"))
            .column(column)
            .from(table);
        if let Some(id) = id {
            select.and_where(Expr::col(sea_query::Alias::new("schedule_id")).eq(id));
        }
        select
    };
    let companies: Vec<(String, String)> = executor
        .fetch_all(&links(
            ReportScheduleCompanies::Table.into_iden_dyn(),
            ReportScheduleCompanies::CompanyId.into_iden_dyn(),
        ))
        .await?;
    let tags: Vec<(String, String)> = executor
        .fetch_all(&links(
            ReportScheduleTags::Table.into_iden_dyn(),
            ReportScheduleTags::TagId.into_iden_dyn(),
        ))
        .await?;
    let users: Vec<(String, String)> = executor
        .fetch_all(&links(
            ReportScheduleUsers::Table.into_iden_dyn(),
            ReportScheduleUsers::UserId.into_iden_dyn(),
        ))
        .await?;
    let (mut companies, mut tags, mut users) = (grouped(companies), grouped(tags), grouped(users));
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id = row.id.clone();
            let schedule = schedule_of(
                row,
                companies.remove(&id).unwrap_or_default(),
                tags.remove(&id).unwrap_or_default(),
                users.remove(&id).unwrap_or_default(),
            );
            if schedule.is_none() {
                tracing::warn!(schedule_id = id, "could not read a report schedule");
            }
            schedule
        })
        .collect())
}

fn grouped(pairs: Vec<(String, String)>) -> HashMap<String, Vec<String>> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (owner, id) in pairs {
        grouped.entry(owner).or_default().push(id);
    }
    for ids in grouped.values_mut() {
        ids.sort();
    }
    grouped
}

fn schedule_of(
    row: Row,
    company_ids: Vec<String>,
    tag_ids: Vec<String>,
    user_ids: Vec<String>,
) -> Option<Schedule> {
    let frequency = match FrequencyKind::parse(&row.frequency)? {
        FrequencyKind::Daily => Frequency::Daily,
        FrequencyKind::Weekly => Frequency::Weekly(u8::try_from(row.weekday?).ok()?),
        FrequencyKind::Monthly => Frequency::Monthly(u32::try_from(row.day_of_month?).ok()?),
    };
    Some(Schedule {
        valid: Valid {
            name: row.name,
            enabled: row.enabled,
            template: Template::parse(&row.template)?,
            scope: Scope::from_stored(&row.target, &row.tag_match, company_ids, tag_ids)?,
            period: PeriodKind::parse(&row.period)?,
            stale_days: row.stale_days,
            when: When {
                frequency,
                time: schedule::parse_time_of_day(&row.time_of_day)?,
                zone: schedule::parse_zone(&row.time_zone)?,
            },
            user_ids,
            emails: serde_json::from_str(&row.emails).ok()?,
        },
        id: row.id,
        owner: row
            .owner_email
            .unwrap_or_else(|| "a removed user".to_owned()),
        owner_user_id: row.owner_user_id,
        next_run_at: row.next_run_at,
        last_run_at: row.last_run_at,
        last_report_id: row.last_report_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<Schedule>> {
    Ok(load(executor, Some(id)).await?.into_iter().next())
}

/// Writes the schedule `id` from `schedule`, owned by `owner_user_id`: a new
/// one, or over the existing one. Its next run counts from `now`.
pub async fn save(
    executor: &mut impl Executor,
    id: &str,
    schedule: &Valid,
    owner_user_id: &str,
    now: i64,
    create: bool,
) -> db::Result<()> {
    let (weekday, day_of_month) = match schedule.when.frequency {
        Frequency::Daily => (None, None),
        Frequency::Weekly(day) => (Some(i64::from(day)), None),
        Frequency::Monthly(day) => (None, Some(i64::from(day))),
    };
    let target = schedule.scope.target();
    let values: Vec<(ReportSchedules, sea_query::Expr)> = vec![
        (ReportSchedules::Name, schedule.name.clone().into()),
        (ReportSchedules::Enabled, schedule.enabled.into()),
        (ReportSchedules::Template, schedule.template.as_str().into()),
        (ReportSchedules::Target, target.kind().as_str().into()),
        (
            ReportSchedules::TagMatch,
            target.tag_match().as_str().into(),
        ),
        (ReportSchedules::Period, schedule.period.as_str().into()),
        (ReportSchedules::StaleDays, schedule.stale_days.into()),
        (
            ReportSchedules::Frequency,
            schedule.when.kind().as_str().into(),
        ),
        (
            ReportSchedules::TimeOfDay,
            schedule::format_time_of_day(schedule.when.time).into(),
        ),
        (ReportSchedules::Weekday, weekday.into()),
        (ReportSchedules::DayOfMonth, day_of_month.into()),
        (ReportSchedules::TimeZone, schedule.when.zone.name().into()),
        (
            ReportSchedules::Emails,
            serde_json::to_string(&schedule.emails)
                .unwrap_or_else(|_| "[]".to_owned())
                .into(),
        ),
        (ReportSchedules::OwnerUserId, owner_user_id.into()),
        (ReportSchedules::NextRunAt, schedule.next_run(now).into()),
        (ReportSchedules::UpdatedAt, now.into()),
    ];
    if create {
        let mut columns = vec![ReportSchedules::Id, ReportSchedules::CreatedAt];
        let mut row: Vec<sea_query::Expr> = vec![id.into(), now.into()];
        for (column, value) in values {
            columns.push(column);
            row.push(value);
        }
        executor
            .execute(
                &Query::insert()
                    .into_table(ReportSchedules::Table)
                    .columns(columns)
                    .values_panic(row)
                    .to_owned(),
            )
            .await?;
    } else {
        executor
            .execute(
                &Query::update()
                    .table(ReportSchedules::Table)
                    .values(values)
                    .and_where(Expr::col(ReportSchedules::Id).eq(id))
                    .to_owned(),
            )
            .await?;
        for (table, column) in [
            (
                ReportScheduleCompanies::Table.into_iden_dyn(),
                ReportScheduleCompanies::ScheduleId.into_iden_dyn(),
            ),
            (
                ReportScheduleTags::Table.into_iden_dyn(),
                ReportScheduleTags::ScheduleId.into_iden_dyn(),
            ),
            (
                ReportScheduleUsers::Table.into_iden_dyn(),
                ReportScheduleUsers::ScheduleId.into_iden_dyn(),
            ),
        ] {
            executor
                .execute(
                    &Query::delete()
                        .from_table(table)
                        .and_where(Expr::col(column).eq(id))
                        .to_owned(),
                )
                .await?;
        }
    }
    let links: [(sea_query::DynIden, sea_query::DynIden, &[String]); 3] = [
        (
            ReportScheduleCompanies::Table.into_iden_dyn(),
            ReportScheduleCompanies::CompanyId.into_iden_dyn(),
            target.company_ids(),
        ),
        (
            ReportScheduleTags::Table.into_iden_dyn(),
            ReportScheduleTags::TagId.into_iden_dyn(),
            target.tag_ids(),
        ),
        (
            ReportScheduleUsers::Table.into_iden_dyn(),
            ReportScheduleUsers::UserId.into_iden_dyn(),
            &schedule.user_ids,
        ),
    ];
    for (table, column, ids) in links {
        if ids.is_empty() {
            continue;
        }
        let mut insert = Query::insert();
        insert
            .into_table(table)
            .columns([sea_query::Alias::new("schedule_id").into_iden_dyn(), column]);
        for linked in ids {
            insert.values_panic([id.into(), linked.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

/// `into_iden` as a [`sea_query::DynIden`], for lists of tables and columns.
trait IntoIdenDyn {
    fn into_iden_dyn(self) -> sea_query::DynIden;
}

impl<T: sea_query::IntoIden> IntoIdenDyn for T {
    fn into_iden_dyn(self) -> sea_query::DynIden {
        self.into_iden()
    }
}

/// The addresses a schedule's reports go to: its enabled users' and its
/// other addresses, each once.
pub async fn recipients(
    executor: &mut impl Executor,
    schedule: &Schedule,
) -> db::Result<Vec<String>> {
    let mut addresses = schedule.valid.emails.clone();
    if !schedule.valid.user_ids.is_empty() {
        let rows: Vec<(String,)> = executor
            .fetch_all(
                &Query::select()
                    .column(Users::Email)
                    .from(Users::Table)
                    .and_where(Expr::col(Users::Id).is_in(schedule.valid.user_ids.iter().cloned()))
                    .and_where(Expr::col(Users::Disabled).eq(false))
                    .to_owned(),
            )
            .await?;
        addresses.extend(rows.into_iter().map(|(email,)| email));
    }
    addresses.sort();
    addresses.dedup();
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(zone: Tz, text: &str) -> i64 {
        schedule::resolve(zone, schedule::parse_local_date_time(text).unwrap()).unwrap()
    }

    fn local(zone: Tz, at: i64) -> String {
        schedule::format_local_date_time(
            DateTime::<Utc>::from_timestamp_millis(at)
                .unwrap()
                .with_timezone(&zone)
                .naive_local(),
        )
    }

    fn input(value: Value) -> Result<Valid, ApiError> {
        let mut base = json!({
            "name": "Weekly summary", "template": "executive_summary", "target": "all",
            "period": "last_7_days", "frequency": "weekly", "weekday": 1, "time": "07:00",
            "time_zone": "Europe/London",
        });
        for (key, value) in value.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value::<ScheduleInput>(base)
            .unwrap()
            .validate()
    }

    #[test]
    fn schedules_fire_daily_weekly_or_monthly_in_their_zone() {
        let zone: Tz = "America/New_York".parse().unwrap();
        let time = schedule::parse_time_of_day("07:00").unwrap();
        let daily = When {
            frequency: Frequency::Daily,
            time,
            zone,
        };
        let next = daily.next_after(at(zone, "2026-03-07T08:00")).unwrap();
        assert_eq!(local(zone, next), "2026-03-08T07:00");
        let weekly = When {
            frequency: Frequency::Weekly(1),
            ..daily
        };
        // Wednesday 11 November 2026: the next Monday.
        let next = weekly.next_after(at(zone, "2026-11-11T12:00")).unwrap();
        assert_eq!(local(zone, next), "2026-11-16T07:00");
        let monthly = When {
            frequency: Frequency::Monthly(1),
            ..daily
        };
        let next = monthly.next_after(at(zone, "2026-11-11T12:00")).unwrap();
        assert_eq!(local(zone, next), "2026-12-01T07:00");
        let next = monthly.next_after(at(zone, "2026-12-01T06:59")).unwrap();
        assert_eq!(local(zone, next), "2026-12-01T07:00");
        let next = monthly.next_after(next).unwrap();
        assert_eq!(local(zone, next), "2027-01-01T07:00");
        let late = When {
            frequency: Frequency::Monthly(28),
            ..daily
        };
        let next = late.next_after(at(zone, "2027-02-28T07:00")).unwrap();
        assert_eq!(local(zone, next), "2027-03-28T07:00");
    }

    #[test]
    fn schedules_are_checked() {
        let valid =
            input(json!({ "emails": [" Ops@Example.COM ", "ops@example.com", ""] })).unwrap();
        assert_eq!(valid.emails, ["ops@example.com"]);
        assert_eq!(valid.when.frequency, Frequency::Weekly(1));
        assert!(input(json!({ "period": "custom" })).is_err());
        assert!(input(json!({ "weekday": 8 })).is_err());
        assert!(input(json!({ "frequency": "monthly", "day_of_month": 29 })).is_err());
        assert!(input(json!({ "frequency": "monthly", "day_of_month": 28 })).is_ok());
        assert!(input(json!({ "time": "7:00" })).is_err());
        assert!(input(json!({ "time_zone": "Mars/Olympus" })).is_err());
        assert!(input(json!({ "emails": ["not an address"] })).is_err());
        assert!(input(json!({ "stale_days": 0 })).is_err());
        assert!(input(json!({ "target": "devices" })).is_err());
        assert!(input(json!({ "target": "companies" })).is_err());
        assert!(input(json!({ "name": " " })).is_err());
        let many = (0..=MAX_RECIPIENTS)
            .map(|n| format!("person{n}@example.com"))
            .collect::<Vec<_>>();
        assert!(input(json!({ "emails": many })).is_err());
        let off = input(json!({ "enabled": false })).unwrap();
        assert_eq!(off.next_run(0), None);
    }
}
