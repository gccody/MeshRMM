//! Checks of other computers, from the device: ping, a TCP port, an HTTP(S)
//! address, and when a TLS certificate expires.
use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use super::{Outcome, parse, platform};

/// The most of a page searched for a keyword.
const MAX_BODY_BYTES: u64 = 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

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

fn resolve(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| anyhow::anyhow!("could not look up “{host}”: {error}"))?
        .collect::<Vec<_>>();
    anyhow::ensure!(!addresses.is_empty(), "“{host}” has no address");
    Ok(addresses)
}

/// A connection to the first of the host's addresses that answers.
fn connect(host: &str, port: u16, timeout: Duration) -> anyhow::Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    for address in resolve(host, port)? {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, left) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) => anyhow::anyhow!("{error}"),
        None => anyhow::anyhow!("timed out"),
    })
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
    match connect(host, port, timeout) {
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
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        // A redirect's own status is what is checked.
        .max_redirects(0)
        .user_agent("MeshRMM-Agent")
        .tls_config(tls)
        .build()
        .new_agent();
    let started = Instant::now();
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
    let mut stream = connect(host, port, timeout)?;
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
        server.join().unwrap();
    }
}
