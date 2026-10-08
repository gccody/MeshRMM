//! Enrollment of a new endpoint with the installer authorization the
//! dashboard issued, shared by the Windows and macOS installers.
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(crate) struct InstallerBootstrap {
    pub server: String,
    install_token: String,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct RedeemInstallerRequest {
    name: String,
    redemption_key: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProvisionedAgentConfig {
    pub server: String,
    pub device_id: String,
    pub agent_token: String,
    pub update_manifest_url: String,
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub json_logs: bool,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
}

/// The key that lets a retried installation recover its enrollment, kept
/// until enrollment finishes.
pub(crate) fn recovery_key(config_directory: &Path) -> anyhow::Result<String> {
    let path = config_directory.join("enrollment-recovery.json");
    match std::fs::read_to_string(&path) {
        Ok(key) => Ok(key),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            crate::installer::replace_file(&path, key.as_bytes())?;
            Ok(key)
        }
        Err(error) => Err(error).context("could not read enrollment recovery key"),
    }
}

/// Redeems the installer authorization, or reuses the configuration an
/// interrupted installation already redeemed.
pub(crate) fn redeem_once(
    config_directory: &Path,
    bootstrap: &InstallerBootstrap,
    machine_name: String,
    recovery_key: String,
) -> anyhow::Result<ProvisionedAgentConfig> {
    let pending = config_directory.join("enrollment-pending.json");
    if pending.exists() {
        return Ok(serde_json::from_slice(&std::fs::read(&pending)?)?);
    }
    let config = redeem_installer(bootstrap, machine_name, recovery_key)?;
    crate::installer::replace_file(&pending, &serde_json::to_vec(&config)?)?;
    Ok(config)
}

/// Removes the enrollment's recovery state once the Agent is installed.
pub(crate) fn finish(config_directory: &Path) {
    let _ = std::fs::remove_file(config_directory.join("enrollment-pending.json"));
    let _ = std::fs::remove_file(config_directory.join("enrollment-recovery.json"));
}

/// Verifies certificates with the operating system and offers only TLS 1.3,
/// which every MeshRMM host requires. Used for updates and enrollment.
pub(crate) fn https_tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(meshrmm_signaling_client::tls::tls13_crypto_provider())
        .build()
}

pub(crate) fn validate_bootstrap(config: &[u8]) -> anyhow::Result<InstallerBootstrap> {
    let bootstrap: InstallerBootstrap = serde_json::from_slice(config)
        .context("the embedded Agent installer authorization is invalid JSON")?;
    let server = url::Url::parse(&bootstrap.server)
        .context("the embedded Agent installer server URL is invalid")?;
    if server.scheme() != "https" || server.host_str().is_none() {
        bail!("the Agent installer requires an HTTPS server URL");
    }
    if bootstrap.install_token.len() < 32
        || !bootstrap
            .install_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("the embedded Agent installer authorization is invalid");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the system clock is before the Unix epoch")?
        .as_millis() as u64;
    if bootstrap.expires_at_unix_ms <= now {
        bail!("this Agent installer authorization has expired; download a new installer");
    }
    Ok(bootstrap)
}

fn redeem_installer(
    bootstrap: &InstallerBootstrap,
    machine_name: String,
    redemption_key: String,
) -> anyhow::Result<ProvisionedAgentConfig> {
    let endpoint = format!(
        "{}/v1/agent-installers/redeem",
        bootstrap.server.trim_end_matches('/')
    );
    let http = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .tls_config(https_tls_config())
        .build()
        .new_agent();
    let mut response = http
        .post(&endpoint)
        .header(
            "Authorization",
            &format!("Bearer {}", bootstrap.install_token),
        )
        .send_json(&RedeemInstallerRequest {
            name: machine_name,
            redemption_key,
        })
        .context("failed to contact the MeshRMM Agent enrollment service")?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response
            .body_mut()
            .read_json::<ApiError>()
            .map(|body| body.error)
            .unwrap_or_else(|_| "the Agent enrollment service rejected the installer".to_owned());
        bail!("Agent enrollment failed with HTTP {status}: {detail}");
    }
    let config = response
        .body_mut()
        .read_json::<ProvisionedAgentConfig>()
        .context("the Agent enrollment service returned an invalid configuration")?;
    if config.device_id.is_empty() || config.agent_token.is_empty() || config.server.is_empty() {
        bail!("the Agent enrollment service returned an incomplete configuration");
    }
    meshrmm_self_update::validate_manifest_url(&config.update_manifest_url)
        .context("the Agent enrollment service returned an invalid update manifest URL")?;
    Ok(config)
}
