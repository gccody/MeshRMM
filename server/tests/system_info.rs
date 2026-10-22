//! Devices' hardware and software: Agents' snapshots over the control
//! connection, the device list's summary, refreshes, application changes,
//! and searching devices and the fleet's applications.
mod common;

use std::time::Duration;

use common::{App, apps, receive_any_json, receive_json, send_json, set_up, user_with};
use sea_query::ExprTrait;
use serde_json::{Value, json};

fn application(name: &str, version: Option<&str>) -> Value {
    json!({ "name": name, "version": version, "publisher": "Example Corp", "source": "windows" })
}

fn snapshot(serial: &str, address: &str, applications: Vec<Value>) -> Value {
    json!({
        "type": "system_info",
        "system": {
            "collected_at_unix_ms": 1_700_000_000_000_i64,
            "os": {
                "id": "windows",
                "name": "Windows 11 Pro",
                "version": "24H2",
                "build": "26100.4652",
                "kernel": "10.0.26100",
                "architecture": "x86_64",
                "package_managers": ["windows_update", "winget"]
            },
            "hardware": {
                "manufacturer": "Dell Inc.",
                "model": "OptiPlex 7090",
                "serial_number": serial,
                "memory_bytes": 17_179_869_184_i64,
                "processor": { "name": "Intel(R) Core(TM) i7", "cores": 8, "threads": 16 }
            },
            "network": {
                "hostname": "DESK",
                "workgroup": "WORKGROUP",
                "adapters": [{
                    "name": "Ethernet",
                    "mac_address": "00-15-5d-01-02-03",
                    "kind": "ethernet",
                    "up": true,
                    "addresses": [
                        { "address": address, "prefix_length": 24 },
                        { "address": "fe80::1%7", "prefix_length": 64 }
                    ],
                    "gateways": ["10.1.2.1"]
                }]
            },
            "users": [{ "name": "DESK\\ada", "session": "Console" }],
            "applications": applications
        }
    })
}

/// The device in a presence event.
fn device<'a>(event: &'a Value, id: &str) -> &'a Value {
    assert_eq!(event["type"], "agent_upsert", "{event}");
    assert_eq!(event["agent"]["id"], id, "{event}");
    &event["agent"]
}

async fn count(app: &App, table: &str, device_id: &str) -> i64 {
    let (count,): (i64,) = app
        .db()
        .fetch_one(
            &sea_query::Query::select()
                .expr(sea_query::Func::count(sea_query::Expr::col("device_id")))
                .from(sea_query::Alias::new(table))
                .and_where(sea_query::Expr::col("device_id").eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap();
    count
}

#[tokio::test]
async fn agents_send_system_information_the_website_shows_and_searches() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let path = format!("/v1/agents/{}/system-info", desk.device_id);
        let missing = admin.get(&path).await;
        assert_eq!(missing.status, 404, "{}", app.name);
        assert_eq!(missing.code(), "not_collected");

        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        // A device without a snapshot is asked for one when it connects.
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(
            receive_any_json(&mut control).await["type"],
            "collect_system_info"
        );
        receive_json(&mut events).await;

        send_json(
            &mut control,
            snapshot(
                "SER-1",
                "10.1.2.3",
                vec![
                    application("7-Zip", Some("23.01")),
                    application("Firefox", Some("130.0")),
                    application("Notepad++", None),
                ],
            ),
        )
        .await;
        let event = receive_json(&mut events).await;
        let summary = &device(&event, &desk.device_id)["system"];
        assert_eq!(summary["os"], "Windows 11 Pro 24H2", "{}", app.name);
        assert_eq!(summary["model"], "OptiPlex 7090");
        assert_eq!(summary["serial_number"], "SER-1");
        assert_eq!(summary["ip_addresses"], json!(["10.1.2.3"]));
        assert_eq!(summary["public_ip"], "127.0.0.1");
        assert!(summary["received_at"].is_i64());

        let stored = admin.get(&path).await;
        assert_eq!(stored.status, 200, "{:?}", stored.body);
        assert_eq!(stored.body["collected_at"], 1_700_000_000_000_i64);
        assert_eq!(stored.body["application_count"], 3);
        assert_eq!(stored.body["os"]["build"], "26100.4652");
        assert_eq!(stored.body["hardware"]["processor"]["threads"], 16);
        let adapter = &stored.body["network"]["adapters"][0];
        assert_eq!(adapter["mac_address"], "00:15:5D:01:02:03");
        assert_eq!(adapter["addresses"][1]["address"], "fe80::1");
        assert_eq!(stored.body["users"][0]["name"], "DESK\\ada");
        assert!(stored.body.get("applications").is_none());
        let listed = admin
            .get(&format!("/v1/agents/{}/applications", desk.device_id))
            .await;
        let names: Vec<_> = listed.body["applications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|application| application["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["7-Zip", "Firefox", "Notepad++"]);

        // A refresh reaches the Agent, and its new list is compared with the
        // last one.
        let refreshed = admin.post(&format!("{path}/refresh"), json!({})).await;
        assert_eq!(refreshed.status, 202, "{:?}", refreshed.body);
        assert_eq!(
            receive_any_json(&mut control).await["type"],
            "collect_system_info"
        );
        send_json(
            &mut control,
            snapshot(
                "SER-1",
                "10.1.2.3",
                vec![
                    application("7-Zip", Some("24.08")),
                    application("Firefox", Some("130.0")),
                    application("VLC media player", Some("3.0.21")),
                ],
            ),
        )
        .await;
        receive_json(&mut events).await;
        let changes = admin
            .get(&format!(
                "/v1/agents/{}/application-changes",
                desk.device_id
            ))
            .await;
        let mut changes: Vec<_> = changes.body["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| {
                format!(
                    "{} {} {} {}",
                    change["change"].as_str().unwrap(),
                    change["name"].as_str().unwrap(),
                    change["version"],
                    change["previous_version"]
                )
            })
            .collect();
        changes.sort();
        assert_eq!(
            changes,
            [
                "installed VLC media player \"3.0.21\" null",
                "removed Notepad++ null null",
                "updated 7-Zip \"24.08\" \"23.01\"",
            ],
            "{}",
            app.name
        );

        // A second device, on Linux in no company.
        let laptop = app.enroll_on(&mut admin, "Laptop", "linux").await;
        receive_json(&mut events).await;
        let mut laptop_control = laptop.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        send_json(
            &mut laptop_control,
            snapshot(
                "SER-2",
                "192.168.7.20",
                vec![application("Firefox", Some("131.0"))],
            ),
        )
        .await;
        receive_json(&mut events).await;

        let search = |query: &str| format!("/v1/system-info?{query}");
        for (query, expected) in [
            ("serial=ser-1", vec!["Desk"]),
            ("ip=10.1.2", vec!["Desk"]),
            ("ip=127.0.0.1", vec!["Desk", "Laptop"]),
            ("os=windows%2011", vec!["Desk", "Laptop"]),
            ("model=dell", vec!["Desk", "Laptop"]),
            ("application=vlc", vec!["Desk"]),
            ("application=firefox&serial=SER-2", vec!["Laptop"]),
            ("q=192.168", vec!["Laptop"]),
            ("q=lap", vec!["Laptop"]),
            ("q=50%25", vec![]),
            ("company=none", vec!["Desk", "Laptop"]),
        ] {
            let found = admin.get(&search(query)).await;
            assert_eq!(found.status, 200, "{query}: {:?}", found.body);
            let names: Vec<_> = found.body["devices"]
                .as_array()
                .unwrap()
                .iter()
                .map(|device| device["device_name"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(names, expected, "{}: {query}", app.name);
        }
        let found = admin.get(&search("serial=SER-2")).await;
        assert_eq!(found.body["devices"][0]["platform"], "linux");
        assert_eq!(found.body["devices"][0]["application_count"], 1);
        assert_eq!(admin.get(&search("bogus=1")).await.status, 400);
        assert_eq!(admin.get(&search("company=acme")).await.status, 400);

        let software = admin.get("/v1/software?q=FIRE").await;
        assert_eq!(software.status, 200, "{:?}", software.body);
        assert_eq!(
            software.body,
            json!({
                "applications": [{
                    "name": "Firefox",
                    "publisher": "Example Corp",
                    "devices": 2,
                    "versions": [
                        { "version": "131.0", "devices": 1 },
                        { "version": "130.0", "devices": 1 }
                    ]
                }],
                "truncated": false
            }),
            "{}",
            app.name
        );
        let popular = admin.get("/v1/software?limit=1").await;
        assert_eq!(popular.body["applications"][0]["name"], "Firefox");
        assert_eq!(popular.body["truncated"], true);
        let installed = admin.get("/v1/software/devices?name=Firefox").await;
        let on: Vec<_> = installed.body["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|device| {
                (
                    device["device_name"].as_str().unwrap(),
                    device["version"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(on, [("Desk", "130.0"), ("Laptop", "131.0")], "{}", app.name);
        let one = admin
            .get("/v1/software/devices?name=Firefox&version=131.0")
            .await;
        assert_eq!(one.body["devices"].as_array().unwrap().len(), 1);
        let none = admin
            .get("/v1/software/devices?name=Notepad%2B%2B&unversioned=true")
            .await;
        assert_eq!(none.body["devices"], json!([]));
        assert_eq!(
            admin
                .get("/v1/software/devices?name=Firefox&version=1&unversioned=true")
                .await
                .status,
            400
        );

        // A fresh snapshot isn't asked for again on reconnecting; a stale one is.
        drop(control);
        receive_json(&mut events).await;
        let mut control = desk.connect(&server).await.unwrap();
        receive_json(&mut events).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(500), receive_any_json(&mut control))
                .await
                .is_err(),
            "{}",
            app.name
        );
        drop(control);
        receive_json(&mut events).await;
        app.db()
            .execute(
                &sea_query::Query::update()
                    .table(sea_query::Alias::new("device_system_info"))
                    .value(sea_query::Alias::new("received_at"), 0)
                    .and_where(sea_query::Expr::col("device_id").eq(desk.device_id.as_str()))
                    .to_owned(),
            )
            .await
            .unwrap();
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(
            receive_any_json(&mut control).await["type"],
            "collect_system_info"
        );
        receive_json(&mut events).await;

        // Offline devices can't be refreshed.
        drop(control);
        receive_json(&mut events).await;
        let offline = admin.post(&format!("{path}/refresh"), json!({})).await;
        assert_eq!(offline.status, 409, "{:?}", offline.body);
        assert_eq!(offline.code(), "device_offline");

        // Deleting a device removes everything it sent.
        let deleted = admin
            .delete(&format!("/v1/agents/{}", desk.device_id))
            .await;
        assert_eq!(deleted.status, 204, "{:?}", deleted.body);
        for table in [
            "device_system_info",
            "device_ip_addresses",
            "device_applications",
            "device_application_changes",
        ] {
            assert_eq!(count(&app, table, &desk.device_id).await, 0, "{table}");
        }
        assert_eq!(admin.get(&path).await.status, 404);
        drop(laptop_control);
        app.finish().await;
    }
}

#[tokio::test]
async fn seeing_system_information_needs_devices_view() {
    for app in apps().await {
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let (mut auditor, _) =
            user_with(&app, &mut admin, "auditor@example.com", &["audit.view"]).await;
        for path in [
            format!("/v1/agents/{}/system-info", desk.device_id),
            format!("/v1/agents/{}/applications", desk.device_id),
            format!("/v1/agents/{}/application-changes", desk.device_id),
            "/v1/system-info".to_owned(),
            "/v1/software".to_owned(),
            "/v1/software/devices?name=x".to_owned(),
        ] {
            assert_eq!(auditor.get(&path).await.status, 403, "{}: {path}", app.name);
        }
        let refresh = auditor
            .post(
                &format!("/v1/agents/{}/system-info/refresh", desk.device_id),
                json!({}),
            )
            .await;
        assert_eq!(refresh.status, 403);
        let unknown = admin
            .get("/v1/agents/0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a/system-info")
            .await;
        assert_eq!(unknown.status, 404);
        assert_ne!(unknown.code(), "not_collected");
        app.finish().await;
    }
}
