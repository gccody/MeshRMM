use anyhow::{Context, bail};
use pulsermm_protocol::SessionBootstrap;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};
use url::Url;

use crate::config::Config;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn create_session(config: &Config) -> anyhow::Result<SessionBootstrap> {
    let url = endpoint_url(
        config.server.as_str(),
        &["v1", "remote", "handoffs", "redeem"],
        &[],
        false,
    )?;
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(&config.handoff_token)
        .send()
        .await
        .context("Cloudflare session API request failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("session API returned HTTP {status}: {body}");
    }
    response
        .json()
        .await
        .context("session API returned an invalid bootstrap response")
}

pub fn session_signal_url(server: &str, session_id: &str) -> anyhow::Result<Url> {
    endpoint_url(
        server,
        &["v1", "remote", "sessions", session_id, "signal"],
        &[("role", "client")],
        true,
    )
}

fn endpoint_url(
    server: &str,
    segments: &[&str],
    query: &[(&str, &str)],
    websocket: bool,
) -> anyhow::Result<Url> {
    let mut url = Url::parse(server).context("PULSERMM_SERVER is not a valid URL")?;
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
        bail!("session API URL must use HTTP or HTTPS");
    }
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("server URL cannot be a base URL"))?;
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

pub async fn authenticated_websocket(url: Url, token: &str) -> anyhow::Result<Socket> {
    let mut request = url
        .as_str()
        .into_client_request()
        .context("failed to build signaling WebSocket request")?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .context("invalid authentication token header")?,
    );
    let (socket, _) = connect_async(request)
        .await
        .context("signaling WebSocket handshake failed")?;
    Ok(socket)
}
