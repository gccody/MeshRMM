//! A process-wide request to end the remote session cleanly: release the
//! device lease on the server, then return from the session loop. Used when a
//! new dashboard link replaces this viewer.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

static REQUESTED: AtomicBool = AtomicBool::new(false);

fn notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

pub fn request(reason: &'static str) {
    if !REQUESTED.swap(true, Ordering::AcqRel) {
        tracing::info!(reason, "ending the remote session");
    }
    notify().notify_waiters();
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// Completes once [`request`] has been called, including before this call.
pub async fn wait() {
    loop {
        let notified = notify().notified();
        tokio::pin!(notified);
        // Register before checking the flag so a concurrent request is not missed.
        notified.as_mut().enable();
        if requested() {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waiters_wake_on_request_and_later_waiters_return_at_once() {
        let waiter = tokio::spawn(wait());
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        request("test");
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter woke")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), wait())
            .await
            .expect("a later waiter returns at once");
        assert!(requested());
    }
}
