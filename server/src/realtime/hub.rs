//! The Agents connected to this server, and what is sent to them.
//!
//! An Agent holds one control connection. Its socket ([`super::coordinator`])
//! registers with [`AgentHub::connect`] and forwards what it receives; the API
//! sends commands and session requests with [`AgentHub::send`], or holds a
//! place with [`AgentHub::reserve`] first when it records what it sends. A
//! connection whose backlog is full is behind, not offline. A new
//! connection from the same device replaces the old one, whose stream then
//! ends.
//!
//! The instance's update scan interval is not queued: the hub holds the
//! newest one announced ([`AgentHub::announce_scan_interval`]), and every
//! connection sends its Agent a change the next time it turns to what it has
//! to send ([`AgentConnection::next_message`]), however full its backlog.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use meshrmm_protocol_types::{AgentCommand, AgentSessionRequest, UpdateScanSchedule};
use tokio::sync::{mpsc, watch};

use crate::http::ApiError;

/// Messages waiting for a slow connection. Further messages for a
/// connection this far behind are refused as [`Unsent::Full`] until it
/// catches up.
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

#[derive(Debug, Clone)]
pub struct AgentHub {
    connections: Arc<Mutex<HashMap<String, Registered>>>,
    next_id: Arc<AtomicU64>,
    /// The update scan interval last announced, in hours. Each connection
    /// watches it from when it registers; the value before any announcement
    /// is never sent.
    scan_interval: watch::Sender<Option<u32>>,
}

impl Default for AgentHub {
    fn default() -> Self {
        Self {
            connections: Arc::default(),
            next_id: Arc::default(),
            scan_interval: watch::channel(None).0,
        }
    }
}

#[derive(Debug)]
struct Registered {
    id: u64,
    sender: mpsc::Sender<ToAgent>,
}

/// Why a message couldn't be queued for an Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsent {
    /// The device has no connection.
    Offline,
    /// It is connected, but so far behind that its backlog is full.
    Full,
}

impl Unsent {
    /// The answer for a person who asked for the message: `offline` for a
    /// device that isn't connected, and [`ApiError::device_busy`] for one
    /// that is but can't take more yet.
    pub fn answer(self, offline: ApiError) -> ApiError {
        match self {
            Self::Offline => offline,
            Self::Full => ApiError::device_busy(),
        }
    }
}

/// A place held in a connected Agent's backlog by [`AgentHub::reserve`].
/// Dropping it gives the place back.
#[derive(Debug)]
pub struct Reserved {
    permit: mpsc::OwnedPermit<ToAgent>,
    sender: mpsc::Sender<ToAgent>,
}

impl Reserved {
    /// Queues `message` in the held place. Returns whether the connection
    /// was still open: one that closed since the place was held doesn't get
    /// it.
    pub fn send(self, message: impl Into<ToAgent>) -> bool {
        if self.sender.is_closed() {
            return false;
        }
        self.permit.send(message.into());
        true
    }
}

/// Who [`AgentHub::announce_scan_interval`] reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announced {
    /// The connections that will send it.
    pub connected: usize,
    /// Of those, the ones whose backlog was full: each sends it once its
    /// writer has finished what it is sending, ahead of its backlog.
    pub behind: usize,
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
    /// The announced update scan interval, seen as of when the connection
    /// registered.
    scan_interval: watch::Receiver<Option<u32>>,
    hub: AgentHub,
}

impl AgentConnection {
    /// What to send the Agent next: the update scan interval if one was
    /// announced since it was last sent (only the newest of several), else
    /// the next queued message. `None` once the connection was replaced or
    /// the device disconnected. Cancelling it loses nothing.
    pub async fn next_message(&mut self) -> Option<ToAgent> {
        tokio::select! {
            biased;
            // The hub, which holds the sender, outlives the connection.
            Ok(()) = self.scan_interval.changed() => {}
            message = self.commands.recv() => return message,
        }
        let interval_hours = *self.scan_interval.borrow_and_update();
        // Each Agent keeps the last scan it knows of.
        Some(ToAgent::Command(AgentCommand::SetUpdateScanSchedule {
            schedule: UpdateScanSchedule {
                interval_hours,
                last_scan: None,
            },
        }))
    }
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
        // Watched before the connection reads the interval to greet its
        // Agent with, so an announcement after that read is not missed.
        let scan_interval = self.scan_interval.subscribe();
        self.lock()
            .insert(device_id.to_owned(), Registered { id, sender });
        AgentConnection {
            device_id: device_id.to_owned(),
            id,
            commands,
            scan_interval,
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

    /// How many devices have a connection, without copying them as
    /// [`Self::connected`] does. It may count one that is closing.
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// Queues `message` for the device's connection, or says why it
    /// couldn't: the device isn't connected, or it is but so far behind that
    /// its backlog is full. Only the first means it is offline.
    pub fn send(&self, device_id: &str, message: impl Into<ToAgent>) -> Result<(), Unsent> {
        let connections = self.lock();
        let registered = connections.get(device_id).ok_or(Unsent::Offline)?;
        match registered.sender.try_send(message.into()) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    device_id,
                    "an Agent's backlog is full, so a message for it was not queued"
                );
                Err(Unsent::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Unsent::Offline),
        }
    }

    /// Holds a place in the backlog of the device's connection for one
    /// message, so a caller can record what it sends first and still be sure
    /// of a place. Says why there is none: the device isn't connected, or
    /// its backlog is full.
    pub fn reserve(&self, device_id: &str) -> Result<Reserved, Unsent> {
        let sender = self
            .lock()
            .get(device_id)
            .map(|registered| registered.sender.clone())
            .ok_or(Unsent::Offline)?;
        match sender.clone().try_reserve_owned() {
            Ok(permit) => Ok(Reserved { permit, sender }),
            Err(mpsc::error::TrySendError::Full(_)) => Err(Unsent::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Unsent::Offline),
        }
    }

    /// Has every connection send its Agent the update scan interval
    /// `interval_hours`, replacing one announced earlier that a connection
    /// has not sent yet. Nothing is queued, so a full backlog only delays
    /// it, and nothing is read.
    pub fn announce_scan_interval(&self, interval_hours: Option<u32>) -> Announced {
        self.scan_interval.send_replace(interval_hours);
        let connections = self.lock();
        let open = connections
            .values()
            .filter(|registered| !registered.sender.is_closed());
        let (mut connected, mut behind) = (0, 0);
        for registered in open {
            connected += 1;
            if registered.sender.capacity() == 0 {
                behind += 1;
            }
        }
        Announced { connected, behind }
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
        assert_eq!(
            hub.send("device-1", AgentCommand::Uninstall),
            Err(Unsent::Offline)
        );

        let mut first = hub.connect("device-1");
        assert!(hub.is_connected("device-1"));
        assert!(hub.is_current("device-1", first.id));
        assert_eq!(hub.send("device-1", AgentCommand::Uninstall), Ok(()));
        assert_eq!(first.commands.recv().await, uninstall());

        let mut second = hub.connect("device-1");
        assert_ne!(first.id, second.id);
        assert!(!hub.is_current("device-1", first.id));
        assert_eq!(first.commands.recv().await, None, "the old stream ends");
        assert_eq!(hub.send("device-1", AgentCommand::Uninstall), Ok(()));
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
            assert_eq!(hub.send("device-1", AgentCommand::Uninstall), Ok(()));
        }
        assert_eq!(
            hub.send("device-1", AgentCommand::Uninstall),
            Err(Unsent::Full),
            "a full backlog is not an offline device"
        );
        hub.disconnect("device-1");
        assert!(!hub.is_connected("device-1"));
        assert_eq!(
            hub.send("device-1", AgentCommand::Uninstall),
            Err(Unsent::Offline)
        );
        for _ in 0..COMMAND_BACKLOG {
            assert!(connection.commands.recv().await.is_some());
        }
        assert_eq!(connection.commands.recv().await, None);
    }

    #[tokio::test]
    async fn a_reserved_place_tells_a_full_backlog_from_an_offline_agent() {
        let hub = AgentHub::default();
        assert_eq!(hub.reserve("device-1").unwrap_err(), Unsent::Offline);
        let mut connection = hub.connect("device-1");
        let held = hub.reserve("device-1").unwrap();
        for _ in 1..COMMAND_BACKLOG {
            assert_eq!(hub.send("device-1", AgentCommand::Uninstall), Ok(()));
        }
        assert_eq!(
            hub.send("device-1", AgentCommand::Uninstall),
            Err(Unsent::Full),
            "the held place is taken"
        );
        assert_eq!(hub.reserve("device-1").unwrap_err(), Unsent::Full);
        assert!(held.send(AgentCommand::Uninstall));
        assert_eq!(hub.reserve("device-1").unwrap_err(), Unsent::Full);
        assert!(connection.commands.recv().await.is_some());
        let held = hub.reserve("device-1").unwrap();
        hub.disconnect("device-1");
        drop(connection);
        assert!(!held.send(AgentCommand::Uninstall), "it went away");
        assert_eq!(hub.reserve("device-1").unwrap_err(), Unsent::Offline);
    }

    fn schedule(interval_hours: Option<u32>) -> Option<ToAgent> {
        Some(ToAgent::Command(AgentCommand::SetUpdateScanSchedule {
            schedule: UpdateScanSchedule {
                interval_hours,
                last_scan: None,
            },
        }))
    }

    #[tokio::test]
    async fn the_newest_scan_interval_goes_ahead_of_a_full_backlog() {
        let hub = AgentHub::default();
        let mut behind = hub.connect("device-1");
        let mut ready = hub.connect("device-2");
        for _ in 0..COMMAND_BACKLOG {
            assert_eq!(hub.send("device-1", AgentCommand::Uninstall), Ok(()));
        }
        assert_eq!(
            hub.send("device-1", AgentCommand::Uninstall),
            Err(Unsent::Full)
        );

        let first = hub.announce_scan_interval(Some(6));
        // Read straight away by the connection with room, and only once.
        assert_eq!(ready.next_message().await, schedule(Some(6)));
        assert_eq!(
            hub.send("device-2", AgentCommand::CollectSystemInfo),
            Ok(())
        );
        assert_eq!(
            ready.next_message().await,
            Some(ToAgent::Command(AgentCommand::CollectSystemInfo))
        );
        let second = hub.announce_scan_interval(None);

        // The connection far behind sends only the newest, once, then its
        // backlog.
        assert_eq!(behind.next_message().await, schedule(None));
        for _ in 0..COMMAND_BACKLOG {
            assert_eq!(behind.next_message().await, uninstall());
        }
        assert_eq!(
            hub.send("device-1", AgentCommand::CollectSystemInfo),
            Ok(())
        );
        assert_eq!(
            behind.next_message().await,
            Some(ToAgent::Command(AgentCommand::CollectSystemInfo))
        );
        assert_eq!(ready.next_message().await, schedule(None));
        assert_eq!(
            first,
            Announced {
                connected: 2,
                behind: 1
            }
        );
        assert_eq!(second, first);

        // A connection made after an announcement owes nothing: its Agent
        // is greeted with the interval as it then is.
        let mut later = hub.connect("device-3");
        assert_eq!(hub.send("device-3", AgentCommand::Uninstall), Ok(()));
        assert_eq!(later.next_message().await, uninstall());
        hub.disconnect("device-3");
        assert_eq!(later.next_message().await, None);
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
