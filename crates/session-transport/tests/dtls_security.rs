//! Handshake regressions against the DTLS implementation used by both endpoints.
use std::{sync::Arc, time::Duration};
use webrtc::dtls::{
    cipher_suite::CipherSuiteId,
    config::{ClientAuthType, Config},
    conn::DTLSConn,
    crypto::Certificate,
};

const GCM: CipherSuiteId = CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256;
const CHACHA: CipherSuiteId = CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256;
const CBC: CipherSuiteId = CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha;

type Handshake = Result<DTLSConn, webrtc::dtls::Error>;

async fn negotiate(
    client_suites: Vec<CipherSuiteId>,
    server_suites: Vec<CipherSuiteId>,
) -> (Handshake, Handshake) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_cert = Certificate::generate_self_signed(vec!["localhost".into()]).unwrap();
    let server_cert = Certificate::generate_self_signed(vec!["localhost".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(server_cert.certificate[0].clone()).unwrap();
    let mut client_roots = rustls::RootCertStore::empty();
    client_roots
        .add(client_cert.certificate[0].clone())
        .unwrap();
    let client = Config {
        certificates: vec![client_cert],
        cipher_suites: client_suites,
        roots_cas: roots,
        server_name: "localhost".into(),
        ..Default::default()
    };
    let server = Config {
        certificates: vec![server_cert],
        cipher_suites: server_suites,
        client_cas: client_roots,
        client_auth: ClientAuthType::RequireAndVerifyClientCert,
        ..Default::default()
    };
    let (a, b) = webrtc::util::conn::conn_pipe::pipe();
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            DTLSConn::new(Arc::new(a), client, true, None),
            DTLSConn::new(Arc::new(b), server, false, None)
        )
    })
    .await
    .expect("DTLS negotiation must finish or explicitly reject the peer")
}

async fn connect(
    client_suites: Vec<CipherSuiteId>,
    server_suites: Vec<CipherSuiteId>,
) -> (DTLSConn, DTLSConn) {
    let (client, server) = negotiate(client_suites, server_suites).await;
    (client.unwrap(), server.unwrap())
}

async fn exchange(client: &DTLSConn, server: &DTLSConn, expected: CipherSuiteId) {
    for peer in [client, server] {
        assert_eq!(
            peer.connection_state()
                .await
                .negotiated_cipher_suite()
                .await,
            Some(expected)
        );
    }
    for (sender, receiver) in [(client, server), (server, client)] {
        let data = b"remote screen, input, audio, clipboard, file and chat payload";
        sender
            .write(data, Some(Duration::from_secs(2)))
            .await
            .unwrap();
        let mut received = [0; 128];
        let count = receiver
            .read(&mut received, Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(&received[..count], data);
    }
    client.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn secure_defaults_negotiate_gcm_and_exchange_data() {
    let (client, server) = connect(vec![], vec![]).await;
    exchange(&client, &server, GCM).await;
}

#[tokio::test]
async fn legacy_peer_with_cbc_preference_still_uses_aead_in_both_roles() {
    // Emulate an older peer that offers CBC as well as the existing GCM suite.
    for (client_suites, server_suites) in [(vec![CBC, GCM], vec![]), (vec![], vec![CBC, GCM])] {
        let (client, server) = connect(client_suites, server_suites).await;
        exchange(&client, &server, GCM).await;
    }
}

#[tokio::test]
async fn chacha_only_peer_remains_compatible_in_both_roles() {
    for (client_suites, server_suites) in [(vec![CHACHA], vec![]), (vec![], vec![CHACHA])] {
        let (client, server) = connect(client_suites, server_suites).await;
        exchange(&client, &server, CHACHA).await;
    }
}

#[tokio::test]
async fn cbc_only_peer_is_rejected_in_both_roles() {
    for (client_suites, server_suites) in [(vec![CBC], vec![]), (vec![], vec![CBC])] {
        let (client, server) = negotiate(client_suites, server_suites).await;
        assert!(
            client.is_err() && server.is_err(),
            "CBC-only peers must not connect"
        );
        assert!(
            matches!(
                client,
                Err(webrtc::dtls::Error::ErrCipherSuiteNoIntersection)
            ) || matches!(
                server,
                Err(webrtc::dtls::Error::ErrCipherSuiteNoIntersection)
            ),
            "one peer must reject the handshake for lack of a shared cipher"
        );
    }
}
