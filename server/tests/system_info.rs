//! Devices' hardware and software: Agents' snapshots over the control
//! connection, the device list's summary, refreshes, application changes,
//! snapshots that leave out an unchanged list, and searching devices and the
//! fleet's applications.
mod common;

use std::{net::IpAddr, time::Duration};

use common::{
    App, Browser, Ws, apps, receive_any_json, receive_json, send_json, set_up, user_with,
};
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
            assert_eq!(found.body["matching"], expected.len(), "{query}");
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
                "matching": 1,
                "truncated": false
            }),
            "{}",
            app.name
        );
        let popular = admin.get("/v1/software?limit=1").await;
        assert_eq!(popular.body["applications"][0]["name"], "Firefox");
        assert_eq!(popular.body["matching"], 3, "{}", app.name);
        assert_eq!(popular.body["truncated"], true);
        // The rest follow, with as many devices or fewer, by name.
        let rest = admin.get("/v1/software?limit=5&offset=1").await;
        let rest_names: Vec<_> = rest.body["applications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|application| application["name"].as_str().unwrap())
            .collect();
        assert_eq!(rest_names, ["7-Zip", "VLC media player"], "{}", app.name);
        assert_eq!(rest.body["truncated"], false);
        let installed = admin.get("/v1/software/devices?name=Firefox").await;
        assert_eq!(installed.body["matching"], 2, "{}", app.name);
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

#[tokio::test]
async fn a_refresh_for_a_device_too_far_behind_is_busy_not_offline() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let path = format!("/v1/agents/{}/system-info/refresh", desk.device_id);
        let mut connection = app.state.agents.connect(&desk.device_id);
        let queued = common::fill_backlog(&app, &desk.device_id);
        common::assert_device_busy(&admin.post(&path, json!({})).await, name);
        common::drain_backlog(&mut connection, queued).await;
        let refreshed = admin.post(&path, json!({})).await;
        assert_eq!(refreshed.status, 202, "{name}: {:?}", refreshed.body);
        drop(connection);
        let offline = admin.post(&path, json!({})).await;
        assert_eq!(offline.code(), "device_offline", "{name}");
        app.finish().await;
    }
}

/// An application from `publisher`, installed by `source`.
fn published(name: &str, version: Option<&str>, publisher: Option<&str>, source: &str) -> Value {
    json!({ "name": name, "version": version, "publisher": publisher, "source": source })
}

/// Device `n` of a fleet added by [`add_devices`]: many share a name, and
/// each has `Common`, every other one `Even`, and one of five tools.
fn fleet_snapshot(n: usize) -> Value {
    let mut applications = vec![application("Common", Some("1.0"))];
    if n.is_multiple_of(2) {
        applications.push(application("Even", Some("2.0")));
    }
    applications.push(application(&format!("Tool {}", n % 5), None));
    snapshot(
        &format!("SER-{n}"),
        &format!("10.9.{}.{}", n / 200, n % 200 + 1),
        applications,
    )
}

fn fleet_id(n: usize) -> String {
    format!("device-{n:04}")
}

fn fleet_name(n: usize) -> String {
    format!("Device {}", n % 37)
}

/// Adds devices `first..first + count` to the database directly, each with
/// its [`fleet_snapshot`].
async fn add_devices(app: &App, first: usize, count: usize) {
    let numbers = (first..first + count).collect::<Vec<_>>();
    for chunk in numbers.chunks(100) {
        let mut insert = sea_query::Query::insert();
        insert.into_table(sea_query::Alias::new("agents")).columns(
            [
                "id",
                "name",
                "platform",
                "auth_token_hash",
                "created_at",
                "updated_at",
            ]
            .map(sea_query::Alias::new),
        );
        for n in chunk {
            insert.values_panic([
                fleet_id(*n).into(),
                fleet_name(*n).into(),
                "windows".into(),
                "a".repeat(64).into(),
                1_800_000_000_000_i64.into(),
                1_800_000_000_000_i64.into(),
            ]);
        }
        app.db().execute(&insert).await.unwrap();
    }
    for n in numbers {
        let (stored, _) = store(app, &fleet_id(n), &fleet_snapshot(n)).await;
        assert!(matches!(stored, Some(Stored::Listed { .. })), "{stored:?}");
    }
}

/// A successful answer's body.
async fn get_json(browser: &mut Browser, path: &str) -> Value {
    let response = browser.get(path).await;
    assert_eq!(response.status, 200, "{path}: {:?}", response.body);
    response.body
}

/// Every entry of `list` a route returns for `query`, a page of `limit` at
/// a time from offset 0, and the `matching` each page said.
async fn every_page(
    browser: &mut Browser,
    route: &str,
    list: &str,
    query: &str,
    limit: usize,
) -> (Vec<Value>, Vec<i64>) {
    let mut entries = Vec::new();
    let mut matching = Vec::new();
    loop {
        let page = get_json(
            browser,
            &format!("{route}?{query}&limit={limit}&offset={}", entries.len()),
        )
        .await;
        matching.push(page["matching"].as_i64().unwrap());
        let found = page[list].as_array().unwrap().clone();
        assert!(found.len() <= limit, "{route}?{query}");
        if found.is_empty() {
            return (entries, matching);
        }
        entries.extend(found);
    }
}

fn device_ids(devices: &[Value]) -> Vec<String> {
    devices
        .iter()
        .map(|device| device["device_id"].as_str().unwrap().to_owned())
        .collect()
}

type Matches = Box<dyn Fn(usize) -> bool>;

#[tokio::test]
async fn device_searches_come_a_page_at_a_time_at_the_same_cost_for_any_fleet() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        // Authenticating the browser's first request is its own work.
        get_json(&mut admin, "/v1/system-info?limit=2").await;
        let routes = [
            "/v1/system-info?limit=2",
            "/v1/system-info?model=dell&limit=2&offset=1",
            "/v1/software?limit=2",
            "/v1/software/devices?name=Common&limit=2",
        ];
        let mut costs = Vec::new();
        for (first, count) in [(0, 3), (3, 250)] {
            add_devices(&app, first, count).await;
            // A session is marked seen at most once a minute, which adding
            // the devices may have taken: that write happens here rather
            // than in the requests measured.
            get_json(&mut admin, "/v1/system-info?limit=1").await;
            let mut cost = Vec::new();
            for route in routes {
                let before = app.db().counts();
                let page = get_json(&mut admin, route).await;
                cost.push(app.db().counts() - before);
                let list = if route.starts_with("/v1/software?") {
                    "applications"
                } else {
                    "devices"
                };
                assert_eq!(page[list].as_array().unwrap().len(), 2, "{name}: {route}");
            }
            costs.push(cost);
        }
        assert_eq!(costs[0], costs[1], "{name}: {routes:?}");
        assert!(costs[0].iter().all(|cost| cost.writes == 0), "{name}");
        let total = 253;

        // Paging through the fleet sees each device once, by name then ID.
        let (devices, matching) =
            every_page(&mut admin, "/v1/system-info", "devices", "", 100).await;
        assert!(
            matching.iter().all(|count| *count == total),
            "{name}: {matching:?}"
        );
        let mut expected: Vec<usize> = (0..253).collect();
        expected.sort_by_key(|n| (fleet_name(*n).to_lowercase(), fleet_id(*n)));
        let expected_ids: Vec<String> = expected.iter().map(|n| fleet_id(*n)).collect();
        assert_eq!(device_ids(&devices), expected_ids, "{name}");
        for device in &devices {
            let n: usize = device["device_id"].as_str().unwrap()[7..].parse().unwrap();
            assert_eq!(device["device_name"], fleet_name(n));
            assert_eq!(device["hardware"]["serial_number"], format!("SER-{n}"));
            assert_eq!(
                device["network"]["adapters"][0]["mac_address"],
                "00:15:5D:01:02:03"
            );
            assert_eq!(
                device["application_count"],
                if n.is_multiple_of(2) { 3 } else { 2 },
                "{name}"
            );
        }

        // A filter is counted and paged before the page is cut, so every
        // page is full until the last and none misses a match.
        let filters: [(&str, usize, Matches); 4] = [
            (
                "serial=ser-1",
                7,
                Box::new(|n| format!("ser-{n}").contains("ser-1")),
            ),
            ("application=EVEN", 50, Box::new(|n| n.is_multiple_of(2))),
            (
                "application=tool%203&q=device%203",
                3,
                Box::new(|n| n % 5 == 3 && fleet_name(n).to_lowercase().contains("device 3")),
            ),
            ("ip=10.9.1.", 40, Box::new(|n| n >= 200)),
        ];
        for (query, limit, matches) in filters {
            let (found, matching) =
                every_page(&mut admin, "/v1/system-info", "devices", query, limit).await;
            let wanted: Vec<String> = expected
                .iter()
                .filter(|n| matches(**n))
                .map(|n| fleet_id(*n))
                .collect();
            assert!(!wanted.is_empty(), "{query}");
            assert_eq!(device_ids(&found), wanted, "{name}: {query}");
            assert!(
                matching.iter().all(|count| *count == wanted.len() as i64),
                "{name}: {query}: {matching:?}"
            );
        }

        // At most 200 a page; 0 counts only.
        let biggest = get_json(&mut admin, "/v1/system-info?limit=500").await;
        assert_eq!(biggest["devices"].as_array().unwrap().len(), 200, "{name}");
        let counted = get_json(&mut admin, "/v1/system-info?limit=0&application=even").await;
        assert_eq!(counted["devices"], json!([]));
        assert_eq!(counted["matching"], 127, "{name}");
        let past = get_json(&mut admin, "/v1/system-info?offset=253").await;
        assert_eq!(past["devices"], json!([]));
        assert_eq!(past["matching"], total);
        for refused in [
            "/v1/system-info?page=2",
            "/v1/system-info?limit=-1",
            "/v1/software?page=2",
            "/v1/software/devices?name=Common&page=2",
        ] {
            assert_eq!(admin.get(refused).await.status, 400, "{refused}");
        }

        // The software and the devices that have it page the same way.
        let (applications, matching) =
            every_page(&mut admin, "/v1/software", "applications", "", 2).await;
        let names: Vec<_> = applications
            .iter()
            .map(|application| application["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "Common", "Even", "Tool 0", "Tool 1", "Tool 2", "Tool 3", "Tool 4"
            ],
            "{name}"
        );
        assert!(matching.iter().all(|count| *count == 7), "{name}");
        assert_eq!(applications[0]["devices"], total);
        assert_eq!(applications[1]["devices"], 127);
        let (copies, matching) = every_page(
            &mut admin,
            "/v1/software/devices",
            "devices",
            "name=Common",
            60,
        )
        .await;
        assert_eq!(device_ids(&copies), expected_ids, "{name}");
        assert!(matching.iter().all(|count| *count == total), "{name}");
        app.finish().await;
    }
}

/// The application count stored on the device's row, and the ones the
/// device's snapshot and the device search show.
async fn application_counts(app: &App, admin: &mut Browser, id: &str) -> [i64; 3] {
    let (stored,): (i64,) = app
        .db()
        .fetch_one(
            &sea_query::Query::select()
                .column(sea_query::Alias::new("application_count"))
                .from(sea_query::Alias::new("device_system_info"))
                .and_where(sea_query::Expr::col("device_id").eq(id))
                .to_owned(),
        )
        .await
        .unwrap();
    let snapshot = get_json(admin, &format!("/v1/agents/{id}/system-info")).await;
    let found = get_json(admin, "/v1/system-info?q=desk").await;
    [
        stored,
        snapshot["application_count"].as_i64().unwrap(),
        found["devices"][0]["application_count"].as_i64().unwrap(),
    ]
}

#[tokio::test]
async fn each_device_keeps_its_application_count_with_its_list() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let id = desk.device_id.as_str();
        // A first list, with one application from two sources.
        let first = snapshot(
            "SER-1",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("23.01")),
                published("Runtime", Some("1"), None, "windows"),
                published("Runtime", Some("1"), None, "windows_user"),
            ],
        );
        store(&app, id, &first).await;
        assert_eq!(
            application_counts(&app, &mut admin, id).await,
            [3; 3],
            "{name}"
        );
        let second = snapshot(
            "SER-1",
            "10.1.2.3",
            vec![
                application("7-Zip", Some("24.08")),
                application("VLC", None),
            ],
        );
        store(&app, id, &second).await;
        assert_eq!(
            application_counts(&app, &mut admin, id).await,
            [2; 3],
            "{name}"
        );
        // A snapshot naming the stored list leaves the count alone, and one
        // naming another list keeps the stored one's.
        let (stored, _) = store(&app, id, &unchanged("SER-2", &hash_of(&second))).await;
        assert_eq!(stored, Some(Stored::Unchanged));
        assert_eq!(
            application_counts(&app, &mut admin, id).await,
            [2; 3],
            "{name}"
        );
        store(&app, id, &unchanged("SER-3", "another list")).await;
        assert_eq!(
            application_counts(&app, &mut admin, id).await,
            [2; 3],
            "{name}"
        );
        // An empty list replaces it.
        store(&app, id, &snapshot("SER-4", "10.1.2.3", vec![])).await;
        assert_eq!(
            application_counts(&app, &mut admin, id).await,
            [0; 3],
            "{name}"
        );
        // A device first stored from a snapshot naming a list has none.
        let laptop = app.enroll(&mut admin, "Laptop").await;
        store(&app, &laptop.device_id, &unchanged("SER-5", "a list")).await;
        let found = get_json(&mut admin, "/v1/system-info?q=laptop").await;
        assert_eq!(found["devices"][0]["application_count"], 0, "{name}");
        app.finish().await;
    }
}

type NameCounts = Vec<(String, i64)>;
type VersionCounts = Vec<(String, String, String, i64)>;

/// The fleet's software summary, sorted.
async fn software_summary(app: &App) -> (NameCounts, VersionCounts) {
    use sea_query::{Alias, Query};
    let mut names: NameCounts = app
        .db()
        .fetch_all(
            &Query::select()
                .columns([Alias::new("name"), Alias::new("devices")])
                .from(Alias::new("software_names"))
                .to_owned(),
        )
        .await
        .unwrap();
    let mut versions: VersionCounts = app
        .db()
        .fetch_all(
            &Query::select()
                .columns(["name", "version", "publisher", "devices"].map(Alias::new))
                .from(Alias::new("software_versions"))
                .to_owned(),
        )
        .await
        .unwrap();
    names.sort();
    versions.sort();
    (names, versions)
}

/// What the summary must be: the devices' rows grouped afresh.
async fn grouped_applications(app: &App) -> (NameCounts, VersionCounts) {
    use sea_query::{Alias, Expr, Query};
    let mut names: NameCounts = app
        .db()
        .fetch_all(
            &Query::select()
                .column(Alias::new("name"))
                .expr(Expr::cust("COUNT(DISTINCT device_id)"))
                .from(Alias::new("device_applications"))
                .group_by_col(Alias::new("name"))
                .to_owned(),
        )
        .await
        .unwrap();
    let mut versions: VersionCounts = app
        .db()
        .fetch_all(
            &Query::select()
                .column(Alias::new("name"))
                .expr(Expr::cust("COALESCE(version, '')"))
                .expr(Expr::cust("COALESCE(publisher, '')"))
                .expr(Expr::cust("COUNT(DISTINCT device_id)"))
                .from(Alias::new("device_applications"))
                .group_by_col(Alias::new("name"))
                .group_by_col(Alias::new("version"))
                .group_by_col(Alias::new("publisher"))
                .to_owned(),
        )
        .await
        .unwrap();
    names.sort();
    versions.sort();
    (names, versions)
}

/// The summary is the grouping, and the fleet's software read from it is
/// what the same read of the devices' own rows finds.
async fn assert_summary(app: &App, admin: &mut Browser, step: &str) {
    let summary = software_summary(app).await;
    assert_eq!(
        summary,
        grouped_applications(app).await,
        "{}: {step}",
        app.name
    );
    // Every device here is in no company, so the company filter reads the
    // same devices from their rows.
    for query in ["", "q=fire&", "q=E&limit=2&offset=1&"] {
        let whole = get_json(admin, &format!("/v1/software?{query}")).await;
        let scoped = get_json(admin, &format!("/v1/software?{query}company=none")).await;
        assert_eq!(whole, scoped, "{}: {step}: {query}", app.name);
    }
}

#[tokio::test]
async fn the_fleet_software_summary_is_always_the_grouping_of_the_lists() {
    for app in apps().await {
        let mut admin = set_up(&app).await;
        let a = app.enroll(&mut admin, "A").await;
        let b = app.enroll(&mut admin, "B").await;
        let c = app.enroll(&mut admin, "C").await;
        let a_list = vec![
            published("7-Zip", Some("23.01"), Some("Igor Pavlov"), "windows"),
            published("Firefox", Some("130.0"), Some("Mozilla"), "windows"),
            published("Notepad++", None, None, "windows"),
        ];
        store(
            &app,
            &a.device_id,
            &snapshot("A", "10.0.0.1", a_list.clone()),
        )
        .await;
        // Side-by-side versions, and one version from two sources.
        let b_snapshot = snapshot(
            "B",
            "10.0.0.2",
            vec![
                published("Firefox", Some("130.0"), Some("Mozilla"), "windows"),
                published("Firefox", Some("131.0"), Some("Mozilla"), "windows_user"),
                published("Runtime", Some("8.0"), Some("Microsoft"), "windows"),
                published("Runtime", Some("8.0"), Some("Microsoft"), "windows_user"),
            ],
        );
        store(&app, &b.device_id, &b_snapshot).await;
        store(
            &app,
            &c.device_id,
            &snapshot(
                "C",
                "10.0.0.3",
                vec![
                    published("7-Zip", Some("23.01"), Some("Igor Pavlov"), "windows"),
                    published("Runtime", Some("6.0"), None, "windows"),
                ],
            ),
        )
        .await;
        assert_summary(&app, &mut admin, "first lists").await;
        let (names, _) = software_summary(&app).await;
        assert!(names.contains(&("Firefox".into(), 2)), "{names:?}");

        // An update, a removal and an install.
        store(
            &app,
            &a.device_id,
            &snapshot(
                "A",
                "10.0.0.1",
                vec![
                    published("7-Zip", Some("24.08"), Some("Igor Pavlov"), "windows"),
                    published("Firefox", Some("130.0"), Some("Mozilla"), "windows"),
                    published("VLC", Some("3.0.21"), None, "windows"),
                ],
            ),
        )
        .await;
        assert_summary(&app, &mut admin, "a changed list").await;

        // A list named instead of sent, and one replaced with an empty one.
        store(&app, &b.device_id, &unchanged("B", &hash_of(&b_snapshot))).await;
        assert_summary(&app, &mut admin, "an unchanged list").await;
        store(&app, &c.device_id, &snapshot("C", "10.0.0.3", vec![])).await;
        assert_summary(&app, &mut admin, "an empty list").await;

        // One copy of two going, and a publisher changing.
        store(
            &app,
            &b.device_id,
            &snapshot(
                "B",
                "10.0.0.2",
                vec![
                    published("Firefox", Some("131.0"), None, "windows_user"),
                    published("Runtime", Some("8.0"), Some("Microsoft"), "windows"),
                ],
            ),
        )
        .await;
        assert_summary(&app, &mut admin, "a copy removed").await;

        // A deleted device's applications leave the counts, and a device
        // enrolled again in its place adds them back.
        let deleted = admin.delete(&format!("/v1/agents/{}", a.device_id)).await;
        assert_eq!(deleted.status, 204, "{:?}", deleted.body);
        assert_summary(&app, &mut admin, "a device deleted").await;
        let (names, _) = software_summary(&app).await;
        assert!(!names.iter().any(|(name, _)| name == "VLC"), "{names:?}");
        let again = app.enroll(&mut admin, "A").await;
        store(&app, &again.device_id, &snapshot("A", "10.0.0.1", a_list)).await;
        assert_summary(&app, &mut admin, "a device enrolled again").await;
        system_info::forget(app.db(), &b.device_id).await.unwrap();
        assert_summary(&app, &mut admin, "a device's rows removed").await;
        system_info::forget(app.db(), &again.device_id)
            .await
            .unwrap();
        system_info::forget(app.db(), &c.device_id).await.unwrap();
        assert_eq!(
            software_summary(&app).await,
            (vec![], vec![]),
            "{}",
            app.name
        );
        app.finish().await;
    }
}
