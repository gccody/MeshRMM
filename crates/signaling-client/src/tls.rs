//! TLS settings shared by the Agent's and viewer's HTTPS and WSS clients.
//!
//! Every MeshRMM host accepts only TLS 1.3, so the clients never offer an
//! older version. The WebRTC media path uses DTLS 1.2; see
//! docs/transport-security.md.

use anyhow::bail;
use rustls::{ClientConfig, RootCertStore, crypto::CryptoProvider, version::TLS13};
use std::sync::Arc;

/// The ring provider reduced to TLS 1.3 cipher suites. rustls offers a
/// protocol version only when a cipher suite for it is available, so clients
/// that always enable every version, such as ureq, still offer only TLS 1.3.
pub fn tls13_crypto_provider() -> Arc<CryptoProvider> {
    let mut provider = rustls::crypto::ring::default_provider();
    provider
        .cipher_suites
        .retain(|suite| suite.version() == &TLS13);
    Arc::new(provider)
}

/// A TLS 1.3-only client configuration that trusts `roots`.
pub fn tls13_client_config(roots: RootCertStore) -> anyhow::Result<Arc<ClientConfig>> {
    Ok(Arc::new(
        ClientConfig::builder_with_provider(tls13_crypto_provider())
            .with_protocol_versions(&[&TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The WebSocket client configuration: the operating system's root
/// certificates, as tokio-tungstenite's default connector used, and TLS 1.3.
pub(crate) fn websocket_client_config() -> anyhow::Result<Arc<ClientConfig>> {
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::warn!(errors = ?native.errors, "some native root certificates could not be loaded");
    }
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(native.certs);
    if roots.is_empty() {
        bail!(
            "no native root certificates could be loaded: {:?}",
            native.errors
        );
    }
    tls13_client_config(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TlsServer;
    use rustls::{AlertDescription, pki_types::ServerName, version::TLS12};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    async fn handshake(config: Arc<ClientConfig>, server: &TlsServer) -> Result<(), rustls::Error> {
        let stream = TcpStream::connect(server.address).await.unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        match TlsConnector::from(config).connect(name, stream).await {
            Ok(_) => Ok(()),
            Err(error) => Err(*error
                .into_inner()
                .expect("handshake failures carry a rustls error")
                .downcast::<rustls::Error>()
                .expect("handshake failures carry a rustls error")),
        }
    }

    #[tokio::test]
    async fn clients_refuse_tls_12_servers() {
        let tls12 = TlsServer::start(&TLS12).await;
        let tls13 = TlsServer::start(&TLS13).await;

        // The server itself works with a client that still offers TLS 1.2.
        let permissive = Arc::new(
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(rustls::ALL_VERSIONS)
                .unwrap()
                .with_root_certificates(tls12.roots())
                .with_no_client_auth(),
        );
        handshake(permissive, &tls12).await.unwrap();

        let refused = Err(rustls::Error::AlertReceived(
            AlertDescription::ProtocolVersion,
        ));
        let websocket = tls13_client_config(tls12.roots()).unwrap();
        assert_eq!(handshake(websocket, &tls12).await, refused);
        handshake(tls13_client_config(tls13.roots()).unwrap(), &tls13)
            .await
            .unwrap();

        // ureq builds its configuration with every protocol version enabled
        // and relies on the provider to drop TLS 1.2.
        let ureq_like = |roots| {
            Arc::new(
                ClientConfig::builder_with_provider(tls13_crypto_provider())
                    .with_protocol_versions(rustls::ALL_VERSIONS)
                    .unwrap()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        };
        assert_eq!(handshake(ureq_like(tls12.roots()), &tls12).await, refused);
        handshake(ureq_like(tls13.roots()), &tls13).await.unwrap();
    }

    #[test]
    fn provider_keeps_only_tls_13_suites() {
        let provider = tls13_crypto_provider();
        assert!(!provider.cipher_suites.is_empty());
        assert!(
            provider
                .cipher_suites
                .iter()
                .all(|suite| suite.version() == &TLS13)
        );
        assert!(
            rustls::crypto::ring::default_provider()
                .cipher_suites
                .iter()
                .any(|suite| suite.version() == &TLS12)
        );
    }

    #[test]
    fn websocket_config_loads_the_system_roots() {
        websocket_client_config().unwrap();
    }
}
