use std::sync::Arc;

use meshrmm_protocol::SessionMessage;
use meshrmm_session_transport::{ServiceChannel, ServiceRoute};
use tokio::sync::mpsc;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::RTCPeerConnection;

use super::ControlCommand;
use super::service_workers::WorkerQueues;

pub(super) async fn open_service_channels(
    peer: &RTCPeerConnection,
    routes: [(&'static str, Arc<ServiceRoute>); 3],
    queues: &WorkerQueues,
    control_tx: &mpsc::UnboundedSender<ControlCommand>,
) -> anyhow::Result<()> {
    for (label, route) in routes {
        let channel = peer
            .create_data_channel(
                label,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    protocol: Some(label.into()),
                    ..Default::default()
                }),
            )
            .await?;
        let channel = ServiceChannel::new(channel).await;
        route.attach(channel.clone());
        let clipboard = queues.clipboard.clone();
        let files = queues.files.clone();
        let chat = queues.chat.clone();
        let errors = control_tx.clone();
        channel.on_message(Box::new(move |message| {
            let route = route.clone();
            let clipboard = clipboard.clone();
            let files = files.clone();
            let chat = chat.clone();
            let errors = errors.clone();
            Box::pin(async move {
                match SessionMessage::decode(&message.data) {
                    Ok(SessionMessage::ServiceChannelReady) => route.peer_ready(),
                    Ok(message)
                        if meshrmm_session_transport::service_label(&message) == Some(label) =>
                    {
                        let accepted = match message {
                            SessionMessage::FileTransfer(message) => {
                                files.try_send(message).is_ok()
                            }
                            message @ (SessionMessage::Chat { .. }
                            | SessionMessage::ChatAvailable) => chat.try_send(message).is_ok(),
                            message => clipboard.try_send(message).is_ok(),
                        };
                        if !accepted {
                            let _ = errors.send(ControlCommand::MaintenanceError(format!(
                                "{label} queue full or closed"
                            )));
                        }
                    }
                    _ => tracing::warn!(label, "discarding invalid service-channel message"),
                }
            })
        }));
        meshrmm_session_transport::announce(channel);
    }
    Ok(())
}
