//! Serving HTTPS from certificate files, picking up replaced files, resuming
//! TLS sessions, and the listener's limits in both TLS modes.
mod common;

use std::{
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use axum_server::{Handle, tls_rustls::RustlsConfig};
use meshrmm_server::{
    config::Config,
    serve::{CertificateFiles, Endpoint, Timeouts},
};
use rustls::{
    ClientConfig, HandshakeKind, RootCertStore,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::TlsConnector;

/// Short enough to wait out in a test.
const TIMEOUT: Duration = Duration::from_millis(300);

fn write_certificate(dir: &Path) -> String {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let pem = certified.cert.pem();
    std::fs::write(dir.join("cert.pem"), &pem).unwrap();
    std::fs::write(dir.join("key.pem"), certified.signing_key.serialize_pem()).unwrap();
    pem
}

fn files_config(dir: &Path) -> Config {
    common::config(
        dir,
        &format!(
            r#"
            http.listen = "127.0.0.1:0"
            tls = {{ mode = "files", cert_path = "{cert}", key_path = "{key}" }}
            "#,
            cert = dir.join("cert.pem").display(),
            key = dir.join("key.pem").display(),
        ),
    )
}

/// A server on `config` with `timeouts`, and the address it listens on.
struct Server {
    address: SocketAddr,
    handle: Handle<Endpoint>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Server {
    async fn start(config: Config, timeouts: Timeouts) -> Self {
        let state = meshrmm_server::prepare(config).await.unwrap();
        let handle = Handle::<Endpoint>::new();
        let task = tokio::spawn({
            let handle = handle.clone();
            async move {
                meshrmm_server::serve::serve_with(
                    &state.config,
                    meshrmm_server::http::router(state.clone()),
                    handle,
                    timeouts,
                )
                .await
            }
        });
        let Endpoint(address) = handle
            .listening()
            .await
            .expect("the server started listening");
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

/// A TLS 1.3 client that trusts `certificate`. Each connection made with
/// one connector shares its session cache.
fn connector(certificate: &str) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(certificate.as_bytes()).unwrap())
        .unwrap();
    let mut config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsConnector::from(Arc::new(config))
}

/// Fetches `/healthz` over a new connection, and returns how its handshake
/// went.
async fn health(connector: &TlsConnector, address: SocketAddr) -> HandshakeKind {
    let stream = TcpStream::connect(address).await.unwrap();
    let mut tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
        .unwrap();
    let kind = tls.get_ref().1.handshake_kind().unwrap();
    tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    // Reading also takes in the session tickets the server sends after the
    // handshake.
    let mut response = Vec::new();
    let _ = tls.read_to_end(&mut response).await;
    assert!(response.starts_with(b"HTTP/1.1 200"));
    kind
}

/// Waits for the server to close `stream`, and returns how long it took.
async fn closed(mut stream: impl AsyncReadExt + Unpin, since: Instant) -> Duration {
    let mut received = Vec::new();
    tokio::time::timeout(TIMEOUT * 10, stream.read_to_end(&mut received))
        .await
        .expect("the server closed the connection")
        .ok();
    since.elapsed()
}

#[tokio::test]
async fn files_mode_serves_https_with_the_configured_certificate() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let certificate = write_certificate(dir.path());
    let server = Server::start(files_config(dir.path()), Timeouts::default()).await;

    let client = reqwest::Client::builder()
        .tls_certs_only([reqwest::Certificate::from_pem(certificate.as_bytes()).unwrap()])
        .resolve("localhost", server.address)
        .build()
        .unwrap();
    let response = client
        .get(format!(
            "https://localhost:{}/healthz",
            server.address.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()["strict-transport-security"],
        "max-age=31536000"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_reconnecting_client_resumes_its_tls_session() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let certificate = write_certificate(dir.path());
    let server = Server::start(files_config(dir.path()), Timeouts::default()).await;
    let connector = connector(&certificate);

    assert_eq!(
        health(&connector, server.address).await,
        HandshakeKind::Full
    );
    assert_eq!(
        health(&connector, server.address).await,
        HandshakeKind::Resumed
    );
    assert_eq!(
        health(&connector, server.address).await,
        HandshakeKind::Resumed
    );

    server.stop().await;
}

#[tokio::test]
async fn files_mode_closes_a_connection_that_never_starts_tls() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    write_certificate(dir.path());
    let server = Server::start(
        files_config(dir.path()),
        Timeouts {
            tls_handshake: TIMEOUT,
            ..Timeouts::default()
        },
    )
    .await;

    let started = Instant::now();
    let stream = TcpStream::connect(server.address).await.unwrap();
    let waited = closed(stream, started).await;
    assert!(waited >= TIMEOUT, "closed after {waited:?}");

    server.stop().await;
}

#[tokio::test]
async fn files_mode_closes_a_tls_connection_that_never_starts_http() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let certificate = write_certificate(dir.path());
    let server = Server::start(
        files_config(dir.path()),
        Timeouts {
            header_read: TIMEOUT,
            ..Timeouts::default()
        },
    )
    .await;

    let stream = TcpStream::connect(server.address).await.unwrap();
    let tls = connector(&certificate)
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
        .unwrap();
    let started = Instant::now();
    let waited = closed(tls, started).await;
    assert!(waited >= TIMEOUT / 2, "closed after {waited:?}");

    server.stop().await;
}

#[tokio::test]
async fn acme_mode_closes_a_connection_that_never_starts_tls() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    // Nothing answers there, so no certificate is ever issued; the listener
    // runs all the same.
    let config = common::config(
        dir.path(),
        r#"
        http.listen = "127.0.0.1:0"
        tls = { mode = "acme", directory_url = "https://127.0.0.1:9/directory" }
        "#,
    );
    let server = Server::start(
        config,
        Timeouts {
            tls_handshake: TIMEOUT,
            ..Timeouts::default()
        },
    )
    .await;

    let started = Instant::now();
    let stream = TcpStream::connect(server.address).await.unwrap();
    let waited = closed(stream, started).await;
    assert!(waited >= TIMEOUT, "closed after {waited:?}");

    server.stop().await;
}

#[tokio::test]
async fn replaced_certificate_files_are_reloaded_with_the_same_ticket_keys() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    write_certificate(dir.path());
    let files =
        CertificateFiles::new(dir.path().join("cert.pem"), dir.path().join("key.pem")).unwrap();
    let original = files.load().await.unwrap();
    let tls = RustlsConfig::from_config(original.clone());
    let reloader = tokio::spawn(meshrmm_server::serve::reload_on_change(
        tls.clone(),
        files,
        Duration::from_millis(20),
    ));

    // Filesystems with coarse timestamps need the rewrite to land in a later tick.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    write_certificate(dir.path());
    let reloaded = tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::ptr_eq(&tls.get_inner(), &original) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    reloader.abort();
    assert!(reloaded.is_ok(), "the new certificate was not loaded");
    // Tickets issued before the reload still resume.
    assert!(Arc::ptr_eq(&tls.get_inner().ticketer, &original.ticketer));
}
