//! Periodic housekeeping, off the request path.
//!
//! Every query of these tables already ignores expired rows, so purging them
//! only bounds storage and never decides whether a token is valid. Files in
//! the data directory that nothing refers to any more are removed too:
//! partial uploads a stopped server left, library files whose row is gone,
//! and thumbnails of deleted devices.
use std::{collections::HashSet, time::Duration};

use sea_query::{Expr, ExprTrait, IntoIden, Query};

use crate::{
    alerts::RESOLVED_RETENTION_MS,
    db::{
        self, Database,
        tables::{
            AgentInstallTokens, Agents, Alerts, ApiTokens, DeviceApplicationChanges, DeviceMetrics,
            FileDeliveries, Invitations, Jobs, PasswordResets, RemoteHandoffs, RemoteSessions,
            ScriptRuns, ToolboxFiles, UpdateRuns, UserSessions, WakeRequests,
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
const TOOLBOX_HISTORY_MS: i64 = 30 * DAY_MS;
/// A file this old belongs to no upload or deletion still in progress.
const ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub sessions: u64,
    pub api_tokens: u64,
    pub invitations: u64,
    pub password_resets: u64,
    pub installers: u64,
    pub handoffs: u64,
    pub remote_sessions: u64,
    pub script_runs: u64,
    pub file_deliveries: u64,
    pub device_metrics: u64,
    pub resolved_alerts: u64,
    pub application_changes: u64,
    pub jobs: u64,
    pub update_runs: u64,
    pub wakes: u64,
}

/// Purges every expired row and abandoned partial file once now and then
/// every 30 minutes, until the returned task is aborted.
pub fn spawn(database: Database, storage: Storage) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match purge(&database, now_ms()).await {
                Ok(purged) => tracing::info!(?purged, "purged expired rows"),
                Err(error) => tracing::warn!(%error, "could not purge expired rows"),
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
/// before, as their users still see them listed until then), toolbox history older than [`TOOLBOX_HISTORY_MS`], and
/// devices' resource usage older than [`HISTORY_MS`], and alerts resolved
/// more than [`RESOLVED_RETENTION_MS`] ago, and application changes older
/// than [`CHANGES_RETENTION_MS`], and jobs (with their devices) and wake
/// requests (with their relays) older than [`TOOLBOX_HISTORY_MS`].
pub async fn purge(database: &Database, now_ms: i64) -> db::Result<Purged> {
    let expired = now_ms - EXPIRED_GRACE_MS;
    let history = now_ms - TOOLBOX_HISTORY_MS;
    Ok(Purged {
        sessions: delete_before(
            database,
            UserSessions::Table,
            UserSessions::ExpiresAt,
            expired,
        )
        .await?,
        api_tokens: delete_before(
            database,
            ApiTokens::Table,
            ApiTokens::ExpiresAt,
            now_ms - crate::api_tokens::EXPIRED_RETENTION_MS,
        )
        .await?,
        invitations: delete_before(
            database,
            Invitations::Table,
            Invitations::ExpiresAt,
            expired,
        )
        .await?,
        password_resets: delete_before(
            database,
            PasswordResets::Table,
            PasswordResets::ExpiresAt,
            expired,
        )
        .await?,
        installers: delete_before(
            database,
            AgentInstallTokens::Table,
            AgentInstallTokens::ExpiresAt,
            expired,
        )
        .await?,
        handoffs: delete_before(
            database,
            RemoteHandoffs::Table,
            RemoteHandoffs::ExpiresAt,
            expired,
        )
        .await?,
        remote_sessions: delete_before(
            database,
            RemoteSessions::Table,
            RemoteSessions::ExpiresAt,
            expired,
        )
        .await?,
        script_runs: delete_before(database, ScriptRuns::Table, ScriptRuns::CreatedAt, history)
            .await?,
        file_deliveries: delete_before(
            database,
            FileDeliveries::Table,
            FileDeliveries::CreatedAt,
            history,
        )
        .await?,
        device_metrics: delete_before(
            database,
            DeviceMetrics::Table,
            DeviceMetrics::Minute,
            now_ms - HISTORY_MS,
        )
        .await?,
        resolved_alerts: delete_before(
            database,
            Alerts::Table,
            Alerts::ResolvedAt,
            now_ms - RESOLVED_RETENTION_MS,
        )
        .await?,
        application_changes: delete_before(
            database,
            DeviceApplicationChanges::Table,
            DeviceApplicationChanges::ObservedAt,
            now_ms - CHANGES_RETENTION_MS,
        )
        .await?,
        jobs: delete_before(database, Jobs::Table, Jobs::CreatedAt, history).await?,
        update_runs: purge_update_runs(database, now_ms).await?,
        wakes: delete_before(
            database,
            WakeRequests::Table,
            WakeRequests::CreatedAt,
            history,
        )
        .await?,
    })
}

/// Deletes scans older than [`crate::updates::SCAN_HISTORY_MS`] and installs
/// (with their items) older than [`crate::updates::INSTALL_HISTORY_MS`].
async fn purge_update_runs(database: &Database, now_ms: i64) -> db::Result<u64> {
    let mut purged = 0;
    for (kind, retention) in [
        ("scan", crate::updates::SCAN_HISTORY_MS),
        ("install", crate::updates::INSTALL_HISTORY_MS),
    ] {
        purged += database
            .execute(
                &Query::delete()
                    .from_table(UpdateRuns::Table)
                    .and_where(Expr::col(UpdateRuns::Kind).eq(kind))
                    .and_where(Expr::col(UpdateRuns::CreatedAt).lt(now_ms - retention))
                    .to_owned(),
            )
            .await?;
    }
    Ok(purged)
}

async fn delete_before(
    database: &Database,
    table: impl IntoIden,
    column: impl IntoIden,
    cutoff_ms: i64,
) -> db::Result<u64> {
    database
        .execute(
            &Query::delete()
                .from_table(table)
                .and_where(Expr::col(column).lte(cutoff_ms))
                .to_owned(),
        )
        .await
}

/// Removes old files in the data directory that no row refers to: partial
/// uploads, library files whose row was never written or was deleted, and
/// thumbnails of deleted devices. Returns how many it removed.
pub async fn remove_orphans(database: &Database, storage: &Storage) -> anyhow::Result<u64> {
    let listed = storage.clone();
    let (partial, files, thumbnails) = tokio::task::spawn_blocking(move || {
        anyhow::Ok((
            listed.sweep_partial(ORPHAN_AGE)?,
            listed.old_toolbox_files(ORPHAN_AGE)?,
            listed.old_thumbnails(ORPHAN_AGE)?,
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
