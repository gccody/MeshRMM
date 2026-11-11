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
        assert_eq!(fleet["packages"][0]["devices"], 1, "{name}");
        let on = admin
            .get("/v1/packages/outdated/devices?package=winget:Mozilla.Firefox")
            .await
            .body;
        assert_eq!(on["devices"][0]["device_name"], "Desk", "{name}: {on}");

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
        desk
            .report(&format!("application-runs/{run_id}/result"), json!({
                "status": "partial",
                "items": [
                    { "index": 0, "name": "Firefox", "source": "winget", "package_id": "Mozilla.Firefox",
                      "outcome": "unchanged", "version": "130.0", "output": "" },
                    { "index": 1, "name": "Tool", "source": "custom", "outcome": "failed",
                      "exit_code": 1603, "error": "The installer exited with 1603", "output": "Fatal error during installation." },
                ],
            }))
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
        desk.report(
            &format!("application-runs/{update_id}/result"),
            json!({ "status": "succeeded", "items": [] }),
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
        desk.report(
            &format!("application-runs/{uninstall_id}/result"),
            json!({ "status": "succeeded", "items": [] }),
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
        applications::store::forget(app.db(), id).await.unwrap();
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
        desk
            .report(&format!("application-runs/{run_id}/result"), json!({
                "status": "failed",
                "items": [{ "index": 0, "name": "Firefox", "outcome": "failed", "error": "winget failed", "output": "" }],
            }))
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
        desk
            .report(&format!("application-runs/{run_id}/result"), json!({
                "status": "failed",
                "items": [{ "index": 0, "name": "Firefox", "outcome": "failed", "exit_code": 2316632080_i64,
                            "error": "No applicable installer", "output": "" }],
            }))
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
        assert_eq!(winget[0]["devices"], 2);
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
async fn results_an_agent_still_holds_are_taken_once_even_after_an_interruption() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let result = json!({
            "status": "succeeded",
            "items": [{ "index": 0, "name": "Firefox", "outcome": "succeeded", "output": "" }],
        });

        let control = desk.connect(&server).await.unwrap();
        let held = start_install(&mut admin, &desk.device_id, &firefox_id).await;
        drop(control);
        backdate_runs(&app, &desk.device_id).await;
        // The Agent restarted with the run's result in its outbox, and says
        // it still has the run, so it isn't settled as interrupted.
        let mut control = desk.connect(&server).await.unwrap();
        send_json(
            &mut control,
            json!({ "type": "application_activity", "run_ids": [held] }),
        )
        .await;
        // Reports are handled in order: once this shows, so has the above.
        send_json(
            &mut control,
            json!({ "type": "application_progress", "run_id": held, "message": "Done" }),
        )
        .await;
        run_where(&mut admin, &held, |run| run["progress"] == "Done").await;
        assert_eq!(run(&mut admin, &held).await["status"], "pending", "{name}");
        let reported = desk
            .report(&format!("application-runs/{held}/result"), result.clone())
            .await;
        assert_eq!(
            reported.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            reported.body
        );
        let done = run(&mut admin, &held).await;
        assert_eq!(done["status"], "succeeded", "{name}");
        // Sent again, as when the answer never reached the Agent: taken, and
        // changes nothing.
        let again = desk
            .report(
                &format!("application-runs/{held}/result"),
                json!({ "status": "failed", "error": "a different story", "items": [] }),
            )
            .await;
        assert_eq!(again.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(run(&mut admin, &held).await, done, "{name}");
        // Another device has no such run, so its Agent drops the result.
        let other = app.enroll(&mut admin, "Other").await;
        assert_eq!(
            other
                .report(&format!("application-runs/{held}/result"), result.clone())
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}"
        );

        // A run settled as interrupted whose result arrives after all: the
        // run did happen, so its result replaces that.
        let interrupted = start_install(&mut admin, &desk.device_id, &firefox_id).await;
        drop(control);
        backdate_runs(&app, &desk.device_id).await;
        let mut control = desk.connect(&server).await.unwrap();
        send_json(&mut control, json!({ "type": "application_activity" })).await;
        let lost = run_where(&mut admin, &interrupted, |run| run["status"] != "pending").await;
        assert_eq!(lost["error"], applications::runs::INTERRUPTED, "{name}");
        let late = desk
            .report(&format!("application-runs/{interrupted}/result"), result)
            .await;
        assert_eq!(
            late.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            late.body
        );
        let found = run(&mut admin, &interrupted).await;
        assert_eq!(found["status"], "succeeded", "{name}");
        assert_eq!(found["error"], Value::Null);
        assert_eq!(found["items"].as_array().unwrap().len(), 1);
        drop(control);
        app.finish().await;
    }
}

/// Asks `device_id` to install `application_id`, returning the run's ID.
async fn start_install(admin: &mut Browser, device_id: &str, application_id: &str) -> String {
    let started = admin
        .post(
            &format!("/v1/agents/{device_id}/application-runs"),
            json!({ "action": "install", "application_ids": [application_id] }),
        )
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
    started.body["id"].as_str().unwrap().to_owned()
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
        desk
            .report(&format!("application-runs/{run_id}/result"), json!({
                "status": "skipped",
                "error": reason,
                "items": [{ "index": 0, "name": "Firefox", "outcome": "skipped", "error": reason, "output": "" }],
            }))
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

// ---- The fleet's outdated packages ----

/// The ID of the `n`th device of a fleet added with [`add_fleet`].
fn fleet_device(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

/// Enrolls devices `first..first + count` behind the API's back, in the
/// `companies` in turn (`None` for no company).
async fn add_fleet(app: &App, first: usize, count: usize, companies: &[Option<String>], now: i64) {
    let numbers = (first..first + count).collect::<Vec<_>>();
    for chunk in numbers.chunks(100) {
        let mut insert = sea_query::Query::insert();
        insert.into_table(sea_query::Alias::new("agents")).columns(
            [
                "id",
                "name",
                "platform",
                "auth_token_hash",
                "company_id",
                "created_at",
                "updated_at",
            ]
            .map(sea_query::Alias::new),
        );
        for &n in chunk {
            insert.values_panic([
                fleet_device(n).into(),
                format!("Device {n}").into(),
                "windows".into(),
                "a".repeat(64).into(),
                companies[n % companies.len()].clone().into(),
                now.into(),
                now.into(),
            ]);
        }
        app.db().execute(&insert).await.unwrap();
    }
}

fn fleet_managers() -> Value {
    json!([{ "source": "winget" }, { "source": "flatpak" }, { "source": "chocolatey" }])
}

/// What the `n`th device of a fleet has outdated: Firefox on even devices
/// (offering 132 on every fourth, 131 on the others), Git on every third
/// (listed as `git.git` on odd ones, which winget reads as the same), GIMP
/// on every fifth, and 7-Zip on device 1 only.
fn fleet_packages(n: usize) -> Vec<Value> {
    let firefox = n.is_multiple_of(2).then_some(if n.is_multiple_of(4) {
        "132.0"
    } else {
        "131.0"
    });
    let mut packages = vec![package(
        "winget",
        "Mozilla.Firefox",
        "Mozilla Firefox",
        "130.0",
        firefox,
    )];
    if n.is_multiple_of(3) {
        let id = if n % 2 == 1 { "git.git" } else { "Git.Git" };
        packages.push(package("winget", id, "Git", "2.44", Some("2.45")));
    }
    if n.is_multiple_of(5) {
        packages.push(package(
            "flatpak",
            "org.gimp.GIMP",
            "GIMP",
            "2.10",
            Some("3.0"),
        ));
    }
    packages.push(package(
        "chocolatey",
        "7zip",
        "7-Zip",
        "24.08",
        (n == 1).then_some("24.09"),
    ));
    packages
}

async fn store_fleet(app: &App, numbers: std::ops::Range<usize>, now: i64) {
    for n in numbers {
        store(
            app,
            &fleet_device(n),
            scan_at(1, &fleet_managers(), fleet_packages(n)),
            now,
        )
        .await;
    }
}

async fn outdated_page(browser: &mut Browser, query: &str) -> Value {
    let response = browser.get(&format!("/v1/packages/outdated{query}")).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{query}: {:?}",
        response.body
    );
    response.body
}

/// `(package_id, devices)` of each package on a page, in order.
fn counted(page: &Value) -> Vec<(String, i64)> {
    page["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            (
                package["package_id"].as_str().unwrap().to_owned(),
                package["devices"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn owned(pairs: &[(&str, i64)]) -> Vec<(String, i64)> {
    pairs
        .iter()
        .map(|(id, devices)| ((*id).to_owned(), *devices))
        .collect()
}

async fn add_company(browser: &mut Browser, name: &str) -> String {
    let created = browser.post("/v1/companies", json!({ "name": name })).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn outdated_packages_are_counted_paged_and_searched() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let acme = add_company(&mut admin, "Acme").await;
        let globex = add_company(&mut admin, "Globex").await;
        let firefox_id = add_application(&mut admin, firefox()).await;
        add_application(
            &mut admin,
            json!({
                "name": "Archiver", "pinned_version": "24.08",
                "sources": [{ "source": "chocolatey", "package_id": "7zip" }],
            }),
        )
        .await;
        let now = now_ms();
        add_fleet(&app, 0, 12, &[Some(acme), Some(globex), None], now).await;
        store_fleet(&app, 0..12, now).await;

        // On the most devices first; Git once however its ID is cased.
        let page = outdated_page(&mut admin, "").await;
        assert_eq!(
            counted(&page),
            owned(&[
                ("Mozilla.Firefox", 6),
                ("Git.Git", 4),
                ("org.gimp.GIMP", 3),
                ("7zip", 1)
            ]),
            "{name}: {page}"
        );
        assert_eq!(page["matching"], 4, "{name}");
        // The pinned 7-Zip is held back, so it is no update pending.
        assert_eq!(
            page["totals"],
            json!({ "packages": 3, "updates": 13 }),
            "{name}"
        );
        let firefox = &page["packages"][0];
        assert_eq!(firefox["name"], "Mozilla Firefox", "{name}");
        assert_eq!(firefox["application_id"], firefox_id, "{name}");
        assert_eq!(firefox["available_version"], "132.0", "{name}: the newest");
        assert_eq!(firefox["held"], false, "{name}");
        assert_eq!(page["packages"][3]["held"], true, "{name}");
        assert_eq!(page["packages"][3]["application_name"], "Archiver");

        // A page at a time, each package once.
        let mut seen = Vec::new();
        for offset in 0..5 {
            let page = outdated_page(&mut admin, &format!("?limit=1&offset={offset}")).await;
            assert_eq!(page["matching"], 4, "{name}");
            seen.extend(counted(&page));
        }
        assert_eq!(
            seen,
            counted(&outdated_page(&mut admin, "").await),
            "{name}"
        );
        let counts_only = outdated_page(&mut admin, "?limit=0").await;
        assert_eq!(counts_only["packages"], json!([]), "{name}");
        assert_eq!(counts_only["matching"], 4, "{name}");

        // By name, ID or library application, in any case; the totals stay.
        for (query, found) in [
            ("?query=%20fire%20", "Mozilla.Firefox"),
            ("?query=GIT.git", "Git.Git"),
            ("?query=archiver", "7zip"),
        ] {
            let page = outdated_page(&mut admin, query).await;
            assert_eq!(page["matching"], 1, "{name}: {query}");
            assert_eq!(page["packages"][0]["package_id"], found, "{name}: {query}");
            assert_eq!(page["totals"]["packages"], 3, "{name}: {query}");
        }
        assert_eq!(
            outdated_page(&mut admin, "?query=50%25").await["matching"],
            0,
            "{name}: wildcards are text"
        );

        // Whoever may see applications sees every device's: MeshRMM has no
        // per-company access to devices. Others see none.
        let (mut viewer, _) = user_with(
            &app,
            &mut admin,
            "viewer@example.com",
            &["applications.view"],
        )
        .await;
        assert_eq!(
            counted(&outdated_page(&mut viewer, "").await),
            counted(&page),
            "{name}"
        );
        let (mut other, _) =
            user_with(&app, &mut admin, "other@example.com", &["devices.view"]).await;
        assert_eq!(
            other.get("/v1/packages/outdated").await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );

        // A deleted device's packages leave the counts; so does one a scan
        // finds up to date, and a newer version offered shows.
        let deleted = admin
            .delete(&format!("/v1/agents/{}", fleet_device(0)))
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        let mut packages = fleet_packages(2);
        packages[0] = package(
            "winget",
            "Mozilla.Firefox",
            "Mozilla Firefox",
            "132.0",
            None,
        );
        store(
            &app,
            &fleet_device(2),
            scan_at(2, &fleet_managers(), packages),
            now,
        )
        .await;
        let mut packages = fleet_packages(4);
        packages[0] = package(
            "winget",
            "Mozilla.Firefox",
            "Mozilla Firefox",
            "130.0",
            Some("133.0"),
        );
        store(
            &app,
            &fleet_device(4),
            scan_at(2, &fleet_managers(), packages),
            now,
        )
        .await;
        let page = outdated_page(&mut admin, "").await;
        assert_eq!(
            counted(&page),
            owned(&[
                ("Mozilla.Firefox", 4),
                ("Git.Git", 3),
                ("org.gimp.GIMP", 2),
                ("7zip", 1)
            ]),
            "{name}: {page}"
        );
        assert_eq!(page["packages"][0]["available_version"], "133.0", "{name}");
        // Device 1's 7-Zip updated: nothing has it outdated, so it goes.
        let mut packages = fleet_packages(1);
        let last = packages.len() - 1;
        packages[last] = package("chocolatey", "7zip", "7-Zip", "24.09", None);
        store(
            &app,
            &fleet_device(1),
            scan_at(2, &fleet_managers(), packages),
            now,
        )
        .await;
        let page = outdated_page(&mut admin, "").await;
        assert_eq!(page["matching"], 3, "{name}: {page}");
        assert_eq!(
            page["totals"],
            json!({ "packages": 3, "updates": 9 }),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_packages_devices_are_paged_without_missing_or_repeating_one() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let now = now_ms();
        add_fleet(&app, 0, 30, &[None], now).await;
        store_fleet(&app, 0..30, now).await;
        // Device 0 is deleted; device 2 is up to date again.
        assert_eq!(
            admin
                .delete(&format!("/v1/agents/{}", fleet_device(0)))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let mut packages = fleet_packages(2);
        packages[0] = package(
            "winget",
            "Mozilla.Firefox",
            "Mozilla Firefox",
            "131.0",
            None,
        );
        store(
            &app,
            &fleet_device(2),
            scan_at(2, &fleet_managers(), packages),
            now,
        )
        .await;

        // Asked for in another case, as winget reads IDs.
        let mut seen: Vec<Value> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut path =
                "/v1/packages/outdated/devices?package=winget:mozilla.FIREFOX&limit=4".to_owned();
            if let Some(after) = &after {
                path.push_str(&format!("&after={after}"));
            }
            let page = admin.get(&path).await;
            assert_eq!(page.status, StatusCode::OK, "{name}: {:?}", page.body);
            let devices = page.body["devices"].as_array().unwrap().clone();
            assert!(devices.len() <= 4, "{name}");
            seen.extend(devices);
            match page.body["next"].as_str() {
                Some(next) => after = Some(next.to_owned()),
                None => break,
            }
        }
        let ids = seen
            .iter()
            .map(|device| device["device_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let expected = (4..30).step_by(2).map(fleet_device).collect::<Vec<_>>();
        assert_eq!(ids, expected, "{name}: each device once, in ID order");
        assert_eq!(seen[0]["device_name"], "Device 4", "{name}");
        assert_eq!(seen[0]["version"], "130.0", "{name}");
        assert_eq!(seen[0]["available_version"], "132.0", "{name}");
        assert!(seen[0]["outdated_since"].as_i64().is_some(), "{name}");

        // A whole package's devices on one page has no next one.
        let gimp = admin
            .get("/v1/packages/outdated/devices?package=flatpak:org.gimp.GIMP")
            .await
            .body;
        assert_eq!(
            gimp["devices"].as_array().unwrap().len(),
            5,
            "{name}: {gimp}"
        );
        assert_eq!(gimp["next"], Value::Null, "{name}");
        // Flatpak keeps case: another case is another package.
        let other = admin
            .get("/v1/packages/outdated/devices?package=flatpak:org.gimp.gimp")
            .await
            .body;
        assert_eq!(other["devices"], json!([]), "{name}");
        for bad in [
            "package=Mozilla.Firefox",
            "package=winget:Mozilla.Firefox&after=device",
            "package=winget:Mozilla.Firefox&cursor=1",
        ] {
            let refused = admin
                .get(&format!("/v1/packages/outdated/devices?{bad}"))
                .await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}: {bad}");
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn outdated_lists_cost_the_same_for_any_fleet() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let now = now_ms();
        // Authenticating the browser's first request is its own work.
        outdated_page(&mut admin, "?limit=2").await;
        let mut costs = Vec::new();
        for (first, count) in [(0, 3), (3, 500)] {
            add_fleet(&app, first, count, &[None], now).await;
            store_fleet(&app, first..first + count, now).await;
            // The cheapest of three reads: the app's background work counts
            // too, and only adds to what a read costs.
            let mut cheapest = None;
            let (mut page, mut devices) = (Value::Null, Value::Null);
            for _ in 0..3 {
                let before = app.db().counts();
                page = outdated_page(&mut admin, "?limit=2").await;
                devices = admin
                    .get("/v1/packages/outdated/devices?package=winget:Mozilla.Firefox&limit=2")
                    .await
                    .body;
                let cost = app.db().counts() - before;
                if cheapest.is_none_or(|cheapest: meshrmm_server::db::Counts| {
                    cost.statements < cheapest.statements
                }) {
                    cheapest = Some(cost);
                }
            }
            costs.push(cheapest.unwrap());
            assert_eq!(page["packages"].as_array().unwrap().len(), 2, "{name}");
            assert_eq!(
                page["packages"][0]["devices"],
                (first + count).div_ceil(2),
                "{name}"
            );
            assert_eq!(devices["devices"].as_array().unwrap().len(), 2, "{name}");
        }
        assert_eq!(costs[0], costs[1], "{name}");
        assert_eq!(costs[0].writes, 0, "{name}: {costs:?}");
        app.finish().await;
    }
}

#[tokio::test]
async fn a_job_on_outdated_packages_runs_on_the_devices_outdated_as_it_starts() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        add_application(
            &mut admin,
            json!({
                "name": "GIMP", "pinned_version": "2.10",
                "sources": [{ "source": "flatpak", "package_id": "org.gimp.GIMP" }],
            }),
        )
        .await;
        let now = now_ms();
        // 0: Firefox, Git and GIMP outdated (deleted below). 1: 7-Zip.
        // 2: Firefox (updated before the job). 3: Git. 4: Firefox. 5: GIMP
        // only, which the library pins. 6: Firefox and Git. 7: nothing.
        add_fleet(&app, 0, 8, &[None], now).await;
        store_fleet(&app, 0..8, now).await;
        assert_eq!(
            admin
                .delete(&format!("/v1/agents/{}", fleet_device(0)))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let mut packages = fleet_packages(2);
        packages[0] = package(
            "winget",
            "Mozilla.Firefox",
            "Mozilla Firefox",
            "131.0",
            None,
        );
        store(
            &app,
            &fleet_device(2),
            scan_at(2, &fleet_managers(), packages),
            now,
        )
        .await;

        // Applications deploy needs no jobs.run_wide for this target.
        let (mut technician, _) = user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["applications.view", "applications.deploy"],
        )
        .await;
        let start = |packages: Value, applications: Value| {
            json!({
                "applications": applications,
                "target": "outdated", "packages": packages,
                "offline": "skip",
            })
        };
        let firefox_update =
            json!({ "action": "update", "mode": "only", "packages": ["winget:Mozilla.Firefox"] });
        let job = technician
            .post(
                "/v1/jobs",
                start(json!(["winget:mozilla.firefox"]), firefox_update.clone()),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{name}: {:?}", job.body);
        assert_eq!(job.body["target"], "outdated", "{name}");
        assert_eq!(
            job.body["target_label"], "Where Mozilla Firefox is outdated",
            "{name}"
        );
        let job_ids = |devices: Vec<Value>| {
            let mut ids = devices
                .iter()
                .map(|device| device["device_id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let id = job.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            job_ids(common::job_devices(&mut technician, &id, None).await),
            [fleet_device(4), fleet_device(6)],
            "{name}"
        );
        let started = &audit_events(&app, "job.start").await[0];
        assert_eq!(started.metadata["target"], "outdated", "{name}");
        assert_eq!(started.metadata["devices"], 2, "{name}");

        // Several packages: any of them.
        let job = admin
            .post(
                "/v1/jobs",
                start(
                    json!(["winget:Git.Git", "chocolatey:7zip"]),
                    json!({ "action": "update", "mode": "only", "packages": ["winget:Git.Git", "chocolatey:7zip"] }),
                ),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{name}: {:?}", job.body);
        assert_eq!(
            job.body["target_label"], "Where any of Git and 7-Zip is outdated",
            "{name}"
        );
        let id = job.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            job_ids(common::job_devices(&mut admin, &id, None).await),
            [fleet_device(1), fleet_device(3), fleet_device(6)],
            "{name}"
        );

        // None chosen: any package outdated that the library doesn't pin, so
        // device 5, with only GIMP outdated, is left out.
        let job = admin
            .post(
                "/v1/jobs",
                start(json!([]), json!({ "action": "update", "mode": "all" })),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{name}: {:?}", job.body);
        assert_eq!(
            job.body["target_label"], "Where any application is outdated",
            "{name}"
        );
        let id = job.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            job_ids(common::job_devices(&mut admin, &id, None).await),
            [1, 3, 4, 6].map(fleet_device),
            "{name}"
        );

        // Nothing outdated anywhere: no devices.
        let none = admin
            .post(
                "/v1/jobs",
                start(
                    json!(["snap:hello"]),
                    json!({ "action": "update", "mode": "only", "packages": ["snap:hello"] }),
                ),
            )
            .await;
        assert_eq!(none.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(none.body["code"], "no_devices", "{name}");

        // Only an update of applications; never a policy or alert rule.
        let script = admin
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Hi", "language": "powershell", "body": "echo hi", "timeout_seconds": 60, "shared": true }),
            )
            .await;
        assert_eq!(script.status, StatusCode::CREATED, "{:?}", script.body);
        for refused in [
            json!({
                "script_id": script.body["id"], "run_as": "system",
                "target": "outdated", "packages": [], "offline": "skip",
            }),
            start(
                json!([]),
                json!({ "action": "uninstall", "packages": ["winget:Mozilla.Firefox"] }),
            ),
            start(json!(["Mozilla.Firefox"]), firefox_update.clone()),
        ] {
            let response = admin.post("/v1/jobs", refused.clone()).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{name}: {refused}"
            );
        }
        let policy = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Patch", "action": "app_update",
                    "applications": { "mode": "all" },
                    "target": "outdated", "trigger": "daily", "time": "03:00", "time_zone": "UTC",
                    "offline": "wait",
                }),
            )
            .await;
        assert_eq!(
            policy.status,
            StatusCode::BAD_REQUEST,
            "{name}: {:?}",
            policy.body
        );
        let rule = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Stale", "severity": "warning", "condition": "app_updates_pending", "days": 7, "target": "outdated" }),
            )
            .await;
        assert_eq!(
            rule.status,
            StatusCode::BAD_REQUEST,
            "{name}: {:?}",
            rule.body
        );
        app.finish().await;
    }
}
