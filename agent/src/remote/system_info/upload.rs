//! Posting snapshots to `POST /v1/agents/{device}/system-info` over HTTPS,
//! on a thread of their own, so a snapshot of megabytes that takes minutes
//! on a slow uplink holds up nothing else: not the control connection, whose
//! server gives up on a connection it hears nothing whole from for a minute,
//! not the resource usage timer, not the results the outbox posts.
//!
//! A snapshot replaces the one before it: only the newest that has not been
//! delivered is kept, in memory. One that a restart loses is collected again
//! (the server asks for one when its own is old). The answer says which
//! application list the server now holds, and whether it wants a new snapshot
//! to carry the list it lacks; the coordinator hears it through
//! [`Snapshots::new`]'s receiver.
//!
//! Answers mean what they mean to the outbox ([`super::super::outbox`]): a
//! success delivers the snapshot, a 401, 408, 429, 5xx or network error is
//! retried, a little later each time, and at once when the control
//! connection comes back or a newer snapshot arrives, and any other refusal
//! drops it, with a warning.
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use meshrmm_protocol::{SystemInfoReceipt, SystemInfoReport};

use crate::remote::outbox::{Sent, Verdict, backoff, verdict};

/// How long the upload may go without the socket taking a byte before it is
/// given up on and tried again. It has no limit in total. Like the control
/// connection's limit on a write, for the same reasons: a slow link's send
/// buffer drains in bursts, and TCP waits longer each time it retransmits.
const STALL: Duration = Duration::from_secs(60);
/// How long the server may take to answer once it has the whole snapshot:
/// it stores up to 10,000 applications first.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// How long the worker waits with nothing due.
const IDLE_WAKE: Duration = Duration::from_secs(60 * 60);

/// Posts a snapshot's body, returning the server's answer to one it took,
/// or why it did not take it.
pub(crate) trait Post {
    fn post(&self, body: &[u8]) -> Result<SystemInfoReceipt, Sent>;
}

#[derive(Default)]
struct State {
    /// The newest snapshot not yet delivered, with its number.
    pending: Option<(u64, Arc<[u8]>)>,
    /// The number the next snapshot gets.
    next: u64,
    /// Retryable failures in a row.
    failures: u32,
    /// No attempt before this.
    next_attempt: Option<Instant>,
    /// The worker should look again.
    wake: bool,
    /// The control connection came back during the current attempt.
    reconnected: bool,
}

pub(crate) struct Snapshots {
    state: Mutex<State>,
    changed: Condvar,
    receipts: tokio::sync::mpsc::UnboundedSender<SystemInfoReceipt>,
    jitter: fn(Duration) -> Duration,
}

impl Snapshots {
    /// Snapshots, and the answers the server gives them, in order.
    pub(crate) fn new(
        jitter: fn(Duration) -> Duration,
    ) -> (
        Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<SystemInfoReceipt>,
    ) {
        let (receipts, received) = tokio::sync::mpsc::unbounded_channel();
        let snapshots = Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            receipts,
            jitter,
        };
        (Arc::new(snapshots), received)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding it, so a poisoned one is whole.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sends `report`, in place of any snapshot not yet delivered. Returns
    /// at once.
    pub(crate) fn offer(&self, report: &SystemInfoReport) {
        let body = match serde_json::to_vec(report) {
            Ok(body) => body,
            Err(error) => {
                tracing::error!(%error, "could not encode the system information");
                return;
            }
        };
        let mut state = self.lock();
        if state.pending.is_some() {
            tracing::info!("a newer system information snapshot replaces one not yet sent");
        }
        let number = state.next;
        state.next += 1;
        state.pending = Some((number, body.into()));
        // Waiting for the server to come back is for the old one.
        state.failures = 0;
        state.next_attempt = None;
        state.wake = true;
        drop(state);
        self.changed.notify_all();
    }

    /// The control connection came back, and the server with it: a snapshot
    /// waiting for a retry goes now.
    pub(crate) fn connected(&self) {
        let mut state = self.lock();
        state.next_attempt = None;
        state.reconnected = true;
        state.wake = true;
        drop(state);
        self.changed.notify_all();
    }

    /// Delivers the newest snapshot with `poster` at `now`, and any that
    /// replaces it meanwhile, until none is left or a retry must wait.
    /// Returns when the next attempt is due, if one is left.
    fn deliver(&self, poster: &dyn Post, now: Instant) -> Option<Instant> {
        loop {
            let (number, body) = {
                let mut state = self.lock();
                let (number, body) = state.pending.clone()?;
                if let Some(at) = state.next_attempt.filter(|at| now < *at) {
                    return Some(at);
                }
                state.reconnected = false;
                (number, body)
            };
            let posted = poster.post(&body);
            let mut state = self.lock();
            let current = state
                .pending
                .as_ref()
                .is_some_and(|(pending, _)| *pending == number);
            let error = match posted {
                Ok(receipt) => {
                    tracing::info!(bytes = body.len(), "delivered the system information");
                    // The server's word on what it holds, whatever came after.
                    let _ = self.receipts.send(receipt);
                    if current {
                        state.pending = None;
                        state.failures = 0;
                    }
                    continue;
                }
                Err(error) => error,
            };
            if !current {
                // A newer one goes at once in its place.
                tracing::info!(
                    ?error,
                    "a system information snapshot that failed was replaced"
                );
                continue;
            }
            match verdict(&error) {
                Verdict::Taken | Verdict::Refused => {
                    tracing::warn!(
                        ?error,
                        "the server refused the system information; it is dropped"
                    );
                    state.pending = None;
                    state.failures = 0;
                }
                Verdict::Retry => {
                    state.failures = state.failures.saturating_add(1);
                    let wait = (self.jitter)(backoff(state.failures));
                    tracing::warn!(
                        ?error,
                        retry_ms = wait.as_millis() as u64,
                        "could not send the system information; retrying"
                    );
                    if !std::mem::take(&mut state.reconnected) {
                        state.next_attempt = Some(now + wait);
                    }
                }
            }
        }
    }

    /// Delivers with `poster` on a thread of its own, for as long as the
    /// Agent runs.
    pub(crate) fn spawn(self: &Arc<Self>, poster: impl Post + Send + 'static) {
        let snapshots = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-snapshots".into())
            .spawn(move || {
                loop {
                    let next = snapshots.deliver(&poster, Instant::now());
                    let mut state = snapshots.lock();
                    while !state.wake {
                        let wait = next
                            .map_or(IDLE_WAKE, |at| at.saturating_duration_since(Instant::now()))
                            .min(IDLE_WAKE);
                        if wait.is_zero() {
                            break;
                        }
                        let (guard, waited) = snapshots
                            .changed
                            .wait_timeout(state, wait)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state = guard;
                        if waited.timed_out() {
                            break;
                        }
                    }
                    state.wake = false;
                }
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start the thread that sends system information");
        }
    }
}

/// Posts over HTTPS with the Agent's credential, which the coordinator
/// replaces when it rotates.
pub(crate) struct Https {
    server: String,
    device_id: String,
    token: Arc<Mutex<String>>,
    http: ureq::Agent,
}

impl Https {
    pub(crate) fn new(config: &crate::remote::config::Config, token: Arc<Mutex<String>>) -> Self {
        Self {
            server: config.server.clone(),
            device_id: config.device_id.clone(),
            token,
            http: ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(30)))
                // Each write, headers and TLS too, not the body in total.
                .timeout_per_write(Some(STALL))
                .timeout_recv_response(Some(ANSWER_TIMEOUT))
                .timeout_recv_body(Some(Duration::from_secs(30)))
                .http_status_as_error(false)
                .tls_config(crate::enrollment::https_tls_config())
                .build()
                .new_agent(),
        }
    }
}

impl Post for Https {
    fn post(&self, body: &[u8]) -> Result<SystemInfoReceipt, Sent> {
        let url = meshrmm_signaling_client::endpoint_url(
            &self.server,
            &["v1", "agents", &self.device_id, "system-info"],
            &[],
            false,
        )
        .map_err(|error| Sent::Unreachable(format!("{error:#}")))?;
        let token = self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut response = self
            .http
            .post(url.as_str())
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|error| match error {
                ureq::Error::StatusCode(status) => Sent::Status(status),
                error => Sent::Unreachable(error.to_string()),
            })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Sent::Status(status));
        }
        // The server took it; without an answer the Agent knows of no list
        // the server holds, and sends its whole list next time.
        Ok(response
            .body_mut()
            .read_json::<SystemInfoReceipt>()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the server's answer to the system information is unreadable");
                SystemInfoReceipt::default()
            }))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use meshrmm_protocol::SystemInfo;

    use super::*;

    fn report(serial: &str) -> SystemInfoReport {
        let mut system = SystemInfo::default();
        system.hardware.serial_number = Some(serial.into());
        SystemInfoReport {
            system,
            applications_unchanged: None,
        }
    }

    fn serial(body: &[u8]) -> String {
        let report: SystemInfoReport = serde_json::from_slice(body).unwrap();
        report.system.hardware.serial_number.unwrap()
    }

    fn receipt(hash: &str) -> SystemInfoReceipt {
        SystemInfoReceipt {
            applications_hash: Some(hash.into()),
            collect_again: false,
        }
    }

    /// Answers each post with the next of its answers, then with a
    /// success, and remembers the serial number of each snapshot posted.
    #[derive(Default)]
    struct Server {
        answers: RefCell<VecDeque<Result<SystemInfoReceipt, Sent>>>,
        posted: RefCell<Vec<String>>,
        /// Called during each post, as the coordinator may offer meanwhile.
        during: RefCell<Option<Box<dyn FnMut()>>>,
    }

    impl Server {
        fn answering(answers: impl IntoIterator<Item = Result<SystemInfoReceipt, Sent>>) -> Self {
            Self {
                answers: RefCell::new(answers.into_iter().collect()),
                ..Self::default()
            }
        }

        fn posted(&self) -> Vec<String> {
            self.posted.borrow().clone()
        }
    }

    impl Post for Server {
        fn post(&self, body: &[u8]) -> Result<SystemInfoReceipt, Sent> {
            self.posted.borrow_mut().push(serial(body));
            if let Some(during) = self.during.borrow_mut().as_mut() {
                during();
            }
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(receipt("held")))
        }
    }

    fn snapshots() -> (
        Arc<Snapshots>,
        tokio::sync::mpsc::UnboundedReceiver<SystemInfoReceipt>,
    ) {
        Snapshots::new(|wait| wait)
    }

    #[test]
    fn a_snapshot_is_posted_and_its_answer_reaches_the_coordinator() {
        let (snapshots, mut receipts) = snapshots();
        let server = Server::answering([Ok(receipt("abc"))]);
        let now = Instant::now();
        assert_eq!(snapshots.deliver(&server, now), None, "nothing to send");
        snapshots.offer(&report("SER-1"));
        assert_eq!(snapshots.deliver(&server, now), None);
        assert_eq!(server.posted(), ["SER-1"]);
        assert_eq!(receipts.try_recv().unwrap(), receipt("abc"));
        // Delivered, so not sent again.
        assert_eq!(snapshots.deliver(&server, now), None);
        assert_eq!(server.posted().len(), 1);
    }

    #[test]
    fn a_failed_snapshot_is_retried_with_a_growing_wait() {
        let (snapshots, mut receipts) = snapshots();
        snapshots.offer(&report("SER-1"));
        let server = Server::answering([
            Err(Sent::Status(503)),
            Err(Sent::Unreachable("timed out".into())),
            Err(Sent::Status(408)),
        ]);
        let now = Instant::now();
        let first = now + backoff(1);
        assert_eq!(snapshots.deliver(&server, now), Some(first));
        // Nothing is sent before it is due.
        assert_eq!(
            snapshots.deliver(&server, first - Duration::from_millis(1)),
            Some(first)
        );
        assert_eq!(server.posted().len(), 1);
        let second = first + backoff(2);
        assert_eq!(snapshots.deliver(&server, first), Some(second));
        // A reconnect cuts the wait short.
        snapshots.connected();
        let reconnected = second - Duration::from_secs(1);
        let third = reconnected + backoff(3);
        assert_eq!(snapshots.deliver(&server, reconnected), Some(third));
        assert_eq!(snapshots.deliver(&server, third), None);
        assert_eq!(server.posted(), ["SER-1"; 4]);
        assert_eq!(receipts.try_recv().unwrap(), receipt("held"));
        assert!(receipts.try_recv().is_err());

        // A refusal drops it.
        snapshots.offer(&report("SER-2"));
        let server = Server::answering([Err(Sent::Status(409))]);
        assert_eq!(snapshots.deliver(&server, third), None);
        assert_eq!(snapshots.deliver(&server, third), None);
        assert_eq!(server.posted(), ["SER-2"]);
        assert!(receipts.try_recv().is_err());
    }

    #[test]
    fn a_newer_snapshot_replaces_one_not_yet_delivered() {
        let (snapshots, mut receipts) = snapshots();
        let now = Instant::now();
        // One waiting for a retry is replaced, and the newer goes at once.
        snapshots.offer(&report("SER-1"));
        let server = Server::answering([Err(Sent::Status(503))]);
        assert!(snapshots.deliver(&server, now).is_some());
        for serial in ["SER-2", "SER-3", "SER-4"] {
            snapshots.offer(&report(serial));
        }
        assert_eq!(snapshots.deliver(&server, now), None);
        assert_eq!(
            server.posted(),
            ["SER-1", "SER-4"],
            "only the newest is kept"
        );
        assert_eq!(receipts.try_recv().unwrap(), receipt("held"));

        // One that fails while a newer one arrives gives way to it at once,
        // with no wait.
        let server = Server::answering([Err(Sent::Status(503))]);
        offer_during_the_first_post(&server, &snapshots, "SER-6");
        snapshots.offer(&report("SER-5"));
        assert_eq!(snapshots.deliver(&server, now), None);
        assert_eq!(server.posted(), ["SER-5", "SER-6"]);
        assert_eq!(receipts.try_recv().unwrap(), receipt("held"));
        assert!(receipts.try_recv().is_err());

        // One that is delivered while a newer arrives still answers, and
        // the newer follows.
        let server = Server::default();
        offer_during_the_first_post(&server, &snapshots, "SER-8");
        snapshots.offer(&report("SER-7"));
        assert_eq!(snapshots.deliver(&server, now), None);
        assert_eq!(server.posted(), ["SER-7", "SER-8"]);
        assert_eq!(receipts.try_recv().unwrap(), receipt("held"));
        assert_eq!(receipts.try_recv().unwrap(), receipt("held"));
    }

    /// Offers a snapshot with `serial` while the server takes its first
    /// post, as the coordinator may.
    fn offer_during_the_first_post(server: &Server, snapshots: &Arc<Snapshots>, serial: &str) {
        let offering = Arc::clone(snapshots);
        let serial = serial.to_owned();
        let mut offered = false;
        *server.during.borrow_mut() = Some(Box::new(move || {
            if !std::mem::replace(&mut offered, true) {
                offering.offer(&report(&serial));
            }
        }));
    }

    /// Holds each post until the test lets it go.
    struct Gated {
        started: tokio::sync::mpsc::UnboundedSender<String>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Post for Gated {
        fn post(&self, body: &[u8]) -> Result<SystemInfoReceipt, Sent> {
            let _ = self.started.send(serial(body));
            let _ = self.release.lock().unwrap().recv();
            Ok(receipt("held"))
        }
    }

    /// A control connection, and the server's end of it.
    async fn control() -> (
        meshrmm_signaling_client::SignalingConnection,
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (client, _) = tokio_tungstenite::client_async_tls_with_config(
            format!("ws://{address}"),
            meshrmm_signaling_client::Tracked::new(stream),
            None,
            None,
        )
        .await
        .unwrap();
        (
            meshrmm_signaling_client::SignalingConnection::new(client),
            server.await.unwrap(),
        )
    }

    #[tokio::test]
    async fn an_upload_in_progress_holds_up_neither_the_control_connection_nor_a_newer_snapshot() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (snapshots, mut receipts) = snapshots();
        let (started, mut posts) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        snapshots.spawn(Gated {
            started,
            release: Mutex::new(gate),
        });
        let (connection, mut server) = control().await;
        snapshots.offer(&report("SER-1"));
        assert_eq!(posts.recv().await.unwrap(), "SER-1");

        // While it is held, the coordinator writes to its connection as
        // usual, and a newer snapshot is taken at once.
        connection
            .send(Message::Text("metrics".into()))
            .await
            .unwrap();
        assert_eq!(
            server.next().await.unwrap().unwrap(),
            Message::Text("metrics".into())
        );
        snapshots.offer(&report("SER-2"));
        connection
            .send(Message::Text("more metrics".into()))
            .await
            .unwrap();
        assert_eq!(
            server.next().await.unwrap().unwrap(),
            Message::Text("more metrics".into())
        );
        assert!(receipts.try_recv().is_err(), "the first is still going");

        // Once it ends, the newer follows.
        release.send(()).unwrap();
        assert_eq!(posts.recv().await.unwrap(), "SER-2");
        release.send(()).unwrap();
        assert_eq!(receipts.recv().await.unwrap(), receipt("held"));
        assert_eq!(receipts.recv().await.unwrap(), receipt("held"));
    }
}
