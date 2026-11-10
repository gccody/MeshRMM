//! How notification channels deliver a burst: in order, over few
//! connections, as a digest when many emails wait, a few webhook requests at
//! a time, and no more than a channel's queue holds.
mod common;

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::http::StatusCode;
use common::{App, Browser, FakeSmtp, apps, set_up};
use meshrmm_server::notifications::{
    self, Notification,
    email::DIGEST_THRESHOLD,
    queue::{MAX_WAITING, WEBHOOK_IN_FLIGHT},
};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};

fn notification(n: usize) -> Notification {
    let mut data = Map::new();
    data.insert("n".into(), json!(n));
    Notification {
        event: "alert.raised",
        summary: format!("Critical: Offline on PC-{n}"),
        body: format!("PC-{n} has been offline for 15 minutes.\n"),
        data,
    }
}

fn dispatch(app: &App, channel: &str, numbers: std::ops::Range<usize>) {
    for n in numbers {
        notifications::dispatch(&app.state, vec![channel.to_owned()], notification(n));
    }
}

async fn create_channel(admin: &mut Browser, channel: Value) -> String {
    let created = admin.post("/v1/notification-channels", channel).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

/// A webhook receiver that counts the requests open at once, holds each one
/// until `release` is true, and hands over each body.
struct Receiver {
    url: String,
    bodies: mpsc::UnboundedReceiver<Value>,
    open: Arc<AtomicUsize>,
    most_open: Arc<AtomicUsize>,
    release: watch::Sender<bool>,
    hold: Duration,
}

impl Receiver {
    async fn start(released: bool, hold: Duration) -> Self {
        let (sender, bodies) = mpsc::unbounded_channel();
        let open = Arc::<AtomicUsize>::default();
        let most_open = Arc::<AtomicUsize>::default();
        let (release, released_watch) = watch::channel(released);
        let router = axum::Router::new().route(
            "/hook",
            axum::routing::post({
                let (open, most_open) = (open.clone(), most_open.clone());
                move |body: axum::body::Bytes| {
                    let (sender, open, most_open) =
                        (sender.clone(), open.clone(), most_open.clone());
                    let mut released = released_watch.clone();
                    async move {
                        let now_open = open.fetch_add(1, Ordering::SeqCst) + 1;
                        most_open.fetch_max(now_open, Ordering::SeqCst);
                        released.wait_for(|released| *released).await.unwrap();
                        tokio::time::sleep(hold).await;
                        open.fetch_sub(1, Ordering::SeqCst);
                        sender.send(serde_json::from_slice(&body).unwrap()).ok();
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            url,
            bodies,
            open,
            most_open,
            release,
            hold,
        }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.bodies.recv())
            .await
            .expect("no webhook arrived")
            .unwrap()
    }

    async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(300) + self.hold, self.bodies.recv())
                .await
                .is_err(),
            "no webhook expected"
        );
    }
}

async fn quiet(smtp: &mut FakeSmtp) {
    assert!(
        tokio::time::timeout(Duration::from_millis(500), smtp.messages.recv())
            .await
            .is_err(),
        "no email expected"
    );
}

/// The channel's `last_delivery_error` once it contains `text`.
async fn delivery_error_containing(admin: &mut Browser, channel: &str, text: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let listed = admin.get("/v1/notification-channels").await.body;
        let error = listed["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|listed| listed["id"] == channel)
            .unwrap()["last_delivery_error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if error.contains(text) {
            return error;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the channel's error is {error:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn email_bursts_become_one_digest_and_small_ones_keep_their_order() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (_, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let mut smtp = FakeSmtp::start().await;
        let configured = admin
            .put(
                "/v1/settings/smtp",
                json!({ "host": "127.0.0.1", "port": smtp.port, "security": "none", "from": "Acme IT <rmm@example.com>" }),
            )
            .await;
        assert_eq!(configured.status, StatusCode::OK);
        let channel = create_channel(
            &mut admin,
            json!({ "name": "Admins", "kind": "email", "role_ids": ["administrator"], "user_ids": [tech_id] }),
        )
        .await;

        // A WAN outage: many devices go offline at once.
        let before = app.db().counts();
        dispatch(&app, &channel, 0..200);
        let mut digests = [smtp.next().await, smtp.next().await];
        digests.sort();
        quiet(&mut smtp).await;
        let writes = (app.db().counts() - before).writes;
        assert!(
            writes <= 2,
            "{name}: the channel was written {writes} times"
        );
        assert!(digests[0].contains("To: admin@example.com"), "{name}");
        assert!(digests[1].contains("To: tech@example.com"));
        for digest in &digests {
            assert!(
                digest.contains("Subject: 200 alerts raised on Acme IT"),
                "{name}: {digest}"
            );
            assert!(digest.contains("\nCritical: Offline on PC-0\nCritical: Offline on PC-1\n"));
            assert!(digest.contains("\nCritical: Offline on PC-199\n"));
            assert!(!digest.contains("has been offline"), "only the summaries");
        }
        let after_digests = smtp.connections.load(Ordering::SeqCst);
        assert!(after_digests <= 2, "{name}: {after_digests} connections");

        // At the threshold, one email per notification, as dispatched.
        dispatch(&app, &channel, 0..DIGEST_THRESHOLD);
        for n in 0..DIGEST_THRESHOLD {
            for to in ["admin@example.com", "tech@example.com"] {
                let message = smtp.next().await;
                assert!(message.contains(&format!("To: {to}")), "{name}: {message}");
                assert!(
                    message.contains(&format!("Subject: Critical: Offline on PC-{n}\n")),
                    "{name}: {n}: {message}"
                );
                assert!(message.contains(&format!("PC-{n} has been offline for 15 minutes.")));
            }
        }
        quiet(&mut smtp).await;
        let connections = smtp.connections.load(Ordering::SeqCst) - after_digests;
        assert!(
            connections <= 2,
            "{name}: {} messages over {connections} connections",
            2 * DIGEST_THRESHOLD
        );
        let listed = admin.get("/v1/notification-channels").await.body;
        assert!(listed["channels"][0]["last_delivery_at"].is_i64(), "{name}");
        assert!(listed["channels"][0]["last_delivery_error"].is_null());
        app.finish().await;
    }
}

#[tokio::test]
async fn webhook_bursts_keep_a_few_requests_open_and_deliver_each_once() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut receiver = Receiver::start(true, Duration::from_millis(20)).await;
        let channel = create_channel(
            &mut admin,
            json!({ "name": "On-call", "kind": "webhook", "webhook_url": receiver.url }),
        )
        .await;

        const BURST: usize = 100;
        let before = app.db().counts();
        dispatch(&app, &channel, 0..BURST);
        let mut seen = BTreeSet::new();
        for _ in 0..BURST {
            let body = receiver.next().await;
            let keys: BTreeSet<&str> = body
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                BTreeSet::from([
                    "event",
                    "delivery_id",
                    "sent_at",
                    "server",
                    "summary",
                    "text",
                    "n"
                ]),
                "{name}: {body}"
            );
            let n = body["n"].as_u64().unwrap() as usize;
            assert_eq!(body["event"], "alert.raised");
            assert_eq!(body["summary"], format!("Critical: Offline on PC-{n}"));
            assert_eq!(body["text"], body["summary"]);
            assert!(seen.insert(n), "{name}: {n} arrived twice");
        }
        receiver.quiet().await;
        // The channel's last delivery is written at most once a second.
        let writes = (app.db().counts() - before).writes;
        assert!(
            writes < 10,
            "{name}: the channel was written {writes} times for {BURST}"
        );
        assert_eq!(seen, (0..BURST).collect::<BTreeSet<_>>());
        let most_open = receiver.most_open.load(Ordering::SeqCst);
        assert!(
            most_open <= WEBHOOK_IN_FLIGHT,
            "{name}: {most_open} requests were open at once"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_stalled_channel_keeps_its_bound_and_says_how_many_it_dropped() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let mut receiver = Receiver::start(false, Duration::ZERO).await;
        let channel = create_channel(
            &mut admin,
            json!({ "name": "On-call", "kind": "webhook", "webhook_url": receiver.url }),
        )
        .await;

        // The receiver stops answering, then far more arrive than fit.
        dispatch(&app, &channel, 0..WEBHOOK_IN_FLIGHT);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while receiver.open.load(Ordering::SeqCst) < WEBHOOK_IN_FLIGHT {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{name}: the requests never came"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        dispatch(&app, &channel, WEBHOOK_IN_FLIGHT..MAX_WAITING + 25);

        receiver.release.send(true).unwrap();
        let mut seen = BTreeSet::new();
        for _ in 0..MAX_WAITING {
            seen.insert(receiver.next().await["n"].as_u64().unwrap() as usize);
        }
        receiver.quiet().await;
        assert_eq!(
            seen,
            (0..MAX_WAITING).collect::<BTreeSet<_>>(),
            "{name}: the oldest are kept"
        );
        let error = delivery_error_containing(&mut admin, &channel, "dropped").await;
        assert_eq!(
            error,
            format!("25 notifications were dropped because {MAX_WAITING} were already waiting"),
            "{name}"
        );
        app.finish().await;
    }
}
