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

/// The update scan schedule the Agent is sent, failing on any scan asked
/// for before it.
async fn schedule_of(control: &mut Ws) -> Value {
    loop {
        let message = receive_any_json(control).await;
        assert_ne!(message["type"], "scan_updates", "{message}");
        if message["type"] == "set_update_scan_schedule" {
            return message["schedule"].clone();
        }
    }
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

        // A device that never scanned is told so with the schedule, and
        // scans by it.
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(
            schedule_of(&mut control).await,
            json!({ "interval_hours": 24 }),
            "{name}"
        );

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

/// Stores a scan as if the device's Agent reported it at `now`.
async fn store(app: &App, device_id: &str, scan: Value, now: i64) -> updates::store::Stored {
    updates::store::store_scan(
        app.db(),
        device_id,
        serde_json::from_value(scan).unwrap(),
        now,
    )
    .await
    .unwrap()
}

fn scan_at(scanned_at: i64, updates: Vec<Value>, reboot_pending: bool) -> Value {
    let mut scan = scan("dnf", updates, reboot_pending);
    scan["scanned_at_unix_ms"] = scanned_at.into();
    scan
}

/// Retitles the device's stored update `id` behind the store's back, so a
/// rewrite of the list shows.
async fn mark(app: &App, device_id: &str, id: &str) {
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new("device_pending_updates"))
                .value(sea_query::Alias::new("title"), "Marked")
                .and_where(sea_query::Expr::col(sea_query::Alias::new("device_id")).eq(device_id))
                .and_where(sea_query::Expr::col(sea_query::Alias::new("update_id")).eq(id))
                .to_owned(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn scans_that_list_the_same_updates_write_only_their_status() {
    use updates::store::Stored;
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let id = desk.device_id.as_str();
        let now = now_ms();
        let listed = || vec![update("a", "security"), update("b", "other")];
        assert_eq!(
            store(&app, id, scan_at(1, listed(), false), now).await,
            Stored::Replaced,
            "{name}"
        );
        let first_seen =
            device_updates(&mut admin, id).await["updates"][0]["first_seen_at"].clone();
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([2, 1, 0, now]),
            "{name}"
        );
        mark(&app, id, "a").await;

        // The same updates in another order: the list stays, the status
        // says when the scan was.
        let mut reversed = listed();
        reversed.reverse();
        assert_eq!(
            store(&app, id, scan_at(2, reversed, false), now + 1_000).await,
            Stored::Unchanged {
                reboot_changed: false
            },
            "{name}"
        );
        let shown = device_updates(&mut admin, id).await;
        assert_eq!(
            titles(&shown),
            ["Marked", "Update b"],
            "{name}: not rewritten"
        );
        assert_eq!(shown["updates"][0]["first_seen_at"], first_seen);
        assert_eq!(shown["status"]["scanned_at"], 2, "{name}");
        assert_eq!(shown["status"]["received_at"], now + 1_000);
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([2, 1, 0, now]),
            "{name}: an unchanged list keeps its counts"
        );

        // A restart pending is the scan's own, not the list's.
        assert_eq!(
            store(&app, id, scan_at(3, listed(), true), now + 2_000).await,
            Stored::Unchanged {
                reboot_changed: true
            },
            "{name}"
        );
        assert_eq!(
            store(&app, id, scan_at(4, listed(), true), now + 3_000).await,
            Stored::Unchanged {
                reboot_changed: false
            },
            "{name}"
        );
        let shown = device_updates(&mut admin, id).await;
        assert_eq!(shown["status"]["reboot_pending"], true, "{name}");
        assert_eq!(shown["status"]["reboot_pending_since"], now + 2_000);
        assert_eq!(shown["status"]["scanned_at"], 4);

        // A failed scan shows its error; the next that works clears it.
        assert_eq!(
            store(
                &app,
                id,
                json!({ "scanned_at_unix_ms": 5, "manager": "dnf", "error": "dnf failed" }),
                now + 4_000,
            )
            .await,
            Stored::Failed,
            "{name}"
        );
        let shown = device_updates(&mut admin, id).await;
        assert_eq!(shown["status"]["scan_error"], "dnf failed", "{name}");
        assert_eq!(shown["status"]["received_at"], now + 4_000);
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([2, 1, 0, now]),
            "{name}"
        );
        assert_eq!(
            store(&app, id, scan_at(6, listed(), true), now + 5_000).await,
            Stored::Unchanged {
                reboot_changed: false
            },
            "{name}"
        );
        let shown = device_updates(&mut admin, id).await;
        assert_eq!(shown["status"]["scan_error"], Value::Null, "{name}");
        assert_eq!(shown["status"]["scanned_at"], 6);
        assert_eq!(titles(&shown), ["Marked", "Update b"]);

        // Different updates replace the list.
        assert_eq!(
            store(
                &app,
                id,
                scan_at(7, vec![update("a", "security")], true),
                now + 6_000
            )
            .await,
            Stored::Replaced,
            "{name}"
        );
        let shown = device_updates(&mut admin, id).await;
        assert_eq!(titles(&shown), ["Update a"], "{name}");
        assert_eq!(shown["updates"][0]["first_seen_at"], first_seen);
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([1, 1, 0, now]),
            "{name}"
        );
        assert_eq!(
            store(
                &app,
                id,
                scan_at(
                    7,
                    vec![update("a", "security"), update("n", "critical")],
                    true
                ),
                now + 6_500
            )
            .await,
            Stored::Replaced,
            "{name}"
        );
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([2, 1, 1, now]),
            "{name}"
        );

        // Once the device's updates are removed, the same scan stores them
        // again.
        updates::store::forget(app.db(), id).await.unwrap();
        assert_eq!(
            store(
                &app,
                id,
                scan_at(8, vec![update("a", "security")], true),
                now + 7_000
            )
            .await,
            Stored::Replaced,
            "{name}"
        );
        let shown = device_updates(&mut admin, id).await;
        assert_eq!(titles(&shown), ["Update a"], "{name}");
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([1, 1, 0, now + 7_000]),
            "{name}"
        );

        // Nothing pending counts nothing.
        assert_eq!(
            store(&app, id, scan_at(9, vec![], false), now + 8_000).await,
            Stored::Replaced,
            "{name}"
        );
        assert_eq!(
            counts_of(&mut admin, id).await,
            json!([0, 0, 0, null]),
            "{name}"
        );
        app.finish().await;
    }
}

/// The device's pending, security and critical counts and its oldest
/// pending update's first sighting, which its Updates tab and the fleet
/// show alike.
async fn counts_of(browser: &mut Browser, device_id: &str) -> Value {
    let counts = |summary: &Value| {
        json!([
            summary["pending"],
            summary["security"],
            summary["critical"],
            summary["oldest_pending_at"]
        ])
    };
    let status = device_updates(browser, device_id).await["status"].clone();
    let page = fleet(browser, "").await;
    let row = page["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["device_id"] == device_id)
        .unwrap();
    assert_eq!(counts(&status), counts(row));
    counts(row)
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
        let page = fleet(&mut tech, "").await;
        assert_eq!(page["devices"][0]["installing"], true, "{name}");
        assert_eq!(page["devices"][0]["pending"], 2);
        assert_eq!(page["devices"][0]["security"], 1);
        assert_eq!(page["totals"]["security"], 1);

        desk
            .report(&format!("update-runs/{install_id}/result"), json!({
                "status": "partial",
                "items": [
                    { "id": "openssl", "title": "openssl", "category": "security", "outcome": "installed", "reboot_required": true },
                    { "id": "vim", "title": "vim", "category": "other", "outcome": "failed", "error": "E: broken dependency" },
                ],
                "reboot_required": true,
                "reboot_scheduled_at_unix_ms": 1_700_000_600_000_i64,
            }))
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
        // The fleet counts what the scan after the install left.
        let page = fleet(&mut tech, "").await;
        let row = &page["devices"][0];
        assert_eq!(row["pending"], 1, "{name}");
        assert_eq!(row["security"], 0);
        assert_eq!(row["installing"], false);
        assert_eq!(row["connected"], true);
        assert_eq!(row["last_install"]["status"], "partial");
        assert_eq!(page["totals"]["security"], 0, "{name}");
        assert_eq!(page["totals"]["failed"], 1);

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

        // The Agent sends a result again when the answer to it never
        // arrived: it is taken, and changes nothing.
        let before = run(&mut tech, &install_id).await;
        let again = desk
            .report(
                &format!("update-runs/{install_id}/result"),
                json!({ "status": "succeeded", "items": [
                    { "id": "other", "title": "other", "category": "other", "outcome": "installed" },
                ] }),
            )
            .await;
        assert_eq!(
            again.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            again.body
        );
        assert_eq!(run(&mut tech, &install_id).await, before, "{name}");
        // Another device has no such install, so its Agent drops the result.
        let other = app.enroll(&mut admin, "Other").await;
        let refused = other
            .report(
                &format!("update-runs/{install_id}/result"),
                json!({ "status": "succeeded" }),
            )
            .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(run(&mut tech, &install_id).await, before);

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
        let created = common::job_with_devices(&mut admin, &job_id).await;
        assert_eq!(outcome(&created, "SUSE"), "skipped", "{name}");
        assert_eq!(outcome(&created, "Away"), "missed");
        assert_eq!(outcome(&created, "Desk"), "queued");

        automation::pass(&app.state, now_ms()).await.unwrap();
        let command = receive_json(&mut control).await;
        assert_eq!(command["type"], "install_updates", "{command}");
        assert_eq!(
            command["install"]["selection"]["categories"],
            json!(["security", "critical"])
        );
        let run_id = command["install"]["run_id"].as_str().unwrap().to_owned();
        let job = common::job_with_devices(&mut admin, &job_id).await;
        assert_eq!(outcome(&job, "Desk"), "running", "{name}");
        desk
            .report(&format!("update-runs/{run_id}/result"), json!({
                "status": "succeeded",
                "items": [{ "id": "kb1", "title": "KB1", "category": "security", "outcome": "installed" }],
            }))
            .await;
        run_where(&mut admin, &run_id, |run| run["status"] == "succeeded").await;
        automation::pass(&app.state, now_ms()).await.unwrap();
        let job = common::job_with_devices(&mut admin, &job_id).await;
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

/// Evaluates until no alert is open: the server tells the alert engine of a
/// scan just after storing it, so an evaluation may come between the two.
async fn evaluate_until_clear(app: &App, admin: &mut Browser) {
    for _ in 0..100 {
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        if alerts_of(admin).await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the alerts never cleared");
}

/// Makes the device's pending updates and restart look `days` old, and tells
/// the alert engine, which reads them again only for devices that reported.
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
    app.state.alerts.device_reported(device_id);
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
        evaluate_until_clear(&app, &mut admin).await;

        // A failed install raises an alert that stays until resolved.
        let started = admin
            .post(
                &format!("/v1/agents/{}/updates/install", desk.device_id),
                json!({ "reboot": "never" }),
            )
            .await;
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        desk.report(
            &format!("update-runs/{run_id}/result"),
            json!({
                "status": "failed", "error": "Another program is using the package manager",
            }),
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

        // An install's result that arrives after its run was counted as lost,
        // such as one the Agent kept while the server was down, replaces that:
        // the install did run.
        let mut control = desk.connect(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan("apt", vec![update("openssl", "security")], false),
        )
        .await;
        eventually(&mut admin, &desk.device_id, |updates| {
            updates["status"]["received_at"].is_number()
        })
        .await;
        let started = admin
            .post(
                &format!("/v1/agents/{}/updates/install", desk.device_id),
                json!({ "reboot": "never" }),
            )
            .await;
        assert_eq!(
            started.status,
            StatusCode::ACCEPTED,
            "{name}: {:?}",
            started.body
        );
        let install_id = started.body["id"].as_str().unwrap().to_owned();
        updates::sweep(&app.state, now_ms() + 24 * HOUR_MS)
            .await
            .unwrap();
        assert_eq!(
            run(&mut admin, &install_id).await["status"],
            "lost",
            "{name}"
        );
        let late = desk
            .report(
                &format!("update-runs/{install_id}/result"),
                json!({ "status": "succeeded", "items": [
                    { "id": "openssl", "title": "openssl", "category": "security", "outcome": "installed" },
                ] }),
            )
            .await;
        assert_eq!(
            late.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            late.body
        );
        let found = run(&mut admin, &install_id).await;
        assert_eq!(found["status"], "succeeded", "{name}");
        assert_eq!(found["error"], Value::Null);
        assert_eq!(found["installed"], 1);
        assert_eq!(found["items"].as_array().unwrap().len(), 1);
        let shown = eventually(&mut admin, &desk.device_id, |updates| {
            updates["status"]["last_install"]["status"] == "succeeded"
        })
        .await;
        assert_eq!(shown["status"]["last_install"]["id"], install_id, "{name}");
        drop(control);
        app.finish().await;
    }
}

#[tokio::test]
async fn agents_are_told_the_scan_schedule_and_asked_only_for_scans_people_want() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;

        // A scan that said to restart is in the schedule, with how long ago
        // it arrived, and the Agent isn't asked to scan.
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(
            schedule_of(&mut control).await,
            json!({ "interval_hours": 24 })
        );
        report_scan(
            &mut control,
            &[],
            scan("windows_update", vec![update("a", "security")], true),
        )
        .await;
        eventually(&mut admin, &desk.device_id, |updates| {
            updates["status"]["reboot_pending"] == true
        })
        .await;
        drop(control);
        let mut control = desk.connect(&server).await.unwrap();
        let schedule = schedule_of(&mut control).await;
        assert_eq!(schedule["interval_hours"], 24, "{name}: {schedule}");
        assert_eq!(schedule["last_scan"]["reboot_pending"], true, "{schedule}");
        let ago = schedule["last_scan"]["received_ms_ago"].as_i64().unwrap();
        assert!((0..60_000).contains(&ago), "{name}: {schedule}");
        let mut other = laptop.connect(&server).await.unwrap();
        assert_eq!(
            schedule_of(&mut other).await,
            json!({ "interval_hours": 24 }),
            "{name}"
        );

        // A changed interval reaches every connected Agent, without a last
        // scan, which each keeps; saving it unchanged sends nothing.
        let saved = admin
            .patch("/v1/settings", json!({ "update_scan_interval_hours": 6 }))
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        for control in [&mut control, &mut other] {
            assert_eq!(
                receive_any_json(control).await,
                json!({ "type": "set_update_scan_schedule", "schedule": { "interval_hours": 6 } }),
                "{name}"
            );
        }
        for change in [
            json!({ "update_scan_interval_hours": 6 }),
            json!({ "instance_name": "Renamed" }),
        ] {
            let saved = admin.patch("/v1/settings", change).await;
            assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        }
        // Only a person's scan, with its run, follows.
        let started = admin
            .post(
                &format!("/v1/agents/{}/updates/scan", desk.device_id),
                json!({}),
            )
            .await;
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        assert_eq!(
            receive_any_json(&mut control).await,
            json!({ "type": "scan_updates", "run_id": started.body["id"] }),
            "{name}"
        );

        // Off, the schedule says so, on a change and on connecting.
        let saved = admin
            .patch("/v1/settings", json!({ "update_scan_interval_hours": 0 }))
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(
            receive_any_json(&mut other).await,
            json!({ "type": "set_update_scan_schedule", "schedule": {} }),
            "{name}"
        );
        drop(control);
        let mut control = desk.connect(&server).await.unwrap();
        let schedule = schedule_of(&mut control).await;
        assert!(
            schedule.get("interval_hours").is_none(),
            "{name}: {schedule}"
        );
        assert_eq!(schedule["last_scan"]["reboot_pending"], true, "{schedule}");
        app.finish().await;
    }
}

#[tokio::test]
async fn the_engine_does_no_work_for_each_device() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut connections = Vec::new();
        let mut work = Vec::new();
        for devices in [2, 20] {
            // Connected, never scanned, so every one is overdue.
            while connections.len() < devices {
                let agent = app
                    .enroll(&mut admin, &format!("Device {}", connections.len()))
                    .await;
                connections.push(app.state.agents.connect(&agent.device_id));
            }
            let before = app.db().counts();
            updates::pass(&app.state, now_ms()).await.unwrap();
            work.push(app.db().counts() - before);
        }
        assert_eq!(work[0], work[1], "{name}");
        assert_eq!(work[0].writes, 0, "{name}: {work:?}");
        drop(connections);
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

/// A page of the Updates page's fleet.
async fn fleet(browser: &mut Browser, query: &str) -> Value {
    let response = browser.get(&format!("/v1/updates/devices{query}")).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{query}: {:?}",
        response.body
    );
    response.body
}

/// The names of a fleet page's devices, in order.
fn names(page: &Value) -> Vec<String> {
    page["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|device| device["device_name"].as_str().unwrap().to_owned())
        .collect()
}

/// A fleet page's device named `name`.
fn named<'a>(page: &'a Value, name: &str) -> &'a Value {
    page["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["device_name"] == name)
        .unwrap()
}

/// Removes the pending run `run_id`, as if its Agent had never got it.
async fn discard(app: &App, run_id: &str) {
    let mut transaction = app.db().begin().await.unwrap();
    updates::runs::discard(&mut transaction, run_id)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
}

/// Records an install on the device, as if a user had started it at
/// `created_at`, and, unless `outcome` is None, the Agent's report of it.
async fn install(app: &App, device_id: &str, created_at: i64, outcome: Option<&str>) {
    use updates::runs;
    let id = format!("run-{device_id}-{created_at}");
    let mut transaction = app.db().begin().await.unwrap();
    runs::insert(
        &mut transaction,
        &runs::NewRun {
            id: &id,
            device_id,
            kind: runs::Kind::Install,
            requested_by_user_id: "user",
            policy_name: None,
            job_id: None,
            install: Some(&runs::Install {
                update_ids: Vec::new(),
                categories: Vec::new(),
                reboot: meshrmm_protocol_types::RebootBehavior::Never,
                reboot_delay_minutes: 5,
            }),
            created_at,
        },
    )
    .await
    .unwrap();
    if let Some(outcome) = outcome {
        let result = serde_json::from_value(json!({ "status": outcome })).unwrap();
        assert!(
            runs::finish_install(&mut transaction, device_id, &id, result, created_at + 1)
                .await
                .unwrap()
        );
    }
    transaction.commit().await.unwrap();
}

#[tokio::test]
async fn the_fleet_is_filtered_counted_and_ordered_by_the_server() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let now = now_ms();
        let acme = admin
            .post("/v1/companies", json!({ "name": "Acme" }))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let servers = admin
            .post("/v1/tags", json!({ "name": "Servers", "color": "blue" }))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let web = app.enroll(&mut admin, "web").await;
        let crit = app.enroll(&mut admin, "crit").await;
        let db = app.enroll(&mut admin, "db").await;
        let mac = app.enroll_on(&mut admin, "Mac", "macos").await;
        let old = app.enroll_on(&mut admin, "old", "linux").await;
        let new = app.enroll_on(&mut admin, "new", "linux").await;
        let zypper = app.enroll_on(&mut admin, "zypper", "linux").await;
        let moved = admin
            .put(
                &format!("/v1/agents/{}/company", web.device_id),
                json!({ "company_id": acme }),
            )
            .await;
        assert_eq!(moved.status, StatusCode::NO_CONTENT, "{name}");
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [web.device_id], "add": [servers], "remove": [] }),
            )
            .await;
        assert_eq!(tagged.status, StatusCode::OK, "{:?}", tagged.body);

        let three = vec![
            update("a", "security"),
            update("b", "other"),
            update("c", "other"),
        ];
        store(&app, &web.device_id, scan_at(1, three, false), now).await;
        store(
            &app,
            &crit.device_id,
            scan_at(1, vec![update("k", "critical")], false),
            now,
        )
        .await;
        store(
            &app,
            &db.device_id,
            scan_at(1, vec![update("x", "other"), update("y", "driver")], false),
            now,
        )
        .await;
        store(&app, &mac.device_id, scan_at(1, vec![], true), now).await;
        store(
            &app,
            &old.device_id,
            scan_at(1, vec![], false),
            now - 5 * DAY_MS,
        )
        .await;
        install(&app, &old.device_id, now - 5 * DAY_MS, Some("partial")).await;
        store(
            &app,
            &zypper.device_id,
            json!({ "scanned_at_unix_ms": 1, "updates": [], "reboot_pending": false }),
            now - 9 * DAY_MS,
        )
        .await;

        // The most security and critical updates first, then the most
        // pending, then by name in any case.
        let all = fleet(&mut admin, "").await;
        assert_eq!(
            names(&all),
            ["web", "crit", "db", "Mac", "new", "old", "zypper"],
            "{name}"
        );
        assert_eq!(all["matching"], 7, "{name}");
        let totals = json!({ "devices": 7, "pending": 3, "security": 2, "failed": 1, "reboot": 1, "stale": 2 });
        assert_eq!(
            all["totals"], totals,
            "{name}: unsupported devices aren't stale"
        );
        let shown = named(&all, "web");
        assert_eq!(shown["pending"], 3, "{name}");
        assert_eq!(shown["security"], 1);
        assert_eq!(shown["critical"], 0);
        assert_eq!(shown["oldest_pending_at"], now);
        assert_eq!(shown["company_id"], acme.as_str());
        assert_eq!(shown["connected"], false);
        assert_eq!(named(&all, "crit")["critical"], 1);
        assert_eq!(named(&all, "old")["last_install"]["status"], "partial");
        assert_eq!(named(&all, "old")["installing"], false);
        let never = named(&all, "new");
        assert_eq!(never["pending"], 0, "{name}: never scanned");
        assert_eq!(never["received_at"], Value::Null);
        assert_eq!(never["supported"], true);
        assert_eq!(named(&all, "zypper")["supported"], false);

        // Each filter narrows the list and what matches, never the totals.
        let by_company = format!("?company={acme}");
        let by_tag = format!("?tag={servers}");
        let filters: [(&str, &[&str]); 14] = [
            ("?query=WE", &["web"]),
            ("?query=%20ma%20", &["Mac"]),
            ("?query=%25", &[]),
            (&by_company, &["web"]),
            (
                "?company=none",
                &["crit", "db", "Mac", "new", "old", "zypper"],
            ),
            (&by_tag, &["web"]),
            ("?platform=macos", &["Mac"]),
            ("?status=pending", &["web", "crit", "db"]),
            ("?status=security", &["web", "crit"]),
            ("?status=failed", &["old"]),
            ("?status=reboot", &["Mac"]),
            ("?status=stale", &["new", "old"]),
            ("?platform=linux&status=stale", &["new", "old"]),
            ("?platform=windows&status=pending&query=d", &["db"]),
        ];
        for (query, expected) in filters {
            let page = fleet(&mut admin, query).await;
            assert_eq!(names(&page), expected, "{name}: {query}");
            assert_eq!(page["matching"], expected.len(), "{name}: {query}");
            assert_eq!(page["totals"], totals, "{name}: {query}");
        }

        // Pages, and only the counts.
        let mut paged = Vec::new();
        for offset in [0, 3, 6, 9] {
            let page = fleet(&mut admin, &format!("?limit=3&offset={offset}")).await;
            assert_eq!(page["matching"], 7, "{name}");
            paged.extend(names(&page));
        }
        assert_eq!(paged, names(&all), "{name}");
        let counts = fleet(&mut admin, "?limit=0&status=failed").await;
        assert_eq!(counts["devices"], json!([]), "{name}");
        assert_eq!(counts["matching"], 1);
        assert_eq!(counts["totals"], totals);

        // The latest install is the one that counts.
        install(&app, &old.device_id, now, None).await;
        let page = fleet(&mut admin, "?status=failed").await;
        assert!(names(&page).is_empty(), "{name}");
        assert_eq!(page["totals"]["failed"], 0);
        let page = fleet(&mut admin, "?query=old").await;
        assert_eq!(page["devices"][0]["installing"], true, "{name}");
        assert_eq!(page["devices"][0]["last_install"]["status"], "pending");

        // A device that never scanned has no status row until its first
        // install, here one that never reported: lost, so failed, and still
        // never scanned.
        install(&app, &new.device_id, now - 10 * DAY_MS, None).await;
        updates::sweep(&app.state, now).await.unwrap();
        let page = fleet(&mut admin, "?status=failed").await;
        assert_eq!(names(&page), ["new"], "{name}");
        let row = &page["devices"][0];
        assert_eq!(row["last_install"]["status"], "lost", "{name}");
        assert_eq!(row["received_at"], Value::Null, "{name}");
        assert_eq!(row["supported"], true);
        assert_eq!(row["pending"], 0);
        let stale = fleet(&mut admin, "?status=stale").await;
        assert_eq!(names(&stale), ["new", "old"], "{name}");

        // An install the Agent never got is discarded: the one before it is
        // the latest again, or none.
        install(&app, &new.device_id, now, None).await;
        assert_eq!(
            fleet(&mut admin, "?status=failed").await["matching"],
            0,
            "{name}"
        );
        discard(&app, &format!("run-{}-{now}", new.device_id)).await;
        let page = fleet(&mut admin, "?status=failed").await;
        assert_eq!(names(&page), ["new"], "{name}");
        assert_eq!(page["devices"][0]["last_install"]["status"], "lost");
        install(&app, &crit.device_id, now, None).await;
        discard(&app, &format!("run-{}-{now}", crit.device_id)).await;
        let page = fleet(&mut admin, "?query=crit").await;
        assert_eq!(page["devices"][0]["last_install"], Value::Null, "{name}");
        assert_eq!(page["devices"][0]["installing"], false);

        // An install older than the history keeps counts no more.
        install(&app, &db.device_id, now - 366 * DAY_MS, Some("failed")).await;
        let page = fleet(&mut admin, "?query=db").await;
        assert_eq!(page["devices"][0]["last_install"], Value::Null, "{name}");
        assert_eq!(page["totals"]["failed"], 1, "{name}");

        // Unknown or wrong parameters are refused.
        for query in [
            "?nope=1",
            "?status=broken",
            "?platform=beos",
            "?limit=-1",
            "?company=..%2Fx",
        ] {
            let refused = admin.get(&format!("/v1/updates/devices{query}")).await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}: {query}");
        }
        app.finish().await;
    }
}

/// Adds `count` devices from the `first`th, named `{prefix} {n % 37}` so
/// some share names, straight to the database, each with a scan listing a
/// few updates, some of them security updates.
async fn add_devices(app: &App, prefix: &str, first: usize, count: usize, now: i64) {
    let numbers = (first..first + count).collect::<Vec<_>>();
    for chunk in numbers.chunks(100) {
        let mut insert = sea_query::Query::insert();
        insert.into_table(sea_query::Alias::new("agents")).columns(
            [
                "id",
                "name",
                "platform",
                "auth_token_hash",
                "created_at",
                "updated_at",
            ]
            .map(sea_query::Alias::new),
        );
        for n in chunk {
            insert.values_panic([
                format!("{prefix}-{n:04}").into(),
                format!("{prefix} {}", n % 37).into(),
                "linux".into(),
                "a".repeat(64).into(),
                now.into(),
                now.into(),
            ]);
        }
        app.db().execute(&insert).await.unwrap();
    }
    for n in numbers {
        let updates = (0..n % 5)
            .map(|i| update(&format!("s{i}"), "security"))
            .chain((0..n % 3).map(|i| update(&format!("o{i}"), "other")))
            .collect();
        store(
            app,
            &format!("{prefix}-{n:04}"),
            scan_at(1, updates, false),
            now,
        )
        .await;
    }
}

#[tokio::test]
async fn fleet_pages_cost_the_same_for_any_fleet_and_miss_no_device() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let now = now_ms();
        // Authenticating the browser's first request is its own work.
        fleet(&mut admin, "?limit=2").await;
        let mut costs = Vec::new();
        for (first, count) in [(0, 3), (3, 500)] {
            add_devices(&app, "device", first, count, now).await;
            let before = app.db().counts();
            let page = fleet(&mut admin, "?limit=2").await;
            costs.push(app.db().counts() - before);
            assert_eq!(page["devices"].as_array().unwrap().len(), 2, "{name}");
            assert_eq!(page["totals"]["devices"], first + count, "{name}");
        }
        assert_eq!(costs[0], costs[1], "{name}");
        assert_eq!(costs[0].writes, 0, "{name}: {costs:?}");

        // Paging through the fleet sees each device once, in order.
        let mut seen = Vec::new();
        loop {
            let page = fleet(&mut admin, &format!("?offset={}", seen.len())).await;
            let devices = page["devices"].as_array().unwrap().clone();
            if devices.is_empty() {
                break;
            }
            assert!(devices.len() <= 100, "{name}: 100 by default");
            seen.extend(devices);
        }
        assert_eq!(seen.len(), 503, "{name}");
        let ids = seen
            .iter()
            .map(|device| device["device_id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 503, "{name}: no device twice");
        let key = |device: &Value| {
            (
                -(device["security"].as_i64().unwrap() + device["critical"].as_i64().unwrap()),
                -device["pending"].as_i64().unwrap(),
                device["device_name"].as_str().unwrap().to_lowercase(),
                device["device_id"].as_str().unwrap().to_owned(),
            )
        };
        let mut sorted = seen.clone();
        sorted.sort_by_key(key);
        assert_eq!(seen, sorted, "{name}");
        let biggest = fleet(&mut admin, "?limit=500").await;
        assert_eq!(
            biggest["devices"].as_array().unwrap().len(),
            200,
            "{name}: at most 200 a page"
        );
        app.finish().await;
    }
}
