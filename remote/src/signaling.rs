use anyhow::{Context, bail};
use pulsermm_protocol::SessionBootstrap;
use pulsermm_signaling_client::{Socket, endpoint_url};
use url::Url;

use crate::config::Config;

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

pub async fn authenticated_websocket(url: Url, token: &str) -> anyhow::Result<Socket> {
    let (socket, _) = pulsermm_signaling_client::authenticated_websocket(url, token).await?;
    Ok(socket)
}
