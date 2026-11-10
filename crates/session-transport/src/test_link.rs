//! Two peers on an in-memory network that a test can cut and restore, for tests of what
//! waits on a channel while its link carries nothing. Nothing on it touches a real socket,
//! so it runs on tokio's paused clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use meshrmm_protocol::SessionMessage;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::util::vnet::net::{Net, NetConfig};
use webrtc::util::vnet::router::{Router, RouterConfig};

use crate::ServiceChannel;

pub struct TestLink {
    /// Creates the channels and writes on them.
    pub sender: RTCPeerConnection,
    /// Accepts the channels and passes on what arrives.
    pub receiver: RTCPeerConnection,
    cut: Arc<AtomicBool>,
    arrived: mpsc::UnboundedReceiver<(String, Result<SessionMessage, String>)>,
}

impl TestLink {
    /// Connects the peers with an ordered channel for each label, open on the sender.
    pub async fn connect(labels: &[&str]) -> (Self, Vec<ServiceChannel>) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let wan = Arc::new(tokio::sync::Mutex::new(
            Router::new(RouterConfig {
                cidr: "1.2.3.0/24".to_owned(),
                ..Default::default()
            })
            .unwrap(),
        ));
        let cut = Arc::new(AtomicBool::new(false));
        let dropping = cut.clone();
        wan.lock()
            .await
            .add_chunk_filter(Box::new(move |_| !dropping.load(Ordering::Acquire)))
            .await;
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
        let receiver = peers.pop().unwrap();
        let sender = peers.pop().unwrap();
        let (deliver, arrived) = mpsc::unbounded_channel();
        receiver.on_data_channel(Box::new(move |channel| {
            let deliver = deliver.clone();
            Box::pin(async move {
                let label = channel.label().to_owned();
                channel.on_message(Box::new(move |message| {
                    let message =
                        SessionMessage::decode(&message.data).map_err(|error| error.to_string());
                    let _ = deliver.send((label.clone(), message));
                    Box::pin(async {})
                }));
            })
        }));
        let mut channels = Vec::new();
        for label in labels {
            channels.push(
                ServiceChannel::new(
                    sender
                        .create_data_channel(
                            label,
                            Some(RTCDataChannelInit {
                                ordered: Some(true),
                                ..Default::default()
                            }),
                        )
                        .await
                        .unwrap(),
                )
                .await,
            );
        }
        negotiate(&sender, &receiver).await;
        for channel in &channels {
            channel.wait_open().await.unwrap();
        }
        let link = Self {
            sender,
            receiver,
            cut,
            arrived,
        };
        (link, channels)
    }

    /// Drops every packet either way until [`TestLink::restore`].
    pub fn cut(&self) {
        self.cut.store(true, Ordering::Release);
    }

    pub fn restore(&self) {
        self.cut.store(false, Ordering::Release);
    }

    /// The next message the receiver got, and the label of its channel.
    pub async fn next(&mut self) -> (String, SessionMessage) {
        let (label, message) = self.arrived.recv().await.unwrap();
        (label, message.expect("the receiver decodes every message"))
    }

    pub async fn close(&self) {
        self.sender.close().await.unwrap();
        self.receiver.close().await.unwrap();
    }
}

/// Offers from `a` and answers from `b`.
async fn negotiate(a: &RTCPeerConnection, b: &RTCPeerConnection) {
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
