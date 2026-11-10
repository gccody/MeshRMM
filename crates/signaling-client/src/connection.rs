//! A signaling WebSocket's network task. It reads and writes at once, so a
//! large message that takes minutes to write on a slow uplink neither
//! starves pings and liveness detection nor holds up what the server sends.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::progress::Progress;

type Outgoing = (Message, oneshot::Sender<anyhow::Result<()>>);

/// How long the connection may go without writing, or without reading,
/// before it pings the server (see [`next_ping`]).
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// How long the server may go unheard before the connection is dead.
const LIVENESS: Duration = Duration::from_secs(60);
/// How long a write may go without the socket taking a byte before the
/// connection is dead. A write has no limit in total: a 6 MiB report on a few
/// hundred kbit/s takes minutes, and a write cut short is tried again on the
/// next connection. This is longer than the few seconds a slow link takes to
/// move some bytes because the operating system wakes a writer whose send
/// buffer is full only once a third of that buffer has gone out, which on a
/// slow link with a buffer of a few megabytes takes many seconds, and because
/// a lost packet holds everything up while TCP retransmits it, waiting longer
/// each time. It is half of [`LIVENESS`], which finds a dead link that
/// nothing is being written to.
const STALL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
struct Limits {
    ping_interval: Duration,
    liveness: Duration,
    stall: Duration,
}

/// Owns network liveness independently of application command processing.
/// A full command queue disconnects instead of blocking pong handling or
/// allowing unbounded memory growth. Dropping the owner cancels the pump.
pub struct SignalingConnection {
    incoming: mpsc::Receiver<anyhow::Result<Message>>,
    outgoing: mpsc::Sender<Outgoing>,
    task: tokio::task::JoinHandle<()>,
}

impl SignalingConnection {
    pub fn new(socket: crate::Socket) -> Self {
        Self::start(
            socket,
            Limits {
                ping_interval: PING_INTERVAL,
                liveness: LIVENESS,
                stall: STALL,
            },
        )
    }

    fn start(socket: crate::Socket, limits: Limits) -> Self {
        let (incoming_tx, incoming) = mpsc::channel(256);
        let (outgoing, mut outgoing_rx) = mpsc::channel::<Outgoing>(64);
        let progress = socket.get_ref().get_ref().progress();
        let task = tokio::spawn(async move {
            let (mut sink, mut stream) = socket.split();
            let ping = Notify::new();
            let read = async {
                let mut last_read = Instant::now();
                let mut pinged = last_read;
                // While a large message goes out, what the server sends, its
                // pongs too, may wait behind it, but the server taking the
                // message in shows it is there.
                let heard = |last_read: Instant| {
                    progress
                        .drained_at()
                        .map_or(last_read, |drained| drained.max(last_read))
                };
                loop {
                    let ping_at = next_ping(
                        last_read,
                        progress.written_at(),
                        pinged,
                        limits.ping_interval,
                    );
                    let dead_at = heard(last_read) + limits.liveness;
                    tokio::select! {
                        message = stream.next() => {
                            let message = message.context("signaling connection closed")??;
                            last_read = Instant::now();
                            match message {
                                // tungstenite answers pings, writing the pong
                                // with the next read or write.
                                Message::Ping(_) | Message::Pong(_) => {}
                                Message::Close(frame) => return Err(crate::signaling_close_error(frame)),
                                message => incoming_tx.try_send(Ok(message))
                                    .map_err(|_| anyhow::anyhow!("signaling command queue full or closed"))?,
                            }
                        }
                        () = tokio::time::sleep_until(ping_at.min(dead_at)) => {
                            anyhow::ensure!(
                                heard(last_read).elapsed() < limits.liveness,
                                "signaling liveness timeout"
                            );
                            let now = Instant::now();
                            if next_ping(last_read, progress.written_at(), pinged, limits.ping_interval) <= now {
                                pinged = now;
                                // Written once the write going on, if any, ends.
                                ping.notify_one();
                            }
                        }
                    }
                }
            };
            let write = async {
                loop {
                    let (message, done) = tokio::select! {
                        biased;
                        () = ping.notified() => (Message::Ping(Default::default()), None),
                        outgoing = outgoing_rx.recv() => match outgoing {
                            Some((message, done)) => (message, Some(done)),
                            // The owner is gone, and aborts this task.
                            None => return std::future::pending().await,
                        },
                    };
                    if let Err(error) = write(&mut sink, message, &progress, limits.stall).await {
                        if let Some(done) = done {
                            let _ = done.send(Err(anyhow::anyhow!("{error:#}")));
                        }
                        return Err(error);
                    }
                    if let Some(done) = done {
                        let _ = done.send(Ok(()));
                    }
                }
            };
            let result: anyhow::Result<()> = tokio::select! {
                result = read => result,
                result = write => result,
            };
            drop((sink, stream));
            // Those waiting for a write learn it won't happen, and why.
            outgoing_rx.close();
            let reason = match &result {
                Ok(()) => "signaling connection ended".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            while let Ok((_, done)) = outgoing_rx.try_recv() {
                let _ = done.send(Err(anyhow::anyhow!("{reason}")));
            }
            drop(outgoing_rx);
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

    /// Acknowledges the actual socket flush, not merely enqueueing the
    /// message. The wait covers the messages queued ahead, however long they
    /// take to write: it ends once the socket has written `message`, or the
    /// connection has failed, as it does when a write stalls or the server
    /// goes silent.
    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        self.sending(message).await
    }

    /// [`send`](Self::send) as a future that does not borrow the connection,
    /// so a caller can wait for a long write while it goes on reading.
    pub fn sending(
        &self,
        message: Message,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'static + use<> {
        let outgoing = self.outgoing.clone();
        async move {
            let (done, written) = oneshot::channel();
            outgoing
                .send((message, done))
                .await
                .map_err(|_| anyhow::anyhow!("signaling writer closed"))?;
            written.await.context("signaling writer stopped")?
        }
    }

    /// Queues `message` without waiting for the flush. Fails only if the
    /// queue is full or the pump has stopped.
    pub fn queue(&self, message: Message) -> anyhow::Result<()> {
        let (done, _) = oneshot::channel();
        self.outgoing
            .try_send((message, done))
            .map_err(|_| anyhow::anyhow!("signaling writer full or closed"))
    }
}

/// When to ping: once the connection has gone `interval` without writing,
/// since the server gives up on a client it hasn't heard a frame from for
/// its liveness limit, or without reading, since the pong is then all the
/// client would hear. A connection busy both ways needs no ping. A ping
/// waiting behind a long write is not asked for again within `interval`.
fn next_ping(
    last_read: Instant,
    last_written: Instant,
    pinged: Instant,
    interval: Duration,
) -> Instant {
    last_read.min(last_written).max(pinged) + interval
}

/// Writes `message`, failing once the socket has taken nothing for `stall`.
async fn write(
    sink: &mut SplitSink<crate::Socket, Message>,
    message: Message,
    progress: &Arc<Progress>,
    stall: Duration,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let send = sink.send(message);
    tokio::pin!(send);
    loop {
        let deadline = progress.written_at().max(started) + stall;
        tokio::select! {
            result = &mut send => return result.context("signaling socket write failed"),
            () = tokio::time::sleep_until(deadline) => anyhow::ensure!(
                progress.written_at().max(started) + stall > Instant::now(),
                "signaling socket write stalled: nothing written for {} ms",
                stall.as_millis()
            ),
        }
    }
}

impl Drop for SignalingConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::{TcpSocket, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    use super::*;

    /// The kernel buffers on each side of the large-message tests, so a few
    /// megabytes can't all sit in them and the writer has to wait for the
    /// reader.
    const SMALL_BUFFER: u32 = 64 * 1024;
    const LARGE: usize = 2 * 1024 * 1024;
    /// Reads of a slow but steady peer: 32 KiB every 20 ms or so (Windows
    /// rounds sleeps up to its 16 ms timer), so 2 MiB takes one to two
    /// seconds.
    const SLOW_READS: Option<(usize, Duration)> = Some((32 * 1024, Duration::from_millis(20)));

    fn large() -> Message {
        Message::Text("x".repeat(LARGE).into())
    }

    /// No pings or liveness timeout to get in the way of a test of writes.
    fn stalling_after(stall: Duration) -> Limits {
        Limits {
            ping_interval: Duration::from_secs(60),
            liveness: Duration::from_secs(120),
            stall,
        }
    }

    /// The server's end of the TCP connection, reading at most `chunk` bytes
    /// every `pause` when throttled.
    struct Server {
        stream: TcpStream,
        throttle: Option<(usize, Duration)>,
        pause: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl AsyncRead for Server {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let Some((chunk, pause)) = this.throttle else {
                return Pin::new(&mut this.stream).poll_read(cx, buf);
            };
            if let Some(sleep) = &mut this.pause {
                ready!(sleep.as_mut().poll(cx));
                this.pause = None;
            }
            let mut read = vec![0; chunk.min(buf.remaining())];
            let mut limited = ReadBuf::new(&mut read);
            ready!(Pin::new(&mut this.stream).poll_read(cx, &mut limited))?;
            buf.put_slice(limited.filled());
            if !limited.filled().is_empty() {
                this.pause = Some(Box::pin(tokio::time::sleep(pause)));
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for Server {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.stream).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.stream).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.stream).poll_shutdown(cx)
        }
    }

    type ServerSocket = WebSocketStream<tokio_rustls::server::TlsStream<Server>>;

    async fn pair() -> (crate::Socket, ServerSocket) {
        pair_with(None, None).await
    }

    /// A connected pair over TLS, as in production, whose server reads as
    /// `throttle` says, with kernel buffers of `buffer` bytes where given.
    /// TLS matters to the large-message tests: rustls writes a message to
    /// the socket a few records at a time, while without it the whole
    /// message goes in one write, which Windows takes whole into its buffer
    /// whatever the buffer's size.
    async fn pair_with(
        throttle: Option<(usize, Duration)>,
        buffer: Option<u32>,
    ) -> (crate::Socket, ServerSocket) {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = generated.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder_with_provider(crate::tls::tls13_crypto_provider())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], key.into())
                .unwrap(),
        ));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let connector =
            tokio_tungstenite::Connector::Rustls(crate::tls::tls13_client_config(roots).unwrap());

        let listening = TcpSocket::new_v4().unwrap();
        if let Some(buffer) = buffer {
            // Accepted sockets take the listener's buffer size.
            listening.set_recv_buffer_size(buffer).unwrap();
        }
        listening.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = listening.listen(16).unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor
                .accept(Server {
                    stream,
                    throttle,
                    pause: None,
                })
                .await
                .unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let connecting = TcpSocket::new_v4().unwrap();
        if let Some(buffer) = buffer {
            connecting.set_send_buffer_size(buffer).unwrap();
        }
        let stream = connecting.connect(address).await.unwrap();
        let (client, _) = tokio_tungstenite::client_async_tls_with_config(
            format!("wss://localhost:{}", address.port()),
            crate::Tracked::new(stream),
            None,
            Some(connector),
        )
        .await
        .unwrap();
        (client, server.await.unwrap())
    }

    #[tokio::test]
    async fn answers_pings_while_command_consumer_is_stalled() {
        let (client, mut server) = pair().await;
        let mut connection = SignalingConnection::start(
            client,
            Limits {
                ping_interval: Duration::from_secs(20),
                liveness: Duration::from_secs(60),
                stall: STALL,
            },
        );
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

    #[test]
    fn pings_only_once_a_direction_has_been_quiet_for_the_interval() {
        let interval = Duration::from_secs(20);
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        // Traffic both ways 5 seconds ago: no ping for another 15.
        assert_eq!(next_ping(at(95), at(95), at(0), interval), at(115));
        // Writing but hearing nothing since 70: the pong is due.
        assert_eq!(next_ping(at(70), at(99), at(0), interval), at(90));
        // Hearing but writing nothing since 70: the server needs a frame.
        assert_eq!(next_ping(at(99), at(70), at(0), interval), at(90));
        // A ping still waiting behind a long write isn't asked for again.
        assert_eq!(next_ping(at(0), at(99), at(95), interval), at(115));
    }

    #[tokio::test]
    async fn detects_dead_peer_without_application_polling() {
        let (client, _server) = pair().await;
        let mut connection = SignalingConnection::start(
            client,
            Limits {
                ping_interval: Duration::from_millis(10),
                liveness: Duration::from_millis(40),
                stall: STALL,
            },
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

    #[tokio::test]
    async fn a_slow_but_steady_peer_gets_a_message_that_takes_longer_than_the_stall_limit() {
        let stall = Duration::from_millis(400);
        let (client, mut server) = pair_with(SLOW_READS, Some(SMALL_BUFFER)).await;
        let connection = SignalingConnection::start(client, stalling_after(stall));
        let started = Instant::now();
        connection.queue(large()).unwrap();
        // Its wait covers the large message ahead of it.
        let after = tokio::spawn(connection.sending(Message::Text("after".into())));
        let received = tokio::time::timeout(Duration::from_secs(60), server.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let took = started.elapsed();
        assert_eq!(received.len(), LARGE);
        assert!(
            took > stall,
            "the write took {took:?}, too short to show anything"
        );
        tokio::time::timeout(Duration::from_secs(10), after)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            server.next().await.unwrap().unwrap(),
            Message::Text("after".into())
        );
    }

    #[tokio::test]
    async fn a_long_write_keeps_the_connection_alive_though_nothing_is_read() {
        let limits = Limits {
            ping_interval: Duration::from_millis(50),
            liveness: Duration::from_millis(400),
            stall: Duration::from_secs(5),
        };
        let (client, server) = pair_with(SLOW_READS, Some(SMALL_BUFFER)).await;
        let mut connection = SignalingConnection::start(client, limits);
        let (mut server_tx, mut server_rx) = server.split();
        // Reading answers the connection's pings once they get through.
        let (received_tx, mut received) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(Ok(message)) = server_rx.next().await {
                if message.is_text() {
                    let _ = received_tx.send(message.len());
                }
            }
        });
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(60), connection.send(large()))
            .await
            .unwrap()
            .unwrap();
        let took = started.elapsed();
        assert!(
            took > limits.liveness,
            "the write took {took:?}, too short to show anything"
        );
        assert_eq!(received.recv().await, Some(LARGE));
        server_tx
            .send(Message::Text("still here".into()))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), connection.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Message::Text("still here".into())
        );
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_fails_the_write_after_the_stall_limit() {
        let stall = Duration::from_millis(300);
        let (client, _server) = pair_with(None, Some(SMALL_BUFFER)).await;
        let mut connection = SignalingConnection::start(client, stalling_after(stall));
        let started = Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(10), connection.send(large()))
            .await
            .unwrap()
            .unwrap_err();
        assert!(started.elapsed() >= stall);
        assert!(format!("{error:#}").contains("stalled"), "{error:#}");
        let error = tokio::time::timeout(Duration::from_secs(5), connection.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("stalled"), "{error:#}");
    }

    #[tokio::test]
    async fn a_small_message_on_a_dead_link_fails_after_the_stall_limit() {
        let stall = Duration::from_millis(300);
        let (client, _server) = pair_with(None, Some(SMALL_BUFFER)).await;
        let connection = SignalingConnection::start(client, stalling_after(stall));
        // Fills the buffers, as earlier traffic to a peer that went away does.
        connection.queue(large()).unwrap();
        // Within the stall limit, with a margin for a busy machine.
        let error = tokio::time::timeout(
            stall + Duration::from_secs(5),
            connection.send(Message::Text("small".into())),
        )
        .await
        .expect("the wait ends with the stalled write")
        .unwrap_err();
        assert!(format!("{error:#}").contains("stalled"), "{error:#}");
    }
}
