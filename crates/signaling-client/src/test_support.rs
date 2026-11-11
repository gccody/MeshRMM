//! A local TLS server for handshake tests in the workspace's crates.

use rustls::{
    RootCertStore, ServerConfig, SupportedProtocolVersion,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

const NO_CONTENT: &[u8] = b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n";
/// The most of a request [`TlsServer`] reads.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Accepts only `version`, with a self-signed certificate for `localhost`,
/// and answers every HTTP request with 204 No Content, or the response it was
/// started with.
pub struct TlsServer {
    pub address: SocketAddr,
    pub certificate: CertificateDer<'static>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl TlsServer {
    pub async fn start(version: &'static SupportedProtocolVersion) -> Self {
        Self::answering(version, NO_CONTENT).await
    }

    /// Answers every request with `response`, a whole HTTP/1.1 response, and
    /// keeps each request it reads, head and body, for [`TlsServer::requests`].
    pub async fn answering(version: &'static SupportedProtocolVersion, response: &[u8]) -> Self {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = generated.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[version])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], key.into())
                .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let response: Arc<[u8]> = response.into();
        let task = tokio::spawn({
            let requests = Arc::clone(&requests);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let requests = Arc::clone(&requests);
                    let response = Arc::clone(&response);
                    tokio::spawn(async move {
                        let Ok(mut stream) = acceptor.accept(stream).await else {
                            return;
                        };
                        let mut request = Vec::new();
                        let mut buffer = [0; 4096];
                        while !is_whole(&request) && request.len() < MAX_REQUEST_BYTES {
                            match stream.read(&mut buffer).await {
                                Ok(0) | Err(_) => break,
                                Ok(read) => request.extend_from_slice(&buffer[..read]),
                            }
                        }
                        requests.lock().unwrap().push(request);
                        let _ = stream.write_all(&response).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });
        Self {
            address,
            certificate,
            requests,
            task,
        }
    }

    pub fn roots(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots.add(self.certificate.clone()).unwrap();
        roots
    }

    /// The requests read so far, oldest first.
    pub fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

/// Whether `request` holds its head and the body its `content-length` gives.
fn is_whole(request: &[u8]) -> bool {
    let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|length| length.trim().parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= end + 4 + length
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
