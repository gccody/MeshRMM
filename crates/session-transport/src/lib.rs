//! Independent reliable streams with a sticky, backwards-compatible route.
pub mod identity;
use anyhow::Context;
use meshrmm_protocol::SessionMessage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, OnceCell};
use webrtc::data_channel::RTCDataChannel;

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

/// Writers wait while more than this is queued on a channel.
const SEND_BUFFER_LIMIT: usize = 64 * 1024;

/// One observer per physical channel, shared by legacy service routes too.
/// Callbacks only wake consumers; no socket work runs in a callback.
#[derive(Clone)]
pub struct ServiceChannel {
    channel: Arc<RTCDataChannel>,
    changed: Arc<Notify>,
    low_buffer_wake: Arc<OnceCell<()>>,
}
impl std::ops::Deref for ServiceChannel {
    type Target = Arc<RTCDataChannel>;
    fn deref(&self) -> &Self::Target {
        &self.channel
    }
}
impl ServiceChannel {
    pub async fn new(channel: Arc<RTCDataChannel>) -> Self {
        let result = Self {
            channel,
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

    /// Wakes writers when the send buffer drains. webrtc-rs keeps a threshold and handler set
    /// before a channel opens only for channels this side created: one the peer created reaches
    /// `on_data_channel` unopened and would lose both. So they are set once the channel is open.
    async fn wake_when_buffer_drains(&self) {
        self.low_buffer_wake
            .get_or_init(|| async {
                self.channel
                    .set_buffered_amount_low_threshold(SEND_BUFFER_LIMIT - 1)
                    .await;
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

    pub async fn writable(&self) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
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
                if self.buffered_amount().await < SEND_BUFFER_LIMIT {
                    return Ok(());
                }
                changed.await;
            }
        })
        .await
        .context("session channel buffer remained full for five seconds")?
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

pub async fn send(channel: &RTCDataChannel, message: SessionMessage) -> anyhow::Result<()> {
    let bytes = bytes::Bytes::from(message.encode().context("invalid session message")?);
    tokio::time::timeout(Duration::from_secs(5), channel.send(&bytes))
        .await
        .context("session channel write timed out")?
        .context("session channel write failed")?;
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
            if files.buffered_amount().await >= SEND_BUFFER_LIMIT {
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
