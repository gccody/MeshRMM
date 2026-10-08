//! Periodic housekeeping, off the request path.
//!
//! Every query of these tables already ignores expired rows, so purging them
//! only bounds storage and never decides whether a token is valid.
use std::time::Duration;

use sea_query::{Expr, ExprTrait, IntoIden, Query};

use crate::{
    db::{
        self, Database,
        tables::{
            AgentInstallTokens, FileDeliveries, Invitations, PasswordResets, RemoteHandoffs,
            RemoteSessions, ScriptRuns, UserSessions,
        },
    },
    time::{DAY_MS, MINUTE_MS, now_ms},
};

const INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Expired rows are kept this long, so a request racing the purge still sees
/// a token it considers live.
const EXPIRED_GRACE_MS: i64 = 10 * MINUTE_MS;
/// How long toolbox script runs, with their output, and file deliveries are
/// kept. The audit log keeps that they happened.
const TOOLBOX_HISTORY_MS: i64 = 30 * DAY_MS;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub sessions: u64,
    pub invitations: u64,
    pub password_resets: u64,
    pub installers: u64,
    pub handoffs: u64,
    pub remote_sessions: u64,
    pub script_runs: u64,
    pub file_deliveries: u64,
}

/// Purges every expired row once now and then every 30 minutes, until the
/// returned task is aborted.
pub fn spawn(database: Database) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match purge(&database, now_ms()).await {
                Ok(purged) => tracing::info!(?purged, "purged expired rows"),
                Err(error) => tracing::warn!(%error, "could not purge expired rows"),
            }
        }
    })
}

/// Deletes tokens and sessions that expired more than [`EXPIRED_GRACE_MS`]
/// before `now_ms`, and toolbox history older than [`TOOLBOX_HISTORY_MS`].
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
    })
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
