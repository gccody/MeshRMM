//! The registry tool: keys and values browsed and changed over a Windows
//! Agent's control connection, who may do what, the audit log (which never
//! holds the data written), and devices without a registry.
mod common;

use std::time::Duration;

use common::{
    Agent, App, Browser, Server, Ws, apps_with, audit_events, receive_json, send_json, set_up,
    user_with,
};
use meshrmm_server::tools::{Timeouts, Tools};
use serde_json::{Value, json};

fn quick(state: &mut meshrmm_server::http::AppState) {
    state.tools = Tools::new(Timeouts {
        request: Duration::from_secs(5),
        slow_request: Duration::from_secs(8),
    });
}

/// Changes the device must answer at once.
fn impatient(state: &mut meshrmm_server::http::AppState) {
    state.tools = Tools::new(Timeouts {
        request: Duration::from_millis(300),
        slow_request: Duration::from_millis(300),
    });
}

async fn enrolled(app: &App, admin: &mut Browser, server: &Server, platform: &str) -> (Agent, Ws) {
    let agent = app.enroll_on(admin, "registry-01", platform).await;
    let control = agent.connect(server).await.unwrap();
    (agent, control)
}

/// The next tool request on `control`, answered with `reply`.
async fn answer(control: &mut Ws, reply: Value) -> Value {
    let command = receive_json(control).await;
    assert_eq!(command["type"], "tool_request", "{command}");
    let request = command["request"].clone();
    send_json(
        control,
        json!({ "type": "tool_reply", "request_id": request["request_id"], "reply": reply }),
    )
    .await;
    request
}

type Call = tokio::task::JoinHandle<common::Response>;

fn get(browser: &Browser, path: String) -> Call {
    let mut browser = browser.clone();
    tokio::spawn(async move { browser.get(&path).await })
}

fn post(browser: &Browser, path: String, body: Value) -> Call {
    let mut browser = browser.clone();
    tokio::spawn(async move { browser.post(&path, body).await })
}

const KEY: &str = r"SOFTWARE\Contoso";

fn changed() -> Value {
    json!({ "reply": "registry_changed" })
}

#[tokio::test]
async fn keys_and_values_go_through_the_agent() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "windows").await;
        let base = format!("/v1/agents/{}/registry", agent.device_id);

        // A listing is relayed as the device sends it, and isn't audited.
        let call = get(
            &admin,
            format!("{base}?root=HKLM&path=SOFTWARE%5CContoso&view=32"),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "registry_key", "key": { "root": "HKLM", "path": KEY, "view": "32" },
                    "subkeys": [{ "name": "Settings", "has_subkeys": true }],
                    "values": [{ "name": "", "type": "string", "data": "Contoso", "size": 16 },
                               { "name": "Big", "type": "qword", "data": "18446744073709551615", "size": 8 }] }),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "list_registry_key", "key": { "root": "HKLM", "path": KEY, "view": "32" } }),
            "{name}"
        );
        let listed = call.await.unwrap();
        assert_eq!(listed.status, 200, "{name}: {:?}", listed.body);
        assert_eq!(listed.body["subkeys"][0]["name"], "Settings");
        assert_eq!(listed.body["values"][1]["data"], "18446744073709551615");

        let call = get(
            &admin,
            format!("{base}/value?root=HKU&path=S-1-5-18&name=Path"),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "registry_value", "name": "Path", "type": "expand_string", "data": "%TEMP%", "size": 14 }),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "get_registry_value", "key": { "root": "HKU", "path": "S-1-5-18", "view": "64" }, "name": "Path" })
        );
        assert_eq!(call.await.unwrap().body["data"], "%TEMP%", "{name}");
        assert!(
            audit_events(&app, "registry.value.set").await.is_empty(),
            "{name}"
        );

        // Setting a value: the audit log has its type and size, never its
        // data.
        let call = post(
            &admin,
            format!("{base}/values"),
            json!({ "root": "HKLM", "path": KEY, "name": "Secret", "type": "string", "data": "hunter2", "create": true }),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "registry_value", "name": "Secret", "type": "string", "data": "hunter2", "size": 16 }),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "set_registry_value", "key": { "root": "HKLM", "path": KEY, "view": "64" },
                    "name": "Secret", "data": { "type": "string", "data": "hunter2" }, "create": true }),
            "{name}"
        );
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let recorded = audit_events(&app, "registry.value.set").await;
        assert_eq!(recorded.len(), 1, "{name}");
        assert_eq!(recorded[0].target_id, agent.device_id);
        let metadata = &recorded[0].metadata;
        assert_eq!(metadata["key"], r"HKLM\SOFTWARE\Contoso", "{name}");
        assert_eq!(metadata["value"], "Secret");
        assert_eq!(metadata["type"], "REG_SZ");
        assert_eq!(metadata["size"], 16);
        assert_eq!(metadata["created"], true);
        assert_eq!(metadata["result"], "completed");
        assert!(
            !metadata.to_string().contains("hunter2"),
            "{name}: {metadata}"
        );

        // Keys: created, and deleted with their subkeys.
        let call = post(
            &admin,
            format!("{base}/keys"),
            json!({ "root": "HKLM", "path": format!(r"{KEY}\New"), "view": "32" }),
        );
        let request = answer(&mut control, changed()).await;
        assert_eq!(request["operation"]["op"], "create_registry_key", "{name}");
        assert_eq!(call.await.unwrap().status, 200);
        let recorded = audit_events(&app, "registry.key.create").await;
        assert_eq!(
            recorded[0].metadata["key"], r"HKLM\SOFTWARE\Contoso\New",
            "{name}"
        );
        assert_eq!(recorded[0].metadata["view"], "32");

        let call = post(
            &admin,
            format!("{base}/keys/delete"),
            json!({ "root": "HKLM", "path": KEY, "recursive": true }),
        );
        let request = answer(&mut control, changed()).await;
        assert_eq!(
            request["timeout_ms"], 8_000,
            "{name}: deleting a tree is slow"
        );
        assert_eq!(request["operation"]["recursive"], true);
        assert_eq!(call.await.unwrap().status, 200);
        let recorded = audit_events(&app, "registry.key.delete").await;
        assert_eq!(recorded[0].metadata["recursive"], true, "{name}");

        // Values: renamed and deleted.
        let call = post(
            &admin,
            format!("{base}/values/rename"),
            json!({ "root": "HKLM", "path": KEY, "from": "Old", "to": "New" }),
        );
        answer(&mut control, changed()).await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let recorded = audit_events(&app, "registry.value.rename").await;
        assert_eq!(recorded[0].metadata["value"], "Old", "{name}");
        assert_eq!(recorded[0].metadata["renamed_to"], "New");

        // The device's refusal keeps its code, and is recorded as failed.
        let call = post(
            &admin,
            format!("{base}/values/delete"),
            json!({ "root": "HKLM", "path": KEY, "name": "Gone" }),
        );
        answer(
            &mut control,
            json!({ "reply": "failed", "error": { "code": "not_found", "message": "It doesn't exist." } }),
        )
        .await;
        let refused = call.await.unwrap();
        assert_eq!(refused.status, 404, "{name}");
        assert_eq!(refused.code(), "not_found");
        let recorded = audit_events(&app, "registry.value.delete").await;
        assert_eq!(recorded[0].metadata["result"], "failed", "{name}");
        assert_eq!(recorded[0].metadata["error"], "not_found");

        let call = post(
            &admin,
            format!("{base}/keys/delete"),
            json!({ "root": "HKLM", "path": KEY }),
        );
        answer(
            &mut control,
            json!({ "reply": "failed", "error": { "code": "not_empty", "message": "It has subkeys." } }),
        )
        .await;
        let refused = call.await.unwrap();
        assert_eq!(refused.status, 409, "{name}");
        assert_eq!(refused.code(), "not_empty");
        app.finish().await;
    }
}

#[tokio::test]
async fn bad_requests_and_other_platforms_never_reach_the_device() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "windows").await;
        let base = format!("/v1/agents/{}/registry", agent.device_id);

        for (path, body) in [
            ("keys", json!({ "root": "HKLM", "path": "" })),
            (
                "keys",
                json!({ "root": "HKLM", "path": r"SOFTWARE\\Contoso" }),
            ),
            ("keys", json!({ "root": "HKLM", "path": r"SOFTWARE\" })),
            ("keys", json!({ "root": "HKLM", "path": "x".repeat(256) })),
            ("keys/delete", json!({ "root": "HKLM", "path": "" })),
            (
                "values",
                json!({ "root": "HKLM", "path": KEY, "name": "a", "type": "binary", "data": "abc" }),
            ),
            (
                "values",
                json!({ "root": "HKLM", "path": KEY, "name": "a", "type": "multi_string", "data": ["a", ""] }),
            ),
            (
                "values",
                json!({ "root": "HKLM", "path": KEY, "name": "a", "type": "other", "data": { "kind": 0, "hex": "" } }),
            ),
            (
                "values",
                json!({ "root": "HKLM", "path": KEY, "name": "a", "type": "binary", "data": "00".repeat(256 * 1024 + 1) }),
            ),
            (
                "values",
                json!({ "root": "HKLM", "path": KEY, "name": "a\u{0}", "type": "dword", "data": 1 }),
            ),
            (
                "values/rename",
                json!({ "root": "HKLM", "path": KEY, "from": "a", "to": "a" }),
            ),
        ] {
            let refused = admin.post(&format!("{base}/{path}"), body.clone()).await;
            assert_eq!(refused.status, 400, "{name}: {path} {body}");
        }
        let refused = admin.get(&format!("{base}?root=HKCU")).await;
        assert_eq!(
            refused.status, 400,
            "{name}: HKCU is each user's, under HKU"
        );
        let refused = admin.get(&format!("{base}?root=HKLM&view=16")).await;
        assert_eq!(refused.status, 400, "{name}");
        common::quiet(&mut control, Duration::from_millis(200)).await;

        // A Linux device has no registry.
        let linux = app.enroll_on(&mut admin, "linux-01", "linux").await;
        let mut linux_control = linux.connect(&server).await.unwrap();
        let refused = admin
            .get(&format!(
                "/v1/agents/{}/registry?root=HKLM",
                linux.device_id
            ))
            .await;
        assert_eq!(refused.status, 400, "{name}");
        assert_eq!(refused.code(), "unsupported");
        common::quiet(&mut linux_control, Duration::from_millis(200)).await;
        assert!(
            audit_events(&app, "registry.key.create").await.is_empty(),
            "{name}: nothing asked, nothing recorded"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn permissions_decide_who_may_browse_and_change() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "windows").await;
        let base = format!("/v1/agents/{}/registry", agent.device_id);
        let create = json!({ "root": "HKLM", "path": format!(r"{KEY}\New") });

        let (mut viewer, _) = user_with(
            &app,
            &mut admin,
            "viewer@example.com",
            &["devices.view", "tools.services.view"],
        )
        .await;
        for refused in [
            viewer.get(&format!("{base}?root=HKLM")).await,
            viewer.get(&format!("{base}/value?root=HKLM")).await,
            viewer.post(&format!("{base}/keys"), create.clone()).await,
        ] {
            assert_eq!(refused.status, 403, "{name}");
            assert_eq!(refused.code(), "permission_denied");
        }

        let (mut browser, _) = user_with(
            &app,
            &mut admin,
            "browser@example.com",
            &["tools.registry.view"],
        )
        .await;
        let call = get(&browser, format!("{base}?root=HKCC"));
        answer(
            &mut control,
            json!({ "reply": "registry_key", "key": { "root": "HKCC", "path": "", "view": "64" }, "subkeys": [], "values": [] }),
        )
        .await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        for (path, body) in [
            ("keys", create.clone()),
            ("keys/delete", create.clone()),
            (
                "values",
                json!({ "root": "HKLM", "path": KEY, "type": "dword", "data": 1 }),
            ),
            (
                "values/rename",
                json!({ "root": "HKLM", "path": KEY, "from": "a", "to": "b" }),
            ),
            (
                "values/delete",
                json!({ "root": "HKLM", "path": KEY, "name": "a" }),
            ),
        ] {
            let refused = browser.post(&format!("{base}/{path}"), body).await;
            assert_eq!(refused.status, 403, "{name}: {path}");
        }

        let (editor, _) = user_with(
            &app,
            &mut admin,
            "editor@example.com",
            &["tools.registry.view", "tools.registry.manage"],
        )
        .await;
        let call = post(&editor, format!("{base}/keys"), create);
        answer(&mut control, changed()).await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let recorded = audit_events(&app, "registry.key.create").await;
        assert_eq!(recorded[0].actor_label, "editor@example.com", "{name}");

        let missing = uuid::Uuid::new_v4();
        assert_eq!(
            admin
                .get(&format!("/v1/agents/{missing}/registry?root=HKLM"))
                .await
                .status,
            404
        );
        common::quiet(&mut control, Duration::from_millis(200)).await;
        app.finish().await;
    }
}

#[tokio::test]
async fn changes_the_device_never_confirms_are_recorded_as_unconfirmed() {
    for app in apps_with(impatient).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "windows").await;
        let base = format!("/v1/agents/{}/registry", agent.device_id);

        let call = post(
            &admin,
            format!("{base}/values/delete"),
            json!({ "root": "HKLM", "path": KEY, "name": "Slow" }),
        );
        let command = receive_json(&mut control).await;
        assert_eq!(command["type"], "tool_request", "{name}");
        let timed_out = call.await.unwrap();
        assert_eq!(timed_out.status, 504, "{name}");
        assert_eq!(timed_out.code(), "device_timeout");
        let recorded = audit_events(&app, "registry.value.delete").await;
        assert_eq!(recorded[0].metadata["result"], "unconfirmed", "{name}");
        assert_eq!(recorded[0].metadata["error"], "device_timeout");
        assert_eq!(recorded[0].metadata["value"], "Slow");

        // Offline: never asked, so nothing is recorded.
        drop(control);
        let deadline = tokio::time::Instant::now() + common::SOCKET_WAIT;
        while app.state.agents.is_connected(&agent.device_id) {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let offline = admin
            .post(
                &format!("{base}/keys"),
                json!({ "root": "HKLM", "path": format!(r"{KEY}\New") }),
            )
            .await;
        assert_eq!(offline.code(), "device_offline", "{name}");
        assert!(audit_events(&app, "registry.key.create").await.is_empty());
        app.finish().await;
    }
}
