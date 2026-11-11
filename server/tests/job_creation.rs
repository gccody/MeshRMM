//! Creating a job on more devices than one transaction writes: its devices a
//! batch per transaction, the job left alone until the last one, and a job
//! whose creation stopped part-way removed.
mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{App, Browser, apps, audit_events, set_up};
use meshrmm_server::{
    automation::{
        self, Batch, CreationPause,
        jobs::{self, DEVICES_PER_TRANSACTION, Target},
    },
    db::tables::{Agents, JobDevices, Jobs, UpdateRuns, Users},
    time::{HOUR_MS, MINUTE_MS, now_ms},
};
use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Value, json};

/// Devices for a job of three transactions of devices: two full, one not.
const FLEET: usize = 2 * DEVICES_PER_TRANSACTION + 500;

/// Enrolls `count` Windows devices straight into the database. Returns their
/// IDs.
async fn fleet(app: &App, count: usize) -> Vec<String> {
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
                format!("FLEET-{}", chunk_index * 400 + index).into(),
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

/// An update scan of every device that waits for offline ones, started by
/// the first user, or by a tester before there is one.
async fn update_scan(app: &App) -> jobs::NewJob {
    let user: Option<(String,)> = app
        .db()
        .fetch_optional(
            &Query::select()
                .column(Users::Id)
                .from(Users::Table)
                .to_owned(),
        )
        .await
        .unwrap();
    jobs::NewJob {
        payload: jobs::Payload::UpdateScan,
        target: Target::All,
        offline: jobs::Offline::Wait,
        wait_ms: HOUR_MS,
        requested_by_user_id: user.map_or_else(|| "tester".to_owned(), |(id,)| id),
        policy: None,
        actor: meshrmm_server::audit::Actor {
            user_id: None,
            label: "tester".to_owned(),
            ip: None,
            api_token: None,
        },
        scheduled: None,
        within: meshrmm_server::access::CompanySet::All,
    }
}

/// Plans an update scan of every device and starts creating it in a task of
/// its own, as a request would.
async fn start_creating(
    app: &App,
) -> tokio::task::JoinHandle<Result<jobs::Created, meshrmm_server::http::ApiError>> {
    let plan = jobs::plan(&mut app.db(), &app.state.agents, update_scan(app).await)
        .await
        .unwrap();
    assert!(plan.batched(), "{}", app.name);
    let state = app.state.clone();
    tokio::spawn(async move { jobs::create(&state, &plan, now_ms()).await })
}

/// The next transaction of devices that `creating` committed, paused.
async fn next_batch<T: std::fmt::Debug>(
    pause: &mut CreationPause,
    creating: &mut tokio::task::JoinHandle<T>,
) -> Batch {
    let batch = async {
        tokio::select! {
            batch = pause.next() => batch,
            ended = creating => panic!("the creation ended first: {ended:?}"),
        }
    };
    tokio::time::timeout(Duration::from_secs(60), batch)
        .await
        .expect("a batch of devices is written")
}

/// Whether the job is marked as being created, or `None` without its row.
async fn marked(app: &App, job_id: &str) -> Option<bool> {
    let row: Option<(bool,)> = app
        .db()
        .fetch_optional(
            &Query::select()
                .column(Jobs::Creating)
                .from(Jobs::Table)
                .and_where(Expr::col(Jobs::Id).eq(job_id))
                .to_owned(),
        )
        .await
        .unwrap();
    row.map(|(creating,)| creating)
}

/// The job's devices as stored, by ID, with their states.
async fn stored_devices(app: &App, job_id: &str) -> Vec<(String, String)> {
    app.db()
        .fetch_all(
            &Query::select()
                .columns([JobDevices::DeviceId, JobDevices::State])
                .from(JobDevices::Table)
                .and_where(Expr::col(JobDevices::JobId).eq(job_id))
                .order_by(JobDevices::DeviceId, sea_query::Order::Asc)
                .to_owned(),
        )
        .await
        .unwrap()
}

/// The devices the job started an update scan on, once for each start.
async fn started_on(app: &App, job_id: &str) -> Vec<String> {
    let rows: Vec<(String,)> = app
        .db()
        .fetch_all(
            &Query::select()
                .column(UpdateRuns::DeviceId)
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::JobId).eq(job_id))
                .order_by(UpdateRuns::DeviceId, sea_query::Order::Asc)
                .to_owned(),
        )
        .await
        .unwrap();
    let mut devices = rows.into_iter().map(|(id,)| id).collect::<Vec<_>>();
    devices.sort();
    devices
}

/// How many jobs there are, shown or not.
async fn job_rows(app: &App) -> i64 {
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

async fn pass(state: &meshrmm_server::http::AppState, now: i64) {
    automation::pass(state, now).await.unwrap();
}

async fn script_policy(admin: &mut Browser) -> (String, i64) {
    let script = admin
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": "Health check", "language": "powershell", "body": "echo hi", "timeout_seconds": 60, "shared": true }),
        )
        .await;
    assert_eq!(script.status, StatusCode::CREATED, "{:?}", script.body);
    let policy = admin
        .post(
            "/v1/automation-policies",
            json!({
                "name": "Every five minutes", "script_id": script.body["id"], "run_as": "system",
                "target": "all", "trigger": "interval", "interval_minutes": 5,
                "offline": "wait", "catch_up_hours": 24,
            }),
        )
        .await;
    assert_eq!(policy.status, StatusCode::CREATED, "{:?}", policy.body);
    (
        policy.body["id"].as_str().unwrap().to_owned(),
        policy.body["next_run_at"].as_i64().unwrap(),
    )
}

async fn listed_policy(admin: &mut Browser, id: &str) -> Value {
    admin.get("/v1/automation-policies").await.body["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|policy| policy["id"] == id)
        .unwrap()
        .clone()
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

/// How many jobs the policy has, shown or not.
async fn policy_job_rows(app: &App, policy_id: &str) -> i64 {
    let (count,): (i64,) = app
        .db()
        .fetch_one(
            &Query::select()
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(Jobs::Table)
                .and_where(Expr::col(Jobs::PolicyId).eq(policy_id))
                .to_owned(),
        )
        .await
        .unwrap();
    count
}

#[tokio::test]
async fn a_large_job_is_written_a_batch_per_transaction_and_counted_whole() {
    for app in apps().await {
        let name = app.name;
        let devices = fleet(&app, FLEET).await;
        let _online = devices[..3]
            .iter()
            .map(|id| app.state.agents.connect(id))
            .collect::<Vec<_>>();
        let mut pause = app.state.automation.pause_creation();
        let before = app.db().counts();
        let mut creating = start_creating(&app).await;
        let planned = app.db().counts();
        // Each transaction of devices, with what was run since the last.
        let mut batches: Vec<(Batch, meshrmm_server::db::Counts)> = Vec::new();
        let mut last = planned;
        let mut devices_written = 0;
        while devices_written < FLEET {
            let batch = next_batch(&mut pause, &mut creating).await;
            let counts = app.db().counts();
            devices_written += batch.devices;
            batches.push((batch, counts - last));
            last = counts;
            pause.resume();
        }
        let created = creating.await.unwrap().unwrap();
        let written = app.db().counts() - planned;
        assert_eq!(created.devices, FLEET as i64, "{name}");
        assert_eq!(batches.len(), 3, "{name}: {batches:?}");
        for (index, (batch, counts)) in batches.iter().enumerate() {
            assert_eq!(batch.job_id, created.id, "{name}");
            assert!(
                batch.devices <= DEVICES_PER_TRANSACTION,
                "{name}: {batches:?}"
            );
            // A transaction of its own, of a statement for each 400 devices
            // of a state; the first follows the one that stored the job.
            let own = usize::from(index == 0);
            assert_eq!(counts.transactions as usize, 1 + own, "{name}: {batches:?}");
            assert_eq!(counts.statements, counts.writes, "{name}: nothing is read");
            assert!(
                counts.writes as usize <= own + DEVICES_PER_TRANSACTION.div_ceil(400) + 1,
                "{name}: {batches:?}"
            );
        }
        // Full transactions but the last.
        let sizes = batches
            .iter()
            .map(|(batch, _)| batch.devices)
            .collect::<Vec<_>>();
        let full = DEVICES_PER_TRANSACTION;
        assert_eq!(sizes, [full, full, FLEET - 2 * full], "{name}");
        // The job's row, three transactions of devices (9 statements, each of
        // at most 400 devices of one state and what fits in its transaction)
        // and one that clears its mark and records it.
        assert_eq!(
            (written.transactions, written.statements, written.writes),
            (5, 1 + 9 + 2, 1 + 9 + 2),
            "{name}"
        );
        assert_eq!((planned - before).writes, 0, "{name}: planning only reads");
        let job = jobs::get(&mut app.db(), &created.id, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (job.counts.total, job.counts.queued, job.counts.waiting),
            (FLEET as i64, 3, FLEET as i64 - 3),
            "{name}"
        );
        assert_eq!(marked(&app, &created.id).await, Some(false), "{name}");
        let started = audit_events(&app, "job.start").await;
        assert_eq!(started.len(), 1, "{name}");
        assert_eq!(started[0].metadata["devices"], FLEET, "{name}");
        assert_eq!(started[0].metadata["queued"], 3, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn nothing_starts_shows_or_queues_a_job_until_it_is_created() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let devices = fleet(&app, FLEET).await;
        // Online when the job is planned, and so queued in it.
        let online = devices[0].clone();
        let _online = app.state.agents.connect(&online);
        // Offline then, and so waiting; it connects while the job is written.
        let late = devices[FLEET - 1].clone();
        let mut pause = app.state.automation.pause_creation();
        let mut creating = start_creating(&app).await;
        let first = next_batch(&mut pause, &mut creating).await;
        let id = first.job_id.clone();
        assert_eq!(marked(&app, &id).await, Some(true), "{name}");
        let stored = stored_devices(&app, &id).await;
        assert_eq!(stored.len(), first.devices, "{name}");

        let _late = app.state.agents.connect(&late);
        app.state.automation.device_connected(&late);
        let now = now_ms();
        // A full pass and a quick step: they start, queue, expire, finish and
        // remove nothing of it.
        pass(&app.state, now).await;
        automation::step(&app.state, now + 1).await.unwrap();
        assert!(started_on(&app, &id).await.is_empty(), "{name}");
        assert_eq!(stored_devices(&app, &id).await, stored, "{name}");
        assert_eq!(marked(&app, &id).await, Some(true), "{name}");
        // Neither the lists nor its pages show it, and it can't be cancelled.
        assert!(
            jobs::get(&mut app.db(), &id, now).await.unwrap().is_none(),
            "{name}"
        );
        let listed = admin.get("/v1/jobs").await;
        assert_eq!(listed.body["jobs"], json!([]), "{name}");
        for path in [format!("/v1/jobs/{id}"), format!("/v1/jobs/{id}/devices")] {
            assert_eq!(
                admin.get(&path).await.status,
                StatusCode::NOT_FOUND,
                "{name}: {path}"
            );
        }
        let cancelled = admin
            .post(&format!("/v1/jobs/{id}/cancel"), json!({}))
            .await;
        assert_eq!(cancelled.status, StatusCode::NOT_FOUND, "{name}");

        // Created: the website announces it, and both devices start, once.
        drop(pause);
        let created = creating.await.unwrap().unwrap();
        assert_eq!(created.id, id, "{name}");
        assert_eq!(created.devices, FLEET as i64, "{name}");
        app.state.automation.job_changed(&id);
        automation::step(&app.state, now + 2).await.unwrap();
        let mut both = vec![online.clone(), late.clone()];
        both.sort();
        assert_eq!(started_on(&app, &id).await, both, "{name}");
        pass(&app.state, now + 3).await;
        automation::step(&app.state, now + 4).await.unwrap();
        assert_eq!(started_on(&app, &id).await, both, "{name}: not twice");
        let shown = admin.get(&format!("/v1/jobs/{id}")).await;
        assert_eq!(shown.status, StatusCode::OK, "{name}");
        assert_eq!(shown.body["counts"]["total"], FLEET, "{name}");
        assert_eq!(shown.body["counts"]["running"], 2, "{name}");
        assert_eq!(shown.body["counts"]["waiting"], FLEET - 2, "{name}");
        assert_eq!(
            admin.get("/v1/jobs").await.body["jobs"][0]["id"],
            id.as_str(),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_job_whose_creation_stopped_is_removed_and_its_firing_fires_once() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        fleet(&app, FLEET).await;

        // A request that ends after its first transaction of devices, as
        // when the server stops: the job stays marked, with those devices.
        let mut pause = app.state.automation.pause_creation();
        let mut creating = start_creating(&app).await;
        let first = next_batch(&mut pause, &mut creating).await;
        creating.abort();
        assert!(creating.await.unwrap_err().is_cancelled(), "{name}");
        let id = first.job_id.clone();
        assert_eq!(marked(&app, &id).await, Some(true), "{name}");
        assert_eq!(
            stored_devices(&app, &id).await.len(),
            first.devices,
            "{name}"
        );
        // The server after a restart removes it on its first pass.
        let restarted = app.restarted().await;
        pass(&restarted.state, now_ms()).await;
        assert_eq!(marked(&app, &id).await, None, "{name}");
        assert!(stored_devices(&app, &id).await.is_empty(), "{name}");
        assert_eq!(job_rows(&app).await, 0, "{name}");

        // A policy's firing that stops part-way isn't claimed: the policy is
        // still due, and the next pass removes the half-made job and fires.
        let (policy, due) = script_policy(&mut admin).await;
        let state = app.state.clone();
        let mut firing = tokio::spawn(async move { automation::pass(&state, due).await });
        let first = next_batch(&mut pause, &mut firing).await;
        firing.abort();
        assert!(firing.await.unwrap_err().is_cancelled(), "{name}");
        drop(pause);
        assert_eq!(marked(&app, &first.job_id).await, Some(true), "{name}");
        assert_eq!(listed_policy(&mut admin, &policy).await["next_run_at"], due);
        assert!(policy_jobs(&mut admin, &policy).await.is_empty(), "{name}");
        pass(&restarted.state, due + 1_000).await;
        let fired = policy_jobs(&mut admin, &policy).await;
        assert_eq!(fired.len(), 1, "{name}");
        assert_ne!(fired[0]["id"], first.job_id.as_str(), "{name}");
        assert_eq!(fired[0]["counts"]["total"], FLEET, "{name}");
        assert_eq!(policy_job_rows(&app, &policy).await, 1, "{name}");
        let listed = listed_policy(&mut admin, &policy).await;
        assert_eq!(listed["next_run_at"], due + 5 * MINUTE_MS, "{name}");
        assert_eq!(listed["last_job_id"], fired[0]["id"], "{name}");
        // Claimed once: the next passes before its next time fire nothing.
        pass(&restarted.state, due + 2_000).await;
        pass(&app.state, due + 3_000).await;
        assert_eq!(policy_job_rows(&app, &policy).await, 1, "{name}");
        restarted.finish().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn deleting_devices_and_policies_while_a_job_is_created_keeps_it_right() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let devices = fleet(&app, FLEET).await;
        let mut pause = app.state.automation.pause_creation();
        let mut creating = start_creating(&app).await;
        let first = next_batch(&mut pause, &mut creating).await;
        let id = first.job_id.clone();
        let stored = stored_devices(&app, &id).await;
        // One device deleted after its batch, one before.
        let early = stored[0].0.clone();
        let late = devices
            .iter()
            .find(|device| !stored.iter().any(|(stored, _)| stored == *device))
            .unwrap()
            .clone();
        for device in [&early, &late] {
            let deleted = admin.delete(&format!("/v1/agents/{device}")).await;
            assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        }
        drop(pause);
        let created = creating.await.unwrap().unwrap();
        // The later batch left the late one out, and the early one is missed.
        assert_eq!(created.devices, FLEET as i64 - 1, "{name}");
        let shown = admin.get(&format!("/v1/jobs/{id}")).await.body;
        assert_eq!(shown["counts"]["total"], FLEET - 1, "{name}");
        assert_eq!(shown["counts"]["missed"], 1, "{name}");
        assert_eq!(shown["counts"]["waiting"], FLEET - 2, "{name}");
        let missed = admin
            .get(&format!("/v1/jobs/{id}/devices?outcome=missed"))
            .await
            .body;
        assert_eq!(missed["devices"][0]["device_id"], early.as_str(), "{name}");
        assert_eq!(missed["devices"][0]["detail"], "The device was deleted");
        // Created, it cancels as any job does.
        let cancelled = admin
            .post(&format!("/v1/jobs/{id}/cancel"), json!({}))
            .await;
        assert_eq!(cancelled.status, StatusCode::OK, "{name}");
        assert_eq!(cancelled.body["counts"]["cancelled"], FLEET - 2, "{name}");

        // A policy deleted while its firing writes its job: nothing starts,
        // and the half-made job is removed.
        let (policy, due) = script_policy(&mut admin).await;
        let mut pause = app.state.automation.pause_creation();
        let state = app.state.clone();
        let mut firing = tokio::spawn(async move { automation::pass(&state, due).await });
        next_batch(&mut pause, &mut firing).await;
        assert_eq!(policy_job_rows(&app, &policy).await, 1, "{name}");
        let deleted = admin
            .delete(&format!("/v1/automation-policies/{policy}"))
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        drop(pause);
        firing.await.unwrap().unwrap();
        assert_eq!(policy_job_rows(&app, &policy).await, 0, "{name}");
        assert_eq!(job_rows(&app).await, 1, "{name}: the first job only");
        app.finish().await;
    }
}
