//! Device presence for the website: the enrolled devices and which are
//! online, as a snapshot followed by numbered changes.
//!
//! Whatever changes a device (its Agent connecting or disconnecting, an
//! enrollment, a deletion) calls [`Presence::refresh`], which reads the
//! device's current state and publishes it if it differs from what was last
//! published. Reading the truth rather than trusting the caller keeps
//! presence right however the changes interleave: the last refresh sees the
//! last change. Snapshots publish whatever they find changed first, so every
//! subscriber converges on the same state at the same revision.
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use sea_query::{Expr, ExprTrait, Query};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast, watch};

use super::AgentHub;
use crate::{
    db::{self, Database, tables::Agents},
    time::now_ms,
};

/// How long an Agent that went offline to install an update shows as
/// updating. An update normally restarts the Agent within a minute; one
/// that doesn't come back shows as offline.
pub const UPDATE_GRACE: Duration = Duration::from_secs(10 * 60);
/// Changes a subscriber may fall behind by before it gets a new snapshot.
const EVENT_BACKLOG: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresenceAgent {
    pub id: String,
    pub name: String,
    /// The Agent's control connection is open.
    pub connected: bool,
    /// The release an offline Agent said it is installing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updating_to: Option<String>,
    pub created_at: i64,
}

/// What the website's event socket sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceEvent {
    Snapshot {
        revision: u64,
        agents: Vec<PresenceAgent>,
        generated_at_unix_ms: i64,
    },
    AgentUpsert {
        revision: u64,
        agent: PresenceAgent,
    },
    AgentDeleted {
        revision: u64,
        agent_id: String,
    },
}

impl PresenceEvent {
    pub fn revision(&self) -> u64 {
        match self {
            Self::Snapshot { revision, .. }
            | Self::AgentUpsert { revision, .. }
            | Self::AgentDeleted { revision, .. } => *revision,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Presence {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    database: Database,
    hub: AgentHub,
    update_grace: Duration,
    state: Mutex<State>,
    events: broadcast::Sender<Arc<PresenceEvent>>,
    /// Bumped when users' access may have changed, so event sockets check
    /// their user's session and permissions again at once.
    access: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct State {
    revision: u64,
    /// What subscribers were last told about each device; `None` once it
    /// was deleted.
    published: HashMap<String, Option<PresenceAgent>>,
    /// Updates Agents announced before going offline.
    updating: HashMap<String, Update>,
}

#[derive(Debug)]
struct Update {
    version: String,
    until: Instant,
}

type AgentRow = (String, String, i64);

fn agent_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([Agents::Id, Agents::Name, Agents::CreatedAt])
        .from(Agents::Table)
        .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
        .to_owned()
}

impl Presence {
    pub fn new(database: Database, hub: AgentHub, update_grace: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                database,
                hub,
                update_grace,
                state: Mutex::default(),
                events: broadcast::channel(EVENT_BACKLOG).0,
                access: watch::channel(0).0,
            }),
        }
    }

    /// Changes from now on. Take the snapshot after subscribing, and skip
    /// changes it already includes.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<PresenceEvent>> {
        self.inner.events.subscribe()
    }

    /// Every device as it is now.
    pub async fn snapshot(&self) -> db::Result<PresenceEvent> {
        let mut state = self.inner.state.lock().await;
        let rows: Vec<AgentRow> = self.inner.database.fetch_all(&agent_select()).await?;
        state.prune_updates();
        let mut agents = rows
            .into_iter()
            .map(|row| self.agent(&state, row))
            .collect::<Vec<_>>();
        let gone = state
            .published
            .iter()
            .filter(|(id, agent)| agent.is_some() && !agents.iter().any(|a| &a.id == *id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in gone {
            self.publish(&mut state, &id, None);
        }
        for agent in &agents {
            self.publish(&mut state, &agent.id.clone(), Some(agent.clone()));
        }
        sort(&mut agents);
        Ok(PresenceEvent::Snapshot {
            revision: state.revision,
            agents,
            generated_at_unix_ms: now_ms(),
        })
    }

    /// Publishes the device's current state, if it changed.
    pub async fn refresh(&self, device_id: &str) {
        let mut state = self.inner.state.lock().await;
        let row: db::Result<Option<AgentRow>> = self
            .inner
            .database
            .fetch_optional(
                &agent_select()
                    .and_where(Expr::col(Agents::Id).eq(device_id))
                    .to_owned(),
            )
            .await;
        match row {
            Ok(row) => {
                state.prune_updates();
                let agent = row.map(|row| self.agent(&state, row));
                self.publish(&mut state, device_id, agent);
            }
            Err(error) => {
                tracing::warn!(device_id, %error, "could not read a device to publish its presence")
            }
        }
    }

    /// The device's Agent connected; any update it was installing is over.
    pub async fn connected(&self, device_id: &str) {
        self.inner.state.lock().await.updating.remove(device_id);
        self.refresh(device_id).await;
    }

    /// The device's Agent disconnected. If it said it was updating, it shows
    /// as updating until it reconnects or the grace period passes.
    pub async fn disconnected(&self, device_id: &str) {
        self.refresh(device_id).await;
        let until = self
            .inner
            .state
            .lock()
            .await
            .updating
            .get(device_id)
            .map(|update| update.until);
        if let Some(until) = until {
            let presence = self.clone();
            let device_id = device_id.to_owned();
            tokio::spawn(async move {
                tokio::time::sleep_until(until.into()).await;
                presence.refresh(&device_id).await;
            });
        }
    }

    /// The connected Agent is about to go offline to install `version`.
    pub async fn updating(&self, device_id: &str, version: String) {
        self.inner.state.lock().await.updating.insert(
            device_id.to_owned(),
            Update {
                version,
                until: Instant::now() + self.inner.update_grace,
            },
        );
    }

    /// Asks event sockets to check their user's access again.
    pub fn recheck_access(&self) {
        self.inner.access.send_modify(|generation| *generation += 1);
    }

    pub fn access_changes(&self) -> watch::Receiver<u64> {
        self.inner.access.subscribe()
    }

    fn agent(&self, state: &State, (id, name, created_at): AgentRow) -> PresenceAgent {
        let connected = self.inner.hub.is_connected(&id);
        PresenceAgent {
            updating_to: (!connected)
                .then(|| state.updating.get(&id).map(|update| update.version.clone()))
                .flatten(),
            connected,
            id,
            name,
            created_at,
        }
    }

    fn publish(&self, state: &mut State, device_id: &str, agent: Option<PresenceAgent>) {
        if state.published.get(device_id) == Some(&agent) {
            return;
        }
        state.revision += 1;
        let event = match &agent {
            Some(agent) => PresenceEvent::AgentUpsert {
                revision: state.revision,
                agent: agent.clone(),
            },
            None => PresenceEvent::AgentDeleted {
                revision: state.revision,
                agent_id: device_id.to_owned(),
            },
        };
        state.published.insert(device_id.to_owned(), agent);
        // Nobody listening is fine.
        let _ = self.inner.events.send(Arc::new(event));
    }
}

impl State {
    fn prune_updates(&mut self) {
        let now = Instant::now();
        self.updating.retain(|_, update| update.until > now);
    }
}

/// Online devices first, then by name ignoring case, then by ID.
pub fn sort(agents: &mut [PresenceAgent]) {
    agents.sort_by_cached_key(|agent| {
        (
            !agent.connected,
            agent.name.to_lowercase(),
            agent.id.clone(),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str, connected: bool) -> PresenceAgent {
        PresenceAgent {
            id: id.into(),
            name: name.into(),
            connected,
            updating_to: None,
            created_at: 0,
        }
    }

    #[test]
    fn agents_are_sorted_online_first_then_by_name() {
        let mut agents = vec![
            agent("b", "Zulu", false),
            agent("c", "alpha", true),
            agent("a", "Alpha", true),
        ];
        sort(&mut agents);
        let ids = agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["a", "c", "b"]);
    }

    #[test]
    fn events_use_the_website_wire_format() {
        let deleted = PresenceEvent::AgentDeleted {
            revision: 7,
            agent_id: "endpoint-1".into(),
        };
        assert_eq!(
            serde_json::to_value(deleted).unwrap(),
            serde_json::json!({ "type": "agent_deleted", "revision": 7, "agent_id": "endpoint-1" })
        );
        let mut updating = agent("a", "Desk", false);
        updating.updating_to = Some("1.2.3".into());
        let upsert = PresenceEvent::AgentUpsert {
            revision: 8,
            agent: updating,
        };
        assert_eq!(
            serde_json::to_value(upsert).unwrap(),
            serde_json::json!({
                "type": "agent_upsert",
                "revision": 8,
                "agent": { "id": "a", "name": "Desk", "connected": false, "updating_to": "1.2.3", "created_at": 0 }
            })
        );
    }
}
