//! Jobs that run a script or send a file on many devices, and automation
//! policies that start them on a schedule or when devices enroll.
mod common;

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use common::{
    App, Browser, apps, audit_events, job_with_devices, receive_any_json, set_up, user_with,
};
use meshrmm_protocol_types::{AgentCommand, FileDeliveryDestination, RunAs};
use meshrmm_server::{
    automation,
    realtime::AgentConnection,
    time::{HOUR_MS, MINUTE_MS, SECOND_MS, now_ms},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const RUNNER: &[&str] = &["devices.view", "scripts.run", "files.deliver"];

async fn create_script(browser: &mut Browser, name: &str, language: &str, shared: bool) -> String {
    let created = browser
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": name, "language": language, "body": "echo hi", "timeout_seconds": 60, "shared": shared }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

/// Starts a job, and returns it with its devices.
async fn start_job(browser: &mut Browser, body: Value) -> Value {
    let started = browser.post("/v1/jobs", body).await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
    assert!(started.body.get("devices").is_none(), "{:?}", started.body);
    job(browser, started.body["id"].as_str().unwrap()).await
}

/// The job with its devices, by name.
async fn job(browser: &mut Browser, id: &str) -> Value {
    let mut job = job_with_devices(browser, id).await;
    job["devices"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|device| {
            device["device_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        });
    job
}

/// Each device's outcome, by device name.
fn outcomes(job: &Value) -> Vec<(String, String)> {
    job["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|device| {
            (
                device["device_name"].as_str().unwrap().to_owned(),
                device["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn pass(app: &App, now: i64) -> bool {
    automation::pass(&app.state, now).await.unwrap()
}

/// The script runs queued for a stand-in Agent connection.
fn runs(connection: &mut AgentConnection) -> Vec<meshrmm_protocol_types::ScriptRunRequest> {
    let mut runs = Vec::new();
    while let Ok(message) = connection.commands.try_recv() {
        match message {
            meshrmm_server::realtime::ToAgent::Command(AgentCommand::RunScript { run }) => {
                runs.push(run)
            }
            meshrmm_server::realtime::ToAgent::Command(AgentCommand::CollectSystemInfo) => {}
            other => panic!("expected a script run, got {other:?}"),
        }
    }
    runs
}

/// The next job the event socket shows.
async fn job_update(events: &mut common::Ws) -> Value {
    loop {
        let message = receive_any_json(events).await;
        if message["type"] == "job_update" {
            return message;
        }
    }
}

fn report(status: &str, exit_code: Option<i32>) -> Value {
    json!({ "status": status, "ran_as": "SYSTEM", "exit_code": exit_code, "stdout": "done\n", "stderr": "" })
}

#[tokio::test]
async fn jobs_run_on_each_device_that_can_and_report_every_outcome() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let online = app.enroll(&mut admin, "WIN-ONLINE").await;
        let offline = app.enroll(&mut admin, "WIN-OFFLINE").await;
        let mac = app.enroll_on(&mut admin, "MAC", "macos").await;
        let linux = app.enroll_on(&mut admin, "LINUX", "linux").await;
        let (mut ada, ada_id) = user_with(&app, &mut admin, "ada@example.com", RUNNER).await;
        let (mut bob, _) = user_with(&app, &mut admin, "bob@example.com", RUNNER).await;
        let mut events = ada.events(&server).await.unwrap();
        let mut connection = app.state.agents.connect(&online.device_id);
        let _mac = app.state.agents.connect(&mac.device_id);
        let script = create_script(&mut ada, "Patch", "powershell", false).await;
        let devices = [&online, &offline, &mac, &linux]
            .iter()
            .map(|agent| agent.device_id.clone())
            .collect::<Vec<_>>();
        let body = json!({
            "script_id": script, "run_as": "system", "target": "devices",
            "device_ids": devices, "offline": "wait", "wait_hours": 2,
        });

        let started = start_job(&mut ada, body.clone()).await;
        let id = started["id"].as_str().unwrap().to_owned();
        assert_eq!(started["target_label"], "4 devices", "{name}");
        assert_eq!(started["requested_by_you"], true);
        assert_eq!(started["can_cancel"], true);
        assert_eq!(
            outcomes(&started),
            [
                ("LINUX".into(), "skipped".into()),
                ("MAC".into(), "skipped".into()),
                ("WIN-OFFLINE".into(), "waiting".into()),
                ("WIN-ONLINE".into(), "queued".into()),
            ],
            "{name}: a Mac and a Linux computer have no PowerShell"
        );
        assert_eq!(
            started["devices"][0]["detail"],
            "Linux has no PowerShell interpreter"
        );

        let now = now_ms();
        pass(&app, now).await;
        let sent = runs(&mut connection);
        assert_eq!(sent.len(), 1, "{name}");
        assert_eq!(sent[0].run_as, RunAs::System);
        assert_eq!(sent[0].body, "echo hi");
        let first_run = sent[0].run_id.clone();
        let update = job_update(&mut events).await;
        assert_eq!(update["type"], "job_update", "{name}: {update}");
        assert_eq!(update["job"]["id"], id.as_str());
        assert_eq!(update["job"]["counts"]["running"], 1);
        assert_eq!(update["job"]["counts"]["waiting"], 1);
        assert_eq!(update["job"]["counts"]["skipped"], 2);

        // The script changes; the job keeps what it started with.
        let changed = ada
            .put(
                &format!("/v1/toolbox/scripts/{script}"),
                json!({ "name": "Patch", "language": "powershell", "body": "changed", "timeout_seconds": 60 }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK);
        assert_eq!(
            online
                .report(
                    &format!("script-runs/{first_run}/result"),
                    report("completed", Some(0))
                )
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let mut late = app.state.agents.connect(&offline.device_id);
        pass(&app, now + SECOND_MS).await;
        let sent = runs(&mut late);
        assert_eq!(
            sent.len(),
            1,
            "{name}: the waiting device runs once it connects"
        );
        assert_eq!(sent[0].body, "echo hi");
        offline
            .report(
                &format!("script-runs/{}/result", sent[0].run_id),
                report("completed", Some(1)),
            )
            .await;
        pass(&app, now + 2 * SECOND_MS).await;
        let finished = job(&mut ada, &id).await;
        assert_eq!(
            finished["counts"],
            json!({
                "total": 4, "queued": 0, "waiting": 0, "running": 0, "succeeded": 1,
                "failed": 1, "timed_out": 0, "no_result": 0, "skipped": 2, "missed": 0,
                "cancelled": 0,
            }),
            "{name}"
        );
        assert!(finished["finished_at_unix_ms"].is_i64());
        assert_eq!(finished["can_cancel"], false);
        let failed = &finished["devices"][2];
        assert_eq!(failed["outcome"], "failed");
        assert_eq!(failed["exit_code"], 1);
        let run_id = failed["run_id"].as_str().unwrap();

        // The job's runs read like any others, in the device's history only.
        let run = ada.get(&format!("/v1/script-runs/{run_id}")).await;
        assert_eq!(run.body["source"], "job", "{name}");
        assert_eq!(run.body["job_id"], id.as_str());
        assert_eq!(run.body["stdout"], "done\n");
        assert!(
            ada.get("/v1/script-runs").await.body["runs"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{name}: jobs' runs stay out of the run history"
        );
        let history = ada
            .get(&format!("/v1/script-runs?device_id={}", offline.device_id))
            .await;
        assert_eq!(history.body["runs"][0]["job_id"], id.as_str());

        // Others see their own jobs; the audit log's readers see everyone's.
        assert_eq!(
            bob.get(&format!("/v1/jobs/{id}")).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            bob.get(&format!("/v1/script-runs/{run_id}")).await.status,
            StatusCode::NOT_FOUND
        );
        assert!(
            bob.get("/v1/jobs").await.body["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let listed = admin.get("/v1/jobs").await.body;
        assert_eq!(listed["jobs"][0]["id"], id.as_str(), "{name}");
        assert_eq!(listed["jobs"][0]["requested_by_you"], false);
        assert_eq!(listed["jobs"][0]["requested_by"], "ada@example.com");
        let (mut viewer, _) = user_with(&app, &mut admin, "v@example.com", &["devices.view"]).await;
        assert_eq!(viewer.get("/v1/jobs").await.status, StatusCode::FORBIDDEN);

        // Whole companies and every device need their own permission.
        let mut everywhere = body.clone();
        everywhere["target"] = json!("all");
        let refused = ada.post("/v1/jobs", everywhere.clone()).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        everywhere["script_id"] = json!(create_script(&mut admin, "All", "shell", true).await);
        let all = start_job(&mut admin, everywhere).await;
        assert_eq!(all["target_label"], "All devices");
        assert_eq!(all["counts"]["skipped"], 2, "{name}: Windows has no shell");

        let audited = audit_events(&app, "job.start").await;
        assert_eq!(audited.len(), 2, "{name}");
        let ada_job = audited.iter().find(|event| event.target_id == id).unwrap();
        assert_eq!(ada_job.actor_user_id.as_deref(), Some(ada_id.as_str()));
        assert_eq!(ada_job.metadata["devices"], 4);
        assert_eq!(ada_job.metadata["skipped"], 2);
        assert!(
            audit_events(&app, "script.run").await.is_empty(),
            "a job is audited once, not per device"
        );
        drop(connection);
        app.finish().await;
    }
}

#[tokio::test]
async fn offline_devices_are_skipped_waited_for_or_cancelled() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESK").await;
        let script = create_script(&mut admin, "Restart spooler", "cmd", false).await;
        let body = |offline: &str| {
            json!({
                "script_id": script, "run_as": "user", "target": "devices",
                "device_ids": [agent.device_id], "offline": offline, "wait_hours": 1,
            })
        };
        let skipped = start_job(&mut admin, body("skip")).await;
        assert_eq!(skipped["devices"][0]["outcome"], "missed", "{name}");
        assert_eq!(skipped["devices"][0]["detail"], "The device was offline");
        let now = now_ms();
        pass(&app, now).await;
        assert!(
            job(&mut admin, skipped["id"].as_str().unwrap()).await["finished_at_unix_ms"].is_i64()
        );

        let expiring = start_job(&mut admin, body("wait")).await;
        let expiring_id = expiring["id"].as_str().unwrap();
        pass(&app, now).await;
        assert_eq!(
            job(&mut admin, expiring_id).await["devices"][0]["outcome"],
            "waiting"
        );
        pass(&app, now + HOUR_MS + SECOND_MS).await;
        let expired = job(&mut admin, expiring_id).await;
        assert_eq!(expired["devices"][0]["outcome"], "missed", "{name}");
        assert_eq!(
            expired["devices"][0]["detail"],
            "The device didn't connect before the job expired"
        );

        let cancelling = start_job(&mut admin, body("wait")).await;
        let cancelling_id = cancelling["id"].as_str().unwrap();
        let (mut ada, _) = user_with(&app, &mut admin, "ada@example.com", &["audit.view"]).await;
        assert_eq!(
            ada.post(&format!("/v1/jobs/{cancelling_id}/cancel"), json!({}))
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{name}: reading the audit log doesn't cancel others' jobs"
        );
        let cancelled = admin
            .post(&format!("/v1/jobs/{cancelling_id}/cancel"), json!({}))
            .await;
        assert_eq!(cancelled.status, StatusCode::OK, "{:?}", cancelled.body);
        assert_eq!(cancelled.body["counts"]["cancelled"], 1, "{name}");
        assert!(cancelled.body.get("devices").is_none(), "{name}");
        assert_eq!(
            job(&mut admin, cancelling_id).await["devices"][0]["outcome"],
            "cancelled"
        );
        let mut connection = app.state.agents.connect(&agent.device_id);
        pass(&app, now_ms()).await;
        assert!(runs(&mut connection).is_empty(), "{name}");
        assert_eq!(audit_events(&app, "job.cancel").await.len(), 1);

        // A device deleted while a job waits for it misses it.
        drop(connection);
        let waiting = start_job(&mut admin, body("wait")).await;
        admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        pass(&app, now_ms()).await;
        let gone = job(&mut admin, waiting["id"].as_str().unwrap()).await;
        assert_eq!(gone["devices"][0]["outcome"], "missed", "{name}");
        assert_eq!(gone["devices"][0]["detail"], "The device was deleted");
        app.finish().await;
    }
}

#[tokio::test]
async fn the_dispatcher_limits_each_devices_runs_and_files_are_delivered() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESK").await;
        let mut connection = app.state.agents.connect(&agent.device_id);
        let script = create_script(&mut admin, "Inventory", "powershell", false).await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            let started = start_job(
                &mut admin,
                json!({
                    "script_id": script, "run_as": "system", "target": "devices",
                    "device_ids": [agent.device_id], "offline": "skip",
                }),
            )
            .await;
            ids.push(started["id"].as_str().unwrap().to_owned());
        }
        let now = now_ms();
        pass(&app, now).await;
        let first = runs(&mut connection);
        assert_eq!(
            first.len(),
            automation::dispatch::MAX_PER_DEVICE as usize,
            "{name}"
        );
        assert_eq!(
            job(&mut admin, &ids[2]).await["devices"][0]["outcome"],
            "queued"
        );
        agent
            .report(
                &format!("script-runs/{}/result", first[0].run_id),
                report("timed_out", None),
            )
            .await;
        pass(&app, now + SECOND_MS).await;
        assert_eq!(runs(&mut connection).len(), 1, "{name}");
        assert_eq!(
            job(&mut admin, &ids[0]).await["devices"][0]["outcome"],
            "timed_out"
        );
        // A run its device never reports has no result after its timeout.
        backdate_runs(&app, &ids[1], 10 * MINUTE_MS).await;
        assert_eq!(
            job(&mut admin, &ids[1]).await["devices"][0]["outcome"],
            "no_result",
            "{name}"
        );

        // Files go the same way, checked by the Agent as before.
        let content = b"installer";
        let digest = meshrmm_server::secrets::hex(&<sha2::Sha256 as sha2::Digest>::digest(content));
        let request = admin
            .request(
                axum::http::Method::POST,
                &format!("/v1/toolbox/files?name=setup.exe&folder=&shared=false&sha256={digest}"),
            )
            .header("content-type", "application/octet-stream")
            .body(axum::body::Body::from(content.to_vec()))
            .unwrap();
        let uploaded = admin.clone().raw(request).await;
        assert_eq!(uploaded.status, StatusCode::CREATED);
        let file_id = uploaded.body["id"].as_str().unwrap();
        let sent = start_job(
            &mut admin,
            json!({
                "file_id": file_id, "destination": "public", "target": "devices",
                "device_ids": [agent.device_id], "offline": "skip",
            }),
        )
        .await;
        assert_eq!(sent["kind"], "file");
        assert_eq!(sent["destination"], "public");
        // Its earlier runs are finished or overdue, so there's room.
        pass(&app, now + 10 * MINUTE_MS).await;
        let delivery = loop {
            match connection.commands.try_recv() {
                Ok(meshrmm_server::realtime::ToAgent::Command(AgentCommand::DeliverFile {
                    delivery,
                })) => break delivery,
                Ok(_) => {}
                Err(error) => panic!("{name}: no delivery: {error}"),
            }
        };
        assert_eq!(delivery.destination, FileDeliveryDestination::Public);
        assert_eq!(delivery.sha256, digest);
        let reported = agent
            .report(
                &format!("file-deliveries/{}/result", delivery.delivery_id),
                json!({ "status": "delivered", "path": "C:\\Users\\Public\\Documents\\setup.exe" }),
            )
            .await;
        assert_eq!(
            reported.status,
            StatusCode::NO_CONTENT,
            "{:?}",
            reported.body
        );
        let delivered = job(&mut admin, sent["id"].as_str().unwrap()).await;
        assert_eq!(delivered["devices"][0]["outcome"], "succeeded", "{name}");
        assert_eq!(
            delivered["devices"][0]["path"],
            "C:\\Users\\Public\\Documents\\setup.exe"
        );
        for invalid in [
            json!({ "target": "devices", "device_ids": [agent.device_id], "offline": "skip" }),
            json!({ "script_id": script, "file_id": file_id, "run_as": "user", "target": "devices", "device_ids": [agent.device_id], "offline": "skip" }),
            json!({ "script_id": script, "target": "devices", "device_ids": [agent.device_id], "offline": "skip" }),
            json!({ "script_id": script, "run_as": "user", "target": "devices", "device_ids": [], "offline": "skip" }),
            json!({ "script_id": script, "run_as": "user", "target": "companies", "offline": "skip" }),
            json!({ "script_id": script, "run_as": "user", "target": "devices", "device_ids": [agent.device_id], "offline": "wait", "wait_hours": 0 }),
        ] {
            assert_eq!(
                admin.post("/v1/jobs", invalid.clone()).await.status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid}"
            );
        }
        let nowhere = admin
            .post(
                "/v1/jobs",
                json!({ "script_id": script, "run_as": "user", "target": "devices", "device_ids": [uuid::Uuid::new_v4().to_string()], "offline": "skip" }),
            )
            .await;
        assert_eq!(nowhere.code(), "no_devices", "{name}");
        app.finish().await;
    }
}

/// The script runs and update installs queued for a stand-in Agent
/// connection.
fn started(connection: &mut AgentConnection) -> Vec<AgentCommand> {
    let mut commands = Vec::new();
    while let Ok(message) = connection.commands.try_recv() {
        if let meshrmm_server::realtime::ToAgent::Command(
            command @ (AgentCommand::RunScript { .. } | AgentCommand::InstallUpdates { .. }),
        ) = message
        {
            commands.push(command);
        }
    }
    commands
}

#[tokio::test]
async fn devices_that_cannot_start_yet_do_not_hold_up_later_jobs() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let script = create_script(&mut admin, "Inventory", "powershell", false).await;
        let mut busy = Vec::new();
        let mut patching = Vec::new();
        for index in 0..3 {
            busy.push(app.enroll(&mut admin, &format!("BUSY-{index}")).await);
            let agent = app.enroll(&mut admin, &format!("PATCHING-{index}")).await;
            meshrmm_server::updates::store::store_scan(
                app.db(),
                &agent.device_id,
                serde_json::from_value(json!({
                    "scanned_at_unix_ms": 1, "manager": "windows_update",
                    "updates": [], "reboot_pending": false,
                }))
                .unwrap(),
                now_ms(),
            )
            .await
            .unwrap();
            patching.push(agent);
        }
        let free = app.enroll(&mut admin, "FREE").await;
        let ids = |agents: &[common::Agent]| {
            agents
                .iter()
                .map(|agent| agent.device_id.clone())
                .collect::<Vec<_>>()
        };
        let (busy, patching) = (ids(&busy), ids(&patching));
        let mut connections = busy
            .iter()
            .chain(&patching)
            .map(|id| app.state.agents.connect(id))
            .collect::<Vec<_>>();
        let mut free_connection = app.state.agents.connect(&free.device_id);
        let script_job = |devices: &[String]| {
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": devices, "offline": "skip",
            })
        };
        let install_job = json!({
            "updates": { "action": "install", "categories": ["security"], "reboot": "never" },
            "target": "devices", "device_ids": patching, "offline": "skip",
        });

        // The busy devices take as many runs as they may, and the others
        // start installing.
        for _ in 0..automation::dispatch::MAX_PER_DEVICE {
            start_job(&mut admin, script_job(&busy)).await;
        }
        start_job(&mut admin, install_job.clone()).await;
        pass(&app, now_ms()).await;
        let sent = connections.iter_mut().map(started).collect::<Vec<_>>();
        assert!(
            sent[..3]
                .iter()
                .all(|commands| commands.len() == automation::dispatch::MAX_PER_DEVICE as usize),
            "{name}: {sent:?}"
        );
        assert!(sent[3..].iter().all(|commands| commands.len() == 1));

        // More than a pass reads of parts that can't start yet: runs on the
        // busy devices, then installs on the installing ones.
        for _ in 0..34 {
            start_job(&mut admin, script_job(&busy)).await;
        }
        for _ in 0..34 {
            start_job(&mut admin, install_job.clone()).await;
        }
        assert!(
            !pass(&app, now_ms()).await,
            "{name}: nothing queued can start, so there's no hurry"
        );
        assert!(
            connections
                .iter_mut()
                .all(|connection| started(connection).is_empty())
        );

        // A later job on another device starts in the next pass.
        let later = start_job(
            &mut admin,
            script_job(std::slice::from_ref(&free.device_id)),
        )
        .await;
        assert!(
            !pass(&app, now_ms()).await,
            "{name}: it started all that it could"
        );
        assert_eq!(started(&mut free_connection).len(), 1, "{name}");
        assert_eq!(
            job(&mut admin, later["id"].as_str().unwrap()).await["devices"][0]["outcome"],
            "running"
        );
        assert!(
            connections
                .iter_mut()
                .all(|connection| started(connection).is_empty())
        );
        app.finish().await;
    }
}

/// Moves a job's runs into the past, with the report deadlines its devices
/// keep.
async fn backdate_runs(app: &App, job_id: &str, by_ms: i64) {
    use meshrmm_server::db::tables::{JobDevices, ScriptRuns};
    use sea_query::{Expr, ExprTrait, Query};
    app.db()
        .execute(
            &Query::update()
                .table(ScriptRuns::Table)
                .value(
                    ScriptRuns::CreatedAt,
                    Expr::col(ScriptRuns::CreatedAt).sub(by_ms),
                )
                .and_where(Expr::col(ScriptRuns::JobId).eq(job_id))
                .to_owned(),
        )
        .await
        .unwrap();
    app.db()
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(
                    JobDevices::ReportBy,
                    Expr::col(JobDevices::ReportBy).sub(by_ms),
                )
                .and_where(Expr::col(JobDevices::JobId).eq(job_id))
                .to_owned(),
        )
        .await
        .unwrap();
}

async fn create_policy(admin: &mut Browser, body: Value) -> Value {
    let created = admin.post("/v1/automation-policies", body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body
}

async fn policy_jobs(admin: &mut Browser, policy_id: &str) -> Vec<Value> {
    admin
        .get(&format!("/v1/jobs?policy_id={policy_id}"))
        .await
        .body["jobs"]
        .as_array()
        .unwrap()
        .clone()
}

async fn set_next_run(app: &App, policy_id: &str, at: i64) {
    use sea_query::{Expr, ExprTrait, Query};
    app.db()
        .execute(
            &Query::update()
                .table(meshrmm_server::db::tables::AutomationPolicies::Table)
                .value(
                    meshrmm_server::db::tables::AutomationPolicies::NextRunAt,
                    at,
                )
                .and_where(
                    Expr::col(meshrmm_server::db::tables::AutomationPolicies::Id).eq(policy_id),
                )
                .to_owned(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn interval_policies_fire_once_per_time_even_across_a_restart() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESK").await;
        let script = create_script(&mut admin, "Health check", "powershell", true).await;
        let before = now_ms();
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Every five minutes", "script_id": script, "run_as": "system",
                "target": "all", "trigger": "interval", "interval_minutes": 5,
                "offline": "wait", "catch_up_hours": 24,
            }),
        )
        .await;
        let id = policy["id"].as_str().unwrap().to_owned();
        let next = policy["next_run_at"].as_i64().unwrap();
        assert!(
            (before + 5 * MINUTE_MS..=now_ms() + 5 * MINUTE_MS).contains(&next),
            "{name}: the first run is an interval after saving"
        );
        assert_eq!(policy["owned_by_you"], true);

        pass(&app, next - SECOND_MS).await;
        assert!(policy_jobs(&mut admin, &id).await.is_empty(), "{name}");
        pass(&app, next).await;
        pass(&app, next + SECOND_MS).await;
        let restarted = app.restarted().await;
        automation::pass(&restarted.state, next + 2 * SECOND_MS)
            .await
            .unwrap();
        let fired = policy_jobs(&mut admin, &id).await;
        assert_eq!(fired.len(), 1, "{name}: fired once");
        assert_eq!(fired[0]["policy_name"], "Every five minutes");
        assert_eq!(fired[0]["devices"], Value::Null);
        assert_eq!(fired[0]["counts"]["waiting"], 1);
        let listed = admin.get("/v1/automation-policies").await.body;
        let listed = &listed["policies"][0];
        assert_eq!(listed["next_run_at"], next + 5 * MINUTE_MS, "{name}");
        assert_eq!(listed["last_job"]["id"], fired[0]["id"]);
        let audited = audit_events(&app, "job.start").await;
        assert_eq!(audited.len(), 1);
        assert_eq!(audited[0].actor_user_id, None);
        assert_eq!(
            audited[0].actor_label,
            "policy \u{201c}Every five minutes\u{201d}"
        );
        assert_eq!(audited[0].metadata["policy_id"], id.as_str());

        // The next run replaces the device's wait for the last one.
        automation::pass(&restarted.state, next + 5 * MINUTE_MS)
            .await
            .unwrap();
        let fired = policy_jobs(&mut admin, &id).await;
        assert_eq!(fired.len(), 2, "{name}");
        let first = job(&mut admin, fired[1]["id"].as_str().unwrap()).await;
        assert_eq!(first["devices"][0]["outcome"], "missed", "{name}");
        assert_eq!(
            first["devices"][0]["detail"],
            "The policy ran again before the device connected"
        );

        // Hours of downtime: an interval policy runs once, then keeps time.
        set_next_run(&app, &id, next + 10 * MINUTE_MS).await;
        let late = next + 10 * MINUTE_MS + 3 * HOUR_MS + MINUTE_MS;
        pass(&app, late).await;
        pass(&app, late + SECOND_MS).await;
        assert_eq!(policy_jobs(&mut admin, &id).await.len(), 3, "{name}");
        let listed = admin.get("/v1/automation-policies").await.body;
        assert_eq!(
            listed["policies"][0]["next_run_at"],
            next + 10 * MINUTE_MS + 3 * HOUR_MS + 5 * MINUTE_MS
        );

        // Turned off, it doesn't run.
        let off = json!({
            "name": "Every five minutes", "enabled": false, "script_id": script, "run_as": "system",
            "target": "all", "trigger": "interval", "interval_minutes": 5, "offline": "wait",
        });
        let saved = admin
            .put(&format!("/v1/automation-policies/{id}"), off.clone())
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["next_run_at"], Value::Null);
        pass(&app, late + HOUR_MS).await;
        assert_eq!(policy_jobs(&mut admin, &id).await.len(), 3, "{name}");
        let run_now = admin
            .post(&format!("/v1/automation-policies/{id}/run"), json!({}))
            .await;
        assert_eq!(run_now.status, StatusCode::CREATED, "{:?}", run_now.body);
        assert_eq!(run_now.body["requested_by_you"], true);
        assert_eq!(audit_events(&app, "automation_policy.run").await.len(), 1);
        drop(agent);
        restarted.finish().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn times_of_day_missed_while_down_are_skipped_and_said_why() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        app.enroll(&mut admin, "DESK").await;
        let script = create_script(&mut admin, "Defrag", "cmd", true).await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Nightly", "script_id": script, "run_as": "system", "target": "all",
                "trigger": "daily", "time": "03:00", "time_zone": "America/Chicago",
                "offline": "skip",
            }),
        )
        .await;
        let id = policy["id"].as_str().unwrap().to_owned();
        let next = policy["next_run_at"].as_i64().unwrap();
        assert_eq!(policy["time"], "03:00");
        assert_eq!(policy["time_zone"], "America/Chicago");
        assert!(next > now_ms() && next <= now_ms() + 25 * HOUR_MS);

        // Down for two hours over its time: skipped until tomorrow.
        pass(&app, next + 2 * HOUR_MS).await;
        assert!(policy_jobs(&mut admin, &id).await.is_empty(), "{name}");
        let listed = admin.get("/v1/automation-policies").await.body;
        let listed = &listed["policies"][0];
        assert!(
            listed["last_problem"]
                .as_str()
                .unwrap()
                .contains("the server wasn't running"),
            "{name}: {listed}"
        );
        let tomorrow = listed["next_run_at"].as_i64().unwrap();
        assert!(tomorrow > next + 2 * HOUR_MS && tomorrow <= next + 25 * HOUR_MS);
        assert_eq!(audit_events(&app, "automation_policy.skip").await.len(), 1);

        // Down for half an hour: it runs late, once.
        pass(&app, tomorrow + 30 * MINUTE_MS).await;
        pass(&app, tomorrow + 31 * MINUTE_MS).await;
        let fired = policy_jobs(&mut admin, &id).await;
        assert_eq!(fired.len(), 1, "{name}");
        assert_eq!(fired[0]["counts"]["missed"], 1, "skips the offline device");

        // A policy that runs once needs a time ahead.
        let past = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Once", "script_id": script, "run_as": "system", "target": "all",
                    "trigger": "once", "run_at": "2001-01-01T00:00", "time_zone": "UTC",
                    "offline": "skip",
                }),
            )
            .await;
        assert_eq!(past.status, StatusCode::BAD_REQUEST, "{name}");
        let once = create_policy(
            &mut admin,
            json!({
                "name": "Once", "script_id": script, "run_as": "system", "target": "all",
                "trigger": "once", "run_at": "2099-01-01T00:00", "time_zone": "Asia/Tokyo",
                "offline": "skip",
            }),
        )
        .await;
        let at = once["next_run_at"].as_i64().unwrap();
        pass(&app, at).await;
        let listed = admin.get("/v1/automation-policies").await.body;
        let once = listed["policies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|policy| policy["name"] == "Once")
            .unwrap();
        assert_eq!(once["next_run_at"], Value::Null, "{name}: done");
        assert!(once["last_run_at"].is_i64());
        app.finish().await;
    }
}

#[tokio::test]
async fn policies_run_shared_scripts_with_their_owners_access() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        app.enroll(&mut admin, "DESK").await;
        let private = create_script(&mut admin, "Mine", "powershell", false).await;
        let shared = create_script(&mut admin, "Ours", "powershell", true).await;
        let body = |script: &str, name: &str| {
            json!({
                "name": name, "script_id": script, "run_as": "user", "target": "all",
                "trigger": "interval", "interval_minutes": 60, "offline": "skip",
            })
        };
        let refused = admin
            .post("/v1/automation-policies", body(&private, "Private"))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(refused.code(), "script_not_shared");

        let manager_role = common::add_role(
            &mut admin,
            "Automation",
            &["devices.view", "scripts.run", "automation.manage"],
        )
        .await;
        let (mut manager, _) =
            common::add_user(&app, &mut admin, "manager@example.com", &[&manager_role]).await;
        let (mut technician, _) = user_with(&app, &mut admin, "tech@example.com", RUNNER).await;
        assert_eq!(
            technician
                .post("/v1/automation-policies", body(&shared, "Tech"))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            technician.get("/v1/automation-policies").await.status,
            StatusCode::FORBIDDEN
        );
        let policy = create_policy(&mut manager, body(&shared, "Hourly")).await;
        let id = policy["id"].as_str().unwrap().to_owned();
        assert_eq!(policy["owner"], "manager@example.com");
        assert_eq!(
            manager
                .post("/v1/automation-policies", body(&shared, "Hourly"))
                .await
                .code(),
            "name_taken"
        );

        // Its script can't be deleted or unshared while it runs it.
        let deleted = admin.delete(&format!("/v1/toolbox/scripts/{shared}")).await;
        assert_eq!(deleted.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(deleted.code(), "script_in_use");
        let unshared = admin
            .put(
                &format!("/v1/toolbox/scripts/{shared}"),
                json!({ "name": "Ours", "language": "powershell", "body": "x", "shared": false }),
            )
            .await;
        assert_eq!(unshared.code(), "script_in_use", "{name}");

        // Its owner loses the permission: it doesn't run, and says why.
        let changed = admin
            .patch(
                &format!("/v1/roles/{manager_role}"),
                json!({ "permissions": ["devices.view", "scripts.run"] }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        let next = policy["next_run_at"].as_i64().unwrap();
        pass(&app, next).await;
        assert!(policy_jobs(&mut admin, &id).await.is_empty(), "{name}");
        let listed = admin.get("/v1/automation-policies").await.body;
        assert!(
            listed["policies"][0]["last_problem"]
                .as_str()
                .unwrap()
                .contains("no longer has the automation.manage"),
            "{name}: {listed}"
        );
        // An administrator saves it, and it runs with their access.
        let saved = admin
            .put(
                &format!("/v1/automation-policies/{id}"),
                body(&shared, "Hourly"),
            )
            .await;
        assert_eq!(saved.body["owner"], "Ada Admin", "{:?}", saved.body);
        assert_eq!(saved.body["last_problem"], Value::Null);
        pass(&app, saved.body["next_run_at"].as_i64().unwrap()).await;
        assert_eq!(policy_jobs(&mut admin, &id).await.len(), 1, "{name}");

        for invalid in [
            json!({ "name": "", "script_id": shared, "run_as": "user", "target": "all", "trigger": "interval", "interval_minutes": 60, "offline": "skip" }),
            json!({ "name": "X", "script_id": shared, "run_as": "user", "target": "all", "trigger": "interval", "interval_minutes": 1, "offline": "skip" }),
            json!({ "name": "X", "script_id": shared, "run_as": "user", "target": "all", "trigger": "weekly", "time": "08:00", "time_zone": "UTC", "offline": "skip" }),
            json!({ "name": "X", "script_id": shared, "run_as": "user", "target": "devices", "trigger": "enroll", "device_ids": [uuid::Uuid::new_v4().to_string()], "offline": "skip" }),
            json!({ "name": "X", "script_id": shared, "run_as": "user", "target": "all", "trigger": "hourly", "offline": "skip" }),
        ] {
            assert_eq!(
                admin
                    .post("/v1/automation-policies", invalid.clone())
                    .await
                    .status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid}"
            );
        }
        let deleted = admin.delete(&format!("/v1/automation-policies/{id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert_eq!(
            policy_jobs(&mut admin, &id).await.len(),
            1,
            "{name}: its jobs stay in the history"
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/toolbox/scripts/{shared}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        for action in [
            "automation_policy.create",
            "automation_policy.update",
            "automation_policy.delete",
            "automation_policy.skip",
        ] {
            assert_eq!(
                audit_events(&app, action).await.len(),
                1,
                "{name}: {action}"
            );
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn enrollment_policies_run_on_new_devices_in_their_companies() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let script = create_script(&mut admin, "Onboard", "powershell", true).await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Onboarding", "script_id": script, "run_as": "system", "target": "all",
                "trigger": "enroll", "offline": "skip", "catch_up_hours": 2,
            }),
        )
        .await;
        let id = policy["id"].as_str().unwrap().to_owned();
        assert_eq!(policy["next_run_at"], Value::Null);
        let agent = app.enroll(&mut admin, "NEW-PC").await;
        let fired = policy_jobs(&mut admin, &id).await;
        assert_eq!(fired.len(), 1, "{name}");
        assert_eq!(fired[0]["target_label"], "NEW-PC");
        assert_eq!(
            fired[0]["counts"]["waiting"], 1,
            "{name}: a new device is waited for even if the policy skips offline ones"
        );
        let mut connection = app.state.agents.connect(&agent.device_id);
        pass(&app, now_ms()).await;
        assert_eq!(runs(&mut connection).len(), 1, "{name}");

        // Only new devices in its companies.
        let company = admin
            .post("/v1/companies", json!({ "name": "Acme" }))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let saved = admin
            .put(
                &format!("/v1/automation-policies/{id}"),
                json!({
                    "name": "Onboarding", "script_id": script, "run_as": "system",
                    "target": "companies", "company_ids": [company], "trigger": "enroll",
                    "offline": "skip",
                }),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        app.enroll(&mut admin, "OTHER-PC").await;
        assert_eq!(policy_jobs(&mut admin, &id).await.len(), 1, "{name}");
        app.finish().await;
    }
}

/// A webhook receiver on a loopback port.
struct Webhook {
    url: String,
    requests: mpsc::UnboundedReceiver<Value>,
}

impl Webhook {
    async fn start() -> Self {
        let (sender, requests) = mpsc::unbounded_channel();
        let router = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |_: HeaderMap, body: axum::body::Bytes| {
                let sender = sender.clone();
                async move {
                    sender
                        .send(serde_json::from_slice::<Value>(&body).unwrap())
                        .ok();
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
}

#[tokio::test]
async fn failing_jobs_raise_script_alerts_and_notify_their_policys_channels() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESK").await;
        let mut connection = app.state.agents.connect(&agent.device_id);
        let mut hook = Webhook::start().await;
        let channel = admin
            .post(
                "/v1/notification-channels",
                json!({ "name": "On-call", "kind": "webhook", "webhook_url": hook.url }),
            )
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let rule = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Script failures", "severity": "warning", "condition": "script_failed", "nonzero_exit_fails": true }),
            )
            .await;
        assert_eq!(rule.status, StatusCode::CREATED);
        let script = create_script(&mut admin, "Backup", "powershell", true).await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Backups", "script_id": script, "run_as": "system", "target": "all",
                "trigger": "interval", "interval_minutes": 60, "offline": "skip",
                "channel_ids": [channel],
            }),
        )
        .await;
        assert_eq!(policy["channel_ids"], json!([channel]));
        let started = admin
            .post(
                &format!(
                    "/v1/automation-policies/{}/run",
                    policy["id"].as_str().unwrap()
                ),
                json!({}),
            )
            .await;
        let job_id = started.body["id"].as_str().unwrap().to_owned();
        let now = now_ms();
        pass(&app, now).await;
        let run = runs(&mut connection).remove(0);
        agent
            .report(
                &format!("script-runs/{}/result", run.run_id),
                report("completed", Some(2)),
            )
            .await;
        let alerts = admin.get("/v1/alerts").await.body;
        assert_eq!(
            alerts["alerts"][0]["script_run_id"],
            run.run_id.as_str(),
            "{name}"
        );
        assert_eq!(
            alerts["alerts"][0]["detail"],
            "\u{201c}Backup\u{201d} exited with code 2"
        );
        pass(&app, now + SECOND_MS).await;
        let notified = hook.next().await;
        assert_eq!(notified["event"], "job.failed", "{name}");
        assert_eq!(
            notified["summary"],
            "\u{201c}Backups\u{201d}: 1 of 1 devices failed"
        );
        assert_eq!(notified["job"]["id"], job_id.as_str());
        app.finish().await;
    }
}

#[tokio::test]
async fn maintenance_purges_old_jobs() {
    for app in apps().await {
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESK").await;
        let script = create_script(&mut admin, "Old", "cmd", false).await;
        let started = start_job(
            &mut admin,
            json!({ "script_id": script, "run_as": "user", "target": "devices", "device_ids": [agent.device_id], "offline": "skip" }),
        )
        .await;
        let purged =
            meshrmm_server::maintenance::purge(app.db(), now_ms() + 31 * 24 * HOUR_MS).await;
        assert_eq!(purged.failed, Vec::<String>::new(), "{}", app.name);
        assert_eq!(purged.rows("jobs"), 1, "{}", app.name);
        assert_eq!(purged.rows("job_devices"), 1, "{}", app.name);
        let id = started["id"].as_str().unwrap();
        // Sessions expired too, so the database says.
        assert!(
            automation::jobs::get(&mut app.db(), id, now_ms())
                .await
                .unwrap()
                .is_none()
        );
        app.finish().await;
    }
}

async fn step(app: &App, now: i64) {
    automation::step(&app.state, now).await.unwrap();
}

/// Marks a device deleted without telling the engine, as if the server had
/// stopped in between.
async fn mark_deleted(app: &App, device_id: &str) {
    use meshrmm_server::db::tables::Agents;
    use sea_query::{Expr, ExprTrait, Query};
    app.db()
        .execute(
            &Query::update()
                .table(Agents::Table)
                .value(Agents::DeletionRequestedAt, now_ms())
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn agents_that_reconnect_together_start_without_full_passes() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut agents = Vec::new();
        for index in 0..10 {
            agents.push(app.enroll(&mut admin, &format!("DESK-{index}")).await);
        }
        let script = create_script(&mut admin, "Inventory", "cmd", false).await;
        let devices = agents
            .iter()
            .map(|agent| agent.device_id.clone())
            .collect::<Vec<_>>();
        let started = start_job(
            &mut admin,
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": devices, "offline": "wait", "wait_hours": 24,
            }),
        )
        .await;
        let id = started["id"].as_str().unwrap().to_owned();
        // A restart's first pass, with every device offline.
        let now = now_ms();
        pass(&app, now).await;
        let engine = &app.state.automation;
        let full = engine.full_passes();
        let recounted = engine.jobs_recounted();

        // The Agents reconnect within a quarter of a second, and each starts
        // its run as it connects.
        let mut connections = Vec::new();
        for (index, agent) in agents.iter().enumerate() {
            let mut connection = app.state.agents.connect(&agent.device_id);
            engine.device_connected(&agent.device_id);
            step(&app, now + 25 * index as i64 + 1).await;
            assert_eq!(runs(&mut connection).len(), 1, "{name}: device {index}");
            connections.push((agent, connection));
        }
        assert_eq!(engine.full_passes(), full, "{name}: no full pass");
        assert_eq!(
            engine.jobs_recounted(),
            recounted,
            "{name}: and no recount within a second of the last"
        );
        step(&app, now + SECOND_MS).await;
        assert_eq!(
            engine.jobs_recounted(),
            recounted + 1,
            "{name}: the job is recounted once for them all"
        );
        assert_eq!(job(&mut admin, &id).await["counts"]["running"], 10);

        // The job finishes on the first recount after its last device reports.
        let every = automation::jobs::DevicePage {
            limit: 100,
            ..Default::default()
        };
        let ran = automation::jobs::devices(&mut app.db(), &id, &every, now_ms())
            .await
            .unwrap();
        for device in &ran {
            let agent = agents
                .iter()
                .find(|agent| agent.device_id == device.device_id)
                .unwrap();
            let reported = agent
                .report(
                    &format!("script-runs/{}/result", device.run_id.as_ref().unwrap()),
                    report("completed", Some(0)),
                )
                .await;
            assert_eq!(reported.status, StatusCode::NO_CONTENT, "{name}");
        }
        step(&app, now + SECOND_MS + 500).await;
        assert!(
            job(&mut admin, &id).await["finished_at_unix_ms"].is_null(),
            "{name}: reports within a second wait for the next recount"
        );
        step(&app, now + 2 * SECOND_MS).await;
        let finished = job(&mut admin, &id).await;
        assert!(finished["finished_at_unix_ms"].is_i64(), "{name}");
        assert_eq!(finished["counts"]["succeeded"], 10);
        assert_eq!(engine.full_passes(), full, "{name}: still no full pass");
        drop(connections);
        app.finish().await;
    }
}

#[tokio::test]
async fn the_full_pass_catches_what_the_engine_was_not_told() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let quiet = app.enroll(&mut admin, "QUIET").await;
        let deleted = app.enroll(&mut admin, "DELETED").await;
        let lost = app.enroll(&mut admin, "LOST").await;
        let script = create_script(&mut admin, "Inventory", "cmd", false).await;
        let started = start_job(
            &mut admin,
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": [quiet.device_id, deleted.device_id, lost.device_id],
                "offline": "wait", "wait_hours": 24,
            }),
        )
        .await;
        let id = started["id"].as_str().unwrap().to_owned();
        let now = now_ms();
        pass(&app, now).await;
        let engine = &app.state.automation;
        let full = engine.full_passes();
        let outcome = |job: &Value, device: &common::Agent| {
            job["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|part| part["device_id"] == device.device_id.as_str())
                .unwrap()
                .clone()
        };

        // A device deleted while it waits misses the job at once.
        admin
            .delete(&format!("/v1/agents/{}", deleted.device_id))
            .await;
        step(&app, now + SECOND_MS).await;
        let after = job(&mut admin, &id).await;
        assert_eq!(outcome(&after, &deleted)["outcome"], "missed", "{name}");
        assert_eq!(
            outcome(&after, &deleted)["detail"],
            "The device was deleted"
        );
        assert_eq!(after["counts"]["missed"], 1, "{name}: and is counted");

        // An Agent whose connection the engine wasn't told of, and a device
        // deleted while the server was stopping, wait for the full pass.
        let mut connection = app.state.agents.connect(&quiet.device_id);
        mark_deleted(&app, &lost.device_id).await;
        step(&app, now + 10 * SECOND_MS).await;
        assert!(runs(&mut connection).is_empty(), "{name}");
        assert_eq!(engine.full_passes(), full, "{name}");
        step(&app, now + automation::RECONCILE_INTERVAL_MS).await;
        assert_eq!(engine.full_passes(), full + 1, "{name}");
        assert_eq!(
            runs(&mut connection).len(),
            1,
            "{name}: the quiet device runs"
        );
        let after = job(&mut admin, &id).await;
        assert_eq!(outcome(&after, &quiet)["outcome"], "running", "{name}");
        assert_eq!(outcome(&after, &lost)["outcome"], "missed", "{name}");
        assert_eq!(outcome(&after, &lost)["detail"], "The device was deleted");
        app.finish().await;
    }
}

/// Records a pending run of the dispatcher's `class` on each of
/// `device_ids`, as if other jobs' runs awaited a report there.
async fn hold(app: &App, class: automation::dispatch::Class, device_ids: &[String]) {
    use meshrmm_server::db::tables::{ScriptRuns, UpdateRuns};
    use sea_query::Query;
    let now = now_ms();
    for chunk in device_ids.chunks(100) {
        let mut insert = Query::insert();
        match class {
            automation::dispatch::Class::Quick => {
                insert.into_table(ScriptRuns::Table).columns([
                    ScriptRuns::Id,
                    ScriptRuns::DeviceId,
                    ScriptRuns::ScriptId,
                    ScriptRuns::ScriptName,
                    ScriptRuns::Language,
                    ScriptRuns::RequestedByUserId,
                    ScriptRuns::Source,
                    ScriptRuns::JobId,
                    ScriptRuns::RunAs,
                    ScriptRuns::TimeoutSeconds,
                    ScriptRuns::CreatedAt,
                ]);
                for device_id in chunk {
                    insert.values_panic([
                        uuid::Uuid::new_v4().to_string().into(),
                        device_id.as_str().into(),
                        "held".into(),
                        "Held".into(),
                        "cmd".into(),
                        "held".into(),
                        "job".into(),
                        "held".into(),
                        "system".into(),
                        3_600.into(),
                        now.into(),
                    ]);
                }
            }
            automation::dispatch::Class::Long => {
                insert.into_table(UpdateRuns::Table).columns([
                    UpdateRuns::Id,
                    UpdateRuns::DeviceId,
                    UpdateRuns::Kind,
                    UpdateRuns::RequestedByUserId,
                    UpdateRuns::JobId,
                    UpdateRuns::CreatedAt,
                ]);
                for device_id in chunk {
                    insert.values_panic([
                        uuid::Uuid::new_v4().to_string().into(),
                        device_id.as_str().into(),
                        "install".into(),
                        "held".into(),
                        "held".into(),
                        now.into(),
                    ]);
                }
            }
        }
        app.db().execute(&insert).await.unwrap();
    }
}

/// Ends the runs [`hold`] recorded.
async fn release(app: &App) {
    use meshrmm_server::db::tables::{ScriptRuns, UpdateRuns};
    use sea_query::{Expr, ExprTrait, Query};
    app.db()
        .execute(
            &Query::delete()
                .from_table(ScriptRuns::Table)
                .and_where(Expr::col(ScriptRuns::JobId).eq("held"))
                .to_owned(),
        )
        .await
        .unwrap();
    app.db()
        .execute(
            &Query::delete()
                .from_table(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::JobId).eq("held"))
                .to_owned(),
        )
        .await
        .unwrap();
}

/// `count` stand-in device IDs that no job covers.
fn others(prefix: &str, count: i64) -> Vec<String> {
    (0..count)
        .map(|index| format!("{prefix}-{index}"))
        .collect()
}

#[tokio::test]
async fn more_connected_agents_start_more_devices_a_pass() {
    use automation::dispatch::{MIN_BATCH, limits};
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut agents = Vec::new();
        for index in 0..32 {
            agents.push(app.enroll(&mut admin, &format!("DESK-{index}")).await);
        }
        let mut connections = agents
            .iter()
            .map(|agent| app.state.agents.connect(&agent.device_id))
            .collect::<Vec<_>>();
        // The rest of a fleet of about 3,000, with nothing to run.
        let _fleet = others("idle", 3_000)
            .iter()
            .map(|id| app.state.agents.connect(id))
            .collect::<Vec<_>>();
        let batch = limits(app.state.agents.count()).batch;
        assert_eq!(batch, 30, "{name}");
        assert!(batch > MIN_BATCH);
        let script = create_script(&mut admin, "Inventory", "cmd", false).await;
        let devices = agents
            .iter()
            .map(|agent| agent.device_id.clone())
            .collect::<Vec<_>>();
        start_job(
            &mut admin,
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": devices, "offline": "skip",
            }),
        )
        .await;
        let sent = |connections: &mut Vec<AgentConnection>| {
            connections
                .iter_mut()
                .map(|connection| runs(connection).len())
                .sum::<usize>()
        };
        let now = now_ms();
        assert!(
            pass(&app, now).await,
            "{name}: it stopped at the batch with more to start"
        );
        assert_eq!(sent(&mut connections), 30, "{name}");
        assert!(
            !pass(&app, now + SECOND_MS).await,
            "{name}: nothing is left to start"
        );
        assert_eq!(sent(&mut connections), 2, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn long_jobs_leave_room_for_quick_ones_and_wakes_start_however_full() {
    use automation::dispatch::{Class, MIN_IN_FLIGHT};
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "DESK").await;
        let patching = app.enroll(&mut admin, "PATCHING").await;
        let asleep = app.enroll(&mut admin, "ASLEEP").await;
        meshrmm_server::updates::store::store_scan(
            app.db(),
            &patching.device_id,
            serde_json::from_value(json!({
                "scanned_at_unix_ms": 1, "manager": "windows_update",
                "updates": [], "reboot_pending": false,
            }))
            .unwrap(),
            now_ms(),
        )
        .await
        .unwrap();
        let mut desk_connection = app.state.agents.connect(&desk.device_id);
        let mut patching_connection = app.state.agents.connect(&patching.device_id);
        let script = create_script(&mut admin, "Inventory", "powershell", false).await;
        let script_job = json!({
            "script_id": script, "run_as": "system", "target": "devices",
            "device_ids": [desk.device_id], "offline": "skip",
        });

        // A fleet-wide install holds every slot of its class, and another
        // install waits behind it.
        hold(&app, Class::Long, &others("installing", MIN_IN_FLIGHT)).await;
        let install = start_job(
            &mut admin,
            json!({
                "updates": { "action": "install", "categories": ["security"], "reboot": "never" },
                "target": "devices", "device_ids": [patching.device_id], "offline": "skip",
            }),
        )
        .await;
        let first = start_job(&mut admin, script_job.clone()).await;
        let now = now_ms();
        assert!(
            !pass(&app, now).await,
            "{name}: the install waits for a report, not another pass"
        );
        assert!(started(&mut patching_connection).is_empty(), "{name}");
        assert_eq!(
            job(&mut admin, install["id"].as_str().unwrap()).await["devices"][0]["outcome"],
            "queued"
        );
        assert_eq!(
            runs(&mut desk_connection).len(),
            1,
            "{name}: the script starts at once"
        );
        assert_eq!(
            job(&mut admin, first["id"].as_str().unwrap()).await["devices"][0]["outcome"],
            "running"
        );

        // With both classes full, a wake job's device is still woken.
        hold(&app, Class::Quick, &others("running", MIN_IN_FLIGHT)).await;
        let second = start_job(&mut admin, script_job).await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Wake", "action": "wake", "target": "devices",
                "device_ids": [asleep.device_id], "trigger": "daily", "time": "01:50",
                "time_zone": "UTC", "offline": "skip",
            }),
        )
        .await;
        let woken = admin
            .post(
                &format!(
                    "/v1/automation-policies/{}/run",
                    policy["id"].as_str().unwrap()
                ),
                json!({}),
            )
            .await;
        assert_eq!(woken.status, StatusCode::CREATED, "{:?}", woken.body);
        let woken_id = woken.body["id"].as_str().unwrap();
        assert_eq!(
            job(&mut admin, woken_id).await["devices"][0]["outcome"],
            "queued",
            "{name}"
        );
        assert!(!pass(&app, now + SECOND_MS).await, "{name}");
        let woken = job(&mut admin, woken_id).await;
        assert_ne!(
            woken["devices"][0]["outcome"], "queued",
            "{name}: the wake started: {woken}"
        );
        assert!(woken["devices"][0]["wake_id"].is_string(), "{name}");
        assert!(runs(&mut desk_connection).is_empty(), "{name}");
        assert_eq!(
            job(&mut admin, second["id"].as_str().unwrap()).await["devices"][0]["outcome"],
            "queued"
        );

        // Once the runs report, both start.
        release(&app).await;
        assert!(!pass(&app, now + 2 * SECOND_MS).await, "{name}");
        assert_eq!(runs(&mut desk_connection).len(), 1, "{name}");
        assert_eq!(started(&mut patching_connection).len(), 1, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn a_small_job_is_not_left_behind_a_large_one() {
    use automation::dispatch::{Class, MIN_IN_FLIGHT};
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut fleet = Vec::new();
        for index in 0..6 {
            fleet.push(app.enroll(&mut admin, &format!("FLEET-{index}")).await);
        }
        let desk = app.enroll(&mut admin, "DESK").await;
        let mut fleet_connections = fleet
            .iter()
            .map(|agent| app.state.agents.connect(&agent.device_id))
            .collect::<Vec<_>>();
        let mut desk_connection = app.state.agents.connect(&desk.device_id);
        let script = create_script(&mut admin, "Inventory", "cmd", false).await;
        let script_job = |devices: Vec<String>| {
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": devices, "offline": "skip",
            })
        };
        // One slot is free at a time, as when runs report one by one.
        hold(&app, Class::Quick, &others("running", MIN_IN_FLIGHT - 1)).await;
        let large = start_job(
            &mut admin,
            script_job(fleet.iter().map(|agent| agent.device_id.clone()).collect()),
        )
        .await;
        let small = start_job(&mut admin, script_job(vec![desk.device_id.clone()])).await;

        assert!(
            !pass(&app, now_ms()).await,
            "{name}: the class is full, so no hurry"
        );
        assert_eq!(
            runs(&mut desk_connection).len(),
            1,
            "{name}: the small job takes the free slot"
        );
        assert!(
            fleet_connections
                .iter_mut()
                .all(|connection| runs(connection).is_empty())
        );
        assert_eq!(
            job(&mut admin, small["id"].as_str().unwrap()).await["devices"][0]["outcome"],
            "running"
        );
        let large_id = large["id"].as_str().unwrap();
        assert_eq!(job(&mut admin, large_id).await["counts"]["queued"], 6);

        // The large one carries on once there's room.
        release(&app).await;
        pass(&app, now_ms()).await;
        assert!(
            fleet_connections
                .iter_mut()
                .all(|connection| runs(connection).len() == 1),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn policies_skip_a_run_while_their_last_runs_devices_are_queued() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESK").await;
        let mut connection = app.state.agents.connect(&agent.device_id);
        // The device is at its limit, so its part stays queued.
        hold(
            &app,
            automation::dispatch::Class::Quick,
            &vec![agent.device_id.clone(); automation::dispatch::MAX_PER_DEVICE as usize],
        )
        .await;
        let script = create_script(&mut admin, "Health check", "powershell", true).await;
        let policy = create_policy(
            &mut admin,
            json!({
                "name": "Every five minutes", "script_id": script, "run_as": "system",
                "target": "all", "trigger": "interval", "interval_minutes": 5,
                "offline": "wait", "catch_up_hours": 24,
            }),
        )
        .await;
        let id = policy["id"].as_str().unwrap().to_owned();
        let next = policy["next_run_at"].as_i64().unwrap();
        let queued = |jobs: &[Value]| {
            jobs.iter()
                .map(|job| job["counts"]["queued"].as_i64().unwrap())
                .sum::<i64>()
        };

        pass(&app, next).await;
        let fired = policy_jobs(&mut admin, &id).await;
        assert_eq!(fired.len(), 1, "{name}");
        assert_eq!(queued(&fired), 1, "{name}");
        for run in 1..=3 {
            pass(&app, next + run * 5 * MINUTE_MS).await;
            let fired = policy_jobs(&mut admin, &id).await;
            assert_eq!(fired.len(), 1, "{name}: run {run} is skipped");
            assert_eq!(queued(&fired), 1, "{name}: and queues nothing more");
        }
        let listed = admin.get("/v1/automation-policies").await.body;
        let problem = listed["policies"][0]["last_problem"].as_str().unwrap();
        assert!(
            problem.starts_with("Skipped its run at ")
                && problem.ends_with(" because 1 device from its last run hadn't started yet"),
            "{name}: {problem}"
        );
        assert_eq!(
            listed["policies"][0]["next_run_at"],
            next + 20 * MINUTE_MS,
            "{name}: it keeps time"
        );
        assert_eq!(
            audit_events(&app, "automation_policy.skip").await.len(),
            3,
            "{name}"
        );
        assert!(runs(&mut connection).is_empty(), "{name}");

        // Once the device starts, the next run happens.
        release(&app).await;
        pass(&app, next + 16 * MINUTE_MS).await;
        assert_eq!(runs(&mut connection).len(), 1, "{name}");
        pass(&app, next + 20 * MINUTE_MS).await;
        assert_eq!(policy_jobs(&mut admin, &id).await.len(), 2, "{name}");
        let listed = admin.get("/v1/automation-policies").await.body;
        assert_eq!(listed["policies"][0]["last_problem"], Value::Null, "{name}");
        app.finish().await;
    }
}

/// Enrolls `count` Windows devices called `prefix-N` straight into the
/// database, for jobs larger than enrolling each one allows. Returns their IDs.
async fn fleet(app: &App, prefix: &str, count: usize) -> Vec<String> {
    use meshrmm_server::db::tables::Agents;
    use sea_query::Query;
    let ids = (0..count)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    for (chunk_index, chunk) in ids.chunks(400).enumerate() {
        let mut insert = Query::insert();
        insert.into_table(Agents::Table).columns([
            Agents::Id,
            Agents::Name,
            Agents::Platform,
            Agents::AuthTokenHash,
            Agents::CreatedAt,
            Agents::UpdatedAt,
        ]);
        for (index, id) in chunk.iter().enumerate() {
            insert.values_panic([
                id.as_str().into(),
                format!("{prefix}-{}", chunk_index * 400 + index).into(),
                "windows".into(),
                "0".repeat(64).into(),
                0.into(),
                0.into(),
            ]);
        }
        app.db().execute(&insert).await.unwrap();
    }
    ids
}

/// An update scan on `target` that waits for offline devices.
fn update_scan(target: automation::jobs::Target) -> automation::jobs::NewJob {
    automation::jobs::NewJob {
        payload: automation::jobs::Payload::UpdateScan,
        target,
        offline: automation::jobs::Offline::Wait,
        wait_ms: HOUR_MS,
        requested_by_user_id: "tester".to_owned(),
        policy: None,
        actor: meshrmm_server::audit::Actor {
            user_id: None,
            label: "tester".to_owned(),
            ip: None,
            api_token: None,
        },
    }
}

/// Writes a planned job, as the website does.
async fn write_job(
    app: &App,
    plan: &automation::jobs::Plan,
) -> Result<automation::jobs::Created, meshrmm_server::http::ApiError> {
    automation::jobs::create(&app.state, plan, now_ms()).await
}

/// How many jobs there are, finished or not.
async fn job_rows(app: &App) -> i64 {
    use meshrmm_server::db::tables::Jobs;
    use sea_query::{Expr, Query};
    let (count,): (i64,) = app
        .db()
        .fetch_one(
            &Query::select()
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(Jobs::Table)
                .to_owned(),
        )
        .await
        .unwrap();
    count
}

/// Plans and writes an update scan on `target`, as the website starts one.
async fn create_job(app: &App, target: automation::jobs::Target) -> String {
    let plan = automation::jobs::plan(&mut app.db(), &app.state.agents, update_scan(target))
        .await
        .unwrap();
    write_job(app, &plan).await.unwrap().id
}

#[tokio::test]
async fn counts_and_the_next_batch_cost_the_same_however_large_the_job() {
    use automation::jobs::{self, Target};
    for app in apps().await {
        let name = app.name;
        let mut costs = Vec::new();
        let mut connections = Vec::new();
        for size in [20, 2_000] {
            let devices = fleet(&app, &format!("FLEET{size}"), size).await;
            connections.extend(devices.iter().map(|id| app.state.agents.connect(id)));
            let id = create_job(&app, Target::Devices(devices)).await;
            let before = app.db().counts();
            let counts = jobs::counts(&mut app.db(), std::slice::from_ref(&id), now_ms())
                .await
                .unwrap();
            let counting = app.db().counts() - before;
            assert_eq!(counts[&id].queued, size as i64, "{name}");
            let before = app.db().counts();
            let batch = automation::dispatch::next_batch(&app.state, now_ms())
                .await
                .unwrap();
            let batching = app.db().counts() - before;
            assert_eq!(
                batch.len() as u64,
                (size as u64).min(automation::dispatch::MIN_BATCH * 4),
                "{name}"
            );
            costs.push((counting.statements, batching.statements));
            // The next size is measured alone.
            jobs::cancel(&mut app.db(), &id, now_ms()).await.unwrap();
        }
        assert_eq!(costs[0], costs[1], "{name}: {costs:?}");
        assert_eq!(costs[0].0, 1, "{name}: one grouped read counts a job");
        app.finish().await;
    }
}

#[tokio::test]
async fn a_page_of_a_jobs_devices_costs_the_same_however_large_the_job() {
    use automation::jobs::Target;
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut costs = Vec::new();
        let mut connections = Vec::new();
        for size in [20, 2_000] {
            let devices = fleet(&app, &format!("PAGE{size}"), size).await;
            connections.extend(devices.iter().map(|id| app.state.agents.connect(id)));
            let id = create_job(&app, Target::Devices(devices)).await;
            let path = |query: &str| format!("/v1/jobs/{id}/devices?limit=10{query}");
            // Once first, so what the request's signing in reads is known.
            assert_eq!(admin.get(&path("")).await.status, StatusCode::OK);
            let mut cost = Vec::new();
            for query in ["", "&outcome=queued", "&outcome=failed", "&offset=10"] {
                let before = app.db().counts();
                let page = admin.get(&path(query)).await;
                cost.push((app.db().counts() - before).statements);
                assert_eq!(page.status, StatusCode::OK, "{name}: {:?}", page.body);
                if query.is_empty() {
                    assert_eq!(page.body["devices"].as_array().unwrap().len(), 10);
                    assert_eq!(page.body["matching"], size, "{name}");
                }
            }
            let before = app.db().counts();
            let job = admin.get(&format!("/v1/jobs/{id}")).await;
            cost.push((app.db().counts() - before).statements);
            assert!(job.body.get("devices").is_none(), "{name}");
            costs.push(cost);
        }
        assert_eq!(costs[0], costs[1], "{name}: {costs:?}");
        drop(connections);
        app.finish().await;
    }
}

#[tokio::test]
async fn a_large_job_s_queued_devices_do_not_starve_a_small_one() {
    use automation::jobs::Target;
    for app in apps().await {
        let name = app.name;
        let page = automation::dispatch::MIN_BATCH * 4;
        // Far more queued than a pass reads, all before the small job's.
        let mut large = fleet(&app, "LARGE", 3 * page as usize).await;
        let small = fleet(&app, "SMALL", 1).await;
        let _connections = large
            .iter()
            .chain(&small)
            .map(|id| app.state.agents.connect(id))
            .collect::<Vec<_>>();
        let large_job = create_job(&app, Target::Devices(large.clone())).await;
        let small_job = create_job(&app, Target::Devices(small.clone())).await;
        let batch = automation::dispatch::next_batch(&app.state, now_ms())
            .await
            .unwrap();
        assert_eq!(batch.len() as u64, page, "{name}");
        assert_eq!(
            (batch[0].job_id.as_str(), batch[0].device_id.as_str()),
            (small_job.as_str(), small[0].as_str()),
            "{name}: the job with fewer devices left goes first in the round"
        );
        large.sort();
        assert!(
            batch[1..]
                .iter()
                .zip(&large)
                .all(|(row, device_id)| row.job_id == large_job && &row.device_id == device_id),
            "{name}: then the large job's devices, by ID"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn jobs_are_planned_without_the_writer_and_written_with_inserts_alone() {
    use automation::jobs::{self, Target};
    use meshrmm_server::db::Backend;
    for app in apps().await {
        let name = app.name;
        let devices = fleet(&app, "FLEET", 1_000).await;

        // Planning only reads, so it goes on while another transaction holds
        // SQLite's one writer; writing the job waits for that one to end.
        let before = app.db().counts();
        let held = app.db().begin().await.unwrap();
        let plan = jobs::plan(&mut app.db(), &app.state.agents, update_scan(Target::All))
            .await
            .unwrap();
        let planning = app.db().counts() - before;
        assert_eq!(plan.devices(), 1_000, "{name}");
        assert_eq!(
            (planning.statements, planning.writes, planning.transactions),
            (1, 0, 1),
            "{name}: one read, and only the held transaction began"
        );
        let state = app.state.clone();
        let writing = tokio::spawn(async move {
            let before = state.database.counts();
            let created = jobs::create(&state, &plan, now_ms()).await;
            (created, state.database.counts() - before)
        });
        if app.db().backend() == Backend::Sqlite {
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!writing.is_finished(), "{name}: the write waits its turn");
        }
        held.rollback().await.unwrap();
        let (created, written) = writing.await.unwrap();
        let created = created.unwrap();
        assert_eq!(created.devices, 1_000, "{name}");
        // One transaction: the job, a statement for each 400 devices, the
        // clearing of its mark and the audit event. Nothing is read, and only
        // the devices' inserts grow with them.
        assert_eq!(
            (written.statements, written.writes, written.transactions),
            (1 + 3 + 2, 1 + 3 + 2, 1),
            "{name}"
        );
        let job = jobs::get(&mut app.db(), &created.id, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.counts.waiting, 1_000, "{name}");
        assert_eq!(job.target_label, "All devices");

        // Chosen devices are read 500 at a time; still nothing is written.
        let before = app.db().counts();
        jobs::plan(
            &mut app.db(),
            &app.state.agents,
            update_scan(Target::Devices(devices)),
        )
        .await
        .unwrap();
        let planning = app.db().counts() - before;
        assert_eq!((planning.statements, planning.writes), (2, 0), "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_deleted_after_planning_are_left_out_of_the_job() {
    use automation::jobs::{self, Target};
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "DESK").await;
        let laptop = app.enroll(&mut admin, "LAPTOP").await;
        let spare = app.enroll(&mut admin, "SPARE").await;
        let connection = app.state.agents.connect(&desk.device_id);
        let chosen = |agents: &[&common::Agent]| {
            update_scan(Target::Devices(
                agents.iter().map(|agent| agent.device_id.clone()).collect(),
            ))
        };

        // Three planned, one deleted before the write: the job is for two.
        let plan = jobs::plan(
            &mut app.db(),
            &app.state.agents,
            chosen(&[&desk, &laptop, &spare]),
        )
        .await
        .unwrap();
        assert_eq!(plan.devices(), 3, "{name}");
        mark_deleted(&app, &spare.device_id).await;
        let created = write_job(&app, &plan).await.unwrap();
        assert_eq!(created.devices, 2, "{name}");
        let written = jobs::get(&mut app.db(), &created.id, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                written.counts.total,
                written.counts.queued,
                written.counts.waiting
            ),
            (2, 1, 1),
            "{name}"
        );
        assert_eq!(written.target_label, "2 devices", "{name}");
        let started = audit_events(&app, "job.start").await;
        assert_eq!(started[0].metadata["devices"], 2, "{name}");
        assert_eq!(started[0].metadata["queued"], 1, "{name}");
        assert_eq!(started[0].metadata["waiting"], 1, "{name}");
        assert_eq!(started[0].metadata["target_label"], "2 devices");
        pass(&app, now_ms()).await;
        let shown = job(&mut admin, &created.id).await;
        assert_eq!(
            outcomes(&shown),
            [
                ("DESK".to_owned(), "running".to_owned()),
                ("LAPTOP".to_owned(), "waiting".to_owned()),
            ],
            "{name}: the job runs on the devices left"
        );

        // One left: the job is named after it.
        let plan = jobs::plan(&mut app.db(), &app.state.agents, chosen(&[&desk, &laptop]))
            .await
            .unwrap();
        mark_deleted(&app, &laptop.device_id).await;
        let created = write_job(&app, &plan).await.unwrap();
        assert_eq!(created.devices, 1, "{name}");
        let written = jobs::get(&mut app.db(), &created.id, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(written.target_label, "DESK", "{name}");

        // None left: no devices, and nothing is written.
        let plan = jobs::plan(&mut app.db(), &app.state.agents, chosen(&[&desk]))
            .await
            .unwrap();
        mark_deleted(&app, &desk.device_id).await;
        let jobs_before = job_rows(&app).await;
        let error = write_job(&app, &plan).await.unwrap_err();
        assert_eq!(error.code(), Some("no_devices"), "{name}");
        assert_eq!(job_rows(&app).await, jobs_before, "{name}");
        assert_eq!(audit_events(&app, "job.start").await.len(), 2, "{name}");
        // Planned now, it fails the same way, before anything is written.
        let error = jobs::plan(&mut app.db(), &app.state.agents, chosen(&[&desk]))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some("no_devices"), "{name}");
        drop(connection);
        app.finish().await;
    }
}

/// The policy `id` as the policies page lists it.
async fn listed_policy(admin: &mut Browser, id: &str) -> Value {
    admin.get("/v1/automation-policies").await.body["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|policy| policy["id"] == id)
        .unwrap()
        .clone()
}

#[tokio::test]
async fn a_policy_changed_after_planning_its_firing_does_not_start_its_old_job() {
    use automation::policies::{Fired, plan_firing, write_firing};
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        app.enroll(&mut admin, "DESK").await;
        let script = create_script(&mut admin, "Health check", "powershell", true).await;
        let body = |policy: &str, run_as: &str, enabled: bool| {
            json!({
                "name": policy, "enabled": enabled, "script_id": script, "run_as": run_as,
                "target": "all", "trigger": "interval", "interval_minutes": 5,
                "offline": "wait", "catch_up_hours": 24,
            })
        };
        let mut created = Vec::new();
        for policy in ["Edited", "Disabled", "Deleted", "Raced"] {
            let policy = create_policy(&mut admin, body(policy, "system", true)).await;
            created.push((
                policy["id"].as_str().unwrap().to_owned(),
                policy["next_run_at"].as_i64().unwrap(),
            ));
        }
        let [edited, disabled, deleted, raced] = created.try_into().unwrap();

        // Saved in between: nothing is written, and the firing is planned
        // again from what was saved.
        let (id, due) = &edited;
        let firing = plan_firing(&app.state, id, *due, *due)
            .await
            .unwrap()
            .unwrap();
        let saved = admin
            .put(
                &format!("/v1/automation-policies/{id}"),
                body("Edited", "user", true),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        // Due at the same time still, so only the save tells them apart.
        set_next_run(&app, id, *due).await;
        assert_eq!(
            write_firing(&app.state, &firing).await.unwrap(),
            Fired::Changed,
            "{name}"
        );
        assert!(policy_jobs(&mut admin, id).await.is_empty(), "{name}");
        let firing = plan_firing(&app.state, id, *due, *due)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            write_firing(&app.state, &firing).await.unwrap(),
            Fired::Claimed { started: true },
            "{name}"
        );
        let fired = policy_jobs(&mut admin, id).await;
        assert_eq!(fired.len(), 1, "{name}");
        assert_eq!(fired[0]["run_as"], "user", "{name}: as saved");

        // Turned off or deleted in between: not due, and nothing starts.
        let (id, due) = &disabled;
        let firing = plan_firing(&app.state, id, *due, *due)
            .await
            .unwrap()
            .unwrap();
        let saved = admin
            .put(
                &format!("/v1/automation-policies/{id}"),
                body("Disabled", "system", false),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(
            write_firing(&app.state, &firing).await.unwrap(),
            Fired::NotDue,
            "{name}"
        );
        assert!(policy_jobs(&mut admin, id).await.is_empty(), "{name}");
        let (id, due) = &deleted;
        let firing = plan_firing(&app.state, id, *due, *due)
            .await
            .unwrap()
            .unwrap();
        let removed = admin.delete(&format!("/v1/automation-policies/{id}")).await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(
            write_firing(&app.state, &firing).await.unwrap(),
            Fired::NotDue,
            "{name}"
        );
        assert!(policy_jobs(&mut admin, id).await.is_empty(), "{name}");

        // Two passes plan the same firing: only the first claims it.
        let (id, due) = &raced;
        let first = plan_firing(&app.state, id, *due, *due)
            .await
            .unwrap()
            .unwrap();
        let second = plan_firing(&app.state, id, *due, *due + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            write_firing(&app.state, &first).await.unwrap(),
            Fired::Claimed { started: true },
            "{name}"
        );
        assert_eq!(
            write_firing(&app.state, &second).await.unwrap(),
            Fired::NotDue,
            "{name}"
        );
        assert!(
            plan_firing(&app.state, id, *due, *due + 2)
                .await
                .unwrap()
                .is_none(),
            "{name}: claimed"
        );
        assert_eq!(policy_jobs(&mut admin, id).await.len(), 1, "{name}");
        assert_eq!(
            listed_policy(&mut admin, id).await["next_run_at"],
            *due + 5 * MINUTE_MS,
            "{name}"
        );
        app.finish().await;
    }
}

/// Inserts `count` unfinished update scan jobs straight into the database,
/// each waiting for `device_id`, for more jobs than creating each one
/// allows. Returns their IDs.
async fn waiting_jobs(app: &App, device_id: &str, count: usize) -> Vec<String> {
    use meshrmm_server::db::tables::{JobDevices, Jobs};
    use sea_query::Query;
    let now = now_ms();
    let ids = (0..count)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    for chunk in ids.chunks(400) {
        let mut jobs = Query::insert();
        jobs.into_table(Jobs::Table).columns([
            Jobs::Id,
            Jobs::Kind,
            Jobs::ItemId,
            Jobs::ItemName,
            Jobs::Target,
            Jobs::TargetLabel,
            Jobs::Offline,
            Jobs::ExpiresAt,
            Jobs::RequestedByUserId,
            Jobs::CreatedAt,
        ]);
        let mut devices = Query::insert();
        devices.into_table(JobDevices::Table).columns([
            JobDevices::JobId,
            JobDevices::DeviceId,
            JobDevices::State,
            JobDevices::UpdatedAt,
        ]);
        for id in chunk {
            jobs.values_panic([
                id.as_str().into(),
                "update_scan".into(),
                "".into(),
                "Scan for updates".into(),
                "devices".into(),
                "1 device".into(),
                "wait".into(),
                (now + HOUR_MS).into(),
                "tester".into(),
                now.into(),
            ]);
            devices.values_panic([
                id.as_str().into(),
                device_id.into(),
                "waiting".into(),
                now.into(),
            ]);
        }
        app.db().execute(&jobs).await.unwrap();
        app.db().execute(&devices).await.unwrap();
    }
    ids
}

/// The statements reading jobs that don't exist by these IDs takes.
async fn statements_to_read(app: &App, ids: &[String]) -> u64 {
    let before = app.db().counts();
    automation::jobs::by_ids(&mut app.db(), ids, now_ms())
        .await
        .unwrap();
    (app.db().counts() - before).statements
}

#[tokio::test]
async fn any_number_of_jobs_is_read_a_chunk_of_ids_at_a_time() {
    use automation::{dispatch::QUEUE_CHUNK, jobs};
    // Both databases bind a few tens of thousands of values to a statement
    // at most; a chunk stays well under either.
    const { assert!(QUEUE_CHUNK < 32_766) };
    for app in apps().await {
        let name = app.name;
        // More IDs than either database binds in one statement. Looking jobs
        // up needs only their IDs, not the jobs.
        let ids = (0..70_000).map(|n| format!("job-{n}")).collect::<Vec<_>>();
        assert!(
            jobs::by_ids(&mut app.db(), &ids, now_ms())
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert_eq!(
            statements_to_read(&app, &ids[..QUEUE_CHUNK]).await,
            1,
            "{name}: a chunk is one statement"
        );
        assert_eq!(
            statements_to_read(&app, &ids[..QUEUE_CHUNK + 1]).await,
            2,
            "{name}: one more is another"
        );

        // A pass recounts every unfinished job, more than a chunk of them,
        // and still starts the queued device.
        let away = fleet(&app, "AWAY", 1).await;
        waiting_jobs(&app, &away[0], QUEUE_CHUNK + 1).await;
        let desk = fleet(&app, "DESK", 1).await;
        let mut connection = app.state.agents.connect(&desk[0]);
        let scan = create_job(&app, automation::jobs::Target::Devices(desk)).await;
        let recounted = app.state.automation.jobs_recounted();
        pass(&app, now_ms()).await;
        assert_eq!(
            app.state.automation.jobs_recounted() - recounted,
            QUEUE_CHUNK as u64 + 2,
            "{name}"
        );
        let mut scans = 0;
        while let Ok(message) = connection.commands.try_recv() {
            if matches!(
                message,
                meshrmm_server::realtime::ToAgent::Command(AgentCommand::ScanUpdates { .. })
            ) {
                scans += 1;
            }
        }
        assert_eq!(scans, 1, "{name}");
        let started = jobs::get(&mut app.db(), &scan, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(started.counts.running, 1, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn a_connected_device_with_a_full_backlog_waits_its_turn_rather_than_being_missed() {
    use meshrmm_server::db::tables::ScriptRuns;
    use sea_query::{Expr, ExprTrait, Query};
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let busy = app.enroll(&mut admin, "BUSY").await;
        let away = app.enroll(&mut admin, "AWAY").await;
        let mut busy_connection = app.state.agents.connect(&busy.device_id);
        let away_connection = app.state.agents.connect(&away.device_id);
        let script = create_script(&mut admin, "Inventory", "powershell", false).await;
        let started = start_job(
            &mut admin,
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": [busy.device_id, away.device_id], "offline": "skip",
            }),
        )
        .await;
        let id = started["id"].as_str().unwrap();
        // BUSY's Agent stops reading for a while; AWAY goes offline.
        while app
            .state
            .agents
            .send(&busy.device_id, AgentCommand::CollectSystemInfo)
        {}
        drop(away_connection);
        let now = now_ms();
        pass(&app, now).await;
        let outcome = |job: &Value, device: &str| {
            outcomes(job)
                .into_iter()
                .find(|(name, _)| name == device)
                .unwrap()
                .1
        };
        let waiting = job(&mut admin, id).await;
        assert_eq!(outcome(&waiting, "BUSY"), "queued", "{name}: {waiting}");
        assert_eq!(outcome(&waiting, "AWAY"), "missed", "{name}");
        let busy_runs = || {
            Query::select()
                .column(ScriptRuns::Id)
                .from(ScriptRuns::Table)
                .and_where(Expr::col(ScriptRuns::DeviceId).eq(busy.device_id.as_str()))
                .to_owned()
        };
        let runs_of_busy: Vec<(String,)> = app.db().fetch_all(&busy_runs()).await.unwrap();
        assert!(
            runs_of_busy.is_empty(),
            "{name}: nothing was written for it"
        );

        // Its Agent catches up, and the next pass starts it.
        assert_eq!(runs(&mut busy_connection).len(), 0);
        pass(&app, now + SECOND_MS).await;
        assert_eq!(runs(&mut busy_connection).len(), 1, "{name}");
        let running = job(&mut admin, id).await;
        assert_eq!(outcome(&running, "BUSY"), "running", "{name}");
        assert_eq!(outcome(&running, "AWAY"), "missed", "{name}");
        app.finish().await;
    }
}
