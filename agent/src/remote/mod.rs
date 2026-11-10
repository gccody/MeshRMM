#[cfg(any(windows, target_os = "macos", test))]
mod annotation;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod applications;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod audio_mode;
#[cfg(windows)]
mod background;
#[cfg(windows)]
pub(crate) mod background_files;
#[cfg(windows)]
pub(crate) mod background_tasks;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod bitrate;
#[cfg(windows)]
mod blackout;
#[cfg(windows)]
pub(crate) mod capture_helper;
#[cfg(windows)]
mod clipboard;
pub mod config;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod connection_approval;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod connection_notification;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod display_border;
#[cfg(windows)]
mod drag_windows;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod files;
#[cfg(unix)]
pub(crate) mod helper;
#[cfg(windows)]
mod indicator;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod input_block;
#[cfg(windows)]
mod keep_awake;
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) mod metrics;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod monitors;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod native_task;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod platform;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod process;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod secure_attention;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod sender_failure;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod sender_progress;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) mod service_link;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod session;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
pub(crate) mod session_close;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod signaling;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) mod system_info;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) mod terminal;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod thumbnail;
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
mod toolbox;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod transport;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod unsent;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod updates;
#[cfg(unix)]
pub(crate) mod user_database;
#[cfg(windows)]
pub(crate) mod virtual_display;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod wake;
// The bitrate controller's tests share the encoded-frame queue bound.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod video;
#[cfg(windows)]
pub(crate) mod wallpaper;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::ops::ControlFlow;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::time::{Duration, Instant};

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use anyhow::Context;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use meshrmm_protocol::{AgentCommand, AgentSessionRequest, AgentStatusMessage, RemoteSessionId};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use meshrmm_signaling_client::{SignalingConnection, jittered};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use tokio::time::sleep;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use tokio_tungstenite::tungstenite::Message;

use self::config::{Config, ExecutionMode};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use self::signaling::{
    ControlAttempt, ControlReconnect, agent_connection_url, authenticated_websocket,
};

/// An update or application report the connection is writing. Each source
/// has one at a time, so its reports keep their order, and the one a
/// connection doesn't get through is the one kept for the next.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct Writing {
    report: unsent::Report,
    written: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Writing {
    /// Hands `report` to `socket`, which may take minutes to write a large
    /// one on a slow link, so the coordinator waits for it alongside the
    /// server's commands rather than before them.
    fn start(socket: &SignalingConnection, report: unsent::Report) -> Self {
        let written: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> =
            match serde_json::to_string(&report.message) {
                Ok(text) => Box::pin(socket.sending(Message::Text(text.into()))),
                Err(error) => Box::pin(std::future::ready(Err(error.into()))),
            };
        Self { report, written }
    }
}

/// Completes when the report `writing` holds is written or has failed;
/// never without one.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
async fn written(writing: &mut Option<Writing>) -> anyhow::Result<()> {
    match writing {
        Some(writing) => (&mut writing.written).await,
        None => std::future::pending().await,
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct ActiveSession {
    session_id: RemoteSessionId,
    request: AgentSessionRequest,
    task: tokio::task::JoinHandle<()>,
}

/// Whether `request` repeats the session the Agent is running, as the coordinator does when the
/// Agent reconnects. Servers that renew the session's lease rewrite the expiry of the request they
/// replay, so the expiry is ignored. A resume carries new TURN credentials, so it still restarts
/// the session.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
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
/// the update instead of an unexplained outage. The stop goes ahead if this fails, or if a large
/// report ahead of it keeps it waiting.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
async fn announce_update(socket: &SignalingConnection, version: String) {
    let status = AgentStatusMessage::Updating { version };
    let sent = match serde_json::to_string(&status) {
        Ok(text) => tokio::time::timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(text.into())),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out behind other messages"))),
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
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
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

/// Reports what became of the server's wake packets, without waiting for it
/// to be written. A report that can't be queued is lost; the server gives
/// up on it after a few seconds.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn report_wake_results(
    socket: &SignalingConnection,
    results: Vec<meshrmm_protocol::WakePacketResult>,
) {
    let status = AgentStatusMessage::WakePacketsSent { results };
    let queued = match serde_json::to_string(&status) {
        Ok(text) => socket.queue(Message::Text(text.into())),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = queued {
        tracing::warn!(error = %error, "could not report the wake packets sent");
    }
}

/// Sends monitor results and self-healing outcomes without waiting for them
/// to be written. Those that can't be queued go again.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn report_monitors(
    socket: &SignalingConnection,
    monitors: &monitors::Monitors,
    messages: Vec<AgentStatusMessage>,
) {
    let mut unsent = Vec::new();
    for message in messages {
        let queued = match serde_json::to_string(&message) {
            Ok(text) => socket.queue(Message::Text(text.into())),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = queued {
            tracing::warn!(error = %error, "could not report monitor results");
            unsent.push(message);
        }
    }
    if !unsent.is_empty() {
        monitors.unsent(unsent);
    }
}

/// Sends a file operation's answer without waiting for it to be written.
/// One that can't be queued is lost; the server gives up on it.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn report_file_reply(
    socket: &SignalingConnection,
    request_id: String,
    reply: meshrmm_protocol::FileReply,
) {
    let status = AgentStatusMessage::FileReply {
        request_id,
        reply: Box::new(reply),
    };
    let queued = match serde_json::to_string(&status) {
        Ok(text) => socket.queue(Message::Text(text.into())),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = queued {
        tracing::warn!(error = %error, "could not answer a file request");
    }
}

/// Sends a system information snapshot without waiting for it to be
/// written, without its applications when they are the list the server
/// holds. One that can't be queued is dropped: the server asks for a new one
/// when the Agent reconnects and the one it has is old.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn report_system_info(
    socket: &SignalingConnection,
    collector: &system_info::Collector,
    system: meshrmm_protocol::SystemInfo,
) {
    let status = collector.report(system);
    let queued = match serde_json::to_string(&status) {
        Ok(text) => socket.queue(Message::Text(text.into())),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = queued {
        tracing::warn!(error = %error, "could not send the system information");
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
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
    helper::host::run(&helper::socket_path())
}

/// Entry point of `--session-helper`, which the installed Linux coordinator
/// starts in each graphical session, as the session's user.
#[cfg(target_os = "linux")]
pub fn run_session_helper() -> anyhow::Result<()> {
    crate::logging::initialize_helper("agent-session-helper.log")?;
    linux::sessions::run_helper()
}

#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(unused_variables)
)]
pub async fn run(config: Config, mode: ExecutionMode) -> anyhow::Result<()> {
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    anyhow::bail!("the MeshRMM Agent requires Windows, macOS or Linux");

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        connection_approval::restore_after_restart();
        #[cfg(target_os = "macos")]
        if mode == ExecutionMode::Service {
            helper::coordinator::listen(&helper::socket_path())?;
        }
        #[cfg(target_os = "linux")]
        if mode == ExecutionMode::Service {
            linux::sessions::supervise()?;
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
        #[cfg(target_os = "linux")]
        if mode == ExecutionMode::Service {
            tokio::spawn(crate::linux_updater::run(
                config.clone(),
                std::sync::Arc::clone(&link),
            ));
        }
        Coordinator {
            // Created once, so reconnecting does not capture again before the
            // interval ends, or send an image the server already has.
            thumbnails: thumbnail::Thumbnails::new(mode),
            // Likewise, so the first sample after a reconnect still measures
            // from the previous one.
            metrics: metrics::Reports::new(),
            system_info: system_info::Collector::new(),
            // Likewise, so a result that arrives while offline is sent on
            // the next connection.
            updates: updates::Updates::new(),
            // Likewise, so a scan that finished offline still counts.
            update_schedule: updates::Schedule::new(
                meshrmm_signaling_client::random_fraction().unwrap_or(0.5),
            ),
            applications: applications::Applications::new(),
            wake: wake::Relay::new(),
            files: files::Files::new(),
            // Likewise, so monitors keep running while the Agent reconnects.
            monitors: monitors::Monitors::new(mode),
            writing_update: None,
            writing_application: None,
            config,
            mode,
            link,
            active_session: None,
            session_close: None,
            connection_recorded: false,
        }
        .run()
        .await
    }
}

/// The Agent's connection to the server, and the remote session it runs.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct Coordinator {
    config: Config,
    mode: ExecutionMode,
    link: std::sync::Arc<service_link::ServiceLink>,
    thumbnails: thumbnail::Thumbnails,
    metrics: metrics::Reports,
    system_info: system_info::Collector,
    updates: updates::Updates,
    update_schedule: updates::Schedule,
    applications: applications::Applications,
    wake: wake::Relay,
    files: files::Files,
    monitors: monitors::Monitors,
    writing_update: Option<Writing>,
    writing_application: Option<Writing>,
    active_session: Option<ActiveSession>,
    // Outlives session tasks, which end whenever the viewer drops its
    // connection, so the close action runs only once the session ends.
    session_close: Option<(RemoteSessionId, std::sync::Arc<session_close::SessionClose>)>,
    /// Whether this process has recorded that the server accepted it, which commits an update
    /// that installed it (see [`crate::update_policy::CONFIRM_TIMEOUT`]).
    connection_recorded: bool,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Coordinator {
    async fn run(mut self) -> anyhow::Result<()> {
        let mut reconnect = ControlReconnect::new();
        loop {
            let url = agent_connection_url(&self.config.server, &self.config.device_id)?;
            tracing::info!(device_id = %self.config.device_id, url = %url, "connecting Agent to the server");
            let delay = match authenticated_websocket(url, &self.config.agent_token).await {
                Ok((socket, _response)) => {
                    let mut socket = SignalingConnection::new(socket);
                    let connected_at = Instant::now();
                    tracing::info!(device_id = %self.config.device_id, "Agent signaling connected");
                    let ended = self.serve(&mut socket).await;
                    if matches!(ended, Ok(true)) {
                        self.end_sessions().await;
                        return Ok(());
                    }
                    let lasted = connected_at.elapsed();
                    let delay = jittered(reconnect.delay_after(ControlAttempt::Connected(lasted)));
                    // Jittered delays are often under a second.
                    let (retry_ms, connected_seconds) =
                        (delay.as_millis() as u64, lasted.as_secs());
                    match ended {
                        Err(error) => {
                            tracing::warn!(error = %error, retry_ms, connected_seconds, "Agent signaling connection ended with an error")
                        }
                        Ok(_) => {
                            tracing::warn!(
                                retry_ms,
                                connected_seconds,
                                "Agent signaling disconnected"
                            )
                        }
                    }
                    delay
                }
                Err(error) => {
                    let attempt = ControlAttempt::failed(&error);
                    let delay = jittered(reconnect.delay_after(attempt));
                    let retry_ms = delay.as_millis() as u64;
                    if matches!(attempt, ControlAttempt::Refused) {
                        tracing::warn!(error = %error, retry_ms, "the server does not recognize this device's credential; retrying less often");
                    } else {
                        tracing::warn!(error = %error, retry_ms, "Agent signaling connection failed");
                    }
                    delay
                }
            };
            tokio::select! {
                _ = sleep(delay) => {},
                () = self.link.stopped() => {
                    self.end_sessions().await;
                    return Ok(());
                },
            }
        }
    }

    /// Once per process, after the server accepted this coordinator and it handled the server's
    /// first message, records that this build works, for the installer that may be waiting to
    /// commit the update that started it. Only the installed Agent's coordinator records it.
    fn record_connection(&mut self) {
        if self.connection_recorded {
            return;
        }
        self.connection_recorded = true;
        #[cfg(windows)]
        let recorded = (self.mode == ExecutionMode::Worker).then(|| {
            self.config
                .config_path
                .parent()
                .context("Agent configuration has no parent directory")
                .and_then(crate::updater::record_connected)
        });
        #[cfg(target_os = "linux")]
        let recorded =
            (self.mode == ExecutionMode::Service).then(crate::linux_updater::record_connected);
        #[cfg(target_os = "macos")]
        let recorded =
            (self.mode == ExecutionMode::Service).then(crate::macos_updater::record_connected);
        match recorded {
            Some(Ok(())) => {
                tracing::info!("recorded that the server accepted this Agent build")
            }
            Some(Err(error)) => tracing::warn!(
                error = ?error,
                "could not record that the server accepted this Agent build; an update that installed it is rolled back"
            ),
            None => {}
        }
    }

    /// Handles the server's messages until the connection ends, returning whether the Agent is
    /// stopping.
    async fn serve(&mut self, socket: &mut SignalingConnection) -> anyhow::Result<bool> {
        let ended = self.serve_messages(socket).await;
        // A report still being written when the connection ended goes first
        // on the next, unless it got through.
        use futures_util::FutureExt;
        if let Some(mut writing) = self.writing_update.take()
            && !matches!((&mut writing.written).now_or_never(), Some(Ok(())))
        {
            self.updates.unsent(writing.report);
        }
        if let Some(mut writing) = self.writing_application.take() {
            match (&mut writing.written).now_or_never() {
                Some(Ok(())) => self.applications.delivered(&writing.report.message),
                _ => self.applications.unsent(writing.report),
            }
        }
        ended
    }

    async fn serve_messages(&mut self, socket: &mut SignalingConnection) -> anyhow::Result<bool> {
        // Before anything the server sends on this connection, so the
        // server knows which application runs from before it ended.
        let activity = serde_json::to_string(&self.applications.activity())?;
        socket
            .send(Message::Text(activity.into()))
            .await
            .context("could not send the application activity")?;
        self.monitors.connected();
        self.thumbnails.connected();
        // The server sends this connection's schedule first thing.
        self.update_schedule.connected();
        // And the application list it holds.
        self.system_info.connected();
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
                            self.record_connection();
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
                system = self.system_info.next() => report_system_info(socket, &self.system_info, system),
                results = self.wake.next() => report_wake_results(socket, results),
                (request_id, reply) = self.files.next() => report_file_reply(socket, request_id, reply),
                messages = self.monitors.next() => report_monitors(socket, &self.monitors, messages),
                () = self.update_schedule.due() => {
                    tracing::info!("starting the scheduled update scan");
                    self.update_schedule.started(updates::now_unix_ms());
                    // A scan already running answers it.
                    self.updates.scan(None, self.link.activity());
                    self.applications.scan(None, self.link.activity());
                }
                report = self.updates.next(), if self.writing_update.is_none() => {
                    if let AgentStatusMessage::UpdateScan { scan, .. } = &report.message {
                        self.update_schedule.scanned(scan, updates::now_unix_ms());
                    }
                    self.writing_update = Some(Writing::start(socket, report));
                }
                result = written(&mut self.writing_update) => {
                    let writing = self.writing_update.take();
                    if let Err(error) = result {
                        if let Some(writing) = writing {
                            self.updates.unsent(writing.report);
                        }
                        return Err(error).context("could not send an update report");
                    }
                }
                report = self.applications.next(), if self.writing_application.is_none() => {
                    // A finished run changed what is installed.
                    if matches!(report.message, AgentStatusMessage::ApplicationResult { .. }) {
                        self.system_info.request();
                    }
                    self.writing_application = Some(Writing::start(socket, report));
                }
                result = written(&mut self.writing_application) => {
                    if let Some(writing) = self.writing_application.take() {
                        if let Err(error) = result {
                            self.applications.unsent(writing.report);
                            return Err(error).context("could not send an application report");
                        }
                        self.applications.delivered(&writing.report.message);
                    }
                }
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
                    toolbox::run_script(&self.config, self.mode, run, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::DeliverFile { delivery } => {
                    tracing::info!(delivery_id = %delivery.delivery_id, size_bytes = delivery.size_bytes, "receiving a toolbox file");
                    toolbox::deliver_file(&self.config, self.mode, delivery, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::CollectSystemInfo => {
                    tracing::info!("the server asked for the system information");
                    self.system_info.request();
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::StoredApplications { applications_hash } => {
                    self.system_info.stored(applications_hash);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::ScanUpdates { run_id } => {
                    tracing::info!(%run_id, "the server asked for an update scan");
                    self.updates.scan(Some(run_id), self.link.activity());
                    // Applications scan on the update scan's cadence.
                    self.applications.scan(None, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::SetUpdateScanSchedule { schedule } => {
                    let now = updates::now_unix_ms();
                    self.update_schedule
                        .set(schedule, now, metrics::booted_at_unix_ms(now));
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::InstallUpdates { install } => {
                    tracing::info!(
                        run_id = %install.run_id,
                        selection = ?install.selection,
                        reboot = install.reboot.as_str(),
                        reboot_delay_minutes = install.reboot_delay_minutes,
                        "the server asked for an update install"
                    );
                    self.updates.install(install, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::OpenTerminal { terminal } => {
                    tracing::info!(
                        terminal_id = %terminal.terminal_id,
                        shell = terminal.shell.as_str(),
                        run_as = terminal.run_as.as_str(),
                        columns = terminal.columns,
                        rows = terminal.rows,
                        "the server asked for a terminal"
                    );
                    // Automatic updates wait for open terminals, as for sessions.
                    let activity = self.link.activity();
                    terminal::open(&self.config, self.mode, terminal, activity);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::SendWakePackets { packets } => {
                    tracing::info!(packets = packets.len(), "the server asked for wake packets");
                    self.wake.send(packets);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::FileRequest { request } => {
                    tracing::debug!(
                        request_id = %request.request_id,
                        operation = request.operation.name(),
                        run_as = request.run_as.as_str(),
                        "the server asked for a file operation"
                    );
                    self.files.request(self.mode, request);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::ScanApplications { run_id } => {
                    tracing::info!(?run_id, "the server asked for an application scan");
                    self.applications.scan(run_id, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::RunApplications { run } => {
                    tracing::info!(run_id = %run.run_id, operation = ?run.operation, "the server asked for an application run");
                    self.applications
                        .run(&self.config, self.mode, run, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::SetMonitors { monitors, known } => {
                    self.monitors.set(monitors, known);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::CheckMonitors { monitor_ids } => {
                    tracing::info!(
                        monitors = monitor_ids.len(),
                        "the server asked for monitor checks"
                    );
                    self.monitors.check_now(monitor_ids);
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::RunMonitorAction { action } => {
                    tracing::info!(
                        action_id = %action.action_id,
                        monitor_id = %action.monitor_id,
                        action = ?action.action,
                        "the server asked for a self-healing action"
                    );
                    self.monitors.act(action, self.link.activity());
                    return Ok(ControlFlow::Continue(()));
                }
                AgentCommand::FileTransfer { transfer } => {
                    tracing::info!(
                        transfer_id = %transfer.transfer_id,
                        run_as = transfer.run_as.as_str(),
                        "the server asked for a file transfer"
                    );
                    // Automatic updates wait for transfers, as for sessions.
                    let activity = self.link.activity();
                    files::transfer::start(&self.config, self.mode, transfer, activity);
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
        let activity = self.link.activity();
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
