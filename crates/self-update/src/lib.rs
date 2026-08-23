use std::collections::BTreeMap;

use anyhow::{Context, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const CURRENT_VERSION: &str = env!("MESHRMM_RELEASE_VERSION");
pub const DEFAULT_MANIFEST_URL: &str = env!("MESHRMM_UPDATE_MANIFEST_URL");
pub const AGENT_WINDOWS_X64: &str = "agent-windows-x64";
pub const CLIENT_WINDOWS_X64: &str = "client-windows-x64";
pub const CLIENT_MACOS_X64: &str = "client-macos-x64";
pub const CLIENT_MACOS_ARM64: &str = "client-macos-arm64";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub schema_version: u32,
    pub releases: BTreeMap<String, Release>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

impl UpdateManifest {
    pub fn parse(contents: &[u8]) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_slice(contents).context("invalid update manifest")?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            bail!(
                "unsupported update manifest schema {}",
                manifest.schema_version
            );
        }
        Ok(manifest)
    }

    pub fn newer_release(
        &self,
        target: &str,
        current_version: &str,
    ) -> anyhow::Result<Option<Release>> {
        let Some(release) = self.releases.get(target) else {
            return Ok(None);
        };
        release.validate()?;
        let current =
            Version::parse(current_version).context("invalid current application version")?;
        let offered = Version::parse(&release.version).context("invalid release version")?;
        Ok((offered > current).then(|| release.clone()))
    }
}

impl Release {
    pub fn validate(&self) -> anyhow::Result<()> {
        Version::parse(&self.version).context("invalid release version")?;
        let url = url::Url::parse(&self.url).context("invalid release URL")?;
        if url.scheme() != "https" || url.host_str().is_none() {
            bail!("release URL must use HTTPS");
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("release SHA-256 is invalid");
        }
        Ok(())
    }

    pub fn verify(&self, contents: &[u8]) -> anyhow::Result<()> {
        let actual = format!("{:x}", Sha256::digest(contents));
        if actual.eq_ignore_ascii_case(&self.sha256) {
            Ok(())
        } else {
            bail!("downloaded update failed SHA-256 verification")
        }
    }
}

pub fn validate_manifest_url(value: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(value).context("update manifest URL is invalid")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        bail!("update manifest URL must use HTTPS");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, sha256: &str) -> UpdateManifest {
        UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            releases: [(
                AGENT_WINDOWS_X64.to_owned(),
                Release {
                    version: version.to_owned(),
                    url: "https://downloads.example.com/agent.exe".to_owned(),
                    sha256: sha256.to_owned(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn embeds_valid_release_configuration() {
        Version::parse(CURRENT_VERSION).unwrap();
        validate_manifest_url(DEFAULT_MANIFEST_URL).unwrap();
    }

    #[test]
    fn selects_only_newer_semantic_versions() {
        let checksum = format!("{:x}", Sha256::digest(b"agent"));
        assert!(
            manifest("1.2.0", &checksum)
                .newer_release(AGENT_WINDOWS_X64, "1.1.9")
                .unwrap()
                .is_some()
        );
        assert!(
            manifest("1.2.0", &checksum)
                .newer_release(AGENT_WINDOWS_X64, "1.2.0")
                .unwrap()
                .is_none()
        );
        assert!(
            manifest("1.2.0", &checksum)
                .newer_release(AGENT_WINDOWS_X64, "2.0.0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn verifies_release_bytes() {
        let checksum = format!("{:x}", Sha256::digest(b"agent"));
        let release = manifest("1.2.0", &checksum)
            .newer_release(AGENT_WINDOWS_X64, "1.0.0")
            .unwrap()
            .unwrap();
        release.verify(b"agent").unwrap();
        assert!(release.verify(b"tampered").is_err());
    }

    #[test]
    fn rejects_insecure_release_urls_and_unknown_schemas() {
        let checksum = format!("{:x}", Sha256::digest(b"agent"));
        let mut value = manifest("1.2.0", &checksum);
        value.releases.get_mut(AGENT_WINDOWS_X64).unwrap().url =
            "http://downloads.example.com/agent.exe".to_owned();
        assert!(value.newer_release(AGENT_WINDOWS_X64, "1.0.0").is_err());

        value.schema_version = 2;
        assert!(UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
