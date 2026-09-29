//! Holds the session at the approval prompt: tells the viewer it is waiting
//! for the user, and tells it when the user denied the connection.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use meshrmm_protocol::{SignalErrorCode, SignalMessage};
use meshrmm_signaling_client::{
    SignalingConnection, authenticated_websocket, is_terminal_websocket_error,
};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use super::{ApprovalPrompt, ConnectionApproval, Decision, remaining_seconds};
use crate::remote::config::ExecutionMode;

/// How long after its own deadline the service waits for a helper that has
/// not answered, before answering for it.
const HELPER_GRACE: Duration = Duration::from_secs(5);
/// How often a lost signaling connection is tried again while waiting.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Whether the connection may go ahead: asks the user unless this session
/// was already answered. A denial is reported to the viewer.
pub async fn obtain(
    approval: &ConnectionApproval,
    signal_url: &Url,
    token: &str,
    mode: ExecutionMode,
) -> anyhow::Result<bool> {
    let (accepted, socket) = match approval.previous_answer() {
        Some(accepted) => (accepted, None),
        None => {
            let (decision, socket) =
                ask_while_signaling(approval.prompt(), signal_url, token, mode).await?;
            tracing::info!(?decision, "connection approval answered");
            approval.record(decision.accepted());
            (decision.accepted(), socket)
        }
    };
    if !accepted {
        report_declined(socket, signal_url, token).await;
    }
    Ok(accepted)
}

/// Shows the prompt, and meanwhile answers the viewer's requests to start
/// with how long it may wait. Returns the answer and the signaling
/// connection, if one is open.
async fn ask_while_signaling(
    prompt: &ApprovalPrompt,
    signal_url: &Url,
    token: &str,
    mode: ExecutionMode,
) -> anyhow::Result<(Decision, Option<SignalingConnection>)> {
    let started = Instant::now();
    let deadline = started + prompt.timeout + HELPER_GRACE;
    let mut asking = Prompt::start(prompt, mode);
    let mut socket = None;
    let mut reconnect = tokio::time::interval(RECONNECT_INTERVAL);
    reconnect.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let waiting = SignalMessage::AwaitingApproval {
            remaining_seconds: remaining_seconds(prompt.timeout, started.elapsed()),
        };
        tokio::select! {
            answer = asking.answer() => match answer {
                Ok(decision) => return Ok((decision, socket)),
                Err(error) => {
                    // Nobody can answer a prompt that is not shown, so the
                    // policy's timeout still applies.
                    tracing::warn!(error = ?error, "the connection approval prompt failed");
                    asking = Prompt::Unavailable;
                }
            },
            () = tokio::time::sleep_until(deadline) => {
                tracing::warn!("the connection approval prompt did not answer in time");
                return Ok((Decision::TimedOut, socket));
            }
            _ = reconnect.tick(), if socket.is_none() => {
                match authenticated_websocket(signal_url.clone(), token).await {
                    Ok((connected, _)) => {
                        let connected = SignalingConnection::new(connected);
                        // The viewer may be waiting already.
                        if send(&connected, &waiting).await.is_ok() {
                            socket = Some(connected);
                        }
                    }
                    Err(error) if is_terminal_websocket_error(&error) => return Err(error),
                    Err(error) => {
                        tracing::warn!(error = %error, "approval signaling connection failed; retrying");
                    }
                }
            }
            incoming = next(&mut socket) => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if matches!(serde_json::from_str(text.as_str()), Ok(SignalMessage::Ready))
                        && let Some(connected) = socket.as_ref()
                        && send(connected, &waiting).await.is_err()
                    {
                        socket = None;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) if is_terminal_websocket_error(&error) => return Err(error),
                Some(Err(_)) | None => socket = None,
            },
        }
    }
}

async fn next(socket: &mut Option<SignalingConnection>) -> Option<anyhow::Result<Message>> {
    match socket {
        Some(socket) => socket.next().await,
        None => std::future::pending().await,
    }
}

async fn send(socket: &SignalingConnection, message: &SignalMessage) -> anyhow::Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(message)?.into()))
        .await
}

/// Tells the viewer the user denied the connection. The server keeps the
/// message for a viewer that has not connected yet.
async fn report_declined(socket: Option<SignalingConnection>, signal_url: &Url, token: &str) {
    let socket = match socket {
        Some(socket) => Ok(socket),
        None => authenticated_websocket(signal_url.clone(), token)
            .await
            .map(|(socket, _)| SignalingConnection::new(socket)),
    };
    let declined = SignalMessage::Error {
        message: "the remote user declined the connection".into(),
        code: Some(SignalErrorCode::ConnectionDeclined),
    };
    if let Err(error) = match socket {
        Ok(socket) => send(&socket, &declined).await,
        Err(error) => Err(error),
    } {
        tracing::warn!(error = %error, "could not tell the viewer the connection was declined");
    }
}

/// Where the prompt runs: a helper on the console for the service, or this
/// process in console mode.
enum Prompt {
    Helper(crate::remote::capture_helper::ApprovalHelper),
    InProcess {
        answer: tokio::task::JoinHandle<Option<Decision>>,
        cancelled: Arc<AtomicBool>,
    },
    /// The prompt could not be shown.
    Unavailable,
}

impl Prompt {
    fn start(prompt: &ApprovalPrompt, mode: ExecutionMode) -> Self {
        if mode == ExecutionMode::Worker {
            return match crate::remote::capture_helper::ApprovalHelper::start(prompt) {
                Ok(helper) => Self::Helper(helper),
                Err(error) => {
                    tracing::warn!(error = ?error, "could not start the connection approval prompt");
                    Self::Unavailable
                }
            };
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let prompt = prompt.clone();
        Self::InProcess {
            answer: tokio::task::spawn_blocking(move || {
                super::ask(&prompt, || stop.load(Ordering::Acquire))
            }),
            cancelled,
        }
    }

    async fn answer(&mut self) -> anyhow::Result<Decision> {
        match self {
            Self::Helper(helper) => helper.answer().await,
            Self::InProcess { answer, .. } => (&mut *answer)
                .await?
                .ok_or_else(|| anyhow::anyhow!("the connection approval prompt was cancelled")),
            Self::Unavailable => std::future::pending().await,
        }
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        if let Self::InProcess { cancelled, .. } = self {
            cancelled.store(true, Ordering::Release);
        }
    }
}
