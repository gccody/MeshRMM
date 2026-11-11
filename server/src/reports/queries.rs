//! The reads reports are made from. Each is one statement over the report's
//! devices, grouped in the database where the rows are many (alerts, runs,
//! pending updates, packages and applications), so a report costs a few
//! dozen statements on any fleet, and none of them writes or runs in a
//! transaction: they go to the database's readers, never SQLite's writer.
use std::collections::{HashMap, HashSet};

use sea_query::{Expr, ExprTrait, Order, Query, SelectStatement};

use super::scope::Scope;
use crate::{
    applications::tables::{ApplicationRuns, DevicePackages},
    db::{
        self, Database,
        tables::{
            Agents, AlertRules, Alerts, Companies, DeviceApplications, DeviceMetrics,
            DevicePendingUpdates, DeviceSystemInfo, DeviceUpdateStatus, JobDevices, Jobs,
            UpdateRuns,
        },
    },
    time::MINUTE_MS,
};

fn count() -> Expr {
    Expr::cust("CAST(COUNT(*) AS BIGINT)")
}

/// `cast` of `sql`, so both backends return a 64-bit integer.
fn bigint(sql: &str) -> Expr {
    Expr::cust(format!("CAST({sql} AS BIGINT)"))
}

/// A device the report covers, as every template knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub company: Option<String>,
    pub agent_version: Option<String>,
    pub created_at: i64,
    pub online: bool,
    /// The start of its latest stored minute of resource readings, which
    /// are kept for [`crate::realtime::metrics::HISTORY_MS`], with that
    /// minute's storage.
    pub last_minute: Option<i64>,
    pub storage_used: Option<i64>,
    pub storage_total: Option<i64>,
    /// When the server received its latest hardware and software snapshot.
    pub snapshot_received_at: Option<i64>,
}

impl Device {
    /// When the server last heard from the device: now while its Agent is
    /// connected; otherwise the end of its latest minute of readings, its
    /// latest snapshot, or its enrollment, whichever is latest.
    pub fn last_seen(&self, now: i64) -> i64 {
        if self.online {
            return now;
        }
        [
            Some(self.created_at),
            self.last_minute
                .map(|minute| Ord::min(minute + MINUTE_MS, now)),
            self.snapshot_received_at,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(self.created_at)
    }

    pub fn platform_label(&self) -> &'static str {
        platform_label(&self.platform)
    }
}

pub fn platform_label(platform: &str) -> &'static str {
    match platform {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        _ => "Other",
    }
}

type DeviceRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// The report's devices by name, each with its latest minute of readings
/// (found through the readings' primary key, one lookup per device), its
/// company and its snapshot's time. `online` is the devices whose Agent is
/// connected.
pub async fn devices(
    database: &Database,
    scope: &Scope,
    online: &HashSet<String>,
) -> db::Result<Vec<Device>> {
    let latest = Expr::cust(
        "(SELECT MAX(latest.minute) FROM device_metrics AS latest WHERE latest.device_id = agents.id)",
    );
    let rows: Vec<DeviceRow> = database
        .fetch_all(
            &Query::select()
                .column((Agents::Table, Agents::Id))
                .column((Agents::Table, Agents::Name))
                .column((Agents::Table, Agents::Platform))
                .column((Companies::Table, Companies::Name))
                .column((Agents::Table, Agents::AgentVersion))
                .column((Agents::Table, Agents::CreatedAt))
                .column((DeviceMetrics::Table, DeviceMetrics::Minute))
                .column((DeviceMetrics::Table, DeviceMetrics::StorageUsedBytes))
                .column((DeviceMetrics::Table, DeviceMetrics::StorageTotalBytes))
                .column((DeviceSystemInfo::Table, DeviceSystemInfo::ReceivedAt))
                .from(Agents::Table)
                .left_join(
                    Companies::Table,
                    Expr::col((Companies::Table, Companies::Id))
                        .equals((Agents::Table, Agents::CompanyId)),
                )
                .left_join(
                    DeviceMetrics::Table,
                    Expr::col((DeviceMetrics::Table, DeviceMetrics::DeviceId))
                        .equals((Agents::Table, Agents::Id))
                        .and(Expr::col((DeviceMetrics::Table, DeviceMetrics::Minute)).eq(latest)),
                )
                .left_join(
                    DeviceSystemInfo::Table,
                    Expr::col((DeviceSystemInfo::Table, DeviceSystemInfo::DeviceId))
                        .equals((Agents::Table, Agents::Id)),
                )
                .and_where(scope.covers((Agents::Table, Agents::Id)))
                .order_by((Agents::Table, Agents::Name), Order::Asc)
                .order_by((Agents::Table, Agents::Id), Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                name,
                platform,
                company,
                agent_version,
                created_at,
                last_minute,
                storage_used,
                storage_total,
                snapshot_received_at,
            )| Device {
                online: online.contains(&id),
                id,
                name,
                platform,
                company,
                agent_version,
                created_at,
                last_minute,
                storage_used,
                storage_total,
                snapshot_received_at,
            },
        )
        .collect())
}

/// How many of the report's devices have each operating system and version,
/// most first.
pub async fn operating_systems(
    database: &Database,
    scope: &Scope,
) -> db::Result<Vec<(String, String, i64)>> {
    database
        .fetch_all(
            &Query::select()
                .column(DeviceSystemInfo::OsName)
                .column(DeviceSystemInfo::OsVersion)
                .expr(count())
                .from(DeviceSystemInfo::Table)
                .and_where(scope.covers(DeviceSystemInfo::DeviceId))
                .group_by_col(DeviceSystemInfo::OsName)
                .group_by_col(DeviceSystemInfo::OsVersion)
                .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
                .order_by(DeviceSystemInfo::OsName, Order::Asc)
                .order_by(DeviceSystemInfo::OsVersion, Order::Asc)
                .to_owned(),
        )
        .await
}

/// How many of the report's devices have each manufacturer and model.
pub async fn models(
    database: &Database,
    scope: &Scope,
) -> db::Result<Vec<(Option<String>, Option<String>, i64)>> {
    database
        .fetch_all(
            &Query::select()
                .column(DeviceSystemInfo::Manufacturer)
                .column(DeviceSystemInfo::Model)
                .expr(count())
                .from(DeviceSystemInfo::Table)
                .and_where(scope.covers(DeviceSystemInfo::DeviceId))
                .group_by_col(DeviceSystemInfo::Manufacturer)
                .group_by_col(DeviceSystemInfo::Model)
                .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
                .order_by(DeviceSystemInfo::Manufacturer, Order::Asc)
                .order_by(DeviceSystemInfo::Model, Order::Asc)
                .to_owned(),
        )
        .await
}

/// A device's stored snapshot columns: operating system name, version and
/// build, manufacturer, model, serial number, and the rest as JSON.
pub type SnapshotRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

/// The report's devices' latest snapshots, without their applications.
pub async fn snapshots(
    database: &Database,
    scope: &Scope,
) -> db::Result<HashMap<String, SnapshotRow>> {
    let rows: Vec<SnapshotRow> = database
        .fetch_all(
            &Query::select()
                .columns([
                    DeviceSystemInfo::DeviceId,
                    DeviceSystemInfo::OsName,
                    DeviceSystemInfo::OsVersion,
                    DeviceSystemInfo::OsBuild,
                    DeviceSystemInfo::Manufacturer,
                    DeviceSystemInfo::Model,
                    DeviceSystemInfo::SerialNumber,
                    DeviceSystemInfo::Details,
                ])
                .from(DeviceSystemInfo::Table)
                .and_where(scope.covers(DeviceSystemInfo::DeviceId))
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|row| (row.0.clone(), row)).collect())
}

/// Which alerts a count takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertsIn {
    /// Raised in the period.
    Raised,
    /// Resolved in the period, whenever they were raised.
    Resolved,
}

/// What a group of alerts adds up to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlertCounts {
    pub alerts: i64,
    /// Not resolved yet.
    pub open: i64,
    pub resolved: i64,
    /// The time from raised to resolved, added up over the resolved ones,
    /// and the longest.
    pub resolve_ms: i64,
    pub longest_ms: Option<i64>,
}

impl AlertCounts {
    pub fn mean_resolve_ms(&self) -> Option<i64> {
        (self.resolved > 0).then(|| self.resolve_ms / self.resolved)
    }

    pub fn add(&mut self, other: &Self) {
        self.alerts += other.alerts;
        self.open += other.open;
        self.resolved += other.resolved;
        self.resolve_ms += other.resolve_ms;
        self.longest_ms = Ord::max(self.longest_ms, other.longest_ms);
    }
}

/// Which column alerts are grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKey {
    /// The rule's condition: the kind of alert.
    Kind,
    Rule,
    Device,
}

/// The report's alerts in `start..end`, as `which` says, counted for each
/// kind, rule or device.
pub async fn alert_groups(
    database: &Database,
    scope: &Scope,
    (start, end): (i64, i64),
    which: AlertsIn,
    key: AlertKey,
) -> db::Result<Vec<(String, AlertCounts)>> {
    let time = match which {
        AlertsIn::Raised => Alerts::RaisedAt,
        AlertsIn::Resolved => Alerts::ResolvedAt,
    };
    let mut select = Query::select();
    match key {
        AlertKey::Kind => select.column((AlertRules::Table, AlertRules::Condition)),
        AlertKey::Rule => select.column((Alerts::Table, Alerts::RuleId)),
        AlertKey::Device => select.column((Alerts::Table, Alerts::DeviceId)),
    };
    select
        .expr(count())
        .expr(bigint(
            "SUM(CASE WHEN alerts.state <> 'resolved' THEN 1 ELSE 0 END)",
        ))
        .expr(bigint(
            "SUM(CASE WHEN alerts.state = 'resolved' THEN 1 ELSE 0 END)",
        ))
        .expr(bigint(
            "SUM(CASE WHEN alerts.state = 'resolved' THEN alerts.resolved_at - alerts.raised_at ELSE 0 END)",
        ))
        .expr(bigint(
            "MAX(CASE WHEN alerts.state = 'resolved' THEN alerts.resolved_at - alerts.raised_at END)",
        ))
        .from(Alerts::Table)
        .inner_join(
            AlertRules::Table,
            Expr::col((AlertRules::Table, AlertRules::Id)).equals((Alerts::Table, Alerts::RuleId)),
        )
        .and_where(Expr::col((Alerts::Table, time)).gte(start))
        .and_where(Expr::col((Alerts::Table, time)).lt(end))
        .and_where(scope.covers((Alerts::Table, Alerts::DeviceId)));
    match key {
        AlertKey::Kind => select.group_by_col((AlertRules::Table, AlertRules::Condition)),
        AlertKey::Rule => select.group_by_col((Alerts::Table, Alerts::RuleId)),
        AlertKey::Device => select.group_by_col((Alerts::Table, Alerts::DeviceId)),
    };
    let rows: Vec<(String, i64, i64, i64, i64, Option<i64>)> = database.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .map(|(key, alerts, open, resolved, resolve_ms, longest_ms)| {
            (
                key,
                AlertCounts {
                    alerts,
                    open,
                    resolved,
                    resolve_ms,
                    longest_ms,
                },
            )
        })
        .collect())
}

/// How many of the report's alerts are open now, whenever they were raised.
pub async fn open_alerts(database: &Database, scope: &Scope) -> db::Result<i64> {
    let (open,): (i64,) = database
        .fetch_one(
            &Query::select()
                .expr(count())
                .from(Alerts::Table)
                .and_where(Expr::col(Alerts::State).ne("resolved"))
                .and_where(scope.covers(Alerts::DeviceId))
                .to_owned(),
        )
        .await?;
    Ok(open)
}

/// An alert rule as reports name it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub severity: String,
    pub condition: String,
}

pub async fn rules(database: &Database) -> db::Result<HashMap<String, Rule>> {
    let rules: Vec<Rule> = database
        .fetch_all(
            &Query::select()
                .columns([
                    AlertRules::Id,
                    AlertRules::Name,
                    AlertRules::Severity,
                    AlertRules::Condition,
                ])
                .from(AlertRules::Table)
                .to_owned(),
        )
        .await?;
    Ok(rules
        .into_iter()
        .map(|rule| (rule.id.clone(), rule))
        .collect())
}

/// One alert, as the alerts report lists it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AlertRow {
    pub raised_at: i64,
    pub rule_id: String,
    pub device_id: String,
    pub subject: String,
    pub detail: String,
    pub state: String,
    pub resolved_at: Option<i64>,
    pub resolution: Option<String>,
}

/// The report's alerts raised in `start..end`, newest first, at most `limit`.
pub async fn alerts(
    database: &Database,
    scope: &Scope,
    (start, end): (i64, i64),
    limit: u64,
) -> db::Result<Vec<AlertRow>> {
    database
        .fetch_all(
            &Query::select()
                .columns([
                    Alerts::RaisedAt,
                    Alerts::RuleId,
                    Alerts::DeviceId,
                    Alerts::Subject,
                    Alerts::Detail,
                    Alerts::State,
                    Alerts::ResolvedAt,
                    Alerts::Resolution,
                ])
                .from(Alerts::Table)
                .and_where(Expr::col(Alerts::RaisedAt).gte(start))
                .and_where(Expr::col(Alerts::RaisedAt).lt(end))
                .and_where(scope.covers(Alerts::DeviceId))
                .order_by(Alerts::RaisedAt, Order::Desc)
                .order_by(Alerts::Id, Order::Asc)
                .limit(limit)
                .to_owned(),
        )
        .await
}

/// A device's latest update scan, as `device_update_status` keeps it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UpdateStatus {
    pub device_id: String,
    pub manager: Option<String>,
    pub supported: bool,
    pub scanned_at: Option<i64>,
    pub scan_error: Option<String>,
    pub reboot_pending: bool,
    pub reboot_pending_since: Option<i64>,
    pub pending_count: i64,
    pub security_count: i64,
    pub critical_count: i64,
    pub last_install_status: Option<String>,
}

pub async fn update_statuses(
    database: &Database,
    scope: &Scope,
) -> db::Result<HashMap<String, UpdateStatus>> {
    let rows: Vec<UpdateStatus> = database
        .fetch_all(
            &Query::select()
                .columns([
                    DeviceUpdateStatus::DeviceId,
                    DeviceUpdateStatus::Manager,
                    DeviceUpdateStatus::Supported,
                    DeviceUpdateStatus::ScannedAt,
                    DeviceUpdateStatus::ScanError,
                    DeviceUpdateStatus::RebootPending,
                    DeviceUpdateStatus::RebootPendingSince,
                    DeviceUpdateStatus::PendingCount,
                    DeviceUpdateStatus::SecurityCount,
                    DeviceUpdateStatus::CriticalCount,
                    DeviceUpdateStatus::LastInstallStatus,
                ])
                .from(DeviceUpdateStatus::Table)
                .and_where(scope.covers(DeviceUpdateStatus::DeviceId))
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|status| (status.device_id.clone(), status))
        .collect())
}

/// How many updates are pending on each device in each category and of
/// each severity (`None` for unrated).
pub async fn pending_updates(
    database: &Database,
    scope: &Scope,
) -> db::Result<Vec<(String, String, Option<String>, i64)>> {
    database
        .fetch_all(
            &Query::select()
                .column(DevicePendingUpdates::DeviceId)
                .column(DevicePendingUpdates::Category)
                .column(DevicePendingUpdates::Severity)
                .expr(count())
                .from(DevicePendingUpdates::Table)
                .and_where(scope.covers(DevicePendingUpdates::DeviceId))
                .group_by_col(DevicePendingUpdates::DeviceId)
                .group_by_col(DevicePendingUpdates::Category)
                .group_by_col(DevicePendingUpdates::Severity)
                .to_owned(),
        )
        .await
}

/// Installs that failed outright or in part on each device, started in
/// `start..end`: how many, and how many updates they failed to install.
pub async fn failed_installs(
    database: &Database,
    scope: &Scope,
    (start, end): (i64, i64),
) -> db::Result<HashMap<String, (i64, i64)>> {
    let rows: Vec<(String, i64, i64)> = database
        .fetch_all(
            &Query::select()
                .column(UpdateRuns::DeviceId)
                .expr(count())
                .expr(bigint("SUM(update_runs.failed)"))
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::Kind).eq("install"))
                .and_where(Expr::col(UpdateRuns::Status).is_in(["failed", "partial"]))
                .and_where(Expr::col(UpdateRuns::CreatedAt).gte(start))
                .and_where(Expr::col(UpdateRuns::CreatedAt).lt(end))
                .and_where(scope.covers(UpdateRuns::DeviceId))
                .group_by_col(UpdateRuns::DeviceId)
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(device, runs, updates)| (device, (runs, updates)))
        .collect())
}

/// A package outdated on some of the report's devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outdated {
    pub source: String,
    pub package_id: String,
    pub name: String,
    pub available_version: Option<String>,
    pub devices: i64,
}

/// The packages outdated on the report's devices, on the most devices
/// first, at most `limit`.
pub async fn outdated_packages(
    database: &Database,
    scope: &Scope,
    limit: Option<u64>,
) -> db::Result<Vec<Outdated>> {
    let mut select = Query::select();
    select
        .column(DevicePackages::Source)
        .expr(Expr::cust("MIN(device_packages.package_id)"))
        .expr(Expr::cust("MIN(device_packages.name)"))
        .expr(Expr::cust("MAX(device_packages.available_version)"))
        .expr(count())
        .from(DevicePackages::Table)
        .and_where(Expr::col(DevicePackages::OutdatedSince).is_not_null())
        .and_where(scope.covers(DevicePackages::DeviceId))
        .group_by_col(DevicePackages::Source)
        .group_by_col(DevicePackages::PackageKey)
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .order_by_expr(Expr::cust("MIN(device_packages.name)"), Order::Asc)
        .order_by(DevicePackages::Source, Order::Asc);
    if let Some(limit) = limit {
        select.limit(limit);
    }
    let rows: Vec<(String, String, String, Option<String>, i64)> =
        database.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .map(
            |(source, package_id, name, available_version, devices)| Outdated {
                source,
                package_id,
                name,
                available_version,
                devices,
            },
        )
        .collect())
}

/// How many outdated packages the report's devices have in all, and on how
/// many devices.
pub async fn outdated_totals(database: &Database, scope: &Scope) -> db::Result<(i64, i64)> {
    database
        .fetch_one(
            &Query::select()
                .expr(count())
                .expr(bigint("COUNT(DISTINCT device_packages.device_id)"))
                .from(DevicePackages::Table)
                .and_where(Expr::col(DevicePackages::OutdatedSince).is_not_null())
                .and_where(scope.covers(DevicePackages::DeviceId))
                .to_owned(),
        )
        .await
}

/// Application installs and updates that failed outright or in part,
/// started in `start..end`: for each action, how many and on how many
/// devices.
pub async fn failed_application_runs(
    database: &Database,
    scope: &Scope,
    (start, end): (i64, i64),
) -> db::Result<HashMap<String, (i64, i64)>> {
    let rows: Vec<(String, i64, i64)> = database
        .fetch_all(
            &Query::select()
                .column(ApplicationRuns::Action)
                .expr(count())
                .expr(bigint("COUNT(DISTINCT application_runs.device_id)"))
                .from(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::Action).is_in(["install", "update"]))
                .and_where(Expr::col(ApplicationRuns::Status).is_in(["failed", "partial"]))
                .and_where(Expr::col(ApplicationRuns::CreatedAt).gte(start))
                .and_where(Expr::col(ApplicationRuns::CreatedAt).lt(end))
                .and_where(scope.covers(ApplicationRuns::DeviceId))
                .group_by_col(ApplicationRuns::Action)
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(action, runs, devices)| (action, (runs, devices)))
        .collect())
}

/// What one policy's jobs, or the jobs users started (`policy_id` `None`),
/// did on the report's devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobCounts {
    pub policy_id: Option<String>,
    pub policy_name: Option<String>,
    pub jobs: i64,
    /// Jobs that failed, timed out or got no result on any of the devices.
    pub failed_jobs: i64,
    pub succeeded: i64,
    pub failed: i64,
    /// Skipped, missed or cancelled devices.
    pub not_run: i64,
}

const FAILED_RESULTS: &str = "('failed', 'timed_out', 'no_result')";

/// A policy (or none, for users' own jobs) and its name, then its jobs, jobs
/// with failures, and devices succeeded, failed and not run.
type JobRow = (Option<String>, Option<String>, i64, i64, i64, i64, i64);

/// The jobs created in `start..end` that covered any of the report's
/// devices, counted for each policy and for users' own.
pub async fn jobs(
    database: &Database,
    scope: &Scope,
    (start, end): (i64, i64),
) -> db::Result<Vec<JobCounts>> {
    let rows: Vec<JobRow> = database
        .fetch_all(
            &Query::select()
                .column((Jobs::Table, Jobs::PolicyId))
                .expr(Expr::cust("MAX(jobs.policy_name)"))
                .expr(bigint("COUNT(DISTINCT jobs.id)"))
                .expr(bigint(&format!(
                    "COUNT(DISTINCT CASE WHEN job_devices.result IN {FAILED_RESULTS} THEN jobs.id END)"
                )))
                .expr(bigint(
                    "SUM(CASE WHEN job_devices.result = 'succeeded' THEN 1 ELSE 0 END)",
                ))
                .expr(bigint(&format!(
                    "SUM(CASE WHEN job_devices.result IN {FAILED_RESULTS} THEN 1 ELSE 0 END)"
                )))
                .expr(bigint(
                    "SUM(CASE WHEN job_devices.state IN ('skipped', 'missed', 'cancelled') OR job_devices.result = 'skipped' THEN 1 ELSE 0 END)",
                ))
                .from(JobDevices::Table)
                .inner_join(
                    Jobs::Table,
                    Expr::col((Jobs::Table, Jobs::Id)).equals((JobDevices::Table, JobDevices::JobId)),
                )
                .and_where(Expr::col((Jobs::Table, Jobs::CreatedAt)).gte(start))
                .and_where(Expr::col((Jobs::Table, Jobs::CreatedAt)).lt(end))
                .and_where(Expr::col((Jobs::Table, Jobs::Creating)).eq(false))
                .and_where(scope.covers((JobDevices::Table, JobDevices::DeviceId)))
                .group_by_col((Jobs::Table, Jobs::PolicyId))
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(policy_id, policy_name, jobs, failed_jobs, succeeded, failed, not_run)| JobCounts {
                policy_id,
                policy_name,
                jobs,
                failed_jobs,
                succeeded,
                failed,
                not_run,
            },
        )
        .collect())
}

/// An application on the report's devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Software {
    pub name: String,
    /// Its distinct versions.
    pub versions: i64,
    pub devices: i64,
}

fn applications_select(scope: &Scope) -> SelectStatement {
    Query::select()
        .from(DeviceApplications::Table)
        .and_where(scope.covers(DeviceApplications::DeviceId))
        .to_owned()
}

/// The applications on the report's devices, on the most devices first.
pub async fn applications(database: &Database, scope: &Scope) -> db::Result<Vec<Software>> {
    let rows: Vec<(String, i64, i64)> = database
        .fetch_all(
            &applications_select(scope)
                .column(DeviceApplications::Name)
                .expr(bigint(
                    "COUNT(DISTINCT COALESCE(device_applications.version, ''))",
                ))
                .expr(bigint("COUNT(DISTINCT device_applications.device_id)"))
                .group_by_col(DeviceApplications::Name)
                .order_by_expr(
                    Expr::cust("COUNT(DISTINCT device_applications.device_id)"),
                    Order::Desc,
                )
                .order_by(DeviceApplications::Name, Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(name, versions, devices)| Software {
            name,
            versions,
            devices,
        })
        .collect())
}

/// Each version of each application from each publisher on the report's
/// devices, with how many devices have it, by name and most devices.
pub async fn application_versions(
    database: &Database,
    scope: &Scope,
) -> db::Result<Vec<(String, String, String, i64)>> {
    database
        .fetch_all(
            &applications_select(scope)
                .column(DeviceApplications::Name)
                .expr(Expr::cust("COALESCE(device_applications.version, '')"))
                .expr(Expr::cust("COALESCE(device_applications.publisher, '')"))
                .expr(bigint("COUNT(DISTINCT device_applications.device_id)"))
                .group_by_col(DeviceApplications::Name)
                .add_group_by([
                    Expr::cust("COALESCE(device_applications.version, '')"),
                    Expr::cust("COALESCE(device_applications.publisher, '')"),
                ])
                .order_by(DeviceApplications::Name, Order::Asc)
                .order_by_expr(
                    Expr::cust("COUNT(DISTINCT device_applications.device_id)"),
                    Order::Desc,
                )
                .order_by_expr(
                    Expr::cust("COALESCE(device_applications.version, '')"),
                    Order::Asc,
                )
                .to_owned(),
        )
        .await
}

/// How many of the report's devices list any applications.
pub async fn devices_with_applications(database: &Database, scope: &Scope) -> db::Result<i64> {
    let (devices,): (i64,) = database
        .fetch_one(
            &applications_select(scope)
                .expr(bigint("COUNT(DISTINCT device_applications.device_id)"))
                .to_owned(),
        )
        .await?;
    Ok(devices)
}
