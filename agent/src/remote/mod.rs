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
#[cfg(any(windows, target_os = "macos"))]
pub(crate) mod metrics;
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
use std::ops::ControlFlow;
#[cfg(any(windows, target_os = "macos"))]
use std::time::Duration;

#[cfg(any(windows, target_os = "macos"))]
use anyhow::Context;
#[cfg(any(windows, target_os = "macos"))]
use meshrmm_protocol::{AgentCommand, AgentSessionRequest, AgentStatusMessage, RemoteSessionId};
#[cfg(any(windows, target_os = "macos"))]
use meshrmm_signaling_client::SignalingConnection;
#[cfg(any(windows, target_os = "macos"))]
use tokio::time::sleep;
#[cfg(any(windows, target_os = "macos"))]
use tokio_tungstenite::tungstenite::Message;

use self::config::{Config, ExecutionMode};
#[cfg(any(windows, target_os = "macos"))]
use self::signaling::{agent_connection_url, authenticated_websocket};

#[cfg(any(windows, target_os = "macos"))]
struct ActiveSession {
    session_id: RemoteSessionId,
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

/// Tells the server the Agent is going offline to install `version`, so the dashboard shows
/// the update instead of an unexplained outage. The stop goes ahead if this fails.
#[cfg(any(windows, target_os = "macos"))]
async fn announce_update(socket: &SignalingConnection, version: String) {
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

/// Sends the computer's resource usage without waiting for it to be written.
/// A report that does not fit in the send queue is dropped; the next one
/// follows within seconds.
#[cfg(any(windows, target_os = "macos"))]
fn report_metrics(socket: &SignalingConnection, metrics: meshrmm_protocol::SystemMetrics) {
    let status = AgentStatusMessage::Metrics {
        metrics: metrics.sanitized(),
    };
    let queued = match serde_json::to_string(&status) {
        Ok(text) => socket.queue(Message::Text(text.into())),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = queued {
        tracing::debug!(error = %error, "dropped a resource usage report");
    }
}

#[cfg(any(windows, target_os = "macos"))]
async fn uninstall(socket: &SignalingConnection) -> anyhow::Result<ControlFlow<bool>> {
    crate::installer::schedule_uninstall().context("failed to schedule Agent self-uninstall")?;
    socket
        .send(Message::Text(
            serde_json::to_string(&AgentStatusMessage::UninstallScheduled)?.into(),
        ))
        .await
        .context("failed to acknowledge Agent self-uninstall")?;
    sleep(Duration::from_millis(250)).await;
    Ok(ControlFlow::Break(true))
}

/// Entry point of `--session-helper`, which launchd runs in each graphical
/// session of an installed macOS Agent.
#[cfg(target_os = "macos")]
pub fn run_session_helper() -> anyhow::Result<()> {
    crate::logging::initialize_helper("agent-session-helper.log")?;
    macos::helper::host::run(&macos::helper::socket_path())
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(unused_variables))]
pub async fn run(config: Config, mode: ExecutionMode) -> anyhow::Result<()> {
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
        Coordinator {
            // Created once, so reconnecting does not capture again before the interval ends.
            thumbnails: thumbnail::Thumbnails::new(mode),
            // Likewise, so the first sample after a reconnect still measures
            // from the previous one.
            metrics: metrics::Reports::new(),
            config,
            mode,
            link,
            active_session: None,
            session_close: None,
        }
        .run()
        .await
    }
}

/// The Agent's connection to the server, and the remote session it runs.
#[cfg(any(windows, target_os = "macos"))]
struct Coordinator {
    config: Config,
    mode: ExecutionMode,
    link: std::sync::Arc<service_link::ServiceLink>,
    thumbnails: thumbnail::Thumbnails,
    metrics: metrics::Reports,
    active_session: Option<ActiveSession>,
    // Outlives session tasks, which end whenever the viewer drops its
    // connection, so the close action runs only once the session ends.
    session_close: Option<(RemoteSessionId, std::sync::Arc<session_close::SessionClose>)>,
}

#[cfg(any(windows, target_os = "macos"))]
impl Coordinator {
    async fn run(mut self) -> anyhow::Result<()> {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let url = agent_connection_url(&self.config.server, &self.config.device_id)?;
            tracing::info!(device_id = %self.config.device_id, url = %url, "connecting Agent to the server");
            match authenticated_websocket(url, &self.config.agent_token).await {
                Ok((socket, _response)) => {
                    let mut socket = SignalingConnection::new(socket);
                    retry_delay = Duration::from_secs(1);
                    tracing::info!(device_id = %self.config.device_id, "Agent signaling connected");
                    match self.serve(&mut socket).await {
                        Ok(true) => {
                            self.end_sessions().await;
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
                () = self.link.stopped() => {
                    self.end_sessions().await;
                    return Ok(());
                },
            }
            retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
        }
    }

    /// Handles the server's messages until the connection ends, returning whether the Agent is
    /// stopping.
    async fn serve(&mut self, socket: &mut SignalingConnection) -> anyhow::Result<bool> {
        loop {
            if self
                .active_session
                .as_ref()
                .is_some_and(|session| session.task.is_finished())
                && let Some(session) = self.active_session.take()
            {
                let _ = session.task.await;
            }
            tokio::select! {
                message = socket.next() => {
                    let Some(message) = message else { break Ok(false); };
                    match message.context("Agent signaling WebSocket read failed")? {
                        Message::Text(text) => {
                            if let ControlFlow::Break(stopping) = self.handle_text(socket, text.as_str()).await? {
                                break Ok(stopping);
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
                () = self.thumbnails.due() => self.thumbnails.refresh(&self.config),
                metrics = self.metrics.next() => report_metrics(socket, metrics),
                () = self.link.stopped() => {
                    if let Some(version) = self.link.update_version() {
                        announce_update(socket, version).await;
                    }
                    break Ok(true);
                }
            }
        }
    }

    /// Breaks, with whether the Agent is stopping, when the connection must end.
    async fn handle_text(
        &mut self,
        socket: &SignalingConnection,
        text: &str,
    ) -> anyhow::Result<ControlFlow<bool>> {
        let background_request = if let Ok(command) = serde_json::from_str::<AgentCommand>(text) {
            match command {
                AgentCommand::RotateToken { token } => return self.rotate_token(token),
                AgentCommand::Uninstall => return uninstall(socket).await,
                AgentCommand::EndSession { session_id } => {
                    self.end_session(session_id).await;
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::StartBackgroundSession { request } => Some(request),
                AgentCommand::RunScript { run } => {
                    tracing::info!(run_id = %run.run_id, language = run.language.as_str(), run_as = run.run_as.as_str(), "running a toolbox script");
                    toolbox::run_script(&self.config, self.mode, run);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::DeliverFile { delivery } => {
                    tracing::info!(delivery_id = %delivery.delivery_id, size_bytes = delivery.size_bytes, "receiving a toolbox file");
                    toolbox::deliver_file(&self.config, self.mode, delivery);
                    return Ok(ControlFlow::Continue(()));
                }
            }
        } else {
            None
        };
        let request: AgentSessionRequest = match background_request
            .map(Ok)
            .unwrap_or_else(|| serde_json::from_str(text))
        {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(error = %error, "discarding invalid remote-session request");
                return Ok(ControlFlow::Continue(()));
            }
        };
        self.start_session(request).await;
        Ok(ControlFlow::Continue(()))
    }

    fn rotate_token(&mut self, token: String) -> anyhow::Result<ControlFlow<bool>> {
        if token == self.config.agent_token {
            return Ok(ControlFlow::Continue(()));
        }
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("invalid rotated Agent credential");
        }
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&self.config.config_path)?)?;
        stored["agent_token"] = serde_json::Value::String(token.clone());
        crate::installer::replace_file(
            &self.config.config_path,
            &serde_json::to_vec_pretty(&stored)?,
        )?;
        self.config.agent_token = token;
        Ok(ControlFlow::Break(false))
    }

    async fn end_session(&mut self, session_id: RemoteSessionId) {
        if self
            .active_session
            .as_ref()
            .is_some_and(|active| active.session_id == session_id)
            && let Some(active) = self.active_session.take()
        {
            active.task.abort();
            let _ = active.task.await;
            tracing::info!(%session_id, "stopped expired remote session");
        }
        if self
            .session_close
            .as_ref()
            .is_some_and(|(id, _)| *id == session_id)
            && let Some((id, close)) = self.session_close.take()
        {
            close.run(&id);
        }
    }

    async fn start_session(&mut self, request: AgentSessionRequest) {
        if self.active_session.as_ref().is_some_and(|active| {
            replays_session(&active.request, &request) && !active.task.is_finished()
        }) {
            tracing::info!(
                session_id = %request.session_id,
                "active remote session request replayed after coordinator reconnect"
            );
            return;
        }
        if let Some(active) = self.active_session.take() {
            tracing::info!(
                previous_session_id = %active.session_id,
                session_id = %request.session_id,
                "replacing active remote session"
            );
            active.task.abort();
            let _ = active.task.await;
        }
        let session_id = request.session_id.clone();
        let close = self.session_close_for(&request);
        let active_request = request.clone();
        let session_config = self.config.clone();
        let task_session_id = session_id.clone();
        let activity = self.link.session_started();
        let mode = self.mode;
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
        self.active_session = Some(ActiveSession {
            session_id,
            request: active_request,
            task,
        });
    }

    /// Runs the close action of any other session, and returns the close state `request` shares
    /// with earlier connections of its session.
    fn session_close_for(
        &mut self,
        request: &AgentSessionRequest,
    ) -> std::sync::Arc<session_close::SessionClose> {
        if self
            .session_close
            .as_ref()
            .is_some_and(|(id, _)| *id != request.session_id)
            && let Some((id, close)) = self.session_close.take()
        {
            close.run(&id);
        }
        std::sync::Arc::clone(
            &self
                .session_close
                .get_or_insert_with(|| {
                    (
                        request.session_id.clone(),
                        std::sync::Arc::new(session_close::SessionClose::new(
                            request.clear_clipboard_policy,
                        )),
                    )
                })
                .1,
        )
    }

    /// Ends the remote session and runs its close actions before the coordinator exits, since the
    /// session cannot outlive it and nothing else would run them.
    async fn end_sessions(&mut self) {
        if let Some(active) = self.active_session.take() {
            active.task.abort();
            let _ = active.task.await;
            tracing::info!(session_id = %active.session_id, "stopped remote session because the Agent is stopping");
        }
        if let Some((id, close)) = self.session_close.take() {
            close.finish(&id).await;
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
