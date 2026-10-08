#[cfg(any(windows, target_os = "macos", test))]
mod annotation;
#[cfg(any(windows, target_os = "macos", test))]
mod audio_mode;
#[cfg(windows)]
mod background;
#[cfg(windows)]
pub(crate) mod background_files;
#[cfg(windows)]
pub(crate) mod background_tasks;
#[cfg(any(windows, target_os = "macos", test))]
mod bitrate;
#[cfg(windows)]
mod blackout;
#[cfg(windows)]
pub(crate) mod capture_helper;
#[cfg(windows)]
mod clipboard;
pub mod config;
#[cfg(any(windows, target_os = "macos", test))]
mod connection_approval;
#[cfg(any(windows, target_os = "macos", test))]
mod connection_notification;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod display_border;
#[cfg(windows)]
mod drag_windows;
#[cfg(windows)]
mod indicator;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod input_block;
#[cfg(windows)]
mod keep_awake;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, target_os = "macos", test))]
mod native_task;
#[cfg(any(windows, target_os = "macos"))]
mod platform;
#[cfg(any(windows, target_os = "macos", test))]
mod secure_attention;
#[cfg(any(windows, target_os = "macos", test))]
mod sender_failure;
#[cfg(any(windows, target_os = "macos", test))]
mod sender_progress;
#[cfg(any(windows, target_os = "macos"))]
pub(crate) mod service_link;
#[cfg(any(windows, target_os = "macos"))]
mod session;
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) mod session_close;
#[cfg(any(windows, target_os = "macos"))]
mod signaling;
#[cfg(any(windows, target_os = "macos", test))]
mod thumbnail;
#[cfg(any(windows, target_os = "macos", test))]
mod toolbox;
#[cfg(any(windows, target_os = "macos"))]
mod transport;
#[cfg(windows)]
pub(crate) mod virtual_display;
// The bitrate controller's tests share the encoded-frame queue bound.
#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod video;
#[cfg(windows)]
pub(crate) mod wallpaper;

#[cfg(any(windows, target_os = "macos"))]
use std::time::Duration;

#[cfg(any(windows, target_os = "macos"))]
use anyhow::Context;
#[cfg(any(windows, target_os = "macos"))]
use meshrmm_protocol::{AgentCommand, AgentSessionRequest, AgentStatusMessage};
#[cfg(any(windows, target_os = "macos"))]
use tokio::time::sleep;
#[cfg(any(windows, target_os = "macos"))]
use tokio_tungstenite::tungstenite::Message;

use self::config::{Config, ExecutionMode};
#[cfg(any(windows, target_os = "macos"))]
use self::signaling::{agent_connection_url, authenticated_websocket};

#[cfg(any(windows, target_os = "macos"))]
struct ActiveSession {
    session_id: meshrmm_protocol::RemoteSessionId,
    request: AgentSessionRequest,
    task: tokio::task::JoinHandle<()>,
}

/// Whether `request` repeats the session the Agent is running, as the coordinator does when the
/// Agent reconnects. Servers that renew the session's lease rewrite the expiry of the request they
/// replay, so the expiry is ignored. A resume carries new TURN credentials, so it still restarts
/// the session.
#[cfg(any(windows, target_os = "macos", test))]
fn replays_session(
    active: &meshrmm_protocol::AgentSessionRequest,
    request: &meshrmm_protocol::AgentSessionRequest,
) -> bool {
    *active
        == meshrmm_protocol::AgentSessionRequest {
            expires_at_unix_ms: active.expires_at_unix_ms,
            ..request.clone()
        }
}

/// Ends the remote session and runs its close actions before the coordinator exits, since the
/// session cannot outlive it and nothing else would run them.
/// Tells the server the Agent is going offline to install `version`, so the dashboard shows
/// the update instead of an unexplained outage. The stop goes ahead if this fails.
#[cfg(any(windows, target_os = "macos"))]
async fn announce_update(socket: &meshrmm_signaling_client::SignalingConnection, version: String) {
    let status = AgentStatusMessage::Updating { version };
    let sent = match serde_json::to_string(&status) {
        Ok(text) => socket.send(Message::Text(text.into())).await,
        Err(error) => Err(error.into()),
    };
    match sent {
        Ok(()) => tracing::info!(?status, "told the server the Agent is stopping to update"),
        Err(error) => {
            tracing::warn!(error = %error, "could not tell the server the Agent is stopping to update")
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
async fn end_sessions(
    active_session: &mut Option<ActiveSession>,
    session_close: &mut Option<(
        meshrmm_protocol::RemoteSessionId,
        std::sync::Arc<session_close::SessionClose>,
    )>,
) {
    if let Some(active) = active_session.take() {
        active.task.abort();
        let _ = active.task.await;
        tracing::info!(session_id = %active.session_id, "stopped remote session because the Agent is stopping");
    }
    if let Some((id, close)) = session_close.take() {
        close.finish(&id).await;
    }
}

/// Entry point of `--session-helper`, which launchd runs in each graphical
/// session of an installed macOS Agent.
#[cfg(target_os = "macos")]
pub fn run_session_helper() -> anyhow::Result<()> {
    crate::logging::initialize_helper("agent-session-helper.log")?;
    macos::helper::host::run(&macos::helper::socket_path())
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(unused_variables))]
pub async fn run(
    #[allow(unused_mut)] mut config: Config,
    mode: ExecutionMode,
) -> anyhow::Result<()> {
    #[cfg(not(any(windows, target_os = "macos")))]
    anyhow::bail!("the MeshRMM Agent requires Windows or macOS");

    #[cfg(any(windows, target_os = "macos"))]
    {
        connection_approval::restore_after_restart();
        #[cfg(target_os = "macos")]
        if mode == ExecutionMode::Service {
            macos::helper::coordinator::listen(&macos::helper::socket_path())?;
        }
        let link = std::sync::Arc::new(service_link::ServiceLink::new(
            mode == ExecutionMode::Worker,
        ));
        #[cfg(target_os = "macos")]
        if mode == ExecutionMode::Service {
            tokio::spawn(crate::macos_updater::run(
                config.clone(),
                std::sync::Arc::clone(&link),
            ));
        }
        // Created once, so reconnecting does not capture again before the interval ends.
        let mut thumbnails = thumbnail::Thumbnails::new(mode);
        let mut retry_delay = Duration::from_secs(1);
        let mut active_session = None::<ActiveSession>;
        // Outlives session tasks, which end whenever the viewer drops its
        // connection, so the close action runs only once the session ends.
        let mut session_close = None::<(
            meshrmm_protocol::RemoteSessionId,
            std::sync::Arc<session_close::SessionClose>,
        )>;
        loop {
            let url = agent_connection_url(&config.server, &config.device_id)?;
            tracing::info!(device_id = %config.device_id, url = %url, "connecting Agent to the server");
            match authenticated_websocket(url, &config.agent_token).await {
                Ok((socket, _response)) => {
                    let mut socket = meshrmm_signaling_client::SignalingConnection::new(socket);
                    retry_delay = Duration::from_secs(1);
                    tracing::info!(device_id = %config.device_id, "Agent signaling connected");
                    let connection_result: anyhow::Result<bool> = async {
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
                                    match message.context("Agent signaling WebSocket read failed")? {
                                        Message::Text(text) => {
                                            let background_request = if let Ok(command) = serde_json::from_str::<AgentCommand>(text.as_str()) {
                                                match command {
                                                    AgentCommand::RotateToken { token } => {
                                                        if token == config.agent_token { continue; }
                                                        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                                                            anyhow::bail!("invalid rotated Agent credential");
                                                        }
                                                        let mut stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&config.config_path)?)?;
                                                        stored["agent_token"] = serde_json::Value::String(token.clone());
                                                        crate::installer::replace_file(&config.config_path, &serde_json::to_vec_pretty(&stored)?)?;
                                                        config.agent_token = token;
                                                        break Ok(false);
                                                    }
                                                    AgentCommand::Uninstall => {
                                                        {
                                                        crate::installer::schedule_uninstall()
                                                            .context("failed to schedule Agent self-uninstall")?;
                                                        socket
                                                            .send(Message::Text(
                                                                serde_json::to_string(&AgentStatusMessage::UninstallScheduled)?
                                                                    .into(),
                                                            ))
                                                            .await
                                                            .context("failed to acknowledge Agent self-uninstall")?;
                                                        sleep(Duration::from_millis(250)).await;
                                                        break Ok(true);
                                                        }
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
                                                        if session_close.as_ref().is_some_and(|(id, _)| *id == session_id)
                                                            && let Some((id, close)) = session_close.take()
                                                        {
                                                            close.run(&id);
                                                        }
                                                        continue;
                                                    }
                                                    AgentCommand::StartBackgroundSession { request } => Some(request),
                                                    AgentCommand::RunScript { run } => {
                                                        tracing::info!(run_id = %run.run_id, language = run.language.as_str(), run_as = run.run_as.as_str(), "running a toolbox script");
                                                        toolbox::run_script(&config, mode, run);
                                                        continue;
                                                    }
                                                    AgentCommand::DeliverFile { delivery } => {
                                                        tracing::info!(delivery_id = %delivery.delivery_id, size_bytes = delivery.size_bytes, "receiving a toolbox file");
                                                        toolbox::deliver_file(&config, mode, delivery);
                                                        continue;
                                                    }
                                                }
                                            } else { None };
                                            let request: AgentSessionRequest = match background_request.map(Ok).unwrap_or_else(|| serde_json::from_str(text.as_str())) {
                                                Ok(request) => request,
                                                Err(error) => {
                                                    tracing::warn!(error = %error, "discarding invalid remote-session request");
                                                    continue;
                                                }
                                            };
                                            if active_session.as_ref().is_some_and(|active| {
                                                replays_session(&active.request, &request)
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
                                            if session_close.as_ref().is_some_and(|(id, _)| *id != session_id)
                                                && let Some((id, close)) = session_close.take()
                                            {
                                                close.run(&id);
                                            }
                                            let close = std::sync::Arc::clone(
                                                &session_close
                                                    .get_or_insert_with(|| {
                                                        (
                                                            session_id.clone(),
                                                            std::sync::Arc::new(session_close::SessionClose::new(
                                                                request.clear_clipboard_policy,
                                                            )),
                                                        )
                                                    })
                                                    .1,
                                            );
                                            let active_request = request.clone();
                                            let session_config = config.clone();
                                            let task_session_id = session_id.clone();
                                            let activity = link.session_started();
                                            let task = tokio::spawn(async move {
                                                let _activity = activity;
                                                if let Err(error) = session::run(&session_config, request, mode, close).await {
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
                                () = thumbnails.due() => thumbnails.refresh(&config),
                                () = link.stopped() => {
                                    if let Some(version) = link.update_version() {
                                        announce_update(&socket, version).await;
                                    }
                                    break Ok(true);
                                }
                            }
                        }
                    }.await;
                    match connection_result {
                        Ok(true) => {
                            end_sessions(&mut active_session, &mut session_close).await;
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
                () = link.stopped() => {
                    end_sessions(&mut active_session, &mut session_close).await;
                    return Ok(());
                },
            }
            retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
        }
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{AgentSessionRequest, IceServer, RemoteSessionId};

    use super::replays_session;

    fn request() -> AgentSessionRequest {
        AgentSessionRequest {
            start_in_background: false,
            idle_policy: Default::default(),
            clear_clipboard_policy: Default::default(),
            blackout_message: String::new(),
            session_banner: true,
            connection_notification: true,
            background_connection_notification: false,
            connection_notification_message: String::new(),
            connection_approval: None,
            connection_reason: String::new(),
            viewer_name: "Ada Lovelace".into(),
            session_id: RemoteSessionId::new("session"),
            signaling_token: "token".into(),
            expires_at_unix_ms: 1_000,
            ice_servers: vec![IceServer {
                urls: vec!["turn:rmm.example.com:3478?transport=udp".into()],
                username: Some("first".into()),
                credential: Some("secret".into()),
            }],
        }
    }

    #[test]
    fn renewed_lease_replay_keeps_the_running_session() {
        let active = request();
        let mut replayed = active.clone();
        replayed.expires_at_unix_ms = 31_000;
        assert!(replays_session(&active, &active));
        assert!(replays_session(&active, &replayed));
    }

    #[test]
    fn other_or_resumed_sessions_restart() {
        let active = request();
        let mut other = active.clone();
        other.session_id = RemoteSessionId::new("other");
        let mut token = active.clone();
        token.signaling_token = "other".into();
        let mut resumed = active.clone();
        resumed.ice_servers[0].username = Some("second".into());
        let mut background = active.clone();
        background.start_in_background = true;
        for request in [other, token, resumed, background] {
            assert!(!replays_session(&active, &request));
        }
    }
}
