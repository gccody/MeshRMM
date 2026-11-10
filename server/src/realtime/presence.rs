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
//! interleave: the last refresh sees the last change. Snapshots publish
//! whatever they find changed first, so every subscriber converges on the
//! same state at the same revision.
//!
//! Reads run outside the lock that publishing takes, so a slow read holds up
//! no other publisher. Each read takes a ticket from a counter before it
//! starts, and what is published for a device, company or tag remembers the
//! ticket of the read it came from; a read with an older ticket than that is
//! dropped. A read sees every change made before its ticket was taken, so
//! however reads finish, what stays published comes from a read that began
//! after the last change: the last refresh still sees the last change. A
//! snapshot keeps what a newer read published in place of what it read
//! itself, so it never shows anything older than an event already sent, and
//! once it has published what it found changed it is exactly what is
//! published at its revision. Whether an Agent is connected or updating is
//! decided when publishing, under the lock, and each change to either is
//! followed by a refresh.
//!
//! Snapshots asked for while one is being read share it until it fixes its
//! revision: a requester that subscribed before then gets every later change
//! from its subscription, and was sent no change the snapshot lacks.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
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
    http::ApiError,
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

/// A snapshot couldn't read the database. Everyone who shared its build
/// gets the same error.
#[derive(Debug, Clone, thiserror::Error)]
#[error(transparent)]
pub struct SnapshotFailed(Arc<sqlx::Error>);

impl From<SnapshotFailed> for ApiError {
    fn from(error: SnapshotFailed) -> Self {
        if matches!(*error.0, sqlx::Error::PoolTimedOut) {
            return Self::server_busy();
        }
        tracing::error!(%error, "could not read the devices for a presence snapshot");
        Self::internal()
    }
}

type Built = Result<Arc<PresenceEvent>, SnapshotFailed>;
/// A snapshot being built; it holds the result once there is one.
type Build = watch::Receiver<Option<Built>>;

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
    /// Numbers reads in the order they start.
    tickets: AtomicU64,
    /// The snapshot being built that requesters may still share.
    building: std::sync::Mutex<Option<Build>>,
    events: broadcast::Sender<Arc<PresenceEvent>>,
    /// Bumped when users' access may have changed, so event sockets check
    /// their user's session and permissions again at once.
    access: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct State {
    revision: u64,
    /// What subscribers were last told about each device.
    published: HashMap<String, Published<PresenceAgent>>,
    /// The same for each company.
    published_companies: HashMap<String, Published<PresenceCompany>>,
    /// The same for each tag.
    published_tags: HashMap<String, Published<Tag>>,
    /// Updates Agents announced before going offline.
    updating: HashMap<String, Update>,
}

/// What subscribers were last told about one device, company or tag.
#[derive(Debug)]
struct Published<T> {
    /// The ticket of the read it came from.
    read: u64,
    /// `None` once it was deleted.
    value: Option<T>,
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

/// One device as a refresh read it.
struct DeviceRead {
    ticket: u64,
    device_id: String,
    found: db::Result<Option<(AgentRow, Details)>>,
}

/// Everything a snapshot reads.
struct FleetRead {
    ticket: u64,
    rows: Vec<AgentRow>,
    systems: HashMap<String, system_info::Summary>,
    device_tags: HashMap<String, Vec<String>>,
    notes: HashMap<String, i64>,
    companies: Vec<PresenceCompany>,
    tags: Vec<Tag>,
}

/// Stops requesters joining a snapshot's build when dropped: once the build
/// fixes its revision, or if it ends without doing so.
struct Closing<'a> {
    building: &'a std::sync::Mutex<Option<Build>>,
    build: &'a Build,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        let mut building = self.building.lock().unwrap_or_else(PoisonError::into_inner);
        if building
            .as_ref()
            .is_some_and(|open| open.same_channel(self.build))
        {
            *building = None;
        }
    }
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
                tickets: AtomicU64::new(0),
                building: std::sync::Mutex::default(),
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

    /// Every device, company and tag as they are now. Requesters that ask
    /// while a snapshot is being read share it.
    pub async fn snapshot(&self) -> Result<Arc<PresenceEvent>, SnapshotFailed> {
        loop {
            let (leader, mut build) = {
                let mut building = self
                    .inner
                    .building
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                match &*building {
                    Some(build) => (None, build.clone()),
                    None => {
                        let (sender, build) = watch::channel(None);
                        *building = Some(build.clone());
                        (Some(sender), build)
                    }
                }
            };
            if let Some(sender) = leader {
                let built = self.build(&build).await;
                sender.send_replace(Some(built.clone()));
                return built;
            }
            // Otherwise the requester building it went away first; build
            // another.
            if let Ok(built) = build.wait_for(Option::is_some).await {
                return built.clone().expect("waited for a result");
            }
        }
    }

    async fn build(&self, build: &Build) -> Built {
        let closing = Closing {
            building: &self.inner.building,
            build,
        };
        let read = self
            .read_fleet()
            .await
            .map_err(|error| SnapshotFailed(Arc::new(error)))?;
        Ok(self.publish_fleet(read, Some(closing)).await)
    }

    async fn read_fleet(&self) -> db::Result<FleetRead> {
        let ticket = self.ticket();
        let mut database = &self.inner.database;
        Ok(FleetRead {
            ticket,
            rows: database.fetch_all(&agent_select()).await?,
            systems: system_info::summaries(database, None).await?,
            device_tags: tags::device_tag_ids(&mut database, None).await?,
            notes: notes::revisions(&mut database, None).await?,
            companies: database.fetch_all(&company_select()).await?,
            tags: tags::load(&mut database, None).await?,
        })
    }

    /// Publishes what the read found changed, unless something newer was
    /// published for it since the read began, and returns everything as now
    /// published. Dropping `closing` once the lock is held lets later
    /// requesters build a newer snapshot.
    async fn publish_fleet(
        &self,
        read: FleetRead,
        closing: Option<Closing<'_>>,
    ) -> Arc<PresenceEvent> {
        let FleetRead {
            ticket,
            rows,
            mut systems,
            mut device_tags,
            notes,
            companies,
            tags,
        } = read;
        let mut state = self.inner.state.lock().await;
        drop(closing);
        state.prune_updates();
        let agents = rows
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
        let gone = missing(&state.published, agents.iter().map(|a| a.id.as_str()));
        let gone_companies = missing(
            &state.published_companies,
            companies.iter().map(|c| c.id.as_str()),
        );
        let gone_tags = missing(&state.published_tags, tags.iter().map(|t| t.id.as_str()));
        // New companies and tags first and deleted ones last, so a device
        // never names a company or tag subscribers don't know.
        for company in companies {
            self.publish_company(&mut state, ticket, &company.id.clone(), Some(company));
        }
        for tag in tags {
            self.publish_tag(&mut state, ticket, &tag.id.clone(), Some(tag));
        }
        for id in gone {
            self.publish(&mut state, ticket, &id, None);
        }
        for agent in agents {
            self.publish(&mut state, ticket, &agent.id.clone(), Some(agent));
        }
        for id in gone_companies {
            self.publish_company(&mut state, ticket, &id, None);
        }
        for id in gone_tags {
            self.publish_tag(&mut state, ticket, &id, None);
        }
        let mut agents = present(&state.published);
        let mut companies = present(&state.published_companies);
        let mut tags = present(&state.published_tags);
        sort(&mut agents);
        sort_companies(&mut companies);
        tags::sort(&mut tags);
        Arc::new(PresenceEvent::Snapshot {
            revision: state.revision,
            agents,
            companies,
            tags,
            generated_at_unix_ms: now_ms(),
        })
    }

    /// Publishes the device's current state, if it changed.
    pub async fn refresh(&self, device_id: &str) {
        let read = self.read_device(device_id).await;
        self.publish_device(read).await;
    }

    async fn read_device(&self, device_id: &str) -> DeviceRead {
        let ticket = self.ticket();
        let found = async {
            let row: Option<AgentRow> = self
                .inner
                .database
                .fetch_optional(
                    &agent_select()
                        .and_where(Expr::col(Agents::Id).eq(device_id))
                        .to_owned(),
                )
                .await?;
            let Some(row) = row else {
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
        DeviceRead {
            ticket,
            device_id: device_id.to_owned(),
            found,
        }
    }

    async fn publish_device(&self, read: DeviceRead) {
        let device_id = read.device_id.as_str();
        let found = match read.found {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(device_id, %error, "could not read a device to publish its presence");
                return;
            }
        };
        let mut state = self.inner.state.lock().await;
        state.prune_updates();
        let agent = found.map(|(row, details)| self.agent(&state, row, details));
        self.publish(&mut state, read.ticket, device_id, agent);
    }

    /// Publishes the company's current state, if it changed. Refresh the
    /// devices a change moved first, so no device names a deleted company.
    pub async fn refresh_company(&self, company_id: &str) {
        let ticket = self.ticket();
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
            Ok(company) => {
                let mut state = self.inner.state.lock().await;
                self.publish_company(&mut state, ticket, company_id, company);
            }
            Err(error) => {
                tracing::warn!(company_id, %error, "could not read a company to publish it")
            }
        }
    }

    /// Publishes the tag's current state, if it changed. Refresh the devices
    /// a change untagged first, so no device names a deleted tag.
    pub async fn refresh_tag(&self, tag_id: &str) {
        let ticket = self.ticket();
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
            Ok(tag) => {
                let mut state = self.inner.state.lock().await;
                self.publish_tag(&mut state, ticket, tag_id, tag);
            }
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
    pub async fn updating_devices(&self) -> HashSet<String> {
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

    /// Numbers a read about to start. Acquire-release, so a read with a
    /// later ticket starts after every change made before an earlier one
    /// was taken.
    fn ticket(&self) -> u64 {
        self.inner.tickets.fetch_add(1, Ordering::AcqRel) + 1
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

    fn publish(
        &self,
        state: &mut State,
        ticket: u64,
        device_id: &str,
        agent: Option<PresenceAgent>,
    ) {
        if !record(&mut state.published, device_id, ticket, &agent) {
            return;
        }
        state.revision += 1;
        let event = match agent {
            Some(agent) => PresenceEvent::AgentUpsert {
                revision: state.revision,
                agent,
            },
            None => PresenceEvent::AgentDeleted {
                revision: state.revision,
                agent_id: device_id.to_owned(),
            },
        };
        // Nobody listening is fine.
        let _ = self.inner.events.send(Arc::new(event));
    }

    fn publish_company(
        &self,
        state: &mut State,
        ticket: u64,
        company_id: &str,
        company: Option<PresenceCompany>,
    ) {
        if !record(&mut state.published_companies, company_id, ticket, &company) {
            return;
        }
        state.revision += 1;
        let event = match company {
            Some(company) => PresenceEvent::CompanyUpsert {
                revision: state.revision,
                company,
            },
            None => PresenceEvent::CompanyDeleted {
                revision: state.revision,
                company_id: company_id.to_owned(),
            },
        };
        let _ = self.inner.events.send(Arc::new(event));
    }

    fn publish_tag(&self, state: &mut State, ticket: u64, tag_id: &str, tag: Option<Tag>) {
        if !record(&mut state.published_tags, tag_id, ticket, &tag) {
            return;
        }
        state.revision += 1;
        let event = match tag {
            Some(tag) => PresenceEvent::TagUpsert {
                revision: state.revision,
                tag,
            },
            None => PresenceEvent::TagDeleted {
                revision: state.revision,
                tag_id: tag_id.to_owned(),
            },
        };
        let _ = self.inner.events.send(Arc::new(event));
    }
}

impl State {
    fn prune_updates(&mut self) {
        let now = Instant::now();
        self.updating.retain(|_, update| update.until > now);
    }
}

/// Records `value`, read with `ticket`, as `id`'s state, and returns whether
/// subscribers must be told. A read older than the one last recorded is
/// dropped.
fn record<T: PartialEq + Clone>(
    published: &mut HashMap<String, Published<T>>,
    id: &str,
    ticket: u64,
    value: &Option<T>,
) -> bool {
    match published.get_mut(id) {
        Some(last) if last.read > ticket => false,
        Some(last) => {
            last.read = ticket;
            if &last.value == value {
                return false;
            }
            last.value.clone_from(value);
            true
        }
        None => {
            published.insert(
                id.to_owned(),
                Published {
                    read: ticket,
                    value: value.clone(),
                },
            );
            true
        }
    }
}

/// The IDs published as present that a read didn't find.
fn missing<'a, T>(
    published: &HashMap<String, Published<T>>,
    found: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let found = found.collect::<HashSet<_>>();
    published
        .iter()
        .filter(|(id, last)| last.value.is_some() && !found.contains(id.as_str()))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Everything published as present.
fn present<T: Clone>(published: &HashMap<String, Published<T>>) -> Vec<T> {
    published
        .values()
        .filter_map(|last| last.value.clone())
        .collect()
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
    use tokio::sync::broadcast::error::TryRecvError;

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

    async fn presence() -> Presence {
        // Long waits: a busy test machine can take seconds to open one.
        let wait = Duration::from_secs(30);
        let limits = crate::db::Limits {
            connections: 1,
            acquire_timeout: wait,
            write_timeout: wait,
        };
        let database = Database::connect_with("sqlite::memory:", limits)
            .await
            .expect("an in-memory database opens");
        database.migrate().await.expect("the schema applies");
        Presence::new(database, AgentHub::default(), UPDATE_GRACE)
    }

    async fn enroll(presence: &Presence, ids: &[&str]) {
        for chunk in ids.chunks(500) {
            let mut insert = Query::insert()
                .into_table(Agents::Table)
                .columns([
                    Agents::Id,
                    Agents::Name,
                    Agents::Platform,
                    Agents::AuthTokenHash,
                    Agents::CreatedAt,
                    Agents::UpdatedAt,
                ])
                .to_owned();
            for id in chunk {
                insert.values_panic([
                    (*id).into(),
                    (*id).into(),
                    "windows".into(),
                    "0".repeat(64).into(),
                    0.into(),
                    0.into(),
                ]);
            }
            presence.inner.database.execute(&insert).await.unwrap();
        }
    }

    async fn rename(presence: &Presence, id: &str, name: &str) {
        presence
            .inner
            .database
            .execute(
                &Query::update()
                    .table(Agents::Table)
                    .value(Agents::Name, name)
                    .and_where(Expr::col(Agents::Id).eq(id))
                    .to_owned(),
            )
            .await
            .unwrap();
    }

    /// Marks the devices `which` picks as deleted.
    async fn delete(presence: &Presence, which: Expr) {
        presence
            .inner
            .database
            .execute(
                &Query::update()
                    .table(Agents::Table)
                    .value(Agents::DeletionRequestedAt, 1)
                    .and_where(which)
                    .to_owned(),
            )
            .await
            .unwrap();
    }

    fn drain(events: &mut broadcast::Receiver<Arc<PresenceEvent>>) -> Vec<Arc<PresenceEvent>> {
        let mut drained = Vec::new();
        loop {
            match events.try_recv() {
                Ok(event) => drained.push(event),
                Err(TryRecvError::Empty) => return drained,
                Err(error) => panic!("{error}"),
            }
        }
    }

    /// A snapshot's revision and devices.
    fn devices(snapshot: &PresenceEvent) -> (u64, HashMap<String, PresenceAgent>) {
        let PresenceEvent::Snapshot {
            revision, agents, ..
        } = snapshot
        else {
            panic!("not a snapshot: {snapshot:?}");
        };
        let agents = agents
            .iter()
            .map(|agent| (agent.id.clone(), agent.clone()))
            .collect();
        (*revision, agents)
    }

    fn upserted(event: &PresenceEvent) -> &PresenceAgent {
        match event {
            PresenceEvent::AgentUpsert { agent, .. } => agent,
            _ => panic!("not an upsert: {event:?}"),
        }
    }

    #[tokio::test]
    async fn an_older_read_of_a_device_never_replaces_a_newer_one() {
        let presence = presence().await;
        enroll(&presence, &["a"]).await;
        let mut events = presence.subscribe();
        let older = presence.read_device("a").await;
        rename(&presence, "a", "Laptop").await;
        presence.refresh("a").await;
        let newer = drain(&mut events);
        assert_eq!(newer.len(), 1);
        assert_eq!(upserted(&newer[0]).name, "Laptop");

        // The read that began first finishes last, and is dropped.
        presence.publish_device(older).await;
        assert!(drain(&mut events).is_empty());
        let (revision, agents) = devices(&presence.snapshot().await.unwrap());
        assert_eq!(revision, newer[0].revision());
        assert_eq!(agents["a"].name, "Laptop");
    }

    #[tokio::test]
    async fn a_snapshot_keeps_what_was_published_after_its_read_began() {
        let presence = presence().await;
        enroll(&presence, &["a", "b", "c"]).await;
        presence.snapshot().await.unwrap();
        let mut events = presence.subscribe();
        let stale = presence.read_device("b").await;
        rename(&presence, "b", "Bravo").await;

        let fleet = presence.read_fleet().await.unwrap();
        // Published while the snapshot was being read: a renamed, c deleted
        // and d enrolled.
        rename(&presence, "a", "Alpha").await;
        presence.refresh("a").await;
        delete(&presence, Expr::col(Agents::Id).eq("c")).await;
        presence.refresh("c").await;
        enroll(&presence, &["d"]).await;
        presence.refresh("d").await;
        let before = drain(&mut events);
        assert_eq!(before.len(), 3, "{before:?}");

        let snapshot = presence.publish_fleet(fleet, None).await;
        // It publishes only what nobody published since its read began: b's
        // new name, which no refresh announced.
        let found = drain(&mut events);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(upserted(&found[0]).name, "Bravo");
        assert_eq!(found[0].revision(), before[2].revision() + 1);
        let (revision, agents) = devices(&snapshot);
        assert_eq!(revision, found[0].revision());
        let mut ids = agents.keys().map(String::as_str).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, ["a", "b", "d"], "c stays deleted, d is there");
        assert_eq!(agents["a"].name, "Alpha");
        assert_eq!(agents["b"].name, "Bravo");

        // An older read of b doesn't undo the snapshot's.
        presence.publish_device(stale).await;
        assert!(drain(&mut events).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshots_taken_during_refreshes_match_the_events_around_them() {
        let presence = presence().await;
        let ids = (0..8).map(|i| format!("d{i}")).collect::<Vec<_>>();
        enroll(
            &presence,
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await;
        let mut events = presence.subscribe();
        let (start, mut replayed) = devices(&presence.snapshot().await.unwrap());

        let mut tasks = tokio::task::JoinSet::new();
        for writer in 0..3 {
            let presence = presence.clone();
            let ids = ids.clone();
            tasks.spawn(async move {
                for step in 0..15 {
                    let id = &ids[(writer * 5 + step * 3) % ids.len()];
                    rename(&presence, id, &format!("w{writer}s{step}")).await;
                    presence.refresh(id).await;
                }
                Vec::new()
            });
        }
        for _ in 0..2 {
            let presence = presence.clone();
            tasks.spawn(async move {
                let mut taken = Vec::new();
                for _ in 0..10 {
                    taken.push(presence.snapshot().await.unwrap());
                    tokio::task::yield_now().await;
                }
                taken
            });
        }
        let mut snapshots = tasks
            .join_all()
            .await
            .concat()
            .iter()
            .map(|snapshot| devices(snapshot))
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|(revision, _)| *revision);
        let log = drain(&mut events);
        let log = log.iter().filter(|event| event.revision() > start);

        // Each snapshot is what the events up to its revision make.
        let mut log = log.peekable();
        let mut last = start;
        for (revision, agents) in &snapshots {
            while let Some(event) = log.next_if(|event| event.revision() <= *revision) {
                assert!(event.revision() > last, "revisions strictly increase");
                last = event.revision();
                let agent = upserted(event);
                replayed.insert(agent.id.clone(), agent.clone());
            }
            assert_eq!(&replayed, agents, "the snapshot at {revision}");
        }
        for event in log {
            assert!(event.revision() > last, "revisions strictly increase");
            last = event.revision();
            let agent = upserted(event);
            replayed.insert(agent.id.clone(), agent.clone());
        }
        // The refreshes left the latest names published: a new snapshot
        // finds nothing to change.
        let (revision, agents) = devices(&presence.snapshot().await.unwrap());
        assert_eq!(revision, last);
        assert_eq!(agents, replayed);
        assert!(drain(&mut events).is_empty());
    }

    #[tokio::test]
    async fn snapshots_asked_for_together_share_one_build() {
        let presence = presence().await;
        enroll(&presence, &["a"]).await;
        let together = futures_util::future::join_all((0..10).map(|_| presence.snapshot())).await;
        let first = together[0].as_ref().unwrap();
        for snapshot in &together {
            assert!(Arc::ptr_eq(snapshot.as_ref().unwrap(), first));
        }

        // One asked for after that build gets a new one.
        rename(&presence, "a", "Laptop").await;
        presence.refresh("a").await;
        let later = presence.snapshot().await.unwrap();
        assert!(!Arc::ptr_eq(&later, first));
        let (revision, agents) = devices(&later);
        assert!(revision > first.revision());
        assert_eq!(agents["a"].name, "Laptop");
    }

    #[tokio::test]
    async fn snapshots_find_thousands_of_deleted_devices_and_companies() {
        let presence = presence().await;
        let ids = (0..5000).map(|i| format!("d{i:04}")).collect::<Vec<_>>();
        enroll(
            &presence,
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await;
        for chunk in (0..1000).collect::<Vec<_>>().chunks(500) {
            let mut insert = Query::insert()
                .into_table(Companies::Table)
                .columns([
                    Companies::Id,
                    Companies::Name,
                    Companies::CreatedAt,
                    Companies::UpdatedAt,
                ])
                .to_owned();
            for i in chunk {
                let id = format!("c{i:04}");
                insert.values_panic([id.as_str().into(), id.as_str().into(), 0.into(), 0.into()]);
            }
            presence.inner.database.execute(&insert).await.unwrap();
        }
        let first = presence.snapshot().await.unwrap();
        let PresenceEvent::Snapshot {
            agents, companies, ..
        } = &*first
        else {
            unreachable!()
        };
        assert_eq!((agents.len(), companies.len()), (5000, 1000));

        delete(&presence, Expr::col(Agents::Id).lt("d2500")).await;
        presence
            .inner
            .database
            .execute(
                &Query::delete()
                    .from_table(Companies::Table)
                    .and_where(Expr::col(Companies::Id).lt("c0400"))
                    .to_owned(),
            )
            .await
            .unwrap();
        let after = presence.snapshot().await.unwrap();
        let PresenceEvent::Snapshot {
            revision,
            agents,
            companies,
            ..
        } = &*after
        else {
            unreachable!()
        };
        assert_eq!(*revision, first.revision() + 2500 + 400);
        assert_eq!(agents.len(), 2500);
        assert!(agents.iter().all(|agent| agent.id.as_str() >= "d2500"));
        assert_eq!(companies.len(), 600);
        assert!(
            companies
                .iter()
                .all(|company| company.id.as_str() >= "c0400")
        );
    }
}
