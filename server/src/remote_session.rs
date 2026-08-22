use pulsermm_protocol_types::SignalMessage;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use worker::*;

const CLIENT_TAG: &str = "client";
const AGENT_TAG: &str = "agent";
const PENDING_TERMINAL_SIGNAL_KEY: &str = "pending-terminal-signal";
const MAX_SIGNAL_BYTES: usize = 64 * 1024;
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 15 * 60 * 1000;

fn default_idle_timeout_ms() -> u64 {
    DEFAULT_IDLE_TIMEOUT_MS
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionRecord {
    client_token: String,
    agent_token: String,
    expires_at_unix_ms: u64,
    // Keep existing Durable Object records readable during a rolling deploy.
    #[serde(default = "default_idle_timeout_ms")]
    idle_timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingTerminalSignal {
    destination: String,
    text: String,
}

#[durable_object]
pub struct RemoteSession {
    state: State,
}

impl DurableObject for RemoteSession {
    fn new(state: State, _environment: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        match (request.method(), request.path().as_str()) {
            (Method::Post, "/init") => {
                if self
                    .state
                    .storage()
                    .get::<SessionRecord>("session")
                    .await?
                    .is_some()
                {
                    return Response::error("session already initialized", 409);
                }
                let record: SessionRecord = request.json().await?;
                if record.expires_at_unix_ms <= Date::now().as_millis() {
                    return Response::error("session already expired", 400);
                }
                if record.idle_timeout_ms == 0 {
                    return Response::error("idle timeout must be positive", 400);
                }
                self.state.storage().put("session", &record).await?;
                let alarm_delay = record.expires_at_unix_ms - Date::now().as_millis();
                self.state.storage().set_alarm(alarm_delay as i64).await?;
                Response::ok("initialized")
            }
            (Method::Post, "/expire") => {
                self.expire("session cancelled").await?;
                Response::ok("expired")
            }
            (Method::Get, "/signal") => self.accept_peer(&request).await,
            _ => Response::error("not found", 404),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        let Some(record) = self.state.storage().get::<SessionRecord>("session").await? else {
            return Response::ok("already expired");
        };
        let now = Date::now().as_millis();
        if record.expires_at_unix_ms > now {
            self.state
                .storage()
                .set_alarm((record.expires_at_unix_ms - now) as i64)
                .await?;
            return Response::ok("idle deadline advanced");
        }

        self.expire("session idle timeout").await?;
        Response::ok("expired")
    }

    async fn websocket_message(
        &self,
        socket: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let text = match message {
            WebSocketIncomingMessage::String(text) if text.len() <= MAX_SIGNAL_BYTES => text,
            WebSocketIncomingMessage::String(_) | WebSocketIncomingMessage::Binary(_) => {
                socket.close(Some(1009), Some("invalid signaling message"))?;
                return Ok(());
            }
        };
        let signal: SignalMessage = match serde_json::from_str(&text) {
            Ok(signal) => signal,
            Err(_) => {
                socket.close(Some(1007), Some("invalid signaling JSON"))?;
                return Ok(());
            }
        };
        let tags = self.state.get_tags(&socket);
        let is_client = tags.iter().any(|tag| tag == CLIENT_TAG);
        let is_agent = tags.iter().any(|tag| tag == AGENT_TAG);
        if matches!(signal, SignalMessage::Activity) {
            if !is_client {
                socket.close(Some(1008), Some("only the client may report activity"))?;
                return Ok(());
            }
            self.refresh_idle_deadline().await?;
            return Ok(());
        }
        let destination = if is_client {
            AGENT_TAG
        } else if is_agent {
            CLIENT_TAG
        } else {
            socket.close(Some(1008), Some("unrecognized peer"))?;
            return Ok(());
        };
        let terminal = matches!(signal, SignalMessage::Error { .. });
        if terminal {
            // The Agent can fail capture before the viewer has completed its
            // WebSocket upgrade. Persist the terminal signal first so a DO
            // eviction or deployment cannot turn that failure into a client
            // timeout with no actionable explanation.
            self.state
                .storage()
                .put(
                    PENDING_TERMINAL_SIGNAL_KEY,
                    &PendingTerminalSignal {
                        destination: destination.into(),
                        text: text.clone(),
                    },
                )
                .await?;
            console_log!("event=peer_reported_signaling_error");
        }

        let mut delivered = false;
        for peer in self.state.get_websockets_with_tag(destination) {
            match peer.send_with_str(&text) {
                Ok(()) => delivered = true,
                Err(error) => {
                    console_error!("event=remote_signal_forward_failed error={}", error)
                }
            }
        }
        if terminal && delivered {
            self.state
                .storage()
                .delete(PENDING_TERMINAL_SIGNAL_KEY)
                .await?;
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
        self.notify_other_peer(&socket, &SignalMessage::PeerLeft)?;
        console_log!(
            "event=remote_signal_peer_closed code={} clean={}",
            code,
            was_clean
        );
        Ok(())
    }

    async fn websocket_error(&self, socket: WebSocket, error: Error) -> Result<()> {
        self.notify_other_peer(&socket, &SignalMessage::PeerLeft)?;
        console_error!("event=remote_signal_peer_error error={}", error);
        Ok(())
    }
}

impl RemoteSession {
    async fn refresh_idle_deadline(&self) -> Result<()> {
        let Some(mut record) = self.state.storage().get::<SessionRecord>("session").await? else {
            return Ok(());
        };
        let now = Date::now().as_millis();
        if record.expires_at_unix_ms <= now {
            self.expire("session idle timeout").await?;
            return Ok(());
        }

        record.expires_at_unix_ms = now.saturating_add(record.idle_timeout_ms);
        self.state.storage().put("session", &record).await?;
        self.state
            .storage()
            .set_alarm(record.idle_timeout_ms as i64)
            .await?;
        Ok(())
    }

    async fn accept_peer(&self, request: &Request) -> Result<Response> {
        let record = self
            .state
            .storage()
            .get::<SessionRecord>("session")
            .await?
            .ok_or_else(|| Error::RustError("unknown session".into()))?;
        if record.expires_at_unix_ms <= Date::now().as_millis() {
            return Response::error("session expired", 401);
        }
        let role = request
            .url()?
            .query_pairs()
            .find_map(|(key, value)| (key == "role").then(|| value.into_owned()))
            .ok_or_else(|| Error::RustError("missing peer role".into()))?;
        let expected = match role.as_str() {
            CLIENT_TAG => &record.client_token,
            AGENT_TAG => &record.agent_token,
            _ => return Response::error("invalid peer role", 400),
        };
        let supplied = request
            .headers()
            .get("Authorization")?
            .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned))
            .unwrap_or_default();
        if !token_eq(supplied.as_bytes(), expected.as_bytes()) {
            return Response::error("unauthorized", 401);
        }
        for existing in self.state.get_websockets_with_tag(&role) {
            let _ = existing.close(Some(4000), Some("superseded peer connection"));
        }
        let pair = WebSocketPair::new()?;
        self.state
            .accept_websocket_with_tags(&pair.server, &[role.as_str()]);
        if let Some(pending) = self
            .state
            .storage()
            .get::<PendingTerminalSignal>(PENDING_TERMINAL_SIGNAL_KEY)
            .await?
            && pending.destination == role
        {
            pair.server.send_with_str(&pending.text)?;
            self.state
                .storage()
                .delete(PENDING_TERMINAL_SIGNAL_KEY)
                .await?;
            console_log!("event=pending_terminal_signal_delivered role={}", role);
        }
        console_log!("event=remote_signal_peer_connected role={}", role);
        Response::from_websocket(pair.client)
    }

    fn notify_other_peer(&self, socket: &WebSocket, signal: &SignalMessage) -> Result<()> {
        let tags = self.state.get_tags(socket);
        let destination = if tags.iter().any(|tag| tag == CLIENT_TAG) {
            AGENT_TAG
        } else {
            CLIENT_TAG
        };
        let text = serde_json::to_string(signal)?;
        for peer in self.state.get_websockets_with_tag(destination) {
            let _ = peer.send_with_str(&text);
        }
        Ok(())
    }

    async fn expire(&self, reason: &str) -> Result<()> {
        for socket in self.state.get_websockets() {
            let _ = socket.close(Some(4001), Some(reason));
        }
        self.state.storage().delete_all().await?;
        console_log!("event=remote_session_expired");
        Ok(())
    }
}

fn token_eq(left: &[u8], right: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    bool::from(Sha256::digest(left).ct_eq(&Sha256::digest(right)))
}
