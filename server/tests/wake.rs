//! Wake-on-LAN: waking offline devices through online devices on their
//! network, the relays' reports, devices coming online afterwards, waking
//! many at once, and policies that wake devices.
mod common;

use std::net::IpAddr;

use axum::http::StatusCode;
use common::{App, Browser, apps, audit_events, set_up, user_with};
use meshrmm_protocol_types::{AgentCommand, SystemInfo, WakePacket, WakePacketResult};
use meshrmm_server::{
    automation,
    realtime::{AgentConnection, ToAgent},
    time::{MINUTE_MS, SECOND_MS, now_ms},
    wake,
};
use sea_query::ExprTrait;
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
        meshrmm_server::system_info::store(&app.state.database, device_id, ip, info)
            .await
            .unwrap()
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
