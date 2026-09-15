//! Independent reliable streams with a sticky, backwards-compatible route.
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

#[derive(Default)]
pub struct ServiceRoute {
    dedicated: Mutex<Option<Arc<RTCDataChannel>>>,
    ready: AtomicBool,
    changed: Notify,
    selected: OnceCell<Arc<RTCDataChannel>>,
}

impl ServiceRoute {
    pub fn attach(&self, channel: Arc<RTCDataChannel>) {
        *self.dedicated.lock().unwrap() = Some(channel);
    }

    pub fn peer_ready(&self) {
        self.ready.store(true, Ordering::Release);
        self.changed.notify_one();
    }

    pub async fn resolve(&self, fallback: Arc<RTCDataChannel>) -> Arc<RTCDataChannel> {
        while !self.ready.load(Ordering::Acquire) {
            use webrtc::data_channel::data_channel_state::RTCDataChannelState;
            match fallback.ready_state() {
                RTCDataChannelState::Open | RTCDataChannelState::Closed => break,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        self.resolve_with_timeout(fallback, Duration::from_secs(2))
            .await
    }

    async fn resolve_with_timeout(
        &self,
        fallback: Arc<RTCDataChannel>,
        deadline: Duration,
    ) -> Arc<RTCDataChannel> {
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
pub fn announce(channel: Arc<RTCDataChannel>) {
    if channel.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
    {
        tokio::spawn(async move {
            let _ = send(&channel, SessionMessage::ServiceChannelReady).await;
        });
    } else {
        let sender = channel.clone();
        channel.on_open(Box::new(move || {
            let sender = sender.clone();
            Box::pin(async move {
                let _ = send(&sender, SessionMessage::ServiceChannelReady).await;
            })
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_route_never_switches_mid_transfer() {
        let route = ServiceRoute::default();
        let legacy = Arc::new(RTCDataChannel::default());
        let dedicated = Arc::new(RTCDataChannel::default());
        route.attach(dedicated);
        let selected = route
            .resolve_with_timeout(legacy.clone(), Duration::from_millis(10))
            .await;
        assert!(Arc::ptr_eq(&selected, &legacy));
        route.peer_ready();
        assert!(Arc::ptr_eq(&route.resolve(legacy.clone()).await, &legacy));
    }

    #[tokio::test]
    async fn negotiated_route_uses_dedicated_stream() {
        let route = ServiceRoute::default();
        let legacy = Arc::new(RTCDataChannel::default());
        let dedicated = Arc::new(RTCDataChannel::default());
        route.attach(dedicated.clone());
        route.peer_ready();
        assert!(Arc::ptr_eq(&route.resolve(legacy).await, &dedicated));
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use webrtc::api::APIBuilder;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
    use webrtc::data_channel::data_channel_state::RTCDataChannelState;
    use webrtc::peer_connection::configuration::RTCConfiguration;

    #[tokio::test]
    async fn blocked_file_receiver_does_not_block_input_stream() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let a = APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap();
        let b = APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (input, mut received) = tokio::sync::mpsc::channel(1);
        let blocked = entered.clone();
        let unblock = release.clone();
        b.on_data_channel(Box::new(move |channel| {
            let entered = blocked.clone();
            let release = unblock.clone();
            let input = input.clone();
            Box::pin(async move {
                let bulk = channel.label() == FILE_CHANNEL;
                channel.on_message(Box::new(move |message| {
                    let entered = entered.clone();
                    let release = release.clone();
                    let input = input.clone();
                    Box::pin(async move {
                        if bulk {
                            entered.notify_one();
                            release.notified().await;
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
        tokio::time::timeout(Duration::from_secs(10), async {
            while files.ready_state() != RTCDataChannelState::Open
                || controls.ready_state() != RTCDataChannelState::Open
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
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
        release.notify_one();
        a.close().await.unwrap();
        b.close().await.unwrap();
    }
}
