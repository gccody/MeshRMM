//! Alert rules, the alerts they raise and resolve, and the notification
//! channels they notify.
mod common;

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use common::{App, Browser, FakeSmtp, apps, audit_events, receive_any_json, set_up};
use meshrmm_protocol_types::{MetricsReport, SystemMetrics, VolumeUsage};
use meshrmm_server::{
    alerts,
    time::{MINUTE_MS, SECOND_MS, now_ms},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const GIB: u64 = 1024 * 1024 * 1024;

/// A webhook receiver on a loopback port that hands over each request.
struct Webhook {
    url: String,
    requests: mpsc::UnboundedReceiver<(HeaderMap, Vec<u8>)>,
}

impl Webhook {
    async fn start(status: StatusCode) -> Self {
        let (sender, requests) = mpsc::unbounded_channel();
        let router = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let sender = sender.clone();
                async move {
                    sender.send((headers, body.to_vec())).ok();
                    status
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, requests }
    }

    async fn next(&mut self) -> (HeaderMap, Value) {
        let (headers, body) = tokio::time::timeout(Duration::from_secs(10), self.requests.recv())
            .await
            .expect("no webhook arrived")
            .unwrap();
        let signature = headers
            .get("x-meshrmm-signature")
            .map(|value| value.to_str().unwrap().to_owned());
        if let Some(signature) = signature {
            let timestamp: i64 = headers["x-meshrmm-timestamp"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                signature,
                meshrmm_server::notifications::webhook::signature(b"hook secret", timestamp, &body),
                "the signature covers the timestamp and body"
            );
        }
        (headers, serde_json::from_slice(&body).unwrap())
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

fn reading(cpu: f32, free_gib: u64) -> SystemMetrics {
    SystemMetrics {
        cpu_percent: cpu,
        memory_used_bytes: 10,
        memory_total_bytes: 100,
        network_received_bytes_per_second: 0,
        network_sent_bytes_per_second: 0,
        uptime_seconds: 60,
        volumes: vec![VolumeUsage {
            name: "C:".into(),
            total_bytes: 100 * GIB,
            free_bytes: free_gib * GIB,
        }],
    }
}

/// Feeds the engine readings every five seconds over the `seconds` before
/// `until`.
fn observe(app: &App, device_id: &str, until: i64, seconds: i64, cpu: f32) {
    let mut at = until - seconds * SECOND_MS;
    while at <= until {
        app.state.alerts.observe(device_id, at, &reading(cpu, 50));
        at += 5 * SECOND_MS;
    }
}

async fn create_rule(admin: &mut Browser, rule: Value) -> String {
    let created = admin.post("/v1/alert-rules", rule).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn create_webhook(admin: &mut Browser, url: &str) -> String {
    let created = admin
        .post(
            "/v1/notification-channels",
            json!({ "name": "On-call", "kind": "webhook", "webhook_url": url, "webhook_secret": "hook secret" }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["has_secret"], true);
    assert!(created.body.get("webhook_secret").is_none());
    created.body["id"].as_str().unwrap().to_owned()
}

async fn start_run(admin: &mut Browser, runs_path: &str, script_id: &str) -> String {
    let started = admin
        .post(
            runs_path,
            json!({ "script_id": script_id, "run_as": "system" }),
        )
        .await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
    started.body["id"].as_str().unwrap().to_owned()
}

async fn alerts_of(browser: &mut Browser, query: &str) -> Vec<Value> {
    let listed = browser.get(&format!("/v1/alerts{query}")).await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    listed.body["alerts"].as_array().unwrap().clone()
}

#[tokio::test]
async fn device_state_rules_raise_and_resolve_alerts_and_notify() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let _connection = app.state.agents.connect(&desk.device_id);
        let mut hook = Webhook::start(StatusCode::OK).await;
        let channel = create_webhook(&mut admin, &hook.url).await;
        let cpu = create_rule(
            &mut admin,
            json!({
                "name": "Busy CPU", "severity": "critical", "condition": "cpu",
                "threshold": 80, "duration_minutes": 1, "channel_ids": [channel],
                "notify_on_resolve": true,
            }),
        )
        .await;
        create_rule(
            &mut admin,
            json!({
                "name": "Low disk", "severity": "warning", "condition": "disk_space",
                "threshold": 10, "threshold_unit": "percent",
            }),
        )
        .await;
        let mut events = admin.events(&server).await.unwrap();
        assert_eq!(receive_any_json(&mut events).await["type"], "snapshot");
        let counts = receive_any_json(&mut events).await;
        assert_eq!(counts["type"], "alert_counts", "{name}");
        assert_eq!(counts["counts"]["open"], 0);

        let now = now_ms();
        observe(&app, &desk.device_id, now, 70, 95.0);
        app.state
            .metrics
            .record(&desk.device_id, MetricsReport::new(reading(95.0, 4), true));
        alerts::evaluate(&app.state, now).await.unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 2, "{name}: {open:?}");
        let cpu_alert = open
            .iter()
            .find(|alert| alert["rule_id"] == cpu.as_str())
            .unwrap();
        assert_eq!(cpu_alert["severity"], "critical");
        assert_eq!(cpu_alert["device_name"], "Desk");
        assert_eq!(cpu_alert["state"], "open");
        assert_eq!(cpu_alert["value"], 95.0);
        assert_eq!(cpu_alert["detail"], "CPU averaged 95% over 1 minute");
        let disk_alert = open.iter().find(|alert| alert["subject"] == "C:").unwrap();
        assert_eq!(disk_alert["detail"], "C: has 4 GB free (4%)");
        let cpu_id = cpu_alert["id"].as_str().unwrap().to_owned();
        let disk_id = disk_alert["id"].as_str().unwrap().to_owned();

        let (headers, raised) = hook.next().await;
        assert_eq!(headers["x-meshrmm-event"], "alert.raised");
        assert_eq!(raised["event"], "alert.raised");
        assert_eq!(raised["summary"], "Critical: Busy CPU on Desk");
        assert_eq!(raised["alert"]["id"], cpu_id.as_str());
        assert_eq!(raised["server"], common::ORIGIN);
        hook.quiet().await;
        let mut upserts = 0;
        while upserts < 2 {
            let event = receive_any_json(&mut events).await;
            if event["type"] == "alert_upsert" {
                upserts += 1;
                assert_eq!(event["alert"]["state"], "open");
            }
        }

        // Evaluating again changes nothing.
        alerts::evaluate(&app.state, now + SECOND_MS).await.unwrap();
        assert_eq!(alerts_of(&mut admin, "").await.len(), 2);
        hook.quiet().await;
        let counts = admin.get("/v1/alerts/counts").await.body;
        assert_eq!(
            counts,
            json!({ "open": 2, "acknowledged": 0, "open_info": 0, "open_warning": 1, "open_critical": 1 })
        );

        let acknowledged = admin
            .post(&format!("/v1/alerts/{cpu_id}/acknowledge"), json!({}))
            .await;
        assert_eq!(
            acknowledged.status,
            StatusCode::OK,
            "{:?}",
            acknowledged.body
        );
        assert_eq!(acknowledged.body["state"], "acknowledged");
        assert_eq!(acknowledged.body["acknowledged_by"], common::ADMIN_EMAIL);
        assert_eq!(
            admin
                .post(&format!("/v1/alerts/{cpu_id}/acknowledge"), json!({}))
                .await
                .status,
            StatusCode::CONFLICT
        );
        assert_eq!(audit_events(&app, "alert.acknowledge").await.len(), 1);

        // The load drops: the acknowledged alert resolves itself.
        let later = now + 70 * SECOND_MS;
        observe(&app, &desk.device_id, later, 65, 5.0);
        alerts::evaluate(&app.state, later).await.unwrap();
        let resolved = admin.get(&format!("/v1/alerts/{cpu_id}")).await.body;
        assert_eq!(resolved["state"], "resolved", "{name}");
        assert_eq!(resolved["resolution"], "cleared");
        assert!(resolved["resolved_by"].is_null());
        let (_, cleared) = hook.next().await;
        assert_eq!(cleared["event"], "alert.resolved");
        assert_eq!(cleared["summary"], "Resolved: Busy CPU on Desk");

        let manual = admin
            .post(&format!("/v1/alerts/{disk_id}/resolve"), json!({}))
            .await;
        assert_eq!(manual.status, StatusCode::OK);
        assert_eq!(manual.body["resolution"], "manual");
        assert_eq!(manual.body["resolved_by"], common::ADMIN_EMAIL);
        assert_eq!(audit_events(&app, "alert.resolve").await.len(), 1);
        assert_eq!(alerts_of(&mut admin, "").await.len(), 0);
        assert_eq!(alerts_of(&mut admin, "?state=resolved").await.len(), 2);
        assert_eq!(
            alerts_of(&mut admin, &format!("?state=all&device={}", desk.device_id))
                .await
                .len(),
            2
        );
        assert_eq!(
            alerts_of(&mut admin, "?state=all&severity=info")
                .await
                .len(),
            0
        );
        assert_eq!(
            alerts_of(&mut admin, "?state=all&company=none").await.len(),
            2
        );

        // After a restart nothing is raised again until the readings say so.
        let restarted = app.restarted().await;
        alerts::evaluate(&restarted.state, later + SECOND_MS)
            .await
            .unwrap();
        assert_eq!(alerts_of(&mut admin, "?state=all").await.len(), 2);
        hook.quiet().await;
        restarted.finish().await;
        app.finish().await;
    }
}

/// Feeds the engine a reading at each of `times`.
fn observe_at(app: &App, device_id: &str, times: impl IntoIterator<Item = i64>, cpu: f32) {
    for at in times {
        app.state.alerts.observe(device_id, at, &reading(cpu, 50));
    }
}

#[tokio::test]
async fn load_rules_judge_devices_that_report_once_a_minute() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let idle = app.enroll(&mut admin, "Idle").await;
        let quiet = app.enroll(&mut admin, "Quiet").await;
        let mixed = app.enroll(&mut admin, "Mixed").await;
        let _connections =
            [&idle, &quiet, &mixed].map(|agent| app.state.agents.connect(&agent.device_id));
        create_rule(
            &mut admin,
            json!({
                "name": "Busy CPU", "severity": "warning", "condition": "cpu",
                "threshold": 80, "duration_minutes": 10,
            }),
        )
        .await;
        let now = now_ms();
        // Older than the 30 seconds after which a reading used to count as
        // stale, though the next is not due for a quarter of a minute.
        let latest = now - 45 * SECOND_MS;
        // Nobody watches it: a reading a minute.
        observe_at(
            &app,
            &idle.device_id,
            (0..12).rev().map(|n| latest - n * MINUTE_MS),
            95.0,
        );
        // Connected, but its readings stopped three minutes ago.
        observe_at(
            &app,
            &quiet.device_id,
            (0..12).rev().map(|n| now - 3 * MINUTE_MS - n * MINUTE_MS),
            95.0,
        );
        // Nine minutes nobody watched at 90%, then a minute someone did at
        // 50%: twelve readings five seconds apart.
        observe_at(
            &app,
            &mixed.device_id,
            (1..=10).rev().map(|n| latest - n * MINUTE_MS),
            90.0,
        );
        observe_at(
            &app,
            &mixed.device_id,
            (0..12).rev().map(|n| latest - n * 5 * SECOND_MS),
            50.0,
        );
        alerts::evaluate(&app.state, now).await.unwrap();
        let mut open = alerts_of(&mut admin, "").await;
        open.sort_by_key(|alert| alert["device_name"].as_str().unwrap().to_owned());
        let raised: Vec<_> = open
            .iter()
            .map(|alert| {
                (
                    alert["device_name"].as_str().unwrap(),
                    alert["detail"].as_str().unwrap(),
                )
            })
            .collect();
        // Weighted by the time each reading covers, the mixed device
        // averaged (9 × 90 + 1 × 50) / 10; counting readings it would be
        // (9 × 90 + 12 × 50) / 21, about 67, and no alert.
        assert_eq!(
            raised,
            [
                ("Idle", "CPU averaged 95% over 10 minutes"),
                ("Mixed", "CPU averaged 86% over 10 minutes"),
            ],
            "{name}"
        );
        assert_eq!(open[1]["value"], 86.0, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn the_engine_keeps_readings_only_as_far_back_as_load_rules_look() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let device = desk.device_id.as_str();
        let _connection = app.state.agents.connect(device);
        let start = now_ms() - 4 * 60 * MINUTE_MS;

        // No rule reads loads: nothing is kept, however long it reports.
        create_rule(
            &mut admin,
            json!({ "name": "Gone", "severity": "warning", "condition": "device_offline", "duration_minutes": 15 }),
        )
        .await;
        let mut at = start + 60 * MINUTE_MS;
        observe(&app, device, at, 3_600, 95.0);
        alerts::evaluate(&app.state, at).await.unwrap();
        assert_eq!(
            app.state.alerts.held(device),
            alerts::Held::default(),
            "{name}"
        );

        // A rule at the website's default window keeps ten minutes, as soon as
        // it is saved.
        let mut rule = json!({
            "name": "Busy CPU", "severity": "warning", "condition": "cpu",
            "threshold": 80, "duration_minutes": 10,
        });
        let rule_id = create_rule(&mut admin, rule.clone()).await;
        at += 60 * MINUTE_MS;
        observe(&app, device, at, 3_595, 95.0);
        assert_eq!(app.state.alerts.held(device).readings, 120, "{name}");
        alerts::evaluate(&app.state, at).await.unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        assert_eq!(open[0]["detail"], "CPU averaged 95% over 10 minutes");

        // Lengthened to an hour, it waits for an hour of readings to judge:
        // until then the alert stays, though the load dropped.
        rule["duration_minutes"] = json!(60);
        let updated = admin
            .put(&format!("/v1/alert-rules/{rule_id}"), rule.clone())
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{:?}", updated.body);
        at += 40 * MINUTE_MS;
        observe(&app, device, at, 2_395, 5.0);
        alerts::evaluate(&app.state, at).await.unwrap();
        assert_eq!(alerts_of(&mut admin, "").await.len(), 1, "{name}");
        at += 20 * MINUTE_MS;
        observe(&app, device, at, 1_195, 5.0);
        alerts::evaluate(&app.state, at).await.unwrap();
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");
        assert_eq!(app.state.alerts.held(device).readings, 720, "{name}");

        // Turned off, nothing is kept again.
        rule["enabled"] = json!(false);
        let updated = admin.put(&format!("/v1/alert-rules/{rule_id}"), rule).await;
        assert_eq!(updated.status, StatusCode::OK, "{:?}", updated.body);
        alerts::evaluate(&app.state, at).await.unwrap();
        assert_eq!(
            app.state.alerts.held(device),
            alerts::Held::default(),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn offline_alerts_count_from_the_disconnect_or_the_server_start() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let rule = create_rule(
            &mut admin,
            json!({ "name": "Offline", "severity": "critical", "condition": "device_offline", "duration_minutes": 10 }),
        )
        .await;
        let start = now_ms();
        alerts::evaluate(&app.state, start).await.unwrap();
        alerts::evaluate(&app.state, start + 9 * MINUTE_MS)
            .await
            .unwrap();
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");
        alerts::evaluate(&app.state, start + 11 * MINUTE_MS)
            .await
            .unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 1, "{name}");
        assert_eq!(open[0]["detail"], "Offline for 11 minutes");

        // A restarted server neither repeats it nor raises one for devices
        // that went offline while it was down until the duration passes.
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let restarted = app.restarted().await;
        alerts::evaluate(&restarted.state, start + 20 * MINUTE_MS)
            .await
            .unwrap();
        assert_eq!(alerts_of(&mut admin, "").await.len(), 1, "{name}");
        alerts::evaluate(&restarted.state, start + 31 * MINUTE_MS)
            .await
            .unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 2, "{name}");
        assert!(
            open.iter()
                .any(|alert| alert["device_id"] == laptop.device_id.as_str())
        );

        // Coming back online resolves it.
        let _connection = restarted.state.agents.connect(&desk.device_id);
        alerts::evaluate(&restarted.state, start + 32 * MINUTE_MS)
            .await
            .unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 1, "{name}");
        assert_eq!(open[0]["device_id"], laptop.device_id.as_str());

        // Turning the rule off resolves the rest; deleting it deletes them.
        let disabled = admin
            .put(
                &format!("/v1/alert-rules/{rule}"),
                json!({ "name": "Offline", "severity": "critical", "condition": "device_offline", "duration_minutes": 10, "enabled": false }),
            )
            .await;
        assert_eq!(disabled.status, StatusCode::OK, "{:?}", disabled.body);
        alerts::evaluate(&restarted.state, start + 33 * MINUTE_MS)
            .await
            .unwrap();
        let all = alerts_of(&mut admin, "?state=all").await;
        assert!(
            all.iter().all(|alert| alert["state"] == "resolved"),
            "{all:?}"
        );
        assert!(
            all.iter()
                .any(|alert| alert["resolution"] == "not_applicable")
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/alert-rules/{rule}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert!(alerts_of(&mut admin, "?state=all").await.is_empty());
        assert_eq!(audit_events(&app, "alert_rule.update").await.len(), 1);
        assert_eq!(audit_events(&app, "alert_rule.delete").await.len(), 1);
        restarted.finish().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn rules_cover_all_devices_or_chosen_companies() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let acme = admin
            .post("/v1/companies", json!({ "name": "Acme" }))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let desk = app.enroll(&mut admin, "Desk").await;
        let other = app.enroll(&mut admin, "Other").await;
        let moved = admin
            .put(
                &format!("/v1/agents/{}/company", desk.device_id),
                json!({ "company_id": acme }),
            )
            .await;
        assert_eq!(moved.status, StatusCode::NO_CONTENT);
        create_rule(
            &mut admin,
            json!({
                "name": "Acme offline", "severity": "warning", "condition": "device_offline",
                "duration_minutes": 1, "target": "companies", "company_ids": [acme],
            }),
        )
        .await;
        let start = now_ms();
        alerts::evaluate(&app.state, start).await.unwrap();
        alerts::evaluate(&app.state, start + 2 * MINUTE_MS)
            .await
            .unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 1, "{name}");
        assert_eq!(open[0]["device_id"], desk.device_id.as_str());
        assert_eq!(open[0]["company_name"], "Acme");
        assert_eq!(
            alerts_of(&mut admin, &format!("?company={acme}"))
                .await
                .len(),
            1
        );
        assert!(alerts_of(&mut admin, "?company=none").await.is_empty());

        // Moving the device out of the company leaves the rule's scope.
        admin
            .put(
                &format!("/v1/agents/{}/company", desk.device_id),
                json!({ "company_id": null }),
            )
            .await;
        alerts::evaluate(&app.state, start + 3 * MINUTE_MS)
            .await
            .unwrap();
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");
        let _ = other;

        // Deleting a device deletes its alerts.
        let deleted = admin
            .delete(&format!("/v1/agents/{}", desk.device_id))
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert!(alerts_of(&mut admin, "?state=all").await.is_empty());
        app.finish().await;
    }
}

#[tokio::test]
async fn script_failures_raise_alerts_that_people_resolve() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "Desk").await;
        let _connection = app.state.agents.connect(&agent.device_id);
        let mut hook = Webhook::start(StatusCode::NO_CONTENT).await;
        let channel = create_webhook(&mut admin, &hook.url).await;
        let rule = create_rule(
            &mut admin,
            json!({
                "name": "Script failures", "severity": "warning", "condition": "script_failed",
                "nonzero_exit_fails": true, "channel_ids": [channel],
            }),
        )
        .await;
        let script = admin
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Backup", "language": "cmd", "body": "exit 3", "timeout_seconds": 10 }),
            )
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let runs_path = format!("/v1/agents/{}/script-runs", agent.device_id);
        let succeeded = start_run(&mut admin, &runs_path, &script).await;
        let report = |status: &str, exit_code: Option<i32>| json!({ "status": status, "ran_as": "SYSTEM", "exit_code": exit_code, "stdout": "", "stderr": "" });
        agent
            .report(
                &format!("script-runs/{succeeded}/result"),
                report("completed", Some(0)),
            )
            .await;
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");

        let failed = start_run(&mut admin, &runs_path, &script).await;
        let reported = agent
            .report(
                &format!("script-runs/{failed}/result"),
                report("completed", Some(3)),
            )
            .await;
        assert_eq!(reported.status, StatusCode::NO_CONTENT);
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 1, "{name}");
        assert_eq!(
            open[0]["detail"],
            "\u{201c}Backup\u{201d} exited with code 3"
        );
        assert_eq!(open[0]["script_run_id"], failed.as_str());
        assert_eq!(open[0]["value"], 3.0);
        let (_, raised) = hook.next().await;
        assert_eq!(raised["summary"], "Warning: Script failures on Desk");

        // Another failure is counted on the same alert without notifying.
        let timed_out = start_run(&mut admin, &runs_path, &script).await;
        agent
            .report(
                &format!("script-runs/{timed_out}/result"),
                report("timed_out", None),
            )
            .await;
        // And one the Agent never reports is found lost.
        let lost = start_run(&mut admin, &runs_path, &script).await;
        alerts::evaluate(&app.state, now_ms() + 3 * MINUTE_MS)
            .await
            .unwrap();
        let open = alerts_of(&mut admin, "").await;
        assert_eq!(open.len(), 1, "{name}");
        assert_eq!(open[0]["occurrences"], 3);
        assert_eq!(
            open[0]["detail"],
            "\u{201c}Backup\u{201d} never reported a result"
        );
        assert_eq!(open[0]["script_run_id"], lost.as_str());
        let run = admin.get(&format!("/v1/script-runs/{lost}")).await.body;
        assert_eq!(run["status"], "lost");
        hook.quiet().await;

        // It stays until someone resolves it.
        alerts::evaluate(&app.state, now_ms() + 4 * MINUTE_MS)
            .await
            .unwrap();
        let id = open[0]["id"].as_str().unwrap();
        assert_eq!(alerts_of(&mut admin, "").await.len(), 1);
        let resolved = admin
            .post(&format!("/v1/alerts/{id}/resolve"), json!({}))
            .await;
        assert_eq!(resolved.body["state"], "resolved");
        assert_eq!(
            admin
                .post(&format!("/v1/alerts/{id}/resolve"), json!({}))
                .await
                .status,
            StatusCode::CONFLICT
        );

        // Without the exit code option, only runs that couldn't finish count.
        let updated = admin
            .put(
                &format!("/v1/alert-rules/{rule}"),
                json!({ "name": "Script failures", "severity": "warning", "condition": "script_failed", "channel_ids": [channel] }),
            )
            .await;
        assert_eq!(updated.status, StatusCode::OK);
        let failed = start_run(&mut admin, &runs_path, &script).await;
        agent
            .report(
                &format!("script-runs/{failed}/result"),
                report("completed", Some(1)),
            )
            .await;
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn alerts_need_their_permissions() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let rule = json!({ "name": "Offline", "severity": "info", "condition": "device_offline", "duration_minutes": 5 });
        let rule_id = create_rule(&mut admin, rule.clone()).await;
        let (mut technician, _) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;

        assert_eq!(
            technician.get("/v1/alerts").await.status,
            StatusCode::OK,
            "{name}"
        );
        assert_eq!(
            technician.get("/v1/alert-rules").await.status,
            StatusCode::OK
        );
        assert_eq!(
            technician
                .post("/v1/alert-rules", rule.clone())
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            technician
                .put(&format!("/v1/alert-rules/{rule_id}"), rule.clone())
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            technician.get("/v1/notification-channels").await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(viewer.get("/v1/alerts").await.status, StatusCode::FORBIDDEN);
        assert_eq!(
            viewer.get("/v1/alerts/counts").await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer.get("/v1/alert-rules").await.status,
            StatusCode::FORBIDDEN
        );

        // A rule's condition can't change, and names are unique.
        let changed = admin
            .put(
                &format!("/v1/alert-rules/{rule_id}"),
                json!({ "name": "Offline", "severity": "info", "condition": "script_failed" }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::BAD_REQUEST);
        let duplicate = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "offline", "severity": "info", "condition": "script_failed" }),
            )
            .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        let unknown_channel = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "S", "severity": "info", "condition": "script_failed", "channel_ids": [uuid::Uuid::new_v4().to_string()] }),
            )
            .await;
        assert_eq!(unknown_channel.status, StatusCode::NOT_FOUND);
        app.finish().await;
    }
}

#[tokio::test]
async fn channels_email_users_and_roles_and_test_webhooks() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let email = json!({ "name": "Admins", "kind": "email", "role_ids": ["administrator"] });
        let unavailable = admin.post("/v1/notification-channels", email.clone()).await;
        assert_eq!(unavailable.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(unavailable.code(), "email_unavailable");
        assert_eq!(
            admin.get("/v1/notification-channels").await.body["email_available"],
            false
        );

        // Invited before email is set up, so the invitation is a link.
        let (_, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let mut smtp = FakeSmtp::start().await;
        let configured = admin
            .put(
                "/v1/settings/smtp",
                json!({ "host": "127.0.0.1", "port": smtp.port, "security": "none", "from": "Acme IT <rmm@example.com>" }),
            )
            .await;
        assert_eq!(configured.status, StatusCode::OK);
        let created = admin
            .post(
                "/v1/notification-channels",
                json!({ "name": "Admins", "kind": "email", "role_ids": ["administrator"], "user_ids": [tech_id] }),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let email_id = created.body["id"].as_str().unwrap().to_owned();
        let tested = admin
            .post(
                &format!("/v1/notification-channels/{email_id}/test"),
                json!({}),
            )
            .await;
        assert_eq!(
            tested.body,
            json!({ "delivered": true, "error": null }),
            "{name}"
        );
        let mut recipients = [smtp.next().await, smtp.next().await];
        recipients.sort();
        assert!(
            recipients[0].contains("To: admin@example.com"),
            "{}",
            recipients[0]
        );
        assert!(
            recipients[1].contains("To: tech@example.com"),
            "{}",
            recipients[1]
        );
        assert!(recipients[0].contains("Subject: Test notification from Acme IT"));
        let listed = admin.get("/v1/notification-channels").await.body;
        assert_eq!(listed["email_available"], true);
        assert!(listed["channels"][0]["last_delivery_at"].is_i64());
        assert!(listed["channels"][0]["last_delivery_error"].is_null());
        assert_eq!(
            admin
                .post(
                    "/v1/notification-channels",
                    json!({ "name": "Nobody", "kind": "email" }),
                )
                .await
                .status,
            StatusCode::BAD_REQUEST
        );

        let mut hook = Webhook::start(StatusCode::OK).await;
        let webhook = create_webhook(&mut admin, &hook.url).await;
        let tested = admin
            .post(
                &format!("/v1/notification-channels/{webhook}/test"),
                json!({}),
            )
            .await;
        assert_eq!(tested.body["delivered"], true, "{name}: {:?}", tested.body);
        let (headers, body) = hook.next().await;
        assert!(headers.contains_key("x-meshrmm-signature"));
        assert_eq!(body["event"], "test");
        // Keeping the secret, then removing it.
        let kept = admin
            .put(
                &format!("/v1/notification-channels/{webhook}"),
                json!({ "name": "On-call", "kind": "webhook", "webhook_url": hook.url }),
            )
            .await;
        assert_eq!(kept.body["has_secret"], true, "{:?}", kept.body);
        let removed = admin
            .put(
                &format!("/v1/notification-channels/{webhook}"),
                json!({ "name": "On-call", "kind": "webhook", "webhook_url": hook.url, "webhook_secret": null }),
            )
            .await;
        assert_eq!(removed.body["has_secret"], false);
        admin
            .post(
                &format!("/v1/notification-channels/{webhook}/test"),
                json!({}),
            )
            .await;
        let (headers, _) = hook.next().await;
        assert!(!headers.contains_key("x-meshrmm-signature"));

        // A failing receiver is reported, and the URL stays out of the audit log.
        let broken = Webhook::start(StatusCode::BAD_REQUEST).await;
        let failing = admin
            .post(
                "/v1/notification-channels",
                json!({ "name": "Broken", "kind": "webhook", "webhook_url": format!("{}?token=secret", broken.url) }),
            )
            .await;
        let failing = failing.body["id"].as_str().unwrap().to_owned();
        let tested = admin
            .post(
                &format!("/v1/notification-channels/{failing}/test"),
                json!({}),
            )
            .await;
        assert_eq!(tested.body["delivered"], false);
        assert_eq!(tested.body["error"], "the webhook answered 400 Bad Request");
        let created = audit_events(&app, "notification_channel.create").await;
        assert_eq!(created[0].metadata["webhook_host"], "127.0.0.1");
        assert!(!created[0].metadata.to_string().contains("token=secret"));
        assert_eq!(
            admin
                .delete(&format!("/v1/notification-channels/{failing}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let recipients = admin.get("/v1/notification-channels/recipients").await.body;
        assert_eq!(recipients["users"].as_array().unwrap().len(), 2);
        assert!(recipients["roles"].as_array().unwrap().len() >= 2);
        app.finish().await;
    }
}

/// Update and application rules, judged from what devices' scans found,
/// which the engine keeps and reads again only for devices that reported.
mod reported_scans {
    use meshrmm_protocol_types::{ApplicationScan, UpdateScan};
    use meshrmm_server::{applications, time::DAY_MS, updates};

    use super::*;

    fn security_rule() -> Value {
        json!({ "name": "Old security updates", "severity": "warning", "condition": "updates_pending", "update_category": "security", "days": 7 })
    }

    fn restart_rule() -> Value {
        json!({ "name": "Restart pending", "severity": "info", "condition": "reboot_pending", "days": 3 })
    }

    fn stale_apps_rule() -> Value {
        json!({ "name": "Stale apps", "severity": "warning", "condition": "app_updates_pending", "days": 7 })
    }

    /// A scan finding `updates`, each an ID and a category.
    fn update_scan(updates: &[(&str, &str)], reboot_pending: bool) -> UpdateScan {
        let updates = updates
            .iter()
            .map(|(id, category)| json!({ "id": id, "title": format!("Update {id}"), "category": category }))
            .collect::<Vec<_>>();
        serde_json::from_value(json!({
            "scanned_at_unix_ms": 1_700_000_000_000_i64,
            "manager": "windows_update",
            "updates": updates,
            "reboot_pending": reboot_pending,
        }))
        .unwrap()
    }

    /// A winget scan finding `packages`, each an ID and whether a newer
    /// version is offered.
    fn application_scan(packages: &[(&str, bool)]) -> ApplicationScan {
        let packages = packages
            .iter()
            .map(|(id, outdated)| {
                let mut package =
                    json!({ "source": "winget", "id": id, "name": id, "version": "1.0" });
                if *outdated {
                    package["available_version"] = "2.0".into();
                }
                package
            })
            .collect::<Vec<_>>();
        serde_json::from_value(json!({
            "scanned_at_unix_ms": 1_700_000_000_000_i64,
            "managers": [{ "source": "winget" }],
            "packages": packages,
        }))
        .unwrap()
    }

    /// Makes every device's pending updates look `days` old, without telling
    /// the engine.
    async fn age_updates(app: &App, days: i64) {
        app.db()
            .execute(
                &sea_query::Query::update()
                    .table(sea_query::Alias::new("device_pending_updates"))
                    .value(
                        sea_query::Alias::new("first_seen_at"),
                        now_ms() - days * DAY_MS,
                    )
                    .to_owned(),
            )
            .await
            .unwrap();
    }

    /// Waits until the running engine has finished an evaluation.
    async fn evaluated(app: &App) {
        for _ in 0..200 {
            if app.state.alerts.evaluations() > 0 {
                // Waits for the evaluation in progress.
                app.state.alerts.facts_kept().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the engine never evaluated");
    }

    /// Waits until `count` alerts are open, for the running engine.
    async fn open_until(admin: &mut Browser, count: usize) -> Vec<Value> {
        let mut open = Vec::new();
        for _ in 0..600 {
            open = alerts_of(admin, "?state=open").await;
            if open.len() >= count {
                return open;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{} of {count} alerts opened", open.len());
    }

    const BURST: usize = 50;

    #[tokio::test]
    async fn reports_in_a_burst_are_judged_together() {
        for app in apps().await {
            let name = app.name;
            let mut admin = set_up(&app).await;
            create_rule(&mut admin, security_rule()).await;
            let mut devices = Vec::new();
            for index in 0..BURST {
                let device = app.enroll(&mut admin, &format!("DESK-{index}")).await;
                updates::scan_reported(
                    &app.state,
                    &device.device_id,
                    Vec::new(),
                    update_scan(&[("KB1", "other")], false),
                )
                .await;
                devices.push(device.device_id);
            }
            age_updates(&app, 8).await;
            let engine = alerts::spawn(app.state.clone());
            evaluated(&app).await;
            assert!(
                alerts_of(&mut admin, "?state=open").await.is_empty(),
                "{name}"
            );

            // Each device's scan says the week-old update is a security one,
            // one after another.
            let before = app.state.alerts.evaluations();
            for device in &devices {
                updates::scan_reported(
                    &app.state,
                    device,
                    Vec::new(),
                    update_scan(&[("KB1", "security")], false),
                )
                .await;
            }
            let open = open_until(&mut admin, BURST).await;
            let evaluations = app.state.alerts.evaluations() - before;
            engine.abort();
            assert_eq!(open.len(), BURST, "{name}");
            assert!(
                evaluations <= 10,
                "{name}: {BURST} reports took {evaluations} evaluations"
            );
            app.finish().await;
        }
    }

    /// Reports three pending updates (the latest of `round`), a restart, and
    /// two outdated applications (one of `round`) of three.
    async fn report(app: &App, device_id: &str, round: usize) {
        let latest = format!("KB-{round}");
        updates::scan_reported(
            &app.state,
            device_id,
            Vec::new(),
            update_scan(
                &[("KB1", "security"), ("KB2", "other"), (&latest, "other")],
                true,
            ),
        )
        .await;
        let tool = format!("Tool.{round}");
        applications::scan_reported(
            &app.state,
            device_id,
            Vec::new(),
            application_scan(&[("Git.Git", true), (&tool, true), ("7zip.7zip", false)]),
        )
        .await;
    }

    /// What an evaluation reads: its statements, and the rows of devices'
    /// scans.
    async fn evaluation(app: &App) -> (u64, u64) {
        let before = (app.db().counts(), app.state.alerts.fact_rows_read());
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        (
            (app.db().counts() - before.0).statements,
            app.state.alerts.fact_rows_read() - before.1,
        )
    }

    /// The rows of one device's scans: three pending updates, its restart
    /// and that it scanned; that it scanned applications, and two outdated.
    const DEVICE_ROWS: u64 = 3 + 1 + 1 + 1 + 2;
    /// The statements reading reported devices' scans: three for updates,
    /// two for applications.
    const REPORTED_STATEMENTS: u64 = 5;

    #[tokio::test]
    async fn an_evaluation_reads_only_the_scans_of_devices_that_reported() {
        for app in apps().await {
            let name = app.name;
            let mut admin = set_up(&app).await;
            for rule in [security_rule(), restart_rule(), stale_apps_rule()] {
                create_rule(&mut admin, rule).await;
            }
            let mut devices = Vec::new();
            for index in 0..12 {
                let device = app.enroll(&mut admin, &format!("DESK-{index}")).await;
                devices.push(device.device_id);
            }
            for device in &devices[..2] {
                report(&app, device, 0).await;
            }
            // The engine just started: it reads every device's scans.
            let (_, started) = evaluation(&app).await;
            assert_eq!(started, 2 * DEVICE_ROWS, "{name}");
            let (few_quiet, rows) = evaluation(&app).await;
            assert_eq!(rows, 0, "{name}: nothing reported, nothing read");
            report(&app, &devices[0], 1).await;
            let few = evaluation(&app).await;
            assert_eq!(few.1, DEVICE_ROWS, "{name}: the device that reported");
            assert_eq!(few.0, few_quiet + REPORTED_STATEMENTS, "{name}");

            // Ten more devices with as much pending change neither.
            for device in &devices[2..] {
                report(&app, device, 0).await;
            }
            assert_eq!(evaluation(&app).await.1, 10 * DEVICE_ROWS, "{name}");
            let (many_quiet, rows) = evaluation(&app).await;
            assert_eq!((many_quiet, rows), (few_quiet, 0), "{name}");
            report(&app, &devices[0], 2).await;
            let many = evaluation(&app).await;
            assert_eq!(many, few, "{name}: one device reported");
            assert!(
                alerts_of(&mut admin, "?state=open").await.is_empty(),
                "{name}: nothing is old yet"
            );
            assert_eq!(app.state.alerts.facts_kept().await, (12, 12), "{name}");
            app.finish().await;
        }
    }

    #[tokio::test]
    async fn kept_scans_raise_and_resolve_as_the_stored_ones_do() {
        for app in apps().await {
            let name = app.name;
            let mut admin = set_up(&app).await;
            let security = create_rule(&mut admin, security_rule()).await;
            let desk = app.enroll(&mut admin, "Desk").await.device_id;
            updates::scan_reported(
                &app.state,
                &desk,
                Vec::new(),
                update_scan(&[("KB1", "security")], true),
            )
            .await;
            applications::scan_reported(
                &app.state,
                &desk,
                Vec::new(),
                application_scan(&[("Git.Git", true)]),
            )
            .await;
            let now = now_ms();
            alerts::evaluate(&app.state, now).await.unwrap();
            assert!(
                alerts_of(&mut admin, "?state=open").await.is_empty(),
                "{name}"
            );
            assert_eq!(app.state.alerts.facts_kept().await, (1, 0), "{name}");

            // A week passing raises it, with no report and nothing read.
            let read = app.state.alerts.fact_rows_read();
            alerts::evaluate(&app.state, now + 8 * DAY_MS)
                .await
                .unwrap();
            let open = alerts_of(&mut admin, "?state=open").await;
            assert_eq!(open.len(), 1, "{name}: {open:?}");
            assert_eq!(open[0]["rule_id"], security);
            assert_eq!(app.state.alerts.fact_rows_read(), read, "{name}");

            // Rules saved later judge what was reported before.
            let restart = create_rule(&mut admin, restart_rule()).await;
            let stale = create_rule(&mut admin, stale_apps_rule()).await;
            alerts::evaluate(&app.state, now + 8 * DAY_MS)
                .await
                .unwrap();
            let open = alerts_of(&mut admin, "?state=open").await;
            assert_eq!(open.len(), 3, "{name}: {open:?}");
            assert_eq!(app.state.alerts.facts_kept().await, (1, 1), "{name}");

            // Scans that report them gone resolve them.
            updates::scan_reported(&app.state, &desk, Vec::new(), update_scan(&[], false)).await;
            applications::scan_reported(
                &app.state,
                &desk,
                Vec::new(),
                application_scan(&[("Git.Git", false)]),
            )
            .await;
            alerts::evaluate(&app.state, now + 8 * DAY_MS)
                .await
                .unwrap();
            let open = alerts_of(&mut admin, "?state=open").await;
            assert!(open.is_empty(), "{name}: {open:?}");

            // Without update rules, the engine keeps no update scans.
            for rule in [&security, &restart] {
                let deleted = admin.delete(&format!("/v1/alert-rules/{rule}")).await;
                assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
            }
            alerts::evaluate(&app.state, now + 8 * DAY_MS)
                .await
                .unwrap();
            assert_eq!(app.state.alerts.facts_kept().await, (0, 1), "{name}");

            // A deleted device's alerts and scans go.
            applications::scan_reported(
                &app.state,
                &desk,
                Vec::new(),
                application_scan(&[("Git.Git", true)]),
            )
            .await;
            alerts::evaluate(&app.state, now + 16 * DAY_MS)
                .await
                .unwrap();
            let open = alerts_of(&mut admin, "?state=open").await;
            assert_eq!(open.len(), 1, "{name}: {open:?}");
            assert_eq!(open[0]["rule_id"], stale);
            let deleted = admin.delete(&format!("/v1/agents/{desk}")).await;
            assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
            alerts::evaluate(&app.state, now + 16 * DAY_MS)
                .await
                .unwrap();
            assert!(
                alerts_of(&mut admin, "?state=all").await.is_empty(),
                "{name}"
            );
            assert_eq!(app.state.alerts.facts_kept().await, (0, 0), "{name}");
            app.finish().await;
        }
    }

    #[tokio::test]
    async fn a_saved_rule_is_judged_at_once() {
        for app in apps().await {
            let name = app.name;
            let mut admin = set_up(&app).await;
            let desk = app.enroll(&mut admin, "Desk").await.device_id;
            // Stored as a report would be, without telling the engine.
            updates::store::store_scan(
                &app.state.database,
                &desk,
                update_scan(&[("KB1", "security")], false),
                now_ms(),
            )
            .await
            .unwrap();
            age_updates(&app, 8).await;
            let engine = alerts::spawn(app.state.clone());
            evaluated(&app).await;
            let before = app.state.alerts.evaluations();

            // The next evaluation would come after the interval; saving the
            // rule starts one at once.
            let saved = std::time::Instant::now();
            create_rule(&mut admin, security_rule()).await;
            let open = open_until(&mut admin, 1).await;
            let waited = saved.elapsed();
            let evaluations = app.state.alerts.evaluations() - before;
            engine.abort();
            assert_eq!(open.len(), 1, "{name}");
            assert_eq!(evaluations, 1, "{name}");
            assert!(
                waited < alerts::EVALUATION_INTERVAL / 2,
                "{name}: judged after {waited:?}"
            );
            app.finish().await;
        }
    }
}
