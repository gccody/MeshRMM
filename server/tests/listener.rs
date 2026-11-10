//! The listener's limits on plain HTTP: clients that never finish starting a
//! request are disconnected, and requests that run long are not.
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::ws::WebSocketUpgrade,
    response::Response,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use meshrmm_server::serve::{self, Endpoint, Listener, Timeouts};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Short enough to wait out in a test.
const TIMEOUT: Duration = Duration::from_millis(300);

/// Longer than [`TIMEOUT`], so a request that takes it would be cut off by
/// any timeout on whole requests.
const LONG: Duration = Duration::from_millis(1200);

fn router() -> Router {
    Router::new()
        .route("/", get(|| async { "hello" }))
        .route(
            "/socket",
            get(|upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        )
        .route(
            "/slow",
            get(|| async {
                let chunks = futures_util::stream::iter(0..4).then(|chunk| async move {
                    tokio::time::sleep(LONG / 4).await;
                    Ok::<_, std::io::Error>(Bytes::from(format!("chunk {chunk};")))
                });
                Response::new(Body::from_stream(chunks))
            }),
        )
        .route(
            "/count",
            post(|body: Bytes| async move { body.len().to_string() }),
        )
}

struct Server {
    address: SocketAddr,
    handle: axum_server::Handle<Endpoint>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start() -> Self {
        let listener = Listener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let handle = axum_server::Handle::new();
        let task = tokio::spawn(serve::serve_http(
            listener,
            router(),
            handle.clone(),
            Timeouts {
                header_read: TIMEOUT,
                ..Timeouts::default()
            },
        ));
        Self {
            address,
            handle,
            task,
        }
    }

    async fn stop(self) {
        self.handle.graceful_shutdown(Some(Duration::from_secs(1)));
        self.task.await.unwrap().unwrap();
    }
}

/// Waits for the server to close `stream`, and returns how long it took and
/// what it sent.
async fn closed(mut stream: TcpStream, since: Instant) -> (Duration, Vec<u8>) {
    let mut received = Vec::new();
    tokio::time::timeout(TIMEOUT * 10, stream.read_to_end(&mut received))
        .await
        .expect("the server closed the connection")
        .ok();
    (since.elapsed(), received)
}

#[tokio::test]
async fn a_connection_that_sends_nothing_is_closed() {
    let server = Server::start().await;
    let started = Instant::now();
    let stream = TcpStream::connect(server.address).await.unwrap();
    let (waited, received) = closed(stream, started).await;
    assert!(waited >= TIMEOUT, "closed after {waited:?}");
    assert!(received.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn a_half_sent_request_line_is_closed() {
    let server = Server::start().await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream.write_all(b"GET /hea").await.unwrap();
    let started = Instant::now();
    let (waited, _) = closed(stream, started).await;
    assert!(waited >= TIMEOUT, "closed after {waited:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_half_sent_request_head_is_closed() {
    let server = Server::start().await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let (waited, _) = closed(stream, started).await;
    assert!(waited >= TIMEOUT, "closed after {waited:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_half_sent_http2_preface_is_closed() {
    let server = Server::start().await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream.write_all(b"PRI * HTTP/2.0\r\n").await.unwrap();
    let (_, received) = closed(stream, Instant::now()).await;
    assert!(received.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed_after_its_requests() {
    let server = Server::start().await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    for _ in 0..2 {
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"hello") {
            let mut buffer = [0; 512];
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "the connection closed between requests");
            response.extend_from_slice(&buffer[..read]);
        }
        assert!(response.starts_with(b"HTTP/1.1 200"));
    }
    let started = Instant::now();
    let (waited, _) = closed(stream, started).await;
    assert!(waited >= TIMEOUT, "closed after {waited:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_websocket_outlives_the_timeouts() {
    let server = Server::start().await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/socket", server.address))
            .await
            .unwrap();
    tokio::time::sleep(LONG).await;
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("still here"))
        .await
        .unwrap();
    let echoed = socket.next().await.unwrap().unwrap();
    assert_eq!(echoed.into_text().unwrap(), "still here");
    socket.close(None).await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn a_slow_response_body_streams_to_the_end() {
    let server = Server::start().await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let (waited, received) = closed(stream, started).await;
    let received = String::from_utf8(received).unwrap();
    assert!(received.starts_with("HTTP/1.1 200"), "{received}");
    for chunk in 0..4 {
        assert!(received.contains(&format!("chunk {chunk};")), "{received}");
    }
    assert!(waited >= LONG);
    server.stop().await;
}

#[tokio::test]
async fn a_slow_request_body_is_read_to_the_end() {
    let server = Server::start().await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream
        .write_all(
            b"POST /count HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 4\r\n\r\n",
        )
        .await
        .unwrap();
    for byte in b"body" {
        tokio::time::sleep(LONG / 4).await;
        stream.write_all(&[*byte]).await.unwrap();
    }
    let (_, received) = closed(stream, Instant::now()).await;
    let received = String::from_utf8(received).unwrap();
    assert!(received.starts_with("HTTP/1.1 200"), "{received}");
    assert!(received.ends_with("\r\n\r\n4"), "{received}");
    server.stop().await;
}
