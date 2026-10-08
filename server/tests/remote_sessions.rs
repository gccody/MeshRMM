//! Remote sessions: handoffs, signaling, resuming and ending, idle expiry,
//! restarts and the session's toolbox, over real WebSockets.
mod common;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use common::{
    Agent, App, Browser, Server, Ws, apps, apps_with, audit_events, quiet, receive_close,
    receive_json, send_json, send_text, set_up, user_with,
};
use futures_util::SinkExt;
use meshrmm_server::realtime::{Sessions, sessions::Timeouts};
use serde_json::{Value, json};

const TECHNICIAN: &[&str] = &["devices.view", "sessions.connect", "scripts.run"];

/// A request the viewer sends with a token and no cookie.
async fn viewer(
    browser: &mut Browser,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> common::Response {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    browser.raw(request).await
}

async fn handoff(browser: &mut Browser, device_id: &str, background: bool) -> String {
    let created = browser
        .post(
            "/v1/remote/handoffs",
            json!({ "device_id": device_id, "start_in_background": background, "reason": "Printer queue" }),
        )
        .await;
    assert_eq!(created.status, 200, "{:?}", created.body);
    created.body["handoff_token"].as_str().unwrap().to_owned()
}

async fn redeem(app: &App, token: &str) -> common::Response {
    viewer(
        &mut app.browser(),
        Method::POST,
        "/v1/remote/handoffs/redeem",
        token,
        None,
    )
    .await
}

/// A session the viewer redeemed, and the request its Agent received.
struct Live {
    id: String,
    client_token: String,
    agent_token: String,
    bootstrap: Value,
    request: Value,
}

/// Starts a session on `agent`'s device as `browser`'s user. The Agent's
/// control connection must be open.
async fn start(app: &App, browser: &mut Browser, agent: &Agent, control: &mut Ws) -> Live {
    let token = handoff(browser, &agent.device_id, false).await;
    let redeemed = redeem(app, &token).await;
    assert_eq!(redeemed.status, 200, "{}: {:?}", app.name, redeemed.body);
    let request = receive_json(control).await;
    assert_eq!(request["session_id"], redeemed.body["session_id"]);
    Live {
        id: redeemed.body["session_id"].as_str().unwrap().to_owned(),
        client_token: redeemed.body["signaling_token"]
            .as_str()
            .unwrap()
            .to_owned(),
        agent_token: request["signaling_token"].as_str().unwrap().to_owned(),
        bootstrap: redeemed.body,
        request,
    }
}

async fn signal(server: &Server, session_id: &str, role: &str, token: &str) -> Result<Ws, u16> {
    common::ws(
        &server.ws_url(&format!(
            "/v1/remote/sessions/{session_id}/signal?role={role}"
        )),
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await
}

impl Live {
    async fn client(&self, server: &Server) -> Ws {
        signal(server, &self.id, "client", &self.client_token)
            .await
            .unwrap()
    }

    async fn agent(&self, server: &Server) -> Ws {
        signal(server, &self.id, "agent", &self.agent_token)
            .await
            .unwrap()
    }

    async fn post(&self, browser: &mut Browser, action: &str) -> common::Response {
        viewer(
            browser,
            Method::POST,
            &format!("/v1/remote/sessions/{}/{action}", self.id),
            &self.client_token,
            None,
        )
        .await
    }

    fn ended(&self) -> Value {
        json!({ "type": "end_session", "session_id": self.id })
    }
}

#[tokio::test]
async fn handoffs_start_sessions_with_online_devices() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;

        let offline = redeem(&app, &handoff(&mut admin, &agent.device_id, false).await).await;
        assert_eq!(offline.status, 409, "{}: {:?}", app.name, offline.body);
        assert_eq!(offline.code(), "device_offline");

        let mut control = agent.connect(&server).await.unwrap();
        let token = handoff(&mut admin, &agent.device_id, false).await;
        let redeemed = redeem(&app, &token).await;
        assert_eq!(redeemed.status, 200, "{:?}", redeemed.body);
        let bootstrap = &redeemed.body;
        assert_eq!(bootstrap["start_in_background"], false);
        assert_eq!(bootstrap["display_border"], true);
        assert_eq!(bootstrap["ice_servers"], json!([]));
        assert_eq!(bootstrap["signaling_token"].as_str().unwrap().len(), 64);
        let request = receive_json(&mut control).await;
        assert_eq!(request["session_id"], bootstrap["session_id"]);
        assert_eq!(request["viewer_name"], "Ada Admin");
        assert_eq!(request["connection_reason"], "Printer queue");
        assert_eq!(request["start_in_background"], false);
        assert_eq!(
            request["expires_at_unix_ms"],
            bootstrap["expires_at_unix_ms"]
        );
        assert_ne!(request["signaling_token"], bootstrap["signaling_token"]);

        // A handoff is used once, and a device has one session at a time.
        let reused = redeem(&app, &token).await;
        assert_eq!(reused.status, 401);
        assert_eq!(reused.code(), "handoff_invalid");
        let busy = redeem(&app, &handoff(&mut admin, &agent.device_id, false).await).await;
        assert_eq!(busy.status, 409);
        assert_eq!(busy.code(), "session_in_progress");
        let expired = handoff(&mut admin, &agent.device_id, false).await;
        {
            use meshrmm_server::db::tables::RemoteHandoffs;
            use sea_query::{Expr, ExprTrait, Query};
            app.db()
                .execute(
                    &Query::update()
                        .table(RemoteHandoffs::Table)
                        .value(RemoteHandoffs::ExpiresAt, 0)
                        .and_where(
                            Expr::col(RemoteHandoffs::TokenHash)
                                .eq(meshrmm_server::secrets::token_hash(&expired)),
                        )
                        .to_owned(),
                )
                .await
                .unwrap();
        }
        assert_eq!(redeem(&app, &expired).await.status, 401);
        assert_eq!(redeem(&app, &"0".repeat(64)).await.status, 401);

        // The background desktop's session is a command.
        let lab = app.enroll(&mut admin, "Lab").await;
        let mut lab_control = lab.connect(&server).await.unwrap();
        let background = redeem(&app, &handoff(&mut admin, &lab.device_id, true).await).await;
        assert_eq!(background.status, 200, "{:?}", background.body);
        assert_eq!(background.body["start_in_background"], true);
        let command = receive_json(&mut lab_control).await;
        assert_eq!(command["type"], "start_background_session");
        assert_eq!(
            command["request"]["session_id"],
            background.body["session_id"]
        );

        // The technician must still be allowed when the viewer redeems.
        let (mut tech, tech_id) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let token = handoff(&mut tech, &agent.device_id, false).await;
        admin
            .patch(&format!("/v1/users/{tech_id}"), json!({ "disabled": true }))
            .await;
        let refused = redeem(&app, &token).await;
        assert_eq!(refused.status, 403);
        assert_eq!(refused.code(), "user_disabled");

        assert_eq!(audit_events(&app, "remote.session_create").await.len(), 2);
        app.finish().await;
    }
}

#[tokio::test]
async fn signaling_relays_between_the_viewer_and_the_agent() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let live = start(&app, &mut admin, &agent, &mut control).await;

        assert_eq!(
            signal(&server, &live.id, "client", &live.agent_token)
                .await
                .unwrap_err(),
            401,
            "{}: each role has its own token",
            app.name
        );
        assert_eq!(
            signal(&server, &live.id, "viewer", &live.client_token)
                .await
                .unwrap_err(),
            400
        );
        assert_eq!(
            signal(&server, "not-a-session", "client", &live.client_token)
                .await
                .unwrap_err(),
            400
        );
        assert_eq!(
            signal(
                &server,
                &uuid::Uuid::new_v4().to_string(),
                "client",
                &live.client_token
            )
            .await
            .unwrap_err(),
            410
        );

        let mut client = live.client(&server).await;
        let mut peer = live.agent(&server).await;
        send_json(&mut client, json!({ "type": "ready" })).await;
        assert_eq!(receive_json(&mut peer).await, json!({ "type": "ready" }));
        let offer = json!({ "type": "offer", "sdp": "v=0" });
        send_json(&mut peer, offer.clone()).await;
        assert_eq!(receive_json(&mut client).await, offer);
        let candidate =
            json!({ "type": "ice_candidate", "candidate": "candidate:1", "sdp_mid": "0" });
        send_json(&mut client, candidate.clone()).await;
        assert_eq!(receive_json(&mut peer).await, candidate);
        // Activity is the viewer's and goes nowhere.
        send_json(&mut client, json!({ "type": "activity" })).await;
        quiet(&mut peer, Duration::from_millis(200)).await;

        // Only the viewer reports activity or ends the session.
        send_json(&mut peer, json!({ "type": "activity" })).await;
        assert_eq!(receive_close(&mut peer).await, 1008);
        let mut peer = live.agent(&server).await;
        send_json(&mut peer, json!({ "type": "end_session" })).await;
        assert_eq!(receive_close(&mut peer).await, 1008);
        let mut peer = live.agent(&server).await;
        send_text(&mut peer, "not json").await;
        assert_eq!(receive_close(&mut peer).await, 1007);
        let mut peer = live.agent(&server).await;
        peer.send(tokio_tungstenite::tungstenite::Message::Binary(
            vec![1, 2, 3].into(),
        ))
        .await
        .unwrap();
        assert_eq!(receive_close(&mut peer).await, 1009);
        let mut peer = live.agent(&server).await;
        send_text(&mut peer, &"x".repeat(64 * 1024 + 1)).await;
        assert_eq!(receive_close(&mut peer).await, 1009);

        // A new socket for a role replaces the old one.
        let mut peer = live.agent(&server).await;
        let mut replacement = live.client(&server).await;
        assert_eq!(receive_close(&mut client).await, 4000);
        send_json(&mut peer, json!({ "type": "ice_complete" })).await;
        assert_eq!(
            receive_json(&mut replacement).await,
            json!({ "type": "ice_complete" })
        );
        quiet(&mut control, Duration::from_millis(100)).await;

        app.finish().await;
    }
}

#[tokio::test]
async fn an_error_before_the_viewer_connects_waits_for_it() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let live = start(&app, &mut admin, &agent, &mut control).await;

        let mut peer = live.agent(&server).await;
        let declined = json!({
            "type": "error",
            "message": "the remote user declined the connection",
            "code": "connection_declined",
        });
        send_json(&mut peer, declined.clone()).await;
        send_json(&mut peer, json!({ "type": "ice_complete" })).await;
        // Let the session handle both before the viewer arrives.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut client = live.client(&server).await;
        assert_eq!(receive_json(&mut client).await, declined, "{}", app.name);
        quiet(&mut client, Duration::from_millis(200)).await;

        app.finish().await;
    }
}

#[tokio::test]
async fn viewers_resume_and_end_sessions() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let live = start(&app, &mut admin, &agent, &mut control).await;
        let mut viewer_http = app.browser();

        let forged = Live {
            client_token: live.agent_token.clone(),
            ..clone(&live)
        };
        assert_eq!(forged.post(&mut viewer_http, "resume").await.status, 401);
        assert_eq!(forged.post(&mut viewer_http, "end").await.status, 401);

        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut old_peer = live.agent(&server).await;
        let resumed = live.post(&mut viewer_http, "resume").await;
        assert_eq!(resumed.status, 200, "{}: {:?}", app.name, resumed.body);
        assert_eq!(resumed.body["session_id"], live.id.as_str());
        assert_eq!(resumed.body["signaling_token"], live.client_token.as_str());
        let refreshed = receive_json(&mut control).await;
        assert_eq!(refreshed["session_id"], live.id.as_str());
        assert_eq!(
            refreshed["expires_at_unix_ms"],
            resumed.body["expires_at_unix_ms"]
        );
        // The Agent ignores a request that differs only in its deadline; a
        // resume must make it restart, so its token changes.
        assert!(differs_beyond_the_deadline(&refreshed, &live.request));
        assert_eq!(receive_close(&mut old_peer).await, 4000);
        assert_eq!(
            signal(&server, &live.id, "agent", &live.agent_token)
                .await
                .unwrap_err(),
            401
        );
        let live = Live {
            agent_token: refreshed["signaling_token"].as_str().unwrap().to_owned(),
            ..clone(&live)
        };

        // An Agent that reconnects gets its session back, as last sent.
        drop(control);
        let mut control = agent.connect(&server).await.unwrap();
        assert_eq!(receive_json(&mut control).await, refreshed);

        let mut client = live.client(&server).await;
        let mut peer = live.agent(&server).await;
        let ended = live.post(&mut viewer_http, "end").await;
        assert_eq!(ended.status, 204, "{:?}", ended.body);
        assert_eq!(receive_close(&mut client).await, 4001);
        assert_eq!(receive_close(&mut peer).await, 4001);
        assert_eq!(receive_json(&mut control).await, live.ended());
        assert_eq!(live.post(&mut viewer_http, "resume").await.status, 410);
        assert_eq!(live.post(&mut viewer_http, "end").await.status, 204);
        assert_eq!(
            signal(&server, &live.id, "client", &live.client_token)
                .await
                .unwrap_err(),
            410
        );
        drop(control);
        let mut control = agent.connect(&server).await.unwrap();
        quiet(&mut control, Duration::from_millis(200)).await;

        // The viewer can also end it over signaling.
        let live = start(&app, &mut admin, &agent, &mut control).await;
        let mut client = live.client(&server).await;
        send_json(&mut client, json!({ "type": "end_session" })).await;
        assert_eq!(receive_close(&mut client).await, 4001);
        assert_eq!(receive_json(&mut control).await, live.ended());

        app.finish().await;
    }
}

/// Whether the Agent restarts its session for `request` while running
/// `active`: it ignores one that only moves the deadline.
fn differs_beyond_the_deadline(request: &Value, active: &Value) -> bool {
    let mut request = request.clone();
    request["expires_at_unix_ms"] = active["expires_at_unix_ms"].clone();
    request != *active
}

fn clone(live: &Live) -> Live {
    Live {
        id: live.id.clone(),
        client_token: live.client_token.clone(),
        agent_token: live.agent_token.clone(),
        bootstrap: live.bootstrap.clone(),
        request: live.request.clone(),
    }
}

fn short_timeouts(state: &mut meshrmm_server::http::AppState) {
    state.sessions = Sessions::new(Timeouts {
        idle: Duration::from_millis(800),
        start: Duration::from_millis(800),
    });
}

#[tokio::test]
async fn sessions_end_when_the_viewer_goes_quiet() {
    for app in apps_with(short_timeouts).await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let live = start(&app, &mut admin, &agent, &mut control).await;
        let mut client = live.client(&server).await;

        // Activity keeps it going well past the idle timeout.
        for _ in 0..8 {
            send_json(&mut client, json!({ "type": "activity" })).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut viewer_http = app.browser();
        assert_eq!(
            live.post(&mut viewer_http, "resume").await.status,
            200,
            "{}",
            app.name
        );
        receive_json(&mut control).await;

        assert_eq!(receive_close(&mut client).await, 4001);
        assert_eq!(receive_json(&mut control).await, live.ended());
        let after = live.post(&mut viewer_http, "resume").await;
        assert_eq!(after.status, 410, "{}: {:?}", app.name, after.body);
        // The device is free again.
        start(&app, &mut admin, &agent, &mut control).await;

        app.finish().await;
    }
}

#[tokio::test]
async fn sessions_can_be_closed_from_the_website() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let (mut tech, _) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let (mut other, _) = user_with(&app, &mut admin, "other@example.com", TECHNICIAN).await;
        let (mut watcher, _) =
            user_with(&app, &mut admin, "watcher@example.com", &["devices.view"]).await;
        let close = format!("/v1/agents/{}/close-session", agent.device_id);

        let none = tech.post(&close, json!({})).await;
        assert_eq!(none.status, 200, "{}: {:?}", app.name, none.body);
        assert_eq!(none.body, json!({ "closed": false }));
        assert_eq!(watcher.post(&close, json!({})).await.status, 403);

        let live = start(&app, &mut tech, &agent, &mut control).await;
        let refused = other.post(&close, json!({})).await;
        assert_eq!(
            refused.status, 403,
            "only their own without sessions.close_any"
        );
        let closed = tech.post(&close, json!({})).await;
        assert_eq!(closed.body, json!({ "closed": true }));
        assert_eq!(receive_json(&mut control).await, live.ended());

        let live = start(&app, &mut tech, &agent, &mut control).await;
        let closed = admin.post(&close, json!({})).await;
        assert_eq!(closed.body, json!({ "closed": true }));
        assert_eq!(receive_json(&mut control).await, live.ended());
        let audited = audit_events(&app, "remote.session_close").await;
        assert_eq!(audited.len(), 2);
        assert_eq!(audited[0].metadata["session_id"], live.id.as_str());

        let missing = admin
            .post(
                &format!("/v1/agents/{}/close-session", uuid::Uuid::new_v4()),
                json!({}),
            )
            .await;
        assert_eq!(missing.status, 404);
        app.finish().await;
    }
}

#[tokio::test]
async fn removing_the_device_or_the_technician_ends_the_session() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let (mut tech, tech_id) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;

        let live = start(&app, &mut tech, &agent, &mut control).await;
        let mut client = live.client(&server).await;
        let disabled = admin
            .patch(&format!("/v1/users/{tech_id}"), json!({ "disabled": true }))
            .await;
        assert_eq!(disabled.status, 200, "{}: {:?}", app.name, disabled.body);
        assert_eq!(receive_close(&mut client).await, 4001);
        assert_eq!(receive_json(&mut control).await, live.ended());

        // Losing the permission to connect ends the session at once.
        let (mut tech, tech_id) =
            user_with(&app, &mut admin, "tech2@example.com", TECHNICIAN).await;
        let live = start(&app, &mut tech, &agent, &mut control).await;
        let role = admin.get(&format!("/v1/users/{tech_id}")).await.body["roles"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["devices.view"] }),
            )
            .await;
        assert_eq!(receive_json(&mut control).await, live.ended());
        assert_eq!(live.post(&mut app.browser(), "resume").await.status, 410);

        let live = start(&app, &mut admin, &agent, &mut control).await;
        let deleted = admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert_eq!(deleted.status, 204);
        assert_eq!(receive_json(&mut control).await, live.ended());
        assert_eq!(
            receive_json(&mut control).await,
            json!({ "type": "uninstall" })
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn sessions_survive_a_restart() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let live = start(&app, &mut admin, &agent, &mut control).await;
        drop(control);

        let restarted = app.restarted().await;
        let server = restarted.serve().await;
        let mut control = agent.connect(&server).await.unwrap();
        assert_eq!(
            receive_json(&mut control).await,
            live.request,
            "{}: the Agent gets its session back",
            app.name
        );
        let resumed = live.post(&mut restarted.browser(), "resume").await;
        assert_eq!(resumed.status, 200, "{:?}", resumed.body);
        let refreshed = receive_json(&mut control).await;
        assert_eq!(refreshed["session_id"], live.id.as_str());
        let live = Live {
            agent_token: refreshed["signaling_token"].as_str().unwrap().to_owned(),
            ..clone(&live)
        };
        let mut client = live.client(&server).await;
        let mut peer = live.agent(&server).await;
        send_json(&mut client, json!({ "type": "ready" })).await;
        assert_eq!(receive_json(&mut peer).await, json!({ "type": "ready" }));

        restarted.finish().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn the_session_toolbox_acts_for_its_technician() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let (mut tech, _) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let created = tech
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Who am I", "language": "cmd", "body": "whoami", "folder": "Info" }),
            )
            .await;
        assert_eq!(created.status, 201, "{:?}", created.body);
        let script_id = created.body["id"].as_str().unwrap().to_owned();
        let admins = admin
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Admin only", "language": "cmd", "body": "ver" }),
            )
            .await;
        let live = start(&app, &mut tech, &agent, &mut control).await;
        let mut viewer_http = app.browser();
        let path = |rest: &str| format!("/v1/remote/sessions/{}/{rest}", live.id);

        let listing = viewer(
            &mut viewer_http,
            Method::GET,
            &path("toolbox"),
            &live.client_token,
            None,
        )
        .await;
        assert_eq!(listing.status, 200, "{}: {:?}", app.name, listing.body);
        assert_eq!(
            listing.body,
            json!({
                "scripts": [{ "id": script_id, "name": "Who am I", "folder": "Info", "language": "cmd", "description": "", "shared": false }],
                "files": [],
            })
        );
        let wrong = viewer(
            &mut viewer_http,
            Method::GET,
            &path("toolbox"),
            &live.agent_token,
            None,
        )
        .await;
        assert_eq!(wrong.status, 401);

        // Someone else's private script is not the technician's to run.
        let refused = viewer(
            &mut viewer_http,
            Method::POST,
            &path("script-runs"),
            &live.client_token,
            Some(json!({ "script_id": admins.body["id"], "run_as": "system" })),
        )
        .await;
        assert_eq!(refused.status, 404);
        let started = viewer(
            &mut viewer_http,
            Method::POST,
            &path("script-runs"),
            &live.client_token,
            Some(json!({ "script_id": script_id, "run_as": "system" })),
        )
        .await;
        assert_eq!(started.status, 201, "{:?}", started.body);
        assert_eq!(started.body["status"], "pending");
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        let command = receive_json(&mut control).await;
        assert_eq!(command["type"], "run_script");
        assert_eq!(command["run"]["run_id"], run_id.as_str());
        let reported = agent
            .report(
                &format!("script-runs/{run_id}/result"),
                json!({ "status": "completed", "ran_as": "SYSTEM", "exit_code": 0, "stdout": "nt authority\\system", "stderr": "" }),
            )
            .await;
        assert_eq!(reported.status, 204);
        let run = viewer(
            &mut viewer_http,
            Method::GET,
            &path(&format!("script-runs/{run_id}")),
            &live.client_token,
            None,
        )
        .await;
        assert_eq!(run.body["status"], "completed", "{:?}", run.body);
        assert_eq!(run.body["stdout"], "nt authority\\system");
        let audited = audit_events(&app, "script.run").await;
        assert_eq!(audited[0].metadata["source"], "session");

        let delivery = viewer(
            &mut viewer_http,
            Method::POST,
            &path("file-deliveries"),
            &live.client_token,
            Some(json!({ "file_id": uuid::Uuid::new_v4().to_string(), "background": true })),
        )
        .await;
        assert_eq!(delivery.status, 403, "the technician may not deliver files");
        assert_eq!(delivery.code(), "permission_denied");

        assert_eq!(live.post(&mut viewer_http, "end").await.status, 204);
        let after = viewer(
            &mut viewer_http,
            Method::GET,
            &path("toolbox"),
            &live.client_token,
            None,
        )
        .await;
        assert_eq!(after.status, 410);
        assert_eq!(after.code(), "session_ended");
        app.finish().await;
    }
}

#[tokio::test]
async fn ending_a_session_needs_no_websocket() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut control = agent.connect(&server).await.unwrap();
        let live = start(&app, &mut admin, &agent, &mut control).await;
        assert_eq!(
            live.post(&mut app.browser(), "end").await.status,
            StatusCode::NO_CONTENT,
            "{}",
            app.name
        );
        assert_eq!(receive_json(&mut control).await, live.ended());
        app.finish().await;
    }
}
