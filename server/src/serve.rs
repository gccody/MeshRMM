//! Accepting connections in each TLS mode: an ACME certificate, certificate
//! files, or plain HTTP behind a reverse proxy.
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use axum::Router;
use axum_server::{Handle, tls_rustls::RustlsConfig};
use futures_util::StreamExt;
use rustls_acme::{AcmeConfig, caches::DirCache};

use crate::config::{Config, TlsConfig};

/// How often certificate files are checked for replacement.
const CERTIFICATE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Serves `app` until `handle` shuts the server down.
pub async fn serve(config: &Config, app: Router, handle: Handle<SocketAddr>) -> anyhow::Result<()> {
    let address = config.listen_addr();
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    match &config.tls {
        TlsConfig::Proxy { .. } => {
            tracing::info!(%address, "serving HTTP for a reverse proxy");
            axum_server::bind(address).handle(handle).serve(service).await
        }
        TlsConfig::Files { cert_path, key_path } => {
            let tls = RustlsConfig::from_pem_file(cert_path, key_path)
                .await
                .with_context(|| {
                    format!(
                        "could not load the certificate {} and key {}",
                        cert_path.display(),
                        key_path.display()
                    )
                })?;
            let reloader = tokio::spawn(reload_on_change(
                tls.clone(),
                cert_path.clone(),
                key_path.clone(),
                CERTIFICATE_POLL_INTERVAL,
            ));
            tracing::info!(%address, cert = %cert_path.display(), "serving HTTPS with a certificate file");
            let served = axum_server::bind_rustls(address, tls).handle(handle).serve(service).await;
            reloader.abort();
            served
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
            let acceptor = acme.axum_acceptor(Arc::new(tls));
            let events = tokio::spawn(async move {
                while let Some(event) = acme.next().await {
                    match event {
                        Ok(event) => tracing::info!(?event, "ACME"),
                        Err(error) => tracing::warn!(%error, "ACME certificate request failed; retrying"),
                    }
                }
            });
            tracing::info!(%address, ?domains, directory = %directory_url, "serving HTTPS with an ACME certificate");
            let served = axum_server::bind(address)
                .acceptor(acceptor)
                .handle(handle)
                .serve(service)
                .await;
            events.abort();
            served
        }
    }
    .with_context(|| format!("could not serve on {address}"))
}

/// Reloads the certificate whenever either file's modification time changes.
/// A failed reload keeps the certificate already in use.
pub async fn reload_on_change(
    tls: RustlsConfig,
    cert_path: PathBuf,
    key_path: PathBuf,
    interval: Duration,
) {
    let mut last = modified(&cert_path, &key_path);
    loop {
        tokio::time::sleep(interval).await;
        let current = modified(&cert_path, &key_path);
        if current == last {
            continue;
        }
        match tls.reload_from_pem_file(&cert_path, &key_path).await {
            Ok(()) => {
                tracing::info!(cert = %cert_path.display(), "reloaded the TLS certificate");
                last = current;
            }
            // A renewal tool may be midway through writing the pair; the next
            // poll sees the finished files.
            Err(error) => {
                tracing::warn!(%error, "could not reload the TLS certificate; keeping the current one")
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
