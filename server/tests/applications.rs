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

        // Custom installers run as SYSTEM or root: adding or changing one
        // needs scripts.run as well as applications.manage.
        let (mut librarian, _) = user_with(
            &app,
            &mut admin,
            "librarian@example.com",
            &["applications.view", "applications.manage"],
        )
        .await;
        let refused = librarian
            .post("/v1/applications", {
                let mut body = custom(&shared);
                body["name"] = "Vendor Agent 2".into();
                body
            })
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        assert!(
            refused.body.to_string().contains("scripts.run"),
            "{name}: {:?}",
            refused.body
        );
        let mut edited = custom(&shared);
        edited["sources"][0]["install_arguments"] = "/qn & calc.exe".into();
        let refused = librarian
            .put(&format!("/v1/applications/{vendor}"), edited.clone())
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        let mut detected = custom(&shared);
        detected["sources"][0]["detection"] = json!({ "kind": "file", "path": "C:\\x" });
        assert_eq!(
            librarian
                .put(&format!("/v1/applications/{vendor}"), detected)
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{name}: the detection rule is the installer's too"
        );
        // Package managers' sources, and the rest of an application with a
        // custom installer it leaves as it is, stay applications.manage's.
        let mut described = custom(&shared);
        described["description"] = "Unchanged installer".into();
        described["sources"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "source": "chocolatey", "package_id": "vendor-agent" }));
        let saved = librarian
            .put(&format!("/v1/applications/{vendor}"), described)
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        let packaged = librarian
            .post(
                "/v1/applications",
                json!({ "name": "7-Zip", "sources": [{ "source": "winget", "package_id": "7zip.7zip" }] }),
            )
            .await;
        assert_eq!(
            packaged.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            packaged.body
        );
        // So does an API token limited to applications.manage.
        let token = admin
            .post(
                "/v1/api-tokens",
                json!({ "name": "Library", "expires_in_days": 30, "permissions": ["applications.view", "applications.manage"] }),
            )
            .await;
        assert_eq!(
            token.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            token.body
        );
        let secret = token.body["secret"].as_str().unwrap().to_owned();
        let request = axum::http::Request::builder()
            .method(Method::PUT)
            .uri(format!("/v1/applications/{vendor}"))
            .header("authorization", format!("Bearer {secret}"))
            .header("content-type", "application/json")
            .body(Body::from(edited.to_string()))
            .unwrap();
        assert_eq!(
            app.browser().raw(request).await.status,
            StatusCode::FORBIDDEN,
            "{name}: a limited token"
        );
        assert_eq!(
            admin
                .put(&format!("/v1/applications/{vendor}"), edited)
                .await
                .status,
            StatusCode::OK,
            "{name}: administrators have scripts.run"
        );
        assert_eq!(
            librarian
                .delete(&format!(
                    "/v1/applications/{}",
                    packaged.body["id"].as_str().unwrap()
                ))
                .await
                .status,
            StatusCode::NO_CONTENT
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
        // Library edits and update runs are different audit kinds.
        assert_eq!(audit_events(&app, "application.create").await.len(), 3);
        assert_eq!(audit_events(&app, "application.edit").await.len(), 3);
        assert!(audit_events(&app, "application.update").await.is_empty());
        assert_eq!(audit_events(&app, "application.delete").await.len(), 2);
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

/// Stores a scan as if the device's Agent reported it at `now`.
async fn store(app: &App, device_id: &str, scan: Value, now: i64) -> applications::store::Stored {
    applications::store::store_scan(
        app.db(),
        device_id,
        serde_json::from_value(scan).unwrap(),
        now,
    )
    .await
    .unwrap()
}

fn scan_at(scanned_at: i64, managers: &Value, packages: Vec<Value>) -> Value {
    let mut scan = scan(managers.clone(), packages);
    scan["scanned_at_unix_ms"] = scanned_at.into();
    scan
}

/// Renames the device's stored package `id` behind the store's back, so a
/// rewrite of the packages shows.
async fn mark(app: &App, device_id: &str, id: &str) {
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new("device_packages"))
                .value(sea_query::Alias::new("name"), "Marked")
                .and_where(sea_query::Expr::col(sea_query::Alias::new("device_id")).eq(device_id))
                .and_where(sea_query::Expr::col(sea_query::Alias::new("package_id")).eq(id))
                .to_owned(),
        )
        .await
        .unwrap();
}

/// The names of the device's packages, in order.
fn names(listed: &Value) -> Vec<&str> {
    listed["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| package["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn scans_that_find_the_same_packages_write_only_their_status() {
    use applications::store::Stored;
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let id = desk.device_id.as_str();
        let now = now_ms();
        let managers =
            json!([{ "source": "winget", "version": "1.29" }, { "source": "chocolatey" }]);
        let firefox = || {
            package(
                "winget",
                "Mozilla.Firefox",
                "Mozilla Firefox",
                "130.0",
                Some("131.0"),
            )
        };
        let found = || {
            vec![
                firefox(),
                package("chocolatey", "7zip", "7-Zip", "24.08", None),
            ]
        };
        assert_eq!(
            store(&app, id, scan_at(1, &managers, found()), now).await,
            Stored::Replaced,
            "{name}"
        );
        let since = packages(&mut admin, id).await["packages"][0]["outdated_since"].clone();
        assert!(since.is_i64(), "{name}");
        mark(&app, id, "7zip").await;

        // The same packages in another order, from a newer winget: the
        // packages stay, the status says when the scan was and what found
        // them.
        let mut reversed = found();
        reversed.reverse();
        let newer = json!([{ "source": "winget", "version": "1.30" }, { "source": "chocolatey" }]);
        assert_eq!(
            store(&app, id, scan_at(2, &newer, reversed), now + 1_000).await,
            Stored::Unchanged,
            "{name}"
        );
        let listed = packages(&mut admin, id).await;
        assert_eq!(
            names(&listed),
            ["Mozilla Firefox", "Marked"],
            "{name}: not rewritten"
        );
        assert_eq!(listed["packages"][0]["outdated_since"], since);
        assert_eq!(listed["status"]["scanned_at"], 2, "{name}");
        assert_eq!(listed["status"]["received_at"], now + 1_000);
        assert_eq!(listed["status"]["managers"][0]["version"], "1.30");

        // A manager whose scan fails keeps its packages and shows its error,
        // the same as the scan before it.
        let failing = json!([{ "source": "winget", "version": "1.30" }, { "source": "chocolatey", "error": "choco failed" }]);
        assert_eq!(
            store(&app, id, scan_at(3, &failing, vec![firefox()]), now + 2_000).await,
            Stored::Replaced,
            "{name}"
        );
        assert_eq!(
            store(&app, id, scan_at(4, &failing, vec![firefox()]), now + 3_000).await,
            Stored::Unchanged,
            "{name}"
        );
        let listed = packages(&mut admin, id).await;
        assert_eq!(names(&listed), ["Mozilla Firefox", "Marked"], "{name}");
        assert_eq!(listed["status"]["managers"][1]["error"], "choco failed");
        assert_eq!(listed["status"]["scanned_at"], 4);

        // Different packages replace them.
        let updated = vec![
            package(
                "winget",
                "Mozilla.Firefox",
                "Mozilla Firefox",
                "131.0",
                None,
            ),
            package("chocolatey", "7zip", "7-Zip", "24.08", None),
        ];
        assert_eq!(
            store(&app, id, scan_at(5, &newer, updated.clone()), now + 4_000).await,
            Stored::Replaced,
            "{name}"
        );
        let listed = packages(&mut admin, id).await;
        assert_eq!(names(&listed), ["7-Zip", "Mozilla Firefox"], "{name}");
        assert_eq!(listed["packages"][1]["outdated_since"], Value::Null);

        // Once the device's packages are removed, the same scan stores them
        // again.
        applications::store::forget(&mut app.db(), id)
            .await
            .unwrap();
        assert_eq!(
            store(&app, id, scan_at(6, &newer, updated), now + 5_000).await,
            Stored::Replaced,
            "{name}"
        );
        let listed = packages(&mut admin, id).await;
        assert_eq!(names(&listed), ["7-Zip", "Mozilla Firefox"], "{name}");
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
        let job_id = job.body["id"].as_str().unwrap().to_owned();
        let created = common::job_with_devices(&mut admin, &job_id).await;
        let skipped = outcome(&created, "Mac");
        assert_eq!(skipped["outcome"], "skipped", "{name}");
        assert_eq!(skipped["detail"], "None of the applications is for macOS");
        automation::pass(&app.state, now_ms()).await.unwrap();
        let command = command_of(&mut control, "run_applications").await;
        let run_id = command["run"]["run_id"].as_str().unwrap().to_owned();
        let job = common::job_with_devices(&mut admin, &job_id).await;
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
        let job = common::job_with_devices(&mut admin, &job_id).await;
        assert_eq!(outcome(&job, "Desk")["outcome"], "failed", "{name}");
        assert_eq!(outcome(&job, "Desk")["detail"], "1 application failed");
        assert_eq!(
            outcome(&job, "Desk")["applications"],
            json!([{ "name": "Firefox", "outcome": "failed", "error": "winget failed" }])
        );

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

/// Makes the device's outdated packages look `days` old, and tells the alert
/// engine, which reads them again only for devices that reported.
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
    app.state.alerts.device_reported(device_id);
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
        evaluate_until_clear(&app, &mut admin).await;

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
async fn package_ids_match_as_their_manager_reads_them() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        // winget ignores case; Flatpak doesn't.
        let firefox_id = add_application(
            &mut admin,
            json!({
                "name": "Firefox",
                "pinned_version": "130.0",
                "sources": [
                    { "source": "winget", "package_id": "mozilla.firefox" },
                    { "source": "flatpak", "package_id": "Org.Mozilla.Firefox" },
                ],
            }),
        )
        .await;
        let created = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Stale apps", "severity": "warning", "condition": "app_updates_pending", "days": 7 }),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        for (device, id) in [(&desk, "Mozilla.Firefox"), (&laptop, "MOZILLA.FIREFOX")] {
            let mut control = device.connect(&server).await.unwrap();
            report_scan(
                &mut control,
                &[],
                scan(
                    json!([{ "source": "winget" }, { "source": "flatpak" }]),
                    vec![
                        package("winget", id, "Mozilla Firefox", "130.0", Some("131.0")),
                        package(
                            "flatpak",
                            "org.mozilla.firefox",
                            "Firefox",
                            "130.0",
                            Some("131.0"),
                        ),
                    ],
                ),
            )
            .await;
            let packages = packages_where(&mut admin, &device.device_id, |body| {
                !body["status"].is_null()
            })
            .await;
            let of = |source: &str| {
                packages["packages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|package| package["source"] == source)
                    .unwrap()
                    .clone()
            };
            assert_eq!(of("winget")["application_id"], firefox_id, "{name}: {id}");
            assert_eq!(of("winget")["held"], true, "{name}: {id}");
            assert!(of("flatpak")["application_id"].is_null(), "{name}");
            assert_eq!(of("flatpak")["held"], false, "{name}");
        }

        // One outdated package on two devices, held back by the pin.
        let outdated = admin.get("/v1/packages/outdated").await.body;
        let winget = outdated["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|package| package["source"] == "winget")
            .collect::<Vec<_>>();
        assert_eq!(winget.len(), 1, "{name}: {outdated}");
        assert_eq!(winget[0]["devices"].as_array().unwrap().len(), 2);
        assert_eq!(winget[0]["held"], true);
        let read = admin.get(&format!("/v1/applications/{firefox_id}")).await;
        assert_eq!(read.body["installed"], 2, "{name}: {:?}", read.body);
        assert_eq!(read.body["outdated"], 2);

        // The pinned package raises no alert; the unmatched Flatpak does.
        for device in [&desk, &laptop] {
            age(&app, &device.device_id, 8).await;
        }
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        let open = alerts_of(&mut admin).await;
        assert_eq!(open.len(), 2, "{name}: {open:?}");
        for alert in &open {
            assert!(
                alert["detail"].as_str().unwrap().starts_with(
                    "1 application update pending for more than 7 days, the oldest (Firefox)"
                ),
                "{name}: {}",
                alert["detail"]
            );
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn two_requests_at_once_start_one_run() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let _control = desk.connect(&server).await.unwrap();

        let path = format!("/v1/agents/{}/application-runs", desk.device_id);
        let body = json!({ "action": "install", "application_ids": [firefox_id] });
        let (mut first, mut second) = (admin.clone(), admin.clone());
        let (a, b) = tokio::join!(first.post(&path, body.clone()), second.post(&path, body));
        let mut statuses = [a.status, b.status];
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::ACCEPTED, StatusCode::CONFLICT],
            "{name}: {:?} {:?}",
            a.body,
            b.body
        );
        let refused = if a.status == StatusCode::CONFLICT {
            &a
        } else {
            &b
        };
        assert_eq!(refused.code(), "application_run_running", "{name}");

        // Scans: both get the one scan.
        let path = format!("/v1/agents/{}/packages/scan", desk.device_id);
        let (a, b) = tokio::join!(first.post(&path, json!({})), second.post(&path, json!({})));
        assert_eq!(a.status, StatusCode::ACCEPTED, "{name}: {:?}", a.body);
        assert_eq!(b.status, StatusCode::ACCEPTED, "{name}: {:?}", b.body);
        assert_eq!(a.body["id"], b.body["id"], "{name}");

        // The database holds the line by itself.
        let again = applications::runs::insert(
            &mut app.db(),
            &applications::runs::NewRun {
                id: &meshrmm_server::users::new_id(),
                device_id: &desk.device_id,
                summary: "Scan for applications",
                operation: None,
                requested_by_user_id: "nobody",
                policy_name: None,
                job_id: None,
                created_at: now_ms(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            applications::runs::is_already_pending(&again),
            "{name}: {again}"
        );
        app.finish().await;
    }
}

/// Makes the device's runs a minute older.
async fn backdate_runs(app: &App, device_id: &str) {
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new("application_runs"))
                .value(
                    sea_query::Alias::new("created_at"),
                    sea_query::Expr::col(sea_query::Alias::new("created_at")).sub(60_000),
                )
                .and_where(sea_query::Expr::col(sea_query::Alias::new("device_id")).eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn runs_a_restarted_agent_no_longer_has_are_lost_at_once() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let first = desk.connect(&server).await.unwrap();
        let scan = admin
            .post(
                &format!("/v1/agents/{}/packages/scan", desk.device_id),
                json!({}),
            )
            .await;
        let scan_id = scan.body["id"].as_str().unwrap().to_owned();
        let install = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "install", "application_ids": [firefox_id] }),
            )
            .await;
        assert_eq!(
            install.status,
            StatusCode::ACCEPTED,
            "{name}: {:?}",
            install.body
        );
        let install_id = install.body["id"].as_str().unwrap().to_owned();
        drop(first);
        // Runs asked for just before a connection may be handed to it, so
        // only older ones are settled.
        let mut control = desk.connect(&server).await.unwrap();
        send_json(&mut control, json!({ "type": "application_activity" })).await;
        // Reports are handled in order: once this shows, so has the above.
        send_json(
            &mut control,
            json!({ "type": "application_progress", "run_id": install_id, "message": "Installing 1 of 1: Firefox" }),
        )
        .await;
        run_where(&mut admin, &install_id, |run| {
            run["progress"] == "Installing 1 of 1: Firefox"
        })
        .await;
        assert_eq!(
            run(&mut admin, &scan_id).await["status"],
            "pending",
            "{name}: too recent"
        );
        drop(control);
        backdate_runs(&app, &desk.device_id).await;

        // The Agent reconnects still installing, but without the scan.
        let mut control = desk.connect(&server).await.unwrap();
        send_json(
            &mut control,
            json!({ "type": "application_activity", "run_ids": [install_id] }),
        )
        .await;
        let lost = run_where(&mut admin, &scan_id, |run| run["status"] != "pending").await;
        assert_eq!(lost["status"], "lost", "{name}");
        assert_eq!(lost["error"], applications::runs::INTERRUPTED);
        assert_eq!(
            run(&mut admin, &install_id).await["status"],
            "pending",
            "{name}"
        );
        // So Scan now asks for a new scan.
        let again = admin
            .post(
                &format!("/v1/agents/{}/packages/scan", desk.device_id),
                json!({}),
            )
            .await;
        assert_ne!(again.body["id"], scan_id.as_str(), "{name}");
        drop(control);
        backdate_runs(&app, &desk.device_id).await;

        // It restarted: nothing is going on, so the device is free at once,
        // and the failed install raises its alert.
        let created = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "App failed", "severity": "critical", "condition": "app_install_failed" }),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let mut control = desk.connect(&server).await.unwrap();
        send_json(&mut control, json!({ "type": "application_activity" })).await;
        let lost = run_where(&mut admin, &install_id, |run| run["status"] != "pending").await;
        assert_eq!(lost["status"], "lost", "{name}");
        let packages = packages(&mut admin, &desk.device_id).await;
        assert!(packages["running"].is_null(), "{name}: {packages}");
        assert!(packages["scanning"].is_null(), "{name}: {packages}");
        let mut raised = Vec::new();
        for _ in 0..100 {
            raised = alerts_of(&mut admin).await;
            if !raised.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(raised.len(), 1, "{name}: {raised:?}");
        let started = admin
            .post(
                &format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "install", "application_ids": [firefox_id] }),
            )
            .await;
        assert_eq!(
            started.status,
            StatusCode::ACCEPTED,
            "{name}: {:?}",
            started.body
        );
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

#[tokio::test]
async fn what_a_device_cant_use_is_skipped_without_alerts() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let hello_id = add_application(
            &mut admin,
            json!({
                "name": "GNU Hello",
                "pinned_version": "2.10",
                "sources": [{ "source": "winget", "package_id": "GNU.Hello" }],
            }),
        )
        .await;
        let created = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "App failed", "severity": "critical", "condition": "app_install_failed" }),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let outcome = |job: &Value, device: &str| {
            job["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["device_name"] == device)
                .unwrap()
                .clone()
        };

        // A Linux device without Flatpak can't install Firefox from the
        // library: its job is skipped, and the API says why.
        let tux = app.enroll_on(&mut admin, "Tux", "linux").await;
        let mut tux_control = tux.connect(&server).await.unwrap();
        report_scan(
            &mut tux_control,
            &[],
            scan(json!([{ "source": "snap", "version": "2.66" }]), Vec::new()),
        )
        .await;
        packages_where(&mut admin, &tux.device_id, |body| !body["status"].is_null()).await;
        let refused = admin
            .post(
                &format!("/v1/agents/{}/application-runs", tux.device_id),
                json!({ "action": "install", "application_ids": [firefox_id] }),
            )
            .await;
        assert_eq!(
            refused.code(),
            "not_applicable",
            "{name}: {:?}",
            refused.body
        );
        let job = admin
            .post(
                "/v1/jobs",
                json!({
                    "applications": { "action": "install", "application_ids": [firefox_id] },
                    "target": "devices", "device_ids": [tux.device_id], "offline": "skip",
                }),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{:?}", job.body);
        let job_id = job.body["id"].as_str().unwrap().to_owned();
        automation::pass(&app.state, now_ms()).await.unwrap();
        let job = common::job_with_devices(&mut admin, &job_id).await;
        let tux_outcome = outcome(&job, "Tux");
        assert_eq!(tux_outcome["outcome"], "skipped", "{name}: {tux_outcome:?}");
        assert_eq!(
            tux_outcome["detail"],
            "The device has no Flatpak, which the application installs with"
        );

        // An Agent that finds nothing it can use skips every item: the run
        // and the job's device are skipped, with its reasons, and no alert.
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut control = desk.connect(&server).await.unwrap();
        let job = admin
            .post(
                "/v1/jobs",
                json!({
                    "applications": { "action": "install", "application_ids": [firefox_id] },
                    "target": "devices", "device_ids": [desk.device_id], "offline": "skip",
                }),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{:?}", job.body);
        let job_id = job.body["id"].as_str().unwrap().to_owned();
        automation::pass(&app.state, now_ms()).await.unwrap();
        let command = command_of(&mut control, "run_applications").await;
        let run_id = command["run"]["run_id"].as_str().unwrap().to_owned();
        let reason = "winget and Chocolatey aren't installed";
        send_json(
            &mut control,
            json!({ "type": "application_result", "run_id": run_id, "result": {
                "status": "skipped",
                "error": reason,
                "items": [{ "index": 0, "name": "Firefox", "outcome": "skipped", "error": reason, "output": "" }],
            } }),
        )
        .await;
        let finished = run_where(&mut admin, &run_id, |run| run["status"] == "skipped").await;
        assert_eq!(finished["skipped"], 1, "{name}: {finished:?}");
        assert_eq!(finished["failed"], 0);
        assert_eq!(finished["items"][0]["outcome"], "skipped");
        let job = common::job_with_devices(&mut admin, &job_id).await;
        let desk_outcome = outcome(&job, "Desk");
        assert_eq!(
            desk_outcome["outcome"], "skipped",
            "{name}: {desk_outcome:?}"
        );
        assert_eq!(desk_outcome["detail"], reason);
        assert_eq!(
            desk_outcome["applications"],
            json!([{ "name": "Firefox", "outcome": "skipped", "error": reason }])
        );
        alerts::evaluate(&app.state, now_ms()).await.unwrap();
        assert!(alerts_of(&mut admin).await.is_empty(), "{name}");

        // Updating only pinned applications has nothing to run.
        for (path, body) in [
            (
                format!("/v1/agents/{}/application-runs", desk.device_id),
                json!({ "action": "update", "mode": "only", "application_ids": [hello_id] }),
            ),
            (
                "/v1/jobs".to_owned(),
                json!({
                    "applications": { "action": "update", "mode": "only", "application_ids": [hello_id] },
                    "target": "devices", "device_ids": [desk.device_id], "offline": "skip",
                }),
            ),
        ] {
            let pinned = admin.post(&path, body).await;
            assert_eq!(
                pinned.status,
                StatusCode::CONFLICT,
                "{name}: {:?}",
                pinned.body
            );
            assert_eq!(pinned.code(), "application_pinned");
            assert_eq!(
                pinned.body["error"],
                "GNU Hello is pinned to 2.10 in the library; unpin it to update it"
            );
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn a_scan_for_a_device_too_far_behind_is_busy_and_records_nothing() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let scan_path = format!("/v1/agents/{}/packages/scan", desk.device_id);
        let mut connection = app.state.agents.connect(&desk.device_id);
        let queued = common::fill_backlog(&app, &desk.device_id);
        common::assert_device_busy(&admin.post(&scan_path, json!({})).await, name);
        assert_eq!(
            admin.get("/v1/application-runs").await.body["runs"],
            json!([]),
            "{name}"
        );
        assert!(
            audit_events(&app, "application.scan").await.is_empty(),
            "{name}"
        );

        common::drain_backlog(&mut connection, queued).await;
        let started = admin.post(&scan_path, json!({})).await;
        assert_eq!(started.status, StatusCode::ACCEPTED, "{name}");
        assert_eq!(
            common::next_command(&mut connection).await,
            Some(meshrmm_protocol_types::AgentCommand::ScanApplications {
                run_id: Some(started.body["id"].as_str().unwrap().to_owned())
            })
        );
        drop(connection);
        app.finish().await;
    }
}
