//! Wake-on-LAN: waking offline devices through online devices on their
//! network. See docs/wake-on-lan.md.
//!
//! A wake request is recorded for each device woken, by a user or as its
//! part of a job, with the hardware addresses it is woken by and its relays
//! ([`relays`]), or, when it has none, why. Each relay is asked over its
//! control connection to broadcast the magic packets on the subnets it
//! shares with the device, and reports what it did; a relay that hasn't
//! reported after [`REPORT_WINDOW_MS`] failed. The request is `sent` once
//! any relay sent the packets and `failed` once none could. A device whose
//! Agent connects within [`ONLINE_WINDOW_MS`] of being sent the packets
//! counts as woken (`online_at`), which is how jobs tell that it worked.
pub mod relays;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::Ipv4Addr,
    sync::Arc,
    time::Duration,
};

use meshrmm_protocol_types::{
    AdapterKind, AgentCommand, MAX_WAKE_MAC_ADDRESSES, MAX_WAKE_PACKETS, SystemInfo, WakePacket,
    WakePacketResult, clean_text, parse_mac,
};
use sea_query::{Expr, ExprTrait, LockType, Order, Query};
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast;

use self::relays::{Address, Candidate, Chosen};
use crate::{
    audit::{self, Actor, Target as AuditTarget},
    db::{
        self, Executor,
        tables::{Agents, DeviceIpAddresses, DeviceSystemInfo, Users, WakeRelays, WakeRequests},
    },
    http::AppState,
    time::{MINUTE_MS, SECOND_MS, now_ms},
    users::new_id,
};

/// The most relays a device is woken through.
pub const MAX_RELAYS: usize = 3;
/// How long relays get to report what they sent.
pub const REPORT_WINDOW_MS: i64 = 15 * SECOND_MS;
/// How soon after its packets a device must come online to count as woken.
pub const ONLINE_WINDOW_MS: i64 = 10 * MINUTE_MS;
/// The most devices one request wakes.
pub const MAX_DEVICES: usize = 200;
/// Relays' errors are cut to this many characters.
const MAX_ERROR_CHARS: usize = 300;
/// Changes a waiter may fall behind by before it looks again anyway.
const CHANGE_BACKLOG: usize = 256;
/// Rows per query.
const CHUNK: usize = 400;

/// Announces wake requests whose relays reported, for requests that wait
/// for them.
#[derive(Debug, Clone)]
pub struct Engine {
    changes: broadcast::Sender<Arc<str>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            changes: broadcast::channel(CHANGE_BACKLOG).0,
        }
    }
}

impl Engine {
    /// The IDs of wake requests as their relays report.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.changes.subscribe()
    }

    fn changed(&self, wake_id: &str) {
        // Nobody waiting is fine.
        let _ = self.changes.send(wake_id.into());
    }
}

/// Where a wake request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Relays haven't all reported.
    Sending,
    /// At least one relay sent the packets.
    Sent,
    /// No relay could send them.
    Failed,
    /// There was no relay, or nothing to wake the device by.
    NoRelay,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::NoRelay => "no_relay",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sending" => Some(Self::Sending),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            "no_relay" => Some(Self::NoRelay),
            _ => None,
        }
    }
}

/// What one relay did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayStatus {
    Pending,
    Sent,
    Failed,
}

impl RelayStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A wake request, as the API returns it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Wake {
    pub id: String,
    pub device_id: String,
    /// `None` once the device is gone.
    pub device_name: Option<String>,
    pub status: Status,
    /// Why there is no relay, or why every relay failed.
    pub detail: Option<String>,
    pub mac_addresses: Vec<String>,
    /// The user who asked, or whose access the policy ran with.
    pub requested_by: String,
    pub job_id: Option<String>,
    pub created_at: i64,
    /// When the device came online afterwards, if it did in time.
    pub online_at: Option<i64>,
    /// Until when the device coming online counts as woken.
    pub watch_until: i64,
    pub relays: Vec<Relay>,
}

/// One relay of a wake request, on one subnet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Relay {
    pub device_id: String,
    pub name: String,
    pub broadcast: String,
    pub prefix_length: i64,
    pub status: RelayStatus,
    /// The relay's address it sent from.
    pub source_address: Option<String>,
    pub error: Option<String>,
    pub reported_at: Option<i64>,
}

/// What waking a device takes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    device_name: String,
    mac_addresses: Vec<String>,
    relays: Vec<Chosen>,
    /// Why it can't be woken, when it can't.
    problem: Option<String>,
}

#[derive(sqlx::FromRow)]
struct TargetRow {
    id: String,
    name: String,
    company_id: Option<String>,
    public_ip: Option<String>,
    details: Option<String>,
}

/// The hardware addresses to wake a device by, wired adapters first:
/// every adapter's but virtual ones', which no packet wakes.
fn wake_addresses(info: &SystemInfo) -> Vec<String> {
    let rank = |kind: AdapterKind| match kind {
        AdapterKind::Ethernet => 0,
        AdapterKind::Other => 1,
        AdapterKind::Wireless => 2,
        AdapterKind::Virtual => 3,
    };
    let mut adapters = info
        .network
        .adapters
        .iter()
        .filter(|adapter| adapter.kind != AdapterKind::Virtual)
        .collect::<Vec<_>>();
    adapters.sort_by_key(|adapter| rank(adapter.kind));
    let mut seen = HashSet::new();
    adapters
        .into_iter()
        .filter_map(|adapter| {
            let mac = parse_mac(adapter.mac_address.as_deref()?)?;
            seen.insert(mac).then(|| {
                mac.iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<Vec<_>>()
                    .join(":")
            })
        })
        .take(MAX_WAKE_MAC_ADDRESSES)
        .collect()
}

/// The device's last known IPv4 addresses, with their adapters' kinds.
fn last_addresses(info: &SystemInfo) -> Vec<Address> {
    info.network
        .adapters
        .iter()
        .flat_map(|adapter| {
            adapter.addresses.iter().filter_map(|address| {
                Some(Address {
                    address: address.address.parse().ok()?,
                    prefix_length: address.prefix_length,
                    kind: adapter.kind,
                })
            })
        })
        .collect()
}

/// The relays that may wake devices: every online one with its last known
/// addresses.
async fn candidates(
    executor: &mut impl Executor,
    connected: &HashSet<String>,
) -> db::Result<Vec<Candidate>> {
    let ids = connected.iter().cloned().collect::<Vec<_>>();
    let mut candidates = BTreeMap::new();
    for chunk in ids.chunks(CHUNK) {
        let rows: Vec<(String, String, Option<String>, Option<String>)> = executor
            .fetch_all(
                &Query::select()
                    .columns([
                        (Agents::Table, Agents::Id),
                        (Agents::Table, Agents::Name),
                        (Agents::Table, Agents::CompanyId),
                    ])
                    .column((DeviceSystemInfo::Table, DeviceSystemInfo::PublicIp))
                    .from(Agents::Table)
                    .left_join(
                        DeviceSystemInfo::Table,
                        Expr::col((DeviceSystemInfo::Table, DeviceSystemInfo::DeviceId))
                            .equals((Agents::Table, Agents::Id)),
                    )
                    .and_where(Expr::col((Agents::Table, Agents::Id)).is_in(chunk.iter().cloned()))
                    .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
                    .to_owned(),
            )
            .await?;
        for (device_id, name, company_id, public_ip) in rows {
            candidates.insert(
                device_id.clone(),
                Candidate {
                    device_id,
                    name,
                    public_ip,
                    company_id,
                    addresses: Vec::new(),
                },
            );
        }
        let addresses: Vec<(String, String, i64, String)> = executor
            .fetch_all(
                &Query::select()
                    .columns([
                        DeviceIpAddresses::DeviceId,
                        DeviceIpAddresses::Address,
                        DeviceIpAddresses::PrefixLength,
                        DeviceIpAddresses::Kind,
                    ])
                    .from(DeviceIpAddresses::Table)
                    .and_where(Expr::col(DeviceIpAddresses::DeviceId).is_in(chunk.iter().cloned()))
                    .to_owned(),
            )
            .await?;
        for (device_id, address, prefix_length, kind) in addresses {
            let (Some(candidate), Ok(address), Ok(prefix_length)) = (
                candidates.get_mut(&device_id),
                address.parse::<Ipv4Addr>(),
                u8::try_from(prefix_length),
            ) else {
                continue;
            };
            candidate.addresses.push(Address {
                address,
                prefix_length,
                kind: AdapterKind::parse(&kind),
            });
        }
    }
    Ok(candidates.into_values().collect())
}

/// How to wake each of `targets` that is enrolled.
async fn plans(
    executor: &mut impl Executor,
    targets: &[String],
    connected: &HashSet<String>,
) -> db::Result<HashMap<String, Plan>> {
    let mut rows: Vec<TargetRow> = Vec::new();
    for chunk in targets.chunks(CHUNK) {
        rows.extend(
            executor
                .fetch_all::<TargetRow, _>(
                    &Query::select()
                        .columns([
                            (Agents::Table, Agents::Id),
                            (Agents::Table, Agents::Name),
                            (Agents::Table, Agents::CompanyId),
                        ])
                        .columns([
                            (DeviceSystemInfo::Table, DeviceSystemInfo::PublicIp),
                            (DeviceSystemInfo::Table, DeviceSystemInfo::Details),
                        ])
                        .from(Agents::Table)
                        .left_join(
                            DeviceSystemInfo::Table,
                            Expr::col((DeviceSystemInfo::Table, DeviceSystemInfo::DeviceId))
                                .equals((Agents::Table, Agents::Id)),
                        )
                        .and_where(
                            Expr::col((Agents::Table, Agents::Id)).is_in(chunk.iter().cloned()),
                        )
                        .and_where(
                            Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null(),
                        )
                        .to_owned(),
                )
                .await?,
        );
    }
    let candidates = candidates(executor, connected).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let info = row
                .details
                .as_deref()
                .and_then(|details| serde_json::from_str::<SystemInfo>(details).ok());
            let plan = plan(&row, info.as_ref(), &candidates);
            (row.id, plan)
        })
        .collect())
}

fn plan(row: &TargetRow, info: Option<&SystemInfo>, candidates: &[Candidate]) -> Plan {
    let name = &row.name;
    let mut plan = Plan {
        device_name: name.clone(),
        mac_addresses: Vec::new(),
        relays: Vec::new(),
        problem: None,
    };
    let Some(info) = info else {
        plan.problem = Some(format!(
            "MeshRMM doesn't know {name}'s network adapters: its Agent hasn't sent its hardware and network details yet."
        ));
        return plan;
    };
    plan.mac_addresses = wake_addresses(info);
    if plan.mac_addresses.is_empty() {
        plan.problem = Some(format!(
            "{name} last reported no wired or wireless network adapter with a hardware (MAC) address to wake it by."
        ));
        return plan;
    }
    let addresses = last_addresses(info)
        .into_iter()
        .filter(Address::usable)
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        plan.problem = Some(format!(
            "{name} last reported no IPv4 address on a local network, so MeshRMM can't tell which devices share its network."
        ));
        return plan;
    }
    plan.relays = relays::choose(
        &relays::Target {
            device_id: &row.id,
            public_ip: row.public_ip.as_deref(),
            company_id: row.company_id.as_deref(),
            addresses: &addresses,
        },
        candidates,
        MAX_RELAYS,
    );
    if plan.relays.is_empty() {
        let networks = addresses
            .iter()
            .map(|address| {
                let network = Ipv4Addr::from(
                    u32::from(address.address)
                        & meshrmm_protocol_types::subnet_mask(address.prefix_length),
                );
                format!("{network}/{}", address.prefix_length)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        plan.problem = Some(format!(
            "No online device is on {name}'s network ({networks}). Wake-on-LAN packets don't cross routers, so another device with a MeshRMM Agent must be online on that network to send them."
        ));
    }
    plan
}

/// Who asks for wake requests.
#[derive(Debug, Clone, Copy)]
pub struct Requester<'a> {
    /// The user, or the user whose access a policy runs with.
    pub user_id: &'a str,
    /// The job whose part they are.
    pub job_id: Option<&'a str>,
    /// Recorded in the audit log for each device, when a user asks.
    pub actor: Option<&'a Actor>,
}

/// Wakes each of `targets` that is enrolled: records a request with its
/// relays, or why it has none, and asks the relays to send the packets.
/// Returns the requests' IDs by device.
pub async fn start(
    state: &AppState,
    targets: &[String],
    requester: Requester<'_>,
) -> db::Result<HashMap<String, String>> {
    let connected = state.agents.connected();
    let plans = plans(&mut &state.database, targets, &connected).await?;
    let now = now_ms();
    let mut ids = HashMap::new();
    let mut packets: BTreeMap<String, (String, Vec<WakePacket>)> = BTreeMap::new();
    let mut transaction = state.database.begin().await?;
    for target in targets {
        let Some(plan) = plans.get(target) else {
            continue;
        };
        if ids.contains_key(target) {
            continue;
        }
        let id = new_id();
        let status = if plan.problem.is_some() {
            Status::NoRelay
        } else {
            Status::Sending
        };
        transaction
            .execute(
                &Query::insert()
                    .into_table(WakeRequests::Table)
                    .columns([
                        WakeRequests::Id,
                        WakeRequests::DeviceId,
                        WakeRequests::RequestedByUserId,
                        WakeRequests::JobId,
                        WakeRequests::Status,
                        WakeRequests::Detail,
                        WakeRequests::MacAddresses,
                        WakeRequests::CreatedAt,
                    ])
                    .values_panic([
                        id.as_str().into(),
                        target.as_str().into(),
                        requester.user_id.into(),
                        requester.job_id.into(),
                        status.as_str().into(),
                        plan.problem.clone().into(),
                        plan.mac_addresses.join(",").into(),
                        now.into(),
                    ])
                    .to_owned(),
            )
            .await?;
        for relay in &plan.relays {
            for (broadcast, prefix_length) in &relay.subnets {
                transaction
                    .execute(
                        &Query::insert()
                            .into_table(WakeRelays::Table)
                            .columns([
                                WakeRelays::WakeId,
                                WakeRelays::RelayDeviceId,
                                WakeRelays::RelayName,
                                WakeRelays::Broadcast,
                                WakeRelays::PrefixLength,
                                WakeRelays::Status,
                            ])
                            .values_panic([
                                id.as_str().into(),
                                relay.device_id.as_str().into(),
                                relay.name.as_str().into(),
                                broadcast.to_string().into(),
                                i64::from(*prefix_length).into(),
                                RelayStatus::Pending.as_str().into(),
                            ])
                            .to_owned(),
                    )
                    .await?;
                packets
                    .entry(relay.device_id.clone())
                    .or_insert_with(|| (relay.name.clone(), Vec::new()))
                    .1
                    .push(WakePacket {
                        wake_id: id.clone(),
                        mac_addresses: plan.mac_addresses.clone(),
                        broadcast: broadcast.to_string(),
                        prefix_length: *prefix_length,
                    });
            }
        }
        if let Some(actor) = requester.actor {
            audit::record(
                &mut transaction,
                actor,
                "agent.wake",
                AuditTarget::device(target),
                json!({
                    "wake_id": id,
                    "name": plan.device_name,
                    "status": status.as_str(),
                    "detail": plan.problem,
                    "mac_addresses": plan.mac_addresses,
                    "relays": plan.relays.iter().map(|relay| relay.name.as_str()).collect::<Vec<_>>(),
                }),
            )
            .await?;
        }
        ids.insert(target.clone(), id);
    }
    transaction.commit().await?;
    let mut waiting = Vec::new();
    for (relay_id, (name, packets)) in packets {
        for chunk in packets.chunks(MAX_WAKE_PACKETS) {
            waiting.extend(chunk.iter().map(|packet| packet.wake_id.clone()));
            let command = AgentCommand::SendWakePackets {
                packets: chunk.to_vec(),
            };
            if !state.agents.send(&relay_id, command) {
                let error = format!("{name} went offline before it got the request");
                let results = chunk
                    .iter()
                    .map(|packet| WakePacketResult {
                        wake_id: packet.wake_id.clone(),
                        broadcast: packet.broadcast.clone(),
                        source_address: None,
                        error: Some(error.clone()),
                    })
                    .collect();
                reported(state, &relay_id, results).await;
            }
        }
    }
    if !waiting.is_empty() {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(REPORT_WINDOW_MS as u64)).await;
            if let Err(error) = give_up(&state, &waiting).await {
                tracing::warn!(%error, "could not finish wake requests whose relays didn't report");
            }
        });
    }
    Ok(ids)
}

/// Records what a relay reported for its packets.
pub async fn reported(state: &AppState, relay_id: &str, results: Vec<WakePacketResult>) {
    let mut by_wake: BTreeMap<String, Vec<WakePacketResult>> = BTreeMap::new();
    for result in results.into_iter().take(MAX_WAKE_PACKETS) {
        by_wake
            .entry(result.wake_id.clone())
            .or_default()
            .push(result);
    }
    for (wake_id, results) in by_wake {
        match record_results(state, relay_id, &wake_id, &results).await {
            Ok(true) => state.wake.changed(&wake_id),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(wake_id, %error, "could not record a relay's wake packets")
            }
        }
    }
}

/// Records one relay's results for one request. Returns whether they were
/// news.
async fn record_results(
    state: &AppState,
    relay_id: &str,
    wake_id: &str,
    results: &[WakePacketResult],
) -> db::Result<bool> {
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    // Relays reporting at once take turns, so each sees the others'
    // results when it works out the request's status.
    let exists: Option<(String,)> = transaction
        .fetch_optional(
            &Query::select()
                .column(WakeRequests::Status)
                .from(WakeRequests::Table)
                .and_where(Expr::col(WakeRequests::Id).eq(wake_id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    if exists.is_none() {
        return Ok(false);
    }
    let mut changed = 0;
    for result in results {
        let status = if result.error.is_none() {
            RelayStatus::Sent
        } else {
            RelayStatus::Failed
        };
        let error = result.error.as_deref().map(|error| {
            clean_text(error)
                .chars()
                .take(MAX_ERROR_CHARS)
                .collect::<String>()
        });
        let source = result
            .source_address
            .as_deref()
            .and_then(|address| address.parse::<Ipv4Addr>().ok())
            .map(|address| address.to_string());
        changed += transaction
            .execute(
                &Query::update()
                    .table(WakeRelays::Table)
                    .value(WakeRelays::Status, status.as_str())
                    .value(WakeRelays::SourceAddress, source)
                    .value(WakeRelays::Error, error)
                    .value(WakeRelays::ReportedAt, now)
                    .and_where(Expr::col(WakeRelays::WakeId).eq(wake_id))
                    .and_where(Expr::col(WakeRelays::RelayDeviceId).eq(relay_id))
                    .and_where(Expr::col(WakeRelays::Broadcast).eq(result.broadcast.as_str()))
                    .and_where(Expr::col(WakeRelays::Status).eq(RelayStatus::Pending.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    if changed == 0 {
        return Ok(false);
    }
    settle_status(&mut transaction, wake_id).await?;
    transaction.commit().await?;
    Ok(true)
}

/// Sets a request's status from its relays'.
async fn settle_status(executor: &mut impl Executor, wake_id: &str) -> db::Result<()> {
    let relays: Vec<(String, String, Option<String>)> = executor
        .fetch_all(
            &Query::select()
                .columns([WakeRelays::RelayName, WakeRelays::Status, WakeRelays::Error])
                .from(WakeRelays::Table)
                .and_where(Expr::col(WakeRelays::WakeId).eq(wake_id))
                .order_by(WakeRelays::RelayName, Order::Asc)
                .to_owned(),
        )
        .await?;
    let (status, detail) = if relays.iter().any(|(_, status, _)| status == "sent") {
        (Status::Sent, None)
    } else if relays.iter().any(|(_, status, _)| status == "pending") {
        (Status::Sending, None)
    } else {
        let reasons = relays
            .iter()
            .map(|(name, _, error)| format!("{name}: {}", error.as_deref().unwrap_or("failed")))
            .collect::<Vec<_>>()
            .join("; ");
        (
            Status::Failed,
            Some(format!("No relay could send the packets. {reasons}")),
        )
    };
    executor
        .execute(
            &Query::update()
                .table(WakeRequests::Table)
                .value(WakeRequests::Status, status.as_str())
                .value(WakeRequests::Detail, detail)
                .and_where(Expr::col(WakeRequests::Id).eq(wake_id))
                .and_where(Expr::col(WakeRequests::Status).ne(Status::NoRelay.as_str()))
                .to_owned(),
        )
        .await?;
    Ok(())
}

const NO_REPORT: &str = "The relay didn't report back";

/// Fails the relays of `wake_ids` that haven't reported.
async fn give_up(state: &AppState, wake_ids: &[String]) -> db::Result<()> {
    let mut unique = wake_ids.to_vec();
    unique.sort();
    unique.dedup();
    let now = now_ms();
    for wake_id in unique {
        let mut transaction = state.database.begin().await?;
        let failed = transaction
            .execute(
                &Query::update()
                    .table(WakeRelays::Table)
                    .value(WakeRelays::Status, RelayStatus::Failed.as_str())
                    .value(WakeRelays::Error, NO_REPORT)
                    .value(WakeRelays::ReportedAt, now)
                    .and_where(Expr::col(WakeRelays::WakeId).eq(wake_id.as_str()))
                    .and_where(Expr::col(WakeRelays::Status).eq(RelayStatus::Pending.as_str()))
                    .to_owned(),
            )
            .await?;
        if failed > 0 {
            settle_status(&mut transaction, &wake_id).await?;
        }
        transaction.commit().await?;
        if failed > 0 {
            state.wake.changed(&wake_id);
        }
    }
    Ok(())
}

/// Fails the relays that never reported because the server stopped while
/// they were due. Run at startup.
pub async fn give_up_overdue(state: &AppState, now: i64) -> db::Result<()> {
    let overdue: Vec<(String,)> = state
        .database
        .fetch_all(
            &Query::select()
                .distinct()
                .column(WakeRelays::WakeId)
                .from(WakeRelays::Table)
                .and_where(Expr::col(WakeRelays::Status).eq(RelayStatus::Pending.as_str()))
                .and_where(
                    Expr::col(WakeRelays::WakeId).in_subquery(
                        Query::select()
                            .column(WakeRequests::Id)
                            .from(WakeRequests::Table)
                            .and_where(
                                Expr::col(WakeRequests::CreatedAt).lte(now - REPORT_WINDOW_MS),
                            )
                            .to_owned(),
                    ),
                )
                .to_owned(),
        )
        .await?;
    let ids = overdue.into_iter().map(|(id,)| id).collect::<Vec<_>>();
    give_up(state, &ids).await
}

/// A device's Agent connected: it woke, if it was sent packets lately.
pub async fn device_connected(state: &AppState, device_id: &str) {
    let now = now_ms();
    let woken = state
        .database
        .execute(
            &Query::update()
                .table(WakeRequests::Table)
                .value(WakeRequests::OnlineAt, now)
                .and_where(Expr::col(WakeRequests::DeviceId).eq(device_id))
                .and_where(Expr::col(WakeRequests::OnlineAt).is_null())
                .and_where(
                    Expr::col(WakeRequests::Status)
                        .is_in([Status::Sending.as_str(), Status::Sent.as_str()]),
                )
                .and_where(Expr::col(WakeRequests::CreatedAt).gte(now - ONLINE_WINDOW_MS))
                .to_owned(),
        )
        .await;
    match woken {
        Ok(0) => {}
        Ok(_) => {
            tracing::info!(device_id, "a device came online after it was woken");
            // Its jobs' outcomes changed.
            state.automation.wake();
        }
        Err(error) => {
            tracing::warn!(device_id, %error, "could not record that a woken device came online")
        }
    }
}

/// Waits until the relays of `ids` have all reported, or `timeout` passes.
/// Subscribe before the requests start, so no report is missed.
pub async fn wait_for_relays(
    state: &AppState,
    changes: &mut broadcast::Receiver<Arc<str>>,
    ids: &[String],
    timeout: Duration,
) -> db::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut sending = 0;
        for chunk in ids.chunks(CHUNK) {
            let rows: Vec<(String,)> = state
                .database
                .fetch_all(
                    &Query::select()
                        .column(WakeRequests::Id)
                        .from(WakeRequests::Table)
                        .and_where(Expr::col(WakeRequests::Id).is_in(chunk.iter().cloned()))
                        .and_where(Expr::col(WakeRequests::Status).eq(Status::Sending.as_str()))
                        .to_owned(),
                )
                .await?;
            sending += rows.len();
        }
        if sending == 0 {
            return Ok(());
        }
        tokio::select! {
            change = changes.recv() => {
                if matches!(change, Err(broadcast::error::RecvError::Closed)) {
                    return Ok(());
                }
            }
            () = tokio::time::sleep_until(deadline) => return Ok(()),
        }
    }
}

#[derive(sqlx::FromRow)]
struct WakeRow {
    id: String,
    device_id: String,
    device_name: Option<String>,
    status: String,
    detail: Option<String>,
    mac_addresses: String,
    requested_by: Option<String>,
    job_id: Option<String>,
    created_at: i64,
    online_at: Option<i64>,
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            (WakeRequests::Table, WakeRequests::Id),
            (WakeRequests::Table, WakeRequests::DeviceId),
        ])
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .columns([
            (WakeRequests::Table, WakeRequests::Status),
            (WakeRequests::Table, WakeRequests::Detail),
            (WakeRequests::Table, WakeRequests::MacAddresses),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "requested_by",
        )
        .columns([
            (WakeRequests::Table, WakeRequests::JobId),
            (WakeRequests::Table, WakeRequests::CreatedAt),
            (WakeRequests::Table, WakeRequests::OnlineAt),
        ])
        .from(WakeRequests::Table)
        .left_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((WakeRequests::Table, WakeRequests::DeviceId)),
        )
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((WakeRequests::Table, WakeRequests::RequestedByUserId)),
        )
        .to_owned()
}

#[derive(sqlx::FromRow)]
struct RelayRow {
    wake_id: String,
    relay_device_id: String,
    relay_name: String,
    broadcast: String,
    prefix_length: i64,
    status: String,
    source_address: Option<String>,
    error: Option<String>,
    reported_at: Option<i64>,
}

async fn with_relays(executor: &mut impl Executor, rows: Vec<WakeRow>) -> db::Result<Vec<Wake>> {
    let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
    let mut relays: HashMap<String, Vec<Relay>> = HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let rows: Vec<RelayRow> = executor
            .fetch_all(
                &Query::select()
                    .columns([
                        WakeRelays::WakeId,
                        WakeRelays::RelayDeviceId,
                        WakeRelays::RelayName,
                        WakeRelays::Broadcast,
                        WakeRelays::PrefixLength,
                        WakeRelays::Status,
                        WakeRelays::SourceAddress,
                        WakeRelays::Error,
                        WakeRelays::ReportedAt,
                    ])
                    .from(WakeRelays::Table)
                    .and_where(Expr::col(WakeRelays::WakeId).is_in(chunk.iter().cloned()))
                    .order_by(WakeRelays::RelayName, Order::Asc)
                    .order_by(WakeRelays::Broadcast, Order::Asc)
                    .to_owned(),
            )
            .await?;
        for row in rows {
            let Some(status) = RelayStatus::parse(&row.status) else {
                continue;
            };
            relays.entry(row.wake_id).or_default().push(Relay {
                device_id: row.relay_device_id,
                name: row.relay_name,
                broadcast: row.broadcast,
                prefix_length: row.prefix_length,
                status,
                source_address: row.source_address,
                error: row.error,
                reported_at: row.reported_at,
            });
        }
    }
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let status = Status::parse(&row.status)?;
            Some(Wake {
                relays: relays.remove(&row.id).unwrap_or_default(),
                status,
                detail: row.detail,
                mac_addresses: row
                    .mac_addresses
                    .split(',')
                    .filter(|mac| !mac.is_empty())
                    .map(str::to_owned)
                    .collect(),
                requested_by: row
                    .requested_by
                    .unwrap_or_else(|| "A removed user".to_owned()),
                job_id: row.job_id,
                watch_until: row.created_at + ONLINE_WINDOW_MS,
                created_at: row.created_at,
                online_at: row.online_at,
                device_name: row.device_name,
                device_id: row.device_id,
                id: row.id,
            })
        })
        .collect())
}

/// Wake requests by ID, in the order asked for.
pub async fn by_ids(executor: &mut impl Executor, ids: &[String]) -> db::Result<Vec<Wake>> {
    let mut rows: Vec<WakeRow> = Vec::new();
    for chunk in ids.chunks(CHUNK) {
        rows.extend(
            executor
                .fetch_all::<WakeRow, _>(
                    &select()
                        .and_where(
                            Expr::col((WakeRequests::Table, WakeRequests::Id))
                                .is_in(chunk.iter().cloned()),
                        )
                        .to_owned(),
                )
                .await?,
        );
    }
    let order = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect::<HashMap<_, _>>();
    rows.sort_by_key(|row| order.get(row.id.as_str()).copied().unwrap_or(usize::MAX));
    with_relays(executor, rows).await
}

/// The device's latest wake requests, newest first.
pub async fn recent(
    executor: &mut impl Executor,
    device_id: &str,
    limit: u64,
) -> db::Result<Vec<Wake>> {
    let rows: Vec<WakeRow> = executor
        .fetch_all(
            &select()
                .and_where(Expr::col((WakeRequests::Table, WakeRequests::DeviceId)).eq(device_id))
                .order_by((WakeRequests::Table, WakeRequests::CreatedAt), Order::Desc)
                .order_by((WakeRequests::Table, WakeRequests::Id), Order::Desc)
                .limit(limit)
                .to_owned(),
        )
        .await?;
    with_relays(executor, rows).await
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::{AdapterAddress, NetworkAdapter};

    use super::*;

    fn adapter(
        name: &str,
        kind: AdapterKind,
        mac: Option<&str>,
        addresses: &[(&str, u8)],
    ) -> NetworkAdapter {
        NetworkAdapter {
            name: name.into(),
            mac_address: mac.map(str::to_owned),
            kind,
            addresses: addresses
                .iter()
                .map(|(address, prefix_length)| AdapterAddress {
                    address: (*address).into(),
                    prefix_length: *prefix_length,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn info(adapters: Vec<NetworkAdapter>) -> SystemInfo {
        let mut info = SystemInfo::default();
        info.network.adapters = adapters;
        info
    }

    fn row() -> TargetRow {
        TargetRow {
            id: "pc".into(),
            name: "Front desk".into(),
            company_id: None,
            public_ip: Some("203.0.113.5".into()),
            details: None,
        }
    }

    #[test]
    fn devices_are_woken_by_their_hardware_adapters_wired_first() {
        let info = info(vec![
            adapter(
                "Wi-Fi",
                AdapterKind::Wireless,
                Some("AA:00:00:00:00:02"),
                &[],
            ),
            adapter(
                "vEthernet",
                AdapterKind::Virtual,
                Some("AA:00:00:00:00:03"),
                &[],
            ),
            adapter(
                "Ethernet",
                AdapterKind::Ethernet,
                Some("AA:00:00:00:00:01"),
                &[],
            ),
            adapter(
                "Ethernet 2",
                AdapterKind::Ethernet,
                Some("AA:00:00:00:00:01"),
                &[],
            ),
            adapter("Bluetooth", AdapterKind::Other, None, &[]),
        ]);
        assert_eq!(
            wake_addresses(&info),
            ["AA:00:00:00:00:01", "AA:00:00:00:00:02"]
        );
    }

    #[test]
    fn plans_say_why_a_device_cant_be_woken() {
        let problem = |info: Option<&SystemInfo>, candidates: &[Candidate]| {
            plan(&row(), info, candidates).problem.unwrap_or_default()
        };
        assert!(problem(None, &[]).contains("hasn't sent its hardware and network details"));
        let no_mac = info(vec![adapter(
            "Ethernet",
            AdapterKind::Ethernet,
            None,
            &[("192.168.1.9", 24)],
        )]);
        assert!(problem(Some(&no_mac), &[]).contains("no wired or wireless network adapter"));
        let no_ipv4 = info(vec![adapter(
            "Ethernet",
            AdapterKind::Ethernet,
            Some("AA:00:00:00:00:01"),
            &[("fe80::1", 64), ("169.254.2.2", 16)],
        )]);
        assert!(problem(Some(&no_ipv4), &[]).contains("no IPv4 address"));
        let lan = info(vec![adapter(
            "Ethernet",
            AdapterKind::Ethernet,
            Some("AA:00:00:00:00:01"),
            &[("192.168.1.9", 24), ("10.4.0.9", 16)],
        )]);
        assert_eq!(
            problem(Some(&lan), &[]),
            "No online device is on Front desk's network (10.4.0.0/16, 192.168.1.0/24). Wake-on-LAN packets don't cross routers, so another device with a MeshRMM Agent must be online on that network to send them."
        );
        let relay = Candidate {
            device_id: "mac".into(),
            name: "Mac".into(),
            public_ip: Some("203.0.113.5".into()),
            company_id: None,
            addresses: vec![Address {
                address: "192.168.1.140".parse().unwrap(),
                prefix_length: 24,
                kind: AdapterKind::Wireless,
            }],
        };
        let plan = plan(&row(), Some(&lan), &[relay]);
        assert_eq!(plan.problem, None);
        assert_eq!(plan.mac_addresses, ["AA:00:00:00:00:01"]);
        assert_eq!(plan.relays[0].name, "Mac");
        assert_eq!(
            plan.relays[0].subnets,
            [("192.168.1.255".parse().unwrap(), 24)]
        );
    }
}
