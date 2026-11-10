//! Accepting connections in each TLS mode: an ACME certificate, certificate
//! files, or plain HTTP behind a reverse proxy.
//!
//! Every mode shares the listener and its limits: socket options on each
//! accepted connection, a deadline for the TLS handshake and for HTTP to
//! start, hyper's header-read timeout and HTTP/2 keep-alive pings. None of
//! them bounds a request once its head has arrived, so WebSockets, event
//! streams and long downloads and uploads run as long as they need.
use std::{
    future::Future,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context as _;
use axum::{Router, extract::connect_info::Connected};
use axum_server::{
    AddrListener, Address, Handle, Server,
    accept::{Accept, DefaultAcceptor},
    tls_rustls::{RustlsAcceptor, RustlsConfig},
};
use futures_util::{StreamExt, future::BoxFuture};
use hyper_util::rt::TokioTimer;
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::ProducesTickets,
};
use rustls_acme::{AcmeConfig, caches::DirCache};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    time::Sleep,
};

use crate::config::{Config, TlsConfig};

/// How often certificate files are checked for replacement.
const CERTIFICATE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long a client has to start HTTP on a new connection, and then to send
/// a request's head; hyper also closes an HTTP/1 connection that has been
/// idle this long between requests. A slow mobile link sends a head in well
/// under a second, and an idle socket costs a descriptor and a task.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a TLS handshake may take; axum-server's rustls acceptor uses the
/// same.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often an HTTP/2 connection is pinged, and how long the answer may
/// take, so a browser that vanished is noticed while its connection is idle.
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// TCP keep-alive: probes start after this much silence and are this far
/// apart, and this many unanswered ones close the connection, so a peer that
/// vanished without closing (its NAT mapping gone, say) is found in about 90
/// seconds even on a connection that is idle at the HTTP level. The probes
/// also keep NAT mappings in between alive.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const TCP_KEEPALIVE_PROBES: u32 = 3;

/// At most one accept failure is logged per this interval: when descriptors
/// run out, every attempt fails, 20 times a second.
const ACCEPT_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Below this many open files the server warns at startup. Each Agent holds
/// a socket, and so do browsers, terminals, file transfers, TURN relays and
/// database connections.
#[cfg(unix)]
const LOW_OPEN_FILE_LIMIT: u64 = 65_536;

/// How long the listener waits on clients. Tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// For HTTP to start on a new connection, for each request's head, and
    /// between requests on an idle HTTP/1 connection.
    pub header_read: Duration,
    pub tls_handshake: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            header_read: HEADER_READ_TIMEOUT,
            tls_handshake: TLS_HANDSHAKE_TIMEOUT,
        }
    }
}

/// Serves `app` until `handle` shuts the server down.
pub async fn serve(config: &Config, app: Router, handle: Handle<Endpoint>) -> anyhow::Result<()> {
    serve_with(config, app, handle, Timeouts::default()).await
}

/// [`serve`] with other timeouts.
pub async fn serve_with(
    config: &Config,
    app: Router,
    handle: Handle<Endpoint>,
    timeouts: Timeouts,
) -> anyhow::Result<()> {
    let address = config.listen_addr();
    let served = async {
        match &config.tls {
            TlsConfig::Proxy { .. } => {
                tracing::info!(%address, "serving HTTP for a reverse proxy");
                configure(Server::bind(Endpoint(address)), timeouts)
                    .acceptor(Limited::new(DefaultAcceptor, timeouts))
                    .handle(handle)
                    .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                    .await?;
            }
            TlsConfig::Files {
                cert_path,
                key_path,
            } => {
                let files = CertificateFiles::new(cert_path.clone(), key_path.clone())?;
                let tls = RustlsConfig::from_config(files.load().await?);
                let reloader = tokio::spawn(reload_on_change(
                    tls.clone(),
                    files,
                    CERTIFICATE_POLL_INTERVAL,
                ));
                tracing::info!(%address, cert = %cert_path.display(), "serving HTTPS with a certificate file");
                let acceptor = RustlsAcceptor::new(tls).handshake_timeout(timeouts.tls_handshake);
                let served = configure(Server::bind(Endpoint(address)), timeouts)
                    .acceptor(Limited::new(acceptor, timeouts))
                    .handle(handle)
                    .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                    .await;
                reloader.abort();
                served?;
            }
            TlsConfig::Acme {
                domains,
                contact_email,
                directory_url,
            } => {
                let cache = config.data_dir.join("acme");
                let mut acme = AcmeConfig::new(domains)
                    .contact(contact_email.iter().map(|email| format!("mailto:{email}")))
                    .directory(directory_url.as_str())
                    .cache(DirCache::new(cache))
                    .state();
                // The ACME acceptor answers TLS-ALPN-01 challenges itself and adds
                // their protocol; everything else is HTTP/2 or HTTP/1.1.
                let mut tls = (*acme.default_rustls_config()).clone();
                tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                tls.ticketer = ticketer()?;
                // Unlike axum-server's rustls acceptor, this one has no
                // handshake timeout; `Limited` gives it one.
                let acceptor = Limited::new(acme.axum_acceptor(Arc::new(tls)), timeouts);
                let events = tokio::spawn(async move {
                    while let Some(event) = acme.next().await {
                        match event {
                            Ok(event) => tracing::info!(?event, "ACME"),
                            Err(error) => {
                                tracing::warn!(%error, "ACME certificate request failed; retrying")
                            }
                        }
                    }
                });
                tracing::info!(%address, ?domains, directory = %directory_url, "serving HTTPS with an ACME certificate");
                let served = configure(Server::bind(Endpoint(address)), timeouts)
                    .acceptor(acceptor)
                    .handle(handle)
                    .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                    .await;
                events.abort();
                served?;
            }
        }
        anyhow::Ok(())
    };
    served
        .await
        .with_context(|| format!("could not serve on {address}"))
}

/// Serves plain HTTP on `listener`, as `proxy` mode does.
pub async fn serve_http(
    listener: Listener,
    app: Router,
    handle: Handle<Endpoint>,
    timeouts: Timeouts,
) -> io::Result<()> {
    configure(Server::from_listener(listener), timeouts)
        .acceptor(Limited::new(DefaultAcceptor, timeouts))
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
}

/// Gives hyper its timer and limits.
fn configure(mut server: Server<Endpoint>, timeouts: Timeouts) -> Server<Endpoint> {
    let builder = server.http_builder();
    // Without a timer hyper ignores its header-read timeout and can't ping.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(timeouts.header_read);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT);
    server
}

/// The address the server listens on, or a connection's peer. axum-server
/// accepts through the [`Listener`] it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint(pub SocketAddr);

impl Address for Endpoint {
    type Stream = TcpStream;
    type Listener = Listener;
}

impl Connected<Endpoint> for SocketAddr {
    fn connect_info(peer: Endpoint) -> Self {
        peer.0
    }
}

/// The listening socket. It sets `TCP_NODELAY` and TCP keep-alive on each
/// connection it accepts, and logs failures to accept, which axum-server
/// retries in silence.
#[derive(Debug)]
pub struct Listener {
    inner: TcpListener,
    failures: Mutex<AcceptFailures>,
}

#[derive(Debug, Default)]
struct AcceptFailures {
    last_logged: Option<Instant>,
    unlogged: u64,
}

impl Listener {
    pub async fn bind(address: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            inner: TcpListener::bind(address).await?,
            failures: Mutex::default(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn failed(&self, error: &io::Error) {
        let now = Instant::now();
        // Nothing panics while holding the lock, so a poisoned one is whole.
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if failures
            .last_logged
            .is_some_and(|last| now.duration_since(last) < ACCEPT_FAILURE_LOG_INTERVAL)
        {
            failures.unlogged += 1;
            return;
        }
        failures.last_logged = Some(now);
        let unlogged = std::mem::take(&mut failures.unlogged);
        drop(failures);
        tracing::error!(
            %error,
            unlogged,
            "could not accept a connection; with \"Too many open files\", raise the open-file limit"
        );
    }
}

impl AddrListener<TcpStream, Endpoint> for Listener {
    async fn bind_to(address: Endpoint) -> io::Result<Self> {
        Self::bind(address.0).await
    }

    async fn accept_stream(&self) -> io::Result<(TcpStream, Endpoint)> {
        match self.inner.accept().await {
            Ok((stream, peer)) => {
                // The connection still works without them.
                if let Err(error) = set_socket_options(&stream) {
                    tracing::debug!(%error, %peer, "could not set socket options");
                }
                Ok((stream, Endpoint(peer)))
            }
            Err(error) => {
                self.failed(&error);
                Err(error)
            }
        }
    }

    fn get_local_addr(&self) -> io::Result<Endpoint> {
        self.inner.local_addr().map(Endpoint)
    }
}

/// Small writes, terminal output and signaling above all, go out at once
/// instead of waiting on Nagle's algorithm and the peer's delayed ACK.
fn set_socket_options(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE_IDLE);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let keepalive = keepalive
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_PROBES);
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Wraps an acceptor (TLS, or none) in a deadline for its handshake, and
/// hands on a stream that fails reading if HTTP has not started in time.
#[derive(Debug, Clone)]
pub struct Limited<A> {
    inner: A,
    timeouts: Timeouts,
}

impl<A> Limited<A> {
    pub fn new(inner: A, timeouts: Timeouts) -> Self {
        Self { inner, timeouts }
    }
}

impl<A, I, S> Accept<I, S> for Limited<A>
where
    A: Accept<I, S>,
    A::Future: Send + 'static,
    A::Stream: Send + 'static,
    A::Service: Send + 'static,
{
    type Stream = HttpStart<A::Stream>;
    type Service = A::Service;
    type Future = BoxFuture<'static, io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let accepting = self.inner.accept(stream, service);
        let Timeouts {
            header_read,
            tls_handshake,
        } = self.timeouts;
        Box::pin(async move {
            let (stream, service) = tokio::time::timeout(tls_handshake, accepting)
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out")
                })??;
            Ok((HttpStart::new(stream, header_read), service))
        })
    }
}

/// What an HTTP/2 connection starts with.
const HTTP2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// A connection's stream that fails reading if HTTP has not started by a
/// deadline. hyper-util reads until it has the 24-byte HTTP/2 preface or a
/// byte that rules it out before handing the connection to HTTP/1 or HTTP/2,
/// with no timeout of its own, so a client that sends nothing, or a preface
/// a byte at a time, would hold the connection forever. Once those bytes are
/// in, hyper's own timeouts take over and this passes everything through.
#[derive(Debug)]
pub struct HttpStart<S> {
    inner: S,
    /// Until HTTP has started.
    deadline: Option<Pin<Box<Sleep>>>,
    /// How much of the preface has arrived.
    matched: usize,
}

impl<S> HttpStart<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            deadline: Some(Box::pin(tokio::time::sleep(timeout))),
            matched: 0,
        }
    }

    fn started(&mut self, read: &[u8]) {
        let end = (self.matched + read.len()).min(HTTP2_PREFACE.len());
        let preface = read[..end - self.matched] == HTTP2_PREFACE[self.matched..end];
        self.matched = end;
        // An empty read is the end of the stream.
        if read.is_empty() || !preface || end == HTTP2_PREFACE.len() {
            self.deadline = None;
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for HttpStart<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.deadline.is_none() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                this.started(&buf.filled()[before..]);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => match this
                .deadline
                .as_mut()
                .map(|deadline| deadline.as_mut().poll(cx))
            {
                Some(Poll::Ready(())) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the client did not start HTTP in time",
                ))),
                _ => Poll::Pending,
            },
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HttpStart<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Issues stateless TLS session tickets, so a client that reconnects resumes
/// its session instead of a full handshake with a certificate signature.
/// Without one, rustls keeps TLS 1.3 sessions in a 256-entry cache, which a
/// fleet of Agents overruns at once. Its keys live in memory and rotate every
/// 6 hours; after a restart clients make one full handshake again.
fn ticketer() -> anyhow::Result<Arc<dyn ProducesTickets>> {
    rustls::crypto::ring::Ticketer::new().context("could not set up TLS session tickets")
}

/// A certificate and key in PEM files, and the session ticket keys that
/// outlast each reload of them.
#[derive(Debug, Clone)]
pub struct CertificateFiles {
    cert_path: PathBuf,
    key_path: PathBuf,
    ticketer: Arc<dyn ProducesTickets>,
}

impl CertificateFiles {
    pub fn new(cert_path: PathBuf, key_path: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            cert_path,
            key_path,
            ticketer: ticketer()?,
        })
    }

    /// Reads the files into TLS settings.
    pub async fn load(&self) -> anyhow::Result<Arc<ServerConfig>> {
        self.read().await.with_context(|| {
            format!(
                "could not load the certificate {} and key {}",
                self.cert_path.display(),
                self.key_path.display()
            )
        })
    }

    async fn read(&self) -> anyhow::Result<Arc<ServerConfig>> {
        let cert = tokio::fs::read(&self.cert_path).await?;
        let key = tokio::fs::read(&self.key_path).await?;
        let chain = CertificateDer::pem_slice_iter(&cert).collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(!chain.is_empty(), "the certificate file has no certificate");
        let key = PrivateKeyDer::from_pem_slice(&key)?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        config.ticketer = self.ticketer.clone();
        Ok(Arc::new(config))
    }
}

/// Reloads the certificate whenever either file's modification time changes.
/// A failed reload keeps the certificate already in use.
pub async fn reload_on_change(tls: RustlsConfig, files: CertificateFiles, interval: Duration) {
    let mut last = modified(&files.cert_path, &files.key_path);
    loop {
        tokio::time::sleep(interval).await;
        let current = modified(&files.cert_path, &files.key_path);
        if current == last {
            continue;
        }
        match files.load().await {
            Ok(config) => {
                tls.reload_from_config(config);
                tracing::info!(cert = %files.cert_path.display(), "reloaded the TLS certificate");
                last = current;
            }
            // A renewal tool may be midway through writing the pair; the next
            // poll sees the finished files.
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not reload the TLS certificate; keeping the current one"
                )
            }
        }
    }
}

fn modified(cert_path: &Path, key_path: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
    let modified = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    };
    (modified(cert_path), modified(key_path))
}

/// Warns when the open-file limit would cap connections well below a large
/// fleet: each connection takes a descriptor, and past the limit the server
/// can accept none.
pub fn warn_about_open_file_limit() {
    #[cfg(unix)]
    if let Some(limit) = rustix::process::getrlimit(rustix::process::Resource::Nofile).current
        && limit < LOW_OPEN_FILE_LIMIT
    {
        tracing::warn!(
            limit,
            "the open-file limit is low; it caps how many Agents, browsers, terminals, file transfers and relays can be connected at once. Raise it to 1048576 (LimitNOFILE in the systemd unit, --ulimit nofile with Docker)"
        );
    }
}
