//! Shared URL construction and authenticated WebSocket connection support.

#![forbid(unsafe_code)]

use anyhow::{Context, bail};
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
