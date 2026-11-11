//! The remote tools: processes and services listed and changed over the
//! Agent's control connection, who may do what, the audit log, and devices
//! that are offline, busy or slow to answer.
mod common;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{
    Agent, App, Browser, Server, Ws, apps_with, audit_events, receive_json, send_json, set_up,
    user_with,
};
use meshrmm_server::tools::{MAX_PENDING_PER_DEVICE, Timeouts, Tools};
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
        request: Duration::from_millis(1500),
        slow_request: Duration::from_millis(300),
    });
}

async fn enrolled(app: &App, admin: &mut Browser, server: &Server, platform: &str) -> (Agent, Ws) {
    let agent = app.enroll_on(admin, "tools-01", platform).await;
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

fn process(pid: u32, name: &str) -> Value {
    json!({
        "pid": pid, "parent_pid": 1, "name": name, "user": "root", "cpu_percent": 1.5,
        "memory_bytes": 4096, "started_at": 1_700_000_000_000_i64, "command_line": format!("/usr/bin/{name} --flag"),
    })
}

fn processes(list: Vec<Value>) -> Value {
    json!({ "reply": "processes", "processes": list, "read_at": 1, "logical_cpus": 4, "memory_total_bytes": 8_589_934_592_u64 })
}

fn service(name: &str, state: &str) -> Value {
    json!({
        "name": name, "display_name": name, "state": state, "state_detail": "active (running)",
        "startup": "automatic", "startup_detail": "enabled", "startup_choices": ["automatic", "manual", "disabled"],
        "account": "root", "description": "Regular background program processing daemon", "pid": 812,
    })
}

/// The Agent's answer to a change of the service `name`: it, afterwards.
fn changed(name: &str, state: &str) -> Value {
    let mut reply = service(name, state);
    reply["reply"] = "service".into();
    reply
}

/// Every audit event, of any action.
async fn all_events(app: &App) -> usize {
    meshrmm_server::audit::list(&mut app.db(), &Default::default(), 1000)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn processes_and_services_go_through_the_agent() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "linux").await;
        let base = format!("/v1/agents/{}", agent.device_id);
        let before = all_events(&app).await;

        // Lists are relayed as the device sends them, and aren't audited.
        let call = get(&admin, format!("{base}/processes"));
        let request = answer(
            &mut control,
            processes(vec![process(1, "systemd"), process(812, "cron")]),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "list_processes" }),
            "{name}"
        );
        // How long the server waits, for the Agent to keep to.
        assert_eq!(request["timeout_ms"], 5_000, "{name}");
        let listed = call.await.unwrap();
        assert_eq!(listed.status, 200, "{name}: {:?}", listed.body);
        assert_eq!(listed.body["processes"][1]["name"], "cron");
        assert_eq!(
            listed.body["processes"][1]["command_line"],
            "/usr/bin/cron --flag"
        );
        assert_eq!(listed.body["logical_cpus"], 4);

        let call = get(&admin, format!("{base}/services"));
        let request = answer(
            &mut control,
            json!({ "reply": "services", "services": [service("cron.service", "running")], "read_at": 1 }),
        )
        .await;
        assert_eq!(request["operation"], json!({ "op": "list_services" }));
        let listed = call.await.unwrap();
        assert_eq!(listed.status, 200, "{name}: {:?}", listed.body);
        assert_eq!(listed.body["services"][0]["account"], "root");
        assert_eq!(
            all_events(&app).await,
            before,
            "{name}: views aren't audited"
        );

        // Ending a tree: the device is asked for exactly what was listed,
        // and the audit log says what ended and what didn't.
        let call = post(
            &admin,
            format!("{base}/processes/end"),
            json!({ "processes": [{ "pid": 900, "started_at": 5 }, { "pid": 901, "started_at": 6 }, { "pid": 900 }], "tree": true }),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "processes_ended", "ended": [{ "pid": 901, "name": "worker" }],
                    "failed": [{ "pid": 900, "name": "boss", "error": { "code": "access_denied", "message": "No." } }] }),
        )
        .await;
        assert_eq!(request["timeout_ms"], 8_000, "{name}: ending is slow");
        assert_eq!(
            request["operation"],
            json!({ "op": "end_processes", "processes": [{ "pid": 900, "started_at": 5 }, { "pid": 901, "started_at": 6 }] }),
            "{name}: duplicates are dropped"
        );
        let ended = call.await.unwrap();
        assert_eq!(ended.status, 200, "{name}: {:?}", ended.body);
        assert_eq!(ended.body["failed"][0]["error"]["code"], "access_denied");
        let recorded = audit_events(&app, "process.end").await;
        assert_eq!(recorded.len(), 1, "{name}");
        assert_eq!(recorded[0].target_type, "agent");
        assert_eq!(recorded[0].target_id, agent.device_id);
        assert_eq!(
            recorded[0].metadata["processes"],
            json!([{ "pid": 901, "process": "worker" }])
        );
        assert_eq!(
            recorded[0].metadata["failed"],
            json!([{ "pid": 900, "process": "boss", "code": "access_denied" }])
        );
        assert_eq!(recorded[0].metadata["tree"], true);
        assert_eq!(recorded[0].metadata["result"], "partial");

        // A service action returns the service as it is afterwards.
        let call = post(
            &admin,
            format!("{base}/services/control"),
            json!({ "name": "cron.service", "action": "restart" }),
        );
        let request = answer(&mut control, json!({ "reply": "service", "name": "cron.service", "display_name": "cron.service", "state": "running", "pid": 813 })).await;
        assert_eq!(
            request["operation"],
            json!({ "op": "control_service", "name": "cron.service", "action": "restart" })
        );
        let restarted = call.await.unwrap();
        assert_eq!(restarted.status, 200, "{name}: {:?}", restarted.body);
        assert_eq!(restarted.body["pid"], 813);
        let recorded = audit_events(&app, "service.restart").await;
        assert_eq!(recorded[0].metadata["service"], "cron.service", "{name}");
        assert_eq!(recorded[0].metadata["state"], "running");
        assert_eq!(recorded[0].metadata["result"], "completed");

        let call = post(
            &admin,
            format!("{base}/services/startup"),
            json!({ "name": "cron.service", "startup": "disabled" }),
        );
        let request = answer(&mut control, json!({ "reply": "service", "name": "cron.service", "display_name": "cron.service", "state": "stopped", "startup": "disabled" })).await;
        assert_eq!(
            request["operation"],
            json!({ "op": "set_service_startup", "name": "cron.service", "startup": "disabled" })
        );
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let recorded = audit_events(&app, "service.set_startup").await;
        assert_eq!(recorded[0].metadata["startup"], "disabled", "{name}");
        assert_eq!(recorded[0].metadata["startup_now"], "disabled");

        // The device's refusal keeps its code, and is recorded as failed.
        let call = post(
            &admin,
            format!("{base}/services/control"),
            json!({ "name": "meshrmm-agent.service", "action": "stop" }),
        );
        answer(
            &mut control,
            json!({ "reply": "failed", "error": { "code": "protected", "message": "That is the MeshRMM Agent's own service." } }),
        )
        .await;
        let refused = call.await.unwrap();
        assert_eq!(refused.status, 403, "{name}");
        assert_eq!(refused.code(), "protected");
        let recorded = audit_events(&app, "service.stop").await;
        assert_eq!(recorded[0].metadata["result"], "failed", "{name}");
        assert_eq!(recorded[0].metadata["error"], "protected");
        app.finish().await;
    }
}

#[tokio::test]
async fn permissions_decide_who_may_see_and_change() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "linux").await;
        let base = format!("/v1/agents/{}", agent.device_id);
        let end = json!({ "processes": [{ "pid": 900 }] });
        let control_body = json!({ "name": "cron.service", "action": "stop" });
        let startup = json!({ "name": "cron.service", "startup": "manual" });

        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        for refused in [
            viewer.get(&format!("{base}/processes")).await,
            viewer.get(&format!("{base}/services")).await,
            viewer
                .post(&format!("{base}/processes/end"), end.clone())
                .await,
            viewer
                .post(&format!("{base}/services/control"), control_body.clone())
                .await,
            viewer
                .post(&format!("{base}/services/startup"), startup.clone())
                .await,
        ] {
            assert_eq!(refused.status, 403, "{name}");
            assert_eq!(refused.code(), "permission_denied");
        }

        // Seeing processes doesn't let a user end them, or see services.
        let (mut watcher, _) = user_with(
            &app,
            &mut admin,
            "watcher@example.com",
            &["tools.processes.view"],
        )
        .await;
        assert_eq!(
            watcher
                .post(&format!("{base}/processes/end"), end.clone())
                .await
                .status,
            403,
            "{name}"
        );
        assert_eq!(watcher.get(&format!("{base}/services")).await.status, 403);
        let call = get(&watcher, format!("{base}/processes"));
        answer(&mut control, processes(vec![])).await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");

        let (mut operator, _) = user_with(
            &app,
            &mut admin,
            "operator@example.com",
            &["tools.services.view", "tools.services.manage"],
        )
        .await;
        assert_eq!(
            operator.get(&format!("{base}/processes")).await.status,
            403,
            "{name}"
        );
        let call = post(
            &operator,
            format!("{base}/services/control"),
            control_body.clone(),
        );
        answer(&mut control, changed("cron.service", "stopped")).await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let recorded = audit_events(&app, "service.stop").await;
        assert_eq!(recorded[0].actor_label, "operator@example.com", "{name}");

        // A device that isn't enrolled is not found, whoever asks.
        let missing = uuid::Uuid::new_v4();
        assert_eq!(
            admin
                .get(&format!("/v1/agents/{missing}/processes"))
                .await
                .status,
            404
        );
        assert_eq!(admin.get("/v1/agents/not-an-id/services").await.status, 400);

        // An API token works within its own permissions and its user's.
        let created = admin
            .post("/v1/api-tokens", json!({ "name": "Inventory", "expires_in_days": 30, "permissions": ["tools.services.view"] }))
            .await;
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        let with_token = |method: &str, path: String, body: Option<Value>| {
            let mut browser = app.browser();
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {secret}"))
                .header("content-type", "application/json")
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .unwrap();
            tokio::spawn(async move { browser.raw(request).await })
        };
        let call = with_token("GET", format!("{base}/services"), None);
        answer(
            &mut control,
            json!({ "reply": "services", "services": [], "read_at": 1 }),
        )
        .await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let refused = with_token(
            "POST",
            format!("{base}/services/control"),
            Some(control_body),
        )
        .await
        .unwrap();
        assert_eq!(
            refused.code(),
            "permission_denied",
            "{name}: the token can't change services"
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
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "linux").await;
        let base = format!("/v1/agents/{}", agent.device_id);

        // Asked, and never answered in time.
        let call = post(
            &admin,
            format!("{base}/services/control"),
            json!({ "name": "cron.service", "action": "restart" }),
        );
        let command = receive_json(&mut control).await;
        assert_eq!(command["request"]["timeout_ms"], 300, "{name}");
        let late = call.await.unwrap();
        assert_eq!(
            late.status,
            StatusCode::GATEWAY_TIMEOUT,
            "{name}: {:?}",
            late.body
        );
        assert_eq!(late.code(), "device_timeout");
        let recorded = audit_events(&app, "service.restart").await;
        assert_eq!(recorded[0].metadata["result"], "unconfirmed", "{name}");
        assert_eq!(recorded[0].metadata["error"], "device_timeout");
        // Its answer, when it comes, goes nowhere.
        send_json(
            &mut control,
            json!({ "type": "tool_reply", "request_id": command["request"]["request_id"], "reply": changed("cron.service", "running") }),
        )
        .await;

        // Asked, and the device went away while it worked.
        let call = post(
            &admin,
            format!("{base}/processes/end"),
            json!({ "processes": [{ "pid": 900, "started_at": 5 }] }),
        );
        assert_eq!(receive_json(&mut control).await["type"], "tool_request");
        drop(control);
        let gone = call.await.unwrap();
        assert_eq!(gone.status, 409, "{name}");
        assert_eq!(gone.code(), "device_disconnected");
        let recorded = audit_events(&app, "process.end").await;
        assert_eq!(recorded[0].metadata["result"], "unconfirmed", "{name}");
        assert_eq!(recorded[0].metadata["pids"], json!([900]));
        assert_eq!(app.state.tools.pending_requests(), 0);

        // An offline device is refused, and nothing is recorded: it never
        // heard of it.
        let deadline = tokio::time::Instant::now() + common::SOCKET_WAIT;
        while app.state.agents.is_connected(&agent.device_id) {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let offline = admin
            .post(
                &format!("{base}/services/control"),
                json!({ "name": "cron.service", "action": "stop" }),
            )
            .await;
        assert_eq!(offline.status, 409, "{name}");
        assert_eq!(offline.code(), "device_offline");
        assert!(
            audit_events(&app, "service.stop").await.is_empty(),
            "{name}"
        );
        assert_eq!(
            admin.get(&format!("{base}/processes")).await.code(),
            "device_offline"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_busy_device_is_refused_without_asking_it() {
    for app in apps_with(quick).await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "tools-01", "linux").await;
        let base = format!("/v1/agents/{}", agent.device_id);

        // Its connection's backlog is full.
        let mut connection = app.state.agents.connect(&agent.device_id);
        let queued = common::fill_backlog(&app, &agent.device_id);
        common::assert_device_busy(&admin.get(&format!("{base}/processes")).await, name);
        let busy = admin
            .post(
                &format!("{base}/services/control"),
                json!({ "name": "cron.service", "action": "stop" }),
            )
            .await;
        common::assert_device_busy(&busy, name);
        assert!(
            audit_events(&app, "service.stop").await.is_empty(),
            "{name}: never sent"
        );
        common::drain_backlog(&mut connection, queued).await;

        // As many requests wait on it as may.
        let waiting = (0..MAX_PENDING_PER_DEVICE)
            .map(|_| get(&admin, format!("{base}/services")))
            .collect::<Vec<_>>();
        for _ in 0..MAX_PENDING_PER_DEVICE {
            let command =
                tokio::time::timeout(common::SOCKET_WAIT, common::next_command(&mut connection))
                    .await
                    .unwrap();
            assert!(
                matches!(
                    command,
                    Some(meshrmm_protocol_types::AgentCommand::ToolRequest { .. })
                ),
                "{name}: {command:?}"
            );
        }
        common::assert_device_busy(&admin.get(&format!("{base}/processes")).await, name);
        for call in waiting {
            call.abort();
        }
        let deadline = tokio::time::Instant::now() + common::SOCKET_WAIT;
        while app.state.tools.pending_requests() > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{name}: abandoned requests are forgotten"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(connection);
        app.finish().await;
    }
}

#[tokio::test]
async fn the_agents_own_busy_answer_and_bad_requests() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "linux").await;
        let base = format!("/v1/agents/{}", agent.device_id);

        // The Agent has as many requests stuck as it leaves behind.
        let call = get(&admin, format!("{base}/services"));
        answer(
            &mut control,
            json!({ "reply": "failed", "error": { "code": "busy", "message": "8 earlier tool requests never finished." } }),
        )
        .await;
        let busy = call.await.unwrap();
        assert_eq!(busy.status, StatusCode::TOO_MANY_REQUESTS, "{name}");
        assert_eq!(busy.code(), "busy");

        // Requests outside the limits never reach the device.
        let too_many = (0..=meshrmm_protocol_types::MAX_ENDED_PROCESSES as u32)
            .map(|pid| json!({ "pid": pid + 10 }))
            .collect::<Vec<_>>();
        for body in [json!({ "processes": [] }), json!({ "processes": too_many })] {
            assert_eq!(
                admin
                    .post(&format!("{base}/processes/end"), body)
                    .await
                    .status,
                400,
                "{name}"
            );
        }
        for body in [
            json!({ "name": "cron; reboot", "action": "stop" }),
            json!({ "name": "-x", "action": "start" }),
            json!({ "name": "cron.service", "action": "explode" }),
        ] {
            assert_eq!(
                admin
                    .post(&format!("{base}/services/control"), body)
                    .await
                    .status,
                400,
                "{name}"
            );
        }
        for startup in ["automatic_delayed", "static"] {
            let refused = admin
                .post(
                    &format!("{base}/services/startup"),
                    json!({ "name": "cron.service", "startup": startup }),
                )
                .await;
            assert_eq!(
                refused.status, 400,
                "{name}: {startup} isn't for a Linux unit"
            );
        }
        common::quiet(&mut control, Duration::from_millis(200)).await;
        assert!(audit_events(&app, "process.end").await.is_empty());

        // The longest list an Agent sends fits its control connection.
        let long = "x".repeat(meshrmm_protocol_types::MAX_COMMAND_LINE_BYTES);
        let mut list = (0..meshrmm_protocol_types::MAX_LISTED_PROCESSES as u32)
            .map(|pid| {
                let mut entry = process(pid + 2, "worker");
                entry["command_line"] = long.clone().into();
                entry
            })
            .collect::<Vec<_>>();
        // Bounded as the Agent bounds it.
        let mut bounded: meshrmm_protocol_types::ProcessList = serde_json::from_value(
            json!({ "processes": list.split_off(0), "read_at": 1, "logical_cpus": 64 }),
        )
        .unwrap();
        bounded.bound();
        let call = get(&admin, format!("{base}/processes"));
        let mut reply = serde_json::to_value(&bounded).unwrap();
        reply["reply"] = "processes".into();
        answer(&mut control, reply).await;
        let listed = call.await.unwrap();
        assert_eq!(listed.status, 200, "{name}");
        assert_eq!(
            listed.body["processes"].as_array().unwrap().len(),
            meshrmm_protocol_types::MAX_LISTED_PROCESSES
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_windows_service_takes_a_delayed_start() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "windows").await;
        let base = format!("/v1/agents/{}", agent.device_id);
        let call = post(
            &admin,
            format!("{base}/services/startup"),
            json!({ "name": "MSSQL$SQLEXPRESS", "startup": "automatic_delayed" }),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "service", "name": "MSSQL$SQLEXPRESS", "display_name": "SQL Server (SQLEXPRESS)", "state": "running", "startup": "automatic_delayed" }),
        )
        .await;
        assert_eq!(
            request["operation"]["startup"], "automatic_delayed",
            "{name}"
        );
        assert_eq!(call.await.unwrap().status, 200);
        let recorded = audit_events(&app, "service.set_startup").await;
        assert_eq!(
            recorded[0].metadata["display_name"], "SQL Server (SQLEXPRESS)",
            "{name}"
        );
        // A Windows service name has no slashes.
        assert_eq!(
            admin
                .post(
                    &format!("{base}/services/control"),
                    json!({ "name": "a\\b", "action": "start" })
                )
                .await
                .status,
            400
        );
        app.finish().await;
    }
}
