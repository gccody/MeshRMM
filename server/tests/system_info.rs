//! Devices' hardware and software: Agents' snapshots over the control
//! connection, the device list's summary, refreshes, application changes,
//! snapshots that leave out an unchanged list, and searching devices and the
//! fleet's applications.
mod common;

use std::{net::IpAddr, time::Duration};

use common::{App, Ws, apps, receive_any_json, receive_json, send_json, set_up, user_with};
use meshrmm_protocol_types::{SystemInfo, applications_hash};
use meshrmm_server::{
    db::Counts,
    system_info::{self, Stored},
};
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

/// The hash of a snapshot message's application list, as the server stores
/// it.
fn hash_of(snapshot: &Value) -> String {
    let info: SystemInfo = serde_json::from_value(snapshot["system"].clone()).unwrap();
    applications_hash(&info.sanitized().applications)
}

/// A snapshot message that names its application list instead of listing
/// it.
fn unchanged(serial: &str, hash: &str) -> Value {
    let mut message = snapshot(serial, "10.1.2.3", vec![]);
    message["applications_unchanged"] = json!(hash);
    message
}

/// What the server says it holds, as it tells the Agent.
fn stored_message(hash: Option<&str>) -> Value {
    match hash {
        Some(hash) => json!({ "type": "stored_applications", "applications_hash": hash }),
        None => json!({ "type": "stored_applications" }),
    }
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
        // A device without a snapshot is told it has no list, and asked for
        // one when it connects.
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(receive_any_json(&mut control).await, stored_message(None));
        assert_eq!(
            receive_any_json(&mut control).await["type"],
            "collect_system_info"
        );
        // And told its update scan schedule.
        assert_eq!(
            receive_any_json(&mut control).await["type"],
            "set_update_scan_schedule"
        );
        receive_json(&mut events).await;

        let first = snapshot(
            "SER-1",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("23.01")),
                application("Firefox", Some("130.0")),
                application("Notepad++", None),
            ],
        );
        let first_hash = hash_of(&first);
        send_json(&mut control, first).await;
        // The Agent hears which list the server now holds.
        assert_eq!(
            receive_any_json(&mut control).await,
            stored_message(Some(&first_hash)),
            "{}",
            app.name
        );
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
        let second = snapshot(
            "SER-1",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("24.08")),
                application("Firefox", Some("130.0")),
                application("VLC media player", Some("3.0.21")),
            ],
        );
        let second_hash = hash_of(&second);
        send_json(&mut control, second).await;
        assert_eq!(
            receive_any_json(&mut control).await,
            stored_message(Some(&second_hash))
        );
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
        // It is told the list the server holds and its update scan schedule
        // only.
        assert_eq!(
            receive_any_json(&mut control).await,
            stored_message(Some(&second_hash)),
            "{}",
            app.name
        );
        while let Ok(message) =
            tokio::time::timeout(Duration::from_millis(500), receive_any_json(&mut control)).await
        {
            assert_eq!(message["type"], "set_update_scan_schedule", "{}", app.name);
        }
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
            receive_any_json(&mut control).await,
            stored_message(Some(&second_hash))
        );
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

/// The names of the device's stored applications.
async fn application_names(app: &App, device_id: &str) -> Vec<String> {
    system_info::applications(app.db(), device_id)
        .await
        .unwrap()
        .into_iter()
        .map(|application| format!("{} {:?}", application.name, application.version))
        .collect()
}

/// The device's application changes, sorted.
async fn change_list(app: &App, device_id: &str) -> Vec<String> {
    let mut changes: Vec<_> = system_info::recent_changes(app.db(), device_id, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|change| {
            format!(
                "{} {} {:?} {:?}",
                change.change, change.name, change.version, change.previous_version
            )
        })
        .collect();
    changes.sort();
    changes
}

/// The stored application list's hash, and the serial number of the
/// snapshot stored.
async fn stored_row(app: &App, device_id: &str) -> Option<(Option<String>, Option<String>)> {
    app.db()
        .fetch_optional(
            &sea_query::Query::select()
                .columns([
                    sea_query::Alias::new("applications_hash"),
                    sea_query::Alias::new("serial_number"),
                ])
                .from(sea_query::Alias::new("device_system_info"))
                .and_where(sea_query::Expr::col("device_id").eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap()
}

/// Stores a snapshot message as the control connection does, returning what
/// it ran.
async fn store(app: &App, device_id: &str, message: &Value) -> (Option<Stored>, Counts) {
    let info: SystemInfo = serde_json::from_value(message["system"].clone()).unwrap();
    let unchanged = message["applications_unchanged"]
        .as_str()
        .map(str::to_owned);
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    let before = app.db().counts();
    let stored = system_info::store(app.db(), device_id, ip, info, unchanged)
        .await
        .unwrap();
    (stored, app.db().counts() - before)
}

/// What storing a snapshot that names the stored list runs: whether the
/// device is enrolled, the stored hash, the snapshot's row, and its addresses
/// removed and added. Nothing about applications.
const UNCHANGED_STATEMENTS: Counts = Counts {
    statements: 5,
    writes: 3,
    transactions: 1,
};

#[tokio::test]
async fn a_list_the_server_holds_is_named_instead_of_stored_again() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let id = desk.device_id.as_str();

        // A device's first list is where its history starts.
        let first = snapshot(
            "SER-1",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("23.01")),
                application("Firefox", Some("130.0")),
                application("Notepad++", None),
            ],
        );
        let first_hash = hash_of(&first);
        let (stored, _) = store(&app, id, &first).await;
        assert_eq!(
            stored,
            Some(Stored::Listed {
                applications_hash: first_hash.clone()
            }),
            "{name}"
        );
        let listed = application_names(&app, id).await;
        assert_eq!(listed.len(), 3);
        assert_eq!(change_list(&app, id).await, Vec::<String>::new());

        // One that names that list keeps it and its history, runs no
        // application statement, and still stores the rest.
        let (stored, cost) = store(&app, id, &unchanged("SER-2", &first_hash)).await;
        assert_eq!(stored, Some(Stored::Unchanged), "{name}");
        assert_eq!(cost, UNCHANGED_STATEMENTS, "{name}");
        assert_eq!(application_names(&app, id).await, listed);
        assert_eq!(change_list(&app, id).await, Vec::<String>::new());
        assert_eq!(
            stored_row(&app, id).await,
            Some((Some(first_hash.clone()), Some("SER-2".into())))
        );

        // A changed list records what changed since the stored one, as it
        // would have with no snapshot naming it in between.
        let second = snapshot(
            "SER-2",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("24.08")),
                application("Firefox", Some("130.0")),
                application("VLC media player", Some("3.0.21")),
            ],
        );
        let second_hash = hash_of(&second);
        let (stored, _) = store(&app, id, &second).await;
        assert_eq!(
            stored,
            Some(Stored::Listed {
                applications_hash: second_hash.clone()
            })
        );
        let history = [
            "installed VLC media player Some(\"3.0.21\") None",
            "removed Notepad++ None None",
            "updated 7-Zip Some(\"24.08\") Some(\"23.01\")",
        ];
        assert_eq!(change_list(&app, id).await, history, "{name}");
        let listed = application_names(&app, id).await;

        // One that names another list keeps the stored one, says which that
        // is, and changes nothing about applications.
        for wrong in [first_hash.as_str(), "not a hash"] {
            let (stored, cost) = store(&app, id, &unchanged("SER-3", wrong)).await;
            assert_eq!(
                stored,
                Some(Stored::Unknown {
                    applications_hash: Some(second_hash.clone())
                }),
                "{name}"
            );
            assert_eq!(cost, UNCHANGED_STATEMENTS, "{name}");
            assert_eq!(application_names(&app, id).await, listed);
            assert_eq!(change_list(&app, id).await, history);
            assert_eq!(
                stored_row(&app, id).await,
                Some((Some(second_hash.clone()), Some("SER-3".into())))
            );
        }

        // Once its rows are gone, the device holds no list: a snapshot
        // naming one stores the rest without a list, and the list that
        // follows is where the history starts again.
        system_info::forget(app.db(), id).await.unwrap();
        let (stored, _) = store(&app, id, &unchanged("SER-4", &second_hash)).await;
        assert_eq!(
            stored,
            Some(Stored::Unknown {
                applications_hash: None
            }),
            "{name}"
        );
        assert_eq!(
            stored_row(&app, id).await,
            Some((None, Some("SER-4".into())))
        );
        assert_eq!(application_names(&app, id).await, Vec::<String>::new());
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let connected = system_info::connected(app.db(), id, ip).await.unwrap();
        assert_eq!(
            connected,
            system_info::Connected {
                stale: false,
                applications_hash: None
            }
        );
        let (stored, _) = store(&app, id, &second).await;
        assert_eq!(
            stored,
            Some(Stored::Listed {
                applications_hash: second_hash.clone()
            })
        );
        assert_eq!(application_names(&app, id).await, listed);
        assert_eq!(change_list(&app, id).await, Vec::<String>::new());
        let connected = system_info::connected(app.db(), id, ip).await.unwrap();
        assert_eq!(connected.applications_hash, Some(second_hash));
        app.finish().await;
    }
}

/// The next message on the control connection, skipping the update scan
/// schedule.
async fn next_command(control: &mut Ws) -> Value {
    loop {
        let message = receive_any_json(control).await;
        if message["type"] != "set_update_scan_schedule" {
            return message;
        }
    }
}

#[tokio::test]
async fn an_agent_naming_a_list_the_server_lacks_is_asked_for_it_once() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(next_command(&mut control).await, stored_message(None));
        assert_eq!(
            next_command(&mut control).await["type"],
            "collect_system_info"
        );

        let full = snapshot(
            "SER-1",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("23.01")),
                application("Firefox", Some("130.0")),
            ],
        );
        let hash = hash_of(&full);
        send_json(&mut control, full.clone()).await;
        assert_eq!(
            next_command(&mut control).await,
            stored_message(Some(&hash)),
            "{name}"
        );

        // A snapshot naming the stored list is answered with nothing; one
        // naming another is told the stored one and asked for a snapshot.
        send_json(&mut control, unchanged("SER-2", &hash)).await;
        send_json(&mut control, unchanged("SER-3", "stale")).await;
        assert_eq!(
            next_command(&mut control).await,
            stored_message(Some(&hash)),
            "{name}"
        );
        assert_eq!(
            next_command(&mut control).await["type"],
            "collect_system_info"
        );

        // The rows went away: the Agent hears there is no list, and the
        // whole list it then sends is stored, with nothing asked again.
        system_info::forget(app.db(), &desk.device_id)
            .await
            .unwrap();
        send_json(&mut control, unchanged("SER-4", &hash)).await;
        assert_eq!(
            next_command(&mut control).await,
            stored_message(None),
            "{name}"
        );
        assert_eq!(
            next_command(&mut control).await["type"],
            "collect_system_info"
        );
        send_json(&mut control, full).await;
        assert_eq!(
            next_command(&mut control).await,
            stored_message(Some(&hash)),
            "{name}"
        );
        assert_eq!(
            application_names(&app, &desk.device_id).await,
            ["7-Zip Some(\"23.01\")", "Firefox Some(\"130.0\")"]
        );
        let stored = admin
            .get(&format!("/v1/agents/{}/system-info", desk.device_id))
            .await;
        assert_eq!(stored.body["hardware"]["serial_number"], "SER-1");
        assert_eq!(stored.body["application_count"], 2, "{name}");

        // The next connection's greeting names it.
        drop(control);
        let mut control = desk.connect(&server).await.unwrap();
        assert_eq!(
            next_command(&mut control).await,
            stored_message(Some(&hash)),
            "{name}"
        );
        drop(control);
        app.finish().await;
    }
}

#[tokio::test]
async fn a_snapshot_far_larger_than_the_read_buffer_arrives_whole() {
    for app in apps().await {
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut events = admin.events(&server).await.unwrap();
        receive_json(&mut events).await;
        let mut control = desk.connect(&server).await.unwrap();
        receive_json(&mut events).await;

        // Hundreds of KiB, many times the control connection's read buffer
        // (and tungstenite's default one).
        let padding = "x".repeat(200);
        let applications: Vec<_> = (0..2_000)
            .map(|index| application(&format!("App {index:04} {padding}"), Some("1.0")))
            .collect();
        let large = snapshot("SER-1", "10.1.2.3", applications);
        assert!(large.to_string().len() > 512 * 1024);
        send_json(&mut control, large).await;
        receive_json(&mut events).await;
        let path = format!("/v1/agents/{}/system-info", desk.device_id);
        let stored = admin.get(&path).await;
        assert_eq!(stored.status, 200, "{:?}", stored.body);
        assert_eq!(stored.body["application_count"], 2_000, "{}", app.name);
        let listed = admin
            .get(&format!("/v1/agents/{}/applications", desk.device_id))
            .await;
        let stored_applications = listed.body["applications"].as_array().unwrap();
        assert_eq!(stored_applications.len(), 2_000, "{}", app.name);
        assert_eq!(
            stored_applications[1_999]["name"],
            format!("App 1999 {padding}")
        );

        // A small message after it is still read.
        send_json(
            &mut control,
            snapshot("SER-2", "10.1.2.3", vec![application("7-Zip", None)]),
        )
        .await;
        let event = receive_json(&mut events).await;
        assert_eq!(
            device(&event, &desk.device_id)["system"]["serial_number"],
            "SER-2",
            "{}",
            app.name
        );
        drop(control);
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
