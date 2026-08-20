use anyhow::{Context, bail};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};
use url::Url;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub fn agent_connection_url(server: &str, device_id: &str) -> anyhow::Result<Url> {
    endpoint_url(server, &["v1", "agents", device_id, "connect"], &[])
}

pub fn session_signal_url(server: &str, session_id: &str, role: &str) -> anyhow::Result<Url> {
    endpoint_url(
        server,
        &["v1", "remote", "sessions", session_id, "signal"],
        &[("role", role)],
    )
}

fn endpoint_url(server: &str, segments: &[&str], query: &[(&str, &str)]) -> anyhow::Result<Url> {
    let mut url = Url::parse(server).context("PULSERMM_SERVER is not a valid URL")?;
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
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("signaling URL cannot be a base URL"))?;
        path.clear();
        for segment in segments {
            path.push(segment);
        }
    }
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query.iter().copied());
    }
    Ok(url)
}

pub async fn authenticated_websocket(
    url: Url,
    token: &str,
) -> anyhow::Result<(
    Socket,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
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
