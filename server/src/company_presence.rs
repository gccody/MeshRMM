use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use worker::*;

const DASHBOARD_TAG: &str = "dashboard";
const COMPANY_HEADER: &str = "X-Pulse-Company-Id";
const COMPANY_KEY: &str = "company_id";
const PRESENCE_KEY: &str = "presence";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PresenceAgent {
    pub id: String,
    pub name: String,
    pub connected: bool,
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
}

#[derive(Debug, Deserialize)]
struct AgentRow {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PresenceEvent {
    AgentUpsert { revision: u64, agent: PresenceAgent },
    AgentDeleted { revision: u64, agent_id: String },
}

#[durable_object]
pub struct CompanyPresence {
    state: State,
    environment: Env,
}

impl DurableObject for CompanyPresence {
    fn new(state: State, environment: Env) -> Self {
        Self { state, environment }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        let company_id = self.bind_company(&request).await?;
        match (request.method(), request.path().as_str()) {
            (Method::Get, "/subscribe") => self.subscribe(&company_id, &request).await,
            (Method::Get, "/snapshot") => Response::from_json(&self.snapshot(&company_id).await?),
            (Method::Post, "/presence") => {
                let mutation: PresenceMutation = request.json().await?;
                self.apply_mutation(&company_id, mutation).await?;
                Response::ok("updated")
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
        _socket: WebSocket,
        code: usize,
        _reason: String,
        was_clean: bool,
    ) -> Result<()> {
        console_log!(
            "event=agent_event_subscription_closed code={} clean={}",
            code,
            was_clean
        );
        Ok(())
    }

    async fn websocket_error(&self, _socket: WebSocket, error: Error) -> Result<()> {
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
        self.state
            .accept_websocket_with_tags(&pair.server, &[DASHBOARD_TAG]);
        pair.server
            .send_with_str(serde_json::to_string(&self.snapshot(company_id).await?)?)?;
        console_log!("event=agent_event_subscription_connected");
        Response::from_websocket(pair.client)
    }

    async fn snapshot(&self, company_id: &str) -> Result<PresenceSnapshot> {
        let presence = self.load_presence().await?;
        let db = self.environment.d1("DB")?;
        let result = query!(
            &db,
            "SELECT id, name FROM agents WHERE company_id = ?1 ORDER BY name COLLATE NOCASE, id",
            company_id
        )?
        .all()
        .await?;
        let mut agents = result
            .results::<AgentRow>()?
            .into_iter()
            .map(|row| PresenceAgent {
                connected: presence.connected_agent_ids.contains(&row.id),
                id: row.id,
                name: row.name,
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
        let mut presence = self.load_presence().await?;
        let event = match mutation {
            PresenceMutation::Upsert {
                agent_id,
                name,
                connected,
            } => {
                crate::validate_identifier(&agent_id, "device ID")?;
                let state_changed = match connected {
                    Some(true) => presence.connected_agent_ids.insert(agent_id.clone()),
                    Some(false) => presence.connected_agent_ids.remove(&agent_id),
                    None => true,
                };
                if !state_changed && name.is_none() {
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
            PresenceMutation::Delete { agent_id } => {
                crate::validate_identifier(&agent_id, "device ID")?;
                presence.connected_agent_ids.remove(&agent_id);
                presence.revision = presence.revision.saturating_add(1);
                PresenceEvent::AgentDeleted {
                    revision: presence.revision,
                    agent_id,
                }
            }
        };
        self.state.storage().put(PRESENCE_KEY, &presence).await?;
        let payload = serde_json::to_string(&event)?;
        for socket in self.state.get_websockets_with_tag(DASHBOARD_TAG) {
            if let Err(error) = socket.send_with_str(&payload) {
                console_error!("event=agent_event_broadcast_failed error={}", error);
            }
        }
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
        let name = match supplied_name {
            Some(name) => crate::validate_name(&name, "Agent name")?.to_owned(),
            None => {
                let db = self.environment.d1("DB")?;
                let row = query!(
                    &db,
                    "SELECT id, name FROM agents WHERE id = ?1 AND company_id = ?2",
                    agent_id,
                    company_id
                )?
                .first::<AgentRow>(None)
                .await?;
                let Some(row) = row else { return Ok(None) };
                row.name
            }
        };
        Ok(Some(PresenceAgent {
            id: agent_id.to_owned(),
            name,
            connected: connected.unwrap_or_else(|| presence.connected_agent_ids.contains(agent_id)),
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

pub fn set_company_header(request: &mut Request, company_id: &str) -> Result<()> {
    request.headers_mut()?.set(COMPANY_HEADER, company_id)
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
            },
            PresenceAgent {
                id: "c".into(),
                name: "Alpha".into(),
                connected: true,
            },
            PresenceAgent {
                id: "a".into(),
                name: "Alpha".into(),
                connected: true,
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
