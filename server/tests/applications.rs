//! Third-party applications: the library, Agents' application scans over
//! the control connection, installs, updates and uninstalls on a device and
//! as jobs and policies, custom installer downloads, alerts, lost runs, and
//! API tokens.
mod common;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use common::{App, Browser, Ws, apps, audit_events, receive_json, send_json, set_up, user_with};
use meshrmm_server::{
    alerts, applications, automation,
    time::{DAY_MS, HOUR_MS, now_ms},
};
use sea_query::ExprTrait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const INSTALLER: &[u8] = b"MZ pretend installer";

fn package(source: &str, id: &str, name: &str, version: &str, available: Option<&str>) -> Value {
    let mut package = json!({ "source": source, "id": id, "name": name, "version": version });
    if let Some(available) = available {
        package["available_version"] = available.into();
    }
    package
}

fn scan(managers: Value, packages: Vec<Value>) -> Value {
    json!({ "scanned_at_unix_ms": 1_700_000_000_000_i64, "managers": managers, "packages": packages })
}

async fn report_scan(control: &mut Ws, run_ids: &[&str], scan: Value) {
    send_json(
        control,
        json!({ "type": "application_scan", "run_ids": run_ids, "scan": scan }),
    )
    .await;
}

async fn packages(browser: &mut Browser, device_id: &str) -> Value {
    let response = browser
        .get(&format!("/v1/agents/{device_id}/packages"))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    response.body
}

/// Waits until the device's packages satisfy `ready`, as the server handles
/// the Agent's messages in the background.
async fn packages_where(
    browser: &mut Browser,
    device_id: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..100 {
        let packages = packages(browser, device_id).await;
        if ready(&packages) {
            return packages;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the device's packages never got there");
}

async fn run(browser: &mut Browser, id: &str) -> Value {
    let response = browser.get(&format!("/v1/application-runs/{id}")).await;
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

fn sha256(bytes: &[u8]) -> String {
    meshrmm_server::secrets::hex(&Sha256::digest(bytes))
}

/// Uploads a toolbox file and returns its ID.
async fn upload(browser: &Browser, name: &str, shared: bool) -> String {
    let request = browser
        .request(
            Method::POST,
            &format!(
                "/v1/toolbox/files?name={name}&folder=Installers&shared={shared}&sha256={}",
                sha256(INSTALLER)
            ),
        )
        .header("content-type", "application/octet-stream")
        .body(Body::from(INSTALLER.to_vec()))
        .unwrap();
    let response = browser.clone().raw(request).await;
    assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
    response.body["id"].as_str().unwrap().to_owned()
}

async fn add_application(browser: &mut Browser, body: Value) -> String {
    let created = browser.post("/v1/applications", body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

fn firefox() -> Value {
    json!({
        "name": "Firefox",
        "description": "The browser",
        "sources": [
            { "source": "winget", "package_id": "Mozilla.Firefox" },
            { "source": "chocolatey", "package_id": "firefox" },
            { "source": "flatpak", "package_id": "org.mozilla.firefox" },
        ],
    })
}

/// The next command of `kind`, skipping the scans and refreshes a
/// connection gets.
async fn command_of(control: &mut Ws, kind: &str) -> Value {
    loop {
        let command = receive_json(control).await;
        if command["type"] == kind {
            return command;
        }
    }
}

#[tokio::test]
async fn the_library_checks_what_it_installs() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let id = add_application(&mut admin, firefox()).await;
        let listed = admin.get("/v1/applications").await;
        assert_eq!(listed.status, StatusCode::OK, "{name}");
        let first = &listed.body["applications"][0];
        assert_eq!(first["name"], "Firefox");
        assert_eq!(first["installed"], 0);
        assert_eq!(first["sources"][0]["platform"], "windows");
        assert_eq!(first["sources"][2]["platform"], "linux");

        // Package IDs can't carry options or shell text.
        for bad in ["--override calc", "Mozilla.Firefox&calc", "-h"] {
            let refused = admin
                .post(
                    "/v1/applications",
                    json!({ "name": "Bad", "sources": [{ "source": "winget", "package_id": bad }] }),
                )
                .await;
            assert_eq!(refused.code(), "invalid_application", "{name}: {bad}");
        }
        let taken = admin.post("/v1/applications", firefox()).await;
        assert_eq!(taken.code(), "application_name_taken", "{name}");

        // Custom installers run shared toolbox files only.
        let private = upload(&admin, "agent.msi", false).await;
        let custom = |file_id: &str| {
            json!({
                "name": "Vendor Agent",
                "pinned_version": "4.2",
                "sources": [{
                    "source": "custom", "platform": "windows", "file_id": file_id,
                    "install_arguments": "/qn /norestart",
                    "uninstall_command": "msiexec /x {11111111-2222-3333-4444-555555555555} /qn",
                    "detection": { "kind": "application", "name": "Vendor Agent*", "min_version": "4.2" },
                }],
            })
        };
        let refused = admin.post("/v1/applications", custom(&private)).await;
        assert_eq!(refused.code(), "file_not_shared", "{name}");
        let shared = upload(&admin, "vendor.msi", true).await;
        let vendor = add_application(&mut admin, custom(&shared)).await;
        let read = admin.get(&format!("/v1/applications/{vendor}")).await;
        assert_eq!(read.body["sources"][0]["file_name"], "vendor.msi", "{name}");
        assert_eq!(read.body["pinned_version"], "4.2");

        // A file the library installs with can't be deleted or unshared.
        let deleted = admin.delete(&format!("/v1/toolbox/files/{shared}")).await;
        assert_eq!(deleted.code(), "file_in_use", "{name}");
        let unshared = admin
            .put(
                &format!("/v1/toolbox/files/{shared}"),
                json!({ "name": "vendor.msi", "folder": "Installers", "shared": false }),
            )
            .await;
        assert_eq!(unshared.code(), "file_in_use", "{name}");

        // Technicians see the library but don't change it.
        let (mut tech, _) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "applications.view", "applications.deploy"],
        )
        .await;
        assert_eq!(
            tech.get("/v1/applications").await.status,
            StatusCode::OK,
            "{name}"
        );
        assert_eq!(
            tech.post("/v1/applications", json!({ "name": "x", "sources": [] }))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        let renamed = admin
            .put(
                &format!("/v1/applications/{id}"),
                json!({ "name": "Mozilla Firefox", "sources": [{ "source": "winget", "package_id": "Mozilla.Firefox" }] }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}: {:?}", renamed.body);
        assert_eq!(renamed.body["sources"].as_array().unwrap().len(), 1);
        assert_eq!(
            admin
                .delete(&format!("/v1/applications/{vendor}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/toolbox/files/{shared}"))
                .await
                .status,
            StatusCode::NO_CONTENT,
            "{name}: free again"
        );
        assert_eq!(audit_events(&app, "application.create").await.len(), 2);
        assert_eq!(audit_events(&app, "application.update").await.len(), 1);
        assert_eq!(audit_events(&app, "application.delete").await.len(), 1);
        app.finish().await;
    }
}

#[tokio::test]
async fn scans_install_update_and_uninstall_on_a_device() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let installer = upload(&admin, "tool.exe", true).await;
        let tool_id = add_application(
            &mut admin,
            json!({
                "name": "Tool",
                "sources": [{
                    "source": "custom", "platform": "windows", "file_id": installer,
                    "install_arguments": "/S",
                    "detection": { "kind": "file", "path": "%ProgramFiles%\\Tool\\tool.exe" },
                }],
            }),
        )
        .await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let never = packages(&mut admin, &desk.device_id).await;
        assert_eq!(never["status"], Value::Null, "{name}");

        // Update scans bring application scans; the Agent reports them.
        let mut control = desk.connect(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan(
                json!([{ "source": "winget", "version": "1.29.380" }, { "source": "chocolatey", "version": "2.7.3" }]),
                vec![
                    package("winget", "Mozilla.Firefox", "Mozilla Firefox", "130.0", Some("131.0")),
                    package("winget", "Git.Git", "Git", "2.55.0", None),
                    package("chocolatey", "7zip", "7-Zip", "24.08", None),
                    // Unusable IDs are dropped.
                    package("winget", "--all", "Bad", "1", None),
                ],
            ),
        )
        .await;
        let listed = packages_where(&mut admin, &desk.device_id, |body| {
            !body["status"].is_null()
        })
        .await;
        let ids = listed["packages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|package| package["package_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["Mozilla.Firefox", "7zip", "Git.Git"], "{name}");
        assert_eq!(listed["packages"][0]["application_id"], firefox_id);
        let since = listed["packages"][0]["outdated_since"].as_i64().unwrap();
        let library = admin.get("/v1/applications").await.body;
        let firefox_row = library["applications"]
            .as_array()
            .unwrap()
            .iter()
            .find(|application| application["id"] == firefox_id)
            .unwrap();
        assert_eq!(
            (&firefox_row["installed"], &firefox_row["outdated"]),
            (&json!(1), &json!(1)),
            "{name}"
        );
        let fleet = admin.get("/v1/packages/outdated").await.body;
        assert_eq!(fleet["packages"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(fleet["packages"][0]["devices"][0]["device_name"], "Desk");

        // A failed manager keeps what it listed; outdated_since stays.
        report_scan(
            &mut control,
            &[],
            scan(
                json!([{ "source": "winget", "version": "1.29.380" }, { "source": "chocolatey", "error": "choco failed" }]),
                vec![package("winget", "Mozilla.Firefox", "Mozilla Firefox", "130.0", Some("131.0"))],
            ),
        )
        .await;
        let kept = packages_where(&mut admin, &desk.device_id, |body| {
            body["packages"].as_array().unwrap().len() == 2
        })
        .await;
        assert_eq!(kept["packages"][0]["outdated_since"], since, "{name}");
        assert_eq!(kept["packages"][1]["package_id"], "7zip");

        // Scan now.
        let scanning = admin
            .post(
                &format!("/v1/agents/{}/packages/scan", desk.device_id),
                json!({}),
            )
            .await;
        assert_eq!(scanning.status, StatusCode::ACCEPTED, "{:?}", scanning.body);
        let scan_id = scanning.body["id"].as_str().unwrap().to_owned();
        let command = command_of(&mut control, "scan_applications").await;
        assert_eq!(command["run_id"], scan_id, "{name}");
        report_scan(
            &mut control,
            &[&scan_id],
            scan(
                json!([{ "source": "winget" }, { "source": "chocolatey", "error": "choco failed" }]),
                vec![package("winget", "Mozilla.Firefox", "Mozilla Firefox", "130.0", Some("131.0"))],
            ),
        )
        .await;
        run_where(&mut admin, &scan_id, |run| run["status"] == "succeeded").await;

        // Install Firefox and the custom tool.
        let started = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "install", "application_ids": [firefox_id, tool_id] }),
            )
            .await;
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        assert_eq!(started.body["summary"], "Install Firefox and Tool");
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        let busy = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "update" , "mode": "all" }),
            )
            .await;
        assert_eq!(busy.code(), "application_run_running", "{name}");
        let command = command_of(&mut control, "run_applications").await;
        let request = &command["run"];
        assert_eq!(request["run_id"], run_id);
        let items = request["operation"]["items"].as_array().unwrap();
        assert_eq!(request["operation"]["action"], "install");
        // Windows sources only, in order.
        let sources = items[0]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["source"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(sources, ["winget", "chocolatey"], "{name}");
        let custom = &items[1]["sources"][0]["custom"];
        assert_eq!(custom["file_name"], "tool.exe");
        assert_eq!(custom["sha256"], sha256(INSTALLER));
        assert_eq!(custom["arguments"], "/S");

        // The Agent downloads the installer while the run is pending.
        let download = |file_id: &str| {
            desk.request(
                Method::GET,
                &format!(
                    "/v1/agents/{}/application-runs/{run_id}/files/{file_id}",
                    desk.device_id
                ),
            )
            .body(Body::empty())
            .unwrap()
        };
        let fetched = desk.send(download(&installer)).await;
        assert_eq!(fetched.status, StatusCode::OK, "{name}");
        assert_eq!(fetched.body, INSTALLER);
        let other = upload(&admin, "other.exe", true).await;
        assert_eq!(
            desk.send(download(&other)).await.status,
            StatusCode::NOT_FOUND,
            "{name}: only the run's files"
        );
        send_json(
            &mut control,
            json!({ "type": "application_progress", "run_id": run_id, "message": "Installing Tool (2 of 2)" }),
        )
        .await;
        run_where(&mut admin, &run_id, |run| {
            run["progress"] == "Installing Tool (2 of 2)"
        })
        .await;
        send_json(
            &mut control,
            json!({ "type": "application_result", "run_id": run_id, "result": {
                "status": "partial",
                "items": [
                    { "index": 0, "name": "Firefox", "source": "winget", "package_id": "Mozilla.Firefox",
                      "outcome": "unchanged", "version": "130.0", "output": "" },
                    { "index": 1, "name": "Tool", "source": "custom", "outcome": "failed",
                      "exit_code": 1603, "error": "The installer exited with 1603", "output": "Fatal error during installation." },
                ],
            } }),
        )
        .await;
        let done = run_where(&mut admin, &run_id, |run| run["status"] == "partial").await;
        assert_eq!(done["failed"], 1, "{name}");
        assert_eq!(done["items"][1]["exit_code"], 1603);
        assert_eq!(
            done["items"][1]["output"],
            "Fatal error during installation."
        );
        assert_eq!(
            desk.send(download(&installer)).await.status,
            StatusCode::NOT_FOUND,
            "{name}: not once it finished"
        );
        let history = admin
            .get(&format!(
                "/v1/application-runs?device_id={}",
                desk.device_id
            ))
            .await;
        assert_eq!(history.body["runs"][0]["id"], run_id, "{name}");
        assert!(
            history.body["runs"][0]["items"][1].get("output").is_none(),
            "lists leave output out"
        );
        let audited = &audit_events(&app, "application.install").await[0];
        assert_eq!(audited.target_id, desk.device_id, "{name}");

        // Update all but Git; uninstall a package and an inventory program.
        let update = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "update", "mode": "except", "packages": ["winget:Git.Git"] }),
            )
            .await;
        assert_eq!(update.status, StatusCode::ACCEPTED, "{:?}", update.body);
        let command = command_of(&mut control, "run_applications").await;
        assert_eq!(
            command["run"]["operation"]["choice"],
            json!({ "mode": "except", "packages": [{ "source": "winget", "id": "Git.Git" }] }),
            "{name}"
        );
        let update_id = update.body["id"].as_str().unwrap().to_owned();
        send_json(
            &mut control,
            json!({ "type": "application_result", "run_id": update_id, "result": { "status": "succeeded", "items": [] } }),
        )
        .await;
        run_where(&mut admin, &update_id, |run| run["status"] == "succeeded").await;
        let uninstall = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({
                    "action": "uninstall",
                    "packages": ["chocolatey:7zip"],
                    "inventory": [{ "name": "Old Toolbar", "version": "1.0" }],
                }),
            )
            .await;
        assert_eq!(
            uninstall.status,
            StatusCode::ACCEPTED,
            "{:?}",
            uninstall.body
        );
        let command = command_of(&mut control, "run_applications").await;
        assert_eq!(
            command["run"]["operation"]["items"],
            json!([
                { "method": "package", "name": "7-Zip", "package": { "source": "chocolatey", "id": "7zip" } },
                { "method": "inventory", "name": "Old Toolbar", "version": "1.0" },
            ]),
            "{name}"
        );
        assert_eq!(audit_events(&app, "application.uninstall").await.len(), 1);
        let uninstall_id = uninstall.body["id"].as_str().unwrap().to_owned();
        send_json(
            &mut control,
            json!({ "type": "application_result", "run_id": uninstall_id, "result": { "status": "succeeded", "items": [] } }),
        )
        .await;
        run_where(&mut admin, &uninstall_id, |run| {
            run["status"] == "succeeded"
        })
        .await;
        // Installs are of library applications.
        let refused = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "install", "packages": ["winget:Git.Git"] }),
            )
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn application_jobs_and_policies_run_on_each_device() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mac = app.enroll_on(&mut admin, "Mac", "macos").await;
        let mut control = desk.connect(&server).await.unwrap();

        let job = admin
            .post(
                "/v1/jobs",
                json!({
                    "applications": { "action": "install", "application_ids": [firefox_id] },
                    "target": "devices", "device_ids": [desk.device_id, mac.device_id],
                    "offline": "skip",
                }),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{:?}", job.body);
        assert_eq!(job.body["kind"], "app_install", "{name}");
        assert_eq!(job.body["item_name"], "Install Firefox");
        let outcome = |job: &Value, device: &str| {
            job["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["device_name"] == device)
                .unwrap()
                .clone()
        };
        let skipped = outcome(&job.body, "Mac");
        assert_eq!(skipped["outcome"], "skipped", "{name}");
        assert_eq!(skipped["detail"], "None of the applications is for macOS");
        let job_id = job.body["id"].as_str().unwrap().to_owned();
        automation::pass(&app.state, now_ms()).await.unwrap();
        let command = command_of(&mut control, "run_applications").await;
        let run_id = command["run"]["run_id"].as_str().unwrap().to_owned();
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        assert_eq!(outcome(&job, "Desk")["outcome"], "running", "{name}");
        assert_eq!(outcome(&job, "Desk")["application_run_id"], run_id);
        send_json(
            &mut control,
            json!({ "type": "application_result", "run_id": run_id, "result": {
                "status": "failed",
                "items": [{ "index": 0, "name": "Firefox", "outcome": "failed", "error": "winget failed", "output": "" }],
            } }),
        )
        .await;
        run_where(&mut admin, &run_id, |run| run["status"] == "failed").await;
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        assert_eq!(outcome(&job, "Desk")["outcome"], "failed", "{name}");
        assert_eq!(outcome(&job, "Desk")["detail"], "1 application failed");

        // An install policy, on a schedule, also covers newly enrolled
        // devices; the library application it names can't be deleted.
        let policy = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Browsers", "action": "app_install",
                    "applications": { "application_ids": [firefox_id] },
                    "target": "all", "trigger": "daily", "time": "03:00", "time_zone": "UTC",
                    "offline": "wait",
                }),
            )
            .await;
        assert_eq!(policy.status, StatusCode::CREATED, "{:?}", policy.body);
        assert_eq!(
            policy.body["applications"]["application_ids"],
            json!([firefox_id])
        );
        let policy_id = policy.body["id"].as_str().unwrap().to_owned();
        let in_use = admin
            .delete(&format!("/v1/applications/{firefox_id}"))
            .await;
        assert_eq!(in_use.code(), "application_in_use", "{name}");
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let jobs = admin
            .get(&format!("/v1/jobs?policy_id={policy_id}"))
            .await
            .body;
        assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(jobs["jobs"][0]["target_label"], "Laptop");
        assert_eq!(jobs["jobs"][0]["counts"]["waiting"], 1);
        drop(laptop);

        // An update policy names what it leaves alone.
        let update = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Weekly app updates", "action": "app_update",
                    "applications": { "mode": "except", "packages": ["winget:Git.Git"] },
                    "target": "all", "trigger": "weekly", "weekdays": [6], "time": "02:00",
                    "time_zone": "UTC", "offline": "wait",
                }),
            )
            .await;
        assert_eq!(update.status, StatusCode::CREATED, "{:?}", update.body);
        let update_id = update.body["id"].as_str().unwrap().to_owned();
        let ran = admin
            .post(
                &format!("/v1/automation-policies/{update_id}/run"),
                json!({}),
            )
            .await;
        assert_eq!(ran.status, StatusCode::CREATED, "{:?}", ran.body);
        assert_eq!(
            ran.body["item_name"], "Update all applications except Git.Git",
            "{name}"
        );
        let invalid = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Nothing", "action": "app_install", "target": "all",
                    "trigger": "interval", "interval_minutes": 60, "offline": "skip",
                }),
            )
            .await;
        assert_eq!(invalid.code(), "invalid_policy", "{name}");

        // Technicians deploy but don't manage policies.
        let (mut tech, _) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "applications.view"],
        )
        .await;
        let refused = tech
            .post(
                "/v1/jobs",
                json!({
                    "applications": { "action": "update", "mode": "all" },
                    "target": "devices", "device_ids": [desk.device_id], "offline": "skip",
                }),
            )
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        app.finish().await;
    }
}

/// Makes the device's outdated packages look `days` old.
async fn age(app: &App, device_id: &str, days: i64) {
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new("device_packages"))
                .value(
                    sea_query::Alias::new("outdated_since"),
                    now_ms() - days * DAY_MS,
                )
                .and_where(sea_query::Expr::col(sea_query::Alias::new("device_id")).eq(device_id))
                .and_where(
                    sea_query::Expr::col(sea_query::Alias::new("outdated_since")).is_not_null(),
                )
                .to_owned(),
        )
        .await
        .unwrap();
}

async fn alerts_of(browser: &mut Browser) -> Vec<Value> {
    let response = browser.get("/v1/alerts?state=open").await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    response.body["alerts"].as_array().unwrap().clone()
}

#[tokio::test]
async fn outdated_and_failed_applications_raise_alerts() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        for rule in [
            json!({ "name": "Stale apps", "severity": "warning", "condition": "app_updates_pending", "days": 7 }),
            json!({ "name": "App failed", "severity": "critical", "condition": "app_install_failed" }),
        ] {
            let created = admin.post("/v1/alert-rules", rule).await;
            assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        }
        let without_days = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Bad", "severity": "info", "condition": "app_updates_pending" }),
            )
            .await;
        assert_eq!(without_days.code(), "invalid_rule", "{name}");
        let rules = admin.get("/v1/alert-rules").await.body;
        let described = rules
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["description"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(
            described.contains(&"Application updates pending for 7 days".to_owned()),
            "{described:?}"
        );

        let mut control = desk.connect(&server).await.unwrap();
        report_scan(
            &mut control,
            &[],
            scan(
                json!([{ "source": "winget" }]),
                vec![package(
                    "winget",
                    "Mozilla.Firefox",
                    "Mozilla Firefox",
                    "130.0",
                    Some("131.0"),
                )],
            ),
        )
        .await;
        packages_where(&mut admin, &desk.device_id, |body| {
            !body["status"].is_null()
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
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        assert!(
            open[0]["detail"].as_str().unwrap().starts_with(
                "1 application update pending for more than 7 days, the oldest (Mozilla Firefox)"
            ),
            "{}",
            open[0]["detail"]
        );
        // Updating clears it.
        report_scan(
            &mut control,
            &[],
            scan(
                json!([{ "source": "winget" }]),
                vec![package(
                    "winget",
                    "Mozilla.Firefox",
                    "Mozilla Firefox",
                    "131.0",
                    None,
                )],
            ),
        )
        .await;
        packages_where(&mut admin, &desk.device_id, |body| {
            body["packages"][0]["available_version"].is_null()
        })
        .await;
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        assert!(alerts_of(&mut admin).await.is_empty(), "{name}");

        // Each application that fails to install raises its own alert.
        let started = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "install", "application_ids": [firefox_id] }),
            )
            .await;
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        send_json(
            &mut control,
            json!({ "type": "application_result", "run_id": run_id, "result": {
                "status": "failed",
                "items": [{ "index": 0, "name": "Firefox", "outcome": "failed", "exit_code": 2316632080_i64,
                            "error": "No applicable installer", "output": "" }],
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
        assert_eq!(raised[0]["subject"], "Firefox");
        assert_eq!(
            raised[0]["detail"],
            "Installing Firefox failed: No applicable installer"
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
                &format!("/v1/agents/{}/packages/scan", desk.device_id),
                json!({}),
            )
            .await;
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        applications::sweep(&app.state, now_ms() + HOUR_MS / 2)
            .await
            .unwrap();
        assert_eq!(
            run(&mut admin, &run_id).await["status"],
            "pending",
            "{name}"
        );
        applications::sweep(&app.state, now_ms() + HOUR_MS)
            .await
            .unwrap();
        let lost = run(&mut admin, &run_id).await;
        assert_eq!(lost["status"], "lost", "{name}");
        assert_eq!(lost["error"], "The device never reported how it went");
        app.finish().await;
    }
}

#[tokio::test]
async fn api_tokens_deploy_applications() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut control = desk.connect(&server).await.unwrap();
        let created = admin
            .post(
                "/v1/api-tokens",
                json!({ "name": "Patching", "expires_in_days": 30 }),
            )
            .await;
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        let call = |method: Method, path: String, body: Option<Value>| {
            let request = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {secret}"));
            match body {
                Some(body) => request
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string())),
                None => request.body(Body::empty()),
            }
            .unwrap()
        };
        let outdated = app
            .browser()
            .raw(call(Method::GET, "/v1/packages/outdated".into(), None))
            .await;
        assert_eq!(outdated.status, StatusCode::OK, "{name}");
        let started = app
            .browser()
            .raw(call(
                Method::POST,
                format!("/v1/agents/{}/application-runs", desk.device_id),
                Some(json!({ "action": "install", "application_ids": [firefox_id] })),
            ))
            .await;
        assert_eq!(
            started.status,
            StatusCode::ACCEPTED,
            "{name}: {:?}",
            started.body
        );
        command_of(&mut control, "run_applications").await;
        let event = &audit_events(&app, "application.install").await[0];
        assert!(event.api_token.is_some(), "{name}: the token is recorded");
        app.finish().await;
    }
}

#[tokio::test]
async fn technicians_see_and_deploy_applications() {
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
        assert!(
            permissions.contains(&json!("applications.view")),
            "{}",
            app.name
        );
        assert!(permissions.contains(&json!("applications.deploy")));
        assert!(!permissions.contains(&json!("applications.manage")));
        app.finish().await;
    }
}
