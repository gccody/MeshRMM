//! Each job device's outcome is stored with it, written in the transaction
//! of every change of its run's status, where it used to be worked out on
//! every read by joining the device to its run, delivery, update run,
//! application run or wake request. These tests keep that old reading here
//! and check that the stored outcomes, and the counts, are what it says for
//! the same rows and clock: for a job of each kind, through every change a
//! device's part goes through, on both databases.
mod common;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use common::{App, Browser, apps, set_up};
use meshrmm_protocol_types::{AgentCommand, SystemInfo, WakePacket, WakePacketResult};
use meshrmm_server::{
    applications, automation,
    automation::jobs::{self, Counts, Outcome},
    db::tables::{JobDevices, ScriptRuns},
    maintenance,
    realtime::{AgentConnection, ToAgent},
    time::{DAY_MS, HOUR_MS, MINUTE_MS, SECOND_MS, now_ms},
    toolbox::{DELIVERY_WINDOW_MS, RUN_REPORT_GRACE_MS},
    updates, wake,
};
use sea_query::{Alias, Expr, ExprTrait, Query, SelectStatement};
use serde_json::{Value, json};

/// The outcome as it was worked out before it was stored: from the
/// device's state and its run's status, whether that succeeded, and
/// whether a pending one was overdue.
fn old_outcome(state: &str, status: Option<&str>, succeeded: bool, overdue: bool) -> Outcome {
    match state {
        "queued" => Outcome::Queued,
        "waiting" => Outcome::Waiting,
        "skipped" => Outcome::Skipped,
        "missed" => Outcome::Missed,
        "cancelled" => Outcome::Cancelled,
        "started" => match status {
            Some("pending") if overdue => Outcome::NoResult,
            Some("pending") => Outcome::Running,
            Some("completed" | "delivered" | "succeeded") if succeeded => Outcome::Succeeded,
            Some("completed" | "failed" | "partial") => Outcome::Failed,
            Some("skipped") => Outcome::Skipped,
            Some("timed_out") => Outcome::TimedOut,
            _ => Outcome::NoResult,
        },
        other => panic!("no state {other}"),
    }
}

/// The old read of a job's devices: each one joined to the five tables its
/// run may be in, with the run's status, whether it succeeded and whether
/// it was overdue at `now`.
fn old_select(job_id: &str, now: i64) -> SelectStatement {
    let run_deadline = format!(
        "script_runs.created_at + script_runs.timeout_seconds * {SECOND_MS} + {RUN_REPORT_GRACE_MS}"
    );
    let delivery_deadline = format!("file_deliveries.created_at + {DELIVERY_WINDOW_MS}");
    let wake_deadline = format!("wake_requests.created_at + {}", wake::ONLINE_WINDOW_MS);
    let update_deadline = format!(
        "update_runs.created_at + CASE update_runs.kind WHEN 'scan' THEN {} ELSE {} END",
        updates::runs::SCAN_REPORT_WINDOW_MS,
        updates::runs::INSTALL_REPORT_WINDOW_MS
    );
    let application_deadline = format!(
        "application_runs.created_at + CASE application_runs.action WHEN 'scan' THEN {} ELSE {} END",
        applications::runs::SCAN_REPORT_WINDOW_MS,
        applications::runs::RUN_REPORT_WINDOW_MS
    );
    let mut select = Query::select();
    select
        .columns([
            (JobDevices::Table, JobDevices::DeviceId),
            (JobDevices::Table, JobDevices::State),
        ])
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.status, file_deliveries.status, update_runs.status, \
                 application_runs.status, \
                 CASE WHEN wake_requests.id IS NULL THEN NULL \
                 WHEN wake_requests.online_at IS NOT NULL THEN 'succeeded' \
                 WHEN wake_requests.status IN ('sending', 'sent') THEN 'pending' \
                 ELSE 'failed' END)",
            ),
            "status",
        )
        .expr_as(
            Expr::cust(
                "CAST(CASE WHEN script_runs.status = 'completed' AND script_runs.exit_code = 0 THEN 1 \
                 WHEN file_deliveries.status = 'delivered' THEN 1 \
                 WHEN update_runs.status = 'succeeded' THEN 1 \
                 WHEN application_runs.status = 'succeeded' THEN 1 \
                 WHEN wake_requests.online_at IS NOT NULL THEN 1 ELSE 0 END AS BIGINT)",
            ),
            "succeeded",
        )
        .expr_as(
            Expr::cust(format!(
                "CAST(CASE WHEN script_runs.status = 'pending' AND {run_deadline} < {now} THEN 1 \
                 WHEN file_deliveries.status = 'pending' AND {delivery_deadline} < {now} THEN 1 \
                 WHEN update_runs.status = 'pending' AND {update_deadline} < {now} THEN 1 \
                 WHEN application_runs.status = 'pending' AND {application_deadline} < {now} THEN 1 \
                 WHEN wake_requests.online_at IS NULL AND wake_requests.status IN ('sending', 'sent') \
                 AND {wake_deadline} < {now} THEN 1 \
                 ELSE 0 END AS BIGINT)"
            )),
            "overdue",
        )
        .from(JobDevices::Table);
    for table in [
        "script_runs",
        "file_deliveries",
        "update_runs",
        "wake_requests",
        "application_runs",
    ] {
        select.left_join(
            Alias::new(table),
            Expr::col((Alias::new(table), Alias::new("job_id")))
                .equals((JobDevices::Table, JobDevices::JobId))
                .and(
                    Expr::col((Alias::new(table), Alias::new("device_id")))
                        .equals((JobDevices::Table, JobDevices::DeviceId)),
                ),
        );
    }
    select
        .and_where(Expr::col((JobDevices::Table, JobDevices::JobId)).eq(job_id))
        .to_owned()
}

/// Checks the job's stored outcomes and counts against the old reading at
/// each of `nows`, and returns the outcomes at the first, by device.
async fn same(app: &App, job_id: &str, nows: &[i64]) -> BTreeMap<String, Outcome> {
    let mut first = None;
    for &now in nows {
        let rows: Vec<(String, String, Option<String>, i64, i64)> =
            app.db().fetch_all(&old_select(job_id, now)).await.unwrap();
        let old = rows
            .into_iter()
            .map(|(device_id, state, status, succeeded, overdue)| {
                let outcome = old_outcome(&state, status.as_deref(), succeeded != 0, overdue != 0);
                (device_id, outcome)
            })
            .collect::<BTreeMap<_, _>>();
        let stored = jobs::devices(&mut app.db(), job_id, now)
            .await
            .unwrap()
            .into_iter()
            .map(|device| (device.device_id, device.outcome))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(stored, old, "{}: job {job_id} at {now}", app.name);
        let mut counts = Counts::default();
        for outcome in old.values() {
            counts.add(*outcome, 1);
        }
        let counted = jobs::counts(&mut app.db(), &[job_id.to_owned()], now)
            .await
            .unwrap()
            .remove(job_id)
            .unwrap_or_default();
        assert_eq!(counted, counts, "{}: job {job_id} at {now}", app.name);
        first.get_or_insert(old);
    }
    first.unwrap_or_default()
}

/// Every moment worth looking at after `now`: before and after each kind's
/// report deadline.
fn clocks(now: i64) -> Vec<i64> {
    [
        0,
        2 * MINUTE_MS,
        20 * MINUTE_MS,
        2 * HOUR_MS,
        6 * HOUR_MS,
        2 * DAY_MS,
    ]
    .into_iter()
    .map(|later| now + later)
    .collect()
}

async fn start_job(browser: &mut Browser, body: Value) -> String {
    let started = browser.post("/v1/jobs", body).await;
    assert_eq!(started.status, StatusCode::CREATED, "{:?}", started.body);
    started.body["id"].as_str().unwrap().to_owned()
}

/// Empties the commands queued for stand-in connections, so none fills up.
fn drain(connections: &mut [&mut AgentConnection]) {
    for connection in connections {
        while connection.commands.try_recv().is_ok() {}
    }
}

/// The device's part, as the job's page reads it now.
async fn part(app: &App, job_id: &str, device_id: &str) -> jobs::JobDevice {
    jobs::devices(&mut app.db(), job_id, now_ms())
        .await
        .unwrap()
        .into_iter()
        .find(|device| device.device_id == device_id)
        .unwrap()
}

fn expect(outcomes: &BTreeMap<String, Outcome>, expected: &[(&str, Outcome)], what: &str) {
    for (device_id, outcome) in expected {
        assert_eq!(
            outcomes.get(*device_id),
            Some(outcome),
            "{what}: {device_id}"
        );
    }
}

fn script_report(status: &str, exit_code: Option<i32>) -> Value {
    json!({ "status": status, "ran_as": "SYSTEM", "exit_code": exit_code, "stdout": "", "stderr": "" })
}

#[tokio::test]
async fn script_and_file_jobs_store_what_the_old_reading_said() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let script = admin
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Inventory", "language": "powershell", "body": "echo hi", "timeout_seconds": 60, "shared": false }),
            )
            .await;
        assert_eq!(script.status, StatusCode::CREATED, "{:?}", script.body);
        let script = script.body["id"].as_str().unwrap().to_owned();
        let mut agents = BTreeMap::new();
        for device in [
            "OK", "EXIT1", "FAILED", "TIMEOUT", "PENDING", "LOST", "GONE", "AWAY", "DELETED",
            "LATE",
        ] {
            agents.insert(device, app.enroll(&mut admin, device).await);
        }
        let mac = app.enroll_on(&mut admin, "MAC", "macos").await;
        let id = |device: &str| agents[device].device_id.clone();
        let mut connections = [
            "OK", "EXIT1", "FAILED", "TIMEOUT", "PENDING", "LOST", "DELETED",
        ]
        .into_iter()
        .map(|device| app.state.agents.connect(&id(device)))
        .collect::<Vec<_>>();
        // Online when the job starts, gone before its turn.
        let gone = app.state.agents.connect(&id("GONE"));
        let mut devices = agents
            .values()
            .map(|agent| agent.device_id.clone())
            .collect::<Vec<_>>();
        devices.push(mac.device_id.clone());
        let job = start_job(
            &mut admin,
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": devices, "offline": "wait", "wait_hours": 2,
            }),
        )
        .await;
        let now = now_ms();
        let outcomes = same(&app, &job, &clocks(now)).await;
        expect(
            &outcomes,
            &[
                (&id("OK"), Outcome::Queued),
                (&id("AWAY"), Outcome::Waiting),
                (&mac.device_id, Outcome::Skipped),
            ],
            name,
        );

        // A queued device is deleted.
        let deleted = admin.delete(&format!("/v1/agents/{}", id("DELETED"))).await;
        assert!(deleted.status.is_success(), "{name}: {:?}", deleted.body);
        drop(gone);
        automation::pass(&app.state, now).await.unwrap();
        drain(&mut connections.iter_mut().collect::<Vec<_>>());
        let outcomes = same(&app, &job, &clocks(now)).await;
        expect(
            &outcomes,
            &[
                (&id("DELETED"), Outcome::Missed),
                (&id("GONE"), Outcome::Waiting),
                (&id("OK"), Outcome::Running),
                (&id("PENDING"), Outcome::Running),
            ],
            name,
        );
        let overdue = same(&app, &job, &[now + HOUR_MS]).await;
        expect(&overdue, &[(&id("PENDING"), Outcome::NoResult)], name);

        // Reports of every kind.
        for (device, report) in [
            ("OK", script_report("completed", Some(0))),
            ("EXIT1", script_report("completed", Some(1))),
            (
                "FAILED",
                json!({ "status": "failed", "ran_as": "", "stdout": "", "stderr": "", "error": "no interpreter" }),
            ),
            ("TIMEOUT", script_report("timed_out", None)),
        ] {
            let run_id = part(&app, &job, &id(device)).await.run_id.unwrap();
            let reported = agents[device]
                .report(&format!("script-runs/{run_id}/result"), report)
                .await;
            assert_eq!(
                reported.status,
                StatusCode::NO_CONTENT,
                "{name}: {device}: {:?}",
                reported.body
            );
            same(&app, &job, &clocks(now)).await;
        }
        // The Agent never reports: lost by the alert engine's sweep.
        meshrmm_server::alerts::evaluate(&app.state, now + HOUR_MS)
            .await
            .unwrap();
        let outcomes = same(&app, &job, &clocks(now)).await;
        expect(
            &outcomes,
            &[
                (&id("OK"), Outcome::Succeeded),
                (&id("EXIT1"), Outcome::Failed),
                (&id("FAILED"), Outcome::Failed),
                (&id("TIMEOUT"), Outcome::TimedOut),
                (&id("LOST"), Outcome::NoResult),
                (&id("PENDING"), Outcome::NoResult),
            ],
            name,
        );

        // One that connects late runs; the rest are cancelled.
        let mut late = app.state.agents.connect(&id("LATE"));
        automation::pass(&app.state, now + SECOND_MS).await.unwrap();
        drain(&mut [&mut late]);
        let outcomes = same(&app, &job, &clocks(now + SECOND_MS)).await;
        expect(&outcomes, &[(&id("LATE"), Outcome::Running)], name);
        let cancelled = admin
            .post(&format!("/v1/jobs/{job}/cancel"), json!({}))
            .await;
        assert!(
            cancelled.status.is_success(),
            "{name}: {:?}",
            cancelled.body
        );
        let outcomes = same(&app, &job, &clocks(now + SECOND_MS)).await;
        expect(
            &outcomes,
            &[
                (&id("AWAY"), Outcome::Cancelled),
                (&id("GONE"), Outcome::Cancelled),
            ],
            name,
        );
        // A device deleted after its run reported keeps its outcome.
        let deleted = admin.delete(&format!("/v1/agents/{}", id("OK"))).await;
        assert!(deleted.status.is_success());
        same(&app, &job, &clocks(now)).await;

        // A job that doesn't wait misses offline devices.
        let skipping = start_job(
            &mut admin,
            json!({
                "script_id": script, "run_as": "system", "target": "devices",
                "device_ids": [id("AWAY")], "offline": "skip",
            }),
        )
        .await;
        let outcomes = same(&app, &skipping, &clocks(now)).await;
        expect(&outcomes, &[(&id("AWAY"), Outcome::Missed)], name);

        // Files: delivered, failed, never reported, and deleted before
        // they were sent.
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
        let file_id = uploaded.body["id"].as_str().unwrap().to_owned();
        let file_job = |devices: Vec<String>| {
            json!({
                "file_id": file_id, "destination": "public", "target": "devices",
                "device_ids": devices, "offline": "skip",
            })
        };
        let files = start_job(
            &mut admin,
            file_job(vec![id("EXIT1"), id("FAILED"), id("TIMEOUT")]),
        )
        .await;
        let later = now + 10 * MINUTE_MS;
        automation::pass(&app.state, later).await.unwrap();
        drain(&mut connections.iter_mut().collect::<Vec<_>>());
        same(&app, &files, &clocks(later)).await;
        for (device, status) in [("EXIT1", "delivered"), ("FAILED", "failed")] {
            let delivery_id = part(&app, &files, &id(device)).await.delivery_id.unwrap();
            let reported = agents[device]
                .report(
                    &format!("file-deliveries/{delivery_id}/result"),
                    json!({ "status": status, "path": "C:\\Users\\Public\\Documents\\setup.exe", "error": (status == "failed").then_some("disk full") }),
                )
                .await;
            assert_eq!(
                reported.status,
                StatusCode::NO_CONTENT,
                "{name}: {:?}",
                reported.body
            );
        }
        let outcomes = same(&app, &files, &clocks(later)).await;
        expect(
            &outcomes,
            &[
                (&id("EXIT1"), Outcome::Succeeded),
                (&id("FAILED"), Outcome::Failed),
                (&id("TIMEOUT"), Outcome::Running),
            ],
            name,
        );
        let gone_file = start_job(&mut admin, file_job(vec![id("EXIT1")])).await;
        let removed = admin.delete(&format!("/v1/toolbox/files/{file_id}")).await;
        assert!(removed.status.is_success(), "{name}: {:?}", removed.body);
        automation::pass(&app.state, later + SECOND_MS)
            .await
            .unwrap();
        let outcomes = same(&app, &gone_file, &clocks(later)).await;
        expect(&outcomes, &[(&id("EXIT1"), Outcome::Failed)], name);

        // A run deleted while its job is kept, as between the two deletes of
        // one maintenance pass, keeps its device's outcome, where the old
        // reading had no result: the one place the two differ.
        app.db()
            .execute(
                &Query::delete()
                    .from_table(ScriptRuns::Table)
                    .and_where(Expr::col(ScriptRuns::JobId).eq(job.as_str()))
                    .and_where(Expr::col(ScriptRuns::DeviceId).eq(id("EXIT1")))
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(
            part(&app, &job, &id("EXIT1")).await.outcome,
            Outcome::Failed
        );
        // History purged: the jobs go with their runs.
        maintenance::purge(app.db(), now + 31 * DAY_MS).await;
        for job in [&job, &skipping, &files, &gone_file] {
            assert!(same(&app, job, &clocks(now)).await.is_empty(), "{name}");
        }
        app.finish().await;
    }
}

fn update_scan(error: Option<&str>) -> meshrmm_protocol_types::UpdateScan {
    serde_json::from_value(json!({
        "scanned_at_unix_ms": 1, "manager": "windows_update", "updates": [],
        "reboot_pending": false, "error": error,
    }))
    .unwrap()
}

#[tokio::test]
async fn update_and_application_jobs_store_what_the_old_reading_said() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut agents = BTreeMap::new();
        for device in [
            "SCAN-OK",
            "SCAN-ERROR",
            "SCAN-LOST",
            "INSTALL-OK",
            "INSTALL-PARTIAL",
            "INSTALL-FAILED",
            "INSTALL-LOST",
            "APP-OK",
            "APP-FAILED",
            "APP-PARTIAL",
            "APP-SKIPPED",
            "APP-INTERRUPTED",
            "APP-LOST",
            "APP-UNUSABLE",
        ] {
            agents.insert(device, app.enroll(&mut admin, device).await);
        }
        let id = |device: &str| agents[device].device_id.clone();
        let mut connections = agents
            .values()
            .map(|agent| app.state.agents.connect(&agent.device_id))
            .collect::<Vec<_>>();
        let of = |prefix: &str| {
            agents
                .iter()
                .filter(|(device, _)| device.starts_with(prefix))
                .map(|(_, agent)| agent.device_id.clone())
                .collect::<Vec<_>>()
        };
        let scans = start_job(
            &mut admin,
            json!({ "updates": { "action": "scan" }, "target": "devices", "device_ids": of("SCAN"), "offline": "skip" }),
        )
        .await;
        let installs = start_job(
            &mut admin,
            json!({
                "updates": { "action": "install", "categories": ["security"], "reboot": "never" },
                "target": "devices", "device_ids": of("INSTALL"), "offline": "skip",
            }),
        )
        .await;
        let firefox = admin
            .post(
                "/v1/applications",
                json!({
                    "name": "Firefox", "description": "The browser",
                    "sources": [{ "source": "winget", "package_id": "Mozilla.Firefox" }],
                }),
            )
            .await;
        assert_eq!(firefox.status, StatusCode::CREATED, "{:?}", firefox.body);
        // A device whose package managers can't install it is skipped when
        // its turn comes.
        applications::scan_reported(
            &app.state,
            &id("APP-UNUSABLE"),
            Vec::new(),
            serde_json::from_value(
                json!({ "scanned_at_unix_ms": 1, "managers": [], "packages": [] }),
            )
            .unwrap(),
        )
        .await;
        let installing = start_job(
            &mut admin,
            json!({
                "applications": { "action": "install", "application_ids": [firefox.body["id"]] },
                "target": "devices", "device_ids": of("APP"), "offline": "skip",
            }),
        )
        .await;
        let now = now_ms();
        for job in [&scans, &installs, &installing] {
            same(&app, job, &clocks(now)).await;
        }
        automation::pass(&app.state, now).await.unwrap();
        drain(&mut connections.iter_mut().collect::<Vec<_>>());
        for job in [&scans, &installs, &installing] {
            let outcomes = same(&app, job, &clocks(now)).await;
            assert!(
                outcomes
                    .values()
                    .all(|outcome| *outcome == Outcome::Running || *outcome == Outcome::Skipped),
                "{name}: {outcomes:?}"
            );
        }
        expect(
            &same(&app, &installing, &[now]).await,
            &[(&id("APP-UNUSABLE"), Outcome::Skipped)],
            name,
        );

        // Scans report, with and without an error.
        for (device, error) in [("SCAN-OK", None), ("SCAN-ERROR", Some("no service"))] {
            let run_id = part(&app, &scans, &id(device)).await.update_run_id.unwrap();
            updates::scan_reported(&app.state, &id(device), vec![run_id], update_scan(error)).await;
        }
        // Installs report each way.
        for (device, status) in [
            ("INSTALL-OK", "succeeded"),
            ("INSTALL-PARTIAL", "partial"),
            ("INSTALL-FAILED", "failed"),
        ] {
            let run_id = part(&app, &installs, &id(device))
                .await
                .update_run_id
                .unwrap();
            updates::install_reported(
                &app.state,
                &id(device),
                &run_id,
                serde_json::from_value(json!({ "status": status, "items": [] })).unwrap(),
            )
            .await;
        }
        // Application runs report each way.
        for (device, status, item) in [
            ("APP-OK", "succeeded", "succeeded"),
            ("APP-FAILED", "failed", "failed"),
            ("APP-PARTIAL", "partial", "failed"),
            ("APP-SKIPPED", "skipped", "skipped"),
        ] {
            let run_id = part(&app, &installing, &id(device))
                .await
                .application_run_id
                .unwrap();
            applications::result_reported(
                &app.state,
                &id(device),
                &run_id,
                serde_json::from_value(json!({
                    "status": status,
                    "items": [{ "index": 0, "name": "Firefox", "outcome": item, "output": "" }],
                }))
                .unwrap(),
            )
            .await;
        }
        // An Agent that restarted no longer has its run.
        applications::activity_reported(
            &app.state,
            &id("APP-INTERRUPTED"),
            &[],
            now_ms() + HOUR_MS,
        )
        .await;
        for job in [&scans, &installs, &installing] {
            same(&app, job, &clocks(now)).await;
        }
        // The rest never report: lost by the sweeps once their windows pass.
        updates::sweep(&app.state, now + 6 * HOUR_MS).await.unwrap();
        applications::sweep(&app.state, now + 6 * HOUR_MS)
            .await
            .unwrap();
        let mut all = BTreeMap::new();
        for job in [&scans, &installs, &installing] {
            all.extend(same(&app, job, &clocks(now)).await);
        }
        expect(
            &all,
            &[
                (&id("SCAN-OK"), Outcome::Succeeded),
                (&id("SCAN-ERROR"), Outcome::Failed),
                (&id("SCAN-LOST"), Outcome::NoResult),
                (&id("INSTALL-OK"), Outcome::Succeeded),
                (&id("INSTALL-PARTIAL"), Outcome::Failed),
                (&id("INSTALL-FAILED"), Outcome::Failed),
                (&id("INSTALL-LOST"), Outcome::NoResult),
                (&id("APP-OK"), Outcome::Succeeded),
                (&id("APP-FAILED"), Outcome::Failed),
                (&id("APP-PARTIAL"), Outcome::Failed),
                (&id("APP-SKIPPED"), Outcome::Skipped),
                (&id("APP-INTERRUPTED"), Outcome::NoResult),
                (&id("APP-LOST"), Outcome::NoResult),
                (&id("APP-UNUSABLE"), Outcome::Skipped),
            ],
            name,
        );
        app.finish().await;
    }
}

/// A snapshot with one wired adapter: its MAC and its address on a /24.
fn snapshot(mac: &str, address: &str) -> SystemInfo {
    serde_json::from_value(json!({
        "collected_at_unix_ms": 1_700_000_000_000_i64,
        "os": { "id": "windows", "name": "Windows 11 Pro", "version": "24H2", "architecture": "x86_64" },
        "hardware": { "memory_bytes": 1 },
        "network": { "hostname": "PC", "adapters": [{
            "name": "Ethernet", "kind": "ethernet", "mac_address": mac, "up": true,
            "addresses": [{ "address": address, "prefix_length": 24 }],
        }] },
    }))
    .unwrap()
}

/// The wake packets the stand-in relay was asked to send.
fn packets(connection: &mut AgentConnection) -> Vec<WakePacket> {
    let mut packets = Vec::new();
    while let Ok(message) = connection.commands.try_recv() {
        if let ToAgent::Command(AgentCommand::SendWakePackets { packets: sent }) = message {
            packets.extend(sent);
        }
    }
    packets
}

#[tokio::test]
async fn wake_jobs_store_what_the_old_reading_said() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut agents = BTreeMap::new();
        for device in ["RELAY", "WOKEN", "ASLEEP", "UNSENT", "STRANDED"] {
            agents.insert(device, app.enroll(&mut admin, device).await);
        }
        let id = |device: &str| agents[device].device_id.clone();
        for (index, device) in ["RELAY", "WOKEN", "ASLEEP", "UNSENT"]
            .into_iter()
            .enumerate()
        {
            assert!(
                meshrmm_server::system_info::store(
                    &app.state.database,
                    &id(device),
                    "203.0.113.5".parse().unwrap(),
                    snapshot(
                        &format!("AA:00:00:00:00:1{index}"),
                        &format!("192.168.1.{}", 10 + index)
                    ),
                    None,
                )
                .await
                .unwrap()
                .is_some()
            );
        }
        let mut relay = app.state.agents.connect(&id("RELAY"));
        let policy = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Wake", "action": "wake", "target": "devices",
                    "device_ids": agents.values().map(|agent| agent.device_id.clone()).collect::<Vec<_>>(),
                    "trigger": "daily", "time": "01:50", "time_zone": "UTC", "offline": "skip",
                }),
            )
            .await;
        assert_eq!(
            policy.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            policy.body
        );
        let started = admin
            .post(
                &format!(
                    "/v1/automation-policies/{}/run",
                    policy.body["id"].as_str().unwrap()
                ),
                json!({}),
            )
            .await;
        assert_eq!(
            started.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            started.body
        );
        let job = started.body["id"].as_str().unwrap().to_owned();
        let now = now_ms();
        same(&app, &job, &clocks(now)).await;
        automation::pass(&app.state, now).await.unwrap();
        let outcomes = same(&app, &job, &clocks(now)).await;
        expect(
            &outcomes,
            &[
                (&id("RELAY"), Outcome::Skipped),
                (&id("WOKEN"), Outcome::Running),
                (&id("STRANDED"), Outcome::Failed),
            ],
            name,
        );
        // The relay sends two and fails the third.
        let parts = jobs::devices(&mut app.db(), &job, now).await.unwrap();
        let device_of = |wake_id: &str| {
            parts
                .iter()
                .find(|part| part.wake_id.as_deref() == Some(wake_id))
                .map(|part| part.device_id.clone())
                .unwrap()
        };
        let results = packets(&mut relay)
            .into_iter()
            .map(|packet| WakePacketResult {
                error: (device_of(&packet.wake_id) == id("UNSENT")).then(|| "no route".to_owned()),
                source_address: Some("192.168.1.10".to_owned()),
                wake_id: packet.wake_id,
                broadcast: packet.broadcast,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 3, "{name}");
        wake::reported(&app.state, &id("RELAY"), results).await;
        let outcomes = same(&app, &job, &clocks(now)).await;
        expect(&outcomes, &[(&id("UNSENT"), Outcome::Failed)], name);
        // One comes online; the other never does, and has no result once
        // the window passes.
        let _woken = app.state.agents.connect(&id("WOKEN"));
        wake::device_connected(&app.state, &id("WOKEN")).await;
        let outcomes = same(&app, &job, &clocks(now)).await;
        expect(
            &outcomes,
            &[
                (&id("WOKEN"), Outcome::Succeeded),
                (&id("ASLEEP"), Outcome::Running),
            ],
            name,
        );
        let late = same(&app, &job, &[now + 20 * MINUTE_MS]).await;
        expect(&late, &[(&id("ASLEEP"), Outcome::NoResult)], name);
        app.finish().await;
    }
}
