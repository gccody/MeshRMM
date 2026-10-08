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
