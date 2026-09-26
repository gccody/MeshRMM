//! Housekeeping run by the Worker's cron trigger, off the request path.
//!
//! Every query of these tables already ignores expired rows, so purging them
//! only bounds storage and never decides whether a token is valid.
use crate::*;

/// Expired rows are kept this long, so a Worker whose clock runs behind still
/// finds a token it considers live.
const EXPIRED_TOKEN_GRACE_MS: i64 = 10 * 60 * 1000;

/// Deletes event subscriptions, remote handoffs and Agent installer tokens
/// that expired more than [`EXPIRED_TOKEN_GRACE_MS`] before `now_ms`.
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
        "event=expired_tokens_purged subscriptions={} handoffs={} installers={}",
        deleted(0),
        deleted(1),
        deleted(2)
    );
    Ok(())
}
