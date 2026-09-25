//! HTTPS clients for the MeshRMM API and update downloads.

/// A client builder that offers only TLS 1.3, which every MeshRMM host
/// requires. See `meshrmm_signaling_client::tls`.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_version_min(reqwest::tls::Version::TLS_1_3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_signaling_client::test_support::TlsServer;
    use rustls::version::{TLS12, TLS13};

    async fn get(
        client: &reqwest::Client,
        server: &TlsServer,
    ) -> reqwest::Result<reqwest::Response> {
        client
            .get(format!("https://localhost:{}/", server.address.port()))
            .send()
            .await
    }

    fn trusting(server: &TlsServer, builder: reqwest::ClientBuilder) -> reqwest::Client {
        builder
            .resolve("localhost", server.address)
            .tls_certs_only([reqwest::Certificate::from_der(&server.certificate).unwrap()])
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn clients_refuse_tls_12_servers() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls12 = TlsServer::start(&TLS12).await;
        let tls13 = TlsServer::start(&TLS13).await;

        // The server itself works with a client that still offers TLS 1.2.
        let permissive = trusting(&tls12, reqwest::Client::builder());
        assert_eq!(get(&permissive, &tls12).await.unwrap().status(), 204);

        let error = get(&trusting(&tls12, client_builder()), &tls12)
            .await
            .unwrap_err();
        assert!(
            format!("{error:?}").contains("ProtocolVersion"),
            "{error:?}"
        );
        let response = get(&trusting(&tls13, client_builder()), &tls13)
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
    }
}
