use anyhow::{Context, bail};
use meshrmm_protocol::SessionBootstrap;
use meshrmm_signaling_client::{Socket, endpoint_url};
use url::Url;

use crate::config::Config;

pub async fn create_session(config: &Config) -> anyhow::Result<SessionBootstrap> {
    let url = endpoint_url(
        config.server.as_str(),
        &["v1", "remote", "handoffs", "redeem"],
        &[],
        false,
    )?;
    let response = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()?
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

pub async fn resume_session(
    config: &Config,
    bootstrap: &SessionBootstrap,
) -> anyhow::Result<SessionBootstrap> {
    let url = endpoint_url(
        config.server.as_str(),
        &[
            "v1",
            "remote",
            "sessions",
            bootstrap.session_id.as_str(),
            "resume",
        ],
        &[],
        false,
    )?;
    reqwest::Client::builder()
        .https_only(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build the session resume client")?
        .post(url)
        .bearer_auth(&bootstrap.signaling_token)
        .send()
        .await
        .context("session resume API request failed")?
        .error_for_status()
        .context("session resume API rejected the session")?
        .json()
        .await
        .context("session resume API returned an invalid bootstrap response")
}

pub fn is_terminal_session_error(error: &anyhow::Error) -> bool {
    meshrmm_signaling_client::is_terminal_websocket_error(error)
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
                .is_some_and(|status| matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410))
        })
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
    let (socket, _) = meshrmm_signaling_client::authenticated_websocket(url, token).await?;
    Ok(socket)
}

/// End via a separate authenticated request even if the signaling socket died.
/// The server acknowledges only after releasing the device lease.
pub async fn end_session(config: &Config, bootstrap: &SessionBootstrap) -> anyhow::Result<()> {
    let url = endpoint_url(
        config.server.as_str(),
        &[
            "v1",
            "remote",
            "sessions",
            bootstrap.session_id.as_str(),
            "end",
        ],
        &[],
        false,
    )?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let mut last_error = None;
    for attempt in 0..3 {
        match client
            .post(url.clone())
            .bearer_auth(&bootstrap.signaling_token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                let terminal = error.status().is_some_and(|status| {
                    status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                });
                last_error = Some(error);
                if terminal {
                    break;
                }
            }
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    Err(last_error.expect("at least one close attempt"))
        .context("server did not acknowledge session cleanup")
}
