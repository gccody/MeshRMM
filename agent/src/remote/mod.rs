#[cfg(windows)]
pub(crate) mod capture_helper;
#[cfg(windows)]
mod clipboard;
pub mod config;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod signaling;
#[cfg(windows)]
mod transport;
#[cfg(windows)]
mod video;

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use anyhow::Context;
#[cfg(windows)]
use futures_util::{SinkExt, StreamExt};
#[cfg(windows)]
use meshrmm_protocol::{AgentCommand, AgentSessionRequest, AgentStatusMessage};
#[cfg(windows)]
use tokio::time::sleep;
#[cfg(windows)]
use tokio_tungstenite::tungstenite::Message;

use self::config::{Config, ExecutionMode};
#[cfg(windows)]
use self::signaling::{agent_connection_url, authenticated_websocket};

#[cfg_attr(not(windows), allow(unused_variables))]
pub async fn run(config: Config, mode: ExecutionMode) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    anyhow::bail!("the first MeshRMM remote-screen MVP requires Windows");

    #[cfg(windows)]
    {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let url = agent_connection_url(&config.server, &config.device_id)?;
            tracing::info!(device_id = %config.device_id, url = %url, "connecting Agent to Cloudflare signaling");
            match authenticated_websocket(url, &config.agent_token).await {
                Ok((mut socket, _response)) => {
                    retry_delay = Duration::from_secs(1);
                    tracing::info!(device_id = %config.device_id, "Agent signaling connected");
                    let connection_result: anyhow::Result<bool> = async {
                        loop {
                            tokio::select! {
                                message = socket.next() => {
                                    let Some(message) = message else { break Ok(false); };
                                    match message.context("Agent signaling WebSocket read failed")? {
                                        Message::Text(text) => {
                                            if let Ok(AgentCommand::Uninstall) = serde_json::from_str(text.as_str()) {
                                                crate::installer::schedule_uninstall()
                                                    .context("failed to schedule Agent self-uninstall")?;
                                                socket
                                                    .send(Message::Text(
                                                        serde_json::to_string(&AgentStatusMessage::UninstallScheduled)?
                                                            .into(),
                                                    ))
                                                    .await
                                                    .context("failed to acknowledge Agent self-uninstall")?;
                                                socket.flush().await.context(
                                                    "failed to flush Agent self-uninstall acknowledgement",
                                                )?;
                                                sleep(Duration::from_millis(250)).await;
                                                break Ok(true);
                                            }
                                            let request: AgentSessionRequest = match serde_json::from_str(text.as_str()) {
                                                Ok(request) => request,
                                                Err(error) => {
                                                    tracing::warn!(error = %error, "discarding invalid remote-session request");
                                                    continue;
                                                }
                                            };
                                            // The MVP intentionally permits one viewer/session at a time.
                                            if let Err(error) = session::run(&config, request, mode).await {
                                                tracing::error!(error = ?error, "remote session ended with an error");
                                            }
                                        }
                                        Message::Ping(payload) => socket
                                            .send(Message::Pong(payload))
                                            .await
                                            .context("failed to answer signaling ping")?,
                                        Message::Close(_) => break Ok(false),
                                        _ => {}
                                    }
                                }
                                _ = tokio::signal::ctrl_c() => break Ok(true),
                            }
                        }
                    }.await;
                    match connection_result {
                        Ok(true) => return Ok(()),
                        Ok(false) => tracing::warn!("Agent signaling disconnected"),
                        Err(error) => {
                            tracing::warn!(error = %error, "Agent signaling connection ended with an error")
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, retry_seconds = retry_delay.as_secs(), "Agent signaling connection failed");
                }
            }
            tokio::select! {
                _ = sleep(retry_delay) => {},
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
            retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
        }
    }
}
