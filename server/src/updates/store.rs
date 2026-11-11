//! Each device's update status and pending updates, from its Agent's scans,
//! and the summaries and pages of the fleet the website lists devices by.
use std::collections::{HashMap, HashSet};

use meshrmm_protocol_types::{
    DevicePlatform, PendingUpdate, UpdateCategory, UpdateManager, UpdateScan,
};
use sea_query::{
    Alias, BinOper, Cond, Condition, Expr, ExprTrait, Func, LockType, OnConflict, Order, Query,
    SelectStatement,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    access::CompanySet,
    agents,
    db::{
        self, Database, Executor,
        tables::{Agents, DevicePendingUpdates, DeviceTags, DeviceUpdateStatus, UpdateRuns},
    },
    secrets::hex,
    time::{DAY_MS, now_ms},
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

/// What storing a scan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// The device is no longer enrolled: nothing was stored.
    NotEnrolled,
    /// The scan failed: its error was stored, and the pending list kept.
    Failed,
    /// The scan's pending updates replaced the device's.
    Replaced,
    /// The scan listed the pending updates already stored, so only the
    /// device's status was written; `reboot_changed` says whether its
    /// restart pending changed.
    Unchanged { reboot_changed: bool },
}

/// Identifies a list of pending updates in any order, so an unchanged one
/// isn't rewritten. A sanitized scan has one update per ID.
fn pending_hash(updates: &[PendingUpdate]) -> String {
    let mut updates = updates.iter().collect::<Vec<_>>();
    updates.sort_by(|a, b| a.id.cmp(&b.id));
    let json = serde_json::to_vec(&updates).expect("updates serialize");
    hex(&Sha256::digest(json))
}

/// Stores a scan the device's Agent reported, received at `now`. A failed
/// scan keeps the last pending list, and so does one that lists the same
/// updates: only its status is written then.
pub async fn store_scan(
    database: &Database,
    device_id: &str,
    scan: UpdateScan,
    now: i64,
) -> db::Result<Stored> {
    let scan = scan.sanitized();
    let mut transaction = database.begin().await?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Ok(Stored::NotEnrolled);
    }
    // Another report for the device waits until this one commits, so it
    // compares with this one's list. SQLite's write transactions are
    // serialized anyway.
    let previous: Option<(bool, Option<i64>, Option<String>)> = transaction
        .fetch_optional(
            &Query::select()
                .columns([
                    DeviceUpdateStatus::RebootPending,
                    DeviceUpdateStatus::RebootPendingSince,
                    DeviceUpdateStatus::PendingHash,
                ])
                .from(DeviceUpdateStatus::Table)
                .and_where(Expr::col(DeviceUpdateStatus::DeviceId).eq(device_id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let (was_pending, was_since, stored_hash) = previous.unwrap_or_default();
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
        return Ok(Stored::Failed);
    }
    let since = scan
        .reboot_pending
        .then(|| was_since.filter(|_| was_pending).unwrap_or(now));
    let hash = pending_hash(&scan.updates);
    let unchanged = stored_hash.as_deref() == Some(hash.as_str());
    let mut values = vec![
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
        // In the transaction that writes the list, so it never identifies
        // one that wasn't.
        (DeviceUpdateStatus::PendingHash, hash.into()),
    ];
    if !unchanged {
        // An unchanged list keeps its counts, as it keeps its rows.
        let counts = replace_pending(&mut transaction, device_id, &scan.updates, now).await?;
        values.extend([
            (DeviceUpdateStatus::PendingCount, counts.pending.into()),
            (DeviceUpdateStatus::SecurityCount, counts.security.into()),
            (DeviceUpdateStatus::CriticalCount, counts.critical.into()),
            (DeviceUpdateStatus::OldestPendingAt, counts.oldest.into()),
        ]);
    }
    upsert_status(&mut transaction, device_id, values).await?;
    transaction.commit().await?;
    Ok(if unchanged {
        Stored::Unchanged {
            reboot_changed: (scan.reboot_pending, since) != (was_pending, was_since),
        }
    } else {
        Stored::Replaced
    })
}

/// What a device's pending list holds, as its status row keeps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PendingCounts {
    pending: i64,
    security: i64,
    critical: i64,
    /// When a scan first listed the oldest.
    oldest: Option<i64>,
}

impl PendingCounts {
    fn add(&mut self, category: UpdateCategory, first_seen: i64) {
        self.pending += 1;
        match category {
            UpdateCategory::Security => self.security += 1,
            UpdateCategory::Critical => self.critical += 1,
            _ => {}
        }
        self.oldest = Some(
            self.oldest
                .map_or(first_seen, |oldest| std::cmp::min(oldest, first_seen)),
        );
    }
}

/// Replaces the device's pending updates with `updates`, keeping when each
/// one was first seen. Returns what the new list holds, for the status row.
async fn replace_pending(
    executor: &mut impl Executor,
    device_id: &str,
    updates: &[PendingUpdate],
    now: i64,
) -> db::Result<PendingCounts> {
    let mut counts = PendingCounts::default();
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
            counts.add(update.category, first_seen);
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
    Ok(counts)
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
/// were first seen; of the `only` devices, or of every device.
pub async fn pending_ages(
    executor: &mut impl Executor,
    only: Option<&[String]>,
) -> db::Result<HashMap<String, Vec<(UpdateCategory, i64)>>> {
    let mut select = Query::select();
    select
        .columns([
            DevicePendingUpdates::DeviceId,
            DevicePendingUpdates::Category,
            DevicePendingUpdates::FirstSeenAt,
        ])
        .from(DevicePendingUpdates::Table);
    if let Some(ids) = only {
        select.and_where(Expr::col(DevicePendingUpdates::DeviceId).is_in(ids.iter().cloned()));
    }
    let rows: Vec<(String, String, i64)> = executor.fetch_all(&select).await?;
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
/// since when; of the `only` devices, or of every device.
pub async fn reboots_pending(
    executor: &mut impl Executor,
    only: Option<&[String]>,
) -> db::Result<HashMap<String, i64>> {
    let mut select = Query::select();
    select
        .columns([
            DeviceUpdateStatus::DeviceId,
            DeviceUpdateStatus::RebootPendingSince,
        ])
        .from(DeviceUpdateStatus::Table)
        .and_where(Expr::col(DeviceUpdateStatus::RebootPending).eq(true));
    if let Some(ids) = only {
        select.and_where(Expr::col(DeviceUpdateStatus::DeviceId).is_in(ids.iter().cloned()));
    }
    let rows: Vec<(String, Option<i64>)> = executor.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(device_id, since)| Some((device_id, since?)))
        .collect())
}

/// Each device that reported scans, of the `only` devices or of every
/// device: for knowing which devices a rule can judge.
pub async fn scanned_devices(
    executor: &mut impl Executor,
    only: Option<&[String]>,
) -> db::Result<HashSet<String>> {
    let mut select = Query::select();
    select
        .column(DeviceUpdateStatus::DeviceId)
        .from(DeviceUpdateStatus::Table)
        .and_where(Expr::col(DeviceUpdateStatus::ScannedAt).is_not_null());
    if let Some(ids) = only {
        select.and_where(Expr::col(DeviceUpdateStatus::DeviceId).is_in(ids.iter().cloned()));
    }
    let rows: Vec<(String,)> = executor.fetch_all(&select).await?;
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

/// How long a device that has an update manager may go without reporting a
/// scan before the Updates page shows it as not scanned recently.
pub const STALE_SCAN_MS: i64 = 3 * DAY_MS;

/// A device state the Updates page narrows the fleet to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetStatus {
    /// Any update pending.
    Pending,
    /// A security or critical update pending.
    Security,
    /// The latest install failed, partly or entirely, or never reported.
    Failed,
    /// The last scan said the device must restart.
    Reboot,
    /// Supported, and no scan report for [`STALE_SCAN_MS`], or never.
    Stale,
}

/// Which devices the Updates page lists; every one by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetFilter {
    /// Part of the device's name, in any case.
    pub query: Option<String>,
    /// `Some(None)` for devices in no company.
    pub company_id: Option<Option<String>>,
    pub tag_id: Option<String>,
    pub platform: Option<DevicePlatform>,
    pub status: Option<FleetStatus>,
    /// Only devices in these companies, totals included: those the reader
    /// may see.
    pub within: CompanySet,
}

/// How many devices the fleet has, and how many are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FleetTotals {
    pub devices: i64,
    pub pending: i64,
    pub security: i64,
    pub failed: i64,
    pub reboot: i64,
    pub stale: i64,
}

/// One page of the fleet, most urgent first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetPage {
    pub devices: Vec<DeviceSummary>,
    /// Devices the filter matches, on this page or not.
    pub matching: i64,
    /// The whole fleet's the reader sees, whatever the other filters.
    pub totals: FleetTotals,
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
    pending_count: Option<i64>,
    security_count: Option<i64>,
    critical_count: Option<i64>,
    oldest_pending_at: Option<i64>,
    last_install_id: Option<String>,
    last_install_status: Option<String>,
    last_install_created_at: Option<i64>,
    last_install_completed_at: Option<i64>,
    last_install_installed: Option<i64>,
    last_install_failed: Option<i64>,
}

impl SummaryRow {
    /// The device's latest install, unless it is older than the install
    /// history keeps, when it is purged or about to be.
    fn last_install(&mut self, now: i64) -> Option<LastInstall> {
        let created_at = self
            .last_install_created_at
            .filter(|created_at| *created_at >= kept_since(now))?;
        Some(LastInstall {
            id: self.last_install_id.take()?,
            status: self.last_install_status.take()?,
            created_at,
            completed_at: self.last_install_completed_at,
            installed: self.last_install_installed.unwrap_or(0),
            failed: self.last_install_failed.unwrap_or(0),
        })
    }
}

/// The oldest install the history keeps at `now`: older ones are purged.
fn kept_since(now: i64) -> i64 {
    now - crate::updates::INSTALL_HISTORY_MS
}

#[derive(sqlx::FromRow)]
struct CountsRow {
    devices: i64,
    matching: i64,
    pending: i64,
    security: i64,
    failed: i64,
    reboot: i64,
    stale: i64,
}

fn status_column(column: DeviceUpdateStatus) -> Expr {
    Expr::col((DeviceUpdateStatus::Table, column))
}

/// Enrolled devices with their status rows, if they have one yet.
fn devices_select() -> SelectStatement {
    Query::select()
        .from(Agents::Table)
        .left_join(
            DeviceUpdateStatus::Table,
            Expr::col((DeviceUpdateStatus::Table, DeviceUpdateStatus::DeviceId))
                .equals((Agents::Table, Agents::Id)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .to_owned()
}

/// Each device's summary row, before its runs are added.
fn summary_select() -> SelectStatement {
    devices_select()
        .columns([
            (Agents::Table, Agents::Id),
            (Agents::Table, Agents::Name),
            (Agents::Table, Agents::CompanyId),
            (Agents::Table, Agents::Platform),
        ])
        .columns(
            [
                DeviceUpdateStatus::Manager,
                DeviceUpdateStatus::Supported,
                DeviceUpdateStatus::ScannedAt,
                DeviceUpdateStatus::ReceivedAt,
                DeviceUpdateStatus::ScanError,
                DeviceUpdateStatus::RebootPending,
                DeviceUpdateStatus::RebootPendingSince,
                DeviceUpdateStatus::PendingCount,
                DeviceUpdateStatus::SecurityCount,
                DeviceUpdateStatus::CriticalCount,
                DeviceUpdateStatus::OldestPendingAt,
                DeviceUpdateStatus::LastInstallId,
                DeviceUpdateStatus::LastInstallStatus,
                DeviceUpdateStatus::LastInstallCreatedAt,
                DeviceUpdateStatus::LastInstallCompletedAt,
                DeviceUpdateStatus::LastInstallInstalled,
                DeviceUpdateStatus::LastInstallFailed,
            ]
            .map(|column| (DeviceUpdateStatus::Table, column)),
        )
        .to_owned()
}

/// `lower(name) LIKE lower('%text%')`, the pattern's wildcards escaped by
/// `\`. Each database folds both sides the same way, so SQLite, whose
/// `lower` knows only ASCII letters, still matches other letters in their
/// exact case.
fn name_contains(text: &str) -> Expr {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = Expr::Binary(
        Box::new(Func::lower(Expr::val(format!("%{escaped}%"))).into()),
        BinOper::Escape,
        Box::new(Expr::Constant('\\'.into())),
    );
    Expr::expr(Func::lower(Expr::col((Agents::Table, Agents::Name)))).binary(BinOper::Like, pattern)
}

/// Whether the device's latest install, as its status row keeps it, failed,
/// partly or entirely, or never reported, and is still in the history.
fn last_install_failed(now: i64) -> Condition {
    Cond::all()
        .add(
            status_column(DeviceUpdateStatus::LastInstallStatus)
                .is_in(["failed", "partial", "lost"]),
        )
        .add(status_column(DeviceUpdateStatus::LastInstallCreatedAt).gte(kept_since(now)))
}

/// Whether a device is in `status` at `now`. A device without a status row
/// has nothing pending and has never reported a scan.
fn in_status(status: FleetStatus, now: i64) -> Condition {
    use DeviceUpdateStatus as S;
    match status {
        FleetStatus::Pending => Cond::all().add(status_column(S::PendingCount).gt(0)),
        FleetStatus::Security => Cond::any()
            .add(status_column(S::SecurityCount).gt(0))
            .add(status_column(S::CriticalCount).gt(0)),
        FleetStatus::Failed => last_install_failed(now),
        FleetStatus::Reboot => Cond::all().add(status_column(S::RebootPending).eq(true)),
        FleetStatus::Stale => Cond::all()
            .add(
                Cond::any()
                    .add(status_column(S::Supported).is_null())
                    .add(status_column(S::Supported).eq(true)),
            )
            .add(
                Cond::any()
                    .add(status_column(S::ReceivedAt).is_null())
                    .add(status_column(S::ReceivedAt).lt(now - STALE_SCAN_MS)),
            ),
    }
}

/// Whether a device matches the filter at `now`.
fn matches(filter: &FleetFilter, now: i64) -> Condition {
    let mut condition = Cond::all();
    if let Some(query) = filter.query.as_deref().map(str::trim)
        && !query.is_empty()
    {
        condition = condition.add(name_contains(query));
    }
    match &filter.company_id {
        None => {}
        Some(None) => {
            condition = condition.add(Expr::col((Agents::Table, Agents::CompanyId)).is_null())
        }
        Some(Some(company_id)) => {
            condition =
                condition.add(Expr::col((Agents::Table, Agents::CompanyId)).eq(company_id.as_str()))
        }
    }
    if let Some(tag_id) = &filter.tag_id {
        condition = condition.add(Expr::exists(
            Query::select()
                .expr(Expr::val(1))
                .from(DeviceTags::Table)
                .and_where(
                    Expr::col((DeviceTags::Table, DeviceTags::DeviceId))
                        .equals((Agents::Table, Agents::Id)),
                )
                .and_where(Expr::col((DeviceTags::Table, DeviceTags::TagId)).eq(tag_id.as_str()))
                .to_owned(),
        ));
    }
    if let Some(platform) = filter.platform {
        condition =
            condition.add(Expr::col((Agents::Table, Agents::Platform)).eq(platform.as_str()));
    }
    if let Some(status) = filter.status {
        condition = condition.add(in_status(status, now));
    }
    condition
}

/// How many devices `condition` holds for, never NULL.
fn count_where(condition: Condition) -> Expr {
    Func::cast_as(
        Func::coalesce([
            Func::sum(Expr::case(condition, Expr::cust("1")).finally(Expr::cust("0"))).into(),
            Expr::cust("0"),
        ]),
        Alias::new("BIGINT"),
    )
    .into()
}

/// The fleet's totals and how many devices `filter` matches at `now`, in
/// one read of the fleet's devices and status rows.
pub fn fleet_counts_query(filter: &FleetFilter, now: i64) -> SelectStatement {
    let state = |status| count_where(in_status(status, now));
    devices_select()
        .and_where_option(
            filter
                .within
                .company_condition((Agents::Table, Agents::CompanyId)),
        )
        .expr_as(Expr::cust("CAST(COUNT(*) AS BIGINT)"), "devices")
        .expr_as(count_where(matches(filter, now)), "matching")
        .expr_as(state(FleetStatus::Pending), "pending")
        .expr_as(state(FleetStatus::Security), "security")
        .expr_as(state(FleetStatus::Failed), "failed")
        .expr_as(state(FleetStatus::Reboot), "reboot")
        .expr_as(state(FleetStatus::Stale), "stale")
        .to_owned()
}

/// The devices `filter` matches at `now`, most urgent first: the most
/// security and critical updates, then the most pending, then by name. The
/// ID settles ties, so pages neither repeat nor miss a device.
pub fn fleet_page_query(
    filter: &FleetFilter,
    now: i64,
    limit: u64,
    offset: u64,
) -> SelectStatement {
    summary_select()
        .and_where(matches(filter, now).into())
        .and_where_option(
            filter
                .within
                .company_condition((Agents::Table, Agents::CompanyId)),
        )
        .order_by_expr(
            Expr::cust(
                "COALESCE(device_update_status.security_count, 0) \
                 + COALESCE(device_update_status.critical_count, 0)",
            ),
            Order::Desc,
        )
        .order_by_expr(
            Expr::cust("COALESCE(device_update_status.pending_count, 0)"),
            Order::Desc,
        )
        .order_by_expr(
            Func::lower(Expr::col((Agents::Table, Agents::Name))).into(),
            Order::Asc,
        )
        .order_by((Agents::Table, Agents::Id), Order::Asc)
        .limit(limit)
        .offset(offset)
        .to_owned()
}

/// A page of `limit` devices from `offset` of those `filter` matches at
/// `now`, how many it matches, and the fleet's totals. It runs as many
/// statements for any fleet: the fleet is read from its devices and status
/// rows, and runs only for the page's devices.
pub async fn fleet_page(
    executor: &mut impl Executor,
    filter: &FleetFilter,
    now: i64,
    limit: u64,
    offset: u64,
    connected: impl Fn(&str) -> bool,
) -> db::Result<FleetPage> {
    let counts: CountsRow = executor.fetch_one(&fleet_counts_query(filter, now)).await?;
    let devices = if limit == 0 || offset >= u64::try_from(counts.matching).unwrap_or(0) {
        Vec::new()
    } else {
        let rows = executor
            .fetch_all(&fleet_page_query(filter, now, limit, offset))
            .await?;
        complete(executor, rows, now, connected).await?
    };
    Ok(FleetPage {
        devices,
        matching: counts.matching,
        totals: FleetTotals {
            devices: counts.devices,
            pending: counts.pending,
            security: counts.security,
            failed: counts.failed,
            reboot: counts.reboot,
            stale: counts.stale,
        },
    })
}

/// The enrolled device's summary, or `None` when it isn't enrolled.
pub async fn summary(
    executor: &mut impl Executor,
    device_id: &str,
    connected: bool,
) -> db::Result<Option<DeviceSummary>> {
    let rows = executor
        .fetch_all(
            &summary_select()
                .and_where(Expr::col((Agents::Table, Agents::Id)).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(complete(executor, rows, now_ms(), |_| connected)
        .await?
        .into_iter()
        .next())
}

/// The summaries of `rows`' devices at `now`, in their order, with the
/// scans and installs they are running.
async fn complete(
    executor: &mut impl Executor,
    rows: Vec<SummaryRow>,
    now: i64,
    connected: impl Fn(&str) -> bool,
) -> db::Result<Vec<DeviceSummary>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
    let active: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([UpdateRuns::DeviceId, UpdateRuns::Kind])
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                .and_where(Expr::col(UpdateRuns::DeviceId).is_in(ids))
                .to_owned(),
        )
        .await?;
    let busy = |id: &str, kind: &str| {
        active
            .iter()
            .any(|(device, active)| device == id && active == kind)
    };
    Ok(rows
        .into_iter()
        .map(|mut row| {
            let last_install = row.last_install(now);
            let status = Status {
                manager: row.manager.clone(),
                ..Status::default()
            };
            DeviceSummary {
                connected: connected(&row.id),
                platform: DevicePlatform::parse(&row.platform),
                can_select: status.can_select(),
                manager: row.manager,
                supported: row.supported.unwrap_or(true),
                scanned_at: row.scanned_at,
                received_at: row.received_at,
                scan_error: row.scan_error,
                reboot_pending: row.reboot_pending.unwrap_or(false),
                reboot_pending_since: row.reboot_pending_since,
                pending: row.pending_count.unwrap_or(0),
                security: row.security_count.unwrap_or(0),
                critical: row.critical_count.unwrap_or(0),
                oldest_pending_at: row.oldest_pending_at,
                scanning: busy(&row.id, "scan"),
                installing: busy(&row.id, "install"),
                last_install,
                device_id: row.id,
                device_name: row.name,
                company_id: row.company_id,
            }
        })
        .collect())
}

/// An install was asked of the device at `created_at`: it is now its latest.
/// Written in the transaction that records the run.
pub(crate) async fn install_started(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
    created_at: i64,
) -> db::Result<()> {
    upsert_status(
        executor,
        device_id,
        vec![
            (DeviceUpdateStatus::LastInstallId, run_id.into()),
            (DeviceUpdateStatus::LastInstallStatus, "pending".into()),
            (DeviceUpdateStatus::LastInstallCreatedAt, created_at.into()),
            (
                DeviceUpdateStatus::LastInstallCompletedAt,
                Option::<i64>::None.into(),
            ),
            (DeviceUpdateStatus::LastInstallInstalled, 0_i64.into()),
            (DeviceUpdateStatus::LastInstallFailed, 0_i64.into()),
        ],
    )
    .await
}

/// The device's install `run_id` ended as `status` at `completed_at`,
/// with `installed` updates installed and `failed` failed. Its status row
/// says so while the run is the device's latest install. Written in the
/// transaction that records the outcome.
pub(crate) async fn install_ended(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
    status: &str,
    completed_at: i64,
    (installed, failed): (i64, i64),
) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(DeviceUpdateStatus::Table)
                .value(DeviceUpdateStatus::LastInstallStatus, status)
                .value(DeviceUpdateStatus::LastInstallCompletedAt, completed_at)
                .value(DeviceUpdateStatus::LastInstallInstalled, installed)
                .value(DeviceUpdateStatus::LastInstallFailed, failed)
                .and_where(Expr::col(DeviceUpdateStatus::DeviceId).eq(device_id))
                .and_where(Expr::col(DeviceUpdateStatus::LastInstallId).eq(run_id))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// The device's pending install `run_id` was removed before its Agent got
/// it: if it was the device's latest, the one before is again, or none.
/// Written in the transaction that removes the run.
pub(crate) async fn install_discarded(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
) -> db::Result<()> {
    // The device's runs by `idx_update_runs_device`, newest first.
    let previous: Option<LastInstall> = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    UpdateRuns::Id,
                    UpdateRuns::Status,
                    UpdateRuns::CreatedAt,
                    UpdateRuns::CompletedAt,
                    UpdateRuns::Installed,
                    UpdateRuns::Failed,
                ])
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(UpdateRuns::Kind).eq("install"))
                .order_by(UpdateRuns::CreatedAt, Order::Desc)
                .limit(1)
                .to_owned(),
        )
        .await?;
    let previous = previous.as_ref();
    executor
        .execute(
            &Query::update()
                .table(DeviceUpdateStatus::Table)
                .value(
                    DeviceUpdateStatus::LastInstallId,
                    previous.map(|run| run.id.clone()),
                )
                .value(
                    DeviceUpdateStatus::LastInstallStatus,
                    previous.map(|run| run.status.clone()),
                )
                .value(
                    DeviceUpdateStatus::LastInstallCreatedAt,
                    previous.map(|run| run.created_at),
                )
                .value(
                    DeviceUpdateStatus::LastInstallCompletedAt,
                    previous.and_then(|run| run.completed_at),
                )
                .value(
                    DeviceUpdateStatus::LastInstallInstalled,
                    previous.map_or(0, |run| run.installed),
                )
                .value(
                    DeviceUpdateStatus::LastInstallFailed,
                    previous.map_or(0, |run| run.failed),
                )
                .and_where(Expr::col(DeviceUpdateStatus::DeviceId).eq(device_id))
                .and_where(Expr::col(DeviceUpdateStatus::LastInstallId).eq(run_id))
                .to_owned(),
        )
        .await?;
    Ok(())
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
