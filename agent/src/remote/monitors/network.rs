//! Checks of other computers, from the device: ping, a TCP port, an HTTP(S)
//! address, and when a TLS certificate expires.
//!
//! A check's name lookup and connection share its one deadline. A lookup
//! runs on a thread of its own, so a DNS server that doesn't answer costs
//! the check its timeout and no more, and a host's addresses are tried
//! side by side, a moment apart, so one whose packets are dropped (as IPv6
//! often is) doesn't keep the others from being tried.
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    RustlsConnector, Transport,
};

use super::{Outcome, parse, platform};

/// The most of a page searched for a keyword.
const MAX_BODY_BYTES: u64 = 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How long an attempt to connect has to itself before the host's next
/// address is tried alongside it, as browsers do.
const HEAD_START: Duration = Duration::from_millis(250);
/// The most of a host's addresses tried, and the most tried at once.
const MAX_ADDRESSES: usize = 8;
const MAX_ATTEMPTS_AT_ONCE: usize = 4;
/// The most name lookups running at once, those a check stopped waiting
/// for included. A lookup can't be stopped, so one stuck on a DNS server
/// that doesn't answer keeps its thread until the system gives up on it;
/// past this, checks fail at once instead of starting more. Twice the
/// checks the scheduler runs at once, so they keep room for their own
/// lookups while as many are stuck.
const MAX_LOOKUPS: usize = 16;

/// Name lookups running now.
static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

fn millis(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() * 10_000.0).round() / 10.0
}

/// `host` with brackets around an IPv6 address, for messages and URLs.
fn shown(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

type Lookup = fn(&str, u16) -> io::Result<Vec<SocketAddr>>;

fn system_lookup(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok((host, port).to_socket_addrs()?.collect())
}

/// The host's addresses, looked up by `deadline`.
pub(super) fn resolve(host: &str, port: u16, deadline: Instant) -> anyhow::Result<Vec<SocketAddr>> {
    resolve_with(host, port, deadline, &LOOKUPS, system_lookup)
}

/// A lookup's place among those running, given back when its thread ends,
/// however it ends.
struct LookupPlace(&'static AtomicUsize);

impl Drop for LookupPlace {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn resolve_with(
    host: &str,
    port: u16,
    deadline: Instant,
    running: &'static AtomicUsize,
    lookup: Lookup,
) -> anyhow::Result<Vec<SocketAddr>> {
    // An address needs no lookup.
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    let left = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(!left.is_zero(), "could not look up “{host}” in time");
    running
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_LOOKUPS).then_some(count + 1)
        })
        .map_err(|_| {
            anyhow::anyhow!(
                "could not look up “{host}”: {MAX_LOOKUPS} lookups are already waiting for an answer"
            )
        })?;
    let place = LookupPlace(running);
    let (sender, receiver) = mpsc::sync_channel(1);
    let name = host.to_owned();
    std::thread::Builder::new()
        .name("monitor lookup".to_owned())
        .spawn(move || {
            let found = lookup(&name, port);
            drop(place);
            let _ = sender.send(found);
        })
        .map_err(|error| anyhow::anyhow!("could not look up “{host}”: {error}"))?;
    let addresses = match receiver.recv_timeout(left) {
        Ok(found) => {
            found.map_err(|error| anyhow::anyhow!("could not look up “{host}”: {error}"))?
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!("could not look up “{host}” in time")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("could not look up “{host}”"),
    };
    anyhow::ensure!(!addresses.is_empty(), "“{host}” has no address");
    Ok(addresses)
}

/// The addresses to try, at most [`MAX_ADDRESSES`] of them: the families
/// taking turns, starting with the system's first, each in the system's
/// order.
fn order(addresses: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let (mut first, mut other) = (Vec::new(), Vec::new());
    let mut first_family = None;
    for address in addresses {
        let family = *first_family.get_or_insert(address.is_ipv6());
        let list = if address.is_ipv6() == family {
            &mut first
        } else {
            &mut other
        };
        if !list.contains(&address) {
            list.push(address);
        }
    }
    let (mut first, mut other) = (first.into_iter(), other.into_iter());
    let mut ordered = Vec::new();
    while ordered.len() < MAX_ADDRESSES {
        match (first.next(), other.next()) {
            (None, None) => break,
            (one, two) => ordered.extend(one.into_iter().chain(two)),
        }
    }
    ordered.truncate(MAX_ADDRESSES);
    ordered
}

type Attempt = fn(&SocketAddr, Duration) -> io::Result<TcpStream>;

/// How far an attempt that failed got: the host refusing is news of it,
/// a timeout may be its packets being dropped, and the rest (no route, for
/// one) usually means the device can't reach that kind of address at all.
fn reach(error: &io::Error) -> u8 {
    match error.kind() {
        io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => 2,
        io::ErrorKind::TimedOut => 1,
        _ => 0,
    }
}

/// A connection to the first of `addresses` that answers by `deadline`.
/// Each attempt has `head_start` to itself, or until it fails, before the
/// next starts alongside it. The attempts that lose end by the deadline,
/// and a connection one of them makes anyway is closed.
fn connect_to(
    addresses: &[SocketAddr],
    deadline: Instant,
    head_start: Duration,
    attempt: Attempt,
) -> io::Result<TcpStream> {
    let (sender, receiver) = mpsc::channel();
    let mut next = 0;
    let mut running = 0;
    let mut next_start = Instant::now();
    // The failure of the address that got furthest, and how far.
    let mut furthest: Option<(u8, SocketAddr, io::Error)> = None;
    let mut failed = |address, error: io::Error| {
        if furthest
            .as_ref()
            .is_none_or(|(far, _, _)| reach(&error) > *far)
        {
            furthest = Some((reach(&error), address, error));
        }
    };
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let more = next < addresses.len() && running < MAX_ATTEMPTS_AT_ONCE;
        if more && (running == 0 || now >= next_start) {
            let address = addresses[next];
            next += 1;
            next_start = now + head_start;
            let sender = sender.clone();
            let left = deadline - now;
            let started = std::thread::Builder::new()
                .name("monitor connect".to_owned())
                // A connection made after another won is dropped, so
                // closed, when it can't be sent.
                .spawn(move || drop(sender.send((address, attempt(&address, left)))));
            match started {
                Ok(_) => running += 1,
                Err(error) => failed(address, error),
            }
            continue;
        }
        if running == 0 {
            break;
        }
        let until = if more {
            next_start.min(deadline)
        } else {
            deadline
        };
        if let Ok((address, result)) = receiver.recv_timeout(until.saturating_duration_since(now)) {
            running -= 1;
            match result {
                Ok(stream) => return Ok(stream),
                // The next address needn't wait.
                Err(error) => {
                    failed(address, error);
                    next_start = Instant::now();
                }
            }
        }
    }
    // Attempts still running, or never started, ran out of time.
    let unfinished = running > 0 || next < addresses.len();
    Err(match furthest {
        Some((far, address, error)) if far == 2 || (far == 0 && !unfinished) => {
            if addresses.len() > 1 {
                io::Error::new(error.kind(), format!("{error}, at {address}"))
            } else {
                error
            }
        }
        _ => io::Error::new(io::ErrorKind::TimedOut, "timed out"),
    })
}

/// A connection to `host:port` by `deadline`, its lookup included.
fn connect(host: &str, port: u16, deadline: Instant) -> anyhow::Result<TcpStream> {
    let addresses = order(resolve(host, port, deadline)?);
    Ok(connect_to(
        &addresses,
        deadline,
        HEAD_START,
        TcpStream::connect_timeout,
    )?)
}

pub(super) fn ping(host: &str, timeout: Duration) -> Outcome {
    match platform::ping(host, timeout) {
        Ok(Some(round_trip)) => {
            Outcome::passing(format!("“{host}” answered in {round_trip} ms.")).value(round_trip)
        }
        Ok(None) => Outcome::failing(format!(
            "“{host}” didn't answer within {} seconds.",
            timeout.as_secs()
        )),
        Err(error) => Outcome::failing(format!("Could not ping “{host}”: {error:#}")),
    }
}

pub(super) fn tcp(host: &str, port: u16, timeout: Duration) -> Outcome {
    let started = Instant::now();
    match connect(host, port, started + timeout) {
        Ok(_) => {
            let elapsed = millis(started.elapsed());
            Outcome::passing(format!(
                "{} accepted a connection in {elapsed} ms.",
                shown(host, port)
            ))
            .value(elapsed)
        }
        Err(error) => Outcome::failing(format!(
            "{} didn't accept a connection: {error:#}",
            shown(host, port)
        )),
    }
}

/// Why the server's certificate wasn't trusted, in plain words, if that's
/// why a request failed. ureq passes rustls' error on as it is or inside an
/// I/O error.
fn certificate_problem(error: &ureq::Error) -> Option<&'static str> {
    let tls = match error {
        ureq::Error::Rustls(tls) => tls,
        ureq::Error::Io(io) => io.get_ref()?.downcast_ref::<rustls::Error>()?,
        _ => return None,
    };
    let rustls::Error::InvalidCertificate(certificate) = tls else {
        return None;
    };
    use rustls::CertificateError as Why;
    Some(match certificate {
        Why::Expired | Why::ExpiredContext { .. } => "its certificate has expired.",
        Why::NotValidYet | Why::NotValidYetContext { .. } => {
            "its certificate isn't valid yet; check the device's clock."
        }
        Why::UnknownIssuer => {
            "its certificate isn't from an authority the device trusts; it may be self-signed or from a private authority."
        }
        Why::NotValidForName | Why::NotValidForNameContext { .. } => {
            "its certificate is for a different name."
        }
        Why::Revoked => "its certificate was revoked.",
        Why::BadSignature | Why::BadEncoding | Why::UnsupportedSignatureAlgorithmContext { .. } => {
            "its certificate is damaged or signed in a way the device can't check."
        }
        Why::InvalidPurpose | Why::InvalidPurposeContext { .. } => {
            "its certificate isn't meant for servers."
        }
        // webpki's own reasons arrive as text.
        Why::Other(other) => {
            let why = format!("{other:?}");
            if why.contains("CaUsedAsEndEntity") {
                "it presents a certificate authority's certificate as its own, as a self-signed setup often does, and the device doesn't trust it."
            } else if why.contains("UnknownIssuer") {
                "its certificate isn't from an authority the device trusts; it may be self-signed or from a private authority."
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// ureq's name lookups for a check, as the other checks make them.
#[derive(Debug)]
struct CheckResolver {
    deadline: Instant,
}

impl Resolver for CheckResolver {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri
            .host()
            .ok_or_else(|| ureq::Error::BadUri(format!("{uri} has no host")))?;
        let port = match (uri.port_u16(), uri.scheme_str()) {
            (Some(port), _) => port,
            (None, Some("http")) => 80,
            (None, Some("https")) => 443,
            _ => return Err(ureq::Error::BadUri(format!("{uri} has no port"))),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let found = resolve(host, port, self.deadline)
            .map_err(|error| ureq::Error::Io(io::Error::other(format!("{error:#}"))))?;
        let mut addresses = self.empty();
        for address in order(found) {
            addresses.push(address);
        }
        Ok(addresses)
    }
}

/// ureq's connections for a check, as the other checks make them.
#[derive(Debug)]
struct CheckConnector {
    deadline: Instant,
}

impl<In: Transport> Connector<In> for CheckConnector {
    type Out = Either<In, CheckTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        // A proxy's connection, already made.
        if let Some(chained) = chained {
            return Ok(Some(Either::A(chained)));
        }
        let addresses = order(details.addrs.iter().copied());
        let stream = connect_to(
            &addresses,
            self.deadline,
            HEAD_START,
            TcpStream::connect_timeout,
        )
        .map_err(|error| transport_error(error, details.timeout))?;
        if details.config.no_delay() {
            stream.set_nodelay(true)?;
        }
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(CheckTransport { stream, buffers })))
    }
}

fn transport_error(error: io::Error, timeout: NextTimeout) -> ureq::Error {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ureq::Error::Timeout(timeout.reason),
        _ => ureq::Error::Io(error),
    }
}

/// A plain TCP connection for ureq, as its own is.
#[derive(Debug)]
struct CheckTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
}

impl Transport for CheckTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream
            .set_write_timeout(timeout.not_zero().map(|left| *left))?;
        self.stream
            .write_all(&self.buffers.output()[..amount])
            .map_err(|error| transport_error(error, timeout))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream
            .set_read_timeout(timeout.not_zero().map(|left| *left))?;
        let read = self
            .stream
            .read(self.buffers.input_append_buf())
            .map_err(|error| transport_error(error, timeout))?;
        self.buffers.input_appended(read);
        Ok(read > 0)
    }

    // A check's agent makes one request, so its connection is never
    // reused.
    fn is_open(&mut self) -> bool {
        false
    }
}

pub(super) fn http(
    url: &str,
    expected: u16,
    keyword: &str,
    verify_tls: bool,
    timeout: Duration,
) -> Outcome {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(crypto_provider())
        .disable_verification(!verify_tls)
        .build();
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        // A redirect's own status is what is checked.
        .max_redirects(0)
        .user_agent("MeshRMM-Agent")
        .tls_config(tls)
        .build();
    let started = Instant::now();
    let deadline = started + timeout;
    // ureq's own chain, less SOCKS proxies, which it isn't built for.
    let connector =
        ().chain(ConnectProxyConnector::default())
            .chain(CheckConnector { deadline })
            .chain(RustlsConnector::default());
    let agent = ureq::Agent::with_parts(config, connector, CheckResolver { deadline });
    let mut response = match agent.get(url).call() {
        Ok(response) => response,
        Err(error) => {
            return match certificate_problem(&error) {
                // The plain words, with what TLS said kept as the output.
                Some(problem) => Outcome::failing(format!("“{url}” didn't answer: {problem}"))
                    .output(format!("{error}\n")),
                None => Outcome::failing(format!("“{url}” didn't answer: {error}")),
            };
        }
    };
    let status = response.status().as_u16();
    let mut body = Vec::new();
    let read = if keyword.is_empty() {
        Ok(())
    } else {
        response
            .body_mut()
            .as_reader()
            .take(MAX_BODY_BYTES)
            .read_to_end(&mut body)
            .map(drop)
    };
    let elapsed = millis(started.elapsed());
    if status != expected {
        return Outcome::failing(format!(
            "“{url}” answered {status} in {elapsed} ms, not {expected}."
        ))
        .value(elapsed);
    }
    if let Err(error) = read {
        return Outcome::failing(format!("Could not read “{url}”'s page: {error}")).value(elapsed);
    }
    if !keyword.is_empty() && !String::from_utf8_lossy(&body).contains(keyword) {
        return Outcome::failing(format!(
            "“{url}” answered {status} in {elapsed} ms without “{keyword}” in its page."
        ))
        .value(elapsed);
    }
    Outcome::passing(format!("“{url}” answered {status} in {elapsed} ms.")).value(elapsed)
}

/// Accepts any certificate, keeping the server's own: the check is when it
/// expires, not whether the device trusts it.
#[derive(Debug)]
struct KeepCertificate {
    provider: Arc<CryptoProvider>,
    certificate: Mutex<Option<Vec<u8>>>,
}

impl ServerCertVerifier for KeepCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self
            .certificate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(end_entity.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The DER of the certificate `host:port` presents.
fn server_certificate(host: &str, port: u16, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let provider = crypto_provider();
    let verifier = Arc::new(KeepCertificate {
        provider: Arc::clone(&provider),
        certificate: Mutex::new(None),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(&verifier) as Arc<dyn ServerCertVerifier>)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_owned())
        .map_err(|_| anyhow::anyhow!("“{host}” isn't a host name or address"))?;
    let mut connection = rustls::ClientConnection::new(Arc::new(config), name)?;
    let deadline = Instant::now() + timeout;
    let mut stream = connect(host, port, deadline)?;
    while connection.is_handshaking() {
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!left.is_zero(), "the TLS handshake timed out");
        stream.set_read_timeout(Some(left))?;
        stream.set_write_timeout(Some(left))?;
        if let Err(error) = connection.complete_io(&mut stream) {
            // The certificate arrives before a failure the check can ignore.
            if verifier
                .certificate
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
            {
                break;
            }
            return Err(anyhow::anyhow!("the TLS handshake failed: {error}"));
        }
    }
    verifier
        .certificate
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| anyhow::anyhow!("the server sent no certificate"))
}

pub(super) fn certificate(
    host: &str,
    port: u16,
    days: u32,
    timeout: Duration,
    now_ms: i64,
) -> Outcome {
    let where_ = shown(host, port);
    let der = match server_certificate(host, port, timeout) {
        Ok(der) => der,
        Err(error) => {
            return Outcome::failing(format!("Could not read {where_}'s certificate: {error:#}"));
        }
    };
    let Some(not_after) = parse::certificate_not_after(&der) else {
        return Outcome::failing(format!("Could not read {where_}'s certificate's expiry."));
    };
    judge_certificate(&where_, not_after, days, now_ms)
}

pub(super) fn judge_certificate(where_: &str, not_after: i64, days: u32, now_ms: i64) -> Outcome {
    let left = not_after - now_ms;
    let date = parse::format_date(not_after);
    let whole_days = left.div_euclid(DAY_MS);
    let outcome = if left <= 0 {
        Outcome::failing(format!("{where_}'s certificate expired on {date}."))
    } else {
        let when = match whole_days {
            0 => "within a day".to_owned(),
            1 => "in 1 day".to_owned(),
            _ => format!("in {whole_days} days"),
        };
        Outcome::when(
            left > i64::from(days) * DAY_MS,
            format!("{where_}'s certificate expires on {date}, {when}."),
        )
    };
    outcome.value(whole_days as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificates_fail_inside_the_warning() {
        let now = parse::parse_timestamp("2026-11-09T00:00:00Z").unwrap();
        let expires = parse::parse_timestamp("2026-11-20T12:00:00Z").unwrap();
        let ok = judge_certificate("mail.example:993", expires, 7, now);
        assert!(ok.passing);
        assert_eq!(
            ok.detail,
            "mail.example:993's certificate expires on 2026-11-20, in 11 days."
        );
        assert_eq!(ok.value, Some(11.0));
        assert!(!judge_certificate("x:443", expires, 14, now).passing);
        let expired = judge_certificate("x:443", now - 1, 14, now);
        assert!(!expired.passing);
        assert_eq!(expired.detail, "x:443's certificate expired on 2026-11-08.");
    }

    #[test]
    fn hosts_show_with_their_port() {
        assert_eq!(shown("::1", 443), "[::1]:443");
        assert_eq!(shown("example.com", 80), "example.com:80");
    }

    #[test]
    fn ports_are_open_or_closed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(tcp("127.0.0.1", port, Duration::from_secs(5)).passing);
        drop(listener);
        let closed = tcp("127.0.0.1", port, Duration::from_secs(5));
        assert!(!closed.passing);
        assert!(
            closed
                .detail
                .starts_with(&format!("127.0.0.1:{port} didn't accept"))
        );
    }

    #[test]
    fn certificate_failures_read_plainly() {
        use rustls::{CertificateError, OtherError};
        #[derive(Debug)]
        struct CaUsedAsEndEntity;
        impl std::fmt::Display for CaUsedAsEndEntity {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("CaUsedAsEndEntity")
            }
        }
        impl std::error::Error for CaUsedAsEndEntity {}
        // As ureq reports it: rustls' error inside an I/O error.
        let wrapped = |why: CertificateError| {
            ureq::Error::Io(std::io::Error::other(rustls::Error::InvalidCertificate(
                why,
            )))
        };
        let self_signed = wrapped(CertificateError::Other(OtherError(Arc::new(
            CaUsedAsEndEntity,
        ))));
        assert_eq!(
            self_signed.to_string(),
            "io: invalid peer certificate: Other(OtherError(CaUsedAsEndEntity))",
            "what the device used to show"
        );
        assert!(
            certificate_problem(&self_signed)
                .unwrap()
                .starts_with("it presents a certificate authority's certificate")
        );
        for (why, plain) in [
            (CertificateError::Expired, "its certificate has expired."),
            (
                CertificateError::UnknownIssuer,
                "its certificate isn't from an authority the device trusts; it may be self-signed or from a private authority.",
            ),
            (
                CertificateError::NotValidForName,
                "its certificate is for a different name.",
            ),
        ] {
            assert_eq!(certificate_problem(&wrapped(why.clone())), Some(plain));
            assert_eq!(
                certificate_problem(&ureq::Error::Rustls(rustls::Error::InvalidCertificate(why))),
                Some(plain)
            );
        }
        assert_eq!(
            certificate_problem(&ureq::Error::Io(std::io::Error::other("refused"))),
            None
        );
        assert_eq!(
            certificate_problem(&ureq::Error::Timeout(ureq::Timeout::Global)),
            None
        );
    }

    #[test]
    fn http_checks_status_and_keyword() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            use std::io::Write;
            for reply in [
                "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nall is well",
                "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nall is well",
                "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nall is well",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let url = format!("http://127.0.0.1:{port}/health");
        let timeout = Duration::from_secs(5);
        let ok = http(&url, 200, "well", true, timeout);
        assert!(ok.passing, "{}", ok.detail);
        let missing = http(&url, 200, "broken", true, timeout);
        assert!(!missing.passing);
        assert!(
            missing.detail.contains("without “broken”"),
            "{}",
            missing.detail
        );
        let redirected = http(&url, 200, "", true, timeout);
        assert!(
            redirected.detail.contains("answered 302"),
            "{}",
            redirected.detail
        );
        // Looked up as the other checks are; any IPv6 address of it refuses.
        let named = http(
            &format!("http://localhost:{port}/health"),
            200,
            "well",
            true,
            timeout,
        );
        assert!(named.passing, "{}", named.detail);
        server.join().unwrap();
    }

    const BLACK_HOLE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));
    const REFUSING: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 2));

    /// Connects for real, but to two made-up addresses as if one's packets
    /// were dropped and the other refused at once.
    fn attempt(address: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        match address.ip() {
            ip if ip == BLACK_HOLE => {
                std::thread::sleep(timeout);
                Err(io::ErrorKind::TimedOut.into())
            }
            ip if ip == REFUSING => Err(io::ErrorKind::ConnectionRefused.into()),
            _ => TcpStream::connect_timeout(address, timeout),
        }
    }

    #[test]
    fn an_address_whose_packets_are_dropped_does_not_hold_up_the_next() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let good = listener.local_addr().unwrap();
        let started = Instant::now();
        let stream = connect_to(
            &[SocketAddr::new(BLACK_HOLE, good.port()), good],
            started + Duration::from_secs(10),
            Duration::from_millis(50),
            attempt,
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), good);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_refusal_moves_on_without_the_head_start() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let good = listener.local_addr().unwrap();
        let started = Instant::now();
        let stream = connect_to(
            &[SocketAddr::new(REFUSING, good.port()), good],
            started + Duration::from_secs(20),
            Duration::from_secs(10),
            attempt,
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), good);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn addresses_share_one_deadline() {
        let timeout = Duration::from_millis(100);
        let addresses: Vec<_> = (1..=3)
            .map(|port| SocketAddr::new(BLACK_HOLE, port))
            .collect();
        let started = Instant::now();
        let error = connect_to(
            &addresses,
            started + timeout,
            Duration::from_millis(20),
            attempt,
        )
        .unwrap_err();
        let took = started.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "timed out");
        assert!(took >= timeout && took < Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn the_furthest_failure_is_told() {
        let error = connect_to(
            &[
                SocketAddr::new(BLACK_HOLE, 443),
                SocketAddr::new(REFUSING, 443),
            ],
            Instant::now() + Duration::from_millis(100),
            Duration::from_millis(20),
            attempt,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert!(error.to_string().ends_with(", at 192.0.2.2:443"), "{error}");
    }

    #[test]
    fn only_a_few_attempts_run_at_once() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        fn counted(_: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
            let running = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
            MOST.fetch_max(running, Ordering::SeqCst);
            std::thread::sleep(timeout);
            RUNNING.fetch_sub(1, Ordering::SeqCst);
            Err(io::ErrorKind::TimedOut.into())
        }
        let addresses: Vec<_> = (1..=8)
            .map(|port| SocketAddr::new(BLACK_HOLE, port))
            .collect();
        let error = connect_to(
            &addresses,
            Instant::now() + Duration::from_millis(200),
            Duration::from_millis(1),
            counted,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "timed out");
        assert_eq!(MOST.load(Ordering::SeqCst), MAX_ATTEMPTS_AT_ONCE);
    }

    #[test]
    fn address_families_take_turns() {
        let v6 = |n| {
            SocketAddr::new(
                IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, n)),
                443,
            )
        };
        let v4 = |n| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, n)), 443);
        assert_eq!(
            order([v6(1), v6(2), v6(3), v4(1), v4(2), v6(1)]),
            [v6(1), v4(1), v6(2), v4(2), v6(3)]
        );
        assert_eq!(order([v4(1), v6(1), v4(2)]), [v4(1), v6(1), v4(2)]);
        let many = order((1..=20).map(v6).chain([v4(1)]));
        assert_eq!(many.len(), MAX_ADDRESSES);
        assert_eq!(many[1], v4(1));
    }

    #[test]
    fn lookups_give_up_at_the_deadline() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        fn slow(_: &str, _: u16) -> io::Result<Vec<SocketAddr>> {
            std::thread::sleep(Duration::from_secs(5));
            Ok(Vec::new())
        }
        let started = Instant::now();
        let error = resolve_with(
            "stuck.example",
            443,
            started + Duration::from_millis(50),
            &RUNNING,
            slow,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "could not look up “stuck.example” in time"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(RUNNING.load(Ordering::SeqCst), 1, "still stuck");
    }

    #[test]
    fn stuck_lookups_are_capped_until_they_end() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static RELEASED: Mutex<bool> = Mutex::new(false);
        static WAKE: std::sync::Condvar = std::sync::Condvar::new();
        fn stuck(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            if host == "stuck.example" {
                let mut released = RELEASED.lock().unwrap();
                while !*released {
                    released = WAKE.wait(released).unwrap();
                }
            }
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        }
        for _ in 0..MAX_LOOKUPS {
            let soon = Instant::now() + Duration::from_millis(20);
            let error = resolve_with("stuck.example", 443, soon, &RUNNING, stuck).unwrap_err();
            assert!(error.to_string().ends_with("in time"), "{error}");
        }
        let started = Instant::now();
        let later = started + Duration::from_secs(10);
        let error = resolve_with("fine.example", 443, later, &RUNNING, stuck).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            error.to_string(),
            format!(
                "could not look up “fine.example”: {MAX_LOOKUPS} lookups are already waiting for an answer"
            )
        );
        *RELEASED.lock().unwrap() = true;
        WAKE.notify_all();
        while RUNNING.load(Ordering::SeqCst) > 0 {
            assert!(Instant::now() < later, "the stuck lookups never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        let found = resolve_with("fine.example", 443, later, &RUNNING, stuck).unwrap();
        assert_eq!(found, [SocketAddr::from(([127, 0, 0, 1], 443))]);
    }

    #[test]
    fn addresses_are_not_looked_up() {
        // Full, so a lookup would be refused.
        static RUNNING: AtomicUsize = AtomicUsize::new(MAX_LOOKUPS);
        fn unused(host: &str, _: u16) -> io::Result<Vec<SocketAddr>> {
            panic!("{host} was looked up")
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        for (host, address) in [
            ("192.0.2.7", "192.0.2.7:80"),
            ("2001:db8::7", "[2001:db8::7]:80"),
        ] {
            let found = resolve_with(host, 80, deadline, &RUNNING, unused).unwrap();
            assert_eq!(found, [address.parse::<SocketAddr>().unwrap()]);
        }
        assert_eq!(RUNNING.load(Ordering::SeqCst), MAX_LOOKUPS);
    }
}
