use std::collections::{BTreeMap, BTreeSet};

use futures_util::lock::Mutex;

use serde::{Deserialize, Serialize};
use worker::*;

const DASHBOARD_TAG: &str = "dashboard";
const COMPANY_HEADER: &str = "X-Mesh-Company-Id";
const COMPANY_KEY: &str = "company_id";
const PRESENCE_KEY: &str = "presence";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PresenceAgent {
    pub id: String,
    pub name: String,
    pub connected: bool,
    /// The release an offline Agent announced it is installing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updating_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PresenceSnapshot {
    #[serde(rename = "type")]
    pub event_type: String,
    pub revision: u64,
    pub agents: Vec<PresenceAgent>,
    pub generated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceMutation {
    Connection {
        agent_id: String,
        connected: bool,
        generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updating_to: Option<String>,
    },
    Upsert {
        agent_id: String,
        name: Option<String>,
        connected: Option<bool>,
    },
    Delete {
        agent_id: String,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct PresenceState {
    revision: u64,
    connected_agent_ids: BTreeSet<String>,
    #[serde(default)]
    generations: BTreeMap<String, u64>,
    #[serde(default)]
    pending_event: Option<PresenceEvent>,
    /// Offline Agents that went offline to install this release.
    #[serde(default)]
    updating: BTreeMap<String, String>,
}

impl PresenceState {
    fn updating_to(&self, agent_id: &str, connected: bool) -> Option<String> {
        (!connected)
            .then(|| self.updating.get(agent_id).cloned())
            .flatten()
    }
}

// Read legacy attachments during rolling deployments. Only new attachments can
// be renewed; renewal binds the connection to its originally authenticated user.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum Subscription {
    Legacy(u64),
    Current {
        connection_id: String,
        user_id: String,
        expires_at_unix_ms: u64,
    },
}

impl Subscription {
    fn deadline(&self) -> u64 {
        match self {
            Self::Legacy(deadline) => *deadline,
            Self::Current {
                expires_at_unix_ms, ..
            } => *expires_at_unix_ms,
        }
    }
}

#[derive(Deserialize)]
struct Renewal {
    connection_id: String,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct AgentRow {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PresenceEvent {
    AgentUpsert { revision: u64, agent: PresenceAgent },
    AgentDeleted { revision: u64, agent_id: String },
}

#[durable_object]
pub struct CompanyPresence {
    state: State,
    environment: Env,
    // Serialize snapshot + delta production across D1/RPC awaits. No calls back
    // into AgentCoordinator: presence is a materialized backend event stream.
    updates: Mutex<()>,
}

impl DurableObject for CompanyPresence {
    fn new(state: State, environment: Env) -> Self {
        Self {
            state,
            environment,
            updates: Mutex::new(()),
        }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        let _guard = self.updates.lock().await;
        let company_id = self.bind_company(&request).await?;
        match (request.method(), request.path().as_str()) {
            (Method::Post, "/revoke") => {
                for socket in self.state.get_websockets() {
                    let _ = socket.close(Some(4001), Some("company access revoked"));
                }
                Response::ok("revoked")
            }
            (Method::Post, "/renew") => {
                let renewal: Renewal = request.json().await?;
                self.renew(&company_id, renewal).await
            }
            (Method::Post, "/catalog") => {
                self.drain_catalog(&company_id).await?;
                Response::ok("catalog delivered")
            }
            (Method::Get, "/subscribe") => {
                self.drain_catalog(&company_id).await?;
                self.subscribe(&company_id, &request).await
            }
            (Method::Get, "/snapshot") => {
                self.drain_catalog(&company_id).await?;
                Response::from_json(&self.snapshot(&company_id).await?)
            }
            (Method::Post, "/presence") => {
                let mutation: PresenceMutation = request.json().await?;
                self.apply_mutation(&company_id, mutation).await?;
                Response::ok("updated")
            }
            _ => Response::error("not found", 404),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        let _guard = self.updates.lock().await;
        if self.state.get_websockets().is_empty() {
            return Response::ok("no subscribers");
        }
        let now = Date::now().as_millis();
        let company = self
            .state
            .storage()
            .get::<String>(COMPANY_KEY)
            .await?
            .unwrap_or_default();
        let db = self.environment.d1("DB")?;
        let active = query!(
            &db,
            "SELECT 1 AS allowed FROM companies WHERE id = ?1 AND status IN ('active', 'awaiting_admin')",
            company
        )?
        .first::<i64>(Some("allowed"))
        .await?
        .is_some();
        for socket in self.state.get_websockets() {
            if !active
                || socket
                    .deserialize_attachment::<Subscription>()?
                    .is_none_or(|subscription| subscription.deadline() <= now)
            {
                let _ = socket.close(
                    Some(4001),
                    Some("subscription requires fresh authorization"),
                );
            }
        }
        if !self.state.get_websockets().is_empty() {
            self.state.storage().set_alarm(30_000_i64).await?;
        }
        if active {
            self.flush_pending_event().await?;
            self.drain_catalog(&company).await?;
        }
        Response::ok("subscriptions checked")
    }

    async fn websocket_message(
        &self,
        socket: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let _guard = self.updates.lock().await;
        if socket
            .deserialize_attachment::<Subscription>()?
            .is_none_or(|subscription| subscription.deadline() <= Date::now().as_millis())
        {
            socket.close(
                Some(4001),
                Some("subscription requires fresh authorization"),
            )?;
            return Ok(());
        }
        match message {
            WebSocketIncomingMessage::String(value) if value == "refresh" => {
                let company_id = self
                    .state
                    .storage()
                    .get::<String>(COMPANY_KEY)
                    .await?
                    .ok_or_else(|| {
                        Error::RustError("company presence is not initialized".into())
                    })?;
                socket.send_with_str(serde_json::to_string(&self.snapshot(&company_id).await?)?)?;
            }
            WebSocketIncomingMessage::String(_) | WebSocketIncomingMessage::Binary(_) => {
                socket.close(Some(1003), Some("unsupported dashboard message"))?;
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
        let _ = socket.close(Some(1000), Some("dashboard subscription closed"));
        console_log!(
            "event=agent_event_subscription_closed code={} clean={}",
            code,
            was_clean
        );
        Ok(())
    }

    async fn websocket_error(&self, socket: WebSocket, error: Error) -> Result<()> {
        let _ = socket.close(Some(1011), Some("presence stream failed"));
        console_error!("event=agent_event_subscription_error error={}", error);
        Ok(())
    }
}

impl CompanyPresence {
    async fn bind_company(&self, request: &Request) -> Result<String> {
        let company_id = request.headers().get(COMPANY_HEADER)?.ok_or_else(|| {
            Error::RustError("company presence request is missing identity".into())
        })?;
        crate::validate_identifier(&company_id, "company ID")?;
        match self.state.storage().get::<String>(COMPANY_KEY).await? {
            Some(existing) if existing != company_id => Err(Error::RustError(
                "company presence identity mismatch".into(),
            )),
            Some(_) => Ok(company_id),
            None => {
                self.state.storage().put(COMPANY_KEY, &company_id).await?;
                Ok(company_id)
            }
        }
    }

    async fn subscribe(&self, company_id: &str, request: &Request) -> Result<Response> {
        if request
            .headers()
            .get("Upgrade")?
            .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
        {
            return Response::error("WebSocket upgrade required", 426);
        }
        let pair = WebSocketPair::new()?;
        let expires_at_unix_ms = Date::now().as_millis() + 5 * 60_000;
        let connection_id = crate::random_token();
        let renewable = request
            .headers()
            .get("X-Mesh-Presence-Protocol")?
            .as_deref()
            == Some("2");
        let subscription = if renewable {
            let user_id = request
                .headers()
                .get("X-Mesh-User-Id")?
                .ok_or_else(|| Error::RustError("subscription requires user identity".into()))?;
            Subscription::Current {
                connection_id: connection_id.clone(),
                user_id,
                expires_at_unix_ms,
            }
        } else {
            Subscription::Legacy(expires_at_unix_ms)
        };
        pair.server.serialize_attachment(&subscription)?;
        if self.state.storage().get_alarm().await?.is_none() {
            self.state.storage().set_alarm(30_000_i64).await?;
        }
        self.state
            .accept_websocket_with_tags(&pair.server, &[DASHBOARD_TAG]);
        if renewable {
            pair.server.send_with_str(
                serde_json::json!({
                    "type": "authorization", "connection_id": connection_id,
                    "expires_at_unix_ms": expires_at_unix_ms,
                })
                .to_string(),
            )?;
        }
        pair.server
            .send_with_str(serde_json::to_string(&self.snapshot(company_id).await?)?)?;
        console_log!("event=agent_event_subscription_connected");
        Response::from_websocket(pair.client)
    }

    async fn renew(&self, company_id: &str, renewal: Renewal) -> Result<Response> {
        let db = self.environment.d1("DB")?;
        let active = query!(
            &db,
            "SELECT 1 AS allowed FROM companies WHERE id = ?1 AND status IN ('active', 'awaiting_admin')",
            company_id
        )?
        .first::<i64>(Some("allowed"))
        .await?
        .is_some();
        if !active {
            return Response::error("company access revoked", 403);
        }
        let now = Date::now().as_millis();
        for socket in self.state.get_websockets_with_tag(DASHBOARD_TAG) {
            if let Some(Subscription::Current {
                connection_id,
                user_id,
                expires_at_unix_ms,
            }) = socket.deserialize_attachment::<Subscription>()?
                && connection_id == renewal.connection_id
                && user_id == renewal.user_id
                && expires_at_unix_ms > now
            {
                let expires_at_unix_ms = now + 5 * 60_000;
                socket.serialize_attachment(Subscription::Current {
                    connection_id,
                    user_id,
                    expires_at_unix_ms,
                })?;
                return Response::from_json(
                    &serde_json::json!({ "expires_at_unix_ms": expires_at_unix_ms }),
                );
            }
        }
        Response::error("subscription expired or unknown", 410)
    }

    async fn drain_catalog(&self, company_id: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct Change {
            agent_id: String,
            event_id: String,
            name: Option<String>,
        }
        let db = self.environment.d1("DB")?;
        // Bounded work, including on the free plan. Missed immediate notifications
        // are retried while subscribed; an initial snapshot handles a quiet company.
        let changes = query!(&db,
            "SELECT o.agent_id, o.event_id, a.name FROM presence_catalog_outbox o LEFT JOIN agents a ON a.id = o.agent_id AND a.company_id = o.company_id AND a.deletion_requested_at IS NULL WHERE o.company_id = ?1 ORDER BY o.agent_id LIMIT 16",
            company_id
        )?.all().await?.results::<Change>()?;
        for change in changes {
            let mutation = match change.name {
                Some(name) => PresenceMutation::Upsert {
                    agent_id: change.agent_id.clone(),
                    name: Some(name),
                    connected: None,
                },
                None => PresenceMutation::Delete {
                    agent_id: change.agent_id.clone(),
                },
            };
            self.apply_mutation(company_id, mutation).await?;
            // A concurrent catalog edit replaces event_id. Never acknowledge that
            // newer edit while finishing delivery of this older one.
            query!(&db,
                "DELETE FROM presence_catalog_outbox WHERE agent_id = ?1 AND company_id = ?2 AND event_id = ?3",
                change.agent_id, company_id, change.event_id
            )?.run().await?;
        }
        Ok(())
    }

    async fn snapshot(&self, company_id: &str) -> Result<PresenceSnapshot> {
        let presence = self.load_presence().await?;
        let db = self.environment.d1("DB")?;
        let result = query!(
            &db,
            "SELECT id, name FROM agents WHERE company_id = ?1 AND deletion_requested_at IS NULL ORDER BY name COLLATE NOCASE, id",
            company_id
        )?
        .all()
        .await?;
        let mut agents = result
            .results::<AgentRow>()?
            .into_iter()
            .map(|row| {
                let connected = presence.connected_agent_ids.contains(&row.id);
                PresenceAgent {
                    updating_to: presence.updating_to(&row.id, connected),
                    connected,
                    id: row.id,
                    name: row.name,
                }
            })
            .collect::<Vec<_>>();
        sort_agents(&mut agents);
        Ok(PresenceSnapshot {
            event_type: "snapshot".to_owned(),
            revision: presence.revision,
            agents,
            generated_at_unix_ms: Date::now().as_millis(),
        })
    }

    async fn apply_mutation(&self, company_id: &str, mutation: PresenceMutation) -> Result<()> {
        self.flush_pending_event().await?;
        let mut presence = self.load_presence().await?;
        // A connection event also replaces the update the Agent is installing, if any.
        let (mutation, generation, updating_to) = match mutation {
            PresenceMutation::Connection {
                agent_id,
                connected,
                generation,
                updating_to,
            } => {
                if !accept_generation(presence.generations.get(&agent_id).copied(), generation) {
                    return Ok(());
                }
                (
                    PresenceMutation::Upsert {
                        agent_id,
                        name: None,
                        connected: Some(connected),
                    },
                    Some(generation),
                    Some(updating_to),
                )
            }
            mutation => (mutation, None, None),
        };
        let event = match mutation {
            PresenceMutation::Upsert {
                agent_id,
                name,
                connected,
            } => {
                crate::validate_identifier(&agent_id, "device ID")?;
                // Catalog updates cannot overwrite a newer connection event.
                let connected =
                    if generation.is_none() && presence.generations.contains_key(&agent_id) {
                        None
                    } else {
                        connected
                    };
                let connection_changed = match connected {
                    Some(true) => presence.connected_agent_ids.insert(agent_id.clone()),
                    Some(false) => presence.connected_agent_ids.remove(&agent_id),
                    None => true,
                };
                let update_changed = match updating_to {
                    Some(Some(version)) => {
                        presence.updating.insert(agent_id.clone(), version.clone()) != Some(version)
                    }
                    Some(None) => presence.updating.remove(&agent_id).is_some(),
                    None => false,
                };
                let state_changed = connection_changed || update_changed;
                if let Some(generation) = generation {
                    presence.generations.insert(agent_id.clone(), generation);
                }
                if !state_changed && name.is_none() {
                    // Persist the delivery watermark even when online status is
                    // unchanged (e.g. replacement socket); do not broadcast a no-op.
                    if generation.is_some() {
                        self.state.storage().put(PRESENCE_KEY, &presence).await?;
                    }
                    return Ok(());
                }
                let Some(agent) = self
                    .load_agent(company_id, &agent_id, name, connected, &presence)
                    .await?
                else {
                    return Ok(());
                };
                presence.revision = presence.revision.saturating_add(1);
                PresenceEvent::AgentUpsert {
                    revision: presence.revision,
                    agent,
                }
            }
            PresenceMutation::Connection { .. } => unreachable!("connection normalized above"),
            PresenceMutation::Delete { agent_id } => {
                crate::validate_identifier(&agent_id, "device ID")?;
                presence.connected_agent_ids.remove(&agent_id);
                presence.updating.remove(&agent_id);
                presence.revision = presence.revision.saturating_add(1);
                PresenceEvent::AgentDeleted {
                    revision: presence.revision,
                    agent_id,
                }
            }
        };
        presence.pending_event = Some(event);
        self.state.storage().put(PRESENCE_KEY, &presence).await?;
        self.flush_pending_event().await
    }

    async fn flush_pending_event(&self) -> Result<()> {
        let mut presence = self.load_presence().await?;
        let Some(event) = &presence.pending_event else {
            return Ok(());
        };
        let payload = serde_json::to_string(event)?;
        for socket in self.state.get_websockets_with_tag(DASHBOARD_TAG) {
            if socket
                .deserialize_attachment::<Subscription>()?
                .is_none_or(|subscription| subscription.deadline() <= Date::now().as_millis())
            {
                let _ = socket.close(
                    Some(4001),
                    Some("subscription requires fresh authorization"),
                );
                continue;
            }
            if let Err(error) = socket.send_with_str(&payload) {
                console_error!("event=agent_event_broadcast_failed error={}", error);
                let _ = socket.close(
                    Some(1011),
                    Some("presence stream requires resynchronization"),
                );
            }
        }
        presence.pending_event = None;
        self.state.storage().put(PRESENCE_KEY, &presence).await?;
        Ok(())
    }

    async fn load_agent(
        &self,
        company_id: &str,
        agent_id: &str,
        supplied_name: Option<String>,
        connected: Option<bool>,
        presence: &PresenceState,
    ) -> Result<Option<PresenceAgent>> {
        if let Some(name) = supplied_name {
            crate::validate_name(&name, "Agent name")?;
        }
        // Always check the durable catalog, including delayed creation retries:
        // a supplied name must not resurrect a deleted Agent.
        let db = self.environment.d1("DB")?;
        let row = query!(
            &db,
            "SELECT id, name FROM agents WHERE id = ?1 AND company_id = ?2 AND deletion_requested_at IS NULL",
            agent_id, company_id
        )?.first::<AgentRow>(None).await?;
        let Some(row) = row else { return Ok(None) };
        let name = row.name;
        let connected =
            connected.unwrap_or_else(|| presence.connected_agent_ids.contains(agent_id));
        Ok(Some(PresenceAgent {
            id: agent_id.to_owned(),
            name,
            connected,
            updating_to: presence.updating_to(agent_id, connected),
        }))
    }

    async fn load_presence(&self) -> Result<PresenceState> {
        Ok(self
            .state
            .storage()
            .get(PRESENCE_KEY)
            .await?
            .unwrap_or_default())
    }
}

fn accept_generation(current: Option<u64>, incoming: u64) -> bool {
    incoming > current.unwrap_or_default()
}

fn sort_agents(agents: &mut [PresenceAgent]) {
    agents.sort_by(|left, right| {
        right
            .connected
            .cmp(&left.connected)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
}

pub async fn publish(
    environment: &Env,
    company_id: &str,
    mutation: &PresenceMutation,
) -> Result<()> {
    let mut request = crate::internal_json_request("https://presence.internal/presence", mutation)?;
    request.headers_mut()?.set(COMPANY_HEADER, company_id)?;
    let response = crate::object_stub(environment, "COMPANY_PRESENCE", company_id)?
        .fetch_with_request(request)
        .await?;
    crate::ensure_success(response, "publish Agent presence").await
}

pub async fn flush_catalog(environment: &Env, company_id: &str) -> Result<()> {
    let mut request = Request::new("https://presence.internal/catalog", Method::Post)?;
    request.headers_mut()?.set(COMPANY_HEADER, company_id)?;
    let response = crate::object_stub(environment, "COMPANY_PRESENCE", company_id)?
        .fetch_with_request(request)
        .await?;
    crate::ensure_success(response, "publish Agent catalog").await
}

pub async fn snapshot(environment: &Env, company_id: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set(COMPANY_HEADER, company_id)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = Request::new_with_init("https://presence.internal/snapshot", &init)?;
    crate::object_stub(environment, "COMPANY_PRESENCE", company_id)?
        .fetch_with_request(request)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_sorted_online_first_then_by_identity() {
        let mut agents = vec![
            PresenceAgent {
                id: "b".into(),
                name: "Zulu".into(),
                connected: false,
                updating_to: None,
            },
            PresenceAgent {
                id: "c".into(),
                name: "Alpha".into(),
                connected: true,
                updating_to: None,
            },
            PresenceAgent {
                id: "a".into(),
                name: "Alpha".into(),
                connected: true,
                updating_to: None,
            },
        ];

        sort_agents(&mut agents);

        assert_eq!(
            agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c", "b"]
        );
    }

    #[test]
    fn events_use_the_dashboard_wire_protocol() {
        let event = PresenceEvent::AgentDeleted {
            revision: 7,
            agent_id: "endpoint-1".into(),
        };

        assert_eq!(
            serde_json::to_value(event).expect("event should serialize"),
            serde_json::json!({
                "type": "agent_deleted",
                "revision": 7,
                "agent_id": "endpoint-1"
            })
        );
    }
}
