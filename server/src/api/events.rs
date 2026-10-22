//! `GET /v1/events`: the website's socket for device presence, resource
//! usage and alerts.
//!
//! It sends a snapshot of the devices, then each change with the next
//! revision. The website sends `refresh` for a new snapshot when it missed a
//! change. Every few seconds it also sends the latest resource usage of the
//! devices that reported since it last did, which carries no revision: after
//! each snapshot it sends every online device's. A user who may see alerts
//! also gets the unresolved alert counts after each snapshot, and each alert
//! that is raised or changes ([`AlertEvent`]). A user who may see jobs gets
//! `job_update` with each job they may see whose counts changed, and
//! `jobs_resync` if they missed some. A user who may see updates gets
//! `device_updates` with each device whose update status changes and
//! `update_run` with each scan or install that starts, progresses or ends,
//! and `updates_resync` if they missed some. The socket stays authorized by
//! the session cookie it opened with: it checks the session and the
//! `devices.view` permission again every half minute, and at once after a
//! signed-in user changes something, and closes with 4001 when either is
//! gone.
use std::{
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
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{
    alerts::{AlertEvent, store},
    auth::{Authorized, session},
    automation::jobs::Viewer,
    http::{ApiError, AppState, client_ip::ClientIp},
    rbac::Permission,
    realtime::{close_frame, metrics::MetricsEvent, presence::PresenceEvent, send},
};

const RECHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Browsers don't ping; this keeps proxies from closing a quiet socket and
/// finds dead ones.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A browser that hasn't answered two pings is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
/// How often new resource usage readings are sent, as often as Agents report.
const METRICS_INTERVAL: Duration =
    Duration::from_secs(meshrmm_protocol_types::METRICS_INTERVAL_SECONDS);

pub async fn subscribe(
    State(state): State<AppState>,
    actor: Authorized,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
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
        .max_message_size(1024)
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
    let mut events = state.presence.subscribe();
    let mut alert_events = state.alerts.subscribe();
    let mut job_events = state.automation.subscribe();
    let mut update_events = state.updates.subscribe();
    let mut access = state.presence.access_changes();
    let Some(mut revision) = send_snapshot(&state, &mut socket, alerts).await else {
        return;
    };
    let mut recheck = tokio::time::interval(RECHECK_INTERVAL);
    let mut ping = tokio::time::interval(PING_INTERVAL);
    // Both fire at once; the socket was just authorized.
    recheck.tick().await;
    ping.tick().await;
    // Fires at once, sending every online device's usage.
    let mut metrics = tokio::time::interval(METRICS_INTERVAL);
    metrics.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut metrics_sent = 0;
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) if event.revision() > revision => {
                    revision = event.revision();
                    if !send_event(&mut socket, &event).await {
                        return;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => match send_snapshot(&state, &mut socket, alerts).await {
                    Some(sent) => {
                        revision = sent;
                        metrics_sent = 0;
                        metrics.reset_immediately();
                    }
                    None => return,
                },
                Err(RecvError::Closed) => return,
            },
            frame = socket.recv() => {
                last_heard = Instant::now();
                match frame {
                Some(Ok(Message::Text(text))) if text.as_str() == "refresh" => {
                    match send_snapshot(&state, &mut socket, alerts).await {
                        Some(sent) => {
                            revision = sent;
                            metrics_sent = 0;
                            metrics.reset_immediately();
                        }
                        None => return,
                    }
                }
                Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                    send(&mut socket, close_frame(1003, "unsupported message")).await;
                    return;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            }
            },
            event = alert_events.recv(), if alerts => {
                let sent = match event {
                    Ok(event) => send_json(&mut socket, &*event).await,
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
                    Ok(event) => send_json(&mut socket, &*event).await,
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
            _ = metrics.tick() => {
                let (readings, sequence) = state.metrics.since(metrics_sent);
                metrics_sent = sequence;
                if !readings.is_empty() {
                    let json = serde_json::to_string(&MetricsEvent::Metrics { readings })
                        .expect("metrics events serialize");
                    if !send(&mut socket, Message::Text(json.into())).await {
                        return;
                    }
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

/// Sends a snapshot, and the alert counts if `alerts`, and returns the
/// snapshot's revision, or `None` if the socket or the database failed.
async fn send_snapshot(state: &AppState, socket: &mut WebSocket, alerts: bool) -> Option<u64> {
    let snapshot = match state.presence.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(%error, "could not read the devices for a presence snapshot");
            send(socket, close_frame(1011, "try again")).await;
            return None;
        }
    };
    send_event(socket, &snapshot).await.then_some(())?;
    if alerts {
        send_alert_counts(state, socket).await.then_some(())?;
    }
    Some(snapshot.revision())
}

async fn send_event(socket: &mut WebSocket, event: &PresenceEvent) -> bool {
    let json = serde_json::to_string(event).expect("presence events serialize");
    send(socket, Message::Text(json.into())).await
}
