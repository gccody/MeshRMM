//! The toolbox: scripts and library files, who may see and change them, and
//! running and delivering them on devices.
mod common;

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use common::{App, Browser, audit_events};
use meshrmm_protocol_types::{AgentCommand, FileDeliveryDestination, RunAs, ScriptLanguage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RUNNER: &[&str] = &["devices.view", "scripts.run", "files.deliver"];
const MANAGER: &[&str] = &[
    "devices.view",
    "scripts.run",
    "scripts.manage_shared",
    "files.deliver",
    "files.manage_shared",
];

fn script(name: &str, shared: bool) -> Value {
    json!({
        "name": name,
        "folder": " Disk / Cleanup ",
        "description": " Frees space \n",
        "language": "powershell",
        "body": "Get-Date",
        "shared": shared,
    })
}

fn sha256(bytes: &[u8]) -> String {
    meshrmm_server::secrets::hex(&Sha256::digest(bytes))
}

async fn upload(
    browser: &Browser,
    name: &str,
    content: &[u8],
    shared: bool,
    digest: &str,
) -> common::Response {
    let request = browser
        .request(
            Method::POST,
            &format!(
                "/v1/toolbox/files?name={name}&folder=Installers&shared={shared}&sha256={digest}"
            ),
        )
        .header("content-type", "application/octet-stream")
        .body(Body::from(content.to_vec()))
        .unwrap();
    browser.clone().raw(request).await
}

fn names(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn scripts_are_private_until_shared_and_shared_ones_need_the_manage_permission() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut ada, _) = common::user_with(&app, &mut admin, "ada@example.com", RUNNER).await;
        let (mut bob, _) = common::user_with(&app, &mut admin, "bob@example.com", RUNNER).await;
        let (mut manager, _) =
            common::user_with(&app, &mut admin, "manager@example.com", MANAGER).await;

        let private = ada
            .post("/v1/toolbox/scripts", script(" Ada private ", false))
            .await;
        assert_eq!(
            private.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            private.body
        );
        assert_eq!(private.body["name"], "Ada private");
        assert_eq!(private.body["folder"], "Disk/Cleanup");
        assert_eq!(private.body["description"], "Frees space");
        assert_eq!(private.body["timeout_seconds"], 300);
        assert_eq!(private.body["body"], "Get-Date");
        assert_eq!(
            [
                &private.body["shared"],
                &private.body["owned"],
                &private.body["can_edit"]
            ],
            [&json!(false), &json!(true), &json!(true)]
        );
        let private_id = private.body["id"].as_str().unwrap().to_owned();
        let refused = ada
            .post("/v1/toolbox/scripts", script("Ada shared", true))
            .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: sharing needs scripts.manage_shared"
        );
        let shared = manager
            .post("/v1/toolbox/scripts", script("Team script", true))
            .await;
        assert_eq!(shared.status, StatusCode::CREATED);
        let shared_id = shared.body["id"].as_str().unwrap().to_owned();
        let mac = ada
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Mac", "language": "shell", "body": "uptime" }),
            )
            .await;
        assert_eq!(mac.status, StatusCode::CREATED);
        assert_eq!(mac.body["language"], "shell");
        assert_eq!(mac.body["folder"], "");
        for invalid in [
            json!({ "name": "", "language": "cmd", "body": "dir" }),
            json!({ "name": "x", "language": "cmd", "body": "  " }),
            json!({ "name": "x", "language": "cmd", "body": "dir", "timeout_seconds": 5 }),
            json!({ "name": "x", "language": "bash", "body": "ls" }),
            json!({ "name": "x", "language": "cmd", "body": "dir", "folder": "a/".repeat(9) }),
            json!({ "name": "x", "language": "cmd", "body": "dir", "owner": "bob" }),
        ] {
            assert_eq!(
                ada.post("/v1/toolbox/scripts", invalid.clone())
                    .await
                    .status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid}"
            );
        }

        let listing = ada.get("/v1/toolbox").await.body;
        assert_eq!(
            names(&listing["scripts"]),
            ["Mac", "Ada private", "Team script"],
            "{name}"
        );
        assert!(
            listing["scripts"][1].get("body").is_none(),
            "lists leave out bodies"
        );
        assert_eq!(listing["max_file_bytes"], 95 * 1024 * 1024);
        let seen_by_bob = bob.get("/v1/toolbox").await.body;
        assert_eq!(names(&seen_by_bob["scripts"]), ["Team script"]);
        assert_eq!(seen_by_bob["scripts"][0]["owned"], false);
        assert_eq!(seen_by_bob["scripts"][0]["can_edit"], false);
        assert_eq!(
            bob.get(&format!("/v1/toolbox/scripts/{private_id}"))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            bob.put(
                &format!("/v1/toolbox/scripts/{shared_id}"),
                script("Bob edit", true)
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
            "{name}: changing a shared script needs scripts.manage_shared"
        );
        assert_eq!(
            bob.delete(&format!("/v1/toolbox/scripts/{shared_id}"))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            manager
                .delete(&format!("/v1/toolbox/scripts/{private_id}"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: private scripts stay private, even from managers"
        );

        let mut edit = script("Edited", true);
        edit["body"] = json!("Get-Volume");
        let edited = manager
            .put(&format!("/v1/toolbox/scripts/{shared_id}"), edit)
            .await;
        assert_eq!(edited.status, StatusCode::OK, "{name}: {:?}", edited.body);
        assert_eq!(edited.body["body"], "Get-Volume");
        // The manager unshares it: it goes back to being its owner's alone.
        let shared_by_ada = {
            let mut admin_script = script("Admin's", true);
            admin_script["folder"] = json!("");
            admin.post("/v1/toolbox/scripts", admin_script).await.body["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let unshared = manager
            .put(
                &format!("/v1/toolbox/scripts/{shared_by_ada}"),
                script("Admin's", false),
            )
            .await;
        assert_eq!(
            unshared.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            unshared.body
        );
        assert_eq!(
            names(&admin.get("/v1/toolbox").await.body["scripts"]),
            ["Admin's", "Edited"]
        );
        assert_eq!(
            ada.put(
                &format!("/v1/toolbox/scripts/{private_id}"),
                script("Renamed", false)
            )
            .await
            .body["name"],
            "Renamed"
        );
        assert_eq!(
            ada.delete(&format!("/v1/toolbox/scripts/{private_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let (mut outsider, _) =
            common::user_with(&app, &mut admin, "outsider@example.com", &["devices.view"]).await;
        assert_eq!(
            outsider.get("/v1/toolbox").await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            audit_events(&app, "toolbox.script_create").await.len(),
            4,
            "{name}"
        );
        assert_eq!(audit_events(&app, "toolbox.script_update").await.len(), 3);
        assert_eq!(audit_events(&app, "toolbox.script_delete").await.len(), 1);
        app.finish().await;
    }
}

#[tokio::test]
async fn library_files_are_checked_on_upload_and_downloaded_whole() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut runner, _) = common::user_with(&app, &mut admin, "ada@example.com", RUNNER).await;
        let (mut manager, _) =
            common::user_with(&app, &mut admin, "manager@example.com", MANAGER).await;
        let content = b"MZ\x90\x00installer bytes".repeat(1000);
        let digest = sha256(&content);

        let corrupted = upload(&manager, "setup.exe", &content, true, &sha256(b"other")).await;
        assert_eq!(corrupted.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(
            upload(&manager, "bad:name.exe", &content, true, &digest)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            upload(&manager, "CON.txt", &content, true, &digest)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            upload(&manager, "setup.exe", &content, true, "not-a-digest")
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            upload(&runner, "setup.exe", &content, true, &digest)
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{name}: sharing needs files.manage_shared"
        );
        let declared_too_large = manager
            .request(
                Method::POST,
                &format!("/v1/toolbox/files?name=big.bin&sha256={digest}"),
            )
            .header("content-length", (95 * 1024 * 1024 + 1).to_string())
            .body(Body::from(content.clone()))
            .unwrap();
        assert_eq!(
            manager.raw(declared_too_large).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let toolbox_dir = app.state.config.data_dir.join("toolbox");
        assert_eq!(
            std::fs::read_dir(&toolbox_dir).unwrap().count(),
            0,
            "{name}: refused uploads store nothing"
        );

        let uploaded = upload(
            &manager,
            "setup.exe",
            &content,
            true,
            &digest.to_uppercase(),
        )
        .await;
        assert_eq!(
            uploaded.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            uploaded.body
        );
        assert_eq!(uploaded.body["name"], "setup.exe");
        assert_eq!(uploaded.body["folder"], "Installers");
        assert_eq!(uploaded.body["size_bytes"], content.len());
        assert_eq!(uploaded.body["sha256"], digest);
        assert_eq!(uploaded.body["shared"], true);
        let file_id = uploaded.body["id"].as_str().unwrap().to_owned();
        let private = upload(&runner, "notes.txt", b"mine", false, &sha256(b"mine")).await;
        assert_eq!(private.status, StatusCode::CREATED);
        let private_id = private.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            names(&runner.get("/v1/toolbox").await.body["files"]),
            ["notes.txt", "setup.exe"]
        );
        assert_eq!(
            names(&manager.get("/v1/toolbox").await.body["files"]),
            ["setup.exe"]
        );

        let download = |browser: &mut Browser, id: &str| {
            let request = browser
                .request(Method::GET, &format!("/v1/toolbox/files/{id}/content"))
                .body(Body::empty())
                .unwrap();
            let mut browser = browser.clone();
            async move { browser.bytes(request).await }
        };
        let downloaded = download(&mut runner, &file_id).await;
        assert_eq!(downloaded.status, StatusCode::OK, "{name}");
        assert_eq!(downloaded.body, content);
        assert_eq!(
            downloaded.header("content-type"),
            "application/octet-stream"
        );
        assert_eq!(downloaded.header("content-disposition"), "attachment");
        assert_eq!(
            downloaded.header("content-length"),
            content.len().to_string()
        );
        assert_eq!(
            download(&mut manager, &private_id).await.status,
            StatusCode::NOT_FOUND
        );

        assert_eq!(
            runner
                .put(
                    &format!("/v1/toolbox/files/{file_id}"),
                    json!({ "name": "x.exe" })
                )
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        let renamed = manager
            .put(
                &format!("/v1/toolbox/files/{file_id}"),
                json!({ "name": "setup-v2.exe", "folder": "Installers/Old", "shared": true }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}: {:?}", renamed.body);
        assert_eq!(renamed.body["folder"], "Installers/Old");
        assert_eq!(renamed.body["sha256"], digest, "the content doesn't change");
        assert_eq!(
            manager
                .delete(&format!("/v1/toolbox/files/{file_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            download(&mut runner, &file_id).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            std::fs::read_dir(&toolbox_dir).unwrap().count(),
            1,
            "{name}: deleting a file removes its content"
        );
        assert_eq!(audit_events(&app, "toolbox.file_upload").await.len(), 2);
        assert_eq!(audit_events(&app, "toolbox.file_delete").await.len(), 1);
        app.finish().await;
    }
}

async fn create_script(browser: &mut Browser, name: &str) -> String {
    let created = browser
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": name, "language": "cmd", "body": "whoami", "timeout_seconds": 60 }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn script_runs_reach_the_agent_and_record_its_report() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let (mut ada, ada_id) =
            common::user_with(&app, &mut admin, "ada@example.com", RUNNER).await;
        let script_id = create_script(&mut ada, "Who am I").await;
        let start = json!({ "script_id": script_id, "run_as": "user" });
        let runs_path = format!("/v1/agents/{}/script-runs", agent.device_id);

        let offline = ada.post(&runs_path, start.clone()).await;
        assert_eq!(offline.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(offline.code(), "device_offline");
        let listed = ada.get("/v1/script-runs").await.body;
        assert_eq!(listed["runs"][0]["status"], "failed", "{name}");
        assert!(
            listed["runs"][0]["error"]
                .as_str()
                .unwrap()
                .contains("offline")
        );

        let mut connection = app.state.agents.connect(&agent.device_id);
        let started = ada.post(&runs_path, start.clone()).await;
        assert_eq!(
            started.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            started.body
        );
        assert_eq!(started.body["status"], "pending");
        assert_eq!(started.body["source"], "dashboard");
        assert_eq!(started.body["requested_by_you"], true);
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        let Some(AgentCommand::RunScript { run }) = connection.commands.recv().await else {
            panic!("expected a script run");
        };
        assert_eq!(run.run_id, run_id);
        assert_eq!(run.language, ScriptLanguage::Cmd);
        assert_eq!(run.run_as, RunAs::User);
        assert_eq!(run.body, "whoami");
        assert_eq!(run.timeout_seconds, 60);

        let other = app.enroll(&mut admin, "DESKTOP-2").await;
        let report = json!({
            "status": "completed",
            "ran_as": "DESKTOP-1\\ada",
            "exit_code": 0,
            // UTF-16 output piped through cmd brings NULs, which PostgreSQL
            // text can't hold.
            "stdout": "desktop-1\\ada\u{0}\r\n",
            "stderr": "",
        });
        assert_eq!(
            other
                .report(&format!("script-runs/{run_id}/result"), report.clone())
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: another device can't report this run"
        );
        assert_eq!(
            agent
                .report(
                    &format!("script-runs/{run_id}/result"),
                    json!({ "status": "pending", "ran_as": "", "stdout": "", "stderr": "" })
                )
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        let reported = agent
            .report(&format!("script-runs/{run_id}/result"), report.clone())
            .await;
        assert_eq!(
            reported.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            reported.body
        );
        assert_eq!(
            agent
                .report(&format!("script-runs/{run_id}/result"), report)
                .await
                .status,
            StatusCode::NOT_FOUND,
            "a run is reported once"
        );

        let run = ada.get(&format!("/v1/script-runs/{run_id}")).await;
        assert_eq!(run.status, StatusCode::OK);
        assert_eq!(run.body["status"], "completed");
        assert_eq!(run.body["exit_code"], 0);
        assert_eq!(run.body["ran_as"], "DESKTOP-1\\ada");
        assert_eq!(run.body["stdout"], "desktop-1\\ada\u{fffd}\r\n");
        let listed = ada
            .get(&format!("/v1/script-runs?device_id={}", agent.device_id))
            .await
            .body;
        assert_eq!(listed["runs"].as_array().unwrap().len(), 2, "{name}");
        assert_eq!(listed["runs"][0]["id"], run_id.as_str());
        assert_eq!(listed["runs"][0]["stdout"], "", "lists leave out output");

        // Others see their own runs; the audit log's readers see everyone's.
        let (mut bob, _) = common::user_with(&app, &mut admin, "bob@example.com", RUNNER).await;
        assert!(
            bob.get("/v1/script-runs").await.body["runs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            bob.get(&format!("/v1/script-runs/{run_id}")).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            bob.post(&runs_path, start.clone()).await.status,
            StatusCode::NOT_FOUND,
            "{name}: Ada's private script isn't Bob's to run"
        );
        let (mut auditor, _) =
            common::user_with(&app, &mut admin, "auditor@example.com", &["audit.view"]).await;
        let everyone = auditor.get("/v1/script-runs").await.body;
        assert_eq!(everyone["runs"].as_array().unwrap().len(), 2, "{name}");
        assert_eq!(everyone["runs"][0]["requested_by_you"], false);
        assert_eq!(
            auditor.get(&format!("/v1/script-runs/{run_id}")).await.body["stdout"],
            "desktop-1\\ada\u{fffd}\r\n"
        );
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer.get("/v1/script-runs").await.status,
            StatusCode::FORBIDDEN
        );

        let audited = audit_events(&app, "script.run").await;
        assert_eq!(audited.len(), 2, "{name}");
        assert_eq!(audited[0].actor_user_id.as_deref(), Some(ada_id.as_str()));
        assert_eq!(audited[0].target_id, agent.device_id);
        assert_eq!(audited[0].metadata["script_name"], "Who am I");

        admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert_eq!(
            ada.post(&runs_path, start).await.status,
            StatusCode::NOT_FOUND,
            "deleted devices run nothing"
        );
        drop(connection);
        app.finish().await;
    }
}

#[tokio::test]
async fn unreported_runs_are_shown_as_lost() {
    for app in common::apps().await {
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let script_id = create_script(&mut admin, "Slow").await;
        let _connection = app.state.agents.connect(&agent.device_id);
        let started = admin
            .post(
                &format!("/v1/agents/{}/script-runs", agent.device_id),
                json!({ "script_id": script_id, "run_as": "system" }),
            )
            .await;
        assert_eq!(started.status, StatusCode::CREATED);
        let long_ago = meshrmm_server::time::now_ms() - 60 * 60 * 1000;
        backdate(&app, "script_runs", long_ago).await;
        let run = admin
            .get(&format!(
                "/v1/script-runs/{}",
                started.body["id"].as_str().unwrap()
            ))
            .await;
        assert_eq!(run.body["status"], "lost", "{}", app.name);
        app.finish().await;
    }
}

async fn backdate(app: &App, table: &str, created_at: i64) {
    app.db()
        .execute(
            &sea_query::Query::update()
                .table(sea_query::Alias::new(table))
                .value(sea_query::Alias::new("created_at"), created_at)
                .to_owned(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn file_deliveries_let_the_agent_download_the_file_once() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let other = app.enroll(&mut admin, "DESKTOP-2").await;
        let (mut ada, _) = common::user_with(&app, &mut admin, "ada@example.com", RUNNER).await;
        let content = b"report contents".to_vec();
        let file_id = upload(&ada, "report.txt", &content, false, &sha256(&content))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let deliveries_path = format!("/v1/agents/{}/file-deliveries", agent.device_id);
        let start = json!({ "file_id": file_id, "destination": "public" });

        assert_eq!(
            ada.post(&deliveries_path, start.clone()).await.status,
            StatusCode::CONFLICT,
            "{name}"
        );
        let mut connection = app.state.agents.connect(&agent.device_id);
        let started = ada.post(&deliveries_path, start.clone()).await;
        assert_eq!(
            started.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            started.body
        );
        assert_eq!(started.body["destination"], "public");
        assert_eq!(started.body["status"], "pending");
        let delivery_id = started.body["id"].as_str().unwrap().to_owned();
        let Some(AgentCommand::DeliverFile { delivery }) = connection.commands.recv().await else {
            panic!("expected a delivery");
        };
        assert_eq!(delivery.delivery_id, delivery_id);
        assert_eq!(delivery.file_name, "report.txt");
        assert_eq!(delivery.size_bytes, content.len() as u64);
        assert_eq!(delivery.sha256, sha256(&content));
        assert_eq!(delivery.destination, FileDeliveryDestination::Public);

        let content_path = |device_id: &str| {
            format!("/v1/agents/{device_id}/file-deliveries/{delivery_id}/content")
        };
        let fetch = |agent: &common::Agent, path: String| {
            let request = agent
                .request(Method::GET, &path)
                .body(Body::empty())
                .unwrap();
            let agent = agent.clone();
            async move { agent.send(request).await }
        };
        let downloaded = fetch(&agent, content_path(&agent.device_id)).await;
        assert_eq!(downloaded.status, StatusCode::OK, "{name}");
        assert_eq!(downloaded.body, content);
        assert_eq!(
            fetch(&other, content_path(&other.device_id)).await.status,
            StatusCode::NOT_FOUND,
            "{name}: another device can't fetch it"
        );
        assert_eq!(
            fetch(
                &agent.with_token(&other.token),
                content_path(&agent.device_id)
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );

        let reported = agent
            .report(
                &format!("file-deliveries/{delivery_id}/result"),
                json!({ "status": "delivered", "path": "C:\\Users\\Public\\Documents\\report.txt" }),
            )
            .await;
        assert_eq!(
            reported.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            reported.body
        );
        assert_eq!(
            fetch(&agent, content_path(&agent.device_id)).await.status,
            StatusCode::NOT_FOUND,
            "a finished delivery's file can't be fetched again"
        );
        let delivery = ada.get(&format!("/v1/file-deliveries/{delivery_id}")).await;
        assert_eq!(delivery.body["status"], "delivered", "{name}");
        assert_eq!(
            delivery.body["path"],
            "C:\\Users\\Public\\Documents\\report.txt"
        );
        let listed = ada.get("/v1/file-deliveries").await.body;
        assert_eq!(listed["deliveries"].as_array().unwrap().len(), 2);
        assert_eq!(listed["deliveries"][1]["status"], "failed");

        // A pending delivery's file stops being available after its window.
        let second = ada.post(&deliveries_path, start.clone()).await;
        let second_id = second.body["id"].as_str().unwrap().to_owned();
        connection.commands.recv().await.unwrap();
        backdate(
            &app,
            "file_deliveries",
            meshrmm_server::time::now_ms() - 31 * 60 * 1000,
        )
        .await;
        let late = format!(
            "/v1/agents/{}/file-deliveries/{second_id}/content",
            agent.device_id
        );
        assert_eq!(
            fetch(&agent, late).await.status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
        assert_eq!(
            ada.get(&format!("/v1/file-deliveries/{second_id}"))
                .await
                .body["status"],
            "lost"
        );

        let (mut bob, _) = common::user_with(&app, &mut admin, "bob@example.com", RUNNER).await;
        assert_eq!(
            bob.post(&deliveries_path, start).await.status,
            StatusCode::NOT_FOUND,
            "{name}: Ada's private file isn't Bob's to send"
        );
        assert_eq!(audit_events(&app, "file.deliver").await.len(), 3);
        app.finish().await;
    }
}

/// Makes a file look `age` old to the maintenance task.
fn age_file(path: &std::path::Path, age: std::time::Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - age)
        .unwrap();
}

#[tokio::test]
async fn maintenance_removes_files_nothing_refers_to() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let kept = upload(&admin, "kept.txt", b"kept", false, &sha256(b"kept")).await;
        let orphan = upload(&admin, "orphan.txt", b"orphan", false, &sha256(b"orphan")).await;
        let (kept, orphan) = (
            kept.body["id"].as_str().unwrap().to_owned(),
            orphan.body["id"].as_str().unwrap().to_owned(),
        );
        // As if the server stopped between storing the content and its row.
        app.db()
            .execute(
                &sea_query::Query::delete()
                    .from_table(sea_query::Alias::new("toolbox_files"))
                    .and_where(sea_query::ExprTrait::eq(
                        sea_query::Expr::col(sea_query::Alias::new("id")),
                        orphan.as_str(),
                    ))
                    .to_owned(),
            )
            .await
            .unwrap();
        let live = app.enroll(&mut admin, "live").await;
        let deleted = app.enroll(&mut admin, "deleted").await;
        let storage = &app.state.storage;
        for agent in [&live, &deleted] {
            std::fs::write(storage.thumbnail(&agent.device_id), b"\xff\xd8\xffjpeg").unwrap();
        }
        // As if removing the deleted device's image had failed.
        app.db()
            .execute(
                &sea_query::Query::update()
                    .table(sea_query::Alias::new("agents"))
                    .value(sea_query::Alias::new("deletion_requested_at"), 1)
                    .and_where(sea_query::ExprTrait::eq(
                        sea_query::Expr::col(sea_query::Alias::new("id")),
                        deleted.device_id.as_str(),
                    ))
                    .to_owned(),
            )
            .await
            .unwrap();
        let maintenance = || meshrmm_server::maintenance::remove_orphans(app.db(), storage);
        assert_eq!(
            maintenance().await.unwrap(),
            0,
            "{name}: recent files are left alone"
        );

        let day = std::time::Duration::from_secs(25 * 60 * 60);
        for path in [
            storage.toolbox_file(&kept),
            storage.toolbox_file(&orphan),
            storage.thumbnail(&live.device_id),
            storage.thumbnail(&deleted.device_id),
        ] {
            age_file(&path, day);
        }
        assert_eq!(maintenance().await.unwrap(), 2, "{name}");
        assert!(storage.toolbox_file(&kept).exists());
        assert!(!storage.toolbox_file(&orphan).exists());
        assert!(storage.thumbnail(&live.device_id).exists());
        assert!(!storage.thumbnail(&deleted.device_id).exists());
        app.finish().await;
    }
}
