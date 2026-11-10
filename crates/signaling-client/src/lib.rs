//! Shared URL construction and authenticated WebSocket connection support.

#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        handshake::client::Response,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};
use url::Url;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long an attempt must stream before its failure restarts the reconnect
/// schedule. Shorter attempts keep backing off, so a peer that connects and
/// then fails within seconds does not reconnect every second forever.
pub const STABLE_CONNECTION: Duration = Duration::from_secs(20);

/// Capped exponential delay shared by the Agent and viewer reconnect loops.
/// A fresh loop retries quickly, then backs off enough to avoid hammering the
/// control plane while a machine is rebooting or a network is unavailable.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    initial: Duration,
    maximum: Duration,
    next: Duration,
}

impl ReconnectBackoff {
    pub fn new(initial: Duration, maximum: Duration) -> Self {
        assert!(initial > Duration::ZERO);
        assert!(maximum >= initial);
        Self {
            initial,
            maximum,
            next: initial,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.maximum);
        delay
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    /// The delay after an attempt that streamed for `streamed_for` (`None`
    /// if it never streamed). An attempt that streamed for at least
    /// [`STABLE_CONNECTION`] restarts the schedule; a connect-then-fail loop
    /// keeps growing.
    pub fn delay_after(&mut self, streamed_for: Option<Duration>) -> Duration {
        if streamed_for.is_some_and(|streamed| streamed >= STABLE_CONNECTION) {
            self.reset();
        }
        self.next_delay()
    }
}

/// `delay` shortened by up to half at random, so peers that lost the server
/// at the same moment spread their retries out instead of arriving together.
/// For loops that show no countdown: the viewer shows the delay it waits.
pub fn jittered(delay: Duration) -> Duration {
    let fraction = match getrandom::u64() {
        // The top 53 bits, which an `f64` holds exactly, as a fraction of 1.
        Ok(random) => (random >> 11) as f64 / (1_u64 << 53) as f64,
        Err(error) => {
            tracing::warn!(%error, "no random source for reconnect jitter");
            1.0
        }
    };
    scaled(delay, fraction)
}

/// `delay` scaled into `[delay / 2, delay]` by `fraction` in `[0, 1]`.
fn scaled(delay: Duration, fraction: f64) -> Duration {
    delay.mul_f64(0.5 + fraction.clamp(0.0, 1.0) / 2.0)
}

pub fn endpoint_url(
    server: &str,
    segments: &[&str],
    query: &[(&str, &str)],
    websocket: bool,
) -> anyhow::Result<Url> {
    let mut url = Url::parse(server).context("MeshRMM server is not a valid URL")?;
    if websocket {
        match url.scheme() {
            "https" => url
                .set_scheme("wss")
                .map_err(|_| anyhow::anyhow!("invalid HTTPS URL"))?,
            "wss" => {}
            _ => bail!("MeshRMM signaling requires HTTPS or WSS"),
        }
    } else if url.scheme() != "https" {
        bail!("MeshRMM API URL must use HTTPS");
    }
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("server URL cannot be a base URL"))?;
        path.clear();
        path.extend(segments);
    }
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query.iter().copied());
    }
    Ok(url)
}

pub async fn authenticated_websocket(url: Url, token: &str) -> anyhow::Result<(Socket, Response)> {
    if url.scheme() != "wss" {
        bail!("MeshRMM authenticated signaling requires WSS");
    }
    let mut request = url
        .as_str()
        .into_client_request()
        .context("failed to build signaling WebSocket request")?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .context("invalid authentication token header")?,
    );
    let tls = tls::websocket_client_config().context("failed to configure signaling TLS")?;
    tokio::time::timeout(
        Duration::from_secs(15),
        connect_async_tls_with_config(request, None, false, Some(Connector::Rustls(tls))),
    )
    .await
    .context("signaling WebSocket handshake timed out")?
    .context("signaling WebSocket handshake failed")
}

/// Returns true when retrying a WebSocket with the same credentials cannot
/// succeed because the server rejected or no longer recognizes the session.
pub fn is_terminal_websocket_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TerminalClose>().is_some()
        || handshake_status(error)
            .is_some_and(|status| matches!(status, 400 | 401 | 403 | 404 | 410))
}

/// The HTTP status with which the server refused a WebSocket handshake.
pub fn handshake_status(error: &anyhow::Error) -> Option<u16> {
    error.chain().find_map(|cause| {
        match cause.downcast_ref::<tokio_tungstenite::tungstenite::Error>()? {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                Some(response.status().as_u16())
            }
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_urls_require_encryption_and_preserve_routing() {
        for server in [
            "http://example.com",
            "ws://example.com",
            "ftp://example.com",
        ] {
            for websocket in [false, true] {
                assert!(endpoint_url(server, &["v1"], &[], websocket).is_err());
            }
        }
        for server in [
            "https://example.com/old?secret=unused#fragment",
            "wss://example.com",
        ] {
            assert_eq!(
                endpoint_url(
                    server,
                    &["v1", "sessions", "a/b"],
                    &[("role", "client")],
                    true
                )
                .unwrap()
                .as_str(),
                "wss://example.com/v1/sessions/a%2Fb?role=client"
            );
        }
        assert_eq!(
            endpoint_url("https://example.com", &["v1"], &[], false)
                .unwrap()
                .as_str(),
            "https://example.com/v1"
        );
        assert!(endpoint_url("wss://example.com", &["v1"], &[], false).is_err());
    }

    #[tokio::test]
    async fn plaintext_connector_rejects_before_connecting_or_sending_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
        let error = authenticated_websocket(url, "must-not-leave-this-process")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("requires WSS"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn revoked_sessions_stop_retrying_but_network_closures_do_not() {
        use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
        let revoked = signaling_close_error(Some(CloseFrame {
            code: CloseCode::from(4001),
            reason: "expired".into(),
        }));
        assert!(is_terminal_websocket_error(&revoked));
        assert!(!is_terminal_websocket_error(&signaling_close_error(None)));
    }

    #[test]
    fn reconnect_backoff_grows_and_caps() {
        let mut backoff = ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(4));
        assert_eq!(backoff.next_delay(), Duration::from_secs(8));
        assert_eq!(backoff.next_delay(), Duration::from_secs(15));
        assert_eq!(backoff.next_delay(), Duration::from_secs(15));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn only_a_stable_attempt_restarts_the_backoff() {
        let seconds = Duration::from_secs;
        let mut backoff = ReconnectBackoff::new(seconds(1), seconds(15));
        // Never streamed, or failed 5 s after each connect: keeps growing.
        assert_eq!(backoff.delay_after(None), seconds(1));
        assert_eq!(backoff.delay_after(Some(seconds(5))), seconds(2));
        assert_eq!(backoff.delay_after(Some(seconds(5))), seconds(4));
        assert_eq!(backoff.delay_after(Some(seconds(19))), seconds(8));
        assert_eq!(backoff.delay_after(None), seconds(15));
        assert_eq!(backoff.delay_after(Some(seconds(5))), seconds(15));
        // A stable attempt restarts the schedule, which then grows again.
        assert_eq!(backoff.delay_after(Some(STABLE_CONNECTION)), seconds(1));
        assert_eq!(backoff.delay_after(Some(seconds(5))), seconds(2));
        assert_eq!(backoff.delay_after(None), seconds(4));
        assert_eq!(backoff.delay_after(Some(seconds(3600))), seconds(1));
    }

    #[test]
    fn jitter_stays_in_the_upper_half_of_the_delay() {
        let delay = Duration::from_secs(120);
        assert_eq!(scaled(delay, 0.0), Duration::from_secs(60));
        assert_eq!(scaled(delay, 0.5), Duration::from_secs(90));
        assert_eq!(scaled(delay, 1.0), delay);
        // Out-of-range fractions are clamped rather than escaping the bounds.
        assert_eq!(scaled(delay, -1.0), Duration::from_secs(60));
        assert_eq!(scaled(delay, 2.0), delay);
        assert_eq!(scaled(Duration::ZERO, 0.7), Duration::ZERO);
        // The largest fraction `jittered` draws.
        let largest = ((u64::MAX >> 11) as f64) / (1_u64 << 53) as f64;
        assert!(largest < 1.0 && scaled(delay, largest) <= delay);
        for _ in 0..1000 {
            let jittered = jittered(delay);
            assert!((Duration::from_secs(60)..=delay).contains(&jittered));
        }
    }

    #[test]
    fn handshake_status_reads_the_refusal_through_context() {
        use tokio_tungstenite::tungstenite::{Error, http};
        let refused = |status: u16| {
            let response = http::Response::builder().status(status).body(None).unwrap();
            anyhow::Error::new(Error::Http(Box::new(response)))
                .context("signaling WebSocket handshake failed")
        };
        assert_eq!(handshake_status(&refused(401)), Some(401));
        assert!(is_terminal_websocket_error(&refused(404)));
        assert!(!is_terminal_websocket_error(&refused(503)));
        assert_eq!(
            handshake_status(&anyhow::Error::new(Error::ConnectionClosed)),
            None
        );
        assert_eq!(handshake_status(&anyhow::anyhow!("timed out")), None);
    }
}

/// The server closed signaling because the session was ended or revoked.
#[derive(Debug)]
pub struct TerminalClose;
impl std::fmt::Display for TerminalClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "remote session was ended or revoked")
    }
}
impl std::error::Error for TerminalClose {}

/// The server closed this signaling socket because the same peer connected
/// another one.
#[derive(Debug)]
pub struct SupersededClose;
impl std::fmt::Display for SupersededClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "signaling connection was superseded by a newer one")
    }
}
impl std::error::Error for SupersededClose {}

pub fn signaling_close_error(
    frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> anyhow::Error {
    match frame.as_ref().map(|frame| u16::from(frame.code)) {
        Some(1008 | 4001) => TerminalClose.into(),
        Some(4000) => SupersededClose.into(),
        _ => anyhow::anyhow!("signaling connection closed: {frame:?}"),
    }
}

mod connection;
mod session;
pub use connection::SignalingConnection;
pub use session::SessionSignaling;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tls;
