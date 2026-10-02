//! Housekeeping run by the Worker's cron trigger, off the request path.
//!
//! Every query of these tables already ignores expired rows, so purging them
//! only bounds storage and never decides whether a token is valid.
use crate::*;

/// Expired rows are kept this long, so a Worker whose clock runs behind still
/// finds a token it considers live.
const EXPIRED_TOKEN_GRACE_MS: i64 = 10 * 60 * 1000;
/// How long toolbox script runs, with their output, and file deliveries are
/// kept. The audit log keeps that they happened.
const TOOLBOX_HISTORY_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Deletes event subscriptions, remote handoffs and Agent installer tokens
/// that expired more than [`EXPIRED_TOKEN_GRACE_MS`] before `now_ms`, and
/// toolbox runs and deliveries older than [`TOOLBOX_HISTORY_MS`].
pub(crate) async fn purge_expired_tokens(environment: &Env, now_ms: i64) -> Result<()> {
    let db = environment.d1("DB")?;
    let cutoff = now_ms - EXPIRED_TOKEN_GRACE_MS;
    let results = metered_batch(
        &db,
        vec![
            query!(
                &db,
                "DELETE FROM agent_event_subscriptions WHERE expires_at <= ?1",
                cutoff
            )?,
            query!(
                &db,
                "DELETE FROM remote_handoffs WHERE expires_at <= ?1",
                cutoff
            )?,
            query!(
                &db,
                "DELETE FROM agent_install_tokens WHERE expires_at <= ?1",
                cutoff
            )?,
            query!(
                &db,
                "DELETE FROM script_runs WHERE created_at <= ?1",
                now_ms - TOOLBOX_HISTORY_MS
            )?,
            query!(
                &db,
                "DELETE FROM file_deliveries WHERE created_at <= ?1",
                now_ms - TOOLBOX_HISTORY_MS
            )?,
        ],
    )
    .await?;
    let deleted = |statement: usize| {
        results
            .get(statement)
            .and_then(|result| result.meta().ok().flatten())
            .and_then(|meta| meta.changes)
            .unwrap_or_default()
    };
    console_log!(
        "event=expired_tokens_purged subscriptions={} handoffs={} installers={} script_runs={} file_deliveries={}",
        deleted(0),
        deleted(1),
        deleted(2),
        deleted(3),
        deleted(4)
    );
    Ok(())
}
