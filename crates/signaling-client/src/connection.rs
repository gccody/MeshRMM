use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Owns network liveness independently of application command processing.
/// A full command queue disconnects instead of blocking pong handling or
/// allowing unbounded memory growth. Dropping the owner cancels the pump.
pub struct SignalingConnection {
    incoming: mpsc::Receiver<anyhow::Result<Message>>,
    outgoing: mpsc::Sender<(Message, tokio::sync::oneshot::Sender<anyhow::Result<()>>)>,
    task: tokio::task::JoinHandle<()>,
}

impl SignalingConnection {
    pub fn new(socket: crate::Socket) -> Self {
        Self::start(socket, Duration::from_secs(20), Duration::from_secs(60))
    }

    fn start(socket: crate::Socket, interval: Duration, liveness: Duration) -> Self {
        let (incoming_tx, incoming) = mpsc::channel(256);
        let (outgoing, mut outgoing_rx) =
            mpsc::channel::<(Message, tokio::sync::oneshot::Sender<anyhow::Result<()>>)>(64);
        let task = tokio::spawn(async move {
            let mut socket = socket;
            let mut heartbeat = tokio::time::interval(interval);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            heartbeat.tick().await;
            let mut last_message = tokio::time::Instant::now();
            let result: anyhow::Result<()> = async {
                loop {
                    tokio::select! {
                        message = socket.next() => {
                            let message = message.context("signaling connection closed")??;
                            last_message = tokio::time::Instant::now();
                            match message {
                                Message::Ping(payload) => send(&mut socket, Message::Pong(payload)).await?,
                                Message::Pong(_) => {},
                                Message::Close(frame) => return Err(crate::signaling_close_error(frame)),
                                message => incoming_tx.try_send(Ok(message))
                                    .map_err(|_| anyhow::anyhow!("signaling command queue full or closed"))?,
                            }
                        }
                        Some((message, done)) = outgoing_rx.recv() => {
                            match send(&mut socket, message).await {
                                Ok(()) => { let _ = done.send(Ok(())); }
                                Err(error) => {
                                    let _ = done.send(Err(anyhow::anyhow!("{error:#}")));
                                    return Err(error);
                                }
                            }
                        }
                        _ = heartbeat.tick() => {
                            anyhow::ensure!(last_message.elapsed() < liveness, "signaling liveness timeout");
                            send(&mut socket, Message::Ping(Default::default())).await?;
                        }
                    }
                }
            }.await;
            drop(socket);
            if let Err(error) = result {
                // Waiting here is safe: the socket is already being discarded.
                let _ = incoming_tx.send(Err(error)).await;
            }
        });
        Self {
            incoming,
            outgoing,
            task,
        }
    }

    pub async fn next(&mut self) -> Option<anyhow::Result<Message>> {
        self.incoming.recv().await
    }

    /// Acknowledges the actual socket flush, not merely enqueueing the message.
    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        let (done, result) = tokio::sync::oneshot::channel();
        tokio::time::timeout(Duration::from_secs(5), async {
            self.outgoing
                .send((message, done))
                .await
                .context("signaling writer closed")?;
            result.await.context("signaling writer stopped")?
        })
        .await
        .context("signaling send timed out")?
    }
}

async fn send(socket: &mut crate::Socket, message: Message) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), socket.send(message))
        .await
        .context("signaling socket write timed out")?
        .context("signaling socket write failed")
}

impl Drop for SignalingConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pair() -> (
        crate::Socket,
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            tokio_tungstenite::accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap()
        });
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        (client, server.await.unwrap())
    }

    #[tokio::test]
    async fn answers_pings_while_command_consumer_is_stalled() {
        let (client, mut server) = pair().await;
        let mut connection =
            SignalingConnection::start(client, Duration::from_secs(20), Duration::from_secs(60));
        server
            .send(Message::Text("slow session startup".into()))
            .await
            .unwrap();
        for sequence in 0..10u8 {
            server
                .send(Message::Ping(vec![sequence].into()))
                .await
                .unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(1), server.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(reply, Message::Pong(vec![sequence].into()));
        }
        assert_eq!(
            connection.next().await.unwrap().unwrap(),
            Message::Text("slow session startup".into())
        );
        connection.send(Message::Text("ack".into())).await.unwrap();
        assert_eq!(
            server.next().await.unwrap().unwrap(),
            Message::Text("ack".into())
        );
    }

    #[tokio::test]
    async fn detects_dead_peer_without_application_polling() {
        let (client, _server) = pair().await;
        let mut connection = SignalingConnection::start(
            client,
            Duration::from_millis(10),
            Duration::from_millis(40),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let error = connection.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("liveness timeout"));
    }

    #[tokio::test]
    async fn dropping_connection_stops_network_task() {
        let (client, mut server) = pair().await;
        let connection = SignalingConnection::new(client);
        drop(connection);
        let result = tokio::time::timeout(Duration::from_secs(1), server.next())
            .await
            .unwrap();
        assert!(result.is_none_or(|message| message.is_err()));
    }
}
