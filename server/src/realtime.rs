//! The Agents connected to this server, and the commands sent to them.
//!
//! An Agent holds one control connection. Whatever serves it (the Agent's
//! WebSocket) registers with [`AgentHub::connect`] and forwards the commands
//! it receives; the API sends commands with [`AgentHub::send`]. A new
//! connection from the same device replaces the old one, whose command
//! stream then ends.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use meshrmm_protocol_types::AgentCommand;
use tokio::sync::mpsc;

/// Commands waiting for a slow connection. A connection this far behind is
/// stuck, and further commands report the Agent as unreachable.
const COMMAND_BACKLOG: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct AgentHub {
    connections: Arc<Mutex<HashMap<String, Registered>>>,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Registered {
    id: u64,
    commands: mpsc::Sender<AgentCommand>,
}

/// A connected Agent's end of the hub. Dropping it unregisters the
/// connection, unless a newer one has replaced it.
#[derive(Debug)]
pub struct AgentConnection {
    pub device_id: String,
    /// Distinguishes this connection from the device's earlier and later ones.
    pub id: u64,
    /// Commands for the Agent. Ends when another connection replaces this one
    /// or the device is disconnected.
    pub commands: mpsc::Receiver<AgentCommand>,
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
        self.lock().insert(
            device_id.to_owned(),
            Registered {
                id,
                commands: sender,
            },
        );
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
            .is_some_and(|registered| !registered.commands.is_closed())
    }

    /// The devices with a connection.
    pub fn connected(&self) -> HashSet<String> {
        self.lock()
            .iter()
            .filter(|(_, registered)| !registered.commands.is_closed())
            .map(|(device_id, _)| device_id.clone())
            .collect()
    }

    /// Queues `command` for the device's connection. Returns whether there is
    /// one that accepted it; an Agent that is offline, or so far behind that
    /// its backlog is full, did not.
    pub fn send(&self, device_id: &str, command: AgentCommand) -> bool {
        let connections = self.lock();
        let Some(registered) = connections.get(device_id) else {
            return false;
        };
        match registered.commands.try_send(command) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(device_id, %error, "could not queue an Agent command");
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
    use super::*;

    #[tokio::test]
    async fn commands_reach_the_newest_connection_only() {
        let hub = AgentHub::default();
        assert!(!hub.is_connected("device-1"));
        assert!(!hub.send("device-1", AgentCommand::Uninstall));

        let mut first = hub.connect("device-1");
        assert!(hub.is_connected("device-1"));
        assert!(hub.send("device-1", AgentCommand::Uninstall));
        assert_eq!(first.commands.recv().await, Some(AgentCommand::Uninstall));

        let mut second = hub.connect("device-1");
        assert_ne!(first.id, second.id);
        assert_eq!(first.commands.recv().await, None, "the old stream ends");
        assert!(hub.send("device-1", AgentCommand::Uninstall));
        assert_eq!(second.commands.recv().await, Some(AgentCommand::Uninstall));

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
}
