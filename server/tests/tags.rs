//! Tags: managing them, tagging devices one at a time and in bulk, their
//! presence events, using them as the target of jobs, policies, alert rules
//! and software searches, and devices' notes.
mod common;

use axum::http::StatusCode;
use common::{App, Browser, apps, audit_events, receive_json, set_up, user_with};
use meshrmm_server::{
    alerts, automation,
    time::{MINUTE_MS, now_ms},
};
use serde_json::{Value, json};

async fn create_tag(admin: &mut Browser, name: &str, color: &str) -> String {
    let created = admin
        .post("/v1/tags", json!({ "name": name, "color": color }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn tag(browser: &mut Browser, devices: &[&str], add: &[&str], remove: &[&str]) -> Value {
    let changed = browser
        .post(
            "/v1/device-tags",
            json!({ "device_ids": devices, "add": add, "remove": remove }),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
    changed.body
}

/// Each device's tags, by device ID, as the device list shows them.
async fn tags_of(browser: &mut Browser, device_id: &str) -> Value {
    let listed = browser.get("/v1/agents").await.body;
    listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == device_id)
        .unwrap()["tag_ids"]
        .clone()
}

/// The names of the devices a job covers.
async fn job_devices(browser: &mut Browser, job_id: &str) -> Vec<String> {
    let job = browser.get(&format!("/v1/jobs/{job_id}")).await;
    assert_eq!(job.status, StatusCode::OK, "{:?}", job.body);
    job.body["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|device| device["device_name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn tags_are_created_renamed_recolored_and_deleted() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;

        let servers = admin
            .post("/v1/tags", json!({ "name": " Servers ", "color": "blue" }))
            .await;
        assert_eq!(
            servers.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            servers.body
        );
        assert_eq!(servers.body["name"], "Servers");
        assert_eq!(servers.body["color"], "blue");
        assert_eq!(servers.body["device_count"], 0);
        let servers = servers.body["id"].as_str().unwrap().to_owned();
        let london = create_tag(&mut admin, "london", "green").await;

        for invalid in [
            json!({ "name": "", "color": "blue" }),
            json!({ "name": "x".repeat(61), "color": "blue" }),
            json!({ "name": "tab\there", "color": "blue" }),
            json!({ "name": "Ok", "color": "fuchsia" }),
            json!({ "name": "Ok" }),
        ] {
            let refused = admin.post("/v1/tags", invalid.clone()).await;
            assert!(
                refused.status.is_client_error(),
                "{name}: {invalid} was accepted"
            );
        }
        let duplicate = admin
            .post("/v1/tags", json!({ "name": "SERVERS", "color": "red" }))
            .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(duplicate.code(), "tag_exists");
        let taken = admin
            .patch(&format!("/v1/tags/{london}"), json!({ "name": "servers" }))
            .await;
        assert_eq!(taken.status, StatusCode::CONFLICT, "{name}");

        let recolored = admin
            .patch(&format!("/v1/tags/{london}"), json!({ "color": "purple" }))
            .await;
        assert_eq!(
            recolored.status,
            StatusCode::OK,
            "{name}: {:?}",
            recolored.body
        );
        assert_eq!(recolored.body["name"], "london");
        assert_eq!(recolored.body["color"], "purple");
        let renamed = admin
            .patch(&format!("/v1/tags/{london}"), json!({ "name": "London" }))
            .await;
        assert_eq!(renamed.body["name"], "London", "{name}");

        tag(&mut admin, &[&desk.device_id], &[&servers, &london], &[]).await;
        let listed = admin.get("/v1/tags").await.body;
        let names = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| {
                (
                    tag["name"].as_str().unwrap(),
                    tag["device_count"].as_i64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(names, [("London", 1), ("Servers", 1)], "{name}");

        let deleted = admin.delete(&format!("/v1/tags/{servers}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(tags_of(&mut admin, &desk.device_id).await, json!([london]));
        assert_eq!(
            admin.delete(&format!("/v1/tags/{servers}")).await.status,
            StatusCode::NOT_FOUND
        );

        assert_eq!(audit_events(&app, "tag.create").await.len(), 2, "{name}");
        let updates = audit_events(&app, "tag.update").await;
        assert_eq!(updates.len(), 2, "{name}");
        assert_eq!(updates[1].metadata["previous_color"], "green");
        let deletion = &audit_events(&app, "tag.delete").await[0];
        assert_eq!(deletion.target_id, servers);
        assert_eq!(deletion.metadata["device_count"], 1);
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_are_tagged_one_at_a_time_and_in_bulk() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let servers = create_tag(&mut admin, "Servers", "blue").await;
        let london = create_tag(&mut admin, "London", "green").await;
        let mut events = admin.events(&server).await.unwrap();
        let snapshot = receive_json(&mut events).await;
        assert_eq!(snapshot["tags"].as_array().unwrap().len(), 2, "{name}");
        assert_eq!(
            snapshot["tags"][0],
            json!({ "id": london, "name": "London", "color": "green" })
        );

        let both = [desk.device_id.as_str(), laptop.device_id.as_str()];
        let changed = tag(&mut admin, &both, &[&servers], &[]).await;
        assert_eq!(changed["changed"], 2, "{name}");
        for _ in 0..2 {
            let upsert = receive_json(&mut events).await;
            assert_eq!(upsert["type"], "agent_upsert", "{name}");
            assert_eq!(upsert["agent"]["tag_ids"], json!([servers]));
        }
        // Adding a tag a device has, or removing one it lacks, changes nothing.
        let again = tag(&mut admin, &both, &[&servers], &[&london]).await;
        assert_eq!(again["changed"], 0, "{name}");
        tag(&mut admin, &[&desk.device_id], &[&london], &[&servers]).await;
        assert_eq!(tags_of(&mut admin, &desk.device_id).await, json!([london]));
        let upsert = receive_json(&mut events).await;
        assert_eq!(upsert["agent"]["tag_ids"], json!([london]), "{name}");

        let tagged = audit_events(&app, "agent.tag").await;
        assert_eq!(tagged.len(), 3, "{name}");
        // Newest first.
        assert_eq!(tagged[0].target_id, desk.device_id);
        assert_eq!(tagged[0].metadata["added"], json!(["London"]));
        assert_eq!(tagged[0].metadata["removed"], json!(["Servers"]));

        // Renaming publishes the tag; deleting it untags its devices first.
        admin
            .patch(&format!("/v1/tags/{london}"), json!({ "name": "Paris" }))
            .await;
        let renamed = receive_json(&mut events).await;
        assert_eq!(renamed["type"], "tag_upsert", "{name}");
        assert_eq!(renamed["tag"]["name"], "Paris");
        admin.delete(&format!("/v1/tags/{london}")).await;
        let untagged = receive_json(&mut events).await;
        assert_eq!(untagged["type"], "agent_upsert", "{name}");
        assert_eq!(untagged["agent"]["tag_ids"], json!([]));
        let deleted = receive_json(&mut events).await;
        assert_eq!(deleted["type"], "tag_deleted", "{name}");
        assert_eq!(deleted["tag_id"], london.as_str());

        let refusals = [
            json!({ "device_ids": [], "add": [servers] }),
            json!({ "device_ids": both, "add": [], "remove": [] }),
            json!({ "device_ids": both, "add": [servers], "remove": [servers] }),
            json!({ "device_ids": ["nope"], "add": [servers] }),
        ];
        for refused in refusals {
            let response = admin.post("/v1/device-tags", refused.clone()).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{name}: {refused}"
            );
        }
        let unknown = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": both, "add": [uuid::Uuid::new_v4().to_string()] }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{name}");
        let gone = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [uuid::Uuid::new_v4().to_string()], "add": [servers] }),
            )
            .await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn tags_and_notes_need_their_permissions() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let servers = create_tag(&mut admin, "Servers", "blue").await;

        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer.get("/v1/tags").await.status,
            StatusCode::OK,
            "{name}"
        );
        assert_eq!(
            viewer
                .get(&format!("/v1/agents/{}/notes", desk.device_id))
                .await
                .status,
            StatusCode::OK
        );
        let refusals = [
            viewer
                .post("/v1/tags", json!({ "name": "Mine", "color": "red" }))
                .await,
            viewer
                .patch(&format!("/v1/tags/{servers}"), json!({ "name": "Mine" }))
                .await,
            viewer.delete(&format!("/v1/tags/{servers}")).await,
            viewer
                .post(
                    "/v1/device-tags",
                    json!({ "device_ids": [desk.device_id], "add": [servers] }),
                )
                .await,
            viewer
                .put(
                    &format!("/v1/agents/{}/notes", desk.device_id),
                    json!({ "body": "hi", "revision": 0 }),
                )
                .await,
        ];
        for refused in refusals {
            assert_eq!(
                refused.status,
                StatusCode::FORBIDDEN,
                "{name}: {:?}",
                refused.body
            );
        }

        // The Technician role tags devices and edits notes but doesn't manage
        // tags.
        let (mut technician, _) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        tag(&mut technician, &[&desk.device_id], &[&servers], &[]).await;
        let saved = technician
            .put(
                &format!("/v1/agents/{}/notes", desk.device_id),
                json!({ "body": "Front desk PC", "revision": 0 }),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        assert_eq!(
            technician
                .post("/v1/tags", json!({ "name": "Mine", "color": "red" }))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn notes_keep_their_history_and_refuse_overwriting_a_newer_edit() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let path = format!("/v1/agents/{}/notes", desk.device_id);
        let (mut bob, bob_id) = user_with(
            &app,
            &mut admin,
            "bob@example.com",
            &["devices.view", "devices.edit"],
        )
        .await;

        let empty = admin.get(&path).await;
        assert_eq!(empty.status, StatusCode::OK, "{name}");
        assert_eq!(
            empty.body,
            json!({ "body": "", "revision": 0, "updated_at": null, "updated_by_user_id": null, "updated_by": null, "history": [] })
        );

        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        let first = admin
            .put(
                &path,
                json!({ "body": "Printer on desk 4\r\nAsk for Sam", "revision": 0 }),
            )
            .await;
        assert_eq!(first.status, StatusCode::OK, "{name}: {:?}", first.body);
        assert_eq!(first.body["body"], "Printer on desk 4\nAsk for Sam");
        assert_eq!(first.body["revision"], 1);
        assert_eq!(first.body["updated_by"], "Ada Admin");
        let upsert = receive_json(&mut events).await;
        assert_eq!(upsert["agent"]["notes_revision"], 1, "{name}");

        // Bob edits revision 1 first; Ada's edit of it is refused.
        let bobs = bob
            .put(&path, json!({ "body": "Bob's version", "revision": 1 }))
            .await;
        assert_eq!(bobs.status, StatusCode::OK, "{name}: {:?}", bobs.body);
        assert_eq!(bobs.body["updated_by_user_id"], bob_id.as_str());
        let stale = admin
            .put(&path, json!({ "body": "Ada's version", "revision": 1 }))
            .await;
        assert_eq!(stale.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(stale.code(), "notes_changed");
        let current = admin.get(&path).await.body;
        assert_eq!(current["body"], "Bob's version", "{name}");
        assert_eq!(current["history"].as_array().unwrap().len(), 2);
        assert_eq!(current["history"][1]["length"], 29);
        assert_eq!(current["history"][1].get("body"), None);
        let old = admin.get(&format!("{path}/1")).await;
        assert_eq!(old.body["body"], "Printer on desk 4\nAsk for Sam", "{name}");
        assert_eq!(
            admin.get(&format!("{path}/9")).await.status,
            StatusCode::NOT_FOUND
        );

        // Saving the same text again makes no revision.
        let same = bob
            .put(&path, json!({ "body": "Bob's version", "revision": 2 }))
            .await;
        assert_eq!(same.body["revision"], 2, "{name}");

        let too_long = admin
            .put(&path, json!({ "body": "x".repeat(20_001), "revision": 2 }))
            .await;
        assert_eq!(too_long.status, StatusCode::BAD_REQUEST, "{name}");

        for revision in 2..25 {
            let saved = admin
                .put(
                    &path,
                    json!({ "body": format!("v{revision}"), "revision": revision }),
                )
                .await;
            assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        }
        let current = admin.get(&path).await.body;
        assert_eq!(current["revision"], 25, "{name}");
        let history = current["history"].as_array().unwrap();
        assert_eq!(history.len(), 20, "{name}: only the last 20 are kept");
        assert_eq!(history[19]["revision"], 6);

        let audited = audit_events(&app, "agent.notes").await;
        assert_eq!(audited.len(), 25, "{name}");
        let first = &audited[24];
        assert_eq!(first.metadata["length"], 29);
        assert_eq!(first.metadata["revision"], 1);
        assert!(
            !first.metadata.to_string().contains("Printer"),
            "{name}: the audit log doesn't keep the text"
        );

        let missing = admin
            .get(&format!("/v1/agents/{}/notes", uuid::Uuid::new_v4()))
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{name}");
        app.finish().await;
    }
}

async fn create_script(browser: &mut Browser) -> String {
    let created = browser
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": "Check", "language": "shell", "body": "echo hi", "timeout_seconds": 60, "shared": true }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn pass(app: &App, now: i64) {
    automation::pass(&app.state, now).await.unwrap();
}

#[tokio::test]
async fn jobs_and_policies_on_tags_cover_the_devices_tagged_when_they_run() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let web = app.enroll_on(&mut admin, "web", "linux").await;
        let db = app.enroll_on(&mut admin, "db", "linux").await;
        let mac = app.enroll_on(&mut admin, "mac", "macos").await;
        let servers = create_tag(&mut admin, "Servers", "blue").await;
        let london = create_tag(&mut admin, "London", "green").await;
        tag(
            &mut admin,
            &[&web.device_id, &db.device_id],
            &[&servers],
            &[],
        )
        .await;
        tag(
            &mut admin,
            &[&web.device_id, &mac.device_id],
            &[&london],
            &[],
        )
        .await;
        let script = create_script(&mut admin).await;
        let job = |tags: &[&str], matching: &str| {
            json!({
                "script_id": script, "run_as": "system", "target": "tags",
                "tag_ids": tags, "tag_match": matching, "offline": "wait",
            })
        };

        let any = admin
            .post("/v1/jobs", job(&[&servers, &london], "any"))
            .await;
        assert_eq!(any.status, StatusCode::CREATED, "{name}: {:?}", any.body);
        assert_eq!(any.body["target"], "tags");
        assert_eq!(any.body["target_label"], "Tagged London or Servers");
        let id = any.body["id"].as_str().unwrap();
        assert_eq!(
            job_devices(&mut admin, id).await,
            ["db", "mac", "web"],
            "{name}"
        );
        let all = admin
            .post("/v1/jobs", job(&[&servers, &london], "all"))
            .await;
        assert_eq!(
            all.body["target_label"], "Tagged London and Servers",
            "{name}"
        );
        let id = all.body["id"].as_str().unwrap();
        assert_eq!(job_devices(&mut admin, id).await, ["web"], "{name}");

        // Tags reach many devices, so they need jobs.run_wide.
        let (mut runner, _) = user_with(
            &app,
            &mut admin,
            "runner@example.com",
            &["devices.view", "scripts.run"],
        )
        .await;
        let refused = runner.post("/v1/jobs", job(&[&servers], "any")).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        let empty = admin.post("/v1/jobs", job(&[], "any")).await;
        assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(empty.code(), "invalid_job");

        // A policy on a tag covers the devices tagged when it fires.
        let policy = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Servers check", "script_id": script, "run_as": "system",
                    "target": "tags", "tag_ids": [servers], "trigger": "interval",
                    "interval_minutes": 5, "offline": "wait",
                }),
            )
            .await;
        assert_eq!(
            policy.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            policy.body
        );
        assert_eq!(policy.body["tag_ids"], json!([servers]));
        assert_eq!(policy.body["tag_match"], "any");
        let policy_id = policy.body["id"].as_str().unwrap().to_owned();
        let next = policy.body["next_run_at"].as_i64().unwrap();
        tag(&mut admin, &[&mac.device_id], &[&servers], &[]).await;
        tag(&mut admin, &[&db.device_id], &[], &[&servers]).await;
        pass(&app, next).await;
        let fired = admin
            .get(&format!("/v1/jobs?policy_id={policy_id}"))
            .await
            .body["jobs"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            job_devices(&mut admin, &fired).await,
            ["mac", "web"],
            "{name}"
        );

        let listed = admin
            .get("/v1/tags")
            .await
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|tag| tag["id"] == servers.as_str())
            .unwrap()
            .clone();
        assert_eq!(listed["policy_count"], 1, "{name}");

        // Newly enrolled devices have no tags, so enrollment policies can't use them.
        let enroll = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "On enroll", "script_id": script, "run_as": "system",
                    "target": "tags", "tag_ids": [servers], "trigger": "enroll", "offline": "wait",
                }),
            )
            .await;
        assert_eq!(enroll.status, StatusCode::BAD_REQUEST, "{name}");
        let unknown = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Unknown", "script_id": script, "run_as": "system",
                    "target": "tags", "tag_ids": [uuid::Uuid::new_v4().to_string()],
                    "trigger": "interval", "interval_minutes": 5, "offline": "wait",
                }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{name}");

        // Deleting the tag leaves the policy covering nothing.
        admin.delete(&format!("/v1/tags/{servers}")).await;
        pass(&app, next + 5 * MINUTE_MS).await;
        let policies = admin.get("/v1/automation-policies").await.body;
        let saved = &policies["policies"][0];
        assert_eq!(saved["tag_ids"], json!([]), "{name}");
        assert!(saved["last_problem"].is_string(), "{name}: {saved}");
        app.finish().await;
    }
}

#[tokio::test]
async fn alert_rules_and_searches_cover_tagged_devices() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let other = app.enroll(&mut admin, "Other").await;
        let servers = create_tag(&mut admin, "Servers", "blue").await;
        let london = create_tag(&mut admin, "London", "green").await;
        tag(&mut admin, &[&desk.device_id], &[&servers, &london], &[]).await;
        tag(&mut admin, &[&other.device_id], &[&london], &[]).await;

        let rule = admin
            .post(
                "/v1/alert-rules",
                json!({
                    "name": "Servers offline", "severity": "warning", "condition": "device_offline",
                    "duration_minutes": 1, "target": "tags", "tag_ids": [servers, london],
                    "tag_match": "all",
                }),
            )
            .await;
        assert_eq!(rule.status, StatusCode::CREATED, "{name}: {:?}", rule.body);
        assert_eq!(rule.body["target"], "tags");
        assert_eq!(rule.body["tag_match"], "all");
        let start = now_ms();
        alerts::evaluate(&app.state, start).await.unwrap();
        alerts::evaluate(&app.state, start + 2 * MINUTE_MS)
            .await
            .unwrap();
        let open = admin.get("/v1/alerts").await.body["alerts"].clone();
        let open = open.as_array().unwrap();
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        assert_eq!(open[0]["device_id"], desk.device_id.as_str());

        // Untagging the device takes it out of the rule's scope.
        tag(&mut admin, &[&desk.device_id], &[], &[&servers]).await;
        alerts::evaluate(&app.state, start + 3 * MINUTE_MS)
            .await
            .unwrap();
        let open = admin.get("/v1/alerts").await.body["alerts"].clone();
        assert_eq!(open, json!([]), "{name}");

        let refused = admin
            .post(
                "/v1/alert-rules",
                json!({
                    "name": "Nothing", "severity": "warning", "condition": "device_offline",
                    "duration_minutes": 1, "target": "tags",
                }),
            )
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name}");

        // The software search narrows to a tag.
        for (device, application) in [(&desk, "Firefox"), (&other, "Chrome")] {
            app.db()
                .execute(
                    &sea_query::Query::insert()
                        .into_table(sea_query::Alias::new("device_applications"))
                        .columns(["device_id", "name", "source"].map(sea_query::Alias::new))
                        .values_panic([
                            device.device_id.as_str().into(),
                            application.into(),
                            "windows".into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }
        let software = |tag: &str| format!("/v1/software?tag={tag}");
        let names = |body: &Value| {
            body["applications"]
                .as_array()
                .unwrap()
                .iter()
                .map(|application| application["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&admin.get(&software(&london)).await.body),
            ["Chrome", "Firefox"]
        );
        let tagged = admin
            .get(&format!("/v1/software?tag={london}&q=fire"))
            .await;
        assert_eq!(names(&tagged.body), ["Firefox"], "{name}");
        assert!(
            names(&admin.get(&software(&servers)).await.body).is_empty(),
            "{name}"
        );
        let installed = admin
            .get(&format!("/v1/software/devices?name=Chrome&tag={servers}"))
            .await;
        assert_eq!(installed.body["devices"], json!([]), "{name}");
        let invalid = admin.get(&software("nope")).await;
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{name}");
        app.finish().await;
    }
}
