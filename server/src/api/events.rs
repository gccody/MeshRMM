//! `GET /v1/events`: the website's socket for device presence, resource
//! usage, screen thumbnails and alerts.
//!
//! It sends a snapshot of the devices, then each change with the next
//! revision. Live resource usage, which carries no revision, it sends only
//! for the devices the tab says it shows. The tab sends JSON messages
//! ([`Request`]):
//!
//! - `{"type":"watch","devices":[...]}` replaces the devices the socket
//!   watches; an empty list watches none, and a socket opens watching none.
//!   It takes the first [`MAX_WATCHED`] distinct IDs and ignores the rest,
//!   and ignores IDs of no device the user may see: no device, a deleted
//!   one, or one in a company the user's `devices.view` doesn't reach. The socket sends the
//!   latest reading of each device it starts watching at once, then every
//!   few seconds the watched devices' readings taken since, and after each
//!   snapshot every watched device's latest. A deleted device is dropped
//!   from the set. When the Agent of a watched device uploads a new screen
//!   thumbnail, the socket sends `{"type":"thumbnails","device_ids":[...]}`
//!   so the tab loads it; one that fell behind names every device it
//!   watches.
//! - `{"type":"refresh"}` asks for a new snapshot, when the tab missed a
//!   change.
//! - `{"type":"ping"}` is answered with `{"type":"pong"}`, so a tab can tell
//!   a quiet socket from a dead one.
//!
//! A user whose permissions reach only some companies (see [`crate::access`])
//! gets only those companies' devices, companies, alerts, monitor results,
//! update events and jobs. A presence change about anything else is sent as
//! `{"type":"skipped","revision":N}`, so the tab's revisions stay in order, and
//! a device that moves out of reach is sent as deleted. Alert counts count
//! only alerts in reach. When the companies a user reaches change, the
//! socket sends a new snapshot.
//!
//! Any other message closes the socket with 1003, and one over
//! [`MAX_MESSAGE_BYTES`] closes it too. A user who may see alerts
//! also gets the unresolved alert counts after each snapshot, and each alert
//! that is raised or changes ([`AlertEvent`]). A user who may see jobs gets
//! `job_update` with each job they may see whose counts changed, and
//! `jobs_resync` if they missed some. A user who may see updates gets
//! `device_updates` with each device whose update status changes and
//! `update_run` with each scan or install that starts, progresses or ends,
//! and `updates_resync` if they missed some. The socket stays authorized by
//! the session cookie it opened with: it checks the session and the
//! `devices.view` permission again every half minute, and at once after a
//! change that can take access away (to users, roles, sessions or sign-in
//! settings), and closes with 4001 when either is gone.
//!
//! What every socket sends alike is serialised once for all of them: each
//! presence change and snapshot, alert event and update event is
//! [`Shared`](crate::realtime::Shared), and resource usage comes from one
//! feed that scans once a tick and serialises each reading once however
//! many sockets watch its device
//! ([`Metrics::subscribe`](crate::realtime::Metrics::subscribe)). Job
//! events, shaped for each viewer, the alert counts, and each socket's
//! message of the readings it watches are made per socket.
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, header},
    response::Response,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{
    access::CompanySet,
    alerts::{AlertEvent, store},
    auth::{Authorized, SignedIn, accounts::ACCESS_RECHECK, session},
    automation::jobs::Viewer,
    http::{ApiError, AppState, client_ip::ClientIp},
    rbac::Permission,
    realtime::{
        close_frame,
        metrics::{MAX_WATCHED, Subscription},
        presence::PresenceEvent,
        send,
        shared::Shared,
    },
    updates::UpdatesEvent,
};

/// Browsers don't ping; this keeps proxies from closing a quiet socket and
/// finds dead ones.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A browser that hasn't answered two pings is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
/// The largest message the socket takes: a watch of [`MAX_WATCHED`] device
/// IDs, 36 characters each and 3 more to quote and separate them, is about
/// 20 KiB.
const MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// The read buffer each open tab's socket holds (tungstenite's default is
/// 128 KiB). The website's messages are small but for a watch, which a
/// larger message buffer takes as it arrives.
const READ_BUFFER_BYTES: usize = 1024;

/// What a tab sends.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    /// Watch exactly these devices.
    Watch { devices: Vec<String> },
    /// Send a new snapshot.
    Refresh,
    /// Answer, to show the socket is alive.
    Ping,
}

/// The first [`MAX_WATCHED`] distinct IDs, in order.
fn bounded(device_ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    device_ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .take(MAX_WATCHED)
        .collect()
}

pub async fn subscribe(
    State(state): State<AppState>,
    signed_in: SignedIn,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // A website session only: the session cookie keeps the socket's
    // access checked while it stays open.
    let actor = Authorized::new(signed_in)?;
    // Browsers send any site's cookies with a WebSocket handshake, so only
    // the website's own pages may open one.
    let origin = state.config.public_origin();
    if headers
        .get(header::ORIGIN)
        .is_none_or(|value| value.as_bytes() != origin.as_bytes())
    {
        return Err(
            ApiError::forbidden("this socket must be opened by the MeshRMM website")
                .with_code("cross_site_request"),
        );
    }
    actor.require(Permission::DevicesView)?;
    let allowed = Allowed::of(&actor);
    let token = session::token(&headers).expect("an authorized request has a session cookie");
    Ok(upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .read_buffer_size(READ_BUFFER_BYTES)
        .on_upgrade(move |socket| serve(state, token, ip, allowed, socket)))
}

async fn serve(
    state: AppState,
    token: String,
    ip: IpAddr,
    mut allowed: Allowed,
    mut socket: WebSocket,
) {
    let mut events = state.presence.frames();
    let mut alert_events = state.alerts.subscribe();
    let mut job_events = state.automation.subscribe();
    let mut update_events = state.updates.subscribe();
    let mut access = state.presence.access_changes();
    let mut usage = state.metrics.subscribe();
    let mut filter = allowed.filter();
    // Which jobs of a viewer limited to some companies have a device in
    // them, as last read; emptied when their access changes.
    let mut job_reach: HashMap<String, bool> = HashMap::new();
    let Some(mut revision) =
        send_snapshot(&state, &mut socket, &allowed, filter.as_mut(), &mut usage).await
    else {
        return;
    };
    let mut recheck = tokio::time::interval(ACCESS_RECHECK);
    let mut ping = tokio::time::interval(PING_INTERVAL);
    // Both fire at once; the socket was just authorized.
    recheck.tick().await;
    ping.tick().await;
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) if event.revision() > revision => {
                    revision = event.revision();
                    if let PresenceEvent::AgentDeleted { agent_id, .. } = &*event {
                        usage.unwatch(agent_id);
                    }
                    let message = match filter.as_mut() {
                        Some(filter) => {
                            let (message, hidden) = filter.presence(&event, &allowed.devices);
                            if let Some(hidden) = hidden {
                                usage.unwatch(&hidden);
                            }
                            message
                        }
                        None => event.message(),
                    };
                    if !send(&mut socket, message).await {
                        return;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => match send_snapshot(&state, &mut socket, &allowed, filter.as_mut(), &mut usage).await {
                    Some(sent) => revision = sent,
                    None => return,
                },
                Err(RecvError::Closed) => return,
            },
            frame = socket.recv() => {
                last_heard = Instant::now();
                match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(text.as_str()) {
                    Ok(Request::Watch { devices }) => {
                        let devices = state
                            .presence
                            .devices_among(bounded(devices), &allowed.devices)
                            .await;
                        if let Some(readings) = usage.watch(devices)
                            && !send(&mut socket, Message::Text(readings)).await
                        {
                            return;
                        }
                    }
                    Ok(Request::Refresh) => {
                        match send_snapshot(&state, &mut socket, &allowed, filter.as_mut(), &mut usage).await {
                            Some(sent) => revision = sent,
                            None => return,
                        }
                    }
                    Ok(Request::Ping) => {
                        if !send(&mut socket, Message::Text(r#"{"type":"pong"}"#.into())).await {
                            return;
                        }
                    }
                    Err(_) => {
                        send(&mut socket, close_frame(1003, "unsupported message")).await;
                        return;
                    }
                },
                Some(Ok(Message::Binary(_))) => {
                    send(&mut socket, close_frame(1003, "unsupported message")).await;
                    return;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            }
            },
            event = alert_events.recv(), if allowed.alerts.is_some() => {
                let Some(reach) = &allowed.alerts else { continue };
                let sent = match event {
                    Ok(event) => send_alert(&state, &mut socket, &event, reach, filter.as_ref()).await,
                    Err(RecvError::Lagged(_)) => send_alert_counts(&state, &mut socket, reach).await,
                    Err(RecvError::Closed) => return,
                };
                if !sent {
                    return;
                }
            }
            event = job_events.recv(), if allowed.jobs.is_some() => {
                let Some(viewer) = &allowed.jobs else { continue };
                let sent = match event {
                    Ok(job) if viewer.sees(&job) => {
                        let reached = match job_reach.get(&job.id) {
                            Some(reached) => *reached,
                            None => {
                                // A database failure leaves the job out this time.
                                let reached = viewer
                                    .reaches(&mut &state.database, &job.id)
                                    .await
                                    .unwrap_or(false);
                                if job_reach.len() >= MAX_JOBS_KEPT {
                                    job_reach.clear();
                                }
                                job_reach.insert(job.id.clone(), reached);
                                reached
                            }
                        };
                        !reached
                            || send_json(&mut socket, &json!({ "type": "job_update", "job": viewer.view(&job) })).await
                    }
                    Ok(_) => true,
                    Err(RecvError::Lagged(_)) => send_json(&mut socket, &json!({ "type": "jobs_resync" })).await,
                    Err(RecvError::Closed) => return,
                };
                if !sent {
                    return;
                }
            }
            event = update_events.recv(), if allowed.updates.is_some() => {
                let Some(reach) = &allowed.updates else { continue };
                let sent = match event {
                    Ok(event) => !update_in_reach(&event, reach, filter.as_ref())
                        || send(&mut socket, event.message()).await,
                    Err(RecvError::Lagged(_)) => send_json(&mut socket, &json!({ "type": "updates_resync" })).await,
                    Err(RecvError::Closed) => return,
                };
                if !sent {
                    return;
                }
            }
            changed = access.changed() => {
                let current = allowed.clone();
                let Some(now_allowed) = changed.ok().and(still_allowed(&state, &token, ip, current).await) else {
                    send(&mut socket, close_frame(4001, "sign in again")).await;
                    return;
                };
                job_reach.clear();
                match reallow(&state, &mut socket, &mut allowed, now_allowed, &mut filter, &mut usage).await {
                    Reallowed::Same => {}
                    Reallowed::Snapshot(sent) => revision = sent,
                    Reallowed::Failed => return,
                }
            }
            _ = recheck.tick() => {
                let current = allowed.clone();
                let Some(now_allowed) = still_allowed(&state, &token, ip, current).await else {
                    send(&mut socket, close_frame(4001, "sign in again")).await;
                    return;
                };
                match reallow(&state, &mut socket, &mut allowed, now_allowed, &mut filter, &mut usage).await {
                    Reallowed::Same => {}
                    Reallowed::Snapshot(sent) => revision = sent,
                    Reallowed::Failed => return,
                }
            }
            json = usage.next() => {
                if !send(&mut socket, Message::Text(json)).await {
                    return;
                }
            }
            _ = ping.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT
                    || !send(&mut socket, Message::Ping(Default::default())).await
                {
                    return;
                }
            }
        }
    }
}

/// The most jobs whose reach a limited socket remembers.
const MAX_JOBS_KEPT: usize = 1000;

/// What a socket's user may see.
#[derive(Clone)]
struct Allowed {
    /// The companies whose devices it shows.
    devices: CompanySet,
    /// Alerts and monitor results, of devices in these companies.
    alerts: Option<CompanySet>,
    /// Which jobs, if any.
    jobs: Option<Viewer>,
    /// Update events, of devices in these companies.
    updates: Option<CompanySet>,
}

impl Allowed {
    fn of(actor: &Authorized) -> Self {
        let reach = |permission| actor.has(permission).then(|| actor.companies(permission));
        Self {
            devices: actor.companies(Permission::DevicesView),
            alerts: reach(Permission::AlertsView),
            updates: reach(Permission::UpdatesView),
            jobs: Viewer::may_list(&actor.permissions)
                .then(|| Viewer::new(&actor.user.id, &actor.access)),
        }
    }

    /// The companies each kind of event reaches, to tell when they change.
    fn reach(&self) -> [Option<&CompanySet>; 4] {
        [
            Some(&self.devices),
            self.alerts.as_ref(),
            self.updates.as_ref(),
            self.jobs.as_ref().map(|viewer| &viewer.within),
        ]
    }

    /// A filter for what every socket is sent, when the user sees only some
    /// companies' devices, alerts or updates.
    fn filter(&self) -> Option<Filter> {
        let limited = self
            .reach()
            .iter()
            .take(3)
            .flatten()
            .any(|companies| !companies.is_all());
        limited.then(Filter::default)
    }
}

/// What a socket whose user reaches only some companies has been told, so
/// it can leave out what every other socket is sent.
#[derive(Debug, Default)]
struct Filter {
    /// Every device's company, in reach or not, as presence last told.
    company_of: HashMap<String, Option<String>>,
    /// The devices the socket has told about.
    shown: HashSet<String>,
}

impl Filter {
    /// Starts again from a snapshot.
    fn reset(&mut self, snapshot: &PresenceEvent, devices: &CompanySet) {
        self.company_of.clear();
        self.shown.clear();
        if let PresenceEvent::Snapshot { agents, .. } = snapshot {
            for agent in agents {
                if devices.covers(agent.company_id.as_deref()) {
                    self.shown.insert(agent.id.clone());
                }
                self.company_of
                    .insert(agent.id.clone(), agent.company_id.clone());
            }
        }
    }

    /// Whether the device is in one of `companies`, as presence last told.
    fn covers_device(&self, device_id: &str, companies: &CompanySet) -> bool {
        companies.is_all()
            || self
                .company_of
                .get(device_id)
                .is_some_and(|company| companies.covers(company.as_deref()))
    }

    /// What to send of a presence change, and a device that just left
    /// reach, which the socket stops watching.
    fn presence(
        &mut self,
        event: &Shared<PresenceEvent>,
        devices: &CompanySet,
    ) -> (Message, Option<String>) {
        let skipped = |revision: u64| {
            Message::Text(format!(r#"{{"type":"skipped","revision":{revision}}}"#).into())
        };
        match &**event {
            PresenceEvent::AgentUpsert { revision, agent } => {
                self.company_of
                    .insert(agent.id.clone(), agent.company_id.clone());
                if devices.covers(agent.company_id.as_deref()) {
                    self.shown.insert(agent.id.clone());
                    (event.message(), None)
                } else if self.shown.remove(&agent.id) {
                    // It moved to a company out of reach: gone, to this tab.
                    let deleted = PresenceEvent::AgentDeleted {
                        revision: *revision,
                        agent_id: agent.id.clone(),
                    };
                    let json = serde_json::to_string(&deleted).expect("events serialize");
                    (Message::Text(json.into()), Some(agent.id.clone()))
                } else {
                    (skipped(*revision), None)
                }
            }
            PresenceEvent::AgentDeleted { revision, agent_id } => {
                self.company_of.remove(agent_id);
                if self.shown.remove(agent_id) {
                    (event.message(), None)
                } else {
                    (skipped(*revision), None)
                }
            }
            PresenceEvent::CompanyUpsert { revision, company } => {
                if devices.covers(Some(&company.id)) {
                    (event.message(), None)
                } else {
                    (skipped(*revision), None)
                }
            }
            PresenceEvent::CompanyDeleted {
                revision,
                company_id,
            } => {
                if devices.covers(Some(company_id)) {
                    (event.message(), None)
                } else {
                    (skipped(*revision), None)
                }
            }
            PresenceEvent::Snapshot { .. }
            | PresenceEvent::TagUpsert { .. }
            | PresenceEvent::TagDeleted { .. } => (event.message(), None),
        }
    }
}

/// Whether an update event is about a device in `reach`.
fn update_in_reach(event: &UpdatesEvent, reach: &CompanySet, filter: Option<&Filter>) -> bool {
    if reach.is_all() {
        return true;
    }
    match event {
        UpdatesEvent::DeviceUpdates { device } => reach.covers(device.company_id.as_deref()),
        UpdatesEvent::UpdateRun { run } => {
            filter.is_some_and(|filter| filter.covers_device(&run.device_id, reach))
        }
    }
}

/// Sends an alert event as the socket's user may see it: one about a
/// device out of `reach` not at all, and with counts of only the alerts in
/// reach. Returns whether the socket took it.
async fn send_alert(
    state: &AppState,
    socket: &mut WebSocket,
    event: &Shared<AlertEvent>,
    reach: &CompanySet,
    filter: Option<&Filter>,
) -> bool {
    if reach.is_all() {
        return send(socket, event.message()).await;
    }
    match &**event {
        AlertEvent::AlertUpsert { alert, .. } => {
            if !reach.covers(alert.company_id.as_deref()) {
                return true;
            }
            match store::counts_within(&mut &state.database, reach).await {
                Ok(counts) => {
                    send_json(
                        socket,
                        &AlertEvent::AlertUpsert {
                            alert: alert.clone(),
                            counts,
                        },
                    )
                    .await
                }
                Err(error) => {
                    tracing::warn!(%error, "could not count unresolved alerts");
                    true
                }
            }
        }
        AlertEvent::AlertCounts { .. } => send_alert_counts(state, socket, reach).await,
        AlertEvent::MonitorUpdate { device_id, .. } => {
            if filter.is_some_and(|filter| filter.covers_device(device_id, reach)) {
                send(socket, event.message()).await
            } else {
                true
            }
        }
    }
}

/// What changed in what a socket's user may see.
enum Reallowed {
    Same,
    /// They reach other companies now, so the socket sent a new snapshot
    /// at this revision.
    Snapshot(u64),
    /// The socket or the database failed.
    Failed,
}

/// Takes up `now_allowed`: when the companies the user reaches changed, a
/// new snapshot (with the alert counts); when they may newly see alerts,
/// the counts.
async fn reallow(
    state: &AppState,
    socket: &mut WebSocket,
    allowed: &mut Allowed,
    now_allowed: Allowed,
    filter: &mut Option<Filter>,
    usage: &mut Subscription,
) -> Reallowed {
    let moved = now_allowed.reach() != allowed.reach();
    let newly_alerted = allowed.alerts.is_none() && now_allowed.alerts.is_some();
    *allowed = now_allowed;
    if moved {
        *filter = allowed.filter();
        return match send_snapshot(state, socket, allowed, filter.as_mut(), usage).await {
            Some(sent) => Reallowed::Snapshot(sent),
            None => Reallowed::Failed,
        };
    }
    if newly_alerted
        && let Some(reach) = &allowed.alerts
        && !send_alert_counts(state, socket, reach).await
    {
        return Reallowed::Failed;
    }
    Reallowed::Same
}

/// Whether the socket's session is live and its user may still see devices,
/// and what else they may see. Checking doesn't count as the user's
/// activity.
async fn still_allowed(
    state: &AppState,
    token: &str,
    ip: IpAddr,
    current: Allowed,
) -> Option<Allowed> {
    match session::load(state, token, ip, false).await {
        Ok(signed_in) => Authorized::new(signed_in)
            .ok()
            .filter(|authorized| authorized.has(Permission::DevicesView))
            .map(|authorized| Allowed::of(&authorized)),
        // A database outage doesn't sign anyone out, or change their access.
        Err(error) if error.status().is_server_error() => Some(current),
        Err(_) => None,
    }
}

/// Sends the unresolved alert counts of devices in `reach`. Returns whether
/// the socket took them; a database failure sends nothing.
async fn send_alert_counts(state: &AppState, socket: &mut WebSocket, reach: &CompanySet) -> bool {
    match store::counts_within(&mut &state.database, reach).await {
        Ok(counts) => send_json(socket, &AlertEvent::AlertCounts { counts }).await,
        Err(error) => {
            tracing::warn!(%error, "could not count unresolved alerts");
            true
        }
    }
}

async fn send_json(socket: &mut WebSocket, event: &impl serde::Serialize) -> bool {
    let json = serde_json::to_string(event).expect("events serialize");
    send(socket, Message::Text(json.into())).await
}

/// Sends a snapshot of what the user may see, the alert counts if they may
/// see alerts, and every watched device's latest reading, and returns the
/// snapshot's revision, or `None` if the socket or the database failed. A
/// watched device that is no longer in the fleet, as one deleted while the
/// socket fell behind, or no longer in reach, stops being watched.
async fn send_snapshot(
    state: &AppState,
    socket: &mut WebSocket,
    allowed: &Allowed,
    filter: Option<&mut Filter>,
    usage: &mut Subscription,
) -> Option<u64> {
    let snapshot = match state.presence.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(%error, "could not read the devices for a presence snapshot");
            send(socket, close_frame(1011, "try again")).await;
            return None;
        }
    };
    let sent = match filter {
        Some(filter) => {
            filter.reset(&snapshot, &allowed.devices);
            if allowed.devices.is_all() {
                send(socket, snapshot.message()).await
            } else {
                send_json(socket, &snapshot.within(&allowed.devices)).await
            }
        }
        None => send(socket, snapshot.message()).await,
    };
    sent.then_some(())?;
    if let Some(reach) = &allowed.alerts {
        send_alert_counts(state, socket, reach)
            .await
            .then_some(())?;
    }
    let watching = usage.watching().map(str::to_owned).collect();
    let devices = state
        .presence
        .devices_among(watching, &allowed.devices)
        .await;
    usage.retain(devices);
    if let Some(readings) = usage.restart() {
        send(socket, Message::Text(readings)).await.then_some(())?;
    }
    Some(snapshot.revision())
}
