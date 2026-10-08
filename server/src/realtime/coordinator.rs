//! An Agent's control connection, at `/v1/agents/{id}/connect`.
//!
//! The socket's task is the device's coordinator while it is connected: it
//! registers with the [`super::AgentHub`], publishes the device's presence,
//! gives the Agent its live remote session and any rotated credential it
//! hasn't used yet, then forwards what the API sends until the Agent
//! disconnects or a newer connection from the device replaces it.
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use meshrmm_protocol_types::{AgentCommand, AgentStatusMessage, is_release_version};

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
    if greet(&state, &device_id, &mut socket).await.is_ok() {
        let mut last_heard = Instant::now();
        loop {
            tokio::select! {
                outgoing = connection.commands.recv() => {
                    let Some(outgoing) = outgoing else {
                        send(&mut socket, close_frame(4000, "superseded Agent connection")).await;
                        break;
                    };
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
    state.presence.disconnected(&device_id).await;
    tracing::info!(device_id, "Agent disconnected");
}

/// What a newly connected Agent is owed: the session it should be in, and
/// a rotated credential it hasn't used yet.
async fn greet(state: &AppState, device_id: &str, socket: &mut WebSocket) -> Result<(), ()> {
    if let Some(request) = state.sessions.replay(state, device_id).await {
        let message = ToAgent::Session(request).to_json();
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
    Ok(())
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
