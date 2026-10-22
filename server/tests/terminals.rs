//! Remote terminals: who may open one, the relay between the browser's
//! socket and the Agent's, flow control checks, how terminals end, limits,
//! and the audit log.
mod common;

use std::time::Duration;

use common::{
    Agent, App, Browser, ORIGIN, SOCKET_WAIT, Server, Ws, apps, apps_with, audit_events,
    receive_json, set_up, user_with, ws,
};
use futures_util::{SinkExt, StreamExt};
use meshrmm_server::terminals::{MAX_TERMINALS_PER_DEVICE, Terminals, Timeouts};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

fn quick(state: &mut meshrmm_server::http::AppState) {
    state.terminals = Terminals::new(Timeouts {
        agent_connect: Duration::from_millis(500),
        idle: Some(Duration::from_millis(1500)),
        ..Timeouts::default()
    });
}

fn request(shell: &str, run_as: &str) -> Value {
    json!({ "shell": shell, "run_as": run_as, "columns": 100, "rows": 30 })
}

async fn open(browser: &mut Browser, agent: &Agent, body: Value) -> common::Response {
    browser
        .post(&format!("/v1/agents/{}/terminals", agent.device_id), body)
        .await
}

/// The browser's socket for terminal `id`.
async fn browser_socket(server: &Server, browser: &Browser, id: &str) -> Result<Ws, u16> {
    let cookie = browser.cookie.clone().unwrap();
    ws(
        &server.ws_url(&format!("/v1/terminals/{id}")),
        &[("origin", ORIGIN), ("cookie", &cookie)],
    )
    .await
}

/// The Agent's socket for terminal `id`.
async fn agent_socket(server: &Server, agent: &Agent, id: &str) -> Result<Ws, u16> {
    ws(
        &server.ws_url(&format!("/v1/agents/{}/terminals/{id}", agent.device_id)),
        &[("authorization", &format!("Bearer {}", agent.token))],
    )
    .await
}

/// The next message that isn't a ping or pong.
async fn next(socket: &mut Ws) -> Message {
    loop {
        let frame = tokio::time::timeout(SOCKET_WAIT, socket.next())
            .await
            .expect("the socket said nothing");
        match frame {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(message)) => return message,
            Some(Err(_)) | None => return Message::Close(None),
        }
    }
}

async fn next_json(socket: &mut Ws) -> Value {
    match next(socket).await {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected JSON, got {other:?}"),
    }
}

/// The close code and reason a socket ends with, skipping messages before.
async fn closed(socket: &mut Ws) -> (u16, String) {
    loop {
        match next(socket).await {
            Message::Close(Some(frame)) => {
                return (u16::from(frame.code), frame.reason.to_string());
            }
            Message::Close(None) => return (1005, String::new()),
            _ => {}
        }
    }
}

async fn send(socket: &mut Ws, message: Message) {
    socket.send(message).await.unwrap();
}

/// An open terminal: the browser's and the Agent's sockets, and what the
/// Agent was asked for.
struct Terminal {
    id: String,
    browser: Ws,
    agent: Ws,
    command: Value,
}

/// Opens a terminal as `browser` on `agent`, whose control connection is
/// `control`, and has the Agent connect its side and say the shell started.
async fn start(
    server: &Server,
    browser: &mut Browser,
    agent: &Agent,
    control: &mut Ws,
    body: Value,
) -> Terminal {
    let opened = open(browser, agent, body).await;
    assert_eq!(opened.status, 201, "{:?}", opened.body);
    let id = opened.body["id"].as_str().unwrap().to_owned();
    let mut browser_ws = browser_socket(server, browser, &id).await.unwrap();
    let command = receive_json(control).await;
    assert_eq!(command["type"], "open_terminal", "{command}");
    assert_eq!(command["terminal"]["terminal_id"], id.as_str());
    let mut agent_ws = agent_socket(server, agent, &id).await.unwrap();
    send(
        &mut agent_ws,
        Message::Text(
            json!({ "type": "started", "account": "root", "shell": "/bin/bash" })
                .to_string()
                .into(),
        ),
    )
    .await;
    assert_eq!(
        next_json(&mut browser_ws).await,
        json!({ "type": "started", "account": "root", "shell": "/bin/bash" })
    );
    Terminal {
        id,
        browser: browser_ws,
        agent: agent_ws,
        command: command["terminal"].clone(),
    }
}

async fn enrolled(app: &App, admin: &mut Browser, server: &Server, platform: &str) -> (Agent, Ws) {
    let agent = app.enroll_on(admin, "web-01", platform).await;
    let control = agent.connect(server).await.unwrap();
    (agent, control)
}

#[tokio::test]
async fn terminals_relay_both_ways_and_record_their_life() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "linux").await;
        let mut terminal = start(
            &server,
            &mut admin,
            &agent,
            &mut control,
            request("bash", "system"),
        )
        .await;
        assert_eq!(
            terminal.command,
            json!({
                "terminal_id": terminal.id,
                "shell": "bash",
                "run_as": "system",
                "columns": 100,
                "rows": 30
            }),
            "{name}: no notice for root by default"
        );

        // Input reaches the Agent as it was typed, bytes included.
        let input = vec![b'l', b's', 0xff, 0x00, b'\r'];
        send(&mut terminal.browser, Message::Binary(input.clone().into())).await;
        assert_eq!(
            next(&mut terminal.agent).await,
            Message::Binary(input.into())
        );
        // Output reaches the browser, and the browser acknowledges it.
        let output = "héllo\r\n".as_bytes().to_vec();
        send(&mut terminal.agent, Message::Binary(output.clone().into())).await;
        assert_eq!(
            next(&mut terminal.browser).await,
            Message::Binary(output.clone().into())
        );
        let ack = json!({ "type": "ack", "bytes": output.len() });
        send(&mut terminal.browser, Message::Text(ack.to_string().into())).await;
        assert_eq!(next_json(&mut terminal.agent).await, ack);
        let resize = json!({ "type": "resize", "columns": 120, "rows": 40 });
        send(
            &mut terminal.browser,
            Message::Text(resize.to_string().into()),
        )
        .await;
        assert_eq!(next_json(&mut terminal.agent).await, resize);

        // The shell exits: the browser hears how, and the socket closes.
        send(
            &mut terminal.agent,
            Message::Text(json!({ "type": "exited", "code": 3 }).to_string().into()),
        )
        .await;
        assert_eq!(
            next_json(&mut terminal.browser).await,
            json!({ "type": "exited", "code": 3 })
        );
        assert_eq!(closed(&mut terminal.browser).await.0, 1000, "{name}");

        let opened = audit_events(&app, "terminal.open").await;
        assert_eq!(opened.len(), 1, "{name}");
        assert_eq!(opened[0].target_type, "agent");
        assert_eq!(opened[0].target_id, agent.device_id);
        assert_eq!(opened[0].metadata["shell"], "bash");
        assert_eq!(opened[0].metadata["run_as"], "system");
        let closes = wait_for_audit(&app, "terminal.close", 1).await;
        let close = &closes[0].metadata;
        assert_eq!(close["terminal_id"], terminal.id.as_str());
        assert_eq!(close["reason"], "exited");
        assert_eq!(close["exit_code"], 3);
        assert_eq!(close["account"], "root");
        assert_eq!(close["program"], "/bin/bash");
        assert!(close["duration_ms"].is_u64());
        assert_eq!(app.state.terminals.count_on(&agent.device_id), 0);

        // Closing the browser's socket tells the Agent to end the shell.
        let mut terminal = start(
            &server,
            &mut admin,
            &agent,
            &mut control,
            request("login", "system"),
        )
        .await;
        terminal.browser.close(None).await.unwrap();
        assert_eq!(closed(&mut terminal.agent).await.0, 1000, "{name}");
        let closes = wait_for_audit(&app, "terminal.close", 2).await;
        assert_eq!(closes[0].metadata["reason"], "closed");

        // The Agent's socket ending is the device disconnecting.
        let mut terminal = start(
            &server,
            &mut admin,
            &agent,
            &mut control,
            request("sh", "system"),
        )
        .await;
        drop(terminal.agent);
        assert_eq!(closed(&mut terminal.browser).await.0, 4009, "{name}");

        // A shell that can't start says why, then the terminal ends.
        let opened = open(&mut admin, &agent, request("zsh", "user")).await;
        let id = opened.body["id"].as_str().unwrap();
        let mut browser_ws = browser_socket(&server, &admin, id).await.unwrap();
        let command = receive_json(&mut control).await;
        assert_eq!(
            command["terminal"]["notification"], "Ada Admin has connected to this computer.",
            "{name}: a terminal as the user follows the session notification policy"
        );
        let mut agent_ws = agent_socket(&server, &agent, id).await.unwrap();
        let failed = json!({ "type": "failed", "message": "Nobody is signed in." });
        send(&mut agent_ws, Message::Text(failed.to_string().into())).await;
        assert_eq!(next_json(&mut browser_ws).await, failed);
        assert_eq!(closed(&mut browser_ws).await.0, 1000);
        let closes = wait_for_audit(&app, "terminal.close", 4).await;
        assert_eq!(closes[0].metadata["reason"], "failed");
        assert_eq!(closes[0].metadata["error"], "Nobody is signed in.");
        app.finish().await;
    }
}

/// The audit events for `action` once there are `count`.
async fn wait_for_audit(
    app: &App,
    action: &str,
    count: usize,
) -> Vec<meshrmm_server::audit::Event> {
    let deadline = tokio::time::Instant::now() + SOCKET_WAIT;
    loop {
        let events = audit_events(app, action).await;
        if events.len() >= count {
            return events;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} {action}",
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn permissions_and_requests_are_checked() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, _control) = enrolled(&app, &mut admin, &server, "windows").await;

        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        for run_as in ["user", "system"] {
            let refused = open(&mut viewer, &agent, request("powershell", run_as)).await;
            assert_eq!(refused.status, 403, "{name}");
            assert_eq!(refused.code(), "permission_denied");
        }
        let (mut helpdesk, _) = user_with(
            &app,
            &mut admin,
            "helpdesk@example.com",
            &["terminal.connect"],
        )
        .await;
        assert_eq!(
            open(&mut helpdesk, &agent, request("cmd", "system"))
                .await
                .status,
            403
        );
        let allowed = open(&mut helpdesk, &agent, request("cmd", "user")).await;
        assert_eq!(allowed.status, 201, "{name}: {:?}", allowed.body);
        assert_eq!(allowed.body["shell"], "cmd");
        assert_eq!(allowed.body["run_as"], "user");
        // Only the user who asked for it can open its socket, once, from the website.
        let id = allowed.body["id"].as_str().unwrap();
        assert_eq!(browser_socket(&server, &admin, id).await.err(), Some(404));
        let cookie = helpdesk.cookie.clone().unwrap();
        let foreign = ws(
            &server.ws_url(&format!("/v1/terminals/{id}")),
            &[("origin", "https://evil.example"), ("cookie", &cookie)],
        )
        .await;
        assert_eq!(foreign.err(), Some(403));
        // Agents connect only for terminals they were asked to open.
        assert_eq!(agent_socket(&server, &agent, id).await.err(), Some(404));
        assert_eq!(
            agent_socket(&server, &agent.with_token(&"0".repeat(64)), id)
                .await
                .err(),
            Some(401)
        );

        let wrong_shell = open(&mut admin, &agent, request("bash", "system")).await;
        assert_eq!(wrong_shell.status, 400, "{name}");
        assert_eq!(wrong_shell.code(), "shell_unsupported");
        let too_big = open(
            &mut admin,
            &agent,
            json!({ "shell": "cmd", "run_as": "system", "columns": 1001, "rows": 30 }),
        )
        .await;
        assert_eq!(too_big.code(), "invalid_size");
        let unknown = admin
            .post(
                &format!("/v1/agents/{}/terminals", uuid::Uuid::new_v4()),
                request("cmd", "system"),
            )
            .await;
        assert_eq!(unknown.status, 404);
        let offline = app.enroll_on(&mut admin, "offline", "windows").await;
        let refused = open(&mut admin, &offline, request("cmd", "system")).await;
        assert_eq!(refused.status, 409);
        assert_eq!(refused.code(), "device_offline");
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_and_users_have_a_limit() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, _control) = enrolled(&app, &mut admin, &server, "macos").await;
        for _ in 0..MAX_TERMINALS_PER_DEVICE {
            assert_eq!(
                open(&mut admin, &agent, request("zsh", "system"))
                    .await
                    .status,
                201
            );
        }
        let refused = open(&mut admin, &agent, request("zsh", "system")).await;
        assert_eq!(refused.status, 409, "{}", app.name);
        assert_eq!(refused.code(), "too_many_terminals");
        app.finish().await;
    }
}

#[tokio::test]
async fn terminals_end_when_idle_unanswered_or_no_longer_allowed() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server, "linux").await;

        // Nobody typed.
        let mut terminal = start(
            &server,
            &mut admin,
            &agent,
            &mut control,
            request("bash", "system"),
        )
        .await;
        let (code, reason) = closed(&mut terminal.browser).await;
        assert_eq!(code, 4008, "{name}");
        assert_eq!(reason, "closed after 1 second without input");
        assert_eq!(closed(&mut terminal.agent).await.0, 1000);

        // The Agent never connected its side.
        let opened = open(&mut admin, &agent, request("bash", "system")).await;
        let mut browser_ws = browser_socket(&server, &admin, opened.body["id"].as_str().unwrap())
            .await
            .unwrap();
        receive_json(&mut control).await;
        assert_eq!(closed(&mut browser_ws).await.0, 4004, "{name}");

        // Acknowledging output the browser wasn't sent breaks the protocol.
        let mut terminal = start(
            &server,
            &mut admin,
            &agent,
            &mut control,
            request("bash", "system"),
        )
        .await;
        send(
            &mut terminal.browser,
            Message::Text(json!({ "type": "ack", "bytes": 10 }).to_string().into()),
        )
        .await;
        assert_eq!(closed(&mut terminal.browser).await.0, 1003, "{name}");

        // Losing the permission ends it at once.
        let (mut tech, tech_id) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "terminal.connect_system"],
        )
        .await;
        let mut terminal = start(
            &server,
            &mut tech,
            &agent,
            &mut control,
            request("bash", "system"),
        )
        .await;
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
        assert_eq!(closed(&mut terminal.browser).await.0, 4003, "{name}");
        assert_eq!(closed(&mut terminal.agent).await.0, 1000);

        // Signing out ends it too.
        let mut terminal = start(
            &server,
            &mut admin,
            &agent,
            &mut control,
            request("bash", "system"),
        )
        .await;
        admin.post("/v1/auth/sign-out", json!({})).await;
        assert_eq!(closed(&mut terminal.browser).await.0, 4001, "{name}");
        let reasons: Vec<Value> = audit_events(&app, "terminal.close")
            .await
            .into_iter()
            .map(|event| event.metadata["reason"].clone())
            .collect();
        assert!(reasons.contains(&json!("idle")), "{name}: {reasons:?}");
        assert!(reasons.contains(&json!("device_timeout")));
        assert!(reasons.contains(&json!("protocol_error")));
        assert!(reasons.contains(&json!("permission_removed")));
        app.finish().await;
    }
}

#[tokio::test]
async fn idle_time_is_a_setting() {
    for app in apps().await {
        let mut admin = set_up(&app).await;
        let settings = admin.get("/v1/settings").await;
        assert_eq!(
            settings.body["terminal_idle_timeout_minutes"], 30,
            "{}",
            app.name
        );
        let changed = admin
            .patch(
                "/v1/settings",
                json!({ "terminal_idle_timeout_minutes": 60 }),
            )
            .await;
        assert_eq!(changed.body["terminal_idle_timeout_minutes"], 60);
        let refused = admin
            .patch(
                "/v1/settings",
                json!({ "terminal_idle_timeout_minutes": 7 }),
            )
            .await;
        assert_eq!(refused.status, 400);
        app.finish().await;
    }
}
