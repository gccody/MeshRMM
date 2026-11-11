//! Reports: each template's numbers on known data, scopes, periods across
//! time zones, schedules firing and emailing what they make, retention,
//! permissions and the audit log.
mod common;

use std::collections::HashMap;

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use common::{Agent, App, Browser, FakeSmtp, apps, audit_events, set_up, user_with};
use meshrmm_server::{
    maintenance,
    reports::{self, RETENTION_MS, document::Document},
    time::{DAY_MS, HOUR_MS, MINUTE_MS, now_ms},
    updates,
};
use sea_query::{Alias, Query};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Seeding, through the stores the Agents' reports go through, or as rows
// the way other tests write them.

fn snapshot(os: (&str, &str), hardware: Value, applications: Vec<Value>) -> Value {
    json!({
        "system": {
            "collected_at_unix_ms": 1_700_000_000_000_i64,
            "os": {
                "id": "windows", "name": os.0, "version": os.1, "build": "26100.4652",
                "architecture": "x86_64", "package_managers": ["windows_update", "winget"]
            },
            "hardware": hardware,
            "network": { "hostname": "HOST", "adapters": [] },
            "users": [],
            "applications": applications,
        }
    })
}

fn dell() -> Value {
    json!({
        "manufacturer": "Dell Inc.", "model": "OptiPlex 7090", "serial_number": "SN-1",
        "memory_bytes": 17_179_869_184_i64,
        "processor": { "name": "Intel(R) Core(TM) i7", "cores": 8, "threads": 16 },
        "disks": [{ "name": "Disk 0", "model": "Samsung SSD 980", "size_bytes": 512_110_190_592_i64 }]
    })
}

fn application(name: &str, version: &str) -> Value {
    json!({ "name": name, "version": version, "publisher": "Example Corp", "source": "windows" })
}

async fn post_snapshot(agent: &Agent, snapshot: &Value) {
    let posted = agent
        .post_snapshot(Body::from(snapshot.to_string()))
        .await
        .json("snapshot");
    assert_eq!(posted.status, 200, "{:?}", posted.body);
}

fn update(id: &str, category: &str, severity: Option<&str>) -> Value {
    json!({
        "id": id, "title": format!("Update {id}"), "category": category,
        "severity": severity, "reboot_required": false,
    })
}

async fn store_updates(
    app: &App,
    device_id: &str,
    manager: Option<&str>,
    scanned_at: i64,
    pending: Vec<Value>,
    reboot_pending: bool,
) {
    let scan = json!({
        "scanned_at_unix_ms": scanned_at, "manager": manager,
        "updates": pending, "reboot_pending": reboot_pending,
    });
    updates::store::store_scan(
        app.db(),
        device_id,
        serde_json::from_value(scan).unwrap(),
        now_ms(),
    )
    .await
    .unwrap();
}

/// An install on the device started at `created_at` that ended `status`.
async fn install(app: &App, device_id: &str, created_at: i64, status: &str, failed: i64) {
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
    let result = serde_json::from_value(json!({ "status": status, "failed": failed })).unwrap();
    assert!(
        runs::finish_install(&mut transaction, device_id, &id, result, created_at + 1)
            .await
            .unwrap()
    );
    transaction.commit().await.unwrap();
}

async fn store_packages(app: &App, device_id: &str, packages: Vec<Value>) {
    let scan = json!({
        "scanned_at_unix_ms": now_ms(), "managers": [{ "source": "winget", "version": "1.9" }],
        "packages": packages,
    });
    meshrmm_server::applications::store::store_scan(
        app.db(),
        device_id,
        serde_json::from_value(scan).unwrap(),
        now_ms(),
    )
    .await
    .unwrap();
}

fn package(id: &str, name: &str, version: &str, available: Option<&str>) -> Value {
    json!({ "source": "winget", "id": id, "name": name, "version": version, "available_version": available })
}

async fn insert(app: &App, table: &str, row: &[(&str, sea_query::Value)]) {
    app.db()
        .execute(
            &Query::insert()
                .into_table(Alias::new(table))
                .columns(row.iter().map(|(column, _)| Alias::new(*column)))
                .values_panic(row.iter().map(|(_, value)| value.clone().into()))
                .to_owned(),
        )
        .await
        .unwrap();
}

async fn create_rule(admin: &mut Browser, name: &str, severity: &str) -> String {
    let created = admin
        .post(
            "/v1/alert-rules",
            json!({ "name": name, "severity": severity, "condition": "device_offline", "duration_minutes": 10 }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

/// An alert of `rule` on the device, raised at `raised_at` and resolved
/// `resolved_after` later, or still open.
async fn alert(
    app: &App,
    rule: &str,
    device_id: &str,
    raised_at: i64,
    resolved_after: Option<i64>,
) {
    let id = uuid();
    let resolved_at = resolved_after.map(|after| raised_at + after);
    insert(
        app,
        "alerts",
        &[
            ("id", id.into()),
            ("rule_id", rule.into()),
            ("device_id", device_id.into()),
            ("subject", "".into()),
            (
                "state",
                if resolved_at.is_some() {
                    "resolved"
                } else {
                    "open"
                }
                .into(),
            ),
            ("detail", "Offline for 12 minutes".into()),
            ("raised_at", raised_at.into()),
            ("last_occurred_at", raised_at.into()),
            ("resolved_at", resolved_at.into()),
            (
                "resolution",
                resolved_at.map(|_| "cleared".to_owned()).into(),
            ),
        ],
    )
    .await;
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A job created at `created_at`, by a policy when `policy` names one, with
/// each device's state and result.
async fn job(
    app: &App,
    created_at: i64,
    policy: Option<&str>,
    devices: &[(&str, &str, Option<&str>)],
) {
    let id = uuid();
    insert(
        app,
        "jobs",
        &[
            ("id", id.clone().into()),
            ("kind", "script".into()),
            ("item_id", "script".into()),
            ("item_name", "Cleanup".into()),
            ("target", "all".into()),
            ("target_label", "Every device".into()),
            ("offline", "skip".into()),
            ("expires_at", (created_at + HOUR_MS).into()),
            ("requested_by_user_id", "user".into()),
            (
                "policy_id",
                policy.map(|name| format!("policy-{name}")).into(),
            ),
            ("policy_name", policy.map(str::to_owned).into()),
            ("created_at", created_at.into()),
        ],
    )
    .await;
    for (device_id, state, result) in devices {
        insert(
            app,
            "job_devices",
            &[
                ("job_id", id.clone().into()),
                ("device_id", (*device_id).into()),
                ("state", (*state).into()),
                ("result", result.map(str::to_owned).into()),
                ("updated_at", created_at.into()),
            ],
        )
        .await;
    }
}

async fn failed_application_run(app: &App, device_id: &str, action: &str, created_at: i64) {
    insert(
        app,
        "application_runs",
        &[
            ("id", uuid().into()),
            ("device_id", device_id.into()),
            ("action", action.into()),
            ("summary", "Install Firefox".into()),
            ("requested_by_user_id", "user".into()),
            ("status", "failed".into()),
            ("failed", 1_i64.into()),
            ("created_at", created_at.into()),
            ("completed_at", (created_at + 1).into()),
        ],
    )
    .await;
}

/// A stored minute of readings, with its storage.
async fn minute(app: &App, device_id: &str, at: i64, used: i64, total: i64) {
    insert(
        app,
        "device_metrics",
        &[
            ("device_id", device_id.into()),
            ("minute", (at - at % MINUTE_MS).into()),
            ("covered_ms", MINUTE_MS.into()),
            ("cpu_percent", 10.0_f64.into()),
            ("cpu_percent_max", 20.0_f64.into()),
            ("memory_used_bytes", 1_i64.into()),
            ("memory_total_bytes", 2_i64.into()),
            ("network_received_bytes_per_second", 0_i64.into()),
            ("network_sent_bytes_per_second", 0_i64.into()),
            ("storage_used_bytes", used.into()),
            ("storage_total_bytes", total.into()),
        ],
    )
    .await;
}

async fn backdate_enrollment(app: &App, device_id: &str, at: i64) {
    app.db()
        .execute(
            &Query::update()
                .table(Alias::new("agents"))
                .value(Alias::new("created_at"), at)
                .and_where(sea_query::ExprTrait::eq(
                    sea_query::Expr::col(Alias::new("id")),
                    device_id,
                ))
                .to_owned(),
        )
        .await
        .unwrap();
}

async fn add_company(admin: &mut Browser, name: &str) -> String {
    let created = admin.post("/v1/companies", json!({ "name": name })).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn move_to(admin: &mut Browser, device_id: &str, company: &str) {
    let moved = admin
        .put(
            &format!("/v1/agents/{device_id}/company"),
            json!({ "company_id": company }),
        )
        .await;
    assert_eq!(moved.status, StatusCode::NO_CONTENT, "{:?}", moved.body);
}

async fn add_tag(admin: &mut Browser, name: &str, devices: &[&str]) -> String {
    let created = admin
        .post("/v1/tags", json!({ "name": name, "color": "blue" }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let id = created.body["id"].as_str().unwrap().to_owned();
    let tagged = admin
        .post(
            "/v1/device-tags",
            json!({ "device_ids": devices, "add": [id], "remove": [] }),
        )
        .await;
    assert_eq!(tagged.status, StatusCode::OK, "{:?}", tagged.body);
    id
}

// ---------------------------------------------------------------------------
// Running reports and reading them.

/// A run request for `template` on every device over the custom range
/// `from..=to` in UTC.
fn request(template: &str, from: &str, to: &str) -> Value {
    json!({
        "template": template, "target": "all", "period": "custom",
        "from": from, "to": to, "time_zone": "UTC",
    })
}

/// Queues the report, makes it as the server's task would, and returns its
/// ID once it succeeded.
async fn run(app: &App, browser: &mut Browser, body: Value) -> String {
    let queued = browser.post("/v1/reports", body).await;
    assert_eq!(
        queued.status,
        StatusCode::ACCEPTED,
        "{}: {:?}",
        app.name,
        queued.body
    );
    assert_eq!(queued.body["status"], "queued");
    let id = queued.body["id"].as_str().unwrap().to_owned();
    reports::step(&app.state, now_ms()).await.unwrap();
    let report = browser.get(&format!("/v1/reports/{id}")).await;
    assert_eq!(report.status, StatusCode::OK, "{:?}", report.body);
    assert_eq!(
        report.body["status"], "succeeded",
        "{}: {:?}",
        app.name, report.body
    );
    id
}

async fn document(app: &App, id: &str) -> Document {
    reports::document(&app.state, id).await.unwrap().unwrap()
}

/// The figures of a section, by label.
fn figures(document: &Document, section: &str) -> HashMap<String, String> {
    document
        .sections
        .iter()
        .find(|candidate| candidate.id == section)
        .unwrap_or_else(|| panic!("no section {section}"))
        .figures
        .iter()
        .map(|figure| (figure.label.clone(), figure.value.clone()))
        .collect()
}

/// A table's rows as its CSV writes them.
fn rows(document: &Document, table: &str) -> Vec<Vec<String>> {
    let zone = meshrmm_server::reports::html::zone_of(document);
    let table = document
        .sections
        .iter()
        .flat_map(|section| &section.tables)
        .find(|candidate| candidate.id == table)
        .unwrap_or_else(|| panic!("no table {table}"));
    table
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(&table.columns)
                .map(|(cell, column)| cell.csv(column.kind, zone))
                .collect()
        })
        .collect()
}

fn strings(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
        .collect()
}

fn day(text: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

/// The UTC date `days` ago, as `YYYY-MM-DD`.
fn days_ago(days: i64) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms() - days * DAY_MS)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_executive_summary_counts_the_fleet() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll_on(&mut admin, "Desk", "windows").await;
        let laptop = app.enroll_on(&mut admin, "Laptop", "macos").await;
        let server = app.enroll_on(&mut admin, "Server", "linux").await;
        let old = app.enroll_on(&mut admin, "Old", "windows").await;
        let now = now_ms();
        // The period: the last 10 days, through today.
        let start = day(&days_ago(10));
        let (from, to) = (days_ago(10), days_ago(0));
        for device in [&desk, &laptop, &server, &old] {
            backdate_enrollment(&app, &device.device_id, start - 50 * DAY_MS).await;
        }
        backdate_enrollment(&app, &server.device_id, start + DAY_MS).await;
        post_snapshot(&desk, &snapshot(("Windows 11 Pro", "24H2"), dell(), vec![])).await;
        post_snapshot(&old, &snapshot(("Windows 11 Pro", "24H2"), dell(), vec![])).await;
        post_snapshot(&laptop, &snapshot(("macOS", "15.6"), dell(), vec![])).await;
        // Old was last heard from 40 days ago; Laptop's snapshot is from
        // now, and Desk sent a reading a minute ago.
        app.db()
            .execute(
                &Query::update()
                    .table(Alias::new("device_system_info"))
                    .value(Alias::new("received_at"), now - 40 * DAY_MS)
                    .and_where(sea_query::ExprTrait::eq(
                        sea_query::Expr::col(Alias::new("device_id")),
                        old.device_id.as_str(),
                    ))
                    .to_owned(),
            )
            .await
            .unwrap();
        minute(&app, &desk.device_id, now - MINUTE_MS, 100, 1000).await;

        // Alerts: two raised and resolved in the period, one raised and
        // still open, one raised before the period and resolved in it.
        let offline = create_rule(&mut admin, "Offline", "critical").await;
        alert(
            &app,
            &offline,
            &desk.device_id,
            start + HOUR_MS,
            Some(30 * MINUTE_MS),
        )
        .await;
        alert(
            &app,
            &offline,
            &desk.device_id,
            start + 2 * HOUR_MS,
            Some(90 * MINUTE_MS),
        )
        .await;
        alert(&app, &offline, &laptop.device_id, start + 3 * HOUR_MS, None).await;
        alert(
            &app,
            &offline,
            &laptop.device_id,
            start - DAY_MS,
            Some(DAY_MS + HOUR_MS),
        )
        .await;

        // Updates: Desk fully patched, Laptop with a security update and a
        // restart pending, Server with only a driver, Old never scanned.
        store_updates(
            &app,
            &desk.device_id,
            Some("windows_update"),
            now,
            vec![],
            false,
        )
        .await;
        store_updates(
            &app,
            &laptop.device_id,
            Some("softwareupdate"),
            now,
            vec![update("s1", "security", None)],
            true,
        )
        .await;
        store_updates(
            &app,
            &server.device_id,
            Some("apt"),
            now,
            vec![update("d1", "driver", None)],
            false,
        )
        .await;
        install(&app, &laptop.device_id, start + HOUR_MS, "failed", 1).await;
        install(&app, &laptop.device_id, start - 2 * DAY_MS, "failed", 1).await;
        install(&app, &desk.device_id, start + HOUR_MS, "succeeded", 0).await;

        // Applications: Firefox outdated on two devices, a failed install
        // and a failed update in the period.
        store_packages(
            &app,
            &desk.device_id,
            vec![package("Mozilla.Firefox", "Firefox", "120", Some("121"))],
        )
        .await;
        store_packages(
            &app,
            &laptop.device_id,
            vec![
                package("Mozilla.Firefox", "Firefox", "119", Some("121")),
                package("Git.Git", "Git", "2.4", None),
            ],
        )
        .await;
        failed_application_run(&app, &desk.device_id, "install", start + HOUR_MS).await;
        failed_application_run(&app, &desk.device_id, "update", start + HOUR_MS).await;
        failed_application_run(&app, &desk.device_id, "update", start - DAY_MS).await;

        // Jobs: one by a user that failed on one device, two runs of a
        // policy, and one before the period.
        job(
            &app,
            start + HOUR_MS,
            None,
            &[
                (&desk.device_id, "started", Some("succeeded")),
                (&laptop.device_id, "started", Some("failed")),
            ],
        )
        .await;
        job(
            &app,
            start + 2 * HOUR_MS,
            Some("Nightly"),
            &[
                (&desk.device_id, "started", Some("succeeded")),
                (&server.device_id, "missed", None),
            ],
        )
        .await;
        job(
            &app,
            start + 3 * HOUR_MS,
            Some("Nightly"),
            &[(&desk.device_id, "started", Some("timed_out"))],
        )
        .await;
        job(
            &app,
            start - DAY_MS,
            None,
            &[(&desk.device_id, "started", Some("failed"))],
        )
        .await;

        let id = run(&app, &mut admin, request("executive_summary", &from, &to)).await;
        let report = document(&app, &id).await;
        assert_eq!(report.instance_name, "Acme IT", "{name}");
        assert_eq!(report.scope_label, "Every device");
        assert_eq!(report.period_start, start, "{name}");

        let devices = figures(&report, "devices");
        assert_eq!(devices["Devices"], "4", "{name}");
        assert_eq!(devices["Windows"], "2");
        assert_eq!(devices["macOS"], "1");
        assert_eq!(devices["Linux"], "1");
        assert_eq!(devices["Enrolled in the period"], "1");
        assert_eq!(
            rows(&report, "devices-by-os"),
            strings(&[
                &["Windows 11 Pro", "24H2", "2"],
                &["macOS", "15.6", "1"],
                &["No inventory yet", "", "1"],
            ]),
            "{name}"
        );

        let online = figures(&report, "online");
        assert_eq!(online["Online now"], "0 (0%)");
        assert_eq!(online["Not seen in 30 days"], "1", "{name}");
        let stale = rows(&report, "not-seen");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0][0], "Old");

        let alerts = figures(&report, "alerts");
        assert_eq!(alerts["Raised"], "3", "{name}");
        assert_eq!(alerts["Resolved"], "3", "{name}");
        assert_eq!(alerts["Open now"], "1");
        // (30 min + 90 min + 25 h) / 3.
        assert_eq!(
            alerts["Mean time to resolve"],
            "9 h 0 min".replace(" 0 min", ""),
            "{name}"
        );
        assert_eq!(
            rows(&report, "alerts-by-kind"),
            strings(&[&["Device offline", "3", "3", "9.00"]]),
            "{name}"
        );

        let patches = figures(&report, "patches");
        assert_eq!(patches["Fully patched"], "1 (33.3%)", "{name}");
        assert_eq!(patches["Security updates pending"], "1");
        assert_eq!(patches["Failed installs"], "1", "{name}");
        assert_eq!(patches["Restarts pending"], "1");
        assert_eq!(
            rows(&report, "patch-posture"),
            strings(&[
                &["Fully patched", "1"],
                &["Security or critical updates pending", "1"],
                &["Only other updates pending", "1"],
                &["Restart pending", "1"],
                &["An install failed in the period", "1"],
                &["Not scanned yet", "1"],
                &["No update manager MeshRMM supports", "0"],
            ]),
            "{name}"
        );

        let applications = figures(&report, "applications");
        assert_eq!(
            applications["Devices with outdated applications"], "2 (50%)",
            "{name}"
        );
        assert_eq!(applications["Failed installs"], "1");
        assert_eq!(applications["Failed updates"], "1", "{name}");
        assert_eq!(
            rows(&report, "outdated-top"),
            strings(&[&["Firefox", "Mozilla.Firefox", "winget", "121", "2"]]),
            "{name}"
        );

        let jobs = figures(&report, "jobs");
        assert_eq!(jobs["Jobs started by people"], "1", "{name}");
        assert_eq!(jobs["Policy runs"], "2");
        assert_eq!(jobs["Device runs succeeded"], "2");
        assert_eq!(jobs["Device runs failed"], "2", "{name}");
        assert_eq!(
            rows(&report, "jobs"),
            strings(&[
                &["People", "1", "1", "1", "1", "0"],
                &["Policies", "2", "1", "1", "1", "1"]
            ]),
            "{name}"
        );
        assert_eq!(
            rows(&report, "policy-runs"),
            strings(&[&["Nightly", "2", "1", "1", "1", "1"]])
        );

        let top = rows(&report, "top-devices");
        assert_eq!(top.len(), 2, "{name}");
        assert_eq!(top[0][..4], ["Desk", "", "2", "0"]);
        assert_eq!(top[1][..4], ["Laptop", "", "1", "1"]);
        app.finish().await;
    }
}

#[tokio::test]
async fn patch_compliance_judges_each_device() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let now = now_ms();
        let compliant = app.enroll(&mut admin, "A compliant").await;
        let pending = app.enroll(&mut admin, "B pending").await;
        let stale = app.enroll(&mut admin, "C stale").await;
        let never = app.enroll(&mut admin, "D never").await;
        let unsupported = app.enroll_on(&mut admin, "E unsupported", "linux").await;
        store_updates(
            &app,
            &compliant.device_id,
            Some("windows_update"),
            now,
            vec![update("d1", "driver", Some("low"))],
            true,
        )
        .await;
        store_updates(
            &app,
            &pending.device_id,
            Some("windows_update"),
            now,
            vec![
                update("s1", "security", Some("critical")),
                update("s2", "security", Some("important")),
                update("c1", "critical", None),
                update("f1", "feature_pack", Some("moderate")),
            ],
            false,
        )
        .await;
        store_updates(
            &app,
            &stale.device_id,
            Some("windows_update"),
            now - 10 * DAY_MS,
            vec![],
            false,
        )
        .await;
        store_updates(&app, &unsupported.device_id, None, now, vec![], false).await;
        let start = day(&days_ago(3));
        install(&app, &pending.device_id, start + HOUR_MS, "partial", 2).await;
        install(&app, &pending.device_id, start + 2 * HOUR_MS, "failed", 1).await;
        install(&app, &compliant.device_id, start + HOUR_MS, "succeeded", 0).await;
        let _ = never;

        let id = run(
            &app,
            &mut admin,
            request("patch_compliance", &days_ago(3), &days_ago(0)),
        )
        .await;
        let report = document(&app, &id).await;
        let summary = figures(&report, "compliance");
        assert_eq!(
            summary["Compliance"], "25%",
            "{name}: one of four devices that can be patched"
        );
        assert_eq!(summary["Security or critical updates pending"], "1");
        assert_eq!(summary["Not scanned in 7 days"], "2", "{name}");
        assert_eq!(summary["Restarts pending"], "1");
        assert_eq!(summary["Failed installs"], "2", "{name}");
        assert_eq!(
            rows(&report, "compliance"),
            strings(&[
                &["Compliant", "1"],
                &["Updates pending", "1"],
                &["Scan out of date", "1"],
                &["Not scanned", "1"],
                &["Unsupported", "1"],
            ])
        );
        assert_eq!(
            rows(&report, "pending-by-category"),
            strings(&[
                &["Security", "2", "1"],
                &["Critical", "1", "1"],
                &["Drivers", "1", "1"],
                &["Feature packs", "1", "1"],
            ]),
            "{name}"
        );
        assert_eq!(
            rows(&report, "pending-by-severity"),
            strings(&[
                &["Critical", "1", "1"],
                &["Important", "1", "1"],
                &["Moderate", "1", "1"],
                &["Low", "1", "1"],
                &["Unrated", "1", "1"],
            ]),
            "{name}"
        );
        let devices = rows(&report, "devices");
        let by_name: HashMap<&str, &Vec<String>> =
            devices.iter().map(|row| (row[0].as_str(), row)).collect();
        let row = by_name["B pending"];
        assert_eq!(row[2], "Updates pending");
        assert_eq!(row[4], "Windows Update");
        assert_eq!(
            row[5..14],
            ["4", "2", "1", "1", "1", "1", "1", "0", "2"],
            "{name}"
        );
        assert_eq!(row[14], "No");
        assert_eq!(by_name["A compliant"][2], "Compliant");
        assert!(
            by_name["A compliant"][14].starts_with("Since "),
            "{:?}",
            by_name["A compliant"]
        );
        assert_eq!(by_name["C stale"][2], "Scan out of date");
        assert_eq!(by_name["D never"][2], "Not scanned");
        assert_eq!(by_name["D never"][3], "");
        assert_eq!(by_name["E unsupported"][2], "Unsupported");
        app.finish().await;
    }
}

#[tokio::test]
async fn the_inventories_list_hardware_and_software() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let acme = add_company(&mut admin, "Acme").await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        app.enroll(&mut admin, "Bare").await;
        move_to(&mut admin, &desk.device_id, &acme).await;
        post_snapshot(
            &desk,
            &snapshot(
                ("Windows 11 Pro", "24H2"),
                dell(),
                vec![
                    application("Firefox", "121.0"),
                    application("7-Zip", "23.01"),
                ],
            ),
        )
        .await;
        post_snapshot(
            &laptop,
            &snapshot(
                ("Windows 11 Pro", "24H2"),
                json!({ "manufacturer": "Lenovo", "model": "T14", "memory_bytes": 8_000_000_000_i64 }),
                vec![application("Firefox", "120.0")],
            ),
        )
        .await;
        minute(
            &app,
            &desk.device_id,
            now_ms() - 2 * MINUTE_MS,
            200_000_000_000,
            500_000_000_000,
        )
        .await;
        store_packages(
            &app,
            &laptop.device_id,
            vec![package(
                "Mozilla.Firefox",
                "Firefox",
                "120.0",
                Some("121.0"),
            )],
        )
        .await;

        let id = run(
            &app,
            &mut admin,
            request("hardware_inventory", &days_ago(1), &days_ago(0)),
        )
        .await;
        let report = document(&app, &id).await;
        assert_eq!(report.period_label, "As the devices are now");
        let summary = figures(&report, "summary");
        assert_eq!(summary["Devices"], "3", "{name}");
        assert_eq!(summary["With an inventory"], "2");
        assert_eq!(summary["Memory in all"], "25.2 GB");
        assert_eq!(summary["Disk capacity in all"], "512 GB");
        assert_eq!(
            rows(&report, "models"),
            strings(&[
                &["Dell Inc.", "OptiPlex 7090", "1"],
                &["Lenovo", "T14", "1"]
            ]),
            "{name}"
        );
        let devices = rows(&report, "devices");
        assert_eq!(devices.len(), 3);
        let desk_row = devices.iter().find(|row| row[0] == "Desk").unwrap();
        assert_eq!(
            desk_row[..13],
            [
                "Desk",
                "Acme",
                "Windows",
                "Dell Inc.",
                "OptiPlex 7090",
                "SN-1",
                "Intel(R) Core(TM) i7",
                "8 / 16",
                "17.18",
                "Samsung SSD 980 (512 GB)",
                "300.00",
                "500.00",
                "Windows 11 Pro 24H2 (26100.4652)",
            ],
            "{name}"
        );
        let bare_row = devices.iter().find(|row| row[0] == "Bare").unwrap();
        assert_eq!(bare_row[3..13], ["", "", "", "", "", "", "", "", "", ""]);

        let id = run(
            &app,
            &mut admin,
            request("software_inventory", &days_ago(1), &days_ago(0)),
        )
        .await;
        let report = document(&app, &id).await;
        assert_eq!(
            rows(&report, "applications"),
            strings(&[&["Firefox", "2", "2"], &["7-Zip", "1", "1"]]),
            "{name}"
        );
        assert_eq!(
            rows(&report, "versions"),
            strings(&[
                &["7-Zip", "23.01", "Example Corp", "1"],
                &["Firefox", "120.0", "Example Corp", "1"],
                &["Firefox", "121.0", "Example Corp", "1"],
            ])
        );
        assert_eq!(
            rows(&report, "outdated"),
            strings(&[&["Firefox", "Mozilla.Firefox", "winget", "121.0", "1"]])
        );
        assert_eq!(
            figures(&report, "summary")["Devices with an inventory"],
            "2"
        );

        // On Acme's devices only.
        let mut body = request("software_inventory", &days_ago(1), &days_ago(0));
        body["target"] = "companies".into();
        body["company_ids"] = json!([acme]);
        let id = run(&app, &mut admin, body).await;
        let report = document(&app, &id).await;
        assert_eq!(report.scope_label, "Acme");
        assert_eq!(
            rows(&report, "applications"),
            strings(&[&["7-Zip", "1", "1"], &["Firefox", "1", "1"]]),
            "{name}"
        );
        assert!(rows(&report, "outdated").is_empty());
        app.finish().await;
    }
}

#[tokio::test]
async fn the_alerts_report_groups_by_kind_rule_and_device() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let critical = create_rule(&mut admin, "Offline long", "critical").await;
        let warning = create_rule(&mut admin, "Offline short", "warning").await;
        let start = day(&days_ago(5));
        alert(
            &app,
            &critical,
            &desk.device_id,
            start + HOUR_MS,
            Some(2 * HOUR_MS),
        )
        .await;
        alert(
            &app,
            &critical,
            &laptop.device_id,
            start + 2 * HOUR_MS,
            Some(4 * HOUR_MS),
        )
        .await;
        alert(&app, &warning, &desk.device_id, start + 3 * HOUR_MS, None).await;
        // Before the period, and so not counted.
        alert(
            &app,
            &warning,
            &desk.device_id,
            start - HOUR_MS,
            Some(HOUR_MS),
        )
        .await;

        let id = run(
            &app,
            &mut admin,
            request("alerts", &days_ago(5), &days_ago(0)),
        )
        .await;
        let report = document(&app, &id).await;
        let summary = figures(&report, "summary");
        assert_eq!(summary["Raised"], "3", "{name}");
        assert_eq!(summary["Critical"], "2");
        assert_eq!(summary["Still open"], "1");
        assert_eq!(summary["Resolved"], "2");
        assert_eq!(summary["Mean time to resolve"], "3 h");
        assert_eq!(
            summary["Resolved in the period"], "3",
            "{name}: whenever raised"
        );
        assert_eq!(
            rows(&report, "by-kind"),
            strings(&[&["Device offline", "3", "1", "2", "3.00", "4.00"]])
        );
        assert_eq!(
            rows(&report, "by-rule"),
            strings(&[
                &[
                    "Offline long",
                    "Critical",
                    "Device offline",
                    "2",
                    "0",
                    "2",
                    "3.00",
                    "4.00"
                ],
                &[
                    "Offline short",
                    "Warning",
                    "Device offline",
                    "1",
                    "1",
                    "0",
                    "",
                    ""
                ],
            ]),
            "{name}"
        );
        assert_eq!(
            rows(&report, "by-device"),
            strings(&[
                &["Desk", "", "2", "1", "1", "2.00", "2.00"],
                &["Laptop", "", "1", "0", "1", "4.00", "4.00"],
            ])
        );
        let listed = rows(&report, "alerts");
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0][1], "Offline short", "newest first");
        assert_eq!(listed[0][5], "Open");
        assert_eq!(listed[2][7], "2.00");
        assert_eq!(listed[2][8], "Cleared by itself");
        app.finish().await;
    }
}

#[tokio::test]
async fn scopes_take_companies_and_tags() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let acme = add_company(&mut admin, "Acme").await;
        let globex = add_company(&mut admin, "Globex").await;
        let one = app.enroll(&mut admin, "One").await;
        let two = app.enroll(&mut admin, "Two").await;
        let three = app.enroll(&mut admin, "Three").await;
        move_to(&mut admin, &one.device_id, &acme).await;
        move_to(&mut admin, &two.device_id, &globex).await;
        let servers = add_tag(&mut admin, "servers", &[&one.device_id, &three.device_id]).await;
        let london = add_tag(&mut admin, "london", &[&three.device_id]).await;
        let rule = create_rule(&mut admin, "Offline", "warning").await;
        let start = day(&days_ago(2));
        for device in [&one, &two, &three] {
            alert(&app, &rule, &device.device_id, start + HOUR_MS, None).await;
        }
        let summary = |target: Value| {
            let mut body = request("executive_summary", &days_ago(2), &days_ago(0));
            for (key, value) in target.as_object().unwrap() {
                body[key] = value.clone();
            }
            body
        };
        let cases = [
            (json!({ "target": "all" }), "Every device", "3"),
            (
                json!({ "target": "companies", "company_ids": [acme, globex] }),
                "Acme and Globex",
                "2",
            ),
            (
                json!({ "target": "companies", "company_ids": [globex] }),
                "Globex",
                "1",
            ),
            (
                json!({ "target": "tags", "tag_ids": [servers, london], "tag_match": "any" }),
                "Tagged london or servers",
                "2",
            ),
            (
                json!({ "target": "tags", "tag_ids": [servers, london], "tag_match": "all" }),
                "Tagged london and servers",
                "1",
            ),
        ];
        for (target, label, devices) in cases {
            let id = run(&app, &mut admin, summary(target)).await;
            let report = document(&app, &id).await;
            assert_eq!(report.scope_label, label, "{name}");
            assert_eq!(
                figures(&report, "devices")["Devices"],
                devices,
                "{name}: {label}"
            );
            assert_eq!(
                figures(&report, "alerts")["Raised"],
                devices,
                "{name}: {label}"
            );
        }
        // Devices and outdated packages are for jobs; unknown companies
        // aren't anyone's.
        let refused = admin
            .post(
                "/v1/reports",
                summary(json!({ "target": "devices", "device_ids": [one.device_id] })),
            )
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}");
        let refused = admin
            .post(
                "/v1/reports",
                summary(json!({ "target": "companies", "company_ids": [uuid()] })),
            )
            .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn periods_are_judged_in_their_time_zone() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let rule = create_rule(&mut admin, "Offline", "warning").await;
        // 23:30 UTC three days ago: the next day in Tokyo, the same in Los
        // Angeles.
        let raised = day(&days_ago(3)) + 23 * HOUR_MS + 30 * MINUTE_MS;
        alert(&app, &rule, &desk.device_id, raised, None).await;
        let on = |zone: &str| {
            json!({
                "template": "alerts", "target": "all", "period": "custom",
                "from": days_ago(3), "to": days_ago(3), "time_zone": zone,
            })
        };
        for (zone, raised_count, offset_hours) in [
            ("UTC", "1", 0),
            ("Asia/Tokyo", "0", -9),
            ("America/Los_Angeles", "1", 0),
        ] {
            let id = run(&app, &mut admin, on(zone)).await;
            let report = document(&app, &id).await;
            assert_eq!(
                figures(&report, "summary")["Raised"],
                raised_count,
                "{name}: {zone}"
            );
            assert_eq!(report.time_zone, zone);
            if offset_hours != 0 {
                assert_eq!(
                    report.period_start,
                    day(&days_ago(3)) + offset_hours * HOUR_MS
                );
                assert_eq!(report.period_end - report.period_start, DAY_MS);
            }
            let view = admin.get(&format!("/v1/reports/{id}")).await.body;
            assert_eq!(view["period_start"], report.period_start);
            assert_eq!(view["time_zone"], zone);
        }
        // Rolling periods end at today's midnight in the zone.
        let week = admin
            .post(
                "/v1/reports",
                json!({ "template": "alerts", "target": "all", "period": "last_7_days", "time_zone": "UTC" }),
            )
            .await;
        assert_eq!(week.status, StatusCode::ACCEPTED);
        assert_eq!(week.body["period_end"], day(&days_ago(0)), "{name}");
        assert_eq!(week.body["period_start"], day(&days_ago(7)));
        for (body, code) in [
            (
                json!({ "period": "custom", "from": "2026-02-30", "to": "2026-03-01" }),
                "invalid_report",
            ),
            (
                json!({ "period": "custom", "from": days_ago(-2), "to": days_ago(-2) }),
                "invalid_report",
            ),
            (
                json!({ "period": "custom", "from": days_ago(0) }),
                "invalid_report",
            ),
            (
                json!({ "period": "last_7_days", "time_zone": "Mars/Olympus" }),
                "invalid_report",
            ),
            (
                json!({ "period": "last_7_days", "stale_days": 0 }),
                "invalid_report",
            ),
        ] {
            let mut request = json!({ "template": "alerts", "target": "all", "time_zone": "UTC" });
            for (key, value) in body.as_object().unwrap() {
                request[key] = value.clone();
            }
            let refused = admin.post("/v1/reports", request).await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}: {body}");
            assert_eq!(refused.code(), code);
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn reports_are_html_pages_with_csv_tables() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk <one>").await;
        post_snapshot(&desk, &snapshot(("Windows 11 Pro", "24H2"), dell(), vec![])).await;
        let id = run(
            &app,
            &mut admin,
            request("hardware_inventory", &days_ago(1), &days_ago(0)),
        )
        .await;
        let page = admin
            .bytes(
                admin
                    .request(Method::GET, &format!("/v1/reports/{id}/html"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(page.status, StatusCode::OK, "{name}");
        assert_eq!(page.header("content-type"), "text/html; charset=utf-8");
        assert!(
            page.header("content-security-policy")
                .starts_with("default-src 'none'")
        );
        assert!(page.header("content-disposition").starts_with("inline"));
        let html = String::from_utf8(page.body.to_vec()).unwrap();
        assert!(html.contains("<p class=\"instance\">Acme IT</p>"), "{name}");
        assert!(html.contains("<h1>Hardware asset inventory</h1>"));
        assert!(html.contains("<dd>Every device</dd>"));
        assert!(html.contains("Desk &lt;one&gt;"));
        assert!(html.contains("Download CSV"));
        let download = admin
            .bytes(
                admin
                    .request(Method::GET, &format!("/v1/reports/{id}/html?download"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert!(
            download
                .header("content-disposition")
                .starts_with("attachment; filename=\"hardware-inventory-")
        );
        let csv = admin
            .bytes(
                admin
                    .request(Method::GET, &format!("/v1/reports/{id}/tables/devices.csv"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(csv.status, StatusCode::OK, "{name}");
        assert_eq!(csv.header("content-type"), "text/csv; charset=utf-8");
        let csv = String::from_utf8(csv.body.to_vec()).unwrap();
        assert!(
            csv.starts_with("Device,Company,Platform,Manufacturer,Model,Serial number,"),
            "{csv}"
        );
        assert!(
            csv.contains("\r\nDesk <one>,,Windows,Dell Inc.,OptiPlex 7090,SN-1,"),
            "{csv}"
        );
        let missing = admin.get(&format!("/v1/reports/{id}/tables/nothing")).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let view = admin.get(&format!("/v1/reports/{id}")).await.body;
        assert_eq!(view["tables"][0]["id"], "models", "{name}");
        assert_eq!(view["tables"][2]["rows"], 1);

        // A report that isn't made yet has no page.
        let queued = admin
            .post("/v1/reports", request("alerts", &days_ago(1), &days_ago(0)))
            .await;
        let queued_id = queued.body["id"].as_str().unwrap();
        let early = admin.get(&format!("/v1/reports/{queued_id}/html")).await;
        assert_eq!(early.status, StatusCode::CONFLICT);
        assert_eq!(early.code(), "report_not_ready");
        let listed = admin.get("/v1/reports").await.body;
        assert_eq!(listed["reports"][0]["id"], queued_id, "newest first");
        assert_eq!(listed["reports"][0]["status"], "queued");
        assert_eq!(listed["reports"][1]["requested_by_you"], true);

        // Making it only reads: its claim and outcome are its only writes.
        let before = app.db().counts();
        let stepped = reports::step(&app.state, now_ms()).await.unwrap();
        assert_eq!(stepped.made, 1);
        let after = app.db().counts();
        assert_eq!(after.writes - before.writes, 2, "{name}");
        assert_eq!(after.transactions - before.transactions, 0, "{name}");
        app.finish().await;
    }
}

/// A schedule's body, with `changes` over a weekly executive summary on
/// Mondays at 07:00 in London.
fn schedule(changes: Value) -> Value {
    let mut body = json!({
        "name": "Weekly summary", "template": "executive_summary", "target": "all",
        "period": "last_7_days", "frequency": "weekly", "weekday": 1, "time": "07:00",
        "time_zone": "Europe/London",
    });
    for (key, value) in changes.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

/// The report the schedule's latest run made.
async fn latest_report(browser: &mut Browser, schedule_id: &str) -> Value {
    let listed = browser
        .get(&format!("/v1/reports?schedule_id={schedule_id}"))
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    listed.body["reports"][0].clone()
}

#[tokio::test]
async fn schedules_fire_on_time_and_email_what_they_make() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        app.enroll(&mut admin, "Desk").await;
        let (_, reader_id) =
            user_with(&app, &mut admin, "reader@example.com", &["reports.view"]).await;
        // Before email is set up, which sends invitations instead of linking them.
        let role = common::add_role(
            &mut admin,
            "Report managers",
            &["reports.view", "reports.manage"],
        )
        .await;
        let (mut manager, _) =
            common::add_user(&app, &mut admin, "manager@example.com", &[&role]).await;

        // Without email, reports are made and kept, and say so.
        let created = admin
            .post(
                "/v1/report-schedules",
                schedule(json!({ "user_ids": [reader_id], "emails": ["Customer@Example.com"] })),
            )
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["emails"], json!(["customer@example.com"]));
        assert_eq!(created.body["owned_by_you"], true);
        let next = created.body["next_run_at"].as_i64().unwrap();
        let local = chrono::DateTime::from_timestamp_millis(next)
            .unwrap()
            .with_timezone(&chrono_tz::Europe::London);
        assert_eq!(local.format("%a %H:%M").to_string(), "Mon 07:00", "{name}");
        assert!(next > now_ms() && next <= now_ms() + 7 * DAY_MS);
        let templates = admin.get("/v1/report-templates").await.body;
        assert_eq!(templates["email_configured"], false);
        assert_eq!(templates["templates"].as_array().unwrap().len(), 5);

        // Not due yet.
        assert_eq!(reports::step(&app.state, next - 1).await.unwrap().fired, 0);
        let stepped = reports::step(&app.state, next).await.unwrap();
        assert_eq!(
            (stepped.fired, stepped.made, stepped.delivered),
            (1, 1, 1),
            "{name}"
        );
        let report = latest_report(&mut admin, &id).await;
        assert_eq!(report["trigger"], "schedule");
        assert_eq!(report["status"], "succeeded");
        assert_eq!(report["schedule_name"], "Weekly summary");
        assert_eq!(report["requested_by"], "Schedule “Weekly summary”");
        assert_eq!(report["delivery"], "not_configured", "{name}");
        assert_eq!(report["period_end"], {
            let monday = local.date_naive();
            reports::period::midnight(chrono_tz::Europe::London, monday)
        });
        // It fires once, and next a week later.
        assert_eq!(reports::step(&app.state, next).await.unwrap().fired, 0);
        let saved = &admin.get("/v1/report-schedules").await.body["schedules"][0];
        assert_eq!(saved["next_run_at"].as_i64().unwrap() - next, 7 * DAY_MS);
        assert_eq!(saved["last_report_id"], report["id"]);

        // With email, each recipient gets their own message, with the page
        // attached.
        let mut smtp = FakeSmtp::start().await;
        let configured = admin
            .put(
                "/v1/settings/smtp",
                json!({ "host": "127.0.0.1", "port": smtp.port, "security": "none", "from": "Acme IT <rmm@example.com>" }),
            )
            .await;
        assert_eq!(configured.status, StatusCode::OK, "{:?}", configured.body);
        assert_eq!(
            admin.get("/v1/report-templates").await.body["email_configured"],
            true
        );
        let ran = admin
            .post(&format!("/v1/report-schedules/{id}/run"), json!({}))
            .await;
        assert_eq!(ran.status, StatusCode::ACCEPTED, "{name}: {:?}", ran.body);
        assert_eq!(ran.body["trigger"], "run_now");
        assert_eq!(ran.body["requested_by"], common::ADMIN_EMAIL);
        let stepped = reports::step(&app.state, now_ms()).await.unwrap();
        assert_eq!((stepped.made, stepped.delivered), (1, 1));
        let mut recipients = Vec::new();
        for _ in 0..2 {
            let message = smtp.next().await;
            let to = message
                .lines()
                .find_map(|line| line.strip_prefix("To: "))
                .unwrap()
                .to_owned();
            assert!(
                message.contains("Subject: Executive summary: Every device,"),
                "{message}"
            );
            assert!(
                message.contains("Content-Type: multipart/mixed"),
                "{message}"
            );
            assert!(message.contains("Content-Type: multipart/alternative"));
            // Lettre folds the name as `filename*0="…"` (RFC 2231).
            assert!(
                message.contains("Content-Disposition: attachment"),
                "{message}"
            );
            assert!(
                message.contains("\"executive-summary-") && message.contains(".html\""),
                "{message}"
            );
            assert!(message.contains(&format!(
                "https://rmm.example.com/reports?report={}",
                ran.body["id"].as_str().unwrap()
            )));
            recipients.push(to);
        }
        recipients.sort();
        assert_eq!(
            recipients,
            ["customer@example.com", "reader@example.com"],
            "{name}"
        );
        // By ID: the scheduled report was made at its (later) run time.
        let report = admin
            .get(&format!("/v1/reports/{}", ran.body["id"].as_str().unwrap()))
            .await
            .body;
        assert_eq!(report["delivery"], "sent", "{name}");
        assert_eq!(report["delivery_detail"], "Sent to 2 recipients.");
        // Running it now leaves its schedule as it was.
        let saved = &admin.get("/v1/report-schedules").await.body["schedules"][0];
        assert_eq!(saved["next_run_at"].as_i64().unwrap() - next, 7 * DAY_MS);

        // Turned off, it doesn't fire; turned on, it counts from then.
        let off = admin
            .put(
                &format!("/v1/report-schedules/{id}"),
                schedule(json!({ "enabled": false })),
            )
            .await;
        assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
        assert_eq!(off.body["next_run_at"], Value::Null);
        assert_eq!(
            reports::step(&app.state, next + 14 * DAY_MS)
                .await
                .unwrap()
                .fired,
            0
        );

        // A schedule whose owner lost the permission records why it made
        // nothing.
        let monthly = manager
            .post(
                "/v1/report-schedules",
                schedule(json!({ "name": "Monthly patches", "template": "patch_compliance", "period": "previous_month", "frequency": "monthly", "day_of_month": 1, "weekday": null })),
            )
            .await;
        assert_eq!(
            monthly.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            monthly.body
        );
        let monthly_id = monthly.body["id"].as_str().unwrap().to_owned();
        let at = monthly.body["next_run_at"].as_i64().unwrap();
        let changed = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["reports.view"] }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(reports::step(&app.state, at).await.unwrap().fired, 1);
        let failed = latest_report(&mut admin, &monthly_id).await;
        assert_eq!(failed["status"], "failed", "{name}");
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("no longer has the reports.view and reports.manage permissions")
        );
        assert_eq!(failed["period"], "previous_month");
        app.finish().await;
    }
}

#[tokio::test]
async fn reports_expire_with_their_documents() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let kept = run(
            &app,
            &mut admin,
            request("alerts", &days_ago(1), &days_ago(0)),
        )
        .await;
        let expiring = run(
            &app,
            &mut admin,
            request("alerts", &days_ago(1), &days_ago(0)),
        )
        .await;
        app.db()
            .execute(
                &Query::update()
                    .table(Alias::new("reports"))
                    .value(Alias::new("expires_at"), now_ms() - 1)
                    .and_where(sea_query::ExprTrait::eq(
                        sea_query::Expr::col(Alias::new("id")),
                        expiring.as_str(),
                    ))
                    .to_owned(),
            )
            .await
            .unwrap();
        // An expired report is gone from the list at once.
        let listed = admin.get("/v1/reports").await.body;
        assert_eq!(listed["reports"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(
            admin.get(&format!("/v1/reports/{expiring}")).await.status,
            StatusCode::NOT_FOUND
        );
        let purged = maintenance::purge(app.db(), now_ms()).await;
        assert_eq!(purged.rows("reports"), 1, "{name}");
        assert!(app.state.storage.report(&expiring).exists());
        maintenance::remove_orphans(app.db(), &app.state.storage)
            .await
            .unwrap();
        assert!(!app.state.storage.report(&expiring).exists());
        assert!(app.state.storage.report(&kept).exists());
        // Reports last 90 days.
        let view = admin.get(&format!("/v1/reports/{kept}")).await.body;
        assert_eq!(
            view["expires_at"].as_i64().unwrap() - view["created_at"].as_i64().unwrap(),
            RETENTION_MS
        );
        let deleted = admin.delete(&format!("/v1/reports/{kept}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert!(!app.state.storage.report(&kept).exists());
        app.finish().await;
    }
}

#[tokio::test]
async fn reports_need_their_permissions_and_are_audited() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut nobody, _) =
            user_with(&app, &mut admin, "devices@example.com", &["devices.view"]).await;
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["reports.view"]).await;
        let body = request("executive_summary", &days_ago(1), &days_ago(0));
        for (path, method) in [
            ("/v1/report-templates", Method::GET),
            ("/v1/reports", Method::GET),
            ("/v1/reports", Method::POST),
            ("/v1/report-schedules", Method::GET),
        ] {
            let response = nobody.send(method.clone(), path, Some(body.clone())).await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{name}: {method} {path}"
            );
        }
        let id = run(&app, &mut viewer, body.clone()).await;
        assert_eq!(
            nobody.get(&format!("/v1/reports/{id}/html")).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer.get("/v1/report-schedules").await.status,
            StatusCode::OK
        );
        for refused in [
            viewer
                .post("/v1/report-schedules", schedule(json!({})))
                .await,
            viewer.get("/v1/report-schedules/recipients").await,
            viewer.delete(&format!("/v1/reports/{id}")).await,
        ] {
            assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        }

        // API tokens reach reports.
        let token = admin
            .post("/v1/api-tokens", json!({ "name": "Reporting", "expires_in_days": 30, "permissions": ["reports.view"] }))
            .await;
        assert_eq!(token.status, StatusCode::CREATED, "{:?}", token.body);
        let secret = token.body["secret"].as_str().unwrap();
        let by_token = app
            .browser()
            .raw(
                axum::http::Request::builder()
                    .method(Method::POST)
                    .uri("/v1/reports")
                    .header("authorization", format!("Bearer {secret}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            by_token.status,
            StatusCode::ACCEPTED,
            "{name}: {:?}",
            by_token.body
        );

        // Schedule changes and every generation are in the audit log, with
        // who or what asked.
        let created = admin
            .post("/v1/report-schedules", schedule(json!({})))
            .await;
        let schedule_id = created.body["id"].as_str().unwrap().to_owned();
        let renamed = admin
            .put(
                &format!("/v1/report-schedules/{schedule_id}"),
                schedule(json!({ "name": "Renamed" })),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK);
        let duplicate = admin
            .post(
                "/v1/report-schedules",
                schedule(json!({ "name": "Renamed" })),
            )
            .await;
        assert_eq!(duplicate.code(), "name_taken");
        let next = renamed.body["next_run_at"].as_i64().unwrap();
        reports::step(&app.state, next).await.unwrap();
        assert_eq!(
            admin
                .post(
                    &format!("/v1/report-schedules/{schedule_id}/run"),
                    json!({})
                )
                .await
                .status,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/report-schedules/{schedule_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            admin.delete(&format!("/v1/reports/{id}")).await.status,
            StatusCode::NO_CONTENT
        );

        let generated = audit_events(&app, "report.generate").await;
        let triggers = generated
            .iter()
            .map(|event| {
                (
                    event.metadata["trigger"].as_str().unwrap().to_owned(),
                    event.actor_label.clone(),
                    event.api_token.is_some(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            triggers,
            [
                ("run_now".to_owned(), common::ADMIN_EMAIL.to_owned(), false),
                (
                    "schedule".to_owned(),
                    "Report schedule “Renamed”".to_owned(),
                    false
                ),
                ("manual".to_owned(), common::ADMIN_EMAIL.to_owned(), true),
                ("manual".to_owned(), "viewer@example.com".to_owned(), false),
            ],
            "{name}"
        );
        assert_eq!(generated[1].actor_user_id, None);
        assert_eq!(generated[1].metadata["schedule_id"], schedule_id.as_str());
        assert_eq!(generated[3].metadata["template"], "executive_summary");
        assert_eq!(generated[3].metadata["scope"], "Every device");
        for action in [
            "report_schedule.create",
            "report_schedule.update",
            "report_schedule.delete",
            "report.delete",
        ] {
            assert_eq!(
                audit_events(&app, action).await.len(),
                1,
                "{name}: {action}"
            );
        }
        assert_eq!(
            audit_events(&app, "report_schedule.update").await[0].metadata["name"],
            "Renamed"
        );
        app.finish().await;
    }
}
