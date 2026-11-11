//! The request behind every automatic update check, shared by the Windows
//! service and the Linux and macOS coordinators.
//!
//! The Agent asks its server, as itself, which release to install: the server
//! rolls new Agent releases out to some devices before others, so it answers
//! with the update manifest naming the release when it offers this device the
//! release (or the device has it already), and with a manifest naming none
//! otherwise. That is a successful check that found nothing, so the next one
//! comes on the usual schedule; a refused check, such as for a credential the
//! server no longer accepts, is an error and is retried on the failed-check
//! backoff (see [`crate::update_policy`]). The release's signature is checked
//! as before, so the server can withhold a release but not forge one.
use anyhow::Context;
use meshrmm_self_update::{CURRENT_VERSION, UpdateManifest};
use serde::{Deserialize, Serialize};

use crate::remote::config::Config;

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
struct UpdateCheck<'a> {
    version: &'a str,
}

/// Asks the server for this device's update manifest. `http` must treat an
/// error status as an error, as ureq does by default.
pub(crate) fn fetch_manifest(
    http: &ureq::Agent,
    config: &Config,
) -> anyhow::Result<UpdateManifest> {
    let url = meshrmm_signaling_client::endpoint_url(
        &config.server,
        &["v1", "agents", &config.device_id, "update-check"],
        &[],
        false,
    )?;
    let response = http
        .post(url.as_str())
        .header("Authorization", &format!("Bearer {}", credential(config)))
        .send_json(UpdateCheck {
            version: CURRENT_VERSION,
        })
        .context("the server did not answer the Agent update check")?;
    let manifest = response
        .into_body()
        .into_with_config()
        .limit(MAX_MANIFEST_BYTES)
        .read_to_vec()
        .context("the update manifest exceeded its size limit or could not be read")?;
    UpdateManifest::parse(&manifest)
}

/// The Agent's credential as its configuration file holds it now. The
/// coordinator writes a rotated credential there, and the server retires the
/// previous one once the new one is used, while the process that checks for
/// updates (on Windows, another process) loaded its configuration when it
/// started.
fn credential(config: &Config) -> String {
    #[derive(Deserialize)]
    struct Stored {
        agent_token: Option<String>,
    }
    std::fs::read_to_string(&config.config_path)
        .ok()
        .and_then(|contents| {
            serde_json::from_str::<Stored>(contents.trim_start_matches('\u{feff}')).ok()
        })
        .and_then(|stored| stored.agent_token)
        .unwrap_or_else(|| config.agent_token.clone())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use meshrmm_signaling_client::test_support::TlsServer;
    use rustls::version::TLS13;

    use super::*;

    const DEVICE_ID: &str = "3f2b8c4e-1d6a-4e7b-9c3d-5a8f2e1b7c90";
    const EMPTY_MANIFEST: &str = r#"{"schema_version":3,"releases":{}}"#;

    fn response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// An empty directory of its own for `test`.
    fn directory(test: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "meshrmm-update-check-{test}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn config(server: &TlsServer, directory: &Path) -> Config {
        Config {
            config_path: directory.join("agent.json"),
            server: format!("https://localhost:{}", server.address.port()),
            device_id: DEVICE_ID.to_owned(),
            agent_token: "a".repeat(64),
            frames_per_second: 60,
            bitrate_bits_per_second: 12_000_000,
            json_logs: false,
        }
    }

    /// An HTTP client that trusts `server`'s certificate, as the updaters'
    /// trust the operating system's.
    fn trusting(server: &TlsServer) -> ureq::Agent {
        let root = ureq::tls::Certificate::from_der(server.certificate.as_ref()).to_owned();
        ureq::Agent::config_builder()
            .https_only(true)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::Specific(Arc::new(vec![root])))
                    .unversioned_rustls_crypto_provider(
                        meshrmm_signaling_client::tls::tls13_crypto_provider(),
                    )
                    .build(),
            )
            .build()
            .new_agent()
    }

    fn start(response: &[u8]) -> (tokio::runtime::Runtime, TlsServer) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(TlsServer::answering(&TLS13, response));
        (runtime, server)
    }

    #[test]
    fn checks_as_the_device_and_takes_nothing_offered_as_nothing_to_install() {
        let (_runtime, server) = start(&response("200 OK", EMPTY_MANIFEST));
        let directory = directory("nothing-offered");
        let manifest = fetch_manifest(&trusting(&server), &config(&server, &directory)).unwrap();
        assert!(manifest.releases.is_empty());
        assert!(
            manifest
                .newer_release(meshrmm_self_update::AGENT_LINUX_X64, CURRENT_VERSION)
                .unwrap()
                .is_none()
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = String::from_utf8_lossy(&requests[0]);
        assert!(
            request.starts_with(&format!(
                "POST /v1/agents/{DEVICE_ID}/update-check HTTP/1.1\r\n"
            )),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("\r\nauthorization: bearer {}\r\n", "a".repeat(64))),
            "{request}"
        );
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({ "version": CURRENT_VERSION })
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn checks_with_the_credential_the_configuration_file_holds_now() {
        let (_runtime, server) = start(&response("200 OK", EMPTY_MANIFEST));
        let directory = directory("rotated");
        let config = config(&server, &directory);
        // The coordinator rotated it after this process loaded its configuration.
        let rotated = "b".repeat(64);
        std::fs::write(
            &config.config_path,
            format!("\u{feff}{{\"server\":\"x\",\"agent_token\":\"{rotated}\"}}"),
        )
        .unwrap();
        fetch_manifest(&trusting(&server), &config).unwrap();
        let request = String::from_utf8_lossy(&server.requests()[0]).to_ascii_lowercase();
        assert!(
            request.contains(&format!("\r\nauthorization: bearer {rotated}\r\n")),
            "{request}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_refused_check_fails() {
        let (_runtime, server) = start(&response("401 Unauthorized", "{}"));
        let directory = directory("refused");
        let error = fetch_manifest(&trusting(&server), &config(&server, &directory)).unwrap_err();
        assert!(format!("{error:?}").contains("401"), "{error:?}");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
