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
use tokio::time::{Instant, sleep};
#[cfg(windows)]
use tokio_tungstenite::tungstenite::Message;

use self::config::{Config, ExecutionMode};
#[cfg(windows)]
use self::signaling::{agent_connection_url, authenticated_websocket};

#[cfg(windows)]
struct ActiveSession {
    session_id: meshrmm_protocol::RemoteSessionId,
    request: AgentSessionRequest,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(windows)]
const SIGNAL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
#[cfg(windows)]
const SIGNAL_LIVENESS_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg_attr(not(windows), allow(unused_variables))]
pub async fn run(config: Config, mode: ExecutionMode) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    anyhow::bail!("the first MeshRMM remote-screen MVP requires Windows");

    #[cfg(windows)]
    {
        let mut retry_delay = Duration::from_secs(1);
        let mut active_session = None::<ActiveSession>;
        loop {
            let url = agent_connection_url(&config.server, &config.device_id)?;
            tracing::info!(device_id = %config.device_id, url = %url, "connecting Agent to Cloudflare signaling");
            match authenticated_websocket(url, &config.agent_token).await {
                Ok((mut socket, _response)) => {
                    retry_delay = Duration::from_secs(1);
                    tracing::info!(device_id = %config.device_id, "Agent signaling connected");
                    let connection_result: anyhow::Result<bool> = async {
                        let mut heartbeat = tokio::time::interval(SIGNAL_HEARTBEAT_INTERVAL);
                        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        heartbeat.tick().await;
                        let mut last_server_message = Instant::now();
                        loop {
                            if active_session
                                .as_ref()
                                .is_some_and(|session| session.task.is_finished())
                                && let Some(session) = active_session.take()
                            {
                                let _ = session.task.await;
                            }
                            tokio::select! {
                                message = socket.next() => {
                                    let Some(message) = message else { break Ok(false); };
                                    last_server_message = Instant::now();
                                    match message.context("Agent signaling WebSocket read failed")? {
                                        Message::Text(text) => {
                                            if let Ok(command) = serde_json::from_str::<AgentCommand>(text.as_str()) {
                                                match command {
                                                    AgentCommand::Uninstall => {
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
                                                    AgentCommand::EndSession { session_id } => {
                                                        if active_session.as_ref().is_some_and(
                                                            |active| active.session_id == session_id,
                                                        ) && let Some(active) = active_session.take()
                                                        {
                                                            active.task.abort();
                                                            let _ = active.task.await;
                                                            tracing::info!(%session_id, "stopped expired remote session");
                                                        }
                                                        continue;
                                                    }
                                                }
                                            }
                                            let request: AgentSessionRequest = match serde_json::from_str(text.as_str()) {
                                                Ok(request) => request,
                                                Err(error) => {
                                                    tracing::warn!(error = %error, "discarding invalid remote-session request");
                                                    continue;
                                                }
                                            };
                                            if active_session.as_ref().is_some_and(|active| {
                                                active.request == request
                                                    && !active.task.is_finished()
                                            }) {
                                                tracing::info!(
                                                    session_id = %request.session_id,
                                                    "active remote session request replayed after coordinator reconnect"
                                                );
                                                continue;
                                            }
                                            if let Some(active) = active_session.take() {
                                                tracing::info!(
                                                    previous_session_id = %active.session_id,
                                                    session_id = %request.session_id,
                                                    "replacing active remote session"
                                                );
                                                active.task.abort();
                                                let _ = active.task.await;
                                            }
                                            let session_id = request.session_id.clone();
                                            let active_request = request.clone();
                                            let session_config = config.clone();
                                            let task_session_id = session_id.clone();
                                            let task = tokio::spawn(async move {
                                                if let Err(error) = session::run(&session_config, request, mode).await {
                                                    tracing::error!(
                                                        error = ?error,
                                                        session_id = %task_session_id,
                                                        "remote session ended with an error"
                                                    );
                                                }
                                            });
                                            active_session = Some(ActiveSession {
                                                session_id,
                                                request: active_request,
                                                task,
                                            });
                                        }
                                        Message::Ping(payload) => socket
                                            .send(Message::Pong(payload))
                                            .await
                                            .context("failed to answer signaling ping")?,
                                        Message::Close(_) => break Ok(false),
                                        _ => {}
                                    }
                                }
                                _ = heartbeat.tick() => {
                                    if last_server_message.elapsed() >= SIGNAL_LIVENESS_TIMEOUT {
                                        break Err(anyhow::anyhow!(
                                            "Agent signaling did not respond for {} seconds",
                                            SIGNAL_LIVENESS_TIMEOUT.as_secs()
                                        ));
                                    }
                                    socket
                                        .send(Message::Ping(Default::default()))
                                        .await
                                        .context("failed to send Agent signaling heartbeat")?;
                                }
                                _ = tokio::signal::ctrl_c() => break Ok(true),
                            }
                        }
                    }.await;
                    match connection_result {
                        Ok(true) => {
                            if let Some(active) = active_session.take() {
                                active.task.abort();
                                let _ = active.task.await;
                            }
                            return Ok(());
                        }
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
                _ = tokio::signal::ctrl_c() => {
                    if let Some(active) = active_session.take() {
                        active.task.abort();
                        let _ = active.task.await;
                    }
                    return Ok(());
                },
            }
            retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
        }
    }
}
