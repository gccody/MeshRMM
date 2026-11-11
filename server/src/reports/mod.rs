//! Reports: documents made from what the server holds about the fleet, on
//! demand or on a schedule. See docs/reports.md.
//!
//! Asking for a report records it as queued, with its period resolved then,
//! and wakes the engine's task, which makes queued reports one at a time,
//! off the request path. Making one only reads, through the database's
//! readers and outside any transaction, so it never holds SQLite's writer:
//! the writes are its claim, its outcome and its delivery, each a single
//! short statement. The document is kept as JSON in the data directory,
//! from which its HTML page and each table's CSV are made when asked for,
//! until [`RETENTION_MS`] has passed and housekeeping removes both.
//!
//! The task also fires due schedules, each in one transaction that moves the
//! schedule on and queues its report, and emails each schedule's report to
//! its recipients once it is made.
pub mod delivery;
pub mod document;
pub mod html;
pub mod period;
pub mod queries;
pub mod schedules;
pub mod scope;
pub mod templates;

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use bytes::Bytes;
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use self::{
    document::Document,
    period::{Period, PeriodKind},
    schedules::Schedule,
    scope::Scope,
};
use crate::{
    audit::{self, Actor},
    db::{self, Executor, tables::Reports},
    http::AppState,
    rbac::{self, Permission},
    storage::Durability,
    time::{DAY_MS, now_ms},
    users,
};

/// How long a generated report is kept.
pub const RETENTION_MS: i64 = 90 * DAY_MS;
/// How long the executive summary counts a device as not seen after, unless
/// asked otherwise.
pub const DEFAULT_STALE_DAYS: i64 = 30;
pub const MAX_STALE_DAYS: i64 = 365;
/// How often the task looks for due schedules when nothing wakes it.
const IDLE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Template {
    ExecutiveSummary,
    PatchCompliance,
    HardwareInventory,
    SoftwareInventory,
    Alerts,
}

impl Template {
    pub const ALL: [Template; 5] = [
        Self::ExecutiveSummary,
        Self::PatchCompliance,
        Self::HardwareInventory,
        Self::SoftwareInventory,
        Self::Alerts,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecutiveSummary => "executive_summary",
            Self::PatchCompliance => "patch_compliance",
            Self::HardwareInventory => "hardware_inventory",
            Self::SoftwareInventory => "software_inventory",
            Self::Alerts => "alerts",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|template| template.as_str() == text)
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::ExecutiveSummary => "Executive summary",
            Self::PatchCompliance => "Patch compliance",
            Self::HardwareInventory => "Hardware asset inventory",
            Self::SoftwareInventory => "Software inventory",
            Self::Alerts => "Alerts",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ExecutiveSummary => {
                "The fleet at a glance: devices by platform and operating system, online state and devices not seen lately, alerts raised and resolved with the time to resolve them, patch and application posture, jobs and policy runs, and the devices with the most alerts."
            }
            Self::PatchCompliance => {
                "Each device's last update scan, its pending updates by category and severity, failed installs in the period and pending restarts, with totals across the devices and the share that is compliant."
            }
            Self::HardwareInventory => {
                "Each device's model, serial number, processor, memory, disks and free space, operating system, Agent version, company and when it was last seen."
            }
            Self::SoftwareInventory => {
                "The applications on the devices with their versions and how many devices have each, and the packages their package managers can update."
            }
            Self::Alerts => {
                "The alerts raised in the period by kind, rule and device, how many are still open, and how long the resolved ones took."
            }
        }
    }

    /// The sections the template's reports have, in order.
    pub fn sections(self) -> &'static [&'static str] {
        match self {
            Self::ExecutiveSummary => &[
                "Devices",
                "Online state",
                "Alerts",
                "Patch posture",
                "Application posture",
                "Jobs and policies",
                "Top devices by alerts",
            ],
            Self::PatchCompliance => &["Compliance", "Pending updates", "Devices"],
            Self::HardwareInventory => &["Summary", "Devices"],
            Self::SoftwareInventory => {
                &["Summary", "Applications", "Versions", "Outdated packages"]
            }
            Self::Alerts => &["Summary", "By kind", "By rule", "By device", "Alerts"],
        }
    }

    /// Whether anything in it depends on the period. Inventories are as the
    /// devices are when the report is made.
    pub fn uses_period(self) -> bool {
        matches!(
            self,
            Self::ExecutiveSummary | Self::PatchCompliance | Self::Alerts
        )
    }

    pub fn uses_stale_days(self) -> bool {
        self == Self::ExecutiveSummary
    }

    pub fn view(self) -> Value {
        json!({
            "id": self,
            "title": self.title(),
            "description": self.description(),
            "sections": self.sections(),
            "uses_period": self.uses_period(),
            "uses_stale_days": self.uses_stale_days(),
        })
    }
}

/// The task that makes queued reports and fires schedules. Wake it after
/// queuing a report or saving a schedule.
#[derive(Debug, Clone, Default)]
pub struct Engine {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    wake: Notify,
    /// One step at a time, so a report is made and emailed once.
    stepping: tokio::sync::Mutex<()>,
}

impl Engine {
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }
}

/// Makes queued reports, fires due schedules and emails what they made,
/// until the returned task is aborted. Reports a stopped server was making
/// are queued again first.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = recover(&state).await {
            tracing::warn!(%error, "could not queue again the reports being made at shutdown");
        }
        loop {
            if let Err(error) = step(&state, now_ms()).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "the report task failed a step"
                );
            }
            tokio::select! {
                () = state.reports.inner.wake.notified() => {}
                () = tokio::time::sleep(IDLE_INTERVAL) => {}
            }
        }
    })
}

/// Queues again the reports a stopped server was making.
pub async fn recover(state: &AppState) -> db::Result<u64> {
    state
        .database
        .execute(
            &Query::update()
                .table(Reports::Table)
                .value(Reports::Status, "queued")
                .value(Reports::StartedAt, Option::<i64>::None)
                .and_where(Expr::col(Reports::Status).eq("running"))
                .to_owned(),
        )
        .await
}

/// What a step did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stepped {
    pub fired: usize,
    pub made: usize,
    pub delivered: usize,
}

/// Fires the schedules due at `now`, makes every queued report and emails
/// the schedules' reports that wait to be sent.
pub async fn step(state: &AppState, now: i64) -> anyhow::Result<Stepped> {
    let _step = state.reports.inner.stepping.lock().await;
    let fired = fire_due(state, now).await?;
    let mut made = 0;
    while let Some(id) = next_queued(state).await? {
        if make(state, &id).await? {
            made += 1;
        }
    }
    let delivered = delivery::deliver_pending(state).await?;
    Ok(Stepped {
        fired,
        made,
        delivered,
    })
}

async fn next_queued(state: &AppState) -> db::Result<Option<String>> {
    let row: Option<(String,)> = state
        .database
        .fetch_optional(
            &Query::select()
                .column(Reports::Id)
                .from(Reports::Table)
                .and_where(Expr::col(Reports::Status).eq("queued"))
                .order_by(Reports::CreatedAt, Order::Asc)
                .order_by(Reports::Id, Order::Asc)
                .limit(1)
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(id,)| id))
}

/// A stored report.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Report {
    pub id: String,
    pub template: String,
    pub status: String,
    pub error: Option<String>,
    pub trigger: String,
    pub schedule_id: Option<String>,
    pub schedule_name: Option<String>,
    pub requested_by_user_id: Option<String>,
    pub requested_by_label: String,
    pub scope: String,
    pub scope_label: String,
    pub period: String,
    pub period_start: i64,
    pub period_end: i64,
    pub time_zone: String,
    pub stale_days: i64,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub expires_at: i64,
    pub size_bytes: Option<i64>,
    pub delivery: Option<String>,
    pub delivery_detail: Option<String>,
}

const REPORT_COLUMNS: [Reports; 23] = [
    Reports::Id,
    Reports::Template,
    Reports::Status,
    Reports::Error,
    Reports::Trigger,
    Reports::ScheduleId,
    Reports::ScheduleName,
    Reports::RequestedByUserId,
    Reports::RequestedByLabel,
    Reports::Scope,
    Reports::ScopeLabel,
    Reports::Period,
    Reports::PeriodStart,
    Reports::PeriodEnd,
    Reports::TimeZone,
    Reports::StaleDays,
    Reports::CreatedAt,
    Reports::StartedAt,
    Reports::FinishedAt,
    Reports::ExpiresAt,
    Reports::SizeBytes,
    Reports::Delivery,
    Reports::DeliveryDetail,
];

impl Report {
    pub fn template(&self) -> Option<Template> {
        Template::parse(&self.template)
    }

    pub fn zone(&self) -> chrono_tz::Tz {
        self.time_zone.parse().unwrap_or(chrono_tz::Tz::UTC)
    }

    pub fn period(&self) -> Option<Period> {
        Some(Period {
            kind: PeriodKind::parse(&self.period)?,
            start_ms: self.period_start,
            end_ms: self.period_end,
            zone: self.zone(),
        })
    }

    /// The report as the API returns it.
    pub fn view(&self, viewer_id: &str) -> Value {
        let template = self.template();
        let scope = Scope::from_json(&self.scope);
        let target = scope.as_ref().map(Scope::target);
        json!({
            "id": self.id,
            "template": self.template,
            "title": template.map(Template::title),
            "status": self.status,
            "error": self.error,
            "trigger": self.trigger,
            "schedule_id": self.schedule_id,
            "schedule_name": self.schedule_name,
            "requested_by": self.requested_by_label,
            "requested_by_you": self.requested_by_user_id.as_deref() == Some(viewer_id) && self.trigger != "schedule",
            "target": target.map(|target| target.kind()),
            "company_ids": target.map(|target| target.company_ids()),
            "tag_ids": target.map(|target| target.tag_ids()),
            "tag_match": target.map(|target| target.tag_match()),
            "scope_label": self.scope_label,
            "period": self.period,
            "period_start": self.period_start,
            "period_end": self.period_end,
            "period_label": self.period().map(|period| period.label()),
            "time_zone": self.time_zone,
            "stale_days": self.stale_days,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "expires_at": self.expires_at,
            "size_bytes": self.size_bytes,
            "delivery": self.delivery,
            "delivery_detail": self.delivery_detail,
        })
    }
}

/// The reports kept at `now`, newest first: at most `limit`, created before
/// `before`, and only `schedule_id`'s when given.
pub async fn list(
    executor: &mut impl Executor,
    limit: u64,
    before: Option<i64>,
    schedule_id: Option<&str>,
    now: i64,
) -> db::Result<Vec<Report>> {
    let mut select = Query::select();
    select
        .columns(REPORT_COLUMNS)
        .from(Reports::Table)
        .and_where(Expr::col(Reports::ExpiresAt).gt(now))
        .order_by(Reports::CreatedAt, Order::Desc)
        .order_by(Reports::Id, Order::Desc)
        .limit(limit);
    if let Some(before) = before {
        select.and_where(Expr::col(Reports::CreatedAt).lt(before));
    }
    if let Some(schedule_id) = schedule_id {
        select.and_where(Expr::col(Reports::ScheduleId).eq(schedule_id));
    }
    executor.fetch_all(&select).await
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<Report>> {
    executor
        .fetch_optional(
            &Query::select()
                .columns(REPORT_COLUMNS)
                .from(Reports::Table)
                .and_where(Expr::col(Reports::Id).eq(id))
                .to_owned(),
        )
        .await
}

/// Who or what asked for a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A user, from the run dialog or the API.
    Manual,
    /// A schedule firing.
    Schedule,
    /// A user running a schedule now.
    RunNow,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
            Self::RunNow => "run_now",
        }
    }
}

/// A report to queue.
pub struct NewReport<'a> {
    pub id: &'a str,
    pub template: Template,
    pub trigger: Trigger,
    pub schedule: Option<&'a Schedule>,
    /// Whose access it is made with.
    pub user_id: Option<&'a str>,
    pub scope: &'a Scope,
    pub scope_label: &'a str,
    pub period: Period,
    pub stale_days: i64,
    pub now: i64,
    /// Why it can't be made, so it is recorded as failed.
    pub problem: Option<String>,
}

/// Records the report as queued (or failed, with its problem), and the
/// audit event saying who or what asked for it, in `executor`'s
/// transaction. Wake the engine after it commits.
pub async fn queue(
    executor: &mut impl Executor,
    report: &NewReport<'_>,
    actor: &Actor,
) -> db::Result<()> {
    let schedule = report.schedule;
    let delivery = schedule
        .filter(|schedule| schedule.recipients() > 0)
        .map(|_| match report.problem {
            None => "pending",
            Some(_) => "failed",
        });
    let delivery_detail = report
        .problem
        .as_ref()
        .and(delivery)
        .map(|_| "The report couldn't be made, so nothing was sent.");
    let requested_by = match (report.trigger, schedule) {
        (Trigger::Schedule, Some(schedule)) => format!("Schedule “{}”", schedule.valid.name),
        _ => actor.label.clone(),
    };
    let status = if report.problem.is_some() {
        "failed"
    } else {
        "queued"
    };
    executor
        .execute(
            &Query::insert()
                .into_table(Reports::Table)
                .columns([
                    Reports::Id,
                    Reports::Template,
                    Reports::Status,
                    Reports::Error,
                    Reports::Trigger,
                    Reports::ScheduleId,
                    Reports::ScheduleName,
                    Reports::RequestedByUserId,
                    Reports::RequestedByLabel,
                    Reports::Scope,
                    Reports::ScopeLabel,
                    Reports::Period,
                    Reports::PeriodStart,
                    Reports::PeriodEnd,
                    Reports::TimeZone,
                    Reports::StaleDays,
                    Reports::CreatedAt,
                    Reports::FinishedAt,
                    Reports::ExpiresAt,
                    Reports::Delivery,
                    Reports::DeliveryDetail,
                ])
                .values_panic([
                    report.id.into(),
                    report.template.as_str().into(),
                    status.into(),
                    report.problem.clone().into(),
                    report.trigger.as_str().into(),
                    schedule.map(|schedule| schedule.id.clone()).into(),
                    schedule.map(|schedule| schedule.valid.name.clone()).into(),
                    report.user_id.map(str::to_owned).into(),
                    requested_by.into(),
                    report.scope.to_json().into(),
                    report.scope_label.into(),
                    report.period.kind.as_str().into(),
                    report.period.start_ms.into(),
                    report.period.end_ms.into(),
                    report.period.zone.name().into(),
                    report.stale_days.into(),
                    report.now.into(),
                    report.problem.as_ref().map(|_| report.now).into(),
                    (report.now + RETENTION_MS).into(),
                    delivery.into(),
                    delivery_detail.into(),
                ])
                .to_owned(),
        )
        .await?;
    let mut metadata = json!({
        "template": report.template,
        "trigger": report.trigger.as_str(),
        "scope": report.scope_label,
        "target": report.scope.target().kind(),
        "period": report.period.kind.as_str(),
        "period_start": report.period.start_ms,
        "period_end": report.period.end_ms,
        "time_zone": report.period.zone.name(),
    });
    if let Some(schedule) = schedule {
        metadata["schedule_id"] = schedule.id.as_str().into();
        metadata["schedule_name"] = schedule.valid.name.as_str().into();
        metadata["recipients"] = schedule.recipients().into();
    }
    if let Some(problem) = &report.problem {
        metadata["problem"] = problem.as_str().into();
    }
    audit::record(
        executor,
        actor,
        "report.generate",
        audit::Target {
            kind: "report",
            id: report.id,
        },
        metadata,
    )
    .await
}

/// Makes the queued report `id`, if no other step took it. Returns whether
/// it did.
async fn make(state: &AppState, id: &str) -> anyhow::Result<bool> {
    let claimed = state
        .database
        .execute(
            &Query::update()
                .table(Reports::Table)
                .value(Reports::Status, "running")
                .value(Reports::StartedAt, now_ms())
                .and_where(Expr::col(Reports::Id).eq(id))
                .and_where(Expr::col(Reports::Status).eq("queued"))
                .to_owned(),
        )
        .await?;
    if claimed == 0 {
        return Ok(false);
    }
    let Some(report) = by_id(&mut &state.database, id).await? else {
        return Ok(false);
    };
    let outcome = build(state, &report).await;
    let finished_at = now_ms();
    match outcome {
        Ok(contents) => {
            let size = contents.len() as i64;
            let path = state.storage.report(id);
            state
                .storage
                .write(path.clone(), contents, Durability::Durable)
                .await
                .context("could not store a report")?;
            let finished = state
                .database
                .execute(
                    &Query::update()
                        .table(Reports::Table)
                        .value(Reports::Status, "succeeded")
                        .value(Reports::FinishedAt, finished_at)
                        .value(Reports::SizeBytes, size)
                        .and_where(Expr::col(Reports::Id).eq(id))
                        .and_where(Expr::col(Reports::Status).eq("running"))
                        .to_owned(),
                )
                .await?;
            // Deleted while it was being made.
            if finished == 0 {
                state.storage.remove(&path).await?;
            }
        }
        Err(error) => {
            tracing::warn!(
                report_id = id,
                error = format!("{error:#}"),
                "could not make a report"
            );
            state
                .database
                .execute(
                    &Query::update()
                        .table(Reports::Table)
                        .value(Reports::Status, "failed")
                        .value(Reports::Error, "The server couldn't read what the report needs. Try again later.")
                        .value(Reports::FinishedAt, finished_at)
                        .value(
                            Reports::Delivery,
                            Expr::cust("CASE WHEN delivery IS NULL THEN NULL ELSE 'failed' END"),
                        )
                        .value(
                            Reports::DeliveryDetail,
                            Expr::cust("CASE WHEN delivery IS NULL THEN NULL ELSE 'The report couldn''t be made, so nothing was sent.' END"),
                        )
                        .and_where(Expr::col(Reports::Id).eq(id))
                        .and_where(Expr::col(Reports::Status).eq("running"))
                        .to_owned(),
                )
                .await?;
        }
    }
    Ok(true)
}

/// Reads what the report needs and returns its document as JSON.
async fn build(state: &AppState, report: &Report) -> anyhow::Result<Bytes> {
    let template = report.template().context("unknown template")?;
    let scope = Scope::from_json(&report.scope).context("unreadable scope")?;
    let period = report.period().context("unreadable period")?;
    let settings = state.settings.get(&state.database).await?;
    let online = state.agents.connected();
    let now = now_ms();
    let built = templates::build(
        &state.database,
        &templates::Request {
            template,
            scope: &scope,
            period,
            stale_days: report.stale_days,
            now,
            online: &online,
        },
    )
    .await?;
    let period_label = if template.uses_period() {
        format!("{} ({})", period.label(), period.kind.label())
    } else {
        "As the devices are now".to_owned()
    };
    let document = Document {
        template,
        title: template.title().to_owned(),
        instance_name: settings.instance_name.clone(),
        scope_label: report.scope_label.clone(),
        period_start: period.start_ms,
        period_end: period.end_ms,
        period_label,
        time_zone: report.time_zone.clone(),
        generated_at: now,
        coverage: built.coverage,
        sections: built.sections,
    };
    Ok(Bytes::from(serde_json::to_vec(&document)?))
}

/// The stored document of a report that succeeded, if its file is there.
pub async fn document(state: &AppState, id: &str) -> anyhow::Result<Option<Document>> {
    match tokio::fs::read(state.storage.report(id)).await {
        Ok(contents) => Ok(Some(serde_json::from_slice(&contents)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Deletes the report and its document.
pub async fn delete(state: &AppState, id: &str, actor: &Actor) -> db::Result<bool> {
    let mut transaction = state.database.begin().await?;
    let deleted = transaction
        .execute(
            &Query::delete()
                .from_table(Reports::Table)
                .and_where(Expr::col(Reports::Id).eq(id))
                .to_owned(),
        )
        .await?;
    if deleted == 0 {
        return Ok(false);
    }
    audit::record(
        &mut transaction,
        actor,
        "report.delete",
        audit::Target { kind: "report", id },
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    if let Err(error) = state.storage.remove(&state.storage.report(id)).await {
        tracing::warn!(report_id = id, %error, "could not remove a deleted report's file");
    }
    Ok(true)
}

/// Why a schedule's owner can't have it run with their access, if so.
async fn owner_problem(
    executor: &mut impl Executor,
    schedule: &Schedule,
) -> db::Result<Option<String>> {
    let owner = users::by_id(executor, &schedule.owner_user_id).await?;
    let Some(owner) = owner.filter(|owner| !owner.disabled) else {
        return Ok(Some(format!(
            "{}, whose access the schedule runs with, was disabled or removed; save the schedule to run it with your access.",
            schedule.owner
        )));
    };
    let permissions = rbac::permissions_of(&rbac::user_roles(executor, &owner.id).await?);
    if !permissions.contains(&Permission::ReportsView)
        || !permissions.contains(&Permission::ReportsManage)
    {
        return Ok(Some(format!(
            "{}, whose access the schedule runs with, no longer has the reports.view and reports.manage permissions; save the schedule to run it with your access.",
            schedule.owner
        )));
    }
    Ok(None)
}

/// The report a schedule makes now, made with its owner's access.
async fn schedule_report<'a>(
    executor: &mut impl Executor,
    schedule: &'a Schedule,
    id: &'a str,
    scope_label: &'a str,
    trigger: Trigger,
    now: i64,
) -> db::Result<NewReport<'a>> {
    let valid = &schedule.valid;
    let period = Period::resolve(valid.period, None, valid.when.zone, now).unwrap_or(Period {
        kind: valid.period,
        start_ms: now,
        end_ms: now,
        zone: valid.when.zone,
    });
    Ok(NewReport {
        id,
        template: valid.template,
        trigger,
        schedule: Some(schedule),
        user_id: Some(&schedule.owner_user_id),
        scope: &valid.scope,
        scope_label,
        period,
        stale_days: valid.stale_days,
        now,
        problem: owner_problem(executor, schedule).await?,
    })
}

/// Fires the schedules due at `now`. Returns how many fired.
pub async fn fire_due(state: &AppState, now: i64) -> anyhow::Result<usize> {
    use crate::db::tables::ReportSchedules;
    let due: Vec<(String, i64)> = state
        .database
        .fetch_all(
            &Query::select()
                .columns([ReportSchedules::Id, ReportSchedules::NextRunAt])
                .from(ReportSchedules::Table)
                .and_where(Expr::col(ReportSchedules::Enabled).eq(true))
                .and_where(Expr::col(ReportSchedules::NextRunAt).lte(now))
                .to_owned(),
        )
        .await?;
    let mut fired = 0;
    for (id, due_at) in due {
        match fire(state, &id, due_at, now).await {
            Ok(true) => fired += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(
                schedule_id = id,
                error = format!("{error:#}"),
                "could not run a report schedule"
            ),
        }
    }
    Ok(fired)
}

/// Fires the schedule due at `due_at`, once: the transaction that queues its
/// report moves it on only if it is still due then, so a second step or a
/// save in between can't make it fire twice.
async fn fire(state: &AppState, id: &str, due_at: i64, now: i64) -> anyhow::Result<bool> {
    use crate::db::tables::ReportSchedules;
    let mut database = &state.database;
    let Some(schedule) = schedules::by_id(&mut database, id).await? else {
        return Ok(false);
    };
    let scope_label = schedule.valid.scope.label(&mut database).await?;
    let report_id = users::new_id();
    let report = schedule_report(
        &mut database,
        &schedule,
        &report_id,
        &scope_label,
        Trigger::Schedule,
        now,
    )
    .await?;
    // Missed runs (the server was down) fire once, and the schedule goes
    // on from now.
    let next = schedule.valid.when.next_after(Ord::max(now, due_at));
    let mut transaction = state.database.begin().await?;
    let claimed = transaction
        .execute(
            &Query::update()
                .table(ReportSchedules::Table)
                .value(ReportSchedules::NextRunAt, next)
                .value(ReportSchedules::LastRunAt, now)
                .value(ReportSchedules::LastReportId, report_id.as_str())
                .and_where(Expr::col(ReportSchedules::Id).eq(id))
                .and_where(Expr::col(ReportSchedules::Enabled).eq(true))
                .and_where(Expr::col(ReportSchedules::NextRunAt).eq(due_at))
                .to_owned(),
        )
        .await?;
    if claimed == 0 {
        transaction.rollback().await?;
        return Ok(false);
    }
    let actor = Actor {
        user_id: None,
        label: format!("Report schedule “{}”", schedule.valid.name),
        ip: None,
        api_token: None,
    };
    queue(&mut transaction, &report, &actor).await?;
    transaction.commit().await?;
    Ok(true)
}

/// Queues a report of the schedule now, as `actor` asked; the schedule's
/// times don't change. Returns the report's ID.
pub async fn run_now(
    state: &AppState,
    schedule: &Schedule,
    actor: &Actor,
    now: i64,
) -> anyhow::Result<String> {
    use crate::db::tables::ReportSchedules;
    let mut database = &state.database;
    let scope_label = schedule.valid.scope.label(&mut database).await?;
    let report_id = users::new_id();
    let report = schedule_report(
        &mut database,
        schedule,
        &report_id,
        &scope_label,
        Trigger::RunNow,
        now,
    )
    .await?;
    let mut transaction = state.database.begin().await?;
    queue(&mut transaction, &report, actor).await?;
    transaction
        .execute(
            &Query::update()
                .table(ReportSchedules::Table)
                .value(ReportSchedules::LastRunAt, now)
                .value(ReportSchedules::LastReportId, report_id.as_str())
                .and_where(Expr::col(ReportSchedules::Id).eq(schedule.id.as_str()))
                .to_owned(),
        )
        .await?;
    transaction.commit().await?;
    state.reports.wake();
    Ok(report_id)
}
