//! The Agents connected to this server, and what is sent to them.
//!
//! An Agent holds one control connection. Its socket ([`super::coordinator`])
//! registers with [`AgentHub::connect`] and forwards what it receives; the API
//! sends commands and session requests with [`AgentHub::send`]. A new
//! connection from the same device replaces the old one, whose stream then
//! ends.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use meshrmm_protocol_types::{AgentCommand, AgentSessionRequest};
use tokio::sync::mpsc;

/// Messages waiting for a slow connection. A connection this far behind is
/// stuck, and further messages report the Agent as unreachable.
const COMMAND_BACKLOG: usize = 32;

/// Something for a connected Agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToAgent {
    Command(AgentCommand),
    /// A remote session to join, or the live one again with a new deadline
    /// or ICE servers.
    Session(AgentSessionRequest),
}

impl From<AgentCommand> for ToAgent {
    fn from(command: AgentCommand) -> Self {
        Self::Command(command)
    }
}

impl ToAgent {
    /// The message as the Agent reads it. A session on the user's desktop is
    /// the bare request; one on the background desktop is a command.
    pub fn to_json(&self) -> String {
        let json = match self {
            Self::Command(command) => serde_json::to_string(command),
            Self::Session(request) if request.start_in_background => {
                serde_json::to_string(&AgentCommand::StartBackgroundSession {
                    request: request.clone(),
                })
            }
            Self::Session(request) => serde_json::to_string(request),
        };
        json.expect("Agent messages serialize")
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentHub {
    connections: Arc<Mutex<HashMap<String, Registered>>>,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Registered {
    id: u64,
    sender: mpsc::Sender<ToAgent>,
}

/// A connected Agent's end of the hub. Dropping it unregisters the
/// connection, unless a newer one has replaced it.
#[derive(Debug)]
pub struct AgentConnection {
    pub device_id: String,
    /// Distinguishes this connection from the device's earlier and later ones.
    pub id: u64,
    /// What to send the Agent. Ends when another connection replaces this
    /// one or the device is disconnected.
    pub commands: mpsc::Receiver<ToAgent>,
    hub: AgentHub,
}

impl Drop for AgentConnection {
    fn drop(&mut self) {
        let mut connections = self.hub.lock();
        if connections
            .get(&self.device_id)
            .is_some_and(|registered| registered.id == self.id)
        {
            connections.remove(&self.device_id);
        }
    }
}

impl AgentHub {
    /// Registers a connection for `device_id`, replacing any earlier one.
    pub fn connect(&self, device_id: &str) -> AgentConnection {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, commands) = mpsc::channel(COMMAND_BACKLOG);
        self.lock()
            .insert(device_id.to_owned(), Registered { id, sender });
        AgentConnection {
            device_id: device_id.to_owned(),
            id,
            commands,
            hub: self.clone(),
        }
    }

    pub fn is_connected(&self, device_id: &str) -> bool {
        self.lock()
            .get(device_id)
            .is_some_and(|registered| !registered.sender.is_closed())
    }

    /// Whether `connection_id` is the device's current connection.
    pub fn is_current(&self, device_id: &str, connection_id: u64) -> bool {
        self.lock()
            .get(device_id)
            .is_some_and(|registered| registered.id == connection_id)
    }

    /// The devices with a connection.
    pub fn connected(&self) -> HashSet<String> {
        self.lock()
            .iter()
            .filter(|(_, registered)| !registered.sender.is_closed())
            .map(|(device_id, _)| device_id.clone())
            .collect()
    }

    /// Queues `message` for the device's connection. Returns whether there
    /// is one that accepted it; an Agent that is offline, or so far behind
    /// that its backlog is full, did not.
    pub fn send(&self, device_id: &str, message: impl Into<ToAgent>) -> bool {
        let connections = self.lock();
        let Some(registered) = connections.get(device_id) else {
            return false;
        };
        match registered.sender.try_send(message.into()) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(device_id, %error, "could not queue a message for an Agent");
                false
            }
        }
    }

    /// Ends the device's connection, if any.
    pub fn disconnect(&self, device_id: &str) {
        self.lock().remove(device_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Registered>> {
        // The map stays consistent even if a holder panicked.
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::RemoteSessionId;

    use super::*;

    fn uninstall() -> Option<ToAgent> {
        Some(ToAgent::Command(AgentCommand::Uninstall))
    }

    #[tokio::test]
    async fn commands_reach_the_newest_connection_only() {
        let hub = AgentHub::default();
        assert!(!hub.is_connected("device-1"));
        assert!(!hub.send("device-1", AgentCommand::Uninstall));

        let mut first = hub.connect("device-1");
        assert!(hub.is_connected("device-1"));
        assert!(hub.is_current("device-1", first.id));
        assert!(hub.send("device-1", AgentCommand::Uninstall));
        assert_eq!(first.commands.recv().await, uninstall());

        let mut second = hub.connect("device-1");
        assert_ne!(first.id, second.id);
        assert!(!hub.is_current("device-1", first.id));
        assert_eq!(first.commands.recv().await, None, "the old stream ends");
        assert!(hub.send("device-1", AgentCommand::Uninstall));
        assert_eq!(second.commands.recv().await, uninstall());

        drop(first);
        assert!(
            hub.is_connected("device-1"),
            "dropping a replaced connection keeps the newer one"
        );
        assert_eq!(hub.connected(), HashSet::from(["device-1".to_owned()]));
        drop(second);
        assert!(!hub.is_connected("device-1"));
        assert!(hub.connected().is_empty());
    }

    #[tokio::test]
    async fn disconnecting_ends_the_stream_and_a_full_backlog_refuses() {
        let hub = AgentHub::default();
        let mut connection = hub.connect("device-1");
        for _ in 0..COMMAND_BACKLOG {
            assert!(hub.send("device-1", AgentCommand::Uninstall));
        }
        assert!(!hub.send("device-1", AgentCommand::Uninstall));
        hub.disconnect("device-1");
        assert!(!hub.is_connected("device-1"));
        for _ in 0..COMMAND_BACKLOG {
            assert!(connection.commands.recv().await.is_some());
        }
        assert_eq!(connection.commands.recv().await, None);
    }

    #[test]
    fn sessions_on_the_user_desktop_are_sent_bare() {
        let request = AgentSessionRequest {
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
            signaling_token: "token".into(),
            expires_at_unix_ms: 100,
            ice_servers: vec![],
        };
        let json = ToAgent::Session(request.clone()).to_json();
        assert_eq!(
            serde_json::from_str::<AgentSessionRequest>(&json).unwrap(),
            request
        );
        let background = AgentSessionRequest {
            start_in_background: true,
            ..request
        };
        let json = ToAgent::Session(background.clone()).to_json();
        assert!(serde_json::from_str::<AgentSessionRequest>(&json).is_err());
        assert_eq!(
            serde_json::from_str::<AgentCommand>(&json).unwrap(),
            AgentCommand::StartBackgroundSession {
                request: background
            }
        );
    }
}
