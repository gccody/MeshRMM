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
            "http" => url
                .set_scheme("ws")
                .map_err(|_| anyhow::anyhow!("invalid HTTP URL"))?,
            "wss" | "ws" => {}
            scheme => bail!("unsupported signaling URL scheme {scheme}"),
        }
    } else if !matches!(url.scheme(), "https" | "http") {
        bail!("MeshRMM API URL must use HTTP or HTTPS");
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
    let mut request = url
        .as_str()
        .into_client_request()
        .context("failed to build signaling WebSocket request")?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .context("invalid authentication token header")?,
    );
    connect_async(request)
        .await
        .context("signaling WebSocket handshake failed")
}

/// Returns true when retrying a WebSocket with the same credentials cannot
/// succeed because the server rejected or no longer recognizes the session.
pub fn is_terminal_websocket_error(error: &anyhow::Error) -> bool {
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
