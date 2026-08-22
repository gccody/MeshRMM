use meshrmm_protocol_types::{AgentCommand, AgentSessionRequest, AgentStatusMessage};
use serde::{Deserialize, Serialize};
use worker::*;

use crate::company_presence::{self, PresenceMutation};

const AGENT_TAG: &str = "agent";
const COMPANY_HEADER: &str = "X-Mesh-Company-Id";
const DEVICE_HEADER: &str = "X-Mesh-Device-Id";
const UNINSTALL_HEADER: &str = "X-Mesh-Uninstall-Requested";
const IDENTITY_KEY: &str = "agent_identity";

#[derive(Debug, Deserialize, Serialize)]
struct AgentIdentity {
    company_id: String,
    device_id: String,
    connection_id: String,
}

#[durable_object]
pub struct AgentCoordinator {
    state: State,
    environment: Env,
}

impl DurableObject for AgentCoordinator {
    fn new(state: State, environment: Env) -> Self {
        Self { state, environment }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        match (request.method(), request.path().as_str()) {
            (Method::Get, "/connect") => {
                let uninstall_requested = required_header(&request, UNINSTALL_HEADER)? == "true";
                let identity = AgentIdentity {
                    company_id: required_header(&request, COMPANY_HEADER)?,
                    device_id: required_header(&request, DEVICE_HEADER)?,
                    connection_id: crate::random_token(),
                };
                crate::validate_identifier(&identity.company_id, "company ID")?;
                crate::validate_identifier(&identity.device_id, "device ID")?;
                for socket in self.state.get_websockets_with_tag(AGENT_TAG) {
                    let _ = socket.close(Some(4000), Some("superseded Agent connection"));
                }
                self.state.storage().put(IDENTITY_KEY, &identity).await?;
                let pair = WebSocketPair::new()?;
                pair.server
                    .serialize_attachment(identity.connection_id.clone())?;
                self.state
                    .accept_websocket_with_tags(&pair.server, &[AGENT_TAG]);
                if uninstall_requested {
                    pair.server
                        .send_with_str(serde_json::to_string(&AgentCommand::Uninstall)?)?;
                } else if let Err(error) = self.publish_presence(&identity, true).await {
                    let _ = pair
                        .server
                        .close(Some(1011), Some("Agent presence could not be registered"));
                    return Err(error);
                }
                console_log!("event=agent_signaling_connected");
                Response::from_websocket(pair.client)
            }
            (Method::Post, "/uninstall") => {
                if let Some(agent) = self
                    .state
                    .get_websockets_with_tag(AGENT_TAG)
                    .into_iter()
                    .next()
                {
                    agent.send_with_str(serde_json::to_string(&AgentCommand::Uninstall)?)?;
                }
                Response::ok("uninstall queued")
            }
            (Method::Post, "/request") => {
                let session: AgentSessionRequest = request.json().await?;
                let Some(agent) = self
                    .state
                    .get_websockets_with_tag(AGENT_TAG)
                    .into_iter()
                    .next()
                else {
                    return Response::error("Agent is offline", 409);
                };
                agent.send_with_str(serde_json::to_string(&session)?)?;
                console_log!(
                    "event=agent_session_notified session_id={}",
                    session.session_id
                );
                Response::ok("notified")
            }
            (Method::Get, "/status") => {
                let connected = !self.state.get_websockets_with_tag(AGENT_TAG).is_empty();
                Response::from_json(&serde_json::json!({ "connected": connected }))
            }
            _ => Response::error("not found", 404),
        }
    }

    async fn websocket_message(
        &self,
        socket: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        match message {
            WebSocketIncomingMessage::String(value) if value == "ping" => {
                socket.send_with_str("pong")?;
            }
            WebSocketIncomingMessage::String(value) => {
                match serde_json::from_str::<AgentStatusMessage>(&value) {
                    Ok(AgentStatusMessage::UninstallScheduled) => {
                        self.acknowledge_uninstall(&socket).await?;
                    }
                    Err(_) => {
                        socket.close(Some(1003), Some("unsupported Agent registry message"))?;
                    }
                }
            }
            WebSocketIncomingMessage::Binary(_) => {
                socket.close(Some(1003), Some("unsupported Agent registry message"))?;
            }
        }
        Ok(())
    }

    async fn websocket_close(
        &self,
        socket: WebSocket,
        code: usize,
        _reason: String,
        was_clean: bool,
    ) -> Result<()> {
        self.publish_disconnected_if_current(&socket).await;
        console_log!(
            "event=agent_signaling_closed code={} clean={}",
            code,
            was_clean
        );
        Ok(())
    }

    async fn websocket_error(&self, socket: WebSocket, error: Error) -> Result<()> {
        self.publish_disconnected_if_current(&socket).await;
        console_error!("event=agent_signaling_error error={}", error);
        Ok(())
    }
}

impl AgentCoordinator {
    async fn acknowledge_uninstall(&self, socket: &WebSocket) -> Result<()> {
        let Some(connection_id) = socket.deserialize_attachment::<String>()? else {
            return Ok(());
        };
        let Some(identity) = self
            .state
            .storage()
            .get::<AgentIdentity>(IDENTITY_KEY)
            .await?
        else {
            return Ok(());
        };
        if identity.connection_id != connection_id {
            return Ok(());
        }

        console_log!(
            "event=agent_uninstall_scheduled device_id={}",
            identity.device_id
        );
        socket.close(Some(4001), Some("Agent uninstall scheduled"))?;
        Ok(())
    }

    async fn publish_disconnected_if_current(&self, socket: &WebSocket) {
        let connection_id = match socket.deserialize_attachment::<String>() {
            Ok(Some(connection_id)) => connection_id,
            Ok(None) => return,
            Err(error) => {
                console_error!("event=agent_connection_attachment_failed error={}", error);
                return;
            }
        };
        match self
            .state
            .storage()
            .get::<AgentIdentity>(IDENTITY_KEY)
            .await
        {
            Ok(Some(identity)) if identity.connection_id == connection_id => {
                if let Err(error) = self.publish_presence(&identity, false).await {
                    console_error!(
                        "event=agent_presence_publish_failed connected=false error={}",
                        error
                    );
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(error) => console_error!("event=agent_identity_read_failed error={}", error),
        }
    }

    async fn publish_presence(&self, identity: &AgentIdentity, connected: bool) -> Result<()> {
        let mutation = PresenceMutation::Upsert {
            agent_id: identity.device_id.clone(),
            name: None,
            connected: Some(connected),
        };
        company_presence::publish(&self.environment, &identity.company_id, &mutation).await
    }
}

fn required_header(request: &Request, name: &str) -> Result<String> {
    request
        .headers()
        .get(name)?
        .ok_or_else(|| Error::RustError(format!("missing internal Agent header {name}")))
}

pub async fn request_uninstall(environment: &Env, device_id: &str) -> Result<()> {
    let request = Request::new("https://agent.internal/uninstall", Method::Post)?;
    let response = crate::object_stub(environment, "AGENT_COORDINATOR", device_id)?
        .fetch_with_request(request)
        .await?;
    crate::ensure_success(response, "notify Agent uninstall").await
}
