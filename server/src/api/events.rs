//! `GET /v1/events`: the website's socket for device presence, resource
//! usage and alerts.
//!
//! It sends a snapshot of the devices, then each change with the next
//! revision. Live resource usage, which carries no revision, it sends only
//! for the devices the tab says it shows. The tab sends JSON messages
//! ([`Request`]):
//!
//! - `{"type":"watch","devices":[...]}` replaces the devices the socket
//!   watches; an empty list watches none, and a socket opens watching none.
//!   It takes the first [`MAX_WATCHED`] distinct IDs and ignores the rest,
//!   and ignores IDs of no device the user may see: a user who may see
//!   devices (the permission the socket needs) sees every device, so an ID
//!   is ignored when it is no device or a deleted one. The socket sends the
//!   latest reading of each device it starts watching at once, then every
//!   few seconds the watched devices' readings taken since, and after each
//!   snapshot every watched device's latest. A deleted device is dropped
//!   from the set.
//! - `{"type":"refresh"}` asks for a new snapshot, when the tab missed a
//!   change.
//! - `{"type":"ping"}` is answered with `{"type":"pong"}`, so a tab can tell
//!   a quiet socket from a dead one.
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
    collections::HashSet,
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
    },
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
    allowed: Allowed,
    mut socket: WebSocket,
) {
    let Allowed {
        mut alerts,
        mut jobs,
        mut updates,
    } = allowed;
    let mut events = state.presence.frames();
    let mut alert_events = state.alerts.subscribe();
    let mut job_events = state.automation.subscribe();
    let mut update_events = state.updates.subscribe();
    let mut access = state.presence.access_changes();
    let mut usage = state.metrics.subscribe();
    let Some(mut revision) = send_snapshot(&state, &mut socket, alerts, &mut usage).await else {
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
                    if !send(&mut socket, event.message()).await {
                        return;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => match send_snapshot(&state, &mut socket, alerts, &mut usage).await {
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
                        let devices = state.presence.devices_among(bounded(devices)).await;
                        if let Some(readings) = usage.watch(devices)
                            && !send(&mut socket, Message::Text(readings)).await
                        {
                            return;
                        }
                    }
                    Ok(Request::Refresh) => {
                        match send_snapshot(&state, &mut socket, alerts, &mut usage).await {
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
            event = alert_events.recv(), if alerts => {
                let sent = match event {
                    Ok(event) => send(&mut socket, event.message()).await,
                    Err(RecvError::Lagged(_)) => send_alert_counts(&state, &mut socket).await,
                    Err(RecvError::Closed) => return,
                };
                if !sent {
                    return;
                }
            }
            event = job_events.recv(), if jobs.is_some() => {
                let Some(viewer) = &jobs else { continue };
                let sent = match event {
                    Ok(job) if viewer.sees(&job) => {
                        send_json(&mut socket, &json!({ "type": "job_update", "job": viewer.view(&job) })).await
                    }
                    Ok(_) => true,
                    Err(RecvError::Lagged(_)) => send_json(&mut socket, &json!({ "type": "jobs_resync" })).await,
                    Err(RecvError::Closed) => return,
                };
                if !sent {
                    return;
                }
            }
            event = update_events.recv(), if updates => {
                let sent = match event {
                    Ok(event) => send(&mut socket, event.message()).await,
                    Err(RecvError::Lagged(_)) => send_json(&mut socket, &json!({ "type": "updates_resync" })).await,
                    Err(RecvError::Closed) => return,
                };
                if !sent {
                    return;
                }
            }
            changed = access.changed() => {
                let current = Allowed { alerts, jobs: jobs.take(), updates };
                let Some(allowed) = changed.ok().and(still_allowed(&state, &token, ip, current).await) else {
                    send(&mut socket, close_frame(4001, "sign in again")).await;
                    return;
                };
                if allowed.alerts && !alerts && !send_alert_counts(&state, &mut socket).await {
                    return;
                }
                alerts = allowed.alerts;
                jobs = allowed.jobs;
                updates = allowed.updates;
            }
            _ = recheck.tick() => {
                let current = Allowed { alerts, jobs: jobs.take(), updates };
                let Some(allowed) = still_allowed(&state, &token, ip, current).await else {
                    send(&mut socket, close_frame(4001, "sign in again")).await;
                    return;
                };
                if allowed.alerts && !alerts && !send_alert_counts(&state, &mut socket).await {
                    return;
                }
                alerts = allowed.alerts;
                jobs = allowed.jobs;
                updates = allowed.updates;
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

/// What a socket's user may see besides devices.
struct Allowed {
    alerts: bool,
    /// Which jobs, if any.
    jobs: Option<Viewer>,
    updates: bool,
}

impl Allowed {
    fn of(actor: &Authorized) -> Self {
        Self {
            alerts: actor.has(Permission::AlertsView),
            updates: actor.has(Permission::UpdatesView),
            jobs: Viewer::may_list(&actor.permissions)
                .then(|| Viewer::new(&actor.user.id, &actor.permissions)),
        }
    }
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

/// Sends the unresolved alert counts. Returns whether the socket took them;
/// a database failure sends nothing.
async fn send_alert_counts(state: &AppState, socket: &mut WebSocket) -> bool {
    match store::counts(&mut &state.database).await {
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

/// Sends a snapshot, the alert counts if `alerts`, and every watched
/// device's latest reading, and returns the snapshot's revision, or `None`
/// if the socket or the database failed. A watched device that is no longer
/// in the fleet, as one deleted while the socket fell behind, stops being
/// watched.
async fn send_snapshot(
    state: &AppState,
    socket: &mut WebSocket,
    alerts: bool,
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
    send(socket, snapshot.message()).await.then_some(())?;
    if alerts {
        send_alert_counts(state, socket).await.then_some(())?;
    }
    let watching = usage.watching().map(str::to_owned).collect();
    let devices = state.presence.devices_among(watching).await;
    usage.retain(devices);
    if let Some(readings) = usage.restart() {
        send(socket, Message::Text(readings)).await.then_some(())?;
    }
    Some(snapshot.revision())
}
