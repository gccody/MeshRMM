//! Agents' control connections and the website's presence socket, over real
//! WebSockets.
mod common;

use std::time::Duration;

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
            Duration::from_millis(500),
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
        quiet(&mut events, Duration::from_millis(700)).await;

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

async fn assert_closes_for(
    app: &App,
    server: &common::Server,
    admin: &mut common::Browser,
    change: impl AsyncFnOnce(&mut common::Browser, &str),
) {
    let email = format!("{}@example.com", common::random_hex(4));
    let (mut user, user_id) = user_with(app, admin, &email, &["devices.view"]).await;
    let mut events = user.events(server).await.unwrap();
    receive_json(&mut events).await;
    change(admin, &user_id).await;
    assert_eq!(receive_close(&mut events).await, 4001, "{}", app.name);
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

        // Going offline stores the unfinished minute and ends the live view.
        drop(control);
        receive_json(&mut events).await;
        let mut stored = Vec::new();
        for _ in 0..50 {
            stored = stored_minutes(&app, &agent.device_id).await;
            if !stored.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(stored, [(1, 20.0, 20.0)], "{}", app.name);
        assert!(admin.get(&path).await.body["latest"].is_null());

        // A second connection's readings in the same minute merge into it.
        let mut control = agent.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(&mut control, metrics_report(60.0, 8_000)).await;
        assert_eq!(receive_json(&mut events).await["type"], "metrics");
        drop(control);
        receive_json(&mut events).await;
        for _ in 0..50 {
            stored = stored_minutes(&app, &agent.device_id).await;
            if stored.iter().map(|minute| minute.0).sum::<i64>() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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

        // Deleting the device removes its history.
        let deleted = admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert_eq!(deleted.status, 204, "{:?}", deleted.body);
        assert!(stored_minutes(&app, &agent.device_id).await.is_empty());
        assert_eq!(admin.get(&path).await.status, 404);
        app.finish().await;
    }
}
