pub mod config;
mod platform;
mod session;
mod signaling;
mod transport;
mod video;

use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use pulsermm_protocol::AgentSessionRequest;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;

use self::config::Config;
use self::signaling::{agent_connection_url, authenticated_websocket};

pub async fn run(config: Config) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    anyhow::bail!("the first PulseRMM remote-screen MVP requires Windows");

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
                                            let request: AgentSessionRequest = match serde_json::from_str(text.as_str()) {
                                                Ok(request) => request,
                                                Err(error) => {
                                                    tracing::warn!(error = %error, "discarding invalid remote-session request");
                                                    continue;
                                                }
                                            };
                                            // The MVP intentionally permits one viewer/session at a time.
                                            if let Err(error) = session::run(&config, request).await {
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
