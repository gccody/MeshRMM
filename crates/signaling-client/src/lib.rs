//! Shared URL construction and authenticated WebSocket connection support.

#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::client::Response,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};
use url::Url;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
    tokio::time::timeout(Duration::from_secs(15), connect_async(request))
        .await
        .context("signaling WebSocket handshake timed out")?
        .context("signaling WebSocket handshake failed")
}

/// Returns true when retrying a WebSocket with the same credentials cannot
/// succeed because the server rejected or no longer recognizes the session.
pub fn is_terminal_websocket_error(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<TerminalClose>().is_some() {
        return true;
    }
    error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_tungstenite::tungstenite::Error>()
            .and_then(|error| match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    Some(response.status().as_u16())
                }
                _ => None,
            })
            .is_some_and(|status| matches!(status, 400 | 401 | 403 | 404 | 410))
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
}

#[derive(Debug)]
struct TerminalClose;
impl std::fmt::Display for TerminalClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "remote session was ended or revoked")
    }
}
impl std::error::Error for TerminalClose {}

pub fn signaling_close_error(
    frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> anyhow::Error {
    if frame
        .as_ref()
        .is_some_and(|frame| matches!(u16::from(frame.code), 1008 | 4001))
    {
        TerminalClose.into()
    } else {
        anyhow::anyhow!("signaling connection closed: {frame:?}")
    }
}

mod connection;
pub use connection::SignalingConnection;
