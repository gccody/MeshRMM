use std::time::Duration;

pub use meshrmm_signaling_client::authenticated_websocket;
use meshrmm_signaling_client::{ReconnectBackoff, endpoint_url, handshake_status};
use url::Url;

pub fn agent_connection_url(server: &str, device_id: &str) -> anyhow::Result<Url> {
    endpoint_url(server, &["v1", "agents", device_id, "connect"], &[], true)
}

pub fn session_signal_url(server: &str, session_id: &str, role: &str) -> anyhow::Result<Url> {
    endpoint_url(
        server,
        &["v1", "remote", "sessions", session_id, "signal"],
        &[("role", role)],
        true,
    )
}

/// When the control connection tries again. A failed attempt backs off to
/// about two minutes and starts over only after a connection that lasted
/// [`STABLE_CONNECTION`](meshrmm_signaling_client::STABLE_CONNECTION), so an
/// Agent the server accepts and then drops, such as one superseded by another
/// machine with its device ID, does not reconnect every second. A server that
/// does not recognize the credential, which an administrator can fix by
/// restoring the device, is asked far less often. The caller jitters every
/// delay.
#[derive(Debug)]
pub struct ControlReconnect {
    failed: ReconnectBackoff,
    refused: ReconnectBackoff,
}

/// How a control-connection attempt ended.
#[derive(Debug, Clone, Copy)]
pub enum ControlAttempt {
    /// The handshake failed, other than by [`ControlAttempt::Refused`].
    Failed,
    /// The server answered the handshake with 401, 403 or 404.
    Refused,
    /// The server accepted the handshake, and the connection lasted this long.
    Connected(Duration),
}

impl ControlAttempt {
    pub fn failed(error: &anyhow::Error) -> Self {
        if handshake_status(error).is_some_and(|status| matches!(status, 401 | 403 | 404)) {
            Self::Refused
        } else {
            Self::Failed
        }
    }
}

impl ControlReconnect {
    pub fn new() -> Self {
        Self {
            failed: ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(120)),
            refused: ReconnectBackoff::new(Duration::from_secs(120), Duration::from_secs(3600)),
        }
    }

    /// The delay, before jitter, after `attempt`.
    pub fn delay_after(&mut self, attempt: ControlAttempt) -> Duration {
        match attempt {
            ControlAttempt::Failed => self.failed.delay_after(None),
            ControlAttempt::Refused => self.refused.next_delay(),
            ControlAttempt::Connected(lasted) => {
                // The server knows the credential again.
                self.refused.reset();
                self.failed.delay_after(Some(lasted))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_signaling_client::STABLE_CONNECTION;
    use tokio_tungstenite::tungstenite::{Error, http};

    const fn seconds(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn refused(status: u16) -> anyhow::Error {
        let response = http::Response::builder().status(status).body(None).unwrap();
        anyhow::Error::new(Error::Http(Box::new(response)))
            .context("signaling WebSocket handshake failed")
    }

    #[test]
    fn failures_back_off_to_two_minutes() {
        let mut reconnect = ControlReconnect::new();
        let delays: Vec<_> = (0..10)
            .map(|_| reconnect.delay_after(ControlAttempt::Failed))
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 64, 120, 120, 120].map(seconds));
    }

    #[test]
    fn only_a_stable_connection_restarts_the_schedule() {
        let mut reconnect = ControlReconnect::new();
        let short = ControlAttempt::Connected(seconds(1));
        // Accepted, then dropped at once, as when superseded: keeps backing off.
        let delays: Vec<_> = (0..9).map(|_| reconnect.delay_after(short)).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 64, 120, 120].map(seconds));
        assert_eq!(
            reconnect.delay_after(ControlAttempt::Connected(STABLE_CONNECTION)),
            seconds(1)
        );
        assert_eq!(reconnect.delay_after(ControlAttempt::Failed), seconds(2));
    }

    #[test]
    fn a_refused_credential_is_retried_on_the_long_schedule() {
        let mut reconnect = ControlReconnect::new();
        assert_eq!(reconnect.delay_after(ControlAttempt::Failed), seconds(1));
        let delays: Vec<_> = (0..7)
            .map(|_| reconnect.delay_after(ControlAttempt::failed(&refused(401))))
            .collect();
        assert_eq!(delays, [120, 240, 480, 960, 1920, 3600, 3600].map(seconds));
        // Other failures in between keep their own schedule.
        assert_eq!(reconnect.delay_after(ControlAttempt::Failed), seconds(2));
        assert_eq!(
            reconnect.delay_after(ControlAttempt::failed(&refused(404))),
            seconds(3600)
        );
        // Accepted again: a later refusal starts the long schedule over.
        reconnect.delay_after(ControlAttempt::Connected(seconds(1)));
        assert_eq!(
            reconnect.delay_after(ControlAttempt::failed(&refused(403))),
            seconds(120)
        );
    }

    #[test]
    fn only_unknown_credentials_count_as_refused() {
        for status in [401, 403, 404] {
            assert!(matches!(
                ControlAttempt::failed(&refused(status)),
                ControlAttempt::Refused
            ));
        }
        for error in [
            refused(400),
            refused(429),
            refused(502),
            anyhow::anyhow!("signaling WebSocket handshake timed out"),
        ] {
            assert!(matches!(
                ControlAttempt::failed(&error),
                ControlAttempt::Failed
            ));
        }
    }
}
