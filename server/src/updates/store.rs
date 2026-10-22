//! Each device's update status and pending updates, from its Agent's scans,
//! and the summaries the website lists devices by.
use std::collections::{HashMap, HashSet};

use meshrmm_protocol_types::{
    DevicePlatform, PendingUpdate, UpdateCategory, UpdateManager, UpdateScan,
};
use sea_query::{Expr, ExprTrait, LockType, OnConflict, Order, Query};
use serde::Serialize;

use crate::{
    agents,
    db::{
        self, Database, Executor,
        tables::{Agents, DevicePendingUpdates, DeviceUpdateStatus, UpdateRuns},
    },
};

/// Rows per insert, well within both databases' bind parameter limits.
const INSERT_CHUNK: usize = 300;

/// Text as PostgreSQL can store it: without NUL. Scans are sanitized, which
/// removes control characters, so this is a safeguard.
fn storable(text: &str) -> String {
    text.replace('\0', "")
}

fn storable_option(text: &Option<String>) -> Option<String> {
    text.as_deref().map(storable)
}

/// A device's update status, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct Status {
    pub manager: Option<String>,
    pub supported: bool,
    pub scanned_at: Option<i64>,
    pub received_at: Option<i64>,
    pub scan_error: Option<String>,
    pub reboot_pending: bool,
    pub reboot_pending_since: Option<i64>,
    pub scan_requested_at: Option<i64>,
}

impl Status {
    pub fn manager(&self) -> Option<UpdateManager> {
        self.manager.as_deref().and_then(UpdateManager::parse)
    }

    /// Whether the website may offer to install chosen updates, rather than
    /// every pending one.
    pub fn can_select(&self) -> bool {
        self.manager().is_none_or(UpdateManager::installs_selected)
    }
}

fn status_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            DeviceUpdateStatus::Manager,
            DeviceUpdateStatus::Supported,
            DeviceUpdateStatus::ScannedAt,
            DeviceUpdateStatus::ReceivedAt,
            DeviceUpdateStatus::ScanError,
            DeviceUpdateStatus::RebootPending,
            DeviceUpdateStatus::RebootPendingSince,
            DeviceUpdateStatus::ScanRequestedAt,
        ])
        .from(DeviceUpdateStatus::Table)
        .to_owned()
}

/// The device's status; a device that never reported a scan has the default.
pub async fn status(executor: &mut impl Executor, device_id: &str) -> db::Result<Status> {
    Ok(executor
        .fetch_optional(
            &status_select()
                .and_where(Expr::col(DeviceUpdateStatus::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?
        .unwrap_or(Status {
            supported: true,
            ..Status::default()
        }))
}

/// Writes the status columns in `values` for the device, adding its row if
/// it has none.
async fn upsert_status(
    executor: &mut impl Executor,
    device_id: &str,
    values: Vec<(DeviceUpdateStatus, sea_query::Value)>,
) -> db::Result<()> {
    let (mut columns, mut row): (Vec<_>, Vec<_>) = values
        .into_iter()
        .map(|(column, value)| (column, Expr::val(value)))
        .unzip();
    let updated = columns
        .iter()
        .map(|column| sea_query::Alias::new(sea_query::Iden::unquoted(column)))
        .collect::<Vec<_>>();
    columns.push(DeviceUpdateStatus::DeviceId);
    row.push(Expr::val(device_id));
    executor
        .execute(
            &Query::insert()
                .into_table(DeviceUpdateStatus::Table)
                .columns(columns)
                .values_panic(row)
                .on_conflict(
                    OnConflict::column(DeviceUpdateStatus::DeviceId)
                        .update_columns(updated)
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Notes that the server asked the device's Agent for a scan at `now`.
pub async fn scan_requested(
    executor: &mut impl Executor,
    device_id: &str,
    now: i64,
) -> db::Result<()> {
    upsert_status(
        executor,
        device_id,
        vec![(DeviceUpdateStatus::ScanRequestedAt, now.into())],
    )
    .await
}

/// Stores a scan the device's Agent reported, received at `now`. A failed
/// scan keeps the last pending list. Returns `false`, storing nothing, if the
/// device is no longer enrolled.
pub async fn store_scan(
    database: &Database,
    device_id: &str,
    scan: UpdateScan,
    now: i64,
) -> db::Result<bool> {
    let scan = scan.sanitized();
    let mut transaction = database.begin().await?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Ok(false);
    }
    // Another report for the device waits until this one commits. SQLite's
    // write transactions are serialized anyway.
    let previous: Option<Status> = transaction
        .fetch_optional(
            &status_select()
                .and_where(Expr::col(DeviceUpdateStatus::DeviceId).eq(device_id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let manager = scan.manager.map(UpdateManager::as_str);
    if let Some(error) = &scan.error {
        let mut values = vec![
            (DeviceUpdateStatus::ReceivedAt, now.into()),
            (DeviceUpdateStatus::ScanError, storable(error).into()),
        ];
        if manager.is_some() {
            values.push((DeviceUpdateStatus::Manager, manager.into()));
            values.push((DeviceUpdateStatus::Supported, true.into()));
        }
        upsert_status(&mut transaction, device_id, values).await?;
        transaction.commit().await?;
        return Ok(true);
    }
    let since = if scan.reboot_pending {
        Some(
            previous
                .as_ref()
                .filter(|previous| previous.reboot_pending)
                .and_then(|previous| previous.reboot_pending_since)
                .unwrap_or(now),
        )
    } else {
        None
    };
    upsert_status(
        &mut transaction,
        device_id,
        vec![
            (DeviceUpdateStatus::Manager, manager.into()),
            (DeviceUpdateStatus::Supported, scan.manager.is_some().into()),
            (
                DeviceUpdateStatus::ScannedAt,
                scan.scanned_at_unix_ms.into(),
            ),
            (DeviceUpdateStatus::ReceivedAt, now.into()),
            (DeviceUpdateStatus::ScanError, Option::<String>::None.into()),
            (
                DeviceUpdateStatus::RebootPending,
                scan.reboot_pending.into(),
            ),
            (DeviceUpdateStatus::RebootPendingSince, since.into()),
        ],
    )
    .await?;
    replace_pending(&mut transaction, device_id, &scan.updates, now).await?;
    transaction.commit().await?;
    Ok(true)
}

/// Replaces the device's pending updates with `updates`, keeping when each
/// one was first seen.
async fn replace_pending(
    executor: &mut impl Executor,
    device_id: &str,
    updates: &[PendingUpdate],
    now: i64,
) -> db::Result<()> {
    let seen: Vec<(String, i64)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    DevicePendingUpdates::UpdateId,
                    DevicePendingUpdates::FirstSeenAt,
                ])
                .from(DevicePendingUpdates::Table)
                .and_where(Expr::col(DevicePendingUpdates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    let seen = seen.into_iter().collect::<HashMap<_, _>>();
    executor
        .execute(
            &Query::delete()
                .from_table(DevicePendingUpdates::Table)
                .and_where(Expr::col(DevicePendingUpdates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    for chunk in updates.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(DevicePendingUpdates::Table).columns([
            DevicePendingUpdates::DeviceId,
            DevicePendingUpdates::UpdateId,
            DevicePendingUpdates::Title,
            DevicePendingUpdates::Kb,
            DevicePendingUpdates::Version,
            DevicePendingUpdates::CurrentVersion,
            DevicePendingUpdates::Category,
            DevicePendingUpdates::Severity,
            DevicePendingUpdates::SizeBytes,
            DevicePendingUpdates::RebootRequired,
            DevicePendingUpdates::ReleasedAt,
            DevicePendingUpdates::Source,
            DevicePendingUpdates::FirstSeenAt,
        ]);
        for update in chunk {
            let id = storable(&update.id);
            let first_seen = seen.get(&id).copied().unwrap_or(now);
            insert.values_panic([
                device_id.into(),
                id.into(),
                storable(&update.title).into(),
                storable_option(&update.kb).into(),
                storable_option(&update.version).into(),
                storable_option(&update.current_version).into(),
                update.category.as_str().into(),
                update.severity.map(|severity| severity.as_str()).into(),
                update
                    .size_bytes
                    .and_then(|size| i64::try_from(size).ok())
                    .into(),
                update.reboot_required.into(),
                update.released_at_unix_ms.into(),
                storable_option(&update.source).into(),
                first_seen.into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

/// A pending update as the API returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Pending {
    pub id: String,
    pub title: String,
    pub kb: Option<String>,
    pub version: Option<String>,
    pub current_version: Option<String>,
    pub category: String,
    pub severity: Option<String>,
    pub size_bytes: Option<i64>,
    pub reboot_required: bool,
    pub released_at: Option<i64>,
    pub source: Option<String>,
    /// When a scan first listed it.
    pub first_seen_at: i64,
}

/// The device's pending updates: by category, then title.
pub async fn pending(executor: &mut impl Executor, device_id: &str) -> db::Result<Vec<Pending>> {
    let mut rows: Vec<Pending> = executor
        .fetch_all(
            &Query::select()
                .expr_as(Expr::col(DevicePendingUpdates::UpdateId), "id")
                .columns([
                    DevicePendingUpdates::Title,
                    DevicePendingUpdates::Kb,
                    DevicePendingUpdates::Version,
                    DevicePendingUpdates::CurrentVersion,
                    DevicePendingUpdates::Category,
                    DevicePendingUpdates::Severity,
                    DevicePendingUpdates::SizeBytes,
                    DevicePendingUpdates::RebootRequired,
                    DevicePendingUpdates::ReleasedAt,
                    DevicePendingUpdates::Source,
                    DevicePendingUpdates::FirstSeenAt,
                ])
                .from(DevicePendingUpdates::Table)
                .and_where(Expr::col(DevicePendingUpdates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    rows.sort_by_cached_key(|row| {
        (
            UpdateCategory::parse(&row.category),
            row.title.to_lowercase(),
            row.id.clone(),
        )
    });
    Ok(rows)
}

/// For alert rules: each device's pending updates' categories and when they
/// were first seen.
pub async fn pending_ages(
    executor: &mut impl Executor,
) -> db::Result<HashMap<String, Vec<(UpdateCategory, i64)>>> {
    let rows: Vec<(String, String, i64)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    DevicePendingUpdates::DeviceId,
                    DevicePendingUpdates::Category,
                    DevicePendingUpdates::FirstSeenAt,
                ])
                .from(DevicePendingUpdates::Table)
                .to_owned(),
        )
        .await?;
    let mut ages: HashMap<String, Vec<(UpdateCategory, i64)>> = HashMap::new();
    for (device_id, category, first_seen) in rows {
        if let Some(category) = UpdateCategory::parse(&category) {
            ages.entry(device_id)
                .or_default()
                .push((category, first_seen));
        }
    }
    Ok(ages)
}

/// For alert rules: each device whose last scan says it must restart, and
/// since when.
pub async fn reboots_pending(executor: &mut impl Executor) -> db::Result<HashMap<String, i64>> {
    let rows: Vec<(String, Option<i64>)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    DeviceUpdateStatus::DeviceId,
                    DeviceUpdateStatus::RebootPendingSince,
                ])
                .from(DeviceUpdateStatus::Table)
                .and_where(Expr::col(DeviceUpdateStatus::RebootPending).eq(true))
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(device_id, since)| Some((device_id, since?)))
        .collect())
}

/// Each device that reported scans, with whether it must restart: for
/// knowing which devices a rule can judge.
pub async fn scanned_devices(executor: &mut impl Executor) -> db::Result<HashSet<String>> {
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(DeviceUpdateStatus::DeviceId)
                .from(DeviceUpdateStatus::Table)
                .and_where(Expr::col(DeviceUpdateStatus::ScannedAt).is_not_null())
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The latest install of a device, as the fleet list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LastInstall {
    pub id: String,
    pub status: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub installed: i64,
    pub failed: i64,
}

/// A device's updates at a glance, as the fleet list and live events show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceSummary {
    pub device_id: String,
    pub device_name: String,
    pub company_id: Option<String>,
    pub platform: Option<DevicePlatform>,
    pub connected: bool,
    pub manager: Option<String>,
    pub supported: bool,
    pub can_select: bool,
    pub scanned_at: Option<i64>,
    pub received_at: Option<i64>,
    pub scan_error: Option<String>,
    pub reboot_pending: bool,
    pub reboot_pending_since: Option<i64>,
    pub pending: i64,
    pub security: i64,
    pub critical: i64,
    pub oldest_pending_at: Option<i64>,
    pub scanning: bool,
    pub installing: bool,
    pub last_install: Option<LastInstall>,
}

#[derive(sqlx::FromRow)]
struct SummaryRow {
    id: String,
    name: String,
    company_id: Option<String>,
    platform: String,
    manager: Option<String>,
    supported: Option<bool>,
    scanned_at: Option<i64>,
    received_at: Option<i64>,
    scan_error: Option<String>,
    reboot_pending: Option<bool>,
    reboot_pending_since: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct CountRow {
    device_id: String,
    pending: i64,
    security: i64,
    critical: i64,
    oldest: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct LastInstallRow {
    device_id: String,
    id: String,
    status: String,
    created_at: i64,
    completed_at: Option<i64>,
    installed: i64,
    failed: i64,
}

/// Every enrolled device's summary, by name, or only `device_id`'s.
pub async fn summaries(
    executor: &mut impl Executor,
    device_id: Option<&str>,
    connected: &HashSet<String>,
) -> db::Result<Vec<DeviceSummary>> {
    let mut devices = Query::select();
    devices
        .columns([
            (Agents::Table, Agents::Id),
            (Agents::Table, Agents::Name),
            (Agents::Table, Agents::CompanyId),
            (Agents::Table, Agents::Platform),
        ])
        .columns([
            (DeviceUpdateStatus::Table, DeviceUpdateStatus::Manager),
            (DeviceUpdateStatus::Table, DeviceUpdateStatus::Supported),
            (DeviceUpdateStatus::Table, DeviceUpdateStatus::ScannedAt),
            (DeviceUpdateStatus::Table, DeviceUpdateStatus::ReceivedAt),
            (DeviceUpdateStatus::Table, DeviceUpdateStatus::ScanError),
            (DeviceUpdateStatus::Table, DeviceUpdateStatus::RebootPending),
            (
                DeviceUpdateStatus::Table,
                DeviceUpdateStatus::RebootPendingSince,
            ),
        ])
        .from(Agents::Table)
        .left_join(
            DeviceUpdateStatus::Table,
            Expr::col((DeviceUpdateStatus::Table, DeviceUpdateStatus::DeviceId))
                .equals((Agents::Table, Agents::Id)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .order_by((Agents::Table, Agents::Name), Order::Asc)
        .order_by((Agents::Table, Agents::Id), Order::Asc);
    let mut counts = Query::select();
    counts
        .column(DevicePendingUpdates::DeviceId)
        .expr_as(Expr::cust("CAST(COUNT(*) AS BIGINT)"), "pending")
        .expr_as(
            Expr::cust("CAST(SUM(CASE WHEN category = 'security' THEN 1 ELSE 0 END) AS BIGINT)"),
            "security",
        )
        .expr_as(
            Expr::cust("CAST(SUM(CASE WHEN category = 'critical' THEN 1 ELSE 0 END) AS BIGINT)"),
            "critical",
        )
        .expr_as(Expr::cust("MIN(first_seen_at)"), "oldest")
        .from(DevicePendingUpdates::Table)
        .group_by_col(DevicePendingUpdates::DeviceId);
    let mut active = Query::select();
    active
        .columns([UpdateRuns::DeviceId, UpdateRuns::Kind])
        .from(UpdateRuns::Table)
        .and_where(Expr::col(UpdateRuns::Status).eq("pending"));
    let mut last = Query::select();
    last.columns([
        UpdateRuns::DeviceId,
        UpdateRuns::Id,
        UpdateRuns::Status,
        UpdateRuns::CreatedAt,
        UpdateRuns::CompletedAt,
        UpdateRuns::Installed,
        UpdateRuns::Failed,
    ])
    .from(UpdateRuns::Table)
    .and_where(Expr::col(UpdateRuns::Kind).eq("install"))
    .and_where(Expr::cust(
        "update_runs.created_at = (SELECT MAX(latest.created_at) FROM update_runs AS latest \
         WHERE latest.device_id = update_runs.device_id AND latest.kind = 'install')",
    ));
    if let Some(device_id) = device_id {
        devices.and_where(Expr::col((Agents::Table, Agents::Id)).eq(device_id));
        counts.and_where(Expr::col(DevicePendingUpdates::DeviceId).eq(device_id));
        active.and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id));
        last.and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id));
    }
    let rows: Vec<SummaryRow> = executor.fetch_all(&devices).await?;
    let counts: Vec<CountRow> = executor.fetch_all(&counts).await?;
    let counts = counts
        .into_iter()
        .map(|row| (row.device_id.clone(), row))
        .collect::<HashMap<_, _>>();
    let active: Vec<(String, String)> = executor.fetch_all(&active).await?;
    let last: Vec<LastInstallRow> = executor.fetch_all(&last).await?;
    let mut last = last
        .into_iter()
        .map(|row| {
            (
                row.device_id,
                LastInstall {
                    id: row.id,
                    status: row.status,
                    created_at: row.created_at,
                    completed_at: row.completed_at,
                    installed: row.installed,
                    failed: row.failed,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    Ok(rows
        .into_iter()
        .map(|row| {
            let count = counts.get(&row.id);
            let status = Status {
                manager: row.manager.clone(),
                ..Status::default()
            };
            DeviceSummary {
                connected: connected.contains(&row.id),
                platform: DevicePlatform::parse(&row.platform),
                can_select: status.can_select(),
                manager: row.manager,
                supported: row.supported.unwrap_or(true),
                scanned_at: row.scanned_at,
                received_at: row.received_at,
                scan_error: row.scan_error,
                reboot_pending: row.reboot_pending.unwrap_or(false),
                reboot_pending_since: row.reboot_pending_since,
                pending: count.map_or(0, |count| count.pending),
                security: count.map_or(0, |count| count.security),
                critical: count.map_or(0, |count| count.critical),
                oldest_pending_at: count.and_then(|count| count.oldest),
                scanning: active
                    .iter()
                    .any(|(device, kind)| *device == row.id && kind == "scan"),
                installing: active
                    .iter()
                    .any(|(device, kind)| *device == row.id && kind == "install"),
                last_install: last.remove(&row.id),
                device_id: row.id,
                device_name: row.name,
                company_id: row.company_id,
            }
        })
        .collect())
}

/// Removes a deleted device's status and pending updates. Its runs stay in
/// the history until they are purged.
pub async fn forget(database: &Database, device_id: &str) -> db::Result<()> {
    let mut transaction = database.begin().await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceUpdateStatus::Table)
                .and_where(Expr::col(DeviceUpdateStatus::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DevicePendingUpdates::Table)
                .and_where(Expr::col(DevicePendingUpdates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction.commit().await
}
