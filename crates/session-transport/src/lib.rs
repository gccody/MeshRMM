//! Independent reliable streams with a sticky, backwards-compatible route.
pub mod identity;
pub mod queue;
#[cfg(any(test, feature = "test-support"))]
pub mod simulated_link;
#[cfg(any(test, feature = "test-support"))]
pub mod test_link;
use anyhow::Context;
use meshrmm_protocol::SessionMessage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, OnceCell};
use webrtc::data_channel::RTCDataChannel;

pub use queue::{MAX_IN_FLIGHT, MeteredChannel, SendQueue};

pub const CLIPBOARD_CHANNEL: &str = "meshrmm-clipboard-v1";
pub const FILE_CHANNEL: &str = "meshrmm-files-v1";
pub const CHAT_CHANNEL: &str = "meshrmm-chat-v1";
pub const SERVICE_CHANNELS: [&str; 3] = [CLIPBOARD_CHANNEL, FILE_CHANNEL, CHAT_CHANNEL];

pub fn service_label(message: &SessionMessage) -> Option<&'static str> {
    match message {
        SessionMessage::Clipboard { .. } | SessionMessage::ClipboardChunk { .. } => {
            Some(CLIPBOARD_CHANNEL)
        }
        SessionMessage::FileTransfer(_) => Some(FILE_CHANNEL),
        SessionMessage::Chat { .. } | SessionMessage::ChatAvailable => Some(CHAT_CHANNEL),
        _ => None,
    }
}

/// Writers wait while more than this is queued on a channel behind the network (see
/// [`SendQueue::backlog`]); what is in flight does not count. webrtc-sctp keeps one pending
/// queue of at most 128 KiB for every channel of a connection, sent in order and blocking
/// `send` when full, so this leaves at least half of it to the others and puts at most this
/// ahead of their messages. With what SCTP holds in flight a channel buffers at most this
/// plus [`MAX_IN_FLIGHT`].
pub const SEND_QUEUE_LIMIT: usize = 64 * 1024;

/// One observer per physical channel, shared by legacy service routes too.
/// Callbacks only wake consumers; no socket work runs in a callback.
#[derive(Clone)]
pub struct ServiceChannel {
    channel: MeteredChannel,
    changed: Arc<Notify>,
    low_buffer_wake: Arc<OnceCell<()>>,
}
impl std::ops::Deref for ServiceChannel {
    type Target = MeteredChannel;
    fn deref(&self) -> &Self::Target {
        &self.channel
    }
}
impl ServiceChannel {
    pub async fn new(channel: Arc<RTCDataChannel>) -> Self {
        let result = Self {
            channel: MeteredChannel::new(channel),
            changed: Arc::new(Notify::new()),
            low_buffer_wake: Arc::new(OnceCell::new()),
        };
        let changed = result.changed.clone();
        result.channel.on_open(Box::new(move || {
            changed.notify_waiters();
            Box::pin(async {})
        }));
        let changed = result.changed.clone();
        result.channel.on_close(Box::new(move || {
            changed.notify_waiters();
            Box::pin(async {})
        }));
        result
    }

    /// Wakes writers when the send buffer drains below its threshold, which `writable` sets.
    /// webrtc-rs keeps a handler set before a channel opens only for channels this side
    /// created: one the peer created reaches `on_data_channel` unopened and would lose it. So
    /// it is set once the channel is open.
    async fn wake_when_buffer_drains(&self) {
        self.low_buffer_wake
            .get_or_init(|| async {
                let changed = self.changed.clone();
                self.channel
                    .on_buffered_amount_low(Box::new(move || {
                        changed.notify_waiters();
                        Box::pin(async {})
                    }))
                    .await;
            })
            .await;
    }

    /// Application open/close handlers that replace ours must forward the wake.
    pub fn notifier(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    pub async fn wait_open(&self) -> anyhow::Result<()> {
        use webrtc::data_channel::data_channel_state::RTCDataChannelState;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.ready_state() {
                RTCDataChannelState::Open => return Ok(()),
                RTCDataChannelState::Closing | RTCDataChannelState::Closed => {
                    anyhow::bail!("session channel closed")
                }
                _ => changed.await,
            }
        }
    }

    /// Resolves once the channel is closing or closed. A channel that has not opened yet is
    /// not closed.
    pub async fn closed(&self) {
        use webrtc::data_channel::data_channel_state::RTCDataChannelState;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if matches!(
                self.ready_state(),
                RTCDataChannelState::Closing | RTCDataChannelState::Closed
            ) {
                return;
            }
            changed.await;
        }
    }

    /// Waits for room for as long as the channel stays open, and fails only once it is not
    /// open. The channel is reliable and ordered, so nothing is lost by waiting. A connection
    /// that stops carrying anything is ended by the session's own checks (its peer connection
    /// failing, or staying disconnected past the session's grace period), which close every
    /// channel and so wake this.
    pub async fn writable(&self) -> anyhow::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before inspecting state/capacity to avoid a lost wake.
            changed.as_mut().enable();
            use webrtc::data_channel::data_channel_state::RTCDataChannelState;
            anyhow::ensure!(
                self.ready_state() == RTCDataChannelState::Open,
                "session channel is not open"
            );
            self.wake_when_buffer_drains().await;
            let (buffered, backlog) = self.channel.backlog().await;
            if backlog < SEND_QUEUE_LIMIT {
                return Ok(());
            }
            // Wake once acknowledgements bring the backlog under the limit, at the rate
            // measured now. A slower rate only wakes this later, never past the buffer
            // falling under the limit.
            let threshold = buffered - (backlog - SEND_QUEUE_LIMIT) - 1;
            self.channel
                .set_buffered_amount_low_threshold(threshold)
                .await;
            if self.channel.buffered_amount().await <= threshold {
                continue;
            }
            changed.await;
        }
    }
}

#[derive(Default)]
pub struct ServiceRoute {
    dedicated: Mutex<Option<ServiceChannel>>,
    ready: AtomicBool,
    changed: Notify,
    selected: OnceCell<ServiceChannel>,
}

impl ServiceRoute {
    pub fn attach(&self, channel: ServiceChannel) {
        *self.dedicated.lock().unwrap() = Some(channel);
    }

    pub fn peer_ready(&self) {
        self.ready.store(true, Ordering::Release);
        self.changed.notify_one();
    }

    pub async fn resolve(&self, fallback: ServiceChannel) -> anyhow::Result<ServiceChannel> {
        if !self.ready.load(Ordering::Acquire) {
            fallback.wait_open().await?;
        }
        let selected = self
            .resolve_with_timeout(fallback, Duration::from_secs(2))
            .await;
        selected.wait_open().await?;
        Ok(selected)
    }

    async fn resolve_with_timeout(
        &self,
        fallback: ServiceChannel,
        deadline: Duration,
    ) -> ServiceChannel {
        self.selected
            .get_or_init(|| async {
                if !self.ready.load(Ordering::Acquire) {
                    let _ = tokio::time::timeout(deadline, self.changed.notified()).await;
                }
                let dedicated = self.dedicated.lock().unwrap().clone();
                if self.ready.load(Ordering::Acquire)
                    && let Some(channel) = dedicated
                {
                    tracing::info!(
                        channel = channel.label(),
                        "using independent service channel"
                    );
                    return channel;
                }
                tracing::info!(
                    "peer did not negotiate service channel; using legacy control stream"
                );
                fallback
            })
            .await
            .clone()
    }
}

/// Sends `message`, waiting for webrtc-sctp to take it for as long as the channel stays
/// open: its send blocks while the connection's pending queue is full. Fails once the channel
/// closes, as a connection that is gone closes it.
pub async fn send(channel: &ServiceChannel, message: SessionMessage) -> anyhow::Result<()> {
    let bytes = bytes::Bytes::from(message.encode().context("invalid session message")?);
    tokio::select! {
        sent = channel.send(&bytes) => {
            sent.context("session channel write failed")?;
        }
        () = channel.closed() => anyhow::bail!("session channel closed"),
    }
    Ok(())
}

/// Both peers announce support on the dedicated stream. This is not an echo
/// protocol: receiving Ready only marks the route; it never sends another Ready.
pub fn announce(channel: ServiceChannel) {
    tokio::spawn(async move {
        if channel.wait_open().await.is_ok() {
            let _ = send(&channel, SessionMessage::ServiceChannelReady).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_route_never_switches_mid_transfer() {
        let route = ServiceRoute::default();
        let legacy = ServiceChannel::new(Arc::new(RTCDataChannel::default())).await;
        let dedicated = ServiceChannel::new(Arc::new(RTCDataChannel::default())).await;
        route.attach(dedicated);
        let selected = route
            .resolve_with_timeout(legacy.clone(), Duration::from_millis(10))
            .await;
        assert!(Arc::ptr_eq(&selected, &legacy));
        route.peer_ready();
        assert!(Arc::ptr_eq(
            &route
                .resolve_with_timeout(legacy.clone(), Duration::ZERO)
                .await
                .channel,
            &legacy.channel
        ));
    }

    #[tokio::test]
    async fn negotiated_route_uses_dedicated_stream() {
        let route = ServiceRoute::default();
        let legacy = ServiceChannel::new(Arc::new(RTCDataChannel::default())).await;
        let dedicated = ServiceChannel::new(Arc::new(RTCDataChannel::default())).await;
        route.attach(dedicated.clone());
        route.peer_ready();
        assert!(Arc::ptr_eq(
            &route
                .resolve_with_timeout(legacy, Duration::ZERO)
                .await
                .channel,
            &dedicated.channel
        ));
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use webrtc::api::APIBuilder;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
    use webrtc::peer_connection::RTCPeerConnection;
    use webrtc::peer_connection::configuration::RTCConfiguration;

    async fn peer() -> RTCPeerConnection {
        let _ = rustls::crypto::ring::default_provider().install_default();
        APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap()
    }

    /// Offers from `a` and answers from `b`.
    async fn connect(a: &RTCPeerConnection, b: &RTCPeerConnection) {
        let mut gathered = a.gathering_complete_promise().await;
        a.set_local_description(a.create_offer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        b.set_remote_description(a.local_description().await.unwrap())
            .await
            .unwrap();
        let mut gathered = b.gathering_complete_promise().await;
        b.set_local_description(b.create_answer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        a.set_remote_description(b.local_description().await.unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn blocked_file_receiver_does_not_block_input_stream() {
        let a = peer().await;
        let b = peer().await;
        let entered = Arc::new(Notify::new());
        let (release, unblock) = tokio::sync::watch::channel(false);
        let (input, mut received) = tokio::sync::mpsc::channel(1);
        let blocked = entered.clone();
        b.on_data_channel(Box::new(move |channel| {
            let entered = blocked.clone();
            let release = unblock.clone();
            let input = input.clone();
            Box::pin(async move {
                let bulk = channel.label() == FILE_CHANNEL;
                channel.on_message(Box::new(move |message| {
                    let entered = entered.clone();
                    let mut release = release.clone();
                    let input = input.clone();
                    Box::pin(async move {
                        if bulk {
                            entered.notify_one();
                            let _ = release.wait_for(|released| *released).await;
                        } else {
                            let _ = input.send(message.data).await;
                        }
                    })
                }));
            })
        }));
        let files = a
            .create_data_channel(
                FILE_CHANNEL,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let controls = a
            .create_data_channel(meshrmm_protocol::CONTROL_CHANNEL_LABEL, None)
            .await
            .unwrap();
        let files = ServiceChannel::new(files).await;
        let controls = ServiceChannel::new(controls).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), files.wait_open())
                .await
                .is_err()
        );
        connect(&a, &b).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            let (files_open, controls_open) = tokio::join!(files.wait_open(), controls.wait_open());
            files_open.unwrap();
            controls_open.unwrap();
        })
        .await
        .unwrap();
        files
            .send(&bytes::Bytes::from(vec![0; 32 * 1024]))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        // Queue a burst while the application receiver is held. SCTP can still
        // acknowledge bytes, so capacity may recover before either waiter runs.
        for _ in 0..128 {
            tokio::time::timeout(
                Duration::from_secs(2),
                files.send(&bytes::Bytes::from(vec![0; 32 * 1024])),
            )
            .await
            .unwrap()
            .unwrap();
            if files.buffered_amount().await >= SEND_QUEUE_LIMIT {
                break;
            }
        }
        controls
            .send(&bytes::Bytes::from_static(b"input still works"))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), received.recv())
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            b"input still works"
        );
        let first = files.clone();
        let second = files.clone();
        let waiting =
            tokio::spawn(async move { tokio::join!(first.writable(), second.writable()) });
        release.send_replace(true);
        let (one, two) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        one.unwrap();
        two.unwrap();
        files.close().await.unwrap();
        assert!(files.wait_open().await.is_err());
        assert!(files.writable().await.is_err());
        a.close().await.unwrap();
        b.close().await.unwrap();
    }

    /// Two peers on a virtual network whose router delays every packet by `one_way`.
    async fn delayed_peers(one_way: Duration) -> (RTCPeerConnection, RTCPeerConnection) {
        use webrtc::api::setting_engine::SettingEngine;
        use webrtc::util::vnet::net::{Net, NetConfig};
        use webrtc::util::vnet::router::{Router, RouterConfig};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let wan = Arc::new(tokio::sync::Mutex::new(
            Router::new(RouterConfig {
                cidr: "1.2.3.0/24".to_owned(),
                min_delay: one_way,
                ..Default::default()
            })
            .unwrap(),
        ));
        let mut peers = Vec::new();
        for address in ["1.2.3.4", "1.2.3.5"] {
            let net = Arc::new(Net::new(Some(NetConfig {
                static_ips: vec![address.to_owned()],
                ..Default::default()
            })));
            let nic = net.get_nic().unwrap();
            wan.lock().await.add_net(Arc::clone(&nic)).await.unwrap();
            nic.lock().await.set_router(Arc::clone(&wan)).await.unwrap();
            let mut settings = SettingEngine::default();
            settings.set_vnet(Some(net));
            peers.push(
                APIBuilder::new()
                    .with_setting_engine(settings)
                    .build()
                    .new_peer_connection(RTCConfiguration::default())
                    .await
                    .unwrap(),
            );
        }
        wan.lock().await.start().await.unwrap();
        let b = peers.pop().unwrap();
        (peers.pop().unwrap(), b)
    }

    /// Bytes in flight are not queued: over a 100 ms round trip a transfer keeps more than
    /// the queue limit unacknowledged, which a limit on `buffered_amount` would not.
    #[tokio::test]
    async fn a_transfer_over_a_delayed_link_is_not_held_to_its_queue_limit_in_flight() {
        const CHUNK: usize = 16 * 1024;
        const TOTAL: usize = 8 * 1024 * 1024;
        let (a, b) = delayed_peers(Duration::from_millis(50)).await;
        let (arrived, mut received) = tokio::sync::watch::channel(0);
        b.on_data_channel(Box::new(move |channel| {
            let arrived = arrived.clone();
            Box::pin(async move {
                channel.on_message(Box::new(move |message| {
                    arrived.send_modify(|total| *total += message.data.len());
                    Box::pin(async {})
                }));
            })
        }));
        let files = ServiceChannel::new(
            a.create_data_channel(
                FILE_CHANNEL,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await
            .unwrap(),
        )
        .await;
        connect(&a, &b).await;
        tokio::time::timeout(Duration::from_secs(20), files.wait_open())
            .await
            .unwrap()
            .unwrap();
        let mut most_buffered = 0;
        tokio::time::timeout(Duration::from_secs(120), async {
            for _ in 0..TOTAL / CHUNK {
                files.writable().await.unwrap();
                most_buffered = most_buffered.max(files.buffered_amount().await);
                files
                    .send(&bytes::Bytes::from(vec![0; CHUNK]))
                    .await
                    .unwrap();
            }
            received.wait_for(|total| *total == TOTAL).await.unwrap();
        })
        .await
        .unwrap();
        let round_trip = files.round_trip().unwrap();
        assert!(
            round_trip >= Duration::from_millis(100),
            "the link's round trip measured {round_trip:?}"
        );
        // A limit on `buffered_amount` never lets it reach the limit before a write. How far
        // past it a transfer gets depends on how fast SCTP's window opens on this machine.
        assert!(
            most_buffered > 2 * SEND_QUEUE_LIMIT,
            "at most {most_buffered} bytes were unacknowledged when the channel took more"
        );
        a.close().await.unwrap();
        b.close().await.unwrap();
    }

    /// The viewer wraps the Agent's service channels in `on_data_channel`, which webrtc-rs calls
    /// before the channel opens. Its writers must still wake when the send buffer drains.
    #[tokio::test]
    async fn a_channel_the_peer_created_wakes_writers_when_its_buffer_drains() {
        const CHUNK: usize = 16 * 1024;
        const CHUNKS: usize = 64;
        let a = peer().await;
        let b = peer().await;
        let (accepted, mut incoming) = tokio::sync::mpsc::unbounded_channel();
        b.on_data_channel(Box::new(move |channel| {
            let accepted = accepted.clone();
            Box::pin(async move {
                let _ = accepted.send(ServiceChannel::new(channel).await);
            })
        }));
        let files = a
            .create_data_channel(
                FILE_CHANNEL,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let (arrived, mut received) = tokio::sync::watch::channel(0);
        files.on_message(Box::new(move |message| {
            arrived.send_modify(|total| *total += message.data.len());
            Box::pin(async {})
        }));
        connect(&a, &b).await;
        let files = tokio::time::timeout(Duration::from_secs(10), incoming.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), files.wait_open())
            .await
            .unwrap()
            .unwrap();
        for _ in 0..CHUNKS {
            files.writable().await.unwrap();
            files
                .send(&bytes::Bytes::from(vec![0; CHUNK]))
                .await
                .unwrap();
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            received.wait_for(|total| *total == CHUNK * CHUNKS),
        )
        .await
        .unwrap()
        .unwrap();
        a.close().await.unwrap();
        b.close().await.unwrap();
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;
    use crate::test_link::TestLink;

    const MESSAGES: u8 = 32;

    /// The `index`th of a burst, 16 KiB long, so a few fill the channel's queue limit.
    fn message(index: u8) -> SessionMessage {
        SessionMessage::FileTransfer(meshrmm_protocol::FileMessage::Chunk {
            id: 1,
            data: vec![index; 16 * 1024],
        })
    }

    async fn assert_burst_arrives(link: &mut TestLink) {
        for index in 0..MESSAGES {
            let (label, received) = link.next().await;
            assert_eq!(label, FILE_CHANNEL);
            assert_eq!(received, message(index));
        }
    }

    /// While the link carries nothing the channel has no room, past the five seconds that used
    /// to end its writers; once it carries again every message arrives, in order.
    #[tokio::test(start_paused = true)]
    async fn a_writer_waits_out_a_link_that_carries_nothing_for_ten_seconds() {
        let (mut link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
        let channel = channels[0].clone();
        link.cut();
        let writer = tokio::spawn(async move {
            for index in 0..MESSAGES {
                channel.writable().await?;
                send(&channel, message(index)).await?;
            }
            anyhow::Ok(())
        });
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!writer.is_finished(), "{:?}", writer.await);
        link.restore();
        assert_burst_arrives(&mut link).await;
        writer.await.unwrap().unwrap();
        link.close().await;
    }

    /// A send that finds webrtc-sctp's pending queue full waits for it as long as the channel
    /// is open.
    #[tokio::test(start_paused = true)]
    async fn a_send_waits_for_a_full_pending_queue_while_the_link_carries_nothing() {
        let (mut link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
        let channel = channels[0].clone();
        link.cut();
        let writer = tokio::spawn(async move {
            for index in 0..MESSAGES {
                send(&channel, message(index)).await?;
            }
            anyhow::Ok(())
        });
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!writer.is_finished(), "{:?}", writer.await);
        link.restore();
        assert_burst_arrives(&mut link).await;
        writer.await.unwrap().unwrap();
        link.close().await;
    }

    /// Closing the connection ends a writer waiting for room, and one blocked in a send.
    #[tokio::test(start_paused = true)]
    async fn closing_the_connection_ends_writers_waiting_on_it() {
        let (link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
        let channel = channels[0].clone();
        link.cut();
        let waiting = channel.clone();
        let room = tokio::spawn(async move {
            for index in 0..MESSAGES {
                waiting.writable().await?;
                send(&waiting, message(index)).await?;
            }
            anyhow::Ok(())
        });
        let blocked = tokio::spawn(async move {
            for index in 0..MESSAGES {
                send(&channel, message(index)).await?;
            }
            anyhow::Ok(())
        });
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!room.is_finished() && !blocked.is_finished());
        link.sender.close().await.unwrap();
        assert!(room.await.unwrap().is_err());
        assert!(blocked.await.unwrap().is_err());
        link.receiver.close().await.unwrap();
    }
}
