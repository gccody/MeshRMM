//! Monitors the Agent checks, the alerts their results raise and resolve,
//! and self-healing.
mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{App, Browser, Ws, apps, audit_events, receive, send_json, set_up, user_with};
use meshrmm_protocol_types::{
    AgentCommand, MonitorActionResult, MonitorCheck, MonitorResult, RemediationAction, RunAs,
};
use meshrmm_server::{
    alerts, monitors,
    realtime::AgentConnection,
    time::{MINUTE_MS, now_ms},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A webhook receiver on a loopback port that hands over each request body.
struct Webhook {
    url: String,
    requests: mpsc::UnboundedReceiver<Value>,
}

impl Webhook {
    async fn start() -> Self {
        let (sender, requests) = mpsc::unbounded_channel();
        let router = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |body: axum::body::Bytes| {
                let sender = sender.clone();
                async move {
                    sender.send(serde_json::from_slice(&body).unwrap()).ok();
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, requests }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.requests.recv())
            .await
            .expect("no webhook arrived")
            .unwrap()
    }

    async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(300), self.requests.recv())
                .await
                .is_err(),
            "no webhook expected"
        );
    }
}

async fn create_webhook(admin: &mut Browser, url: &str) -> String {
    let created = admin
        .post(
            "/v1/notification-channels",
            json!({ "name": "On-call", "kind": "webhook", "webhook_url": url }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn create_rule(browser: &mut Browser, rule: Value) -> Value {
    let created = browser.post("/v1/alert-rules", rule).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body
}

async fn create_script(admin: &mut Browser, name: &str, shared: bool) -> String {
    let created = admin
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": name, "language": "shell", "body": "systemctl restart nginx", "timeout_seconds": 30, "shared": shared }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

fn service_rule(name: &str, extra: Value) -> Value {
    let mut rule = json!({
        "name": name, "severity": "critical", "condition": "service",
        "monitor": { "interval_seconds": 60, "check": { "kind": "service", "name": "nginx" } },
    });
    for (key, value) in extra.as_object().unwrap() {
        rule[key] = value.clone();
    }
    rule
}

fn result(rule_id: &str, passing: bool, failures: u32, detail: &str) -> MonitorResult {
    MonitorResult {
        monitor_id: rule_id.to_owned(),
        checked_at_unix_ms: now_ms() as u64,
        passing,
        failures,
        value: None,
        detail: detail.to_owned(),
        output: String::new(),
    }
}

/// Waits for the alerts in `query` to number `count`.
async fn alerts_until(browser: &mut Browser, query: &str, count: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let listed = browser.get(&format!("/v1/alerts{query}")).await;
        assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
        let alerts = listed.body["alerts"].as_array().unwrap().clone();
        if alerts.len() == count {
            return alerts;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} alerts in {query:?}, got {alerts:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next message on the Agent's socket of `kind`, skipping others.
async fn next_of(socket: &mut Ws, kind: &str) -> Value {
    loop {
        if let common::Received::Json(value) = receive(socket).await
            && value["type"] == kind
        {
            return value;
        }
    }
}

/// Whether `command` only keeps the Agent's monitors current, which the
/// engine does whenever it looks.
fn routine(command: &AgentCommand) -> bool {
    matches!(
        command,
        AgentCommand::SetMonitors { .. } | AgentCommand::CheckMonitors { .. }
    )
}

/// The next command for a stand-in Agent other than a monitor update.
async fn next_action(connection: &mut AgentConnection) -> AgentCommand {
    loop {
        let command =
            tokio::time::timeout(Duration::from_secs(10), common::next_command(connection))
                .await
                .expect("no command arrived")
                .expect("the connection was replaced");
        if !matches!(command, AgentCommand::SetMonitors { .. }) {
            return command;
        }
    }
}

/// Asserts the stand-in Agent is asked for nothing but monitor updates.
async fn no_action(connection: &mut AgentConnection) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while let Ok(Some(command)) =
        tokio::time::timeout_at(deadline, common::next_command(connection)).await
    {
        assert!(routine(&command), "unexpected command {command:?}");
    }
}

fn action_of(command: AgentCommand) -> meshrmm_protocol_types::MonitorAction {
    match command {
        AgentCommand::RunMonitorAction { action } => action,
        other => panic!("expected a self-healing action, got {other:?}"),
    }
}

async fn remediations(browser: &mut Browser, alert_id: &str) -> Vec<Value> {
    let listed = browser
        .get(&format!("/v1/alerts/{alert_id}/remediations"))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    listed.body.as_array().unwrap().clone()
}

#[tokio::test]
async fn agents_get_their_monitors_and_results_raise_and_resolve_alerts() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let mut hook = Webhook::start().await;
        let channel = create_webhook(&mut admin, &hook.url).await;
        let rule = create_rule(
            &mut admin,
            service_rule(
                "Web server",
                json!({ "channel_ids": [channel], "monitor": {
                    "interval_seconds": 60, "failures": 2,
                    "check": { "kind": "service", "name": " nginx " },
                } }),
            ),
        )
        .await;
        assert_eq!(rule["description"], "\u{201c}nginx\u{201d} isn't running");
        assert_eq!(
            rule["monitor"]["check"]["name"], "nginx",
            "names are trimmed"
        );
        assert_eq!(rule["owner"], "Ada Admin", "{rule}");
        let rule_id = rule["id"].as_str().unwrap().to_owned();

        // The Agent gets its monitors when it connects.
        let mut socket = agent.connect(&server).await.unwrap();
        let set = next_of(&mut socket, "set_monitors").await;
        assert_eq!(set["monitors"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(set["monitors"][0]["id"], rule_id.as_str());
        assert_eq!(set["monitors"][0]["interval_seconds"], 60);
        assert_eq!(
            set["monitors"][0]["check"],
            json!({ "kind": "service", "name": "nginx", "automatic": false })
        );

        // One failure is under the rule's threshold of two.
        let report = |passing: bool, failures: u32, output: &str| {
            json!({ "type": "monitor_results", "results": [{
                "monitor_id": rule_id, "checked_at_unix_ms": now_ms(), "passing": passing,
                "failures": failures, "detail": if passing { "\u{201c}nginx\u{201d} is running." } else { "\u{201c}nginx\u{201d} isn't running (inactive (dead))." },
                "output": output,
            }] })
        };
        send_json(&mut socket, report(false, 1, "")).await;
        let monitors_path = format!("/v1/agents/{}/monitors", agent.device_id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let listed = loop {
            let listed = admin.get(&monitors_path).await;
            assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
            if listed.body[0]["state"]["failures"] == 1 {
                break listed.body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{name}: {}",
                listed.body
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(listed[0]["rule_name"], "Web server");
        assert_eq!(listed[0]["failures_before_alert"], 2);
        assert_eq!(listed[0]["state"]["passing"], false);
        assert_eq!(listed[0]["alert_id"], Value::Null);
        assert!(alerts_until(&mut admin, "", 0).await.is_empty());

        send_json(&mut socket, report(false, 2, "nginx: bind() failed")).await;
        let open = alerts_until(&mut admin, "", 1).await;
        assert_eq!(
            open[0]["detail"],
            "\u{201c}nginx\u{201d} isn't running (inactive (dead))."
        );
        assert_eq!(open[0]["output"], "nginx: bind() failed");
        assert_eq!(open[0]["severity"], "critical");
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        let raised = hook.next().await;
        assert_eq!(raised["summary"], "Critical: Web server on Web", "{name}");
        // Failing on is the same alert, and doesn't notify again.
        send_json(&mut socket, report(false, 3, "nginx: bind() failed")).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            alerts_until(&mut admin, "", 1).await[0]["id"],
            alert_id.as_str()
        );
        hook.quiet().await;
        let listed = admin.get(&monitors_path).await.body;
        assert_eq!(listed[0]["alert_id"], alert_id.as_str());
        let states = admin
            .get(&format!("/v1/monitor-states?rule={rule_id}"))
            .await;
        assert_eq!(
            states.body["states"][0]["device_id"],
            agent.device_id.as_str()
        );

        // People can have it checked now.
        let check_path = format!("/v1/agents/{}/monitors/check", agent.device_id);
        let checked = admin
            .post(&check_path, json!({ "rule_ids": [rule_id] }))
            .await;
        assert_eq!(checked.status, StatusCode::ACCEPTED, "{:?}", checked.body);
        let command = next_of(&mut socket, "check_monitors").await;
        assert_eq!(command["monitor_ids"], json!([rule_id]));
        assert_eq!(audit_events(&app, "monitor.check").await.len(), 1);
        let other = create_rule(
            &mut admin,
            json!({ "name": "Busy CPU", "severity": "warning", "condition": "cpu", "threshold": 90, "duration_minutes": 5 }),
        )
        .await;
        let refused = admin
            .post(&check_path, json!({ "rule_ids": [other["id"]] }))
            .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{name}");

        // Passing resolves it.
        send_json(&mut socket, report(true, 0, "")).await;
        let resolved = alerts_until(&mut admin, "?state=resolved", 1).await;
        assert_eq!(resolved[0]["resolution"], "cleared", "{name}");
        hook.quiet().await;

        // Deleting the rule takes the monitor off the device.
        let deleted = admin.delete(&format!("/v1/alert-rules/{rule_id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        // The engine's task looks after a change; tests look themselves.
        monitors::sync(&app.state).await.unwrap();
        let set = next_of(&mut socket, "set_monitors").await;
        assert_eq!(set["monitors"], json!([]), "{name}");
        assert!(
            admin
                .get(&monitors_path)
                .await
                .body
                .as_array()
                .unwrap()
                .is_empty()
        );
        drop(socket);
        app.finish().await;
    }
}

#[tokio::test]
async fn monitors_follow_their_targets_and_platforms() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let windows = app.enroll(&mut admin, "Desk").await;
        let linux = app.enroll_on(&mut admin, "Web", "linux").await;
        let script = create_script(&mut admin, "Backups fresh", true).await;
        let check = create_rule(
            &mut admin,
            json!({ "name": "Backups", "severity": "warning", "condition": "script_check",
                "monitor": { "interval_seconds": 300, "timeout_seconds": 60,
                    "check": { "kind": "script_check", "script_id": script, "run_as": "system" } } }),
        )
        .await;
        assert_eq!(
            check["description"],
            "\u{201c}Backups fresh\u{201d} fails or times out"
        );
        assert_eq!(
            check["monitor"]["check"].get("body"),
            None,
            "the body isn't stored"
        );
        let events = create_rule(
            &mut admin,
            json!({ "name": "Disk errors", "severity": "warning", "condition": "event_log",
                "monitor": { "check": { "kind": "event_log", "source": "disk", "levels": ["error"] } } }),
        )
        .await;
        let check_id = check["id"].as_str().unwrap();
        let events_id = events["id"].as_str().unwrap();

        // The Linux device runs the shell script with its body; the Windows
        // one can't, and an event log check without a log is Linux-only.
        let mut connection = app.state.agents.connect(&linux.device_id);
        let command = monitors::greeting(&app.state, &linux.device_id)
            .await
            .unwrap()
            .unwrap();
        let AgentCommand::SetMonitors { monitors } = command else {
            panic!("expected monitors");
        };
        assert_eq!(monitors.len(), 2, "{name}");
        let script_check = monitors
            .iter()
            .find(|monitor| monitor.id == check_id)
            .unwrap();
        assert_eq!(script_check.timeout_seconds, 60);
        assert!(matches!(
            &script_check.check,
            MonitorCheck::ScriptCheck { body, run_as: RunAs::System, .. } if body == "systemctl restart nginx"
        ));
        assert!(
            monitors::greeting(&app.state, &windows.device_id)
                .await
                .unwrap()
                .is_none()
        );
        let listed = admin
            .get(&format!("/v1/agents/{}/monitors", windows.device_id))
            .await
            .body;
        let problems = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|monitor| {
                (
                    monitor["rule_id"].as_str().unwrap(),
                    monitor["problem"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            problems.contains(&(
                check_id,
                "Shell (zsh/bash) scripts don't run on Windows devices"
            )),
            "{problems:?}"
        );
        assert!(problems.iter().any(|(id, _)| *id == events_id));

        // Results for a monitor the device doesn't run are refused, and
        // the device gets its monitors again.
        monitors::reported(
            &app.state,
            &windows.device_id,
            vec![result(check_id, false, 5, "exit 1")],
        )
        .await;
        assert!(alerts_until(&mut admin, "", 0).await.is_empty());

        // Targeting a company the device isn't in takes its monitors away.
        let company = admin
            .post("/v1/companies", json!({ "name": "Elsewhere" }))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let retargeted = json!({ "name": "Backups", "severity": "warning", "condition": "script_check",
            "target": "companies", "company_ids": [company],
            "monitor": { "interval_seconds": 300, "timeout_seconds": 60,
                "check": { "kind": "script_check", "script_id": script, "run_as": "system" } } });
        let updated = admin
            .put(&format!("/v1/alert-rules/{check_id}"), retargeted)
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{name}: {:?}", updated.body);
        monitors::sync(&app.state).await.unwrap();
        loop {
            let next = tokio::time::timeout(
                Duration::from_secs(10),
                common::next_command(&mut connection),
            )
            .await
            .expect("no monitors arrived");
            match next.unwrap() {
                AgentCommand::SetMonitors { monitors } => {
                    if monitors.len() == 1 {
                        assert_eq!(monitors[0].id, events_id);
                        break;
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn self_healing_restarts_services_and_resolves_alerts_as_remediated() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = agent.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let mut hook = Webhook::start().await;
        let channel = create_webhook(&mut admin, &hook.url).await;
        let rule = create_rule(
            &mut admin,
            service_rule(
                "Web server",
                json!({ "channel_ids": [channel], "notify_when": "remediation_failed",
                    "remediation": { "action": "restart_service", "attempts": 2, "retry_minutes": 5 } }),
            ),
        )
        .await;
        assert_eq!(
            rule["remediation_description"],
            "Restart the service, up to 2 times 5 minutes apart"
        );
        let rule_id = rule["id"].as_str().unwrap().to_owned();

        // The failure starts the first attempt at once, without notifying.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let open = alerts_until(&mut admin, "", 1).await;
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        let action = action_of(next_action(&mut connection).await);
        assert_eq!(action.monitor_id, rule_id);
        assert_eq!(
            action.action,
            RemediationAction::RestartService {
                name: "nginx".into()
            }
        );
        hook.quiet().await;
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts.len(), 1, "{name}");
        assert_eq!(attempts[0]["status"], "running");
        assert_eq!(attempts[0]["action"], "restart_service");
        assert_eq!(attempts[0]["target"], "nginx");
        let audit = &audit_events(&app, "alert.remediate").await[0];
        assert_eq!(
            audit.actor_label, "rule \u{201c}Web server\u{201d}",
            "{name}"
        );
        assert_eq!(audit.target_id, alert_id);

        // Another device can't answer for it.
        monitors::remediation::action_reported(
            &app.state,
            "00000000000000000000000000000000",
            MonitorActionResult {
                action_id: action.action_id.clone(),
                succeeded: true,
                detail: "no".into(),
            },
        )
        .await;
        assert_eq!(
            remediations(&mut admin, &alert_id).await[0]["status"],
            "running"
        );

        // It worked: the device checks again, passes, and the alert is
        // remediated, which nobody hears about as nobody heard it raised.
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: true,
                detail: "Restarted \u{201c}nginx\u{201d}.".into(),
            },
        )
        .await;
        assert!(matches!(
            next_action(&mut connection).await,
            AgentCommand::CheckMonitors { monitor_ids } if monitor_ids == [rule_id.clone()]
        ));
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts[0]["status"], "succeeded");
        assert_eq!(attempts[0]["detail"], "Restarted \u{201c}nginx\u{201d}.");
        assert_eq!(
            alerts_until(&mut admin, "", 1).await[0]["remediation_state"],
            "waiting"
        );
        monitors::reported(&app.state, device, vec![result(&rule_id, true, 0, "up")]).await;
        let resolved = alerts_until(&mut admin, "?state=resolved", 1).await;
        assert_eq!(resolved[0]["resolution"], "remediated", "{name}");
        assert_eq!(resolved[0]["remediation_state"], "succeeded");
        hook.quiet().await;

        // Next time it doesn't help: attempts wait their turn, and once
        // they are used up the channels hear it failed.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let open = alerts_until(&mut admin, "", 1).await;
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        let first = action_of(next_action(&mut connection).await);
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: first.action_id,
                succeeded: false,
                detail: "Job for nginx.service failed.".into(),
            },
        )
        .await;
        alerts::evaluate(&app.state, now_ms() + MINUTE_MS)
            .await
            .unwrap();
        no_action(&mut connection).await;
        alerts::evaluate(&app.state, now_ms() + 6 * MINUTE_MS)
            .await
            .unwrap();
        let second = action_of(next_action(&mut connection).await);
        assert_eq!(remediations(&mut admin, &alert_id).await.len(), 2);
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: second.action_id,
                succeeded: false,
                detail: "Job for nginx.service failed.".into(),
            },
        )
        .await;
        hook.quiet().await;
        alerts::evaluate(&app.state, now_ms() + 12 * MINUTE_MS)
            .await
            .unwrap();
        let failed = hook.next().await;
        assert_eq!(
            failed["event"], "alert.remediation_failed",
            "{name}: {failed}"
        );
        assert!(
            failed["summary"]
                .as_str()
                .unwrap()
                .contains("self-healing failed"),
            "{failed}"
        );
        // It says how the last attempt ended.
        assert_eq!(failed["last_attempt"]["attempt"], 2, "{failed}");
        assert_eq!(failed["last_attempt"]["status"], "failed");
        assert_eq!(
            failed["last_attempt"]["detail"],
            "Job for nginx.service failed."
        );
        assert_eq!(failed.get("remediation_stopped"), None);
        let open = alerts_until(&mut admin, "", 1).await;
        assert_eq!(open[0]["remediation_state"], "failed");
        assert_eq!(open[0]["remediation_attempts"], 2);
        alerts::evaluate(&app.state, now_ms() + 30 * MINUTE_MS)
            .await
            .unwrap();
        no_action(&mut connection).await;
        hook.quiet().await;

        // An unanswered action is lost after a while.
        let resolved = admin
            .post(&format!("/v1/alerts/{alert_id}/resolve"), json!({}))
            .await;
        assert_eq!(resolved.status, StatusCode::OK);
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let open = alerts_until(&mut admin, "", 1).await;
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        action_of(next_action(&mut connection).await);
        alerts::evaluate(&app.state, now_ms() + 6 * MINUTE_MS)
            .await
            .unwrap();
        assert_eq!(
            remediations(&mut admin, &alert_id).await[0]["status"],
            "lost"
        );
        app.finish().await;
    }
}

/// Asserts the stand-in Agent is asked for nothing at all but monitor
/// updates: no action, script or check.
async fn quiet_agent(connection: &mut AgentConnection) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while let Ok(Some(command)) =
        tokio::time::timeout_at(deadline, common::next_command(connection)).await
    {
        assert!(
            matches!(command, AgentCommand::SetMonitors { .. }),
            "unexpected command {command:?}"
        );
    }
}

fn is_check(command: &AgentCommand, rule_id: &str) -> bool {
    matches!(command, AgentCommand::CheckMonitors { monitor_ids } if monitor_ids == &[rule_id.to_owned()])
}

fn alert_of<'a>(alerts: &'a [Value], rule_id: &str) -> &'a Value {
    alerts
        .iter()
        .find(|alert| alert["rule_id"] == rule_id)
        .unwrap_or_else(|| panic!("no alert of {rule_id} in {alerts:?}"))
}

#[tokio::test]
async fn lost_runs_and_late_answers_move_self_healing_on() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = agent.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let script = create_script(&mut admin, "Restart web", true).await;
        let rule = create_rule(
            &mut admin,
            service_rule(
                "Web server",
                json!({ "remediation": { "action": "script", "script_id": script, "attempts": 2, "retry_minutes": 1 } }),
            ),
        )
        .await;
        let rule_id = rule["id"].as_str().unwrap().to_owned();

        // A script attempt whose run never reports, such as because the
        // device restarted, is lost with its run, and the next attempt
        // follows.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let alert_id = alerts_until(&mut admin, "", 1).await[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = match next_action(&mut connection).await {
            AgentCommand::RunScript { run } => run,
            other => panic!("expected a script run, got {other:?}"),
        };
        alerts::evaluate(&app.state, now_ms() + 3 * MINUTE_MS)
            .await
            .unwrap();
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts.len(), 2, "{name}: {attempts:?}");
        assert_eq!(attempts[0]["status"], "lost");
        assert_eq!(attempts[0]["script_run_id"], first.run_id.as_str());
        assert_eq!(attempts[1]["status"], "running");
        assert!(is_check(&next_action(&mut connection).await, &rule_id));
        let second = match next_action(&mut connection).await {
            AgentCommand::RunScript { run } => run,
            other => panic!("expected the second script run, got {other:?}"),
        };
        assert_ne!(second.run_id, first.run_id);

        // The last attempt fails; self-healing gives up after its wait.
        let reported = agent
            .report(
                &format!("script-runs/{}/result", second.run_id),
                json!({ "status": "completed", "ran_as": "root", "exit_code": 1, "stdout": "", "stderr": "no" }),
            )
            .await;
        assert_eq!(reported.status, StatusCode::NO_CONTENT);
        assert_eq!(
            remediations(&mut admin, &alert_id).await[1]["status"],
            "failed"
        );
        assert!(is_check(&next_action(&mut connection).await, &rule_id));
        alerts::evaluate(&app.state, now_ms() + 10 * MINUTE_MS)
            .await
            .unwrap();
        assert_eq!(
            alerts_until(&mut admin, "", 1).await[0]["remediation_state"],
            "failed",
            "{name}"
        );

        // A built-in action gets longer than the Agent's slowest restart,
        // and a success reported after it was given up as lost still counts.
        let restart = create_rule(
            &mut admin,
            service_rule(
                "Web restart",
                json!({ "remediation": { "action": "restart_service", "attempts": 3, "retry_minutes": 5 } }),
            ),
        )
        .await;
        let restart_id = restart["id"].as_str().unwrap().to_owned();
        monitors::reported(
            &app.state,
            device,
            vec![result(&restart_id, false, 1, "down")],
        )
        .await;
        let open = alerts_until(&mut admin, "", 2).await;
        let restart_alert = alert_of(&open, &restart_id)["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let action = action_of(next_action(&mut connection).await);
        alerts::evaluate(&app.state, now_ms() + 4 * MINUTE_MS)
            .await
            .unwrap();
        assert_eq!(
            remediations(&mut admin, &restart_alert).await[0]["status"],
            "running",
            "{name}: four minutes isn't too long for a restart"
        );
        alerts::evaluate(&app.state, now_ms() + 6 * MINUTE_MS)
            .await
            .unwrap();
        assert_eq!(
            remediations(&mut admin, &restart_alert).await[0]["status"],
            "lost"
        );
        assert!(is_check(&next_action(&mut connection).await, &restart_id));
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: true,
                detail: "Restarted \u{201c}nginx\u{201d}.".into(),
            },
        )
        .await;
        let attempts = remediations(&mut admin, &restart_alert).await;
        assert_eq!(attempts[0]["status"], "succeeded", "{name}");
        assert_eq!(
            attempts[0]["detail"],
            "Restarted \u{201c}nginx\u{201d}. (reported late)"
        );
        assert!(is_check(&next_action(&mut connection).await, &restart_id));
        monitors::reported(&app.state, device, vec![result(&restart_id, true, 0, "up")]).await;
        let resolved = alerts_until(&mut admin, "?state=resolved", 1).await;
        assert_eq!(resolved[0]["id"], restart_alert.as_str());
        assert_eq!(resolved[0]["resolution"], "remediated", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn self_healing_takes_credit_only_for_what_it_fixed() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = agent.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let mut hook = Webhook::start().await;
        let channel = create_webhook(&mut admin, &hook.url).await;
        let rule = create_rule(
            &mut admin,
            service_rule(
                "Web server",
                json!({ "remediation": { "action": "restart_service", "attempts": 2, "retry_minutes": 5 } }),
            ),
        )
        .await;
        let rule_id = rule["id"].as_str().unwrap().to_owned();

        // The attempt failed, and then the problem went away by itself:
        // cleared, not remediated, and self-healing stops.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let alert_id = alerts_until(&mut admin, "", 1).await[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let action = action_of(next_action(&mut connection).await);
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: false,
                detail: "Job for nginx.service failed.".into(),
            },
        )
        .await;
        assert!(is_check(&next_action(&mut connection).await, &rule_id));
        monitors::reported(&app.state, device, vec![result(&rule_id, true, 0, "up")]).await;
        let resolved = alerts_until(&mut admin, "?state=resolved", 1).await;
        assert_eq!(resolved[0]["id"], alert_id.as_str());
        assert_eq!(resolved[0]["resolution"], "cleared", "{name}");
        assert_eq!(resolved[0]["remediation_state"], Value::Null);
        alerts::evaluate(&app.state, now_ms() + 10 * MINUTE_MS)
            .await
            .unwrap();
        quiet_agent(&mut connection).await;

        // Resolved by a person while an attempt runs: its answer is
        // recorded, but nothing follows it.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let alert_id = alerts_until(&mut admin, "", 1).await[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let action = action_of(next_action(&mut connection).await);
        let manual = admin
            .post(&format!("/v1/alerts/{alert_id}/resolve"), json!({}))
            .await;
        assert_eq!(manual.status, StatusCode::OK, "{:?}", manual.body);
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: true,
                detail: "Restarted.".into(),
            },
        )
        .await;
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts[0]["status"], "succeeded", "{name}");
        quiet_agent(&mut connection).await;
        alerts::evaluate(&app.state, now_ms() + 10 * MINUTE_MS)
            .await
            .unwrap();
        quiet_agent(&mut connection).await;
        let resolved = alerts_until(&mut admin, "?state=resolved", 2).await;
        let manual = resolved
            .iter()
            .find(|alert| alert["id"] == alert_id.as_str())
            .unwrap();
        assert_eq!(manual["resolution"], "manual", "{name}: {manual}");
        assert_eq!(manual["remediation_state"], Value::Null);
        assert_eq!(remediations(&mut admin, &alert_id).await.len(), 1);

        // A resolved alert left waiting for an attempt gets none.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let alert_id = alerts_until(&mut admin, "", 1).await[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let action = action_of(next_action(&mut connection).await);
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: false,
                detail: "failed".into(),
            },
        )
        .await;
        assert!(is_check(&next_action(&mut connection).await, &rule_id));
        {
            use meshrmm_server::db::tables::Alerts;
            use sea_query::{Expr, ExprTrait, Query};
            app.db()
                .execute(
                    &Query::update()
                        .table(Alerts::Table)
                        .value(Alerts::State, "resolved")
                        .value(Alerts::ResolvedAt, now_ms())
                        .value(Alerts::Resolution, "manual")
                        .and_where(Expr::col(Alerts::Id).eq(alert_id.as_str()))
                        .to_owned(),
                )
                .await
                .unwrap();
        }
        alerts::evaluate(&app.state, now_ms() + 10 * MINUTE_MS)
            .await
            .unwrap();
        quiet_agent(&mut connection).await;
        assert_eq!(remediations(&mut admin, &alert_id).await.len(), 1, "{name}");

        // A rule that waited for self-healing to fail before notifying
        // tells its channels at once when self-healing is taken off it.
        let quiet = create_rule(
            &mut admin,
            service_rule(
                "Quiet heal",
                json!({ "channel_ids": [channel], "notify_when": "remediation_failed",
                    "remediation": { "action": "restart_service", "attempts": 3, "retry_minutes": 30 } }),
            ),
        )
        .await;
        let quiet_id = quiet["id"].as_str().unwrap().to_owned();
        monitors::reported(
            &app.state,
            device,
            vec![result(&quiet_id, false, 1, "down")],
        )
        .await;
        let open = alerts_until(&mut admin, "", 1).await;
        assert_eq!(open[0]["rule_id"], quiet_id.as_str());
        let action = action_of(next_action(&mut connection).await);
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: false,
                detail: "failed".into(),
            },
        )
        .await;
        hook.quiet().await;
        let updated = admin
            .put(
                &format!("/v1/alert-rules/{quiet_id}"),
                service_rule("Quiet heal", json!({ "channel_ids": [channel] })),
            )
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{:?}", updated.body);
        let raised = hook.next().await;
        assert_eq!(raised["event"], "alert.raised", "{name}: {raised}");
        let open = alerts_until(&mut admin, "", 1).await;
        assert_eq!(open[0]["remediation_state"], Value::Null);
        assert_eq!(open[0]["notified"], true);
        alerts::evaluate(&app.state, now_ms() + 60 * MINUTE_MS)
            .await
            .unwrap();
        hook.quiet().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn self_healing_stops_when_the_problem_keeps_coming_back() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = agent.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let mut hook = Webhook::start().await;
        let channel = create_webhook(&mut admin, &hook.url).await;
        let rule = create_rule(
            &mut admin,
            service_rule(
                "Web server",
                json!({ "channel_ids": [channel],
                    "remediation": { "action": "restart_service", "attempts": 3, "retry_minutes": 5 } }),
            ),
        )
        .await;
        let rule_id = rule["id"].as_str().unwrap().to_owned();

        // Fixed three times over; the rule notifies of each raise.
        for fixed in 1..=monitors::remediation::REPEAT_FIXES {
            monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
            alerts_until(&mut admin, "", 1).await;
            assert_eq!(hook.next().await["event"], "alert.raised", "{name}");
            let action = action_of(next_action(&mut connection).await);
            monitors::remediation::action_reported(
                &app.state,
                device,
                MonitorActionResult {
                    action_id: action.action_id,
                    succeeded: true,
                    detail: "Restarted.".into(),
                },
            )
            .await;
            assert!(is_check(&next_action(&mut connection).await, &rule_id));
            monitors::reported(&app.state, device, vec![result(&rule_id, true, 0, "up")]).await;
            let resolved = alerts_until(&mut admin, "?state=resolved", fixed).await;
            assert!(
                resolved
                    .iter()
                    .all(|alert| alert["resolution"] == "remediated"),
                "{name}: {resolved:?}"
            );
        }
        hook.quiet().await;

        // The fourth time, it doesn't try, and the channels hear why, in one
        // notice rather than a raise and a failure in either order.
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let open = alerts_until(&mut admin, "", 1).await;
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        quiet_agent(&mut connection).await;
        let stopped = hook.next().await;
        assert_eq!(
            stopped["event"], "alert.remediation_failed",
            "{name}: {stopped}"
        );
        assert_eq!(
            stopped["remediation_stopped"],
            "Self-healing fixed this 3 times in the past 24 hours and the problem came back each time",
            "{stopped}"
        );
        assert_eq!(stopped["alert"]["id"], alert_id.as_str());
        assert_eq!(stopped["last_attempt"]["status"], "skipped", "{stopped}");
        assert_eq!(stopped["last_attempt"]["target"], "nginx", "{stopped}");
        hook.quiet().await;
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0]["status"], "skipped");
        assert_eq!(
            attempts[0]["target"], "nginx",
            "skipped attempts name what they would act on"
        );
        let open = alerts_until(&mut admin, "", 1).await;
        assert_eq!(open[0]["remediation_state"], "failed");
        alerts::evaluate(&app.state, now_ms() + 30 * MINUTE_MS)
            .await
            .unwrap();
        quiet_agent(&mut connection).await;
        hook.quiet().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn self_healing_runs_shared_scripts_for_any_rule() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = agent.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let script = create_script(&mut admin, "Restart web", true).await;
        let rule = create_rule(
            &mut admin,
            json!({ "name": "Busy CPU", "severity": "warning", "condition": "cpu",
                "threshold": 80, "duration_minutes": 1,
                "remediation": { "action": "script", "script_id": script, "run_as": "system", "attempts": 1 } }),
        )
        .await;
        assert_eq!(
            rule["remediation_description"],
            "Run \u{201c}Restart web\u{201d} as SYSTEM or root, once"
        );

        let now = now_ms();
        let busy = meshrmm_protocol_types::SystemMetrics {
            cpu_percent: 95.0,
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            network_received_bytes_per_second: 0,
            network_sent_bytes_per_second: 0,
            uptime_seconds: 60,
            volumes: vec![],
        };
        let mut at = now - 70_000;
        while at <= now {
            app.state.alerts.observe(device, at, &busy);
            at += 5_000;
        }
        alerts::evaluate(&app.state, now).await.unwrap();
        let open = alerts_until(&mut admin, "", 1).await;
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        let run = match next_action(&mut connection).await {
            AgentCommand::RunScript { run } => run,
            other => panic!("expected a script run, got {other:?}"),
        };
        assert_eq!(run.body, "systemctl restart nginx");
        assert_eq!(run.run_as, RunAs::System);
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts[0]["action"], "script", "{name}");
        assert_eq!(attempts[0]["target"], "Restart web");
        assert_eq!(attempts[0]["script_run_id"], run.run_id.as_str());
        let listed = admin
            .get(&format!("/v1/script-runs/{}", run.run_id))
            .await
            .body;
        assert_eq!(listed["source"], "remediation", "{listed}");
        let ran = &audit_events(&app, "script.run").await[0];
        assert_eq!(ran.actor_label, "rule \u{201c}Busy CPU\u{201d}");

        let reported = agent
            .report(
                &format!("script-runs/{}/result", run.run_id),
                json!({ "status": "completed", "ran_as": "root", "exit_code": 0, "stdout": "restarted\n", "stderr": "" }),
            )
            .await;
        assert_eq!(reported.status, StatusCode::NO_CONTENT);
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts[0]["status"], "succeeded", "{name}");
        assert_eq!(attempts[0]["exit_code"], 0);
        assert_eq!(attempts[0]["output"], "restarted\n");

        // The load drops: the alert clears as remediated.
        let calm = meshrmm_protocol_types::SystemMetrics {
            cpu_percent: 5.0,
            ..busy
        };
        let later = now + 70_000;
        let mut at = later - 65_000;
        while at <= later {
            app.state.alerts.observe(device, at, &calm);
            at += 5_000;
        }
        alerts::evaluate(&app.state, later).await.unwrap();
        let resolved = alerts_until(&mut admin, "?state=resolved", 1).await;
        assert_eq!(resolved[0]["resolution"], "remediated", "{name}");

        // A rule's script can't be deleted or unshared while it runs it.
        let deleted = admin.delete(&format!("/v1/toolbox/scripts/{script}")).await;
        assert_eq!(deleted.code(), "script_in_use", "{name}");
        app.finish().await;
    }
}

async fn call(app: &App, token: &str, method: Method, path: &str) -> common::Response {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    app.browser().raw(request).await
}

#[tokio::test]
async fn monitors_and_self_healing_need_their_permissions() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = agent.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let private = create_script(&mut admin, "Mine", false).await;
        let shared = create_script(&mut admin, "Ours", true).await;

        // Watching needs alerts.manage; acting on devices, scripts.run too.
        let (mut watcher, _) = user_with(
            &app,
            &mut admin,
            "watcher@example.com",
            &["devices.view", "alerts.view", "alerts.manage"],
        )
        .await;
        create_rule(&mut watcher, service_rule("Watch", json!({}))).await;
        create_rule(
            &mut watcher,
            json!({ "name": "Busy", "severity": "warning", "condition": "process",
                "monitor": { "check": { "kind": "process", "name": "sqlservr.exe", "alert_when": "cpu_above", "percent": 90, "minutes": 5 } } }),
        )
        .await;
        // Checks that reach past the device's services and processes run
        // with the saver's access, like self-healing and scripts.
        let probes = [
            json!({ "name": "Gateway", "severity": "warning", "condition": "ping",
                "monitor": { "check": { "kind": "ping", "host": "gw" } } }),
            json!({ "name": "Database", "severity": "warning", "condition": "tcp_port",
                "monitor": { "check": { "kind": "tcp_port", "host": "db", "port": 5432 } } }),
            json!({ "name": "Intranet", "severity": "warning", "condition": "http",
                "monitor": { "check": { "kind": "http", "url": "http://intranet/", "status": 200, "keyword": "salary" } } }),
            json!({ "name": "Mail cert", "severity": "warning", "condition": "certificate",
                "monitor": { "check": { "kind": "certificate", "host": "mail", "port": 993, "days": 14 } } }),
            json!({ "name": "Disk errors", "severity": "warning", "condition": "event_log",
                "monitor": { "check": { "kind": "event_log", "log": "System", "levels": ["error"] } } }),
        ];
        for refused in [
            service_rule(
                "Heal",
                json!({ "remediation": { "action": "restart_service" } }),
            ),
            json!({ "name": "Check", "severity": "warning", "condition": "script_check",
                "monitor": { "check": { "kind": "script_check", "script_id": shared, "run_as": "system" } } }),
        ]
        .into_iter()
        .chain(probes.iter().cloned())
        {
            let response = watcher.post("/v1/alert-rules", refused).await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{name}: {:?}",
                response.body
            );
        }
        let unshared = admin
            .post(
                "/v1/alert-rules",
                service_rule(
                    "Heal",
                    json!({ "remediation": { "action": "script", "script_id": private } }),
                ),
            )
            .await;
        assert_eq!(unshared.code(), "script_not_shared", "{name}");
        for invalid in [
            service_rule(
                "Wrong",
                json!({ "remediation": { "action": "end_process" } }),
            ),
            json!({ "name": "Offline", "severity": "warning", "condition": "device_offline", "duration_minutes": 5,
                "remediation": { "action": "script", "script_id": shared } }),
            // A failed application install needs a person to look, like a
            // failed script.
            json!({ "name": "App failed", "severity": "warning", "condition": "app_install_failed",
                "remediation": { "action": "script", "script_id": shared } }),
            service_rule(
                "Mismatch",
                json!({ "monitor": { "check": { "kind": "ping", "host": "gw" } } }),
            ),
            service_rule(
                "Option",
                json!({ "monitor": { "check": { "kind": "service", "name": "-nginx" } } }),
            ),
            json!({ "name": "Host", "severity": "warning", "condition": "ping",
                "monitor": { "check": { "kind": "ping", "host": "gw; reboot" } } }),
        ] {
            let response = admin.post("/v1/alert-rules", invalid.clone()).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid} {:?}",
                response.body
            );
        }
        // Pending application updates clear once they are installed, so a
        // script can heal them.
        let pending = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Outdated apps", "severity": "info", "condition": "app_updates_pending", "days": 7,
                    "remediation": { "action": "script", "script_id": shared } }),
            )
            .await;
        assert_eq!(
            pending.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            pending.body
        );

        // A rule runs with its owner's access: once they lose it, it keeps
        // watching but skips self-healing, and says why.
        let role = common::add_role(
            &mut admin,
            "Operators",
            &[
                "devices.view",
                "alerts.view",
                "alerts.manage",
                "scripts.run",
            ],
        )
        .await;
        let (mut operator, _) =
            common::add_user(&app, &mut admin, "operator@example.com", &[&role]).await;
        let rule = create_rule(
            &mut operator,
            service_rule("Heal", json!({ "remediation": { "action": "restart_service", "attempts": 3, "retry_minutes": 1 } })),
        )
        .await;
        let rule_id = rule["id"].as_str().unwrap().to_owned();
        assert_eq!(rule["owner"], "operator@example.com", "{rule}");
        let ping = create_rule(&mut operator, probes[0].clone()).await;
        let ping_id = ping["id"].as_str().unwrap().to_owned();
        monitors::reported(&app.state, device, vec![result(&rule_id, false, 1, "down")]).await;
        let open = alerts_until(&mut admin, "", 1).await;
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        let action = action_of(next_action(&mut connection).await);
        monitors::reported(
            &app.state,
            device,
            vec![result(&ping_id, false, 1, "no reply")],
        )
        .await;
        let ping_alert = alert_of(&alerts_until(&mut admin, "", 2).await, &ping_id)["id"]
            .as_str()
            .unwrap()
            .to_owned();
        monitors::remediation::action_reported(
            &app.state,
            device,
            MonitorActionResult {
                action_id: action.action_id,
                succeeded: false,
                detail: "failed".into(),
            },
        )
        .await;
        let changed = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["devices.view", "alerts.view", "alerts.manage"] }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        alerts::evaluate(&app.state, now_ms() + 2 * MINUTE_MS)
            .await
            .unwrap();
        no_action(&mut connection).await;
        let attempts = remediations(&mut admin, &alert_id).await;
        assert_eq!(attempts.len(), 2, "{name}");
        assert_eq!(attempts[1]["status"], "skipped");
        assert!(
            attempts[1]["detail"]
                .as_str()
                .unwrap()
                .contains("no longer has the scripts.run permission;"),
            "{attempts:?}"
        );
        assert_eq!(
            alerts_until(&mut admin, "", 1).await[0]["remediation_state"],
            "failed"
        );
        let listed = admin.get("/v1/alert-rules").await.body;
        let listed = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|listed| listed["id"] == rule_id.as_str())
            .unwrap()
            .clone();
        assert!(
            listed["problem"]
                .as_str()
                .unwrap()
                .contains("operator@example.com"),
            "{listed}"
        );
        let Some(AgentCommand::SetMonitors { monitors: sent }) =
            monitors::greeting(&app.state, device).await.unwrap()
        else {
            panic!("it still watches");
        };
        let sent = sent
            .iter()
            .map(|monitor| monitor.id.as_str())
            .collect::<Vec<_>>();
        assert!(sent.contains(&rule_id.as_str()), "{name}: {sent:?}");
        // But a check that runs with the owner's access is no longer sent,
        // and its alert, which can't change any more, resolves.
        assert!(!sent.contains(&ping_id.as_str()), "{name}: {sent:?}");
        let resolved = alerts_until(&mut admin, "?state=resolved", 1).await;
        assert_eq!(resolved[0]["id"], ping_alert.as_str());
        assert_eq!(resolved[0]["resolution"], "not_applicable", "{name}");
        let listed = admin
            .get(&format!("/v1/agents/{device}/monitors"))
            .await
            .body;
        let ping_listed = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|monitor| monitor["rule_id"] == ping_id.as_str())
            .unwrap();
        assert!(
            ping_listed["problem"]
                .as_str()
                .unwrap()
                .contains("no longer has the scripts.run permission;"),
            "{ping_listed}"
        );

        // Self-healing that was skipped fixed nothing: the alert clears,
        // and its self-healing stays failed.
        monitors::reported(&app.state, device, vec![result(&rule_id, true, 0, "up")]).await;
        let resolved = alerts_until(&mut admin, "?state=resolved", 2).await;
        let healed = alert_of(&resolved, &rule_id);
        assert_eq!(healed["resolution"], "cleared", "{name}: {healed}");
        assert_eq!(healed["remediation_state"], "failed");

        // People without alerts.view see no monitors or attempts; tokens
        // act for their user.
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let monitors_path = format!("/v1/agents/{device}/monitors");
        assert_eq!(
            viewer.get(&monitors_path).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer
                .get(&format!("/v1/alerts/{alert_id}/remediations"))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        let checked = viewer
            .post(&format!("/v1/agents/{device}/monitors/check"), json!({}))
            .await;
        assert_eq!(checked.status, StatusCode::FORBIDDEN);
        let token = admin
            .post(
                "/v1/api-tokens",
                json!({ "name": "Monitoring", "expires_in_days": 30 }),
            )
            .await
            .body["secret"]
            .as_str()
            .unwrap()
            .to_owned();
        let listed = call(&app, &token, Method::GET, &monitors_path).await;
        assert_eq!(listed.status, StatusCode::OK, "{name}: {:?}", listed.body);
        assert_eq!(listed.body.as_array().unwrap().len(), 4);
        // A token limited to watching can't save a probe either.
        let limited = admin
            .post(
                "/v1/api-tokens",
                json!({ "name": "Watching", "expires_in_days": 30,
                    "permissions": ["devices.view", "alerts.view", "alerts.manage"] }),
            )
            .await;
        assert_eq!(limited.status, StatusCode::CREATED, "{:?}", limited.body);
        let limited = limited.body["secret"].as_str().unwrap().to_owned();
        for (rule, allowed) in [
            (service_rule("Token watch", json!({})), true),
            (probes[1].clone(), false),
        ] {
            let request = axum::http::Request::builder()
                .method(Method::POST)
                .uri("/v1/alert-rules")
                .header("authorization", format!("Bearer {limited}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(rule.to_string()))
                .unwrap();
            let response = app.browser().raw(request).await;
            assert_eq!(
                response.status,
                if allowed {
                    StatusCode::CREATED
                } else {
                    StatusCode::FORBIDDEN
                },
                "{name}: {rule} {:?}",
                response.body
            );
        }
        let attempts = call(
            &app,
            &token,
            Method::GET,
            &format!("/v1/alerts/{alert_id}/remediations"),
        )
        .await;
        assert_eq!(attempts.status, StatusCode::OK);
        assert_eq!(attempts.body.as_array().unwrap().len(), 2);

        // Without scripts.run, a rule that runs with its owner's access can
        // still be turned off or deleted, which only takes power away, and
        // keeps its owner; changing it or turning it on takes scripts.run.
        let ping_path = format!("/v1/alert-rules/{ping_id}");
        let mut off = probes[0].clone();
        off["enabled"] = json!(false);
        let turned_off = watcher.put(&ping_path, off.clone()).await;
        assert_eq!(
            turned_off.status,
            StatusCode::OK,
            "{name}: {:?}",
            turned_off.body
        );
        assert_eq!(turned_off.body["enabled"], false);
        assert_eq!(turned_off.body["owner"], "operator@example.com");
        let mut changed = off.clone();
        changed["name"] = json!("Gateway 2");
        assert_eq!(
            watcher.put(&ping_path, changed).await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let mut on = probes[0].clone();
        on["enabled"] = json!(true);
        assert_eq!(
            watcher.put(&ping_path, on.clone()).await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let listed = admin.get("/v1/alert-rules").await.body;
        let ping_rule = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["id"] == ping_id.as_str())
            .unwrap();
        assert_eq!(ping_rule["name"], "Gateway", "a refused change is undone");
        assert_eq!(ping_rule["enabled"], false);
        let turned_on = admin.put(&ping_path, on).await;
        assert_eq!(turned_on.status, StatusCode::OK, "{:?}", turned_on.body);
        assert_eq!(
            turned_on.body["owner"], "Ada Admin",
            "turning it on lends it your access"
        );
        assert_eq!(
            watcher.delete(&ping_path).await.status,
            StatusCode::NO_CONTENT,
            "{name}"
        );
        app.finish().await;
    }
}
