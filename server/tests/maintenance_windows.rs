//! Maintenance windows: putting devices in maintenance and taking them out,
//! the alerts and self-healing they hold back, the scheduled automation
//! they skip, policies that set them, and who may do so.
mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{
    App, Browser, apps, audit_events, job_with_devices, receive_any_json, send_json, set_up,
    user_with,
};
use meshrmm_protocol_types::{AgentCommand, MetricsReport, SystemMetrics, VolumeUsage};
use meshrmm_server::{
    alerts, automation, maintenance_windows, monitors,
    time::{DAY_MS, HOUR_MS, MINUTE_MS, SECOND_MS, now_ms},
};
use serde_json::{Value, json};

const GIB: u64 = 1024 * 1024 * 1024;

fn reading(free_gib: u64) -> SystemMetrics {
    SystemMetrics {
        cpu_percent: 5.0,
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

/// Reports the device's disk with `free_gib` of 100 GB free. The readings
/// before are forgotten, as the server drops one that comes too soon after
/// the last.
fn disk(app: &App, device_id: &str, free_gib: u64) {
    app.state.metrics.disconnected(device_id);
    app.state
        .metrics
        .record(device_id, MetricsReport::new(reading(free_gib), true));
}

async fn create_rule(admin: &mut Browser, rule: Value) -> String {
    let created = admin.post("/v1/alert-rules", rule).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn low_disk_rule(admin: &mut Browser) -> String {
    create_rule(
        admin,
        json!({
            "name": "Low disk", "severity": "warning", "condition": "disk_space",
            "threshold": 10, "threshold_unit": "percent",
        }),
    )
    .await
}

async fn alerts_of(browser: &mut Browser, query: &str) -> Vec<Value> {
    let listed = browser.get(&format!("/v1/alerts{query}")).await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    listed.body["alerts"].as_array().unwrap().clone()
}

/// The open alerts on `device_id`.
async fn open_on(browser: &mut Browser, device_id: &str) -> Vec<Value> {
    alerts_of(browser, "?state=open")
        .await
        .into_iter()
        .filter(|alert| alert["device_id"] == device_id)
        .collect()
}

/// Puts devices in maintenance, as `browser`.
async fn start(browser: &mut Browser, body: Value) -> Value {
    let started = browser.post("/v1/maintenance-windows", body).await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
    started.body
}

/// A window on the devices `ids` for `minutes`, starting now.
fn on_devices(ids: &[&str], reason: &str, minutes: i64) -> Value {
    json!({
        "target": "devices", "device_ids": ids, "reason": reason,
        "duration_minutes": minutes,
    })
}

async fn windows(browser: &mut Browser, query: &str) -> Vec<Value> {
    let listed = browser
        .get(&format!("/v1/maintenance-windows{query}"))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    listed.body["windows"].as_array().unwrap().clone()
}

/// The device as the device list shows it.
async fn listed_device(browser: &mut Browser, device_id: &str) -> Value {
    let listed = browser.get("/v1/agents").await.body;
    listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == device_id)
        .unwrap_or_else(|| panic!("{device_id} is not in {listed}"))
        .clone()
}

async fn tick(app: &App, now: i64) {
    maintenance_windows::tick(&app.state, now).await.unwrap();
}

async fn evaluate(app: &App, now: i64) {
    alerts::evaluate(&app.state, now).await.unwrap();
}

async fn create_script(browser: &mut Browser, name: &str) -> String {
    let created = browser
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": name, "language": "cmd", "body": "exit 3", "timeout_seconds": 60, "shared": true }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn create_policy(browser: &mut Browser, body: Value) -> Value {
    let created = browser.post("/v1/automation-policies", body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body
}

async fn set_next_run(app: &App, policy_id: &str, at: i64) {
    use meshrmm_server::db::tables::AutomationPolicies;
    use sea_query::{Expr, ExprTrait, Query};
    app.db()
        .execute(
            &Query::update()
                .table(AutomationPolicies::Table)
                .value(AutomationPolicies::NextRunAt, at)
                .and_where(Expr::col(AutomationPolicies::Id).eq(policy_id))
                .to_owned(),
        )
        .await
        .unwrap();
}

async fn policy_jobs(browser: &mut Browser, policy_id: &str) -> Vec<Value> {
    browser
        .get(&format!("/v1/jobs?policy_id={policy_id}"))
        .await
        .body["jobs"]
        .as_array()
        .unwrap()
        .clone()
}

async fn listed_policy(browser: &mut Browser, id: &str) -> Value {
    browser.get("/v1/automation-policies").await.body["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|listed| listed["id"] == id)
        .unwrap()
        .clone()
}

/// The device's entry in a job.
fn job_device<'a>(job: &'a Value, device_id: &str) -> &'a Value {
    job["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["device_id"] == device_id)
        .unwrap_or_else(|| panic!("{device_id} is not in {job}"))
}

fn is_maintenance_skip(entry: &Value, reason: &str) -> bool {
    entry["outcome"] == "skipped"
        && entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("In maintenance") && detail.ends_with(reason))
}

/// Tells the server the device has one pending security update.
async fn has_an_update(app: &App, device_id: &str) {
    meshrmm_server::updates::store::store_scan(
        app.db(),
        device_id,
        serde_json::from_value(json!({
            "scanned_at_unix_ms": 1_700_000_000_000_i64,
            "manager": "windows_update",
            "updates": [{
                "id": "kb1", "title": "KB1", "version": "2.0", "current_version": "1.0",
                "category": "security", "reboot_required": false,
            }],
            "reboot_pending": false,
        }))
        .unwrap(),
        now_ms(),
    )
    .await
    .unwrap();
}

/// The next command a stand-in Agent is sent.
async fn next_command(connection: &mut meshrmm_server::realtime::AgentConnection) -> AgentCommand {
    loop {
        let command =
            tokio::time::timeout(Duration::from_secs(10), common::next_command(connection))
                .await
                .expect("no command arrived")
                .expect("the connection was replaced");
        if !matches!(
            command,
            AgentCommand::SetMonitors { .. } | AgentCommand::CollectSystemInfo
        ) {
            return command;
        }
    }
}

#[tokio::test]
async fn windows_start_change_and_end_and_show_on_the_device() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let mut events = admin.events(&server).await.unwrap();
        let snapshot = receive_any_json(&mut events).await;
        assert_eq!(snapshot["type"], "snapshot", "{name}");

        // Requests that don't make a window.
        for invalid in [
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "  " }),
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "x".repeat(501) }),
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "Disk",
                    "duration_minutes": 30, "ends_at": now_ms() + HOUR_MS }),
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "Disk",
                    "starts_at": now_ms() - HOUR_MS }),
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "Disk",
                    "starts_at": now_ms() + HOUR_MS, "ends_at": now_ms() + 30 * MINUTE_MS }),
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "Disk",
                    "duration_minutes": 0 }),
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "Disk",
                    "starts_at": now_ms() + 400 * DAY_MS }),
            json!({ "target": "outdated", "reason": "Disk" }),
        ] {
            let refused = admin.post("/v1/maintenance-windows", invalid.clone()).await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}: {invalid}");
            assert_eq!(refused.code(), "invalid_maintenance", "{name}: {invalid}");
        }
        let missing = admin
            .post(
                "/v1/maintenance-windows",
                on_devices(&["00000000-0000-4000-8000-000000000000"], "Disk", 30),
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{name}");

        // Desk goes into maintenance now, for an hour.
        let before = now_ms();
        let started = start(
            &mut admin,
            on_devices(&[&desk.device_id], "Replacing the disk", 60),
        )
        .await;
        assert_eq!(started["created"], 1, "{name}");
        let window = &started["windows"][0];
        assert_eq!(window["state"], "current");
        assert_eq!(window["device_name"], "Desk");
        assert_eq!(window["reason"], "Replacing the disk");
        assert_eq!(window["created_by"], "Ada Admin");
        assert_eq!(window["automation"], "skip", "automation skips by default");
        let starts_at = window["starts_at"].as_i64().unwrap();
        assert!((before..=now_ms()).contains(&starts_at), "{name}");
        assert_eq!(window["ends_at"], starts_at + HOUR_MS);
        let window_id = window["id"].as_str().unwrap().to_owned();
        let upsert = loop {
            let event = receive_any_json(&mut events).await;
            if event["type"] == "agent_upsert" && event["agent"]["id"] == desk.device_id {
                break event;
            }
        };
        let current = &upsert["agent"]["maintenance"]["current"];
        assert_eq!(current["reason"], "Replacing the disk", "{name}: {upsert}");
        assert_eq!(current["created_by"], "Ada Admin");
        assert_eq!(current["ends_at"], starts_at + HOUR_MS);
        assert_eq!(current["window_id"], window_id.as_str());
        let listed = listed_device(&mut admin, &desk.device_id).await;
        assert_eq!(
            listed["maintenance"]["current"]["reason"],
            "Replacing the disk"
        );
        assert!(
            listed_device(&mut admin, &laptop.device_id).await["maintenance"].is_null(),
            "{name}: a device not in maintenance shows none"
        );

        // Laptop is booked for tomorrow, until someone ends it.
        let tomorrow = now_ms() + DAY_MS;
        let booked = start(
            &mut admin,
            json!({
                "target": "devices", "device_ids": [laptop.device_id], "reason": "Reimage",
                "starts_at": tomorrow, "automation": "scans",
            }),
        )
        .await;
        let booked = &booked["windows"][0];
        assert_eq!(booked["state"], "upcoming", "{name}");
        assert_eq!(booked["ends_at"], Value::Null);
        assert_eq!(booked["automation"], "scans");
        let booked_id = booked["id"].as_str().unwrap().to_owned();
        let laptop_listed = listed_device(&mut admin, &laptop.device_id).await;
        assert!(laptop_listed["maintenance"]["current"].is_null(), "{name}");
        assert_eq!(laptop_listed["maintenance"]["next_starts_at"], tomorrow);

        // The lists.
        let open = windows(&mut admin, "").await;
        assert_eq!(
            open.iter().map(|window| &window["id"]).collect::<Vec<_>>(),
            [&json!(window_id), &json!(booked_id)],
            "{name}: open windows, by when they start"
        );
        assert_eq!(windows(&mut admin, "?state=current").await.len(), 1);
        assert_eq!(
            windows(&mut admin, "?state=upcoming").await[0]["id"],
            booked_id.as_str()
        );
        assert_eq!(
            windows(&mut admin, &format!("?device={}", laptop.device_id))
                .await
                .len(),
            1
        );
        assert!(windows(&mut admin, "?state=past").await.is_empty());
        let one = admin
            .get(&format!("/v1/maintenance-windows/{window_id}"))
            .await;
        assert_eq!(one.status, StatusCode::OK);
        assert_eq!(one.body["reason"], "Replacing the disk");
        assert_eq!(
            admin
                .get("/v1/maintenance-windows/00000000-0000-4000-8000-000000000000")
                .await
                .status,
            StatusCode::NOT_FOUND
        );

        // Its end and reason change.
        let longer = starts_at + 2 * HOUR_MS;
        let changed = admin
            .patch(
                &format!("/v1/maintenance-windows/{window_id}"),
                json!({ "ends_at": longer, "reason": "Replacing the disk and fan" }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["ends_at"], longer);
        assert_eq!(changed.body["reason"], "Replacing the disk and fan");
        let backwards = admin
            .patch(
                &format!("/v1/maintenance-windows/{window_id}"),
                json!({ "ends_at": starts_at - MINUTE_MS }),
            )
            .await;
        assert_eq!(backwards.code(), "invalid_maintenance", "{name}");
        let open_ended = admin
            .patch(
                &format!("/v1/maintenance-windows/{booked_id}"),
                json!({ "ends_at": tomorrow + 3 * HOUR_MS }),
            )
            .await;
        assert_eq!(open_ended.body["ends_at"], tomorrow + 3 * HOUR_MS);
        assert_eq!(
            listed_device(&mut admin, &desk.device_id).await["maintenance"]["current"]["ends_at"],
            longer,
            "{name}: presence follows the change"
        );

        // Ending it takes Desk out now; cancelling the booking removes it.
        let ended = admin
            .post(
                &format!("/v1/maintenance-windows/{window_id}/end"),
                json!({}),
            )
            .await;
        assert_eq!(ended.status, StatusCode::OK, "{:?}", ended.body);
        assert_eq!(ended.body["state"], "ended", "{name}");
        assert_eq!(ended.body["ended_by"], "Ada Admin");
        assert!(ended.body["ends_at"].as_i64().unwrap() <= now_ms());
        let again = admin
            .post(
                &format!("/v1/maintenance-windows/{window_id}/end"),
                json!({}),
            )
            .await;
        assert_eq!(again.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(again.code(), "window_over");
        let changed_after = admin
            .patch(
                &format!("/v1/maintenance-windows/{window_id}"),
                json!({ "reason": "Too late" }),
            )
            .await;
        assert_eq!(changed_after.code(), "window_over", "{name}");
        assert!(
            listed_device(&mut admin, &desk.device_id).await["maintenance"].is_null(),
            "{name}"
        );
        let cancelled = admin
            .post(
                &format!("/v1/maintenance-windows/{booked_id}/end"),
                json!({}),
            )
            .await;
        assert_eq!(cancelled.body["state"], "cancelled", "{name}");
        assert!(windows(&mut admin, "").await.is_empty());
        assert_eq!(windows(&mut admin, "?state=past").await.len(), 2);
        let on_desk = admin
            .get(&format!(
                "/v1/agents/{}/maintenance-windows",
                desk.device_id
            ))
            .await;
        assert_eq!(on_desk.status, StatusCode::OK);
        assert_eq!(on_desk.body["open"], json!([]));
        assert_eq!(on_desk.body["past"][0]["id"], window_id.as_str());

        // Every start, booking, change, end and cancellation is audited
        // against the device.
        for (action, device, count) in [
            ("maintenance_window.start", &desk.device_id, 1),
            ("maintenance_window.schedule", &laptop.device_id, 1),
            // Desk's window and then Laptop's booking changed.
            ("maintenance_window.update", &laptop.device_id, 2),
            ("maintenance_window.end", &desk.device_id, 1),
            ("maintenance_window.cancel", &laptop.device_id, 1),
        ] {
            let audited = audit_events(&app, action).await;
            assert_eq!(audited.len(), count, "{name}: {action}");
            assert_eq!(audited[0].target_type, "agent", "{action}");
            assert_eq!(&audited[0].target_id, device, "{action}");
            assert!(audited[0].actor_user_id.is_some(), "{action}");
        }
        let updated = &audit_events(&app, "maintenance_window.update").await[1];
        assert_eq!(updated.metadata["window_id"], window_id.as_str());
        assert_eq!(updated.metadata["reason"], "Replacing the disk and fan");
        let audited = &audit_events(&app, "maintenance_window.start").await[0];
        assert_eq!(audited.metadata["reason"], "Replacing the disk");
        assert_eq!(audited.metadata["name"], "Desk");
        drop(events);
        app.finish().await;
    }
}

#[tokio::test]
async fn device_alerts_hold_during_maintenance_and_are_judged_again_after() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let server = app.enroll(&mut admin, "Server").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let _desk = app.state.agents.connect(&desk.device_id);
        let _server = app.state.agents.connect(&server.device_id);
        low_disk_rule(&mut admin).await;
        create_rule(
            &mut admin,
            json!({ "name": "Offline", "severity": "critical", "condition": "device_offline", "duration_minutes": 10 }),
        )
        .await;

        // Desk's disk is full before its window.
        let now = now_ms();
        disk(&app, &desk.device_id, 4);
        disk(&app, &server.device_id, 50);
        evaluate(&app, now).await;
        let desk_alert = open_on(&mut admin, &desk.device_id).await;
        assert_eq!(desk_alert.len(), 1, "{name}");
        let desk_alert_id = desk_alert[0]["id"].as_str().unwrap().to_owned();

        // All three go into maintenance from a minute from now for an hour.
        let starts_at = now + MINUTE_MS;
        let ends_at = starts_at + HOUR_MS;
        let started = start(
            &mut admin,
            json!({
                "target": "devices",
                "device_ids": [desk.device_id, server.device_id, laptop.device_id],
                "reason": "Moving racks", "starts_at": starts_at, "ends_at": ends_at,
            }),
        )
        .await;
        assert_eq!(started["created"], 3, "{name}");
        tick(&app, starts_at).await;

        // During it: Desk's disk frees up, but its alert holds as it was;
        // Server's fills, and Laptop is offline long enough, but neither
        // raises an alert.
        disk(&app, &desk.device_id, 50);
        disk(&app, &server.device_id, 3);
        for at in [starts_at, starts_at + 15 * MINUTE_MS, ends_at - SECOND_MS] {
            evaluate(&app, at).await;
            let open = alerts_of(&mut admin, "?state=open").await;
            assert_eq!(open.len(), 1, "{name}: {open:?}");
            assert_eq!(open[0]["id"], desk_alert_id.as_str());
        }
        // People still look after the alerts that were open.
        let acknowledged = admin
            .post(
                &format!("/v1/alerts/{desk_alert_id}/acknowledge"),
                json!({}),
            )
            .await;
        assert_eq!(
            acknowledged.status,
            StatusCode::OK,
            "{:?}",
            acknowledged.body
        );

        // At its end everything is judged as it stands then.
        tick(&app, ends_at).await;
        evaluate(&app, ends_at).await;
        let resolved = alerts_of(&mut admin, "?state=resolved").await;
        assert_eq!(resolved.len(), 1, "{name}: {resolved:?}");
        assert_eq!(resolved[0]["id"], desk_alert_id.as_str());
        assert_eq!(resolved[0]["resolution"], "cleared");
        assert_eq!(
            open_on(&mut admin, &server.device_id).await.len(),
            1,
            "{name}"
        );
        let offline = open_on(&mut admin, &laptop.device_id).await;
        assert_eq!(offline.len(), 1, "{name}");
        assert!(
            offline[0]["detail"]
                .as_str()
                .unwrap()
                .starts_with("Offline for"),
            "{offline:?}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn failures_during_maintenance_raise_no_alerts() {
    for app in apps().await {
        let name = app.name;
        let ws_server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut control = desk.connect(&ws_server).await.unwrap();
        for rule in [
            json!({ "name": "Script failures", "severity": "warning", "condition": "script_failed", "nonzero_exit_fails": true }),
            json!({ "name": "Install failed", "severity": "critical", "condition": "update_install_failed" }),
            json!({ "name": "App failed", "severity": "critical", "condition": "app_install_failed" }),
        ] {
            create_rule(&mut admin, rule).await;
        }
        let script = create_script(&mut admin, "Backup").await;
        has_an_update(&app, &desk.device_id).await;
        let firefox = admin
            .post(
                "/v1/applications",
                json!({ "name": "Firefox", "sources": [{ "source": "winget", "package_id": "Mozilla.Firefox" }] }),
            )
            .await;
        assert_eq!(firefox.status, StatusCode::CREATED, "{:?}", firefox.body);
        let firefox = firefox.body["id"].as_str().unwrap().to_owned();
        send_json(
            &mut control,
            json!({ "type": "application_scan", "run_ids": [], "scan": {
                "scanned_at_unix_ms": 1_700_000_000_000_i64, "managers": [{ "source": "winget" }], "packages": [],
            } }),
        )
        .await;
        for _ in 0..100 {
            let packages = admin
                .get(&format!("/v1/agents/{}/packages", desk.device_id))
                .await;
            if !packages.body["status"].is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Fails a script run, an update install and an application install,
        // each checked against the rules once it is reported.
        let fail_all = async |admin: &mut Browser| {
            let run = admin
                .post(
                    &format!("/v1/agents/{}/script-runs", desk.device_id),
                    json!({ "script_id": script, "run_as": "system" }),
                )
                .await;
            assert_eq!(run.status, StatusCode::CREATED, "{:?}", run.body);
            let run = run.body["id"].as_str().unwrap().to_owned();
            desk.report(
                &format!("script-runs/{run}/result"),
                json!({ "status": "completed", "ran_as": "SYSTEM", "exit_code": 3, "stdout": "", "stderr": "" }),
            )
            .await;
            let install = admin
                .post(
                    &format!("/v1/agents/{}/updates/install", desk.device_id),
                    json!({ "reboot": "never" }),
                )
                .await;
            assert!(install.status.is_success(), "{:?}", install.body);
            let install = install.body["id"].as_str().unwrap().to_owned();
            desk.report(
                &format!("update-runs/{install}/result"),
                json!({ "status": "failed", "error": "The disk is full" }),
            )
            .await;
            alerts::update_install_finished(&app.state, &install).await;
            let application = admin
                .post(
                    &format!("/v1/agents/{}/application-runs", desk.device_id),
                    json!({ "action": "install", "application_ids": [firefox] }),
                )
                .await;
            assert!(application.status.is_success(), "{:?}", application.body);
            let application = application.body["id"].as_str().unwrap().to_owned();
            desk.report(
                &format!("application-runs/{application}/result"),
                json!({ "status": "failed", "items": [{ "index": 0, "name": "Firefox", "outcome": "failed",
                         "exit_code": 1, "error": "No applicable installer", "output": "" }] }),
            )
            .await;
            alerts::applications::run_finished(&app.state, &application).await;
        };

        // Manual actions still run in maintenance; their failures are
        // dropped, not saved for later.
        let started = start(&mut admin, on_devices(&[&desk.device_id], "Rebuild", 60)).await;
        let window_id = started["windows"][0]["id"].as_str().unwrap().to_owned();
        fail_all(&mut admin).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            alerts_of(&mut admin, "").await.is_empty(),
            "{name}: nothing raised in maintenance"
        );
        let ended = admin
            .post(
                &format!("/v1/maintenance-windows/{window_id}/end"),
                json!({}),
            )
            .await;
        assert_eq!(ended.status, StatusCode::OK);
        tick(&app, now_ms()).await;
        evaluate(&app, now_ms()).await;
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");

        // Out of maintenance, the same failures raise their alerts.
        fail_all(&mut admin).await;
        let mut raised = Vec::new();
        for _ in 0..100 {
            raised = alerts_of(&mut admin, "?state=open").await;
            if raised.len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(raised.len(), 3, "{name}: {raised:?}");
        drop(control);
        app.finish().await;
    }
}

fn service_rule() -> Value {
    json!({
        "name": "Web server", "severity": "critical", "condition": "service",
        "monitor": { "interval_seconds": 60, "check": { "kind": "service", "name": "nginx" } },
        "remediation": { "action": "restart_service", "attempts": 2, "retry_minutes": 5 },
    })
}

fn monitor_result(rule_id: &str, passing: bool) -> meshrmm_protocol_types::MonitorResult {
    meshrmm_protocol_types::MonitorResult {
        monitor_id: rule_id.to_owned(),
        checked_at_unix_ms: now_ms() as u64,
        passing,
        failures: u32::from(!passing),
        value: None,
        detail: if passing { "up" } else { "down" }.to_owned(),
        output: String::new(),
    }
}

#[tokio::test]
async fn monitors_and_self_healing_wait_for_maintenance_to_end() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let web = app.enroll_on(&mut admin, "Web", "linux").await;
        let device = web.device_id.as_str();
        let mut connection = app.state.agents.connect(device);
        let rule = create_rule(&mut admin, service_rule()).await;

        // A failing check during maintenance is kept but raises nothing.
        let started = start(&mut admin, on_devices(&[device], "Upgrading nginx", 60)).await;
        let window_id = started["windows"][0]["id"].as_str().unwrap().to_owned();
        monitors::reported(&app.state, device, vec![monitor_result(&rule, false)]).await;
        let listed = admin
            .get(&format!("/v1/agents/{device}/monitors"))
            .await
            .body;
        assert_eq!(listed[0]["state"]["passing"], false, "{name}: {listed}");
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");

        // Out of maintenance, the stored result is judged: the alert opens
        // and self-healing starts.
        admin
            .post(
                &format!("/v1/maintenance-windows/{window_id}/end"),
                json!({}),
            )
            .await;
        tick(&app, now_ms()).await;
        let open = alerts_of(&mut admin, "?state=open").await;
        assert_eq!(open.len(), 1, "{name}");
        let alert_id = open[0]["id"].as_str().unwrap().to_owned();
        assert!(matches!(
            next_command(&mut connection).await,
            AgentCommand::RunMonitorAction { .. }
        ));
        let attempt = &admin
            .get(&format!("/v1/alerts/{alert_id}/remediations"))
            .await
            .body[0];
        assert_eq!(attempt["status"], "running", "{name}");

        // A retry due during a later window waits for it to end.
        start(&mut admin, on_devices(&[device], "Upgrading again", 60)).await;
        let later = now_ms() + 10 * MINUTE_MS;
        monitors::remediation::tick(&app.state, later)
            .await
            .unwrap();
        let attempts = admin
            .get(&format!("/v1/alerts/{alert_id}/remediations"))
            .await
            .body;
        assert_eq!(
            attempts.as_array().unwrap().len(),
            1,
            "{name}: no attempt starts in maintenance"
        );
        drop(connection);
        app.finish().await;
    }
}

#[tokio::test]
async fn scheduled_runs_skip_devices_in_maintenance_and_say_so() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let _desk = app.state.agents.connect(&desk.device_id);
        let _laptop = app.state.agents.connect(&laptop.device_id);
        let script = create_script(&mut admin, "Cleanup").await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Hourly cleanup", "script_id": script, "run_as": "system",
                "target": "all", "trigger": "interval", "interval_minutes": 60, "offline": "wait",
            }),
        )
        .await;
        let policy_id = policy["id"].as_str().unwrap().to_owned();
        let scans = create_policy(
            &mut admin,
            json!({
                "name": "Hourly scan", "action": "update_scan",
                "target": "all", "trigger": "interval", "interval_minutes": 60, "offline": "wait",
            }),
        )
        .await;
        let scans_id = scans["id"].as_str().unwrap().to_owned();
        start(&mut admin, on_devices(&[&desk.device_id], "Disk swap", 180)).await;
        start(
            &mut admin,
            json!({
                "target": "devices", "device_ids": [laptop.device_id], "reason": "Testing",
                "duration_minutes": 180, "automation": "scans",
            }),
        )
        .await;

        // Due during the windows: the script run skips both devices; the
        // scan skips only Desk, Laptop's window letting scans run.
        let due = now_ms() + MINUTE_MS;
        set_next_run(&app, &policy_id, due).await;
        set_next_run(&app, &scans_id, due).await;
        automation::pass(&app.state, due).await.unwrap();
        let fired = policy_jobs(&mut admin, &policy_id).await;
        assert_eq!(fired.len(), 1, "{name}: the run happens, without them");
        let job = job_with_devices(&mut admin, fired[0]["id"].as_str().unwrap()).await;
        assert!(
            is_maintenance_skip(job_device(&job, &desk.device_id), ": Disk swap"),
            "{name}: {job}"
        );
        assert!(
            is_maintenance_skip(job_device(&job, &laptop.device_id), ": Testing"),
            "{name}: {job}"
        );
        let scan = &policy_jobs(&mut admin, &scans_id).await[0];
        let scan = job_with_devices(&mut admin, scan["id"].as_str().unwrap()).await;
        assert!(is_maintenance_skip(
            job_device(&scan, &desk.device_id),
            ": Disk swap"
        ));
        assert_ne!(
            job_device(&scan, &laptop.device_id)["outcome"],
            "skipped",
            "{name}: {scan}"
        );
        // Nothing is queued for later.
        automation::pass(&app.state, due + 4 * HOUR_MS)
            .await
            .unwrap();
        let job = job_with_devices(&mut admin, fired[0]["id"].as_str().unwrap()).await;
        assert_eq!(job_device(&job, &desk.device_id)["outcome"], "skipped");

        // Run now is a person's choice, and runs on them.
        let ran = admin
            .post(
                &format!("/v1/automation-policies/{policy_id}/run"),
                json!({}),
            )
            .await;
        assert_eq!(ran.status, StatusCode::CREATED, "{:?}", ran.body);
        let ran = job_with_devices(&mut admin, ran.body["id"].as_str().unwrap()).await;
        assert_ne!(
            job_device(&ran, &desk.device_id)["outcome"],
            "skipped",
            "{name}: {ran}"
        );
        // So is a job someone starts.
        let manual = admin
            .post(
                "/v1/jobs",
                json!({ "script_id": script, "run_as": "system", "target": "devices",
                        "device_ids": [desk.device_id], "offline": "skip" }),
            )
            .await;
        assert_eq!(manual.status, StatusCode::CREATED, "{:?}", manual.body);
        let manual = job_with_devices(&mut admin, manual.body["id"].as_str().unwrap()).await;
        assert_eq!(
            job_device(&manual, &desk.device_id)["outcome"],
            "queued",
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_scheduled_run_waiting_for_a_device_skips_it_if_it_connects_in_maintenance() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let script = create_script(&mut admin, "Cleanup").await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Hourly cleanup", "script_id": script, "run_as": "system",
                "target": "all", "trigger": "interval", "interval_minutes": 60, "offline": "wait",
            }),
        )
        .await;
        let policy_id = policy["id"].as_str().unwrap().to_owned();
        let due = now_ms();
        set_next_run(&app, &policy_id, due).await;
        automation::pass(&app.state, due).await.unwrap();
        let fired = policy_jobs(&mut admin, &policy_id).await;
        let job_id = fired[0]["id"].as_str().unwrap().to_owned();
        let job = job_with_devices(&mut admin, &job_id).await;
        assert_eq!(
            job_device(&job, &desk.device_id)["outcome"],
            "waiting",
            "{name}"
        );

        // It goes into maintenance, then connects.
        start(&mut admin, on_devices(&[&desk.device_id], "Repair", 60)).await;
        let _desk = app.state.agents.connect(&desk.device_id);
        for _ in 0..3 {
            automation::pass(&app.state, now_ms()).await.unwrap();
        }
        let job = job_with_devices(&mut admin, &job_id).await;
        assert!(
            is_maintenance_skip(job_device(&job, &desk.device_id), ": Repair"),
            "{name}: {job}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn maintenance_starts_and_ends_on_companies_tags_and_every_device() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let server = app.enroll(&mut admin, "Server").await;
        let spare = app.enroll(&mut admin, "Spare").await;
        let acme = admin.post("/v1/companies", json!({ "name": "Acme" })).await;
        let acme = acme.body["id"].as_str().unwrap().to_owned();
        for device in [&desk, &laptop] {
            let moved = admin
                .put(
                    &format!("/v1/agents/{}/company", device.device_id),
                    json!({ "company_id": acme }),
                )
                .await;
            assert!(moved.status.is_success(), "{:?}", moved.body);
        }
        let servers = admin
            .post("/v1/tags", json!({ "name": "Servers", "color": "blue" }))
            .await;
        let servers = servers.body["id"].as_str().unwrap().to_owned();
        let empty = admin
            .post("/v1/tags", json!({ "name": "Empty", "color": "red" }))
            .await;
        let empty = empty.body["id"].as_str().unwrap().to_owned();
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [server.device_id], "add": [servers], "remove": [] }),
            )
            .await;
        assert!(tagged.status.is_success(), "{:?}", tagged.body);

        let by_company = start(
            &mut admin,
            json!({ "target": "companies", "company_ids": [acme], "reason": "Office move", "duration_minutes": 120 }),
        )
        .await;
        assert_eq!(by_company["created"], 2, "{name}");
        let by_tag = start(
            &mut admin,
            json!({ "target": "tags", "tag_ids": [servers], "reason": "Patching", "duration_minutes": 120 }),
        )
        .await;
        assert_eq!(by_tag["created"], 1, "{name}");
        assert_eq!(by_tag["windows"][0]["device_name"], "Server");
        let nobody = admin
            .post(
                "/v1/maintenance-windows",
                json!({ "target": "tags", "tag_ids": [empty], "reason": "Nothing" }),
            )
            .await;
        assert_eq!(nobody.code(), "no_devices", "{name}");

        let current = windows(&mut admin, "?state=current").await;
        assert_eq!(current.len(), 3, "{name}");
        let in_acme = windows(&mut admin, &format!("?company={acme}")).await;
        assert_eq!(in_acme.len(), 2);
        assert!(
            in_acme
                .iter()
                .all(|window| window["company_name"] == "Acme")
        );
        let in_none = windows(&mut admin, "?company=none").await;
        assert_eq!(in_none.len(), 1);
        assert_eq!(in_none[0]["device_name"], "Server");
        let now = now_ms();
        for device in [&desk, &laptop, &server] {
            assert!(
                app.state
                    .maintenance_windows
                    .in_maintenance(&device.device_id, now),
                "{name}"
            );
        }
        assert!(
            !app.state
                .maintenance_windows
                .in_maintenance(&spare.device_id, now)
        );

        // Ending a company's maintenance leaves the rest.
        let ended = admin
            .post(
                "/v1/maintenance-windows/end",
                json!({ "target": "companies", "company_ids": [acme] }),
            )
            .await;
        assert_eq!(ended.status, StatusCode::OK, "{:?}", ended.body);
        assert_eq!(ended.body["ended"], 2, "{name}");
        assert_eq!(windows(&mut admin, "?state=current").await.len(), 1);

        // Every device is booked for tonight; ending all of them with what
        // is booked ends Server's and cancels the bookings.
        let tonight = now_ms() + 6 * HOUR_MS;
        let everyone = start(
            &mut admin,
            json!({ "target": "all", "reason": "Network upgrade", "starts_at": tonight, "duration_minutes": 60 }),
        )
        .await;
        assert_eq!(everyone["created"], 4, "{name}");
        let current_only = admin
            .post(
                "/v1/maintenance-windows/end",
                json!({ "target": "devices", "device_ids": [desk.device_id] }),
            )
            .await;
        assert_eq!(
            current_only.body["ended"], 0,
            "{name}: a booking stays unless asked"
        );
        let all = admin
            .post(
                "/v1/maintenance-windows/end",
                json!({ "target": "all", "upcoming": true }),
            )
            .await;
        assert_eq!(all.body["ended"], 5, "{name}: {:?}", all.body);
        assert!(windows(&mut admin, "").await.is_empty());
        assert_eq!(
            audit_events(&app, "maintenance_window.start").await.len(),
            3
        );
        assert_eq!(
            audit_events(&app, "maintenance_window.schedule")
                .await
                .len(),
            4
        );
        assert_eq!(audit_events(&app, "maintenance_window.end").await.len(), 3);
        assert_eq!(
            audit_events(&app, "maintenance_window.cancel").await.len(),
            4
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn maintenance_policies_set_windows_on_their_schedule() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let _desk = app.state.agents.connect(&desk.device_id);
        let script = create_script(&mut admin, "Cleanup").await;
        let invalid = [
            json!({ "name": "Short", "action": "maintenance", "maintenance_minutes": 2,
                    "target": "all", "trigger": "weekly", "weekdays": [7], "time": "02:00",
                    "time_zone": "UTC", "offline": "skip" }),
            json!({ "name": "Endless", "action": "maintenance",
                    "target": "all", "trigger": "weekly", "weekdays": [7], "time": "02:00",
                    "time_zone": "UTC", "offline": "skip" }),
            json!({ "name": "On enroll", "action": "maintenance", "maintenance_minutes": 60,
                    "target": "all", "trigger": "enroll", "offline": "skip" }),
        ];
        for body in invalid {
            let refused = admin.post("/v1/automation-policies", body.clone()).await;
            assert_eq!(refused.code(), "invalid_policy", "{name}: {body}");
        }

        // Every Sunday 02:00 to 05:00 UTC.
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Sunday maintenance", "action": "maintenance", "maintenance_minutes": 180,
                "maintenance_reason": "Weekly maintenance", "target": "all", "trigger": "weekly",
                "weekdays": [7], "time": "02:00", "time_zone": "UTC", "offline": "skip",
            }),
        )
        .await;
        assert_eq!(policy["action"], "maintenance", "{name}");
        assert_eq!(policy["maintenance_minutes"], 180);
        assert_eq!(policy["maintenance_reason"], "Weekly maintenance");
        assert_eq!(policy["maintenance_automation"], "skip");
        let id = policy["id"].as_str().unwrap().to_owned();
        let due = policy["next_run_at"].as_i64().unwrap();
        // A script policy due at the same time, which the windows skip.
        let cleanup = create_policy(
            &mut admin,
            json!({
                "name": "Cleanup", "script_id": script, "run_as": "system",
                "target": "all", "trigger": "interval", "interval_minutes": 60, "offline": "wait",
            }),
        )
        .await;
        let cleanup_id = cleanup["id"].as_str().unwrap().to_owned();
        set_next_run(&app, &cleanup_id, due + HOUR_MS).await;

        automation::pass(&app.state, due - SECOND_MS).await.unwrap();
        assert!(windows(&mut admin, "?state=all").await.is_empty(), "{name}");
        automation::pass(&app.state, due).await.unwrap();
        automation::pass(&app.state, due + SECOND_MS).await.unwrap();
        let set = windows(&mut admin, &format!("?state=all&policy={id}")).await;
        assert_eq!(set.len(), 2, "{name}: one window a device, once");
        for window in &set {
            assert_eq!(window["starts_at"], due);
            assert_eq!(window["ends_at"], due + 3 * HOUR_MS);
            assert_eq!(window["reason"], "Weekly maintenance");
            assert_eq!(
                window["created_by"],
                "policy \u{201c}Sunday maintenance\u{201d}"
            );
            assert_eq!(window["policy_name"], "Sunday maintenance");
        }
        assert!(
            app.state
                .maintenance_windows
                .in_maintenance(&laptop.device_id, due + HOUR_MS)
        );
        let listed = listed_policy(&mut admin, &id).await;
        assert_eq!(listed["last_run_at"], due, "{name}");
        assert_eq!(listed["next_run_at"], due + 7 * DAY_MS);
        assert_eq!(listed["last_maintenance"]["devices"], 2, "{listed}");
        assert_eq!(listed["last_maintenance"]["ends_at"], due + 3 * HOUR_MS);
        assert!(policy_jobs(&mut admin, &id).await.is_empty(), "no job");
        let audited = audit_events(&app, "maintenance_window.start").await;
        assert_eq!(audited.len(), 2, "{name}");
        assert_eq!(audited[0].actor_user_id, None);
        assert_eq!(
            audited[0].actor_label,
            "policy \u{201c}Sunday maintenance\u{201d}"
        );
        assert_eq!(audited[0].metadata["policy_id"], id.as_str());

        // The cleanup due during it skips both devices.
        automation::pass(&app.state, due + HOUR_MS).await.unwrap();
        let fired = policy_jobs(&mut admin, &cleanup_id).await;
        let job = job_with_devices(&mut admin, fired[0]["id"].as_str().unwrap()).await;
        assert!(
            is_maintenance_skip(job_device(&job, &desk.device_id), ": Weekly maintenance"),
            "{name}: {job}"
        );

        // At 05:00 they leave maintenance.
        tick(&app, due + 3 * HOUR_MS).await;
        assert!(
            !app.state
                .maintenance_windows
                .in_maintenance(&desk.device_id, due + 3 * HOUR_MS)
        );

        // A firing late after downtime still sets windows for what is left;
        // one too late for any is missed.
        let next = due + 7 * DAY_MS;
        automation::pass(&app.state, next + 2 * HOUR_MS)
            .await
            .unwrap();
        let late = windows(&mut admin, &format!("?state=all&policy={id}")).await;
        let late = late
            .iter()
            .filter(|window| window["starts_at"] == next + 2 * HOUR_MS)
            .collect::<Vec<_>>();
        assert_eq!(late.len(), 2, "{name}");
        assert_eq!(late[0]["ends_at"], next + 3 * HOUR_MS);
        let after = next + 7 * DAY_MS;
        automation::pass(&app.state, after + 3 * HOUR_MS)
            .await
            .unwrap();
        let listed = listed_policy(&mut admin, &id).await;
        assert!(
            listed["last_problem"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Missed its run"),
            "{name}: {listed}"
        );
        assert_eq!(
            windows(&mut admin, &format!("?state=all&policy={id}"))
                .await
                .len(),
            4
        );

        // Run now puts them in maintenance now, as the person who chose it.
        let ran = admin
            .post(&format!("/v1/automation-policies/{id}/run"), json!({}))
            .await;
        assert_eq!(ran.status, StatusCode::CREATED, "{:?}", ran.body);
        assert_eq!(ran.body["maintenance"]["devices"], 2, "{name}");
        let current = windows(&mut admin, &format!("?state=current&policy={id}")).await;
        assert_eq!(current.len(), 2, "{name}");
        assert_eq!(current[0]["created_by"], "Ada Admin");
        assert_eq!(
            audit_events(&app, "automation_policy.run").await[0].metadata["maintenance_windows"],
            2
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn installs_can_put_each_device_in_maintenance_while_they_run() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut connection = app.state.agents.connect(&desk.device_id);
        has_an_update(&app, &desk.device_id).await;
        low_disk_rule(&mut admin).await;
        create_rule(
            &mut admin,
            json!({ "name": "Install failed", "severity": "critical", "condition": "update_install_failed" }),
        )
        .await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Patch night", "action": "update_install", "reboot": "never",
                "maintenance_during_install": true, "target": "all", "trigger": "daily",
                "time": "03:00", "time_zone": "UTC", "offline": "wait",
            }),
        )
        .await;
        assert_eq!(policy["maintenance_during_install"], true, "{name}");
        let policy_id = policy["id"].as_str().unwrap().to_owned();
        let due = now_ms();
        set_next_run(&app, &policy_id, due).await;
        automation::pass(&app.state, due).await.unwrap();
        let job = &policy_jobs(&mut admin, &policy_id).await[0];
        assert_eq!(job["maintenance_during_install"], true, "{name}: {job}");
        automation::pass(&app.state, now_ms()).await.unwrap();
        let AgentCommand::InstallUpdates { install } = next_command(&mut connection).await else {
            panic!("expected an install");
        };
        let run_id = install.run_id;

        // The device is in maintenance while it installs.
        let now = now_ms();
        assert!(
            app.state
                .maintenance_windows
                .in_maintenance(&desk.device_id, now),
            "{name}"
        );
        let window = &windows(&mut admin, "?state=current").await[0];
        assert_eq!(window["update_run_id"], run_id.as_str(), "{name}");
        assert_eq!(window["policy_id"], policy_id.as_str());
        assert_eq!(window["created_by"], "policy \u{201c}Patch night\u{201d}");
        let audited = &audit_events(&app, "maintenance_window.start").await[0];
        assert_eq!(audited.actor_label, "policy \u{201c}Patch night\u{201d}");
        assert_eq!(audited.metadata["update_run_id"], run_id.as_str());
        disk(&app, &desk.device_id, 2);
        evaluate(&app, now).await;
        assert!(alerts_of(&mut admin, "").await.is_empty(), "{name}");

        // Its own failure still alerts, and its report ends the window.
        desk.report(
            &format!("update-runs/{run_id}/result"),
            json!({ "status": "failed", "error": "The disk is full" }),
        )
        .await;
        alerts::update_install_finished(&app.state, &run_id).await;
        let open = alerts_of(&mut admin, "?state=open").await;
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        assert_eq!(open[0]["update_run_id"], run_id.as_str());
        // The task notices the report at its next tick.
        let mut past = Vec::new();
        for _ in 0..100 {
            tick(&app, now_ms()).await;
            past = windows(&mut admin, "?state=past").await;
            if !past.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !app.state
                .maintenance_windows
                .in_maintenance(&desk.device_id, now_ms()),
            "{name}"
        );
        let past = &past[0];
        assert_eq!(past["state"], "ended", "{name}");
        assert_eq!(past["ended_by"], "The update install finished");
        evaluate(&app, now_ms()).await;
        assert_eq!(
            alerts_of(&mut admin, "?state=open").await.len(),
            2,
            "{name}: the full disk alerts once it is out"
        );

        // A person asks for it on a job, which needs the permission.
        let (mut installer, _) = user_with(
            &app,
            &mut admin,
            "installer@example.com",
            &["devices.view", "updates.view", "updates.install"],
        )
        .await;
        let body = json!({
            "updates": { "action": "install", "reboot": "never", "maintenance": true },
            "target": "devices", "device_ids": [desk.device_id], "offline": "skip",
        });
        assert_eq!(
            installer.post("/v1/jobs", body.clone()).await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let started = admin.post("/v1/jobs", body).await;
        assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
        assert_eq!(started.body["maintenance_during_install"], true);
        drop(connection);
        app.finish().await;
    }
}

#[tokio::test]
async fn maintenance_needs_its_permission() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let started = start(&mut admin, on_devices(&[&desk.device_id], "Repair", 60)).await;
        let window_id = started["windows"][0]["id"].as_str().unwrap().to_owned();
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let (mut stranger, _) =
            user_with(&app, &mut admin, "stranger@example.com", &["scripts.run"]).await;
        let (mut tech, _) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "devices.maintenance"],
        )
        .await;

        // Seeing windows needs devices.view.
        assert_eq!(windows(&mut viewer, "").await.len(), 1, "{name}");
        for path in [
            "/v1/maintenance-windows".to_owned(),
            format!("/v1/maintenance-windows/{window_id}"),
            format!("/v1/agents/{}/maintenance-windows", desk.device_id),
        ] {
            assert_eq!(
                stranger.get(&path).await.status,
                StatusCode::FORBIDDEN,
                "{name}: {path}"
            );
        }
        // Changing them needs devices.maintenance too.
        let refused = [
            viewer
                .post(
                    "/v1/maintenance-windows",
                    on_devices(&[&desk.device_id], "Mine", 30),
                )
                .await,
            viewer
                .post(
                    &format!("/v1/maintenance-windows/{window_id}/end"),
                    json!({}),
                )
                .await,
            viewer
                .patch(
                    &format!("/v1/maintenance-windows/{window_id}"),
                    json!({ "reason": "Mine" }),
                )
                .await,
            viewer
                .post("/v1/maintenance-windows/end", json!({ "target": "all" }))
                .await,
        ];
        for response in refused {
            assert_eq!(response.status, StatusCode::FORBIDDEN, "{name}");
        }
        let ended = tech
            .post(
                &format!("/v1/maintenance-windows/{window_id}/end"),
                json!({}),
            )
            .await;
        assert_eq!(ended.status, StatusCode::OK, "{name}: {:?}", ended.body);
        assert_eq!(ended.body["ended_by"], "tech@example.com");
        start(&mut tech, on_devices(&[&desk.device_id], "Theirs", 30)).await;
        // The built-in technician role holds it.
        let roles = admin.get("/v1/roles").await.body;
        let technician = roles
            .as_array()
            .unwrap()
            .iter()
            .find(|role| role["name"] == "Technician")
            .unwrap_or_else(|| panic!("{roles}"));
        assert!(
            technician["permissions"]
                .as_array()
                .unwrap()
                .contains(&json!("devices.maintenance")),
            "{name}"
        );

        // A maintenance policy needs it to save, and stops when its owner
        // loses it.
        let manager_role = common::add_role(
            &mut admin,
            "Manager",
            &["devices.view", "automation.manage", "devices.maintenance"],
        )
        .await;
        let (mut manager, _) =
            common::add_user(&app, &mut admin, "manager@example.com", &[&manager_role]).await;
        let body = json!({
            "name": "Nightly", "action": "maintenance", "maintenance_minutes": 60,
            "target": "all", "trigger": "interval", "interval_minutes": 1440, "offline": "skip",
        });
        let (mut planner, _) = user_with(
            &app,
            &mut admin,
            "planner@example.com",
            &["devices.view", "automation.manage"],
        )
        .await;
        assert_eq!(
            planner
                .post("/v1/automation-policies", body.clone())
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let policy = create_policy(&mut manager, body).await;
        let id = policy["id"].as_str().unwrap().to_owned();
        let changed = admin
            .patch(
                &format!("/v1/roles/{manager_role}"),
                json!({ "permissions": ["devices.view", "automation.manage"] }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        let due = policy["next_run_at"].as_i64().unwrap();
        automation::pass(&app.state, due).await.unwrap();
        assert!(
            windows(&mut admin, &format!("?state=all&policy={id}"))
                .await
                .is_empty(),
            "{name}"
        );
        let listed = listed_policy(&mut admin, &id).await;
        assert!(
            listed["last_problem"]
                .as_str()
                .unwrap_or_default()
                .contains("devices.maintenance"),
            "{name}: {listed}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn old_windows_are_purged_and_go_with_their_device() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        start(&mut admin, on_devices(&[&desk.device_id], "Old", 60)).await;
        start(&mut admin, on_devices(&[&laptop.device_id], "Gone", 60)).await;
        start(
            &mut admin,
            json!({ "target": "devices", "device_ids": [desk.device_id], "reason": "Until ended" }),
        )
        .await;

        // A window over for the retention period is purged; one still open
        // never is.
        let later = now_ms() + HOUR_MS + maintenance_windows::RETENTION_MS + MINUTE_MS;
        let purged = meshrmm_server::maintenance::purge(app.db(), later).await;
        assert_eq!(purged.failed, Vec::<String>::new(), "{name}");
        assert_eq!(purged.rows("maintenance_windows"), 2, "{name}");
        // The purge ended the sessions too.
        let mut admin = common::set_up_or_sign_in(&app).await;
        let left = windows(&mut admin, "?state=all").await;
        assert_eq!(left.len(), 1, "{name}");
        assert_eq!(left[0]["reason"], "Until ended");

        // Deleting a device takes its windows and its maintenance.
        let deleted = admin
            .delete(&format!("/v1/agents/{}", desk.device_id))
            .await;
        assert!(deleted.status.is_success(), "{:?}", deleted.body);
        assert!(windows(&mut admin, "?state=all").await.is_empty(), "{name}");
        assert!(
            !app.state
                .maintenance_windows
                .in_maintenance(&desk.device_id, now_ms())
        );
        app.finish().await;
    }
}
