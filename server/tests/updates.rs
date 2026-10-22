//! Operating system updates: Agents' scans over the control connection, the
//! device's pending list, scans and installs a user asks for, update jobs and
//! policies, alerts about updates, and lost runs.
mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{
    App, Browser, Ws, apps, audit_events, receive_any_json, receive_json, send_json, set_up,
    user_with,
};
use meshrmm_server::{
    alerts, automation,
    time::{DAY_MS, HOUR_MS, now_ms},
    updates,
};
use sea_query::ExprTrait;
use serde_json::{Value, json};

fn update(id: &str, category: &str) -> Value {
    json!({
        "id": id,
        "title": format!("Update {id}"),
        "version": "2.0",
        "current_version": "1.0",
        "category": category,
        "reboot_required": category == "security",
    })
}

fn scan(manager: &str, updates: Vec<Value>, reboot_pending: bool) -> Value {
    json!({
        "scanned_at_unix_ms": 1_700_000_000_000_i64,
        "manager": manager,
        "updates": updates,
        "reboot_pending": reboot_pending,
    })
}

async fn report_scan(control: &mut Ws, run_ids: &[&str], scan: Value) {
    send_json(
        control,
        json!({ "type": "update_scan", "run_ids": run_ids, "scan": scan }),
    )
    .await;
}

async fn device_updates(browser: &mut Browser, device_id: &str) -> Value {
    let response = browser
        .get(&format!("/v1/agents/{device_id}/updates"))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    response.body
}

/// Waits until the device's updates satisfy `ready`, as the server handles
/// the Agent's messages in the background.
async fn eventually(
    browser: &mut Browser,
    device_id: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..100 {
        let updates = device_updates(browser, device_id).await;
        if ready(&updates) {
            return updates;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the device's updates never got there");
}

async fn run(browser: &mut Browser, id: &str) -> Value {
    let response = browser.get(&format!("/v1/update-runs/{id}")).await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    response.body
}

async fn run_where(browser: &mut Browser, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..100 {
        let run = run(browser, id).await;
        if ready(&run) {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the run never got there");
}

/// The update titles the device has pending, in order.
fn titles(updates: &Value) -> Vec<&str> {
    updates["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|update| update["title"].as_str().unwrap())
        .collect()
}

/// The next event of `kind` on the website's socket.
async fn event_of(events: &mut Ws, kind: &str) -> Value {
    loop {
        let message = receive_any_json(events).await;
        if message["type"] == kind {
            return message;
        }
    }
}

#[tokio::test]
async fn agents_report_scans_the_website_shows() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let never = device_updates(&mut admin, &desk.device_id).await;
        assert_eq!(never["status"]["scanned_at"], Value::Null, "{name}");
        assert_eq!(never["updates"], json!([]));

        // A device that never scanned is asked to when it connects.
        let mut control = desk.connect(&server).await.unwrap();
        let mut asked = false;
        for _ in 0..2 {
            let command = receive_any_json(&mut control).await;
            asked |= command == json!({ "type": "scan_updates" });
        }
        assert!(asked, "{name}");

        let mut events = admin.events(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan(
                "windows_update",
                vec![
                    update("b", "security"),
                    update("a", "other"),
                    update("c", "security"),
                ],
                false,
            ),
        )
        .await;
        // The connection's own summary may come first.
        let mut event = event_of(&mut events, "device_updates").await;
        while event["device"]["pending"] == 0 {
            event = event_of(&mut events, "device_updates").await;
        }
        assert_eq!(event["device"]["device_id"], desk.device_id, "{name}");
        assert_eq!(event["device"]["pending"], 3);
        assert_eq!(event["device"]["security"], 2);
        let shown = device_updates(&mut admin, &desk.device_id).await;
        assert_eq!(shown["status"]["manager"], "windows_update");
        assert_eq!(shown["status"]["scanned_at"], 1_700_000_000_000_i64);
        assert_eq!(shown["status"]["can_select"], true);
        assert_eq!(
            titles(&shown),
            ["Update b", "Update c", "Update a"],
            "{name}: security first"
        );
        let first_seen = shown["updates"][0]["first_seen_at"].as_i64().unwrap();

        // A later scan keeps when an update was first seen, and drops what's
        // installed; a failed one keeps the list.
        tokio::time::sleep(Duration::from_millis(5)).await;
        report_scan(
            &mut control,
            &[],
            scan("windows_update", vec![update("b", "security")], true),
        )
        .await;
        // The status and the list are read separately, so a read during the
        // store can see one new and the other old.
        let shown = eventually(&mut admin, &desk.device_id, |updates| {
            updates["updates"].as_array().unwrap().len() == 1
                && updates["status"]["reboot_pending"] == true
        })
        .await;
        assert_eq!(shown["updates"][0]["first_seen_at"], first_seen, "{name}");
        assert_eq!(shown["status"]["reboot_pending"], true);
        let since = shown["status"]["reboot_pending_since"].as_i64().unwrap();
        report_scan(
            &mut control,
            &[],
            json!({ "scanned_at_unix_ms": 1, "manager": "windows_update", "error": "Windows Update failed (0x8024402C)" }),
        )
        .await;
        let shown = eventually(&mut admin, &desk.device_id, |updates| {
            !updates["status"]["scan_error"].is_null()
        })
        .await;
        assert_eq!(titles(&shown), ["Update b"], "{name}");
        assert_eq!(shown["status"]["reboot_pending_since"], since);

        // The fleet list has every device, scanned or not.
        let other = app.enroll_on(&mut admin, "Other", "linux").await;
        let fleet = admin.get("/v1/updates/devices").await;
        assert_eq!(fleet.status, StatusCode::OK);
        let devices = fleet.body["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 2, "{name}");
        let desk_summary = devices
            .iter()
            .find(|device| device["device_id"] == desk.device_id)
            .unwrap();
        assert_eq!(desk_summary["connected"], true);
        assert_eq!(desk_summary["pending"], 1);
        assert_eq!(desk_summary["reboot_pending"], true);
        let other_summary = devices
            .iter()
            .find(|device| device["device_id"] == other.device_id)
            .unwrap();
        assert_eq!(other_summary["received_at"], Value::Null);
        assert_eq!(other_summary["platform"], "linux");

        // Users without the permission see none of it.
        let (mut viewer, _) = user_with(&app, &mut admin, "v@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer
                .get(&format!("/v1/agents/{}/updates", desk.device_id))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer.get("/v1/updates/devices").await.status,
            StatusCode::FORBIDDEN
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn users_scan_and_install_on_a_device_and_see_its_history() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let (mut tech, _) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "updates.view", "updates.install"],
        )
        .await;
        let scan_path = format!("/v1/agents/{}/updates/scan", desk.device_id);
        let install_path = format!("/v1/agents/{}/updates/install", desk.device_id);
        assert_eq!(
            tech.post(&scan_path, json!({})).await.code(),
            "device_offline",
            "{name}"
        );

        let mut control = desk.connect(&server).await.unwrap();
        let install = json!({ "update_ids": ["a"], "reboot": "never" });
        assert_eq!(
            tech.post(&install_path, install.clone()).await.code(),
            "not_scanned",
            "{name}"
        );

        // Scan now: the Agent gets the run's ID and answers it.
        let started = tech.post(&scan_path, json!({})).await;
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        let scan_id = started.body["id"].as_str().unwrap().to_owned();
        assert_eq!(started.body["status"], "pending");
        let again = tech.post(&scan_path, json!({})).await;
        assert_eq!(again.body["id"], scan_id, "{name}: one scan at a time");
        assert_eq!(
            receive_json(&mut control).await,
            json!({ "type": "scan_updates", "run_id": scan_id })
        );
        report_scan(
            &mut control,
            &[&scan_id],
            scan(
                "apt",
                vec![update("openssl", "security"), update("vim", "other")],
                false,
            ),
        )
        .await;
        let finished = run_where(&mut tech, &scan_id, |run| run["status"] != "pending").await;
        assert_eq!(finished["status"], "succeeded", "{name}");
        assert_eq!(finished["requested_by"], "tech@example.com");

        // Install the chosen updates, with progress and each one's outcome.
        let bad = tech
            .post(
                &install_path,
                json!({ "reboot": "never", "reboot_delay_minutes": 0 }),
            )
            .await;
        assert_eq!(bad.code(), "invalid_install", "{name}");
        let started = tech
            .post(
                &install_path,
                json!({ "update_ids": ["openssl", "vim"], "reboot": "if_required", "reboot_delay_minutes": 10 }),
            )
            .await;
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        let install_id = started.body["id"].as_str().unwrap().to_owned();
        let busy = tech.post(&install_path, json!({ "reboot": "never" })).await;
        assert_eq!(busy.code(), "install_running", "{name}");
        let command = receive_json(&mut control).await;
        assert_eq!(command["type"], "install_updates", "{command}");
        assert_eq!(command["install"]["run_id"], install_id);
        assert_eq!(
            command["install"]["selection"]["ids"],
            json!(["openssl", "vim"])
        );
        assert_eq!(command["install"]["reboot"], "if_required");
        assert_eq!(command["install"]["reboot_delay_minutes"], 10);

        let mut events = tech.events(&server).await.unwrap();
        send_json(
            &mut control,
            json!({ "type": "update_progress", "run_id": install_id, "message": "Installing 1 of 2: openssl", "percent": 50 }),
        )
        .await;
        let progress = event_of(&mut events, "update_run").await;
        assert_eq!(
            progress["run"]["progress"], "Installing 1 of 2: openssl",
            "{name}"
        );
        assert_eq!(progress["run"]["progress_percent"], 50);
        let shown = device_updates(&mut tech, &desk.device_id).await;
        assert_eq!(shown["status"]["installing"]["id"], install_id);

        send_json(
            &mut control,
            json!({ "type": "update_install", "run_id": install_id, "result": {
                "status": "partial",
                "items": [
                    { "id": "openssl", "title": "openssl", "category": "security", "outcome": "installed", "reboot_required": true },
                    { "id": "vim", "title": "vim", "category": "other", "outcome": "failed", "error": "E: broken dependency" },
                ],
                "reboot_required": true,
                "reboot_scheduled_at_unix_ms": 1_700_000_600_000_i64,
            } }),
        )
        .await;
        report_scan(
            &mut control,
            &[],
            scan("apt", vec![update("vim", "other")], true),
        )
        .await;
        let done = run_where(&mut tech, &install_id, |run| run["status"] != "pending").await;
        assert_eq!(done["status"], "partial", "{name}");
        assert_eq!(done["installed"], 1);
        assert_eq!(done["failed"], 1);
        assert_eq!(done["reboot_required"], true);
        assert_eq!(done["reboot_scheduled_at"], 1_700_000_600_000_i64);
        assert_eq!(done["progress"], Value::Null);
        assert_eq!(
            done["items"][0]["update_id"], "vim",
            "{name}: failures first"
        );
        assert_eq!(done["items"][0]["error"], "E: broken dependency");
        let shown = eventually(&mut tech, &desk.device_id, |updates| {
            updates["status"]["reboot_pending"] == true
        })
        .await;
        assert_eq!(titles(&shown), ["Update vim"]);
        assert_eq!(shown["status"]["installing"], Value::Null);

        let history = tech
            .get(&format!(
                "/v1/update-runs?device_id={}&kind=install",
                desk.device_id
            ))
            .await;
        assert_eq!(history.status, StatusCode::OK);
        let runs = history.body["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "{name}");
        assert_eq!(runs[0]["id"], install_id);
        assert_eq!(runs[0]["items"].as_array().unwrap().len(), 2);
        let audited = audit_events(&app, "update.install").await;
        assert_eq!(audited.len(), 1, "{name}");
        assert_eq!(audited[0].target_id, desk.device_id);
        assert_eq!(audited[0].metadata["reboot"], "if_required");
        assert_eq!(audit_events(&app, "update.scan").await.len(), 1);

        // A report for a run of another device, or a finished one, is ignored.
        send_json(
            &mut control,
            json!({ "type": "update_install", "run_id": install_id, "result": { "status": "succeeded" } }),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(run(&mut tech, &install_id).await["status"], "partial");

        // Installing needs the permission.
        let (mut looker, _) = user_with(
            &app,
            &mut admin,
            "look@example.com",
            &["devices.view", "updates.view"],
        )
        .await;
        assert_eq!(
            looker
                .post(&install_path, json!({ "reboot": "never" }))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn pacman_installs_everything_and_unsupported_devices_are_said_so() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let arch = app.enroll_on(&mut admin, "Arch", "linux").await;
        let mut control = arch.connect(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan("pacman", vec![update("linux", "other")], false),
        )
        .await;
        let shown = eventually(&mut admin, &arch.device_id, |updates| {
            !updates["status"]["scanned_at"].is_null()
        })
        .await;
        assert_eq!(shown["status"]["can_select"], false, "{name}");
        let path = format!("/v1/agents/{}/updates/install", arch.device_id);
        let chosen = admin
            .post(&path, json!({ "update_ids": ["linux"], "reboot": "never" }))
            .await;
        assert_eq!(chosen.code(), "selection_unsupported", "{name}");
        let all = admin.post(&path, json!({ "reboot": "always" })).await;
        assert_eq!(all.status, StatusCode::ACCEPTED, "{:?}", all.body);

        let suse = app.enroll_on(&mut admin, "SUSE", "linux").await;
        let mut other = suse.connect(&server).await.unwrap();
        report_scan(
            &mut other,
            &[],
            json!({ "scanned_at_unix_ms": 1, "updates": [], "reboot_pending": false }),
        )
        .await;
        let shown = eventually(&mut admin, &suse.device_id, |updates| {
            updates["status"]["supported"] == false
        })
        .await;
        assert_eq!(shown["status"]["manager"], Value::Null, "{name}");
        let refused = admin
            .post(
                &format!("/v1/agents/{}/updates/install", suse.device_id),
                json!({ "reboot": "never" }),
            )
            .await;
        assert_eq!(refused.code(), "unsupported", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn update_jobs_and_policies_run_on_each_device() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let away = app.enroll_on(&mut admin, "Away", "linux").await;
        let suse = app.enroll_on(&mut admin, "SUSE", "linux").await;
        // SUSE's last scan found no supported package manager.
        meshrmm_server::updates::store::store_scan(
            app.db(),
            &suse.device_id,
            serde_json::from_value(json!({ "scanned_at_unix_ms": 1 })).unwrap(),
            now_ms(),
        )
        .await
        .unwrap();
        let mut control = desk.connect(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan("windows_update", vec![update("kb1", "security")], false),
        )
        .await;
        eventually(&mut admin, &desk.device_id, |updates| {
            !updates["status"]["scanned_at"].is_null()
        })
        .await;

        let (mut tech, _) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "updates.view"],
        )
        .await;
        let body = json!({
            "updates": { "action": "install", "categories": ["security", "critical"], "reboot": "never" },
            "target": "devices",
            "device_ids": [desk.device_id, away.device_id, suse.device_id],
            "offline": "skip",
        });
        assert_eq!(
            tech.post("/v1/jobs", body.clone()).await.status,
            StatusCode::FORBIDDEN,
            "{name}: installing needs updates.install"
        );
        let started = admin.post("/v1/jobs", body).await;
        assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
        let job_id = started.body["id"].as_str().unwrap().to_owned();
        assert_eq!(started.body["kind"], "update_install", "{name}");
        assert_eq!(
            started.body["item_name"],
            "Install security and critical updates"
        );
        assert_eq!(started.body["categories"], json!(["security", "critical"]));
        assert_eq!(started.body["reboot"], "never");
        let outcome = |job: &Value, device: &str| {
            job["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["device_name"] == device)
                .unwrap()["outcome"]
                .clone()
        };
        assert_eq!(outcome(&started.body, "SUSE"), "skipped", "{name}");
        assert_eq!(outcome(&started.body, "Away"), "missed");
        assert_eq!(outcome(&started.body, "Desk"), "queued");

        automation::pass(&app.state, now_ms()).await.unwrap();
        let command = receive_json(&mut control).await;
        assert_eq!(command["type"], "install_updates", "{command}");
        assert_eq!(
            command["install"]["selection"]["categories"],
            json!(["security", "critical"])
        );
        let run_id = command["install"]["run_id"].as_str().unwrap().to_owned();
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        assert_eq!(outcome(&job, "Desk"), "running", "{name}");
        send_json(
            &mut control,
            json!({ "type": "update_install", "run_id": run_id, "result": {
                "status": "succeeded",
                "items": [{ "id": "kb1", "title": "KB1", "category": "security", "outcome": "installed" }],
            } }),
        )
        .await;
        run_where(&mut admin, &run_id, |run| run["status"] == "succeeded").await;
        automation::pass(&app.state, now_ms()).await.unwrap();
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        assert_eq!(outcome(&job, "Desk"), "succeeded", "{name}");
        let desk_entry = job["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["device_name"] == "Desk")
            .unwrap();
        assert_eq!(desk_entry["update_run_id"], run_id);
        assert!(job["finished_at_unix_ms"].is_i64(), "{name}");
        assert_eq!(run(&mut admin, &run_id).await["job_id"], job_id);

        // A scan policy, run now; the user it runs as needs updates.view.
        let policy = json!({
            "name": "Daily scan", "action": "update_scan", "target": "devices",
            "device_ids": [desk.device_id], "trigger": "daily", "time": "03:00",
            "time_zone": "UTC", "offline": "wait",
        });
        let (mut manager, _) = user_with(
            &app,
            &mut admin,
            "manager@example.com",
            &["devices.view", "automation.manage"],
        )
        .await;
        assert_eq!(
            manager
                .post("/v1/automation-policies", policy.clone())
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let created = admin.post("/v1/automation-policies", policy).await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["action"], "update_scan");
        assert_eq!(created.body["script_id"], Value::Null);
        let policy_id = created.body["id"].as_str().unwrap().to_owned();
        let ran = admin
            .post(
                &format!("/v1/automation-policies/{policy_id}/run"),
                json!({}),
            )
            .await;
        assert_eq!(ran.status, StatusCode::CREATED, "{:?}", ran.body);
        assert_eq!(ran.body["kind"], "update_scan", "{name}");
        assert_eq!(ran.body["item_name"], "Scan for updates");
        automation::pass(&app.state, now_ms()).await.unwrap();
        let command = receive_json(&mut control).await;
        assert_eq!(command["type"], "scan_updates", "{command}");
        let scan_run = command["run_id"].as_str().unwrap().to_owned();
        assert_eq!(
            run(&mut admin, &scan_run).await["policy_name"],
            "Daily scan",
            "{name}"
        );

        // An install policy keeps its categories and restart.
        let install = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Patch Tuesday", "action": "update_install", "update_categories": ["security"],
                    "reboot": "always", "reboot_delay_minutes": 15, "target": "all",
                    "trigger": "weekly", "weekdays": [2], "time": "22:00", "time_zone": "Europe/London",
                    "offline": "wait",
                }),
            )
            .await;
        assert_eq!(install.status, StatusCode::CREATED, "{:?}", install.body);
        assert_eq!(install.body["update_categories"], json!(["security"]));
        assert_eq!(install.body["reboot"], "always");
        assert_eq!(install.body["reboot_delay_minutes"], 15);
        let missing = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "No restart", "action": "update_install", "target": "all",
                    "trigger": "interval", "interval_minutes": 60, "offline": "skip",
                }),
            )
            .await;
        assert_eq!(missing.code(), "invalid_policy", "{name}");
        let listed = admin.get("/v1/automation-policies").await;
        assert_eq!(listed.body["policies"].as_array().unwrap().len(), 2);
        app.finish().await;
    }
}

async fn create_rule(admin: &mut Browser, rule: Value) -> String {
    let created = admin.post("/v1/alert-rules", rule).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn alerts_of(browser: &mut Browser) -> Vec<Value> {
    let response = browser.get("/v1/alerts?state=open").await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    response.body["alerts"].as_array().unwrap().clone()
}

/// Makes the device's pending updates and restart look `days` old.
async fn age(app: &App, device_id: &str, days: i64) {
    let then = now_ms() - days * DAY_MS;
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new("device_pending_updates"))
                .value(sea_query::Alias::new("first_seen_at"), then)
                .and_where(sea_query::Expr::col(sea_query::Alias::new("device_id")).eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap();
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new("device_update_status"))
                .value(sea_query::Alias::new("reboot_pending_since"), then)
                .and_where(sea_query::Expr::col(sea_query::Alias::new("device_id")).eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn pending_updates_restarts_and_failed_installs_raise_alerts() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let pending = create_rule(
            &mut admin,
            json!({ "name": "Old security updates", "severity": "warning", "condition": "updates_pending", "update_category": "security", "days": 7 }),
        )
        .await;
        let restart = create_rule(
            &mut admin,
            json!({ "name": "Restart pending", "severity": "info", "condition": "reboot_pending", "days": 3 }),
        )
        .await;
        let failed = create_rule(
            &mut admin,
            json!({ "name": "Install failed", "severity": "critical", "condition": "update_install_failed" }),
        )
        .await;
        let invalid = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Bad", "severity": "info", "condition": "updates_pending", "update_category": "everything", "days": 7 }),
            )
            .await;
        assert_eq!(invalid.code(), "invalid_rule", "{name}");
        let rules = admin.get("/v1/alert-rules").await.body;
        let described = rules
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["description"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(
            described.contains(&"Security updates pending for 7 days".to_owned()),
            "{described:?}"
        );
        assert!(described.contains(&"Restart pending for 3 days".to_owned()));

        let mut control = desk.connect(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan(
                "dnf",
                vec![
                    update("openssl.x86_64", "security"),
                    update("vim.x86_64", "other"),
                ],
                true,
            ),
        )
        .await;
        eventually(&mut admin, &desk.device_id, |updates| {
            !updates["status"]["scanned_at"].is_null()
        })
        .await;
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        assert!(
            alerts_of(&mut admin).await.is_empty(),
            "{name}: not old yet"
        );

        age(&app, &desk.device_id, 8).await;
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        let open = alerts_of(&mut admin).await;
        assert_eq!(open.len(), 2, "{name}: {open:?}");
        let old = open
            .iter()
            .find(|alert| alert["rule_id"] == pending)
            .unwrap();
        assert!(
            old["detail"]
                .as_str()
                .unwrap()
                .starts_with("1 security update pending for more than 7 days"),
            "{old}"
        );
        assert!(open.iter().any(|alert| alert["rule_id"] == restart));

        // Installing them and restarting clears both.
        report_scan(
            &mut control,
            &[],
            scan("dnf", vec![update("vim.x86_64", "other")], false),
        )
        .await;
        eventually(&mut admin, &desk.device_id, |updates| {
            updates["status"]["reboot_pending"] == false
        })
        .await;
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        assert!(alerts_of(&mut admin).await.is_empty(), "{name}");

        // A failed install raises an alert that stays until resolved.
        let started = admin
            .post(
                &format!("/v1/agents/{}/updates/install", desk.device_id),
                json!({ "reboot": "never" }),
            )
            .await;
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        send_json(
            &mut control,
            json!({ "type": "update_install", "run_id": run_id, "result": {
                "status": "failed", "error": "Another program is using the package manager",
            } }),
        )
        .await;
        let mut raised = Vec::new();
        for _ in 0..100 {
            raised = alerts_of(&mut admin).await;
            if !raised.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(raised.len(), 1, "{name}");
        assert_eq!(raised[0]["rule_id"], failed);
        assert_eq!(raised[0]["update_run_id"], run_id);
        assert_eq!(
            raised[0]["detail"],
            "Installing updates failed: Another program is using the package manager"
        );
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        assert_eq!(alerts_of(&mut admin).await.len(), 1, "{name}: stays open");
        app.finish().await;
    }
}

#[tokio::test]
async fn runs_the_agent_never_reports_are_lost() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let _control = desk.connect(&server).await.unwrap();
        let started = admin
            .post(
                &format!("/v1/agents/{}/updates/scan", desk.device_id),
                json!({}),
            )
            .await;
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        updates::sweep(&app.state, now_ms() + HOUR_MS / 2)
            .await
            .unwrap();
        assert_eq!(
            run(&mut admin, &run_id).await["status"],
            "pending",
            "{name}"
        );
        updates::sweep(&app.state, now_ms() + HOUR_MS)
            .await
            .unwrap();
        let lost = run(&mut admin, &run_id).await;
        assert_eq!(lost["status"], "lost", "{name}");
        assert_eq!(lost["error"], "The device never reported how it went");
        app.finish().await;
    }
}

#[tokio::test]
async fn the_scan_interval_is_a_setting() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let settings = admin.get("/v1/settings").await;
        assert_eq!(settings.body["update_scan_interval_hours"], 24, "{name}");
        let changed = admin
            .patch("/v1/settings", json!({ "update_scan_interval_hours": 0 }))
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["update_scan_interval_hours"], 0);
        let refused = admin
            .patch("/v1/settings", json!({ "update_scan_interval_hours": 169 }))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn technicians_see_and_install_updates() {
    for app in apps().await {
        let mut admin = set_up(&app).await;
        let roles = admin.get("/v1/roles").await.body;
        let technician = roles
            .as_array()
            .unwrap()
            .iter()
            .find(|role| role["builtin"] == "technician")
            .unwrap()
            .clone();
        let permissions = technician["permissions"].as_array().unwrap();
        assert!(permissions.contains(&json!("updates.view")), "{}", app.name);
        assert!(permissions.contains(&json!("updates.install")));
        app.finish().await;
    }
}
