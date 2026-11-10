//! Wake-on-LAN: waking offline devices through online devices on their
//! network, the relays' reports, devices coming online afterwards, waking
//! many at once, and policies that wake devices. Relays are looked up by
//! the subnets of the devices woken, which costs the same however many
//! devices are online elsewhere, and are those choosing from every online
//! device gives.
mod common;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::IpAddr,
};

use axum::http::StatusCode;
use common::{App, Browser, apps, audit_events, set_up, user_with};
use meshrmm_protocol_types::{
    AdapterKind, AgentCommand, SystemInfo, WakePacket, WakePacketResult, parse_mac,
};
use meshrmm_server::{
    audit::Actor,
    automation,
    realtime::{AgentConnection, ToAgent},
    time::{MINUTE_MS, SECOND_MS, now_ms},
    wake::{self, relays},
};
use sea_query::{Alias, ExprTrait, Query};
use serde_json::{Value, json};

/// A snapshot whose adapters are `adapters`: (name, kind, MAC, address,
/// prefix length).
fn snapshot(adapters: &[(&str, &str, Option<&str>, &str, u8)]) -> SystemInfo {
    let adapters = adapters
        .iter()
        .map(|(name, kind, mac, address, prefix_length)| {
            json!({
                "name": name, "kind": kind, "mac_address": mac, "up": true,
                "addresses": [{ "address": address, "prefix_length": prefix_length }],
            })
        })
        .collect::<Vec<_>>();
    serde_json::from_value(json!({
        "collected_at_unix_ms": 1_700_000_000_000_i64,
        "os": { "id": "windows", "name": "Windows 11 Pro", "version": "24H2", "architecture": "x86_64" },
        "hardware": { "memory_bytes": 1 },
        "network": { "hostname": "PC", "adapters": adapters },
    }))
    .unwrap()
}

/// Stores a snapshot as if the device's Agent sent it from `public_ip`.
async fn store(app: &App, device_id: &str, public_ip: &str, info: SystemInfo) {
    let ip: IpAddr = public_ip.parse().unwrap();
    assert!(
        meshrmm_server::system_info::store(&app.state.database, device_id, ip, info, None)
            .await
            .unwrap()
            .is_some()
    );
}

/// The next wake packets a stand-in relay connection was asked for.
async fn packets(connection: &mut AgentConnection) -> Vec<WakePacket> {
    loop {
        match connection
            .commands
            .recv()
            .await
            .expect("the relay was asked")
        {
            ToAgent::Command(AgentCommand::SendWakePackets { packets }) => return packets,
            ToAgent::Command(AgentCommand::CollectSystemInfo) => {}
            other => panic!("expected wake packets, got {other:?}"),
        }
    }
}

fn sent(packet: &WakePacket, source: &str) -> WakePacketResult {
    WakePacketResult {
        wake_id: packet.wake_id.clone(),
        broadcast: packet.broadcast.clone(),
        source_address: Some(source.to_owned()),
        error: None,
    }
}

/// Wakes `device_id` while `relay` answers as `answer` says.
async fn wake_answered(
    app: &App,
    browser: &mut Browser,
    device_id: &str,
    relay: (&str, &mut AgentConnection),
    answer: impl Fn(&WakePacket) -> WakePacketResult,
) -> (common::Response, Vec<WakePacket>) {
    let (relay_id, connection) = relay;
    let path = format!("/v1/agents/{device_id}/wake");
    let request = browser.post(&path, json!({}));
    let relaying = async {
        let packets = packets(connection).await;
        let results = packets.iter().map(answer).collect();
        wake::reported(&app.state, relay_id, results).await;
        packets
    };
    tokio::join!(request, relaying)
}

#[tokio::test]
async fn offline_devices_are_woken_through_online_devices_on_their_network() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let office = app.enroll(&mut admin, "OFFICE-PC").await;
        let mac = app.enroll_on(&mut admin, "Mac", "macos").await;
        let vm = app.enroll_on(&mut admin, "VM", "linux").await;
        store(
            &app,
            &office.device_id,
            "203.0.113.5",
            snapshot(&[
                (
                    "Wi-Fi",
                    "wireless",
                    Some("aa-00-00-00-00-02"),
                    "10.9.0.4",
                    16,
                ),
                (
                    "Ethernet",
                    "ethernet",
                    Some("AA:00:00:00:00:01"),
                    "192.168.1.23",
                    24,
                ),
                (
                    "vEthernet (WSL)",
                    "virtual",
                    Some("AA:00:00:00:00:03"),
                    "172.20.112.1",
                    20,
                ),
            ]),
        )
        .await;
        store(
            &app,
            &mac.device_id,
            "203.0.113.5",
            snapshot(&[(
                "en0",
                "wireless",
                Some("BB:00:00:00:00:01"),
                "192.168.1.140",
                24,
            )]),
        )
        .await;
        store(
            &app,
            &vm.device_id,
            "203.0.113.5",
            snapshot(&[(
                "eth0",
                "ethernet",
                Some("CC:00:00:00:00:01"),
                "172.29.0.7",
                20,
            )]),
        )
        .await;
        let mut mac_connection = app.state.agents.connect(&mac.device_id);
        let _vm_connection = app.state.agents.connect(&vm.device_id);

        let (woken, packets) = wake_answered(
            &app,
            &mut admin,
            &office.device_id,
            (&mac.device_id, &mut mac_connection),
            |packet| sent(packet, "192.168.1.140"),
        )
        .await;
        assert_eq!(
            woken.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            woken.body
        );
        let wake_id = woken.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            packets,
            [WakePacket {
                wake_id: wake_id.clone(),
                mac_addresses: vec!["AA:00:00:00:00:01".into(), "AA:00:00:00:00:02".into()],
                broadcast: "192.168.1.255".into(),
                prefix_length: 24,
            }],
            "{name}: the virtual adapter's address isn't woken, and wired comes first"
        );
        assert_eq!(woken.body["status"], "sent", "{name}");
        assert_eq!(woken.body["device_name"], "OFFICE-PC");
        assert_eq!(woken.body["requested_by"], "Ada Admin");
        assert_eq!(woken.body["online_at"], Value::Null);
        let relays = woken.body["relays"].as_array().unwrap();
        assert_eq!(relays.len(), 1, "{name}: the VM is on another network");
        assert_eq!(relays[0]["name"], "Mac");
        assert_eq!(relays[0]["status"], "sent");
        assert_eq!(relays[0]["source_address"], "192.168.1.140");
        let created = woken.body["created_at"].as_i64().unwrap();
        assert_eq!(
            woken.body["watch_until"].as_i64().unwrap(),
            created + wake::ONLINE_WINDOW_MS
        );

        let audited = audit_events(&app, "agent.wake").await;
        assert_eq!(audited.len(), 1, "{name}");
        assert_eq!(audited[0].target_id, office.device_id);
        assert_eq!(audited[0].metadata["relays"], json!(["Mac"]));
        assert_eq!(audited[0].metadata["status"], "sending");

        // The device comes online: it was woken.
        let _office_connection = app.state.agents.connect(&office.device_id);
        wake::device_connected(&app.state, &office.device_id).await;
        let listed = admin.get(&format!("/v1/wakes?ids={wake_id}")).await;
        assert_eq!(listed.status, StatusCode::OK);
        assert!(listed.body["wakes"][0]["online_at"].is_i64(), "{name}");
        let recent = admin
            .get(&format!("/v1/agents/{}/wakes", office.device_id))
            .await;
        assert_eq!(recent.body["wakes"][0]["id"], wake_id.as_str());

        let again = admin
            .post(&format!("/v1/agents/{}/wake", office.device_id), json!({}))
            .await;
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(again.code(), "device_online");
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_no_relay_can_wake_say_why() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let unknown = app.enroll(&mut admin, "NEW-PC").await;
        let remote = app.enroll(&mut admin, "BRANCH-PC").await;
        let mac = app.enroll_on(&mut admin, "Mac", "macos").await;
        store(
            &app,
            &remote.device_id,
            "198.51.100.9",
            snapshot(&[(
                "Ethernet",
                "ethernet",
                Some("AA:00:00:00:00:01"),
                "10.20.0.5",
                16,
            )]),
        )
        .await;
        store(
            &app,
            &mac.device_id,
            "203.0.113.5",
            snapshot(&[(
                "en0",
                "wireless",
                Some("BB:00:00:00:00:01"),
                "192.168.1.140",
                24,
            )]),
        )
        .await;
        let mut mac_connection = app.state.agents.connect(&mac.device_id);

        let woken = admin
            .post(&format!("/v1/agents/{}/wake", unknown.device_id), json!({}))
            .await;
        assert_eq!(
            woken.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            woken.body
        );
        assert_eq!(woken.body["status"], "no_relay");
        assert!(
            woken.body["detail"]
                .as_str()
                .unwrap()
                .contains("hasn't sent its hardware and network details"),
            "{name}: {}",
            woken.body["detail"]
        );
        let woken = admin
            .post(&format!("/v1/agents/{}/wake", remote.device_id), json!({}))
            .await;
        assert_eq!(woken.body["status"], "no_relay", "{name}");
        assert_eq!(
            woken.body["detail"],
            "No online device is on BRANCH-PC's network (10.20.0.0/16). Wake-on-LAN packets don't cross routers, so another device with a MeshRMM Agent must be online on that network to send them."
        );
        assert_eq!(woken.body["relays"], json!([]));
        assert!(
            mac_connection.commands.try_recv().is_err(),
            "{name}: no relay was asked"
        );

        // A relay that can't send says why.
        store(
            &app,
            &mac.device_id,
            "198.51.100.9",
            snapshot(&[(
                "en0",
                "wireless",
                Some("BB:00:00:00:00:01"),
                "10.20.7.1",
                16,
            )]),
        )
        .await;
        let (failed, _) = wake_answered(
            &app,
            &mut admin,
            &remote.device_id,
            (&mac.device_id, &mut mac_connection),
            |packet| WakePacketResult {
                error: Some("this computer is no longer on 10.20.0.0/16".into()),
                ..sent(packet, "x")
            },
        )
        .await;
        assert_eq!(failed.body["status"], "failed", "{name}: {:?}", failed.body);
        assert_eq!(
            failed.body["detail"],
            "No relay could send the packets. Mac: this computer is no longer on 10.20.0.0/16"
        );
        assert_eq!(
            failed.body["relays"][0]["source_address"],
            Value::Null,
            "{name}: not an address"
        );

        // Only users who may wake devices can.
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let refused = viewer
            .post(&format!("/v1/agents/{}/wake", remote.device_id), json!({}))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        let refused = viewer
            .post("/v1/wakes", json!({ "device_ids": [remote.device_id] }))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let seen = viewer
            .get(&format!("/v1/agents/{}/wakes", remote.device_id))
            .await;
        assert_eq!(seen.body["wakes"].as_array().unwrap().len(), 2, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn many_devices_wake_at_once_through_one_request_per_relay() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let first = app.enroll(&mut admin, "PC-1").await;
        let second = app.enroll(&mut admin, "PC-2").await;
        let relay = app.enroll(&mut admin, "RELAY").await;
        for (agent, mac, address) in [
            (&first, "AA:00:00:00:00:01", "192.168.1.21"),
            (&second, "AA:00:00:00:00:02", "192.168.1.22"),
            (&relay, "AA:00:00:00:00:03", "192.168.1.2"),
        ] {
            store(
                &app,
                &agent.device_id,
                "203.0.113.5",
                snapshot(&[("Ethernet", "ethernet", Some(mac), address, 24)]),
            )
            .await;
        }
        let mut relay_connection = app.state.agents.connect(&relay.device_id);
        let missing = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a";

        let request = admin.post(
            "/v1/wakes",
            json!({ "device_ids": [first.device_id, second.device_id, relay.device_id, missing] }),
        );
        let relaying = async {
            let packets = packets(&mut relay_connection).await;
            let results = packets
                .iter()
                .map(|packet| sent(packet, "192.168.1.2"))
                .collect();
            wake::reported(&app.state, &relay.device_id, results).await;
            packets
        };
        let (woken, packets) = tokio::join!(request, relaying);
        assert_eq!(
            woken.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            woken.body
        );
        assert_eq!(packets.len(), 2, "{name}: one command carries both");
        let wakes = woken.body["wakes"].as_array().unwrap();
        assert_eq!(wakes.len(), 2);
        assert!(
            wakes.iter().all(|wake| wake["status"] == "sent"),
            "{name}: {wakes:?}"
        );
        let mut skipped = woken.body["skipped"].as_array().unwrap().clone();
        skipped.sort_by_key(|skipped| skipped["reason"].as_str().unwrap().to_owned());
        assert_eq!(
            skipped,
            [
                json!({ "device_id": missing, "reason": "not_found" }),
                json!({ "device_id": relay.device_id, "reason": "online" }),
            ]
        );
        assert_eq!(audit_events(&app, "agent.wake").await.len(), 2);
        let empty = admin.post("/v1/wakes", json!({ "device_ids": [] })).await;
        assert_eq!(empty.status, StatusCode::BAD_REQUEST);
        assert_eq!(empty.code(), "invalid_wake");
        app.finish().await;
    }
}

#[tokio::test]
async fn policies_wake_offline_devices_and_jobs_report_whether_they_came_online() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let asleep = app.enroll(&mut admin, "ASLEEP").await;
        app.enroll(&mut admin, "STRANDED").await;
        let awake = app.enroll(&mut admin, "AWAKE").await;
        store(
            &app,
            &asleep.device_id,
            "203.0.113.5",
            snapshot(&[(
                "Ethernet",
                "ethernet",
                Some("AA:00:00:00:00:01"),
                "192.168.1.21",
                24,
            )]),
        )
        .await;
        store(
            &app,
            &awake.device_id,
            "203.0.113.5",
            snapshot(&[(
                "Ethernet",
                "ethernet",
                Some("AA:00:00:00:00:03"),
                "192.168.1.2",
                24,
            )]),
        )
        .await;
        let mut awake_connection = app.state.agents.connect(&awake.device_id);

        let enrolling = admin
            .post(
                "/v1/automation-policies",
                json!({ "name": "Wake for patching", "action": "wake", "target": "all", "trigger": "enroll", "offline": "skip" }),
            )
            .await;
        assert_eq!(enrolling.status, StatusCode::BAD_REQUEST, "{name}");
        let policy = admin
            .post(
                "/v1/automation-policies",
                json!({
                    "name": "Wake for patching", "action": "wake", "target": "all",
                    "trigger": "daily", "time": "01:50", "time_zone": "UTC", "offline": "wait",
                }),
            )
            .await;
        assert_eq!(
            policy.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            policy.body
        );
        assert_eq!(policy.body["action"], "wake");
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
        let job_id = started.body["id"].as_str().unwrap().to_owned();
        let now = now_ms();
        automation::pass(&app.state, now).await.unwrap();
        let first = packets(&mut awake_connection).await;
        assert_eq!(first.len(), 1, "{name}");
        assert_eq!(first[0].mac_addresses, ["AA:00:00:00:00:01"]);
        wake::reported(
            &app.state,
            &awake.device_id,
            vec![sent(&first[0], "192.168.1.2")],
        )
        .await;
        automation::pass(&app.state, now + SECOND_MS).await.unwrap();
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        assert_eq!(job["kind"], "wake", "{name}");
        assert_eq!(job["item_name"], "Wake devices");
        assert_eq!(job["offline"], "skip", "{name}: wake jobs never wait");
        let outcome = |job: &Value, device: &str| {
            job["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["device_name"] == device)
                .map(|entry| (entry["outcome"].clone(), entry["detail"].clone()))
                .unwrap()
        };
        assert_eq!(outcome(&job, "AWAKE").0, "skipped", "{name}");
        assert_eq!(
            outcome(&job, "ASLEEP").0,
            "running",
            "{name}: sent, waiting for it"
        );
        assert_eq!(outcome(&job, "STRANDED").0, "failed", "{name}");
        assert!(
            outcome(&job, "STRANDED")
                .1
                .as_str()
                .unwrap()
                .contains("hasn't sent its hardware and network details")
        );
        let entry = job["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["device_name"] == "ASLEEP")
            .unwrap()
            .clone();
        assert!(entry["wake_id"].is_string(), "{name}");

        // It comes online: the job succeeded there and finishes.
        let _asleep_connection = app.state.agents.connect(&asleep.device_id);
        wake::device_connected(&app.state, &asleep.device_id).await;
        automation::pass(&app.state, now + 2 * SECOND_MS)
            .await
            .unwrap();
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        assert_eq!(outcome(&job, "ASLEEP").0, "succeeded", "{name}");
        assert!(job["finished_at_unix_ms"].is_i64(), "{name}: {job}");

        // One that never comes online has no result once the window passes.
        drop(_asleep_connection);
        let again = admin
            .post(
                &format!(
                    "/v1/automation-policies/{}/run",
                    policy.body["id"].as_str().unwrap()
                ),
                json!({}),
            )
            .await;
        let job_id = again.body["id"].as_str().unwrap().to_owned();
        automation::pass(&app.state, now + 3 * SECOND_MS)
            .await
            .unwrap();
        let second = packets(&mut awake_connection).await;
        wake::reported(
            &app.state,
            &awake.device_id,
            vec![sent(&second[0], "192.168.1.2")],
        )
        .await;
        // Ten minutes pass.
        app.db()
            .execute(
                &sea_query::Query::update()
                    .table(sea_query::Alias::new("wake_requests"))
                    .value(
                        sea_query::Alias::new("created_at"),
                        now - wake::ONLINE_WINDOW_MS - MINUTE_MS,
                    )
                    .and_where(
                        sea_query::Expr::col(sea_query::Alias::new("job_id")).eq(job_id.as_str()),
                    )
                    .to_owned(),
            )
            .await
            .unwrap();
        automation::pass(&app.state, now_ms()).await.unwrap();
        let job = admin.get(&format!("/v1/jobs/{job_id}")).await.body;
        let (outcome, detail) = outcome(&job, "ASLEEP");
        assert_eq!(outcome, "no_result", "{name}");
        assert_eq!(
            detail,
            "The packets were sent, but the device didn't come online within 10 minutes"
        );
        app.finish().await;
    }
}

/// The first user's ID, to ask for wakes as.
async fn first_user(app: &App) -> String {
    let (id,): (String,) = app
        .db()
        .fetch_one(
            &Query::select()
                .column(Alias::new("id"))
                .from(Alias::new("users"))
                .limit(1)
                .to_owned(),
        )
        .await
        .unwrap();
    id
}

/// Adds devices straight to the database, quicker than through installers
/// for a fleet: each `(name, company)`. Returns their IDs in order.
async fn add_devices(app: &App, devices: &[(String, Option<&str>)]) -> Vec<String> {
    let now = now_ms();
    let ids = devices
        .iter()
        .map(|_| common::random_hex(16))
        .collect::<Vec<_>>();
    for (ids, devices) in ids.chunks(100).zip(devices.chunks(100)) {
        let mut insert = Query::insert();
        insert.into_table(Alias::new("agents")).columns(
            [
                "id",
                "name",
                "platform",
                "auth_token_hash",
                "company_id",
                "created_at",
                "updated_at",
            ]
            .map(Alias::new),
        );
        for (id, (name, company)) in ids.iter().zip(devices) {
            insert.values_panic([
                id.as_str().into(),
                name.as_str().into(),
                "windows".into(),
                common::random_hex(32).into(),
                company.map(str::to_owned).into(),
                now.into(),
                now.into(),
            ]);
        }
        app.db().execute(&insert).await.unwrap();
    }
    ids
}

/// Adds the companies `ids`, named after their IDs.
async fn add_companies(app: &App, ids: &[&str]) {
    let now = now_ms();
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new("companies"))
        .columns(["id", "name", "created_at", "updated_at"].map(Alias::new));
    for id in ids {
        insert.values_panic([(*id).into(), (*id).into(), now.into(), now.into()]);
    }
    app.db().execute(&insert).await.unwrap();
}

/// Every wake packet the stand-in connections were asked for since last
/// looked, by relay.
fn drain_packets(connections: &mut [(String, AgentConnection)]) -> Vec<(String, WakePacket)> {
    let mut all = Vec::new();
    for (device_id, connection) in connections {
        while let Ok(command) = connection.commands.try_recv() {
            if let ToAgent::Command(AgentCommand::SendWakePackets { packets }) = command {
                all.extend(
                    packets
                        .into_iter()
                        .map(|packet| (device_id.clone(), packet)),
                );
            }
        }
    }
    all
}

fn lan_address(address: &str, prefix_length: u8, kind: AdapterKind) -> relays::Address {
    relays::Address {
        address: address.parse().unwrap(),
        prefix_length,
        kind,
    }
}

/// The devices [`wake::neighbours`] finds for `address` (/24, wired),
/// online or not.
async fn neighbours_of(app: &App, address: &str) -> BTreeSet<String> {
    wake::neighbours(
        &mut app.db(),
        &[lan_address(address, 24, AdapterKind::Ethernet)],
        |_| true,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|candidate| candidate.device_id)
    .collect()
}

/// Adds and connects devices `numbers` online on other networks, each on a
/// subnet of its own.
async fn add_elsewhere(
    app: &App,
    numbers: std::ops::Range<usize>,
) -> Vec<(String, AgentConnection)> {
    let names = numbers
        .clone()
        .map(|n| (format!("ELSEWHERE-{n}"), None))
        .collect::<Vec<_>>();
    let ids = add_devices(app, &names).await;
    let mut connections = Vec::new();
    for (n, id) in numbers.zip(&ids) {
        let address = format!("10.{}.{}.5", n / 200, n % 200);
        store(
            app,
            id,
            "203.0.113.5",
            snapshot(&[("Ethernet", "ethernet", None, address.as_str(), 24)]),
        )
        .await;
        connections.push((id.clone(), app.state.agents.connect(id)));
    }
    connections
}

/// A device's stored addresses and their subnets' first addresses.
async fn stored_networks(app: &App, device_id: &str) -> Vec<(String, Option<i64>)> {
    app.db()
        .fetch_all(
            &Query::select()
                .columns(["address", "network"].map(Alias::new))
                .from(Alias::new("device_ip_addresses"))
                .and_where(sea_query::Expr::col(Alias::new("device_id")).eq(device_id))
                .order_by(Alias::new("address"), sea_query::Order::Asc)
                .to_owned(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn waking_a_device_costs_the_same_however_many_devices_elsewhere_are_online() {
    for app in apps().await {
        let name = app.name;
        let _admin = set_up(&app).await;
        let user_id = first_user(&app).await;
        let ids = add_devices(
            &app,
            &[("ASLEEP".to_owned(), None), ("RELAY".to_owned(), None)],
        )
        .await;
        let (asleep, relay) = (&ids[0], &ids[1]);
        for (id, mac, address) in [
            (asleep, "AA:00:00:00:00:01", "192.168.1.23"),
            (relay, "AA:00:00:00:00:02", "192.168.1.2"),
        ] {
            store(
                &app,
                id,
                "203.0.113.5",
                snapshot(&[("Ethernet", "ethernet", Some(mac), address, 24)]),
            )
            .await;
        }
        let mut connections = vec![(relay.clone(), app.state.agents.connect(relay))];
        let requester = wake::Requester {
            user_id: &user_id,
            job_id: None,
            actor: None,
        };

        connections.extend(add_elsewhere(&app, 0..5).await);
        let before = app.db().counts();
        wake::start(&app.state, std::slice::from_ref(asleep), requester)
            .await
            .unwrap();
        let few = app.db().counts() - before;
        let packets = drain_packets(&mut connections);
        assert_eq!(packets.len(), 1, "{name}");
        assert_eq!(&packets[0].0, relay, "{name}");

        connections.extend(add_elsewhere(&app, 5..40).await);
        // And Agents of a thousand more, as far as the wake can tell.
        let _others = (0..1000)
            .map(|n| app.state.agents.connect(&format!("elsewhere-{n}")))
            .collect::<Vec<_>>();
        let before = app.db().counts();
        wake::start(&app.state, std::slice::from_ref(asleep), requester)
            .await
            .unwrap();
        let many = app.db().counts() - before;
        assert_eq!(drain_packets(&mut connections).len(), 1, "{name}");
        assert_eq!(
            many, few,
            "{name}: 1040 devices online elsewhere cost what 5 did"
        );
        // Reading the device, then the devices on its subnets, and recording
        // the request and its relay's subnet.
        assert_eq!(few.statements, 4, "{name}: {few:?}");
        // None of the others' addresses is read at all.
        assert_eq!(
            neighbours_of(&app, "192.168.1.23").await,
            BTreeSet::from([asleep.clone(), relay.clone()]),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_are_found_under_their_latest_addresses_until_deleted() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let moved = app.enroll(&mut admin, "MOVED").await;
        store(
            &app,
            &moved.device_id,
            "203.0.113.5",
            snapshot(&[
                ("Ethernet", "ethernet", None, "192.168.5.50", 24),
                ("Ethernet", "ethernet", None, "fe80::1", 64),
            ]),
        )
        .await;
        assert_eq!(
            stored_networks(&app, &moved.device_id).await,
            [
                ("192.168.5.50".to_owned(), Some(0xC0A8_0500)),
                ("fe80::1".to_owned(), None)
            ],
            "{name}: IPv6 addresses have no IPv4 subnet"
        );
        assert!(
            neighbours_of(&app, "192.168.5.9")
                .await
                .contains(&moved.device_id)
        );

        // A new report from another network.
        store(
            &app,
            &moved.device_id,
            "203.0.113.5",
            snapshot(&[("Ethernet", "ethernet", None, "10.30.0.50", 24)]),
        )
        .await;
        assert!(
            neighbours_of(&app, "192.168.5.9").await.is_empty(),
            "{name}"
        );
        assert_eq!(
            neighbours_of(&app, "10.30.0.9").await,
            BTreeSet::from([moved.device_id.clone()]),
            "{name}"
        );

        let deleted = admin
            .delete(&format!("/v1/agents/{}", moved.device_id))
            .await;
        assert!(deleted.status.is_success(), "{name}: {:?}", deleted.body);
        assert!(neighbours_of(&app, "10.30.0.9").await.is_empty(), "{name}");
        assert!(
            stored_networks(&app, &moved.device_id).await.is_empty(),
            "{name}: its rows went"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_busy_subnet_is_read_whole_a_page_at_a_time() {
    for app in apps().await {
        let name = app.name;
        let _admin = set_up(&app).await;
        let busy = 450;
        let names = (0..busy)
            .map(|n| (format!("busy-{n:03}"), None))
            .collect::<Vec<_>>();
        let ids = add_devices(&app, &names).await;
        // Their addresses written as snapshots would write them, in bulk.
        for (chunk, ids) in ids.chunks(100).enumerate() {
            let mut insert = Query::insert();
            insert
                .into_table(Alias::new("device_ip_addresses"))
                .columns(
                    [
                        "device_id",
                        "adapter",
                        "address",
                        "prefix_length",
                        "kind",
                        "network",
                    ]
                    .map(Alias::new),
                );
            for (n, id) in (chunk * 100..).zip(ids) {
                let address = format!("10.50.{}.{}", n / 250, n % 250 + 1);
                insert.values_panic([
                    id.as_str().into(),
                    "eth0".into(),
                    address.as_str().into(),
                    16_i64.into(),
                    "ethernet".into(),
                    relays::network(&address, 16).into(),
                ]);
            }
            app.db().execute(&insert).await.unwrap();
        }
        let mut connections = ids
            .iter()
            .map(|id| (id.clone(), app.state.agents.connect(id)))
            .collect::<Vec<_>>();
        let asleep = add_devices(&app, &[("ASLEEP".to_owned(), None)])
            .await
            .remove(0);
        store(
            &app,
            &asleep,
            SITE,
            snapshot(&[("Ethernet", "ethernet", MAC, "10.50.9.9", 16)]),
        )
        .await;

        let before = app.db().counts();
        let found = wake::neighbours(
            &mut app.db(),
            &[lan_address("10.50.9.9", 16, AdapterKind::Ethernet)],
            |_| true,
        )
        .await
        .unwrap();
        let cost = app.db().counts() - before;
        assert_eq!(found.len(), busy + 1, "{name}: all of them, and ASLEEP");
        assert_eq!(
            cost.statements,
            busy.div_ceil(wake::NEIGHBOURS_PAGE) as u64,
            "{name}: a page at a time"
        );
        let relays =
            wakes_match_the_reference(&app, std::slice::from_ref(&asleep), &mut connections).await;
        assert_eq!(
            relays[&asleep],
            ["busy-000", "busy-001", "busy-002"],
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_wake_job_costs_statements_by_its_devices_subnets_not_the_online_count() {
    let mut costs: HashMap<&str, Vec<meshrmm_server::db::Counts>> = HashMap::new();
    for unrelated in [5, 1000] {
        for app in apps().await {
            let name = app.name;
            let mut admin = set_up(&app).await;
            let mut devices = (0..12)
                .map(|n| (format!("ASLEEP-{n:02}"), None))
                .collect::<Vec<_>>();
            devices.extend([("RELAY-A".to_owned(), None), ("RELAY-B".to_owned(), None)]);
            let ids = add_devices(&app, &devices).await;
            for (n, id) in ids.iter().enumerate() {
                let address = match n {
                    12 => "192.168.10.2".to_owned(),
                    13 => "192.168.20.2".to_owned(),
                    n => format!("192.168.{}.{}", 10 * (1 + n % 2), 20 + n),
                };
                let mac = format!("AA:00:00:00:00:{n:02X}");
                store(
                    &app,
                    id,
                    "203.0.113.5",
                    snapshot(&[(
                        "Ethernet",
                        "ethernet",
                        Some(mac.as_str()),
                        address.as_str(),
                        24,
                    )]),
                )
                .await;
            }
            let mut connections = ids[12..]
                .iter()
                .map(|id| (id.clone(), app.state.agents.connect(id)))
                .collect::<Vec<_>>();
            // Agents of other devices, online as far as the wake knows.
            let _others = (0..unrelated)
                .map(|n| app.state.agents.connect(&format!("elsewhere-{n}")))
                .collect::<Vec<_>>();
            let policy = admin
                .post(
                    "/v1/automation-policies",
                    json!({
                        "name": "Wake for patching", "action": "wake", "target": "all",
                        "trigger": "daily", "time": "01:50", "time_zone": "UTC", "offline": "wait",
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
            let before = app.db().counts();
            automation::pass(&app.state, now_ms()).await.unwrap();
            costs
                .entry(name)
                .or_default()
                .push(app.db().counts() - before);
            assert_eq!(
                drain_packets(&mut connections).len(),
                12,
                "{name}: each asleep device is woken by the relay on its subnet"
            );
            app.finish().await;
        }
    }
    for (name, costs) in costs {
        assert_eq!(
            costs[0], costs[1],
            "{name}: 1000 Agents online elsewhere cost what 5 did"
        );
    }
}

/// An adapter of a fleet's device: its kind, MAC, address and prefix length.
type Adapter = (&'static str, Option<String>, String, u8);

/// A device of a fleet relays are chosen from.
#[derive(Clone)]
struct Device {
    name: String,
    company: Option<&'static str>,
    public_ip: &'static str,
    /// Its adapters. `None` sends no snapshot.
    adapters: Option<Vec<Adapter>>,
    online: bool,
    deleted: bool,
}

impl Device {
    fn new(
        name: &str,
        public_ip: &'static str,
        company: Option<&'static str>,
        adapters: &[(&'static str, Option<&str>, &str, u8)],
    ) -> Self {
        Self {
            name: name.to_owned(),
            company,
            public_ip,
            adapters: Some(
                adapters
                    .iter()
                    .map(|(kind, mac, address, prefix_length)| {
                        (
                            *kind,
                            mac.map(str::to_owned),
                            (*address).to_owned(),
                            *prefix_length,
                        )
                    })
                    .collect(),
            ),
            online: true,
            deleted: false,
        }
    }

    fn offline(self) -> Self {
        Self {
            online: false,
            ..self
        }
    }

    fn deleted(self) -> Self {
        Self {
            deleted: true,
            ..self
        }
    }
}

const SITE: &str = "203.0.113.5";
const BRANCH: &str = "198.51.100.7";
const MAC: Option<&str> = Some("AA:00:00:00:00:01");

/// A fleet made by hand, with a neighbour for each thing relay choice
/// tells apart around ASLEEP.
fn fleet_by_hand() -> Vec<Device> {
    let lan = |name: &str, address: &str, prefix_length: u8| {
        Device::new(
            name,
            SITE,
            None,
            &[("ethernet", None, address, prefix_length)],
        )
    };
    vec![
        Device::new(
            "ASLEEP",
            SITE,
            Some("acme"),
            &[
                ("ethernet", MAC, "192.168.1.23", 24),
                ("wireless", Some("AA:00:00:00:00:02"), "10.1.4.23", 16),
                ("virtual", Some("AA:00:00:00:00:03"), "172.17.0.5", 16),
            ],
        )
        .offline(),
        lan("Same subnet", "192.168.1.40", 24),
        lan("Wider", "192.168.0.9", 16),
        lan("Narrower", "192.168.1.20", 28),
        lan("Narrow elsewhere", "192.168.1.100", 28),
        Device::new("Docker", SITE, None, &[("virtual", None, "172.17.0.9", 16)]),
        Device::new(
            "Hyper-V",
            SITE,
            None,
            &[("virtual", None, "192.168.1.9", 24)],
        ),
        Device::new("Wi-Fi", SITE, None, &[("wireless", None, "10.1.9.9", 16)]),
        Device::new(
            "Two adapters",
            SITE,
            None,
            &[
                ("ethernet", None, "192.168.1.41", 24),
                ("other", None, "10.1.0.41", 16),
            ],
        ),
        Device::new(
            "acme desk",
            SITE,
            Some("acme"),
            &[("ethernet", None, "192.168.1.42", 24)],
        ),
        Device::new(
            "Acme Desk",
            SITE,
            Some("acme"),
            &[("ethernet", None, "192.168.1.43", 24)],
        ),
        Device::new(
            "Other company",
            SITE,
            Some("globex"),
            &[("ethernet", None, "192.168.1.44", 24)],
        ),
        Device::new(
            "Other site",
            BRANCH,
            None,
            &[("ethernet", None, "192.168.1.50", 24)],
        ),
        Device::new(
            "ACME OFFLINE",
            SITE,
            Some("acme"),
            &[("ethernet", None, "192.168.1.60", 24)],
        )
        .offline(),
        Device::new(
            "ACME DELETED",
            SITE,
            Some("acme"),
            &[("ethernet", None, "192.168.1.61", 24)],
        )
        .deleted(),
        Device::new(
            "REMOTE",
            BRANCH,
            None,
            &[("ethernet", MAC, "192.168.1.70", 24)],
        )
        .offline(),
        Device::new("LONELY", SITE, None, &[("ethernet", MAC, "10.200.0.5", 24)]).offline(),
        Device::new(
            "UNADDRESSED",
            SITE,
            None,
            &[
                ("ethernet", MAC, "fe80::1", 64),
                ("ethernet", MAC, "169.254.1.1", 16),
            ],
        )
        .offline(),
        Device {
            adapters: None,
            ..lan("UNKNOWN", "192.168.1.80", 24).offline()
        },
    ]
}

/// A random fleet on a few overlapping networks, the same for each seed,
/// with about `online` in 20 devices online.
fn random_fleet(seed: u64, size: usize, online: u64) -> Vec<Device> {
    let mut state = seed;
    let mut next = |below: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % below
    };
    let mut fleet = Vec::new();
    for n in 0..size {
        let name = format!(
            "{}-{n:03}",
            ["desk", "DESK", "Mac", "mac", "srv"][next(5) as usize]
        );
        let mut adapters = Vec::new();
        for _ in 0..=next(3) {
            let kind = ["ethernet", "wireless", "virtual", "other"][next(4) as usize];
            let mac = (next(7) > 0)
                .then(|| format!("AA:00:00:{:02X}:{:02X}:{:02X}", n / 256, n % 256, next(256)));
            let (address, prefix_length) = match next(7) {
                0 | 1 => (
                    format!("192.168.1.{}", next(256)),
                    [24, 24, 16, 25, 28, 30, 31][next(7) as usize],
                ),
                2 | 3 => (
                    format!("10.1.{}.{}", next(256), next(256)),
                    [16, 16, 20, 22, 24, 8][next(6) as usize],
                ),
                4 => (format!("172.17.0.{}", next(256)), 16),
                5 => (format!("192.168.2.{}", next(256)), 24),
                _ => (
                    ["fe80::9", "169.254.3.3", "127.0.0.1"][next(3) as usize].to_owned(),
                    16,
                ),
            };
            adapters.push((kind, mac, address, prefix_length));
        }
        fleet.push(Device {
            name,
            company: [None, Some("acme"), Some("globex")][next(3) as usize],
            public_ip: [SITE, SITE, BRANCH][next(3) as usize],
            adapters: (next(12) > 0).then_some(adapters),
            online: next(20) < online,
            deleted: next(12) == 0,
        });
    }
    fleet
}

/// Adds `fleet`, sends its snapshots and connects its online devices.
async fn add_fleet(app: &App, fleet: &[Device]) -> (Vec<String>, Vec<(String, AgentConnection)>) {
    let ids = add_devices(
        app,
        &fleet
            .iter()
            .map(|device| (device.name.clone(), device.company))
            .collect::<Vec<_>>(),
    )
    .await;
    for (id, device) in ids.iter().zip(fleet) {
        let Some(adapters) = &device.adapters else {
            continue;
        };
        let names = (0..adapters.len())
            .map(|n| format!("eth{n}"))
            .collect::<Vec<_>>();
        let adapters = adapters
            .iter()
            .zip(&names)
            .map(|((kind, mac, address, prefix_length), name)| {
                (
                    name.as_str(),
                    *kind,
                    mac.as_deref(),
                    address.as_str(),
                    *prefix_length,
                )
            })
            .collect::<Vec<_>>();
        store(app, id, device.public_ip, snapshot(&adapters)).await;
    }
    let deleted = ids
        .iter()
        .zip(fleet)
        .filter(|(_, device)| device.deleted)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if !deleted.is_empty() {
        app.db()
            .execute(
                &Query::update()
                    .table(Alias::new("agents"))
                    .value(Alias::new("deletion_requested_at"), now_ms())
                    .and_where(sea_query::Expr::col(Alias::new("id")).is_in(deleted))
                    .to_owned(),
            )
            .await
            .unwrap();
    }
    let connections = ids
        .iter()
        .zip(fleet)
        .filter(|(_, device)| device.online)
        .map(|(id, _)| (id.clone(), app.state.agents.connect(id)))
        .collect();
    (ids, connections)
}

/// The relays as the server chose them before it looked them up by subnet:
/// every online device's addresses were read and tried. Kept as the
/// reference the lookup must agree with.
async fn reference_relays(
    app: &App,
    targets: &[String],
    online: &[String],
) -> HashMap<String, Vec<relays::Chosen>> {
    let col =
        |table: &str, column: &str| sea_query::Expr::col((Alias::new(table), Alias::new(column)));
    let devices = |ids: &[String]| {
        Query::select()
            .expr(col("agents", "id"))
            .expr(col("agents", "name"))
            .expr(col("agents", "company_id"))
            .expr(col("device_system_info", "public_ip"))
            .expr(col("device_system_info", "details"))
            .from(Alias::new("agents"))
            .left_join(
                Alias::new("device_system_info"),
                col("device_system_info", "device_id")
                    .equals((Alias::new("agents"), Alias::new("id"))),
            )
            .and_where(col("agents", "id").is_in(ids.iter().cloned()))
            .and_where(col("agents", "deletion_requested_at").is_null())
            .to_owned()
    };
    type DeviceRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let candidates_rows: Vec<DeviceRow> = app.db().fetch_all(&devices(online)).await.unwrap();
    let addresses: Vec<(String, String, i64, String)> = app
        .db()
        .fetch_all(
            &Query::select()
                .columns(["device_id", "address", "prefix_length", "kind"].map(Alias::new))
                .from(Alias::new("device_ip_addresses"))
                .and_where(
                    sea_query::Expr::col(Alias::new("device_id")).is_in(online.iter().cloned()),
                )
                .to_owned(),
        )
        .await
        .unwrap();
    let mut candidates = candidates_rows
        .into_iter()
        .map(|(device_id, name, company_id, public_ip, _)| {
            (
                device_id.clone(),
                relays::Candidate {
                    device_id,
                    name,
                    public_ip,
                    company_id,
                    addresses: Vec::new(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for (device_id, address, prefix_length, kind) in addresses {
        let (Some(candidate), Ok(address), Ok(prefix_length)) = (
            candidates.get_mut(&device_id),
            address.parse::<std::net::Ipv4Addr>(),
            u8::try_from(prefix_length),
        ) else {
            continue;
        };
        candidate.addresses.push(relays::Address {
            address,
            prefix_length,
            kind: AdapterKind::parse(&kind),
        });
    }
    let candidates = candidates.into_values().collect::<Vec<_>>();
    let target_rows: Vec<DeviceRow> = app.db().fetch_all(&devices(targets)).await.unwrap();
    target_rows
        .into_iter()
        .map(|(id, _, company_id, public_ip, details)| {
            let info =
                details.and_then(|details| serde_json::from_str::<SystemInfo>(&details).ok());
            let chosen = info
                .filter(|info| {
                    info.network.adapters.iter().any(|adapter| {
                        adapter.kind != AdapterKind::Virtual
                            && adapter.mac_address.as_deref().and_then(parse_mac).is_some()
                    })
                })
                .map(|info| {
                    let addresses = info
                        .network
                        .adapters
                        .iter()
                        .flat_map(|adapter| {
                            adapter.addresses.iter().filter_map(|address| {
                                Some(relays::Address {
                                    address: address.address.parse().ok()?,
                                    prefix_length: address.prefix_length,
                                    kind: adapter.kind,
                                })
                            })
                        })
                        .filter(relays::Address::usable)
                        .collect::<Vec<_>>();
                    relays::choose(
                        &relays::Target {
                            device_id: &id,
                            public_ip: public_ip.as_deref(),
                            company_id: company_id.as_deref(),
                            addresses: &addresses,
                        },
                        &candidates,
                        wake::MAX_RELAYS,
                    )
                })
                .unwrap_or_default();
            (id, chosen)
        })
        .collect()
}

/// A relay as chosen: its ID, name and subnets (broadcast, prefix length).
type Chosen = (String, String, BTreeSet<(String, i64)>);

/// Wakes `targets` together and checks that each was given the relays, in
/// order, and each relay was asked for the packets, that choosing from
/// every online device gives. Returns each target's relays' names.
async fn wakes_match_the_reference(
    app: &App,
    targets: &[String],
    connections: &mut [(String, AgentConnection)],
) -> HashMap<String, Vec<String>> {
    let name = app.name;
    let user_id = first_user(app).await;
    let online = connections
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let expected = reference_relays(app, targets, &online)
        .await
        .into_iter()
        .map(|(target, chosen)| {
            let chosen = chosen
                .into_iter()
                .map(|relay| {
                    let subnets = relay
                        .subnets
                        .iter()
                        .map(|(broadcast, prefix_length)| {
                            (broadcast.to_string(), i64::from(*prefix_length))
                        })
                        .collect();
                    (relay.device_id, relay.name, subnets)
                })
                .collect::<Vec<Chosen>>();
            (target, chosen)
        })
        .collect::<HashMap<_, _>>();

    let actor = Actor {
        user_id: Some(user_id.clone()),
        label: "test".into(),
        ip: None,
        api_token: None,
    };
    let ids = wake::start(
        &app.state,
        targets,
        wake::Requester {
            user_id: &user_id,
            job_id: None,
            actor: Some(&actor),
        },
    )
    .await
    .unwrap();
    assert_eq!(ids.len(), expected.len(), "{name}: every enrolled target");
    let wake_ids = ids.values().cloned().collect::<Vec<_>>();
    let wakes = wake::by_ids(&mut app.db(), &wake_ids).await.unwrap();
    let audited = meshrmm_server::audit::list(
        &mut app.db(),
        &meshrmm_server::audit::Filter {
            action: Some("agent.wake".into()),
            ..Default::default()
        },
        10_000,
    )
    .await
    .unwrap();
    let mut packets: BTreeMap<(String, String), BTreeSet<(String, i64)>> = BTreeMap::new();
    for (relay, packet) in drain_packets(connections) {
        let wake = wakes.iter().find(|wake| wake.id == packet.wake_id).unwrap();
        assert_eq!(packet.mac_addresses, wake.mac_addresses, "{name}");
        packets
            .entry((relay, wake.device_id.clone()))
            .or_default()
            .insert((packet.broadcast, i64::from(packet.prefix_length)));
    }

    let mut names = HashMap::new();
    for (target, expected) in &expected {
        let wake = wakes.iter().find(|wake| &wake.device_id == target).unwrap();
        let order = audited
            .iter()
            .find(|event| event.metadata["wake_id"] == wake.id.as_str())
            .unwrap()
            .metadata["relays"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let actual = order
            .iter()
            .map(|relay_name| {
                let rows = wake
                    .relays
                    .iter()
                    .filter(|relay| &relay.name == relay_name)
                    .collect::<Vec<_>>();
                (
                    rows[0].device_id.clone(),
                    relay_name.clone(),
                    rows.iter()
                        .map(|relay| (relay.broadcast.clone(), relay.prefix_length))
                        .collect(),
                )
            })
            .collect::<Vec<Chosen>>();
        assert_eq!(
            &actual,
            expected,
            "{name}: the relays of {}",
            wake.device_name.as_deref().unwrap_or_default()
        );
        assert_eq!(
            wake.relays.len(),
            expected
                .iter()
                .map(|(_, _, subnets)| subnets.len())
                .sum::<usize>(),
            "{name}"
        );
        for (relay, _, subnets) in expected {
            assert_eq!(
                packets.remove(&(relay.clone(), target.clone())).as_ref(),
                Some(subnets),
                "{name}: a relay is asked for a packet on each subnet it shares"
            );
        }
        names.insert(target.clone(), order);
    }
    assert!(packets.is_empty(), "{name}: no other packets: {packets:?}");
    names
}

#[tokio::test]
async fn relays_are_those_choosing_from_every_online_device_gives() {
    // How many relays the random fleets' targets got, by database.
    let mut sizes: HashMap<&str, BTreeSet<usize>> = HashMap::new();
    for (fleet, by_hand) in [
        (fleet_by_hand(), true),
        (random_fleet(0x5EED_CAFE, 100, 9), false),
        (random_fleet(0xD1CE, 100, 2), false),
    ] {
        for app in apps().await {
            let name = app.name;
            let _admin = set_up(&app).await;
            add_companies(&app, &["acme", "globex"]).await;
            let (ids, mut connections) = add_fleet(&app, &fleet).await;
            let targets = ids
                .iter()
                .zip(&fleet)
                .filter(|(_, device)| !device.online && !device.deleted)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            // Together, as a job's batch is, and one by one, as from a
            // device's page.
            let together = wakes_match_the_reference(&app, &targets, &mut connections).await;
            for target in targets.iter().take(4) {
                let alone =
                    wakes_match_the_reference(&app, std::slice::from_ref(target), &mut connections)
                        .await;
                assert_eq!(alone[target], together[target], "{name}");
            }
            let relays_of = |device: &str| {
                let at = fleet.iter().position(|each| each.name == device).unwrap();
                together[&ids[at]].clone()
            };
            if by_hand {
                let asleep = relays_of("ASLEEP");
                assert_eq!(
                    asleep[..2]
                        .iter()
                        .map(|name| name.to_lowercase())
                        .collect::<Vec<_>>(),
                    ["acme desk", "acme desk"],
                    "{name}: the online ones of its company first: {asleep:?}"
                );
                assert_eq!(asleep[2], "Narrower", "{name}: then by name: {asleep:?}");
                assert_eq!(
                    relays_of("REMOTE"),
                    ["Other site"],
                    "{name}: the one at its site"
                );
                assert!(relays_of("LONELY").is_empty(), "{name}");
                assert!(relays_of("UNADDRESSED").is_empty(), "{name}");
                assert!(relays_of("UNKNOWN").is_empty(), "{name}");
            } else {
                sizes
                    .entry(name)
                    .or_default()
                    .extend(together.values().map(Vec::len));
            }
            app.finish().await;
        }
    }
    for (name, sizes) in sizes {
        assert_eq!(
            sizes,
            BTreeSet::from_iter(0..=wake::MAX_RELAYS),
            "{name}: targets with no relay, every number between, and the most"
        );
    }
}
