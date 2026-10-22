//! One terminal's browser socket: it asks the Agent to open the terminal,
//! waits for the Agent's socket, then relays between the two until either
//! side ends, the terminal sits idle, or its user may no longer have it.
//!
//! Each direction is its own loop over its own socket halves, so a browser
//! that is slow to draw output never holds up its input, and the server
//! holds at most one message per direction. The Agent sends no more output
//! than the browser has acknowledged plus [`TERMINAL_WINDOW_BYTES`], so a
//! flood waits on the device. The server checks that acknowledgements only
//! count output it relayed, and forwards nothing but well-formed messages.
//!
//! The browser's socket is closed with a code that says why:
//!
//! | Code | Why |
//! |---|---|
//! | 1000 | The shell exited or could not start (after the event saying so), or the terminal was closed |
//! | 1003 | The browser sent something a terminal doesn't take |
//! | 4001 | The user's sign-in ended |
//! | 4003 | The user lost the permission the terminal needs |
//! | 4004 | The device is offline or didn't open the terminal in time |
//! | 4008 | Nobody typed for the company's idle time |
//! | 4009 | The device's socket ended |
//!
//! [`TERMINAL_WINDOW_BYTES`]: meshrmm_protocol_types::TERMINAL_WINDOW_BYTES
use std::{
    net::IpAddr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use meshrmm_protocol_types::{
    AgentCommand, MAX_TERMINAL_MESSAGE_BYTES, TerminalControl, TerminalEvent, TerminalRequest,
    valid_terminal_size,
};
use serde_json::json;
use tokio::sync::oneshot;

use super::{Claim, Requested, permission};
use crate::{
    audit::{self, Target},
    auth::{Authorized, session},
    http::AppState,
    realtime::{LIVENESS, SEND_TIMEOUT},
    settings,
};

/// Pings keep proxies from closing a quiet terminal and find dead peers.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A browser that hasn't answered three pings is gone. The Agent gets
/// [`LIVENESS`], like its control connection.
const BROWSER_SILENCE: Duration = Duration::from_secs(75);
/// How often the user's sign-in and permissions are checked again, besides
/// whenever a signed-in user changes something.
const ACCESS_RECHECK: Duration = Duration::from_secs(30);

/// How a terminal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    /// The shell exited, and the browser was told.
    Exited,
    /// The shell couldn't start, and the browser was told.
    Failed,
    /// The browser closed the terminal or went away.
    BrowserLeft,
    /// The browser sent something a terminal doesn't take.
    BrowserProtocol,
    DeviceOffline,
    DeviceTimeout,
    /// The Agent's socket ended, stalled or sent something unexpected.
    DeviceDisconnected,
    Idle(Duration),
    SignedOut,
    PermissionRemoved,
}

impl End {
    /// The audit log's word for it.
    fn reason(&self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::BrowserLeft => "closed",
            Self::BrowserProtocol => "protocol_error",
            Self::DeviceOffline => "device_offline",
            Self::DeviceTimeout => "device_timeout",
            Self::DeviceDisconnected => "device_disconnected",
            Self::Idle(_) => "idle",
            Self::SignedOut => "signed_out",
            Self::PermissionRemoved => "permission_removed",
        }
    }

    /// The close frame for the browser, if it is still there.
    fn browser_close(&self) -> Option<(u16, String)> {
        Some(match self {
            Self::Exited => (1000, "the shell exited".into()),
            Self::Failed => (1000, "the shell couldn't start".into()),
            Self::BrowserLeft => return None,
            Self::BrowserProtocol => (1003, "unsupported terminal message".into()),
            Self::DeviceOffline => (4004, "the device is offline".into()),
            Self::DeviceTimeout => (4004, "the device didn't open the terminal".into()),
            Self::DeviceDisconnected => (4009, "the device disconnected".into()),
            Self::Idle(idle) => (
                4008,
                format!("closed after {} without input", describe(*idle)),
            ),
            Self::SignedOut => (4001, "sign in again".into()),
            Self::PermissionRemoved => (4003, "you may no longer open this terminal".into()),
        })
    }
}

/// `idle` in minutes or hours, as the settings offer it.
fn describe(idle: Duration) -> String {
    let minutes = idle.as_secs() / 60;
    match minutes {
        0 if idle.as_secs() <= 1 => "1 second".into(),
        0 => format!("{} seconds", idle.as_secs()),
        1 => "1 minute".into(),
        60 => "1 hour".into(),
        minutes if minutes % 60 == 0 => format!("{} hours", minutes / 60),
        minutes => format!("{minutes} minutes"),
    }
}

/// What the Agent said about the shell, for the audit log.
#[derive(Debug, Default)]
struct Outcome {
    account: Option<String>,
    program: Option<String>,
    exit_code: Option<i32>,
    error: Option<String>,
}

/// State both directions share. Atomics, since the task must be `Send`.
struct Shared {
    started: Instant,
    /// Output bytes sent to the browser.
    relayed: AtomicU64,
    /// When the user last typed or resized, as milliseconds after `started`.
    last_input_ms: AtomicU64,
    /// Whether the user typed since the sign-in was last checked.
    typed: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn touch(&self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_input_ms.store(elapsed, Ordering::Relaxed);
        self.typed.store(true, Ordering::Relaxed);
    }

    fn last_input(&self) -> Instant {
        self.started + Duration::from_millis(self.last_input_ms.load(Ordering::Relaxed))
    }
}

/// Serves the browser's socket of `requested`, whose user signed in with
/// the session `token` from `ip`. `agent` brings the Agent's socket. The
/// terminal is forgotten when `claim` drops, however this ends.
pub async fn serve(
    state: AppState,
    claim: Claim,
    requested: Requested,
    agent: oneshot::Receiver<WebSocket>,
    token: String,
    ip: IpAddr,
    mut browser: WebSocket,
) {
    let _claim = claim;
    let idle = match state.terminals.timeouts().idle {
        Some(idle) => idle,
        None => match settings::load(&mut &state.database).await {
            Ok(settings) => {
                Duration::from_secs(settings.terminal_idle_timeout_minutes.max(1) as u64 * 60)
            }
            Err(error) => {
                tracing::warn!(%error, "could not read the terminal idle time");
                close(&mut browser, 1011, "try again").await;
                return;
            }
        },
    };
    let command = AgentCommand::OpenTerminal {
        terminal: TerminalRequest {
            terminal_id: requested.id.clone(),
            shell: requested.shell,
            run_as: requested.run_as,
            columns: requested.columns,
            rows: requested.rows,
            notification: requested.notification.clone(),
        },
    };
    if !state.agents.send(&requested.device_id, command) {
        let (code, reason) = End::DeviceOffline.browser_close().expect("a close frame");
        close(&mut browser, code, &reason).await;
        return;
    }
    let opened_at = Instant::now();
    record(&state, &requested, "terminal.open", json!({})).await;
    tracing::info!(
        terminal_id = requested.id,
        device_id = requested.device_id,
        shell = requested.shell.as_str(),
        run_as = requested.run_as.as_str(),
        "opening a terminal"
    );

    let mut outcome = Outcome::default();
    let end = match wait_for_agent(&state, &mut browser, agent).await {
        Ok((agent, resize)) => {
            relay(
                &state,
                &requested,
                browser,
                agent,
                resize,
                idle,
                &token,
                ip,
                &mut outcome,
            )
            .await
        }
        Err(end) => {
            if let Some((code, reason)) = end.browser_close() {
                close(&mut browser, code, &reason).await;
            }
            end
        }
    };
    let duration = opened_at.elapsed();
    tracing::info!(
        terminal_id = requested.id,
        device_id = requested.device_id,
        reason = end.reason(),
        exit_code = outcome.exit_code,
        duration_ms = duration.as_millis() as u64,
        "terminal closed"
    );
    record(
        &state,
        &requested,
        "terminal.close",
        json!({
            "reason": end.reason(),
            "account": outcome.account,
            "program": outcome.program,
            "exit_code": outcome.exit_code,
            "error": outcome.error,
            "duration_ms": duration.as_millis() as u64,
        }),
    )
    .await;
}

/// Records `action` on the device, with what every terminal event carries.
async fn record(
    state: &AppState,
    requested: &Requested,
    action: &str,
    mut metadata: serde_json::Value,
) {
    metadata["terminal_id"] = requested.id.clone().into();
    metadata["shell"] = requested.shell.as_str().into();
    metadata["run_as"] = requested.run_as.as_str().into();
    if let Err(error) = audit::record(
        &mut &state.database,
        &requested.actor,
        action,
        Target::device(&requested.device_id),
        metadata,
    )
    .await
    {
        tracing::warn!(%error, terminal_id = requested.id, action, "could not record a terminal in the audit log");
    }
}

/// Waits for the Agent's socket. Input before it arrives has no shell to go
/// to and is dropped; the browser waits for `started` before sending any.
/// The latest resize is passed on.
async fn wait_for_agent(
    state: &AppState,
    browser: &mut WebSocket,
    agent: oneshot::Receiver<WebSocket>,
) -> Result<(WebSocket, Option<TerminalControl>), End> {
    let deadline = tokio::time::sleep(state.terminals.timeouts().agent_connect);
    tokio::pin!(deadline);
    tokio::pin!(agent);
    let mut resize = None;
    loop {
        tokio::select! {
            socket = &mut agent => return socket.map(|socket| (socket, resize)).map_err(|_| End::DeviceTimeout),
            () = &mut deadline => return Err(End::DeviceTimeout),
            frame = browser.recv() => match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(control @ TerminalControl::Resize { columns, rows }) if valid_terminal_size(columns, rows) => {
                        resize = Some(control);
                    }
                    Ok(TerminalControl::Ack { bytes: 0 }) => {}
                    _ => return Err(End::BrowserProtocol),
                },
                Some(Ok(Message::Binary(bytes))) if bytes.len() <= MAX_TERMINAL_MESSAGE_BYTES => {}
                Some(Ok(Message::Binary(_))) => return Err(End::BrowserProtocol),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(End::BrowserLeft),
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn relay(
    state: &AppState,
    requested: &Requested,
    browser: WebSocket,
    agent: WebSocket,
    resize: Option<TerminalControl>,
    idle: Duration,
    token: &str,
    ip: IpAddr,
    outcome: &mut Outcome,
) -> End {
    let (mut browser_tx, mut browser_rx) = browser.split();
    let (mut agent_tx, mut agent_rx) = agent.split();
    let shared = Shared {
        started: Instant::now(),
        relayed: AtomicU64::new(0),
        last_input_ms: AtomicU64::new(0),
        typed: Default::default(),
    };
    let end = async {
        if let Some(resize) = resize
            && !send_json(&mut agent_tx, &resize).await
        {
            return End::DeviceDisconnected;
        }
        tokio::select! {
            end = upstream(&mut browser_rx, &mut agent_tx, &shared) => end,
            end = downstream(&mut agent_rx, &mut browser_tx, &shared, outcome) => end,
            end = idle_timer(&shared, idle) => end,
            end = access(state, token, ip, requested, &shared) => end,
        }
    }
    .await;
    if let Some((code, reason)) = end.browser_close() {
        let _ = tokio::time::timeout(
            SEND_TIMEOUT,
            browser_tx.send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            }))),
        )
        .await;
    }
    if end != End::DeviceDisconnected {
        // The Agent ends the shell and everything it started.
        let _ = tokio::time::timeout(
            SEND_TIMEOUT,
            agent_tx.send(Message::Close(Some(CloseFrame {
                code: 1000,
                reason: end.reason().into(),
            }))),
        )
        .await;
    }
    end
}

/// Browser to Agent: input, resizes and acknowledgements.
async fn upstream(
    browser: &mut SplitStream<WebSocket>,
    agent: &mut SplitSink<WebSocket, Message>,
    shared: &Shared,
) -> End {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    let mut acknowledged = 0;
    loop {
        tokio::select! {
            frame = tokio::time::timeout(BROWSER_SILENCE, browser.next()) => {
                let Ok(frame) = frame else { return End::BrowserLeft };
                match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        if bytes.len() > MAX_TERMINAL_MESSAGE_BYTES {
                            return End::BrowserProtocol;
                        }
                        shared.touch();
                        if !forward(agent, Message::Binary(bytes)).await {
                            return End::DeviceDisconnected;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let control = match serde_json::from_str(&text) {
                            Ok(TerminalControl::Resize { columns, rows }) if valid_terminal_size(columns, rows) => {
                                shared.touch();
                                TerminalControl::Resize { columns, rows }
                            }
                            // Only output the browser was sent, and never less than before.
                            Ok(TerminalControl::Ack { bytes })
                                if bytes >= acknowledged && bytes <= shared.relayed.load(Ordering::Relaxed) =>
                            {
                                acknowledged = bytes;
                                TerminalControl::Ack { bytes }
                            }
                            _ => return End::BrowserProtocol,
                        };
                        if !send_json(agent, &control).await {
                            return End::DeviceDisconnected;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return End::BrowserLeft,
                }
            }
            _ = ping.tick() => {
                if !forward(agent, Message::Ping(Default::default())).await {
                    return End::DeviceDisconnected;
                }
            }
        }
    }
}

/// Agent to browser: output and what happens to the shell.
async fn downstream(
    agent: &mut SplitStream<WebSocket>,
    browser: &mut SplitSink<WebSocket, Message>,
    shared: &Shared,
    outcome: &mut Outcome,
) -> End {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    loop {
        tokio::select! {
            frame = tokio::time::timeout(LIVENESS, agent.next()) => {
                let Ok(frame) = frame else { return End::DeviceDisconnected };
                match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        // Counted before sending, so an acknowledgement can't
                        // overtake it.
                        shared.relayed.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        if !forward(browser, Message::Binary(bytes)).await {
                            return End::BrowserLeft;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let Ok(event) = serde_json::from_str::<TerminalEvent>(&text) else {
                            return End::DeviceDisconnected;
                        };
                        let event = event.sanitized();
                        let end = match &event {
                            TerminalEvent::Started { account, shell } => {
                                outcome.account = Some(account.clone());
                                outcome.program = Some(shell.clone());
                                None
                            }
                            TerminalEvent::Failed { message } => {
                                outcome.error = Some(message.clone());
                                Some(End::Failed)
                            }
                            TerminalEvent::Exited { code } => {
                                outcome.exit_code = *code;
                                Some(End::Exited)
                            }
                        };
                        if !send_json(browser, &event).await {
                            return End::BrowserLeft;
                        }
                        if let Some(end) = end {
                            return end;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return End::DeviceDisconnected,
                }
            }
            _ = ping.tick() => {
                if !forward(browser, Message::Ping(Default::default())).await {
                    return End::BrowserLeft;
                }
            }
        }
    }
}

/// Ends the terminal once nobody has typed for `idle`.
async fn idle_timer(shared: &Shared, idle: Duration) -> End {
    loop {
        let deadline = shared.last_input() + idle;
        if Instant::now() >= deadline {
            return End::Idle(idle);
        }
        tokio::time::sleep_until(deadline.into()).await;
    }
}

/// Ends the terminal once its user's sign-in ends or they lose the
/// permission it needs. Typing counts as using the website, so a user at
/// work in a terminal stays signed in.
async fn access(
    state: &AppState,
    token: &str,
    ip: IpAddr,
    requested: &Requested,
    shared: &Shared,
) -> End {
    let needed = permission(requested.run_as);
    let mut changes = state.presence.access_changes();
    let mut recheck = tokio::time::interval(ACCESS_RECHECK);
    recheck.tick().await;
    loop {
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    // The server is shutting down; the timer still checks.
                    std::future::pending::<()>().await;
                }
            }
            _ = recheck.tick() => {}
        }
        let typed = shared.typed.swap(false, Ordering::Relaxed);
        match session::load(state, token, ip, typed).await {
            Ok(signed_in) => match Authorized::new(signed_in) {
                Ok(authorized) if authorized.has(needed) => {}
                Ok(_) => return End::PermissionRemoved,
                Err(_) => return End::SignedOut,
            },
            // A database outage doesn't sign anyone out.
            Err(error) if error.status().is_server_error() => {}
            Err(_) => return End::SignedOut,
        }
    }
}

/// Sends `message` within [`SEND_TIMEOUT`]. Returns whether the peer took it.
async fn forward(sink: &mut SplitSink<WebSocket, Message>, message: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, sink.send(message)).await,
        Ok(Ok(()))
    )
}

async fn send_json(
    sink: &mut SplitSink<WebSocket, Message>,
    value: &impl serde::Serialize,
) -> bool {
    let text = serde_json::to_string(value).expect("terminal messages serialize");
    forward(sink, Message::Text(text.into())).await
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = tokio::time::timeout(
        SEND_TIMEOUT,
        socket.send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_owned().into(),
        }))),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_times_read_as_the_settings_offer_them() {
        assert_eq!(describe(Duration::from_secs(5 * 60)), "5 minutes");
        assert_eq!(describe(Duration::from_secs(60)), "1 minute");
        assert_eq!(describe(Duration::from_secs(60 * 60)), "1 hour");
        assert_eq!(describe(Duration::from_secs(8 * 60 * 60)), "8 hours");
        assert_eq!(describe(Duration::from_millis(300)), "1 second");
        assert_eq!(describe(Duration::from_secs(20)), "20 seconds");
        assert_eq!(describe(Duration::from_secs(90)), "1 minute");
    }

    #[test]
    fn every_end_but_leaving_tells_the_browser_why() {
        for end in [
            End::Exited,
            End::Failed,
            End::BrowserProtocol,
            End::DeviceOffline,
            End::DeviceTimeout,
            End::DeviceDisconnected,
            End::Idle(Duration::from_secs(1800)),
            End::SignedOut,
            End::PermissionRemoved,
        ] {
            let (code, reason) = end.browser_close().unwrap();
            assert!(code == 1000 || code == 1003 || (4000..5000).contains(&code));
            // Close reasons are limited to 123 bytes.
            assert!(reason.len() <= 123, "{reason}");
        }
        assert_eq!(End::BrowserLeft.browser_close(), None);
        assert_eq!(
            End::Idle(Duration::from_secs(1800))
                .browser_close()
                .unwrap()
                .1,
            "closed after 30 minutes without input"
        );
    }
}
