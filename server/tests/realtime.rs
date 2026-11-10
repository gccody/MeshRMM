//! Agents' control connections and the website's presence socket, over real
//! WebSockets.
mod common;

use std::time::{Duration, Instant};

use common::{
    App, Received, apps, apps_with, quiet, receive, receive_close, receive_json, send_json,
    send_text, set_up, user_with,
};
use serde_json::{Value, json};

/// The device in an event, or in a snapshot's list.
fn device<'a>(event: &'a Value, id: &str) -> &'a Value {
    match event["type"].as_str() {
        Some("snapshot") => event["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == id)
            .unwrap_or_else(|| panic!("{id} is not in {event}")),
        Some("agent_upsert") => {
            assert_eq!(event["agent"]["id"], id, "{event}");
            &event["agent"]
        }
        _ => panic!("not a device event: {event}"),
    }
}

#[tokio::test]
async fn an_agent_connection_is_presence_and_carries_commands() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;

        let mut events = admin.events(&server).await.unwrap();
        let snapshot = receive_json(&mut events).await;
        assert_eq!(snapshot["type"], "snapshot", "{}", app.name);
        assert_eq!(device(&snapshot, &agent.device_id)["connected"], false);
        assert!(device(&snapshot, &agent.device_id)["created_at"].is_i64());
        let revision = snapshot["revision"].as_u64().unwrap();

        // The credential is checked before the upgrade.
        let impostor = agent.with_token(&"0".repeat(64));
        assert_eq!(impostor.connect(&server).await.unwrap_err(), 401);

        let mut control = agent.connect(&server).await.unwrap();
        let online = receive_json(&mut events).await;
        assert_eq!(online["type"], "agent_upsert");
        assert_eq!(online["revision"], revision + 1);
        assert_eq!(device(&online, &agent.device_id)["connected"], true);
        let listed = admin.get("/v1/agents").await;
        assert_eq!(listed.body["type"], "snapshot");
        assert_eq!(listed.body["revision"], revision + 1);
        assert_eq!(device(&listed.body, &agent.device_id)["connected"], true);

        // A rotation reaches the socket, and is sent again on reconnecting
        // until the Agent uses it.
        let rotated = admin
            .post(
                &format!("/v1/agents/{}/rotate-credential", agent.device_id),
                json!({}),
            )
            .await;
        assert_eq!(rotated.status, 202, "{:?}", rotated.body);
        let rotation = receive_json(&mut control).await;
        assert_eq!(rotation["type"], "rotate_token");
        let token = rotation["token"].as_str().unwrap().to_owned();

        let mut replacement = agent.connect(&server).await.unwrap();
        assert_eq!(receive_close(&mut control).await, 4000, "superseded");
        assert_eq!(receive_json(&mut replacement).await, rotation);
        drop(replacement);
        let offline = receive_json(&mut events).await;
        assert_eq!(device(&offline, &agent.device_id)["connected"], false);

        let rotated_agent = agent.with_token(&token);
        let mut control = rotated_agent.connect(&server).await.unwrap();
        let online = receive_json(&mut events).await;
        assert_eq!(device(&online, &agent.device_id)["connected"], true);
        quiet(&mut control, Duration::from_millis(300)).await;
        assert_eq!(agent.connect(&server).await.unwrap_err(), 401, "retired");

        // Messages the server doesn't know end the connection.
        send_text(&mut control, "ping").await;
        assert_eq!(receive_close(&mut control).await, 1003);
        let offline = receive_json(&mut events).await;
        assert_eq!(device(&offline, &agent.device_id)["connected"], false);

        let mut control = rotated_agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(
            &mut control,
            json!({ "type": "updating", "version": "1.2.3; rm" }),
        )
        .await;
        assert_eq!(receive_close(&mut control).await, 1003);
        let offline = receive_json(&mut events).await;
        assert!(device(&offline, &agent.device_id)["updating_to"].is_null());

        app.finish().await;
    }
}

#[tokio::test]
async fn an_agent_that_goes_offline_to_update_shows_as_updating() {
    fn short_grace(state: &mut meshrmm_server::http::AppState) {
        state.presence = meshrmm_server::realtime::Presence::new(
            state.database.clone(),
            state.agents.clone(),
            // Long enough for a slow runner to publish the disconnect
            // inside it.
            Duration::from_millis(1500),
        );
    }
    for app in apps_with(short_grace).await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;

        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(
            &mut control,
            json!({ "type": "updating", "version": "1.2.3" }),
        )
        .await;
        // While it is still connected, nothing changes.
        quiet(&mut events, Duration::from_millis(200)).await;
        drop(control);
        let updating = receive_json(&mut events).await;
        let state = device(&updating, &agent.device_id);
        assert_eq!(state["connected"], false, "{}", app.name);
        assert_eq!(state["updating_to"], "1.2.3");

        // An update that never brings the Agent back shows as offline.
        let given_up = receive_json(&mut events).await;
        let state = device(&given_up, &agent.device_id);
        assert_eq!(state["connected"], false);
        assert!(state["updating_to"].is_null(), "{given_up}");

        // One that does clears it at once.
        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(
            &mut control,
            json!({ "type": "updating", "version": "1.2.4" }),
        )
        .await;
        quiet(&mut events, Duration::from_millis(100)).await;
        drop(control);
        assert_eq!(
            device(&receive_json(&mut events).await, &agent.device_id)["updating_to"],
            "1.2.4"
        );
        let _control = agent.connect(&server).await.unwrap();
        let back = receive_json(&mut events).await;
        let state = device(&back, &agent.device_id);
        assert_eq!(state["connected"], true);
        assert!(state["updating_to"].is_null());
        quiet(&mut events, Duration::from_millis(1700)).await;

        app.finish().await;
    }
}

#[tokio::test]
async fn deleted_devices_leave_presence_and_their_agents_uninstall() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;

        let deleted = admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert_eq!(deleted.status, 204, "{}", app.name);
        assert_eq!(
            receive_json(&mut control).await,
            json!({ "type": "uninstall" })
        );
        let gone = receive_json(&mut events).await;
        assert_eq!(gone["type"], "agent_deleted");
        assert_eq!(gone["agent_id"], agent.device_id.as_str());

        // An Agent that missed it hears it when it reconnects, and is not
        // shown again.
        drop(control);
        let mut control = agent.connect(&server).await.unwrap();
        assert_eq!(
            receive_json(&mut control).await,
            json!({ "type": "uninstall" })
        );
        send_json(&mut control, json!({ "type": "uninstall_scheduled" })).await;
        assert_eq!(receive_close(&mut control).await, 4001);
        quiet(&mut events, Duration::from_millis(300)).await;
        send_text(&mut events, "refresh").await;
        let snapshot = receive_json(&mut events).await;
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["agents"], json!([]));
        assert_eq!(snapshot["revision"], gone["revision"]);

        app.finish().await;
    }
}

#[tokio::test]
async fn enrollments_are_announced_and_snapshots_converge() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let mut events = admin.events(&server).await.unwrap();
        let empty = receive_json(&mut events).await;
        assert_eq!(empty["agents"], json!([]), "{}", app.name);

        let first = app.enroll(&mut admin, "Zulu").await;
        let added = receive_json(&mut events).await;
        assert_eq!(added["type"], "agent_upsert");
        assert_eq!(device(&added, &first.device_id)["name"], "Zulu");
        let second = app.enroll(&mut admin, "alpha").await;
        receive_json(&mut events).await;
        let _control = second.connect(&server).await.unwrap();
        let online = receive_json(&mut events).await;

        // A second socket starts at the same revision with the same state.
        let mut other = admin.events(&server).await.unwrap();
        let snapshot = receive_json(&mut other).await;
        assert_eq!(snapshot["revision"], online["revision"]);
        let order = snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(order, ["alpha", "Zulu"], "online first, then by name");

        app.finish().await;
    }
}

/// How often an event socket checks its user's access by itself.
const PERIODIC_RECHECK: Duration = Duration::from_secs(30);

/// The generation event sockets, terminals and remote sessions watch to
/// check their user's access again.
fn access_generation(app: &App) -> u64 {
    *app.state.presence.access_changes().borrow()
}

/// A socket opened at `opened` closed before its first periodic check, so
/// the change's signal closed it.
fn assert_signalled(opened: Instant) {
    assert!(
        opened.elapsed() < PERIODIC_RECHECK,
        "closed only after {:?}",
        opened.elapsed()
    );
}

/// Opens an event socket for a new user who may only see devices, makes
/// `change` as the administrator, and checks the socket closes at once.
async fn assert_closes_for(
    app: &App,
    server: &common::Server,
    admin: &mut common::Browser,
    change: impl AsyncFnOnce(&mut common::Browser, &str),
) {
    let email = format!("{}@example.com", common::random_hex(4));
    let (mut user, user_id) = user_with(app, admin, &email, &["devices.view"]).await;
    let mut events = user.events(server).await.unwrap();
    let opened = Instant::now();
    receive_json(&mut events).await;
    change(admin, &user_id).await;
    assert_eq!(receive_close(&mut events).await, 4001, "{}", app.name);
    assert_signalled(opened);
    assert!(user.get("/v1/agents").await.status.is_client_error());
}

#[tokio::test]
async fn the_event_socket_needs_the_website_and_permission_to_see_devices() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;

        assert_eq!(app.browser().events(&server).await.unwrap_err(), 401);
        let mut foreign = admin.clone();
        let cookie = foreign.cookie.take().unwrap();
        let wrong_origin = common::ws(
            &server.ws_url("/v1/events"),
            &[("origin", "https://evil.example"), ("cookie", &cookie)],
        )
        .await;
        assert_eq!(wrong_origin.unwrap_err(), 403, "{}", app.name);
        let no_origin = common::ws(&server.ws_url("/v1/events"), &[("cookie", &cookie)]).await;
        assert_eq!(no_origin.unwrap_err(), 403);
        let (scripter, _) =
            user_with(&app, &mut admin, "scripter@example.com", &["scripts.run"]).await;
        assert_eq!(scripter.events(&server).await.unwrap_err(), 403);

        // Anything else from the website ends the socket.
        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        send_text(&mut events, "hello").await;
        assert_eq!(receive_close(&mut events).await, 1003);

        // Losing access closes it at once.
        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let response = admin
                .patch(&format!("/v1/users/{user_id}"), json!({ "disabled": true }))
                .await;
            assert_eq!(response.status, 200, "{:?}", response.body);
        })
        .await;
        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let response = admin
                .post(&format!("/v1/users/{user_id}/sign-out"), json!({}))
                .await;
            assert_eq!(response.status, 204, "{:?}", response.body);
        })
        .await;
        let (mut viewer, viewer_id) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let role = admin.get(&format!("/v1/users/{viewer_id}")).await.body["roles"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut events = viewer.events(&server).await.unwrap();
        receive_json(&mut events).await;
        let response = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["scripts.run"] }),
            )
            .await;
        assert_eq!(response.status, 200, "{:?}", response.body);
        assert_eq!(receive_close(&mut events).await, 4001);
        // Signing out on one's own closes it too.
        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        admin.post("/v1/auth/sign-out", json!({})).await;
        assert_eq!(receive_close(&mut events).await, 4001);
        assert_eq!(viewer.get("/v1/agents").await.status, 403);

        assert!(matches!(receive(&mut events).await, Received::Gone));
        app.finish().await;
    }
}

#[tokio::test]
async fn only_changes_to_access_make_sockets_check_it_again() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let _connection = app.state.agents.connect(&desk.device_id);
        let (viewer, viewer_id) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let mut events = viewer.events(&server).await.unwrap();
        let opened = Instant::now();
        receive_json(&mut events).await;
        let before = access_generation(&app);

        // Everyday work doesn't change anyone's access.
        let tag = admin
            .post("/v1/tags", json!({ "name": "Servers", "color": "blue" }))
            .await;
        assert_eq!(tag.status, 201, "{name}: {:?}", tag.body);
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [desk.device_id], "add": [tag.body["id"]] }),
            )
            .await;
        assert_eq!(tagged.status, 200, "{name}: {:?}", tagged.body);
        let noted = admin
            .put(
                &format!("/v1/agents/{}/notes", desk.device_id),
                json!({ "body": "Front desk PC", "revision": 0 }),
            )
            .await;
        assert_eq!(noted.status, 200, "{name}: {:?}", noted.body);
        let company = admin.post("/v1/companies", json!({ "name": "Acme" })).await;
        assert_eq!(company.status, 201, "{name}: {:?}", company.body);
        let moved = admin
            .put(
                &format!("/v1/agents/{}/company", desk.device_id),
                json!({ "company_id": company.body["id"] }),
            )
            .await;
        assert_eq!(moved.status, 204, "{name}: {:?}", moved.body);
        let script = admin
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Date", "language": "powershell", "body": "Get-Date", "shared": false }),
            )
            .await;
        assert_eq!(script.status, 201, "{name}: {:?}", script.body);
        let run = admin
            .post(
                &format!("/v1/agents/{}/script-runs", desk.device_id),
                json!({ "script_id": script.body["id"], "run_as": "user" }),
            )
            .await;
        assert_eq!(run.status, 201, "{name}: {:?}", run.body);
        // Nor does a stranger's request, even one that succeeds.
        let stranger = app.browser().post("/v1/auth/sign-out", json!({})).await;
        assert_eq!(stranger.status, 204, "{name}");
        let reset = app
            .browser()
            .post(
                "/v1/auth/password-reset",
                json!({ "email": "viewer@example.com" }),
            )
            .await;
        assert_eq!(reset.status, 202, "{name}: {:?}", reset.body);
        assert_eq!(access_generation(&app), before, "{name}");

        // Signing in can drop roles from SSO groups.
        let signed_in = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": "viewer@example.com", "password": "a long enough password" }),
            )
            .await;
        assert_eq!(signed_in.status, 200, "{name}: {:?}", signed_in.body);
        assert!(access_generation(&app) > before, "{name}");

        // A change to access is signalled, and the socket closes at once.
        let role = admin.get(&format!("/v1/users/{viewer_id}")).await.body["roles"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let changed = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["scripts.run"] }),
            )
            .await;
        assert_eq!(changed.status, 200, "{name}: {:?}", changed.body);
        assert_eq!(receive_close(&mut events).await, 4001, "{name}");
        assert_signalled(opened);
        app.finish().await;
    }
}

#[tokio::test]
async fn every_change_that_can_end_access_closes_sockets_at_once() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;

        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let response = admin
                .patch(&format!("/v1/users/{user_id}"), json!({ "role_ids": [] }))
                .await;
            assert_eq!(response.status, 200, "{:?}", response.body);
        })
        .await;
        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let response = admin.delete(&format!("/v1/users/{user_id}")).await;
            assert_eq!(response.status, 204, "{:?}", response.body);
        })
        .await;
        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let role = admin.get(&format!("/v1/users/{user_id}")).await.body["roles"][0]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let response = admin.delete(&format!("/v1/roles/{role}")).await;
            assert_eq!(response.status, 204, "{:?}", response.body);
        })
        .await;
        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let response = admin
                .post(&format!("/v1/users/{user_id}/reset-two-factor"), json!({}))
                .await;
            assert_eq!(response.status, 204, "{:?}", response.body);
        })
        .await;
        // The reset link is used by someone not signed in, often elsewhere.
        assert_closes_for(&app, &server, &mut admin, async |admin, user_id| {
            let reset = admin
                .post(&format!("/v1/users/{user_id}/password-reset"), json!({}))
                .await;
            assert_eq!(reset.status, 200, "{:?}", reset.body);
            let token = common::link_token(reset.body["link"].as_str().unwrap());
            let done = app
                .browser()
                .post(
                    "/v1/auth/password-reset/complete",
                    json!({ "token": token, "password": "a brand new password" }),
                )
                .await;
            assert_eq!(done.status, 204, "{:?}", done.body);
        })
        .await;

        // The user's own changes from another browser.
        for change in ["end that session", "change the password"] {
            let email = format!("{}@example.com", common::random_hex(4));
            let (mut user, _) = user_with(&app, &mut admin, &email, &["devices.view"]).await;
            let mut other = app.browser();
            let signed_in = other
                .post(
                    "/v1/auth/sign-in",
                    json!({ "email": email, "password": "a long enough password" }),
                )
                .await;
            assert_eq!(signed_in.status, 200, "{:?}", signed_in.body);
            let mut events = user.events(&server).await.unwrap();
            let opened = Instant::now();
            receive_json(&mut events).await;
            let response = if change == "end that session" {
                let sessions = other.get("/v1/account/sessions").await.body;
                let first = sessions
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|session| session["current"] == false)
                    .unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                other.delete(&format!("/v1/account/sessions/{first}")).await
            } else {
                other
                    .post(
                        "/v1/account/password",
                        json!({ "current_password": "a long enough password", "new_password": "a brand new password" }),
                    )
                    .await
            };
            assert_eq!(
                response.status, 204,
                "{}: {change}: {:?}",
                app.name, response.body
            );
            assert_eq!(
                receive_close(&mut events).await,
                4001,
                "{}: {change}",
                app.name
            );
            assert_signalled(opened);
            assert_eq!(user.get("/v1/agents").await.status, 401);
        }

        // Last, as it confines the administrator too: requiring a second
        // factor confines a password session without one to enrolling.
        assert_closes_for(&app, &server, &mut admin, async |admin, _| {
            let response = admin
                .patch(
                    "/v1/settings/authentication",
                    json!({ "require_two_factor": true }),
                )
                .await;
            assert_eq!(response.status, 200, "{:?}", response.body);
        })
        .await;
        app.finish().await;
    }
}

fn metrics_report(cpu: f64, memory: u64) -> Value {
    json!({
        "type": "metrics",
        "metrics": {
            "cpu_percent": cpu,
            "memory_used_bytes": memory,
            "memory_total_bytes": 16_000,
            "network_received_bytes_per_second": 2_000,
            "network_sent_bytes_per_second": 500,
            "uptime_seconds": 3_600,
            "volumes": [{ "name": "C:", "total_bytes": 1_000, "free_bytes": 250 }]
        }
    })
}

/// The device's stored minutes, as (samples, average CPU, peak CPU).
async fn stored_minutes(app: &App, device_id: &str) -> Vec<(i64, f64, f64)> {
    use sea_query::{Expr, ExprTrait, Query};
    app.db()
        .fetch_all(
            &Query::select()
                .columns(["samples", "cpu_percent", "cpu_percent_max"])
                .from("device_metrics")
                .and_where(Expr::col("device_id").eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn agents_report_resource_usage_to_the_website_and_history() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let path = format!("/v1/agents/{}/metrics", agent.device_id);
        let empty = admin.get(&path).await;
        assert_eq!(empty.status, 200, "{:?}", empty.body);
        assert_eq!(empty.body["range"], "live");
        assert!(empty.body["latest"].is_null());
        assert_eq!(empty.body["points"], json!([]));

        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(&mut control, metrics_report(20.0, 4_000)).await;

        // The event socket sends new readings every few seconds.
        let event = receive_json(&mut events).await;
        assert_eq!(event["type"], "metrics", "{}", app.name);
        let reading = &event["readings"][0];
        assert_eq!(reading["device_id"], agent.device_id.as_str());
        assert_eq!(reading["cpu_percent"], 20.0);
        assert_eq!(reading["volumes"][0]["name"], "C:");
        assert!(reading["at"].is_i64());

        let live = admin.get(&path).await;
        assert_eq!(live.body["step_ms"], 5_000);
        assert_eq!(live.body["latest"]["memory_used_bytes"], 4_000);
        let point = &live.body["points"][0];
        assert_eq!(point["cpu_percent"], 20.0);
        assert_eq!(point["storage_used_bytes"], 750);
        assert_eq!(point["storage_total_bytes"], 1_000);

        // Going offline queues the unfinished minute for the history writer,
        // and ends the live view.
        drop(control);
        receive_json(&mut events).await;
        assert!(stored_minutes(&app, &agent.device_id).await.is_empty());
        let flushed = app.state.metrics.flush().await.unwrap();
        assert_eq!(
            (flushed.minutes, flushed.statements),
            (1, 1),
            "{}",
            app.name
        );
        let stored = stored_minutes(&app, &agent.device_id).await;
        assert_eq!(stored, [(1, 20.0, 20.0)], "{}", app.name);
        assert!(admin.get(&path).await.body["latest"].is_null());

        // A second connection's readings in the same minute merge into it.
        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(&mut control, metrics_report(60.0, 8_000)).await;
        assert_eq!(receive_json(&mut events).await["type"], "metrics");
        drop(control);
        receive_json(&mut events).await;
        app.state.metrics.flush().await.unwrap();
        let stored = stored_minutes(&app, &agent.device_id).await;
        // Unless the minute turned between the two readings.
        if stored.len() == 1 {
            assert_eq!(stored, [(2, 40.0, 60.0)], "{}", app.name);
        } else {
            assert_eq!(stored.len(), 2, "{stored:?}");
        }
        let hour = admin.get(&format!("{path}?range=hour")).await;
        assert_eq!(hour.status, 200, "{:?}", hour.body);
        assert_eq!(hour.body["step_ms"], 60_000);
        let points = hour.body["points"].as_array().unwrap();
        assert!(!points.is_empty() && points.len() <= 2, "{points:?}");
        assert_eq!(points.last().unwrap()["cpu_percent_max"], 60.0);
        assert_eq!(admin.get(&format!("{path}?range=year")).await.status, 400);

        // Deleting the device removes its history, and the minute still
        // waiting to be stored.
        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(&mut control, metrics_report(80.0, 8_000)).await;
        assert_eq!(receive_json(&mut events).await["type"], "metrics");
        drop(control);
        receive_json(&mut events).await;
        let deleted = admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert_eq!(deleted.status, 204, "{:?}", deleted.body);
        assert_eq!(app.state.metrics.flush().await.unwrap().minutes, 0);
        assert!(stored_minutes(&app, &agent.device_id).await.is_empty());
        assert_eq!(admin.get(&path).await.status, 404);
        app.finish().await;
    }
}

fn usage(cpu: f32, received: u64) -> meshrmm_protocol_types::SystemMetrics {
    meshrmm_protocol_types::SystemMetrics {
        cpu_percent: cpu,
        memory_used_bytes: 4_000,
        memory_total_bytes: 16_000,
        network_received_bytes_per_second: received,
        network_sent_bytes_per_second: 500,
        uptime_seconds: 3_600,
        volumes: Vec::new(),
    }
}

/// Queues a minute of one reading for the device, as when its Agent reports
/// once and goes offline.
fn report_once(app: &App, device_id: &str, cpu: f32) {
    app.state.metrics.record(device_id, usage(cpu, 2_000));
    app.state.metrics.disconnected(device_id);
}

#[tokio::test]
async fn finished_minutes_reach_the_history_in_few_statements() {
    for app in apps().await {
        let devices: Vec<String> = (0..2_500).map(|n| format!("device-{n}")).collect();
        for device_id in &devices {
            report_once(&app, device_id, 10.0);
        }
        // A reconnect within the minute queues it twice; the two merge, which
        // PostgreSQL needs of a statement that names a row once.
        report_once(&app, &devices[0], 30.0);

        let flushed = app.state.metrics.flush().await.unwrap();
        let first = stored_minutes(&app, &devices[0]).await;
        // Unless the minute turned between the two reports.
        let expected = if first.len() == 1 {
            assert_eq!(first, [(2, 20.0, 30.0)], "{}", app.name);
            2_500
        } else {
            2_501
        };
        assert_eq!(flushed.minutes, expected, "{}", app.name);
        assert_eq!(flushed.statements, 2, "2,000 rows a statement");
        assert_eq!(flushed.refused, 0);
        assert_eq!(
            stored_minutes(&app, &devices[2_499]).await,
            [(1, 10.0, 10.0)]
        );
        assert_eq!(app.state.metrics.flush().await.unwrap().statements, 0);
        app.finish().await;
    }
}

#[tokio::test]
async fn a_deleted_devices_waiting_minutes_are_dropped_and_the_rest_stored() {
    for app in apps().await {
        report_once(&app, "kept", 10.0);
        report_once(&app, "deleted", 20.0);
        app.state.metrics.forget("deleted").await.unwrap();
        let flushed = app.state.metrics.flush().await.unwrap();
        assert_eq!(flushed.minutes, 1, "{}", app.name);
        assert_eq!(stored_minutes(&app, "kept").await, [(1, 10.0, 10.0)]);
        assert!(stored_minutes(&app, "deleted").await.is_empty());
        app.finish().await;
    }
}

#[tokio::test]
async fn a_minute_the_database_refuses_loses_only_itself() {
    use sea_query::Query;
    for app in apps().await {
        // Merging into this stored minute overflows PostgreSQL's integers;
        // SQLite turns them into reals instead. Stored for this minute and
        // the next, in case it turns.
        let now = meshrmm_server::time::now_ms();
        let minute = now - now % 60_000;
        for start in [minute, minute + 60_000] {
            app.db()
                .execute(
                    &Query::insert()
                        .into_table("device_metrics")
                        .columns([
                            "device_id",
                            "minute",
                            "samples",
                            "cpu_percent",
                            "cpu_percent_max",
                            "memory_used_bytes",
                            "memory_total_bytes",
                            "network_received_bytes_per_second",
                            "network_sent_bytes_per_second",
                            "storage_used_bytes",
                            "storage_total_bytes",
                        ])
                        .values_panic([
                            "huge".into(),
                            start.into(),
                            1_i64.into(),
                            1.0_f64.into(),
                            1.0_f64.into(),
                            0_i64.into(),
                            0_i64.into(),
                            i64::MAX.into(),
                            0_i64.into(),
                            0_i64.into(),
                            0_i64.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }
        report_once(&app, "before", 10.0);
        app.state.metrics.record("huge", usage(10.0, u64::MAX));
        app.state.metrics.disconnected("huge");
        report_once(&app, "after", 30.0);

        let flushed = app.state.metrics.flush().await.unwrap();
        assert_eq!(
            stored_minutes(&app, "before").await,
            [(1, 10.0, 10.0)],
            "{}",
            app.name
        );
        assert_eq!(stored_minutes(&app, "after").await, [(1, 30.0, 30.0)]);
        if app.name == "postgres" {
            assert_eq!((flushed.minutes, flushed.refused), (2, 1));
        }
        // A refused minute isn't tried again.
        assert_eq!(app.state.metrics.flush().await.unwrap().statements, 0);
        app.finish().await;
    }
}
