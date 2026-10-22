//! Jobs that run a script or send a file on many devices, and automation
//! policies that start them on a schedule or when devices enroll.
mod common;

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use common::{App, Browser, apps, audit_events, receive_any_json, set_up, user_with};
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

async fn start_job(browser: &mut Browser, body: Value) -> Value {
    let started = browser.post("/v1/jobs", body).await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
    started.body
}

async fn job(browser: &mut Browser, id: &str) -> Value {
    let job = browser.get(&format!("/v1/jobs/{id}")).await;
    assert_eq!(job.status, StatusCode::OK, "{:?}", job.body);
    job.body
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
        assert_eq!(cancelled.body["devices"][0]["outcome"], "cancelled");
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

/// Moves a job's runs into the past.
async fn backdate_runs(app: &App, job_id: &str, by_ms: i64) {
    use meshrmm_server::db::tables::ScriptRuns;
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
        let purged = meshrmm_server::maintenance::purge(app.db(), now_ms() + 31 * 24 * HOUR_MS)
            .await
            .unwrap();
        assert_eq!(purged.jobs, 1, "{}", app.name);
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
