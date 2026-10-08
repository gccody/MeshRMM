//! A remote session's signaling socket. Signaling only negotiates the peer
//! connection, so once the peers are connected, losing the socket must not
//! end the session: proxies and networks routinely drop long-lived WebSockets. The
//! socket then reconnects in the background while the session streams.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::{ReconnectBackoff, SignalingConnection, Socket, SupersededClose};

type Connect =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Socket>> + Send>> + Send + Sync>;

/// Messages sent while reconnecting, delivered once the socket is back.
/// Only session activity and late ICE candidates flow after the peers
/// connect, so the oldest are dropped past this bound.
const PENDING_LIMIT: usize = 64;

enum Link {
    Open(SignalingConnection),
    Reconnecting(JoinHandle<anyhow::Result<Socket>>),
}

pub struct SessionSignaling {
    connect: Connect,
    link: Link,
    backoff: ReconnectBackoff,
    peer_connected: bool,
    pending: VecDeque<Message>,
}

impl SessionSignaling {
    /// Opens the session's signaling socket, authenticated with `token`.
    pub async fn connect(url: Url, token: String) -> anyhow::Result<Self> {
        Self::open(Arc::new(move || {
            let url = url.clone();
            let token = token.clone();
            Box::pin(async move {
                crate::authenticated_websocket(url, &token)
                    .await
                    .map(|(socket, _)| socket)
            })
        }))
        .await
    }

    async fn open(connect: Connect) -> anyhow::Result<Self> {
        let socket = connect().await?;
        Ok(Self {
            connect,
            link: Link::Open(SignalingConnection::new(socket)),
            backoff: ReconnectBackoff::new(Duration::from_secs(1), Duration::from_secs(15)),
            peer_connected: false,
            pending: VecDeque::new(),
        })
    }

    /// The peers are connected: from now on a lost socket reconnects
    /// instead of ending the session.
    pub fn peer_connected(&mut self) {
        self.peer_connected = true;
    }

    /// The next message from the server. Pings are answered internally.
    /// Cancel-safe, so it can be polled in `tokio::select!`.
    pub async fn next(&mut self) -> Option<anyhow::Result<Message>> {
        loop {
            match &mut self.link {
                Link::Open(connection) => {
                    let error = match connection.next().await {
                        Some(Ok(message)) => return Some(Ok(message)),
                        Some(Err(error)) => error,
                        None => anyhow::anyhow!("signaling connection closed"),
                    };
                    if !self.peer_connected || !recoverable(&error) {
                        return Some(Err(error));
                    }
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "session signaling lost; the peer connection continues while it reconnects"
                    );
                    self.backoff.reset();
                    self.reconnect_after(Duration::ZERO);
                }
                Link::Reconnecting(attempt) => {
                    let result = attempt.await.unwrap_or_else(|error| {
                        Err(anyhow::anyhow!("signaling reconnect task failed: {error}"))
                    });
                    match result {
                        Ok(socket) => {
                            let connection = SignalingConnection::new(socket);
                            for message in self.pending.drain(..) {
                                if let Err(error) = connection.queue(message) {
                                    tracing::warn!(%error, "dropped a signaling message queued while reconnecting");
                                }
                            }
                            self.link = Link::Open(connection);
                            tracing::info!("session signaling reconnected");
                        }
                        Err(error) if crate::is_terminal_websocket_error(&error) => {
                            return Some(Err(error));
                        }
                        Err(error) => {
                            let delay = self.backoff.next_delay();
                            tracing::warn!(
                                error = %format!("{error:#}"),
                                retry_seconds = delay.as_secs(),
                                "session signaling reconnect failed"
                            );
                            self.reconnect_after(delay);
                        }
                    }
                }
            }
        }
    }

    /// Sends `message`. After the peers connect, a message sent while the
    /// socket is down is delivered once it reconnects.
    pub async fn send(&mut self, message: Message) -> anyhow::Result<()> {
        let result = match &self.link {
            Link::Open(connection) => connection.send(message.clone()).await,
            Link::Reconnecting(_) => Err(anyhow::anyhow!("signaling is reconnecting")),
        };
        match result {
            Err(_) if self.peer_connected => {
                // A failed write also ends the socket's pump, which `next`
                // reports and recovers from.
                if self.pending.len() == PENDING_LIMIT {
                    self.pending.pop_front();
                }
                self.pending.push_back(message);
                Ok(())
            }
            result => result,
        }
    }

    fn reconnect_after(&mut self, delay: Duration) {
        let connect = Arc::clone(&self.connect);
        self.link = Link::Reconnecting(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            connect().await
        }));
    }
}

impl Drop for SessionSignaling {
    fn drop(&mut self) {
        if let Link::Reconnecting(attempt) = &self.link {
            attempt.abort();
        }
    }
}

/// Whether reconnecting can restore signaling. A revoked session cannot,
/// and a superseded socket means this peer already connected a newer one.
fn recoverable(error: &anyhow::Error) -> bool {
    !crate::is_terminal_websocket_error(error) && error.downcast_ref::<SupersededClose>().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

    type ServerSocket = WebSocketStream<tokio::net::TcpStream>;

    /// A plaintext server handing each accepted socket to the test, and a
    /// connector that waits while the returned gate is closed (`false`).
    async fn server() -> (
        Connect,
        mpsc::UnboundedReceiver<ServerSocket>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                    let _ = accepted_tx.send(socket);
                }
            }
        });
        let (gate, open) = tokio::sync::watch::channel(true);
        let connect: Connect = Arc::new(move || {
            let mut open = open.clone();
            Box::pin(async move {
                open.wait_for(|open| *open).await?;
                let (socket, _) =
                    tokio_tungstenite::connect_async(format!("ws://{address}")).await?;
                Ok(socket)
            })
        });
        (connect, accepted, gate)
    }

    async fn next_text(signaling: &mut SessionSignaling) -> String {
        match tokio::time::timeout(Duration::from_secs(5), signaling.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(text) => text.to_string(),
            other => panic!("unexpected message {other:?}"),
        }
    }

    async fn server_text(socket: &mut ServerSocket) -> String {
        match tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(text) => text.to_string(),
            other => panic!("unexpected message {other:?}"),
        }
    }

    #[tokio::test]
    async fn losing_the_socket_before_the_peers_connect_ends_the_attempt() {
        let (connect, mut accepted, _gate) = server().await;
        let mut signaling = SessionSignaling::open(connect).await.unwrap();
        drop(accepted.recv().await.unwrap());
        let result = tokio::time::timeout(Duration::from_secs(5), signaling.next())
            .await
            .unwrap();
        assert!(result.is_none_or(|result| result.is_err()));
    }

    #[tokio::test]
    async fn a_dropped_socket_reconnects_once_the_peers_are_connected() {
        let (connect, mut accepted, _gate) = server().await;
        let mut signaling = SessionSignaling::open(connect).await.unwrap();
        signaling.peer_connected();
        let first = accepted.recv().await.unwrap();
        // An abrupt reset, as a proxy may do, without a close handshake.
        drop(first);

        let receive = tokio::spawn(async move {
            let text = next_text(&mut signaling).await;
            (signaling, text)
        });
        let mut second = tokio::time::timeout(Duration::from_secs(5), accepted.recv())
            .await
            .unwrap()
            .unwrap();
        second.send(Message::Text("after".into())).await.unwrap();
        let (mut signaling, text) = receive.await.unwrap();
        assert_eq!(text, "after");

        signaling
            .send(Message::Text("from peer".into()))
            .await
            .unwrap();
        assert_eq!(server_text(&mut second).await, "from peer");
    }

    #[tokio::test]
    async fn messages_sent_while_reconnecting_are_delivered() {
        let (connect, mut accepted, gate) = server().await;
        let mut signaling = SessionSignaling::open(connect).await.unwrap();
        signaling.peer_connected();
        gate.send_replace(false);
        drop(accepted.recv().await.unwrap());
        // The loss surfaces and a reconnect starts, held at the gate.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), signaling.next())
                .await
                .is_err()
        );
        assert!(matches!(signaling.link, Link::Reconnecting(_)));
        signaling
            .send(Message::Text("queued".into()))
            .await
            .unwrap();

        gate.send_replace(true);
        let receive = tokio::spawn(async move {
            let text = next_text(&mut signaling).await;
            (signaling, text)
        });
        let mut second = accepted.recv().await.unwrap();
        assert_eq!(server_text(&mut second).await, "queued");
        second.send(Message::Text("done".into())).await.unwrap();
        assert_eq!(receive.await.unwrap().1, "done");
    }

    #[tokio::test]
    async fn revoked_and_superseded_sockets_do_not_reconnect() {
        for (code, superseded) in [(4001, false), (1008, false), (4000, true)] {
            let (connect, mut accepted, _gate) = server().await;
            let mut signaling = SessionSignaling::open(connect).await.unwrap();
            signaling.peer_connected();
            let mut socket = accepted.recv().await.unwrap();
            socket
                .close(Some(CloseFrame {
                    code: CloseCode::from(code),
                    reason: "closed".into(),
                }))
                .await
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), signaling.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<SupersededClose>().is_some(),
                superseded
            );
            assert_eq!(crate::is_terminal_websocket_error(&error), !superseded);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), accepted.recv())
                    .await
                    .is_err(),
                "close code {code} must not reconnect"
            );
        }
    }
}
