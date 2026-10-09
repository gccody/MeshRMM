//! An Agent's control connection, at `/v1/agents/{id}/connect`.
//!
//! The socket's task is the device's coordinator while it is connected: it
//! registers with the [`super::AgentHub`], publishes the device's presence,
//! gives the Agent its live remote session and any rotated credential it
//! hasn't used yet, then forwards what the API sends, and records the
//! resource usage the Agent reports, until the Agent disconnects or a newer
//! connection from the device replaces it.
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use meshrmm_protocol_types::{
    AgentCommand, AgentSessionRequest, AgentStatusMessage, is_release_version,
};

use super::{LIVENESS, ToAgent, close_frame, send};
use crate::{agents, http::AppState};

/// Serves the control connection of an Agent that authenticated as
/// `device_id`.
pub async fn serve(
    state: AppState,
    device_id: String,
    deletion_requested: bool,
    socket: WebSocket,
) {
    if deletion_requested {
        uninstall(socket).await;
        return;
    }
    let mut socket = socket;
    let mut connection = state.agents.connect(&device_id);
    state.presence.connected(&device_id).await;
    tracing::info!(device_id, "Agent connected");
    if let Ok(replayed) = greet(&state, &device_id, &mut socket).await {
        let mut last_session = replayed;
        let mut last_heard = Instant::now();
        loop {
            tokio::select! {
                outgoing = connection.commands.recv() => {
                    let Some(outgoing) = outgoing else {
                        send(&mut socket, close_frame(4000, "superseded Agent connection")).await;
                        break;
                    };
                    if repeats_session(&mut last_session, &outgoing) {
                        continue;
                    }
                    if !send(&mut socket, Message::Text(outgoing.to_json().into())).await {
                        break;
                    }
                }
                frame = socket.recv() => {
                    last_heard = Instant::now();
                    let text = match frame {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        Some(Ok(Message::Binary(_))) => {
                            send(&mut socket, close_frame(1003, "unsupported Agent message")).await;
                            break;
                        }
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    };
                    match serde_json::from_str::<AgentStatusMessage>(&text) {
                        Ok(AgentStatusMessage::Updating { version }) if is_release_version(&version) => {
                            if state.agents.is_current(&device_id, connection.id) {
                                tracing::info!(device_id, version, "Agent is installing an update");
                                state.presence.updating(&device_id, version).await;
                            }
                        }
                        Ok(AgentStatusMessage::Updating { .. }) => {
                            send(&mut socket, close_frame(1003, "invalid Agent update version")).await;
                            break;
                        }
                        Ok(AgentStatusMessage::Metrics { metrics }) => {
                            if state.agents.is_current(&device_id, connection.id) {
                                state.metrics.record(&device_id, metrics).await;
                            }
                        }
                        // Only a deleted device's Agent uninstalls.
                        Ok(AgentStatusMessage::UninstallScheduled) => {}
                        Err(_) => {
                            send(&mut socket, close_frame(1003, "unsupported Agent message")).await;
                            break;
                        }
                    }
                }
                () = tokio::time::sleep_until((last_heard + LIVENESS).into()) => break,
            }
        }
    }
    drop(connection);
    // A newer connection from the device keeps reporting.
    if !state.agents.is_connected(&device_id) {
        state.metrics.disconnected(&device_id).await;
    }
    state.presence.disconnected(&device_id).await;
    tracing::info!(device_id, "Agent disconnected");
}

/// What a newly connected Agent is owed: the session it should be in, and
/// a rotated credential it hasn't used yet. Returns the session it replayed.
async fn greet(
    state: &AppState,
    device_id: &str,
    socket: &mut WebSocket,
) -> Result<Option<AgentSessionRequest>, ()> {
    let replayed = state.sessions.replay(state, device_id).await;
    if let Some(request) = &replayed {
        let message = ToAgent::Session(request.clone()).to_json();
        socket
            .send(Message::Text(message.into()))
            .await
            .map_err(drop)?;
        tracing::info!(device_id, "replayed the device's remote session");
    }
    let staged = match agents::pending_credential(&mut &state.database, device_id).await {
        Ok(Some(pending)) => pending.token(&state.instance_key, device_id),
        Ok(None) => Ok(None),
        Err(error) => Err(error.into()),
    };
    match staged {
        Ok(Some(token)) => {
            let message = ToAgent::Command(AgentCommand::RotateToken { token }).to_json();
            socket
                .send(Message::Text(message.into()))
                .await
                .map_err(drop)?;
            tracing::info!(device_id, "resent the Agent's rotated credential");
        }
        Ok(None) => {}
        // Rotating again sends the credential, so the Agent stays connected.
        Err(error) => tracing::warn!(
            device_id,
            error = format!("{error:#}"),
            "could not read the Agent's rotated credential"
        ),
    }
    Ok(replayed)
}

/// Whether `outgoing` is the session request the Agent was last sent, which
/// it needn't get again. A session created or resumed while the Agent
/// connects is both queued for the connection and replayed to it, and an
/// Agent whose side of the session has finished would start it again on
/// getting the same request twice. A resume always carries a new token, so
/// it is never a repeat.
fn repeats_session(last: &mut Option<AgentSessionRequest>, outgoing: &ToAgent) -> bool {
    let ToAgent::Session(request) = outgoing else {
        return false;
    };
    if last.as_ref() == Some(request) {
        return true;
    }
    *last = Some(request.clone());
    false
}

/// A deleted device's Agent is told to uninstall itself. It says when it has
/// scheduled that, and the connection ends.
async fn uninstall(mut socket: WebSocket) {
    let message = ToAgent::Command(AgentCommand::Uninstall).to_json();
    if !send(&mut socket, Message::Text(message.into())).await {
        return;
    }
    loop {
        let frame = match tokio::time::timeout(LIVENESS, socket.recv()).await {
            Ok(Some(Ok(frame))) => frame,
            _ => return,
        };
        match frame {
            Message::Text(text)
                if matches!(
                    serde_json::from_str(&text),
                    Ok(AgentStatusMessage::UninstallScheduled)
                ) =>
            {
                tracing::info!("a deleted device's Agent scheduled its uninstall");
                let _ = socket
                    .send(close_frame(4001, "Agent uninstall scheduled"))
                    .await;
                return;
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::RemoteSessionId;

    use super::*;

    fn request(token: &str) -> AgentSessionRequest {
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
            session_id: RemoteSessionId::new("one"),
            signaling_token: token.into(),
            expires_at_unix_ms: 100,
            ice_servers: vec![],
        }
    }

    #[test]
    fn a_replayed_session_is_not_sent_again() {
        let mut last = Some(request("first"));
        assert!(repeats_session(
            &mut last,
            &ToAgent::Session(request("first"))
        ));
        // A resume's new token goes through, and then is the one not repeated.
        assert!(!repeats_session(
            &mut last,
            &ToAgent::Session(request("second"))
        ));
        assert!(repeats_session(
            &mut last,
            &ToAgent::Session(request("second"))
        ));
        assert!(!repeats_session(
            &mut last,
            &ToAgent::Command(AgentCommand::Uninstall)
        ));

        let mut none = None;
        assert!(!repeats_session(
            &mut none,
            &ToAgent::Session(request("first"))
        ));
        assert!(repeats_session(
            &mut none,
            &ToAgent::Session(request("first"))
        ));
    }
}
