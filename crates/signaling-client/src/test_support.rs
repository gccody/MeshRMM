//! A local TLS server for handshake tests in the workspace's crates.

use rustls::{
    RootCertStore, ServerConfig, SupportedProtocolVersion,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

/// Accepts only `version`, with a self-signed certificate for `localhost`,
/// and answers every HTTP request with 204 No Content.
pub struct TlsServer {
    pub address: SocketAddr,
    pub certificate: CertificateDer<'static>,
    task: JoinHandle<()>,
}

impl TlsServer {
    pub async fn start(version: &'static SupportedProtocolVersion) -> Self {
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
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut buffer = [0; 4096];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            address,
            certificate,
            task,
        }
    }

    pub fn roots(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots.add(self.certificate.clone()).unwrap();
        roots
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
