//! Device presence for the website: the enrolled devices, which are online,
//! and the companies and tags they have, as a snapshot followed by numbered
//! changes.
//!
//! Whatever changes a device (its Agent connecting or disconnecting, an
//! enrollment, a deletion, a move to another company, tagging) calls
//! [`Presence::refresh`], which reads the device's current state and
//! publishes it if it differs from what was last published. Changes to a
//! company or tag call [`Presence::refresh_company`] or
//! [`Presence::refresh_tag`] the same way. Reading the truth
//! rather than trusting the caller keeps presence right however the changes
//! interleave: the last refresh sees the last change. Snapshots publish whatever they find changed first, so every
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
    db::{
        self, Database,
        tables::{Agents, Companies, Tags},
    },
    notes, system_info,
    tags::{self, Tag},
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
    /// The operating system: `windows`, `macos` or `linux`.
    pub platform: String,
    /// The Agent's control connection is open.
    pub connected: bool,
    /// The release an offline Agent said it is installing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updating_to: Option<String>,
    /// The company the device belongs to, if any.
    pub company_id: Option<String>,
    /// The device's tags, sorted by ID.
    pub tag_ids: Vec<String>,
    /// The current revision of the device's notes; 0 without notes.
    pub notes_revision: i64,
    pub created_at: i64,
    /// What the device list shows and searches from the device's latest
    /// system information snapshot, once it has one.
    pub system: Option<Box<system_info::Summary>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct PresenceCompany {
    pub id: String,
    pub name: String,
}

/// What the website's event socket sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceEvent {
    Snapshot {
        revision: u64,
        agents: Vec<PresenceAgent>,
        companies: Vec<PresenceCompany>,
        tags: Vec<Tag>,
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
    CompanyUpsert {
        revision: u64,
        company: PresenceCompany,
    },
    CompanyDeleted {
        revision: u64,
        company_id: String,
    },
    TagUpsert {
        revision: u64,
        tag: Tag,
    },
    TagDeleted {
        revision: u64,
        tag_id: String,
    },
}

impl PresenceEvent {
    pub fn revision(&self) -> u64 {
        match self {
            Self::Snapshot { revision, .. }
            | Self::AgentUpsert { revision, .. }
            | Self::AgentDeleted { revision, .. }
            | Self::CompanyUpsert { revision, .. }
            | Self::CompanyDeleted { revision, .. }
            | Self::TagUpsert { revision, .. }
            | Self::TagDeleted { revision, .. } => *revision,
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
    /// The same for each company.
    published_companies: HashMap<String, Option<PresenceCompany>>,
    /// The same for each tag.
    published_tags: HashMap<String, Option<Tag>>,
    /// Updates Agents announced before going offline.
    updating: HashMap<String, Update>,
}

#[derive(Debug)]
struct Update {
    version: String,
    until: Instant,
}

type AgentRow = (String, String, String, Option<String>, i64);

/// What presence shows about a device besides its row.
#[derive(Default)]
struct Details {
    tag_ids: Vec<String>,
    notes_revision: i64,
    system: Option<system_info::Summary>,
}

fn agent_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            Agents::Id,
            Agents::Name,
            Agents::Platform,
            Agents::CompanyId,
            Agents::CreatedAt,
        ])
        .from(Agents::Table)
        .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
        .to_owned()
}

fn company_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([Companies::Id, Companies::Name])
        .from(Companies::Table)
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

    /// Every device and company as they are now.
    pub async fn snapshot(&self) -> db::Result<PresenceEvent> {
        let mut state = self.inner.state.lock().await;
        let rows: Vec<AgentRow> = self.inner.database.fetch_all(&agent_select()).await?;
        let mut systems = system_info::summaries(&self.inner.database, None).await?;
        let mut device_tags = tags::device_tag_ids(&mut &self.inner.database, None).await?;
        let notes = notes::revisions(&mut &self.inner.database, None).await?;
        let mut companies: Vec<PresenceCompany> =
            self.inner.database.fetch_all(&company_select()).await?;
        let tags = tags::load(&mut &self.inner.database, None).await?;
        state.prune_updates();
        // New companies and tags first and deleted ones last, so a device
        // never names a company or tag subscribers don't know.
        let gone_companies = state
            .published_companies
            .iter()
            .filter(|(id, company)| company.is_some() && !companies.iter().any(|c| &c.id == *id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let gone_tags = state
            .published_tags
            .iter()
            .filter(|(id, tag)| tag.is_some() && !tags.iter().any(|t| &t.id == *id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for company in &companies {
            self.publish_company(&mut state, &company.id.clone(), Some(company.clone()));
        }
        for tag in &tags {
            self.publish_tag(&mut state, &tag.id.clone(), Some(tag.clone()));
        }
        let mut agents = rows
            .into_iter()
            .map(|row| {
                let details = Details {
                    system: systems.remove(&row.0),
                    tag_ids: device_tags.remove(&row.0).unwrap_or_default(),
                    notes_revision: notes.get(&row.0).copied().unwrap_or_default(),
                };
                self.agent(&state, row, details)
            })
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
        for id in gone_companies {
            self.publish_company(&mut state, &id, None);
        }
        for id in gone_tags {
            self.publish_tag(&mut state, &id, None);
        }
        sort(&mut agents);
        sort_companies(&mut companies);
        Ok(PresenceEvent::Snapshot {
            revision: state.revision,
            agents,
            companies,
            tags,
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
        let read: db::Result<Option<(AgentRow, Details)>> = async {
            let Some(row) = row? else {
                return Ok(None);
            };
            let mut database = &self.inner.database;
            let details = Details {
                system: system_info::summaries(database, Some(device_id))
                    .await?
                    .remove(device_id),
                tag_ids: tags::device_tag_ids(&mut database, Some(device_id))
                    .await?
                    .remove(device_id)
                    .unwrap_or_default(),
                notes_revision: notes::revisions(&mut database, Some(device_id))
                    .await?
                    .remove(device_id)
                    .unwrap_or_default(),
            };
            Ok(Some((row, details)))
        }
        .await;
        match read {
            Ok(found) => {
                state.prune_updates();
                let agent = found.map(|(row, details)| self.agent(&state, row, details));
                self.publish(&mut state, device_id, agent);
            }
            Err(error) => {
                tracing::warn!(device_id, %error, "could not read a device to publish its presence")
            }
        }
    }

    /// Publishes the company's current state, if it changed. Refresh the
    /// devices a change moved first, so no device names a deleted company.
    pub async fn refresh_company(&self, company_id: &str) {
        let mut state = self.inner.state.lock().await;
        let row: db::Result<Option<PresenceCompany>> = self
            .inner
            .database
            .fetch_optional(
                &company_select()
                    .and_where(Expr::col(Companies::Id).eq(company_id))
                    .to_owned(),
            )
            .await;
        match row {
            Ok(company) => self.publish_company(&mut state, company_id, company),
            Err(error) => {
                tracing::warn!(company_id, %error, "could not read a company to publish it")
            }
        }
    }

    /// Publishes the tag's current state, if it changed. Refresh the devices
    /// a change untagged first, so no device names a deleted tag.
    pub async fn refresh_tag(&self, tag_id: &str) {
        let mut state = self.inner.state.lock().await;
        let row: db::Result<Option<Tag>> = self
            .inner
            .database
            .fetch_optional(
                &tags::select()
                    .and_where(Expr::col(Tags::Id).eq(tag_id))
                    .to_owned(),
            )
            .await;
        match row {
            Ok(tag) => self.publish_tag(&mut state, tag_id, tag),
            Err(error) => tracing::warn!(tag_id, %error, "could not read a tag to publish it"),
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

    /// The devices whose Agent went offline to install an update and isn't
    /// back yet.
    pub async fn updating_devices(&self) -> std::collections::HashSet<String> {
        let mut state = self.inner.state.lock().await;
        state.prune_updates();
        state.updating.keys().cloned().collect()
    }

    /// Asks event sockets to check their user's access again.
    pub fn recheck_access(&self) {
        self.inner.access.send_modify(|generation| *generation += 1);
    }

    pub fn access_changes(&self) -> watch::Receiver<u64> {
        self.inner.access.subscribe()
    }

    fn agent(
        &self,
        state: &State,
        (id, name, platform, company_id, created_at): AgentRow,
        details: Details,
    ) -> PresenceAgent {
        let connected = self.inner.hub.is_connected(&id);
        PresenceAgent {
            updating_to: (!connected)
                .then(|| state.updating.get(&id).map(|update| update.version.clone()))
                .flatten(),
            connected,
            id,
            name,
            platform,
            company_id,
            tag_ids: details.tag_ids,
            notes_revision: details.notes_revision,
            created_at,
            system: details.system.map(Box::new),
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

    fn publish_company(
        &self,
        state: &mut State,
        company_id: &str,
        company: Option<PresenceCompany>,
    ) {
        if state.published_companies.get(company_id) == Some(&company) {
            return;
        }
        state.revision += 1;
        let event = match &company {
            Some(company) => PresenceEvent::CompanyUpsert {
                revision: state.revision,
                company: company.clone(),
            },
            None => PresenceEvent::CompanyDeleted {
                revision: state.revision,
                company_id: company_id.to_owned(),
            },
        };
        state
            .published_companies
            .insert(company_id.to_owned(), company);
        let _ = self.inner.events.send(Arc::new(event));
    }

    fn publish_tag(&self, state: &mut State, tag_id: &str, tag: Option<Tag>) {
        if state.published_tags.get(tag_id) == Some(&tag) {
            return;
        }
        state.revision += 1;
        let event = match &tag {
            Some(tag) => PresenceEvent::TagUpsert {
                revision: state.revision,
                tag: tag.clone(),
            },
            None => PresenceEvent::TagDeleted {
                revision: state.revision,
                tag_id: tag_id.to_owned(),
            },
        };
        state.published_tags.insert(tag_id.to_owned(), tag);
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

/// By name ignoring case, then by ID.
pub fn sort_companies(companies: &mut [PresenceCompany]) {
    companies.sort_by_cached_key(|company| (company.name.to_lowercase(), company.id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str, connected: bool) -> PresenceAgent {
        PresenceAgent {
            id: id.into(),
            name: name.into(),
            platform: "windows".into(),
            connected,
            updating_to: None,
            company_id: None,
            tag_ids: vec![],
            notes_revision: 0,
            created_at: 0,
            system: None,
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
                "agent": { "id": "a", "name": "Desk", "platform": "windows", "connected": false, "updating_to": "1.2.3", "company_id": null, "tag_ids": [], "notes_revision": 0, "created_at": 0, "system": null }
            })
        );
        let company = PresenceEvent::CompanyUpsert {
            revision: 9,
            company: PresenceCompany {
                id: "c".into(),
                name: "Acme".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(company).unwrap(),
            serde_json::json!({ "type": "company_upsert", "revision": 9, "company": { "id": "c", "name": "Acme" } })
        );
        let removed = PresenceEvent::CompanyDeleted {
            revision: 10,
            company_id: "c".into(),
        };
        assert_eq!(
            serde_json::to_value(removed).unwrap(),
            serde_json::json!({ "type": "company_deleted", "revision": 10, "company_id": "c" })
        );
    }
}
