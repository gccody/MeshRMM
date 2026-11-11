//! Periodic housekeeping, off the request path.
//!
//! Every query of these tables already ignores expired rows, so purging them
//! only bounds storage and never decides whether a token is valid. Files in
//! the data directory that nothing refers to any more are removed too:
//! partial uploads a stopped server left, library files whose row is gone,
//! thumbnails of deleted devices, and documents of deleted or purged reports.
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use sea_query::{DeleteStatement, DynIden, Expr, ExprTrait, IntoIden, Order, Query};

use crate::{
    alerts::RESOLVED_RETENTION_MS,
    db::{
        self, Database,
        tables::{
            AgentInstallTokens, Agents, Alerts, ApiTokens, DeploymentTokenEnrollments,
            DeploymentTokens, DeviceApplicationChanges, DeviceMetrics, FileDeliveries, Invitations,
            JobDevices, Jobs, PasswordResets, RemoteHandoffs, RemoteSessions, Reports, ScriptRuns,
            ToolboxFiles, UpdateRuns, UserSessions, WakeRequests,
        },
    },
    realtime::metrics::HISTORY_MS,
    storage::Storage,
    system_info::CHANGES_RETENTION_MS,
    time::{DAY_MS, MINUTE_MS, now_ms},
};

const INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Expired rows are kept this long, so a request racing the purge still sees
/// a token it considers live.
const EXPIRED_GRACE_MS: i64 = 10 * MINUTE_MS;
/// How long toolbox script runs, with their output, file deliveries, jobs
/// and wake requests are kept. The audit log keeps that they happened.
pub const TOOLBOX_HISTORY_MS: i64 = 30 * DAY_MS;
/// A file this old belongs to no upload or deletion still in progress.
const ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Rows one purge statement deletes at most. Each statement is its own short
/// transaction: a few thousand small rows take milliseconds on SQLite, where
/// one writer at a time holds the lock and the rest wait at most 5 seconds.
pub const BATCH_ROWS: u64 = 2000;
/// Waited between a table's batches, so writers queued behind one get the
/// lock. SQLite's busy handler retries at most 100 ms apart, so a shorter gap
/// can pass with none of them trying.
const BATCH_PAUSE: Duration = Duration::from_millis(100);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Purged {
    /// Each table rows were deleted from, by its name.
    pub deleted: BTreeMap<String, Deleted>,
    /// Tables that could not be purged; the next run tries them again.
    pub failed: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Deleted {
    pub rows: u64,
    /// Statements run, including the last that found fewer rows than a batch.
    pub batches: u64,
}

impl Purged {
    /// Rows deleted from `table`.
    pub fn rows(&self, table: &str) -> u64 {
        self.deleted.get(table).map_or(0, |deleted| deleted.rows)
    }
}

/// Purges every expired row and abandoned partial file once now and then
/// every 30 minutes, until the returned task is aborted.
pub fn spawn(database: Database, storage: Storage) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let purged = purge(&database, now_ms()).await;
            if !purged.deleted.is_empty() {
                tracing::info!(deleted = ?purged.deleted, "purged expired rows");
            }
            match remove_orphans(&database, &storage).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "removed files nothing refers to"),
                Err(error) => tracing::warn!(
                    error = format!("{error:#}"),
                    "could not remove unused files"
                ),
            }
        }
    })
}

/// Deletes tokens and sessions that expired more than [`EXPIRED_GRACE_MS`]
/// before `now_ms` (API tokens [`crate::api_tokens::EXPIRED_RETENTION_MS`]
/// before, as their users still see them listed until then, and deployment
/// tokens [`crate::api::DEPLOYMENT_TOKEN_RETENTION_MS`] before, revoked or
/// not, for the same reason), toolbox history older than [`TOOLBOX_HISTORY_MS`], and
/// devices' resource usage older than [`HISTORY_MS`], and alerts resolved
/// more than [`RESOLVED_RETENTION_MS`] ago, and application changes older
/// than [`CHANGES_RETENTION_MS`], and jobs (with their devices) and wake
/// requests (with their relays) older than [`TOOLBOX_HISTORY_MS`], and scans
/// older than [`crate::updates::SCAN_HISTORY_MS`] and installs (with their
/// items) older than [`crate::updates::INSTALL_HISTORY_MS`], and generated
/// reports past their `expires_at` ([`crate::reports::RETENTION_MS`]).
///
/// Each table is purged in batches of [`BATCH_ROWS`]. A table that fails is
/// logged and left for the next run, and the purge goes on to the next.
pub async fn purge(database: &Database, now_ms: i64) -> Purged {
    purge_in_batches(database, now_ms, BATCH_ROWS).await
}

/// [`purge`] with batches of `batch_rows`.
///
/// A run goes on until nothing is due rather than stopping after so many
/// batches: each batch is short and followed by a pause, so a large backlog
/// (a server that was down, or 100,000 devices' resource usage) holds up no
/// other writer, and a cap low enough to matter would fall behind the rows
/// that many devices add.
pub async fn purge_in_batches(database: &Database, now_ms: i64, batch_rows: u64) -> Purged {
    let mut purged = Purged::default();
    for group in due(now_ms, batch_rows) {
        for due in group {
            let table = due.table.inner().into_owned();
            let deleted = purged.deleted.entry(table.clone()).or_default();
            if let Err(error) = due.delete(database, deleted).await {
                tracing::warn!(table = %table, %error, "could not purge expired rows");
                purged.failed.push(table);
                break;
            }
        }
    }
    purged.deleted.retain(|_, deleted| deleted.rows > 0);
    purged
}

/// The statement that deletes one batch of each table, with the table's name,
/// to check how the database runs it.
pub fn batch_statements(now_ms: i64) -> Vec<(String, DeleteStatement)> {
    due(now_ms, BATCH_ROWS)
        .into_iter()
        .flatten()
        .map(|due| (due.table.inner().into_owned(), due.statement()))
        .collect()
}

/// Every table's due rows, in groups purged in order; a failure skips the
/// rest of its group.
fn due(now_ms: i64, batch_rows: u64) -> [Vec<Due>; 17] {
    let expired = now_ms - EXPIRED_GRACE_MS;
    let history = now_ms - TOOLBOX_HISTORY_MS;
    // Script runs hold up to a megabyte of output each, and installs have
    // their items deleted with them.
    let large_rows = Ord::max(batch_rows / 20, 1);
    let old_jobs = Query::select()
        .column(Jobs::Id)
        .from(Jobs::Table)
        .and_where(Expr::col(Jobs::CreatedAt).lte(history))
        .to_owned();
    [
        vec![Due::before(
            UserSessions::Table,
            UserSessions::Id,
            UserSessions::ExpiresAt,
            expired,
            batch_rows,
        )],
        vec![Due::before(
            ApiTokens::Table,
            ApiTokens::Id,
            ApiTokens::ExpiresAt,
            now_ms - crate::api_tokens::EXPIRED_RETENTION_MS,
            batch_rows,
        )],
        vec![Due::before(
            Invitations::Table,
            Invitations::Id,
            Invitations::ExpiresAt,
            expired,
            batch_rows,
        )],
        vec![Due::before(
            PasswordResets::Table,
            PasswordResets::Id,
            PasswordResets::ExpiresAt,
            expired,
            batch_rows,
        )],
        vec![Due::before(
            AgentInstallTokens::Table,
            AgentInstallTokens::Id,
            AgentInstallTokens::ExpiresAt,
            expired,
            batch_rows,
        )],
        // A token's enrollments go with it; those past their recovery
        // window go first.
        vec![
            Due {
                key: vec![
                    DeploymentTokenEnrollments::TokenId.into_iden(),
                    DeploymentTokenEnrollments::RedemptionKeyHash.into_iden(),
                ],
                ..Due::before(
                    DeploymentTokenEnrollments::Table,
                    DeploymentTokenEnrollments::TokenId,
                    DeploymentTokenEnrollments::RecoverableUntil,
                    expired,
                    batch_rows,
                )
            },
            Due::before(
                DeploymentTokens::Table,
                DeploymentTokens::Id,
                DeploymentTokens::ExpiresAt,
                now_ms - crate::api::DEPLOYMENT_TOKEN_RETENTION_MS,
                batch_rows,
            ),
        ],
        vec![Due::before(
            RemoteHandoffs::Table,
            RemoteHandoffs::TokenHash,
            RemoteHandoffs::ExpiresAt,
            expired,
            batch_rows,
        )],
        vec![Due::before(
            RemoteSessions::Table,
            RemoteSessions::Id,
            RemoteSessions::ExpiresAt,
            expired,
            batch_rows,
        )],
        vec![Due::before(
            ScriptRuns::Table,
            ScriptRuns::Id,
            ScriptRuns::CreatedAt,
            history,
            large_rows,
        )],
        vec![Due::before(
            FileDeliveries::Table,
            FileDeliveries::Id,
            FileDeliveries::CreatedAt,
            history,
            batch_rows,
        )],
        vec![Due {
            key: vec![
                DeviceMetrics::DeviceId.into_iden(),
                DeviceMetrics::Minute.into_iden(),
            ],
            ..Due::before(
                DeviceMetrics::Table,
                DeviceMetrics::Minute,
                DeviceMetrics::Minute,
                now_ms - HISTORY_MS,
                batch_rows,
            )
        }],
        vec![Due::before(
            Alerts::Table,
            Alerts::Id,
            Alerts::ResolvedAt,
            now_ms - RESOLVED_RETENTION_MS,
            batch_rows,
        )],
        vec![Due::before(
            DeviceApplicationChanges::Table,
            DeviceApplicationChanges::Id,
            DeviceApplicationChanges::ObservedAt,
            now_ms - CHANGES_RETENTION_MS,
            batch_rows,
        )],
        // A job's devices go first, in batches of their own: a job on every
        // device has a row for each, too many for one batch's cascade.
        vec![
            Due {
                table: JobDevices::Table.into_iden(),
                key: vec![
                    JobDevices::JobId.into_iden(),
                    JobDevices::DeviceId.into_iden(),
                ],
                due: Expr::col(JobDevices::JobId).in_subquery(old_jobs),
                order: None,
                batch_rows,
            },
            Due::before(Jobs::Table, Jobs::Id, Jobs::CreatedAt, history, batch_rows),
        ],
        vec![
            Due::update_runs("scan", now_ms - crate::updates::SCAN_HISTORY_MS, batch_rows),
            Due::update_runs(
                "install",
                now_ms - crate::updates::INSTALL_HISTORY_MS,
                large_rows,
            ),
        ],
        vec![Due::before(
            WakeRequests::Table,
            WakeRequests::Id,
            WakeRequests::CreatedAt,
            history,
            batch_rows,
        )],
        // Their documents go with them, in [`remove_orphans`].
        vec![Due::before(
            Reports::Table,
            Reports::Id,
            Reports::ExpiresAt,
            now_ms,
            batch_rows,
        )],
    ]
}

/// A table's rows that are due for deletion.
struct Due {
    table: DynIden,
    /// The columns of its primary key.
    key: Vec<DynIden>,
    due: Expr,
    /// The indexed column batches take the oldest rows by.
    order: Option<DynIden>,
    batch_rows: u64,
}

impl Due {
    /// Rows whose `column` is at or before `cutoff_ms`.
    fn before(
        table: impl IntoIden,
        key: impl IntoIden,
        column: impl IntoIden,
        cutoff_ms: i64,
        batch_rows: u64,
    ) -> Self {
        let column = column.into_iden();
        Self {
            table: table.into_iden(),
            key: vec![key.into_iden()],
            due: Expr::col(column.clone()).lte(cutoff_ms),
            order: Some(column),
            batch_rows,
        }
    }

    /// Update runs of `kind` created before `cutoff_ms`.
    fn update_runs(kind: &str, cutoff_ms: i64, batch_rows: u64) -> Self {
        Self {
            due: Expr::col(UpdateRuns::Kind)
                .eq(kind)
                .and(Expr::col(UpdateRuns::CreatedAt).lt(cutoff_ms)),
            ..Self::before(
                UpdateRuns::Table,
                UpdateRuns::Id,
                UpdateRuns::CreatedAt,
                cutoff_ms,
                batch_rows,
            )
        }
    }

    /// Deletes the rows a batch at a time until none are left, counting them
    /// into `deleted` as it goes.
    async fn delete(&self, database: &Database, deleted: &mut Deleted) -> db::Result<()> {
        let statement = self.statement();
        loop {
            let rows = database.execute(&statement).await?;
            deleted.rows += rows;
            deleted.batches += 1;
            if rows < self.batch_rows {
                return Ok(());
            }
            tokio::time::sleep(BATCH_PAUSE).await;
        }
    }

    /// Deletes one batch. Neither database has DELETE ... LIMIT, so it deletes
    /// by key the rows a limited select finds.
    fn statement(&self) -> DeleteStatement {
        let mut batch = Query::select();
        batch
            .columns(self.key.iter().cloned())
            .from(self.table.clone())
            .and_where(self.due.clone())
            .limit(self.batch_rows);
        if let Some(order) = &self.order {
            batch.order_by(order.clone(), Order::Asc);
        }
        let key = match self.key.as_slice() {
            [column] => Expr::col(column.clone()),
            columns => Expr::tuple(columns.iter().map(|column| Expr::col(column.clone()))),
        };
        Query::delete()
            .from_table(self.table.clone())
            .and_where(key.in_subquery(batch))
            .to_owned()
    }
}

/// Removes old files in the data directory that no row refers to: partial
/// uploads, library files whose row was never written or was deleted, and
/// thumbnails of deleted devices. Returns how many it removed.
pub async fn remove_orphans(database: &Database, storage: &Storage) -> anyhow::Result<u64> {
    let listed = storage.clone();
    let (partial, files, thumbnails, reports) = tokio::task::spawn_blocking(move || {
        anyhow::Ok((
            listed.sweep_partial(ORPHAN_AGE)?,
            listed.old_toolbox_files(ORPHAN_AGE)?,
            listed.old_thumbnails(ORPHAN_AGE)?,
            listed.report_ids()?,
        ))
    })
    .await??;
    let mut removed = partial;
    let kept = existing(
        database,
        ToolboxFiles::Table,
        ToolboxFiles::Id,
        &files,
        false,
    )
    .await?;
    for id in files.iter().filter(|id| !kept.contains(*id)) {
        storage.remove(&storage.toolbox_file(id)).await?;
        removed += 1;
    }
    let kept = existing(database, Agents::Table, Agents::Id, &thumbnails, true).await?;
    for id in thumbnails.iter().filter(|id| !kept.contains(*id)) {
        storage.remove(&storage.thumbnail(id)).await?;
        removed += 1;
    }
    // A report's row is written before its document, so a document of any
    // age without one belongs to a report that was deleted or purged.
    let kept = existing(database, Reports::Table, Reports::Id, &reports, false).await?;
    for id in reports.iter().filter(|id| !kept.contains(*id)) {
        storage.remove(&storage.report(id)).await?;
        removed += 1;
    }
    Ok(removed)
}

/// Which of `ids` have a row in `table`; for devices, an undeleted one.
async fn existing(
    database: &Database,
    table: impl IntoIden,
    id: impl IntoIden,
    ids: &[String],
    active_devices: bool,
) -> db::Result<HashSet<String>> {
    let (table, id) = (table.into_iden(), id.into_iden());
    let mut found = HashSet::new();
    for chunk in ids.chunks(500) {
        let mut select = Query::select();
        select
            .column(id.clone())
            .from(table.clone())
            .and_where(Expr::col(id.clone()).is_in(chunk.iter().cloned()));
        if active_devices {
            select.and_where(Expr::col(Agents::DeletionRequestedAt).is_null());
        }
        let rows: Vec<(String,)> = database.fetch_all(&select).await?;
        found.extend(rows.into_iter().map(|(id,)| id));
    }
    Ok(found)
}
