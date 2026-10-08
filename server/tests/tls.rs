//! Serving HTTPS from certificate files, and picking up replaced files.
mod common;

use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use axum_server::{Handle, tls_rustls::RustlsConfig};

fn write_certificate(dir: &Path) -> String {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let pem = certified.cert.pem();
    std::fs::write(dir.join("cert.pem"), &pem).unwrap();
    std::fs::write(dir.join("key.pem"), certified.signing_key.serialize_pem()).unwrap();
    pem
}

#[tokio::test]
async fn files_mode_serves_https_with_the_configured_certificate() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let certificate = write_certificate(dir.path());
    let config = common::config(
        dir.path(),
        &format!(
            r#"
            http.listen = "127.0.0.1:0"
            tls = {{ mode = "files", cert_path = "{cert}", key_path = "{key}" }}
            "#,
            cert = dir.path().join("cert.pem").display(),
            key = dir.path().join("key.pem").display(),
        ),
    );
    let state = meshrmm_server::prepare(config).await.unwrap();
    let handle = Handle::<SocketAddr>::new();
    let server = tokio::spawn({
        let handle = handle.clone();
        let state = state.clone();
        async move {
            meshrmm_server::serve::serve(
                &state.config,
                meshrmm_server::http::router(state.clone()),
                handle,
            )
            .await
        }
    });
    let address = handle
        .listening()
        .await
        .expect("the server started listening");

    let client = reqwest::Client::builder()
        .tls_certs_only([reqwest::Certificate::from_pem(certificate.as_bytes()).unwrap()])
        .resolve("localhost", address)
        .build()
        .unwrap();
    let response = client
        .get(format!("https://localhost:{}/healthz", address.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()["strict-transport-security"],
        "max-age=31536000"
    );

    handle.graceful_shutdown(Some(Duration::from_secs(1)));
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn replaced_certificate_files_are_reloaded() {
    meshrmm_server::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    write_certificate(dir.path());
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    let tls = RustlsConfig::from_pem_file(&cert, &key).await.unwrap();
    let original = tls.get_inner();
    let reloader = tokio::spawn(meshrmm_server::serve::reload_on_change(
        tls.clone(),
        cert.clone(),
        key.clone(),
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
}
