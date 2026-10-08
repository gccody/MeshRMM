use std::collections::BTreeMap;

#[cfg(windows)]
pub mod windows;

use anyhow::{Context, bail};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA_VERSION: u32 = 2;
pub const CURRENT_VERSION: &str = env!("MESHRMM_RELEASE_VERSION");
/// The Ed25519 key, in hexadecimal, that every release this build updates
/// to must be signed with. Servers only pass the signatures on, so a server
/// can offer an update but cannot forge one.
pub const RELEASE_PUBLIC_KEY: &str = env!("MESHRMM_RELEASE_PUBLIC_KEY");
pub const AGENT_WINDOWS_X64: &str = "agent-windows-x64";
/// The universal (Apple silicon and Intel) macOS Agent.
pub const AGENT_MACOS: &str = "agent-macos";
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
    /// The release key's Ed25519 signature of [`signed_message`], in
    /// hexadecimal. A server whose downloads are unsigned leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
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

    /// The release of `target` newer than `current_version`, if the manifest
    /// offers one. An offered release that isn't signed with the release key
    /// is an error.
    pub fn newer_release(
        &self,
        target: &str,
        current_version: &str,
    ) -> anyhow::Result<Option<Release>> {
        self.newer_release_with_key(target, current_version, &release_key())
    }

    fn newer_release_with_key(
        &self,
        target: &str,
        current_version: &str,
        key: &VerifyingKey,
    ) -> anyhow::Result<Option<Release>> {
        let Some(release) = self.releases.get(target) else {
            return Ok(None);
        };
        release.validate_with_key(target, key)?;
        let current =
            Version::parse(current_version).context("invalid current application version")?;
        let offered = Version::parse(&release.version).context("invalid release version")?;
        Ok((offered > current).then(|| release.clone()))
    }
}

impl Release {
    /// Checks that this is a well-formed release of `target`, signed with
    /// [`RELEASE_PUBLIC_KEY`].
    pub fn validate(&self, target: &str) -> anyhow::Result<()> {
        self.validate_with_key(target, &release_key())
    }

    fn validate_with_key(&self, target: &str, key: &VerifyingKey) -> anyhow::Result<()> {
        Version::parse(&self.version).context("invalid release version")?;
        let url = url::Url::parse(&self.url).context("invalid release URL")?;
        if url.scheme() != "https" || url.host_str().is_none() {
            bail!("release URL must use HTTPS");
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("release SHA-256 is invalid");
        }
        let signature = self
            .signature
            .as_deref()
            .context("release is not signed with the MeshRMM release key")?;
        verify_signature(key, target, &self.version, &self.sha256, signature)
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

/// What a release's signature covers. The download URL is left out because
/// each server rewrites it to point at itself.
pub fn signed_message(target: &str, version: &str, sha256: &str) -> String {
    format!(
        "meshrmm-release-v1\n{target}\n{version}\n{}\n",
        sha256.to_ascii_lowercase()
    )
}

/// Checks a release signature against [`RELEASE_PUBLIC_KEY`].
pub fn verify_release_signature(
    target: &str,
    version: &str,
    sha256: &str,
    signature: &str,
) -> anyhow::Result<()> {
    verify_signature(&release_key(), target, version, sha256, signature)
}

fn verify_signature(
    key: &VerifyingKey,
    target: &str,
    version: &str,
    sha256: &str,
    signature: &str,
) -> anyhow::Result<()> {
    let signature: [u8; 64] = decode_hex(signature)
        .and_then(|bytes| bytes.try_into().ok())
        .context("release signature is malformed")?;
    key.verify_strict(
        signed_message(target, version, sha256).as_bytes(),
        &Signature::from_bytes(&signature),
    )
    .context("release signature does not match the MeshRMM release key")
}

fn release_key() -> VerifyingKey {
    parse_key(RELEASE_PUBLIC_KEY).expect("the build script checked the release key")
}

fn parse_key(hex: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = decode_hex(hex)?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(value.get(index..index + 2)?, 16).ok())
        .collect()
}

/// Where the server at `server` publishes its update manifest.
pub fn manifest_url(server: &str) -> String {
    format!(
        "{}/downloads/update-manifest.json",
        server.trim_end_matches('/')
    )
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

    // A key made from 32 bytes of 7s, and its signature of version 1.2.0 of
    // AGENT_WINDOWS_X64 with the SHA-256 of b"agent", both made with Node's
    // crypto module as scripts/release-artifacts.mjs signs.
    const TEST_KEY: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const TEST_SIGNATURE: &str = "dfa675701363af7e76af23799742517bed7c0ad6f8f7f572c712b0e8e7d2e3cf16d9c9ff8ec73879fb2da3a2c08403a8f4e987fc21b07f8a148c2a56dd61b80f";

    fn test_key() -> VerifyingKey {
        parse_key(TEST_KEY).unwrap()
    }

    fn release(version: &str) -> Release {
        Release {
            version: version.to_owned(),
            url: "https://downloads.example.com/agent.exe".to_owned(),
            sha256: format!("{:x}", Sha256::digest(b"agent")),
            signature: Some(TEST_SIGNATURE.to_owned()),
        }
    }

    fn manifest(release: Release) -> UpdateManifest {
        UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            releases: [(AGENT_WINDOWS_X64.to_owned(), release)]
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn embeds_valid_release_configuration() {
        Version::parse(CURRENT_VERSION).unwrap();
        parse_key(RELEASE_PUBLIC_KEY).unwrap();
    }

    #[test]
    fn accepts_a_release_signed_with_the_release_key() {
        release("1.2.0")
            .validate_with_key(AGENT_WINDOWS_X64, &test_key())
            .unwrap();
    }

    #[test]
    fn rejects_unsigned_relabeled_or_retargeted_releases() {
        let key = test_key();
        let unsigned = Release {
            signature: None,
            ..release("1.2.0")
        };
        assert!(unsigned.validate_with_key(AGENT_WINDOWS_X64, &key).is_err());
        // The signature covers the version, so an old build can't be offered
        // as a new one.
        assert!(
            release("9.9.9")
                .validate_with_key(AGENT_WINDOWS_X64, &key)
                .is_err()
        );
        assert!(
            release("1.2.0")
                .validate_with_key(CLIENT_WINDOWS_X64, &key)
                .is_err()
        );
        let tampered = Release {
            sha256: format!("{:x}", Sha256::digest(b"tampered")),
            ..release("1.2.0")
        };
        assert!(tampered.validate_with_key(AGENT_WINDOWS_X64, &key).is_err());
        let malformed = Release {
            signature: Some("zz".repeat(64)),
            ..release("1.2.0")
        };
        assert!(
            malformed
                .validate_with_key(AGENT_WINDOWS_X64, &key)
                .is_err()
        );
        // Nor does it verify with any other key, such as this build's.
        assert!(release("1.2.0").validate(AGENT_WINDOWS_X64).is_err());
    }

    #[test]
    fn selects_only_newer_semantic_versions() {
        let key = test_key();
        let offered = |current: &str| {
            manifest(release("1.2.0"))
                .newer_release_with_key(AGENT_WINDOWS_X64, current, &key)
                .unwrap()
                .is_some()
        };
        assert!(offered("1.1.9"));
        assert!(!offered("1.2.0"));
        assert!(!offered("2.0.0"));
        assert!(
            manifest(release("1.2.0"))
                .newer_release_with_key(CLIENT_WINDOWS_X64, "1.0.0", &key)
                .unwrap()
                .is_none()
        );
        // An unsigned offer is refused rather than skipped.
        let unsigned = Release {
            signature: None,
            ..release("1.2.0")
        };
        assert!(
            manifest(unsigned)
                .newer_release_with_key(AGENT_WINDOWS_X64, "1.0.0", &key)
                .is_err()
        );
    }

    #[test]
    fn verifies_release_bytes() {
        let release = release("1.2.0");
        release.verify(b"agent").unwrap();
        assert!(release.verify(b"tampered").is_err());
    }

    #[test]
    fn rejects_insecure_release_urls_and_unknown_schemas() {
        let key = test_key();
        let insecure = Release {
            url: "http://downloads.example.com/agent.exe".to_owned(),
            ..release("1.2.0")
        };
        assert!(insecure.validate_with_key(AGENT_WINDOWS_X64, &key).is_err());

        let mut value = manifest(release("1.2.0"));
        value.schema_version = 1;
        assert!(UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn derives_a_server_manifest_url() {
        assert_eq!(
            manifest_url("https://rmm.example.com/"),
            "https://rmm.example.com/downloads/update-manifest.json"
        );
        validate_manifest_url(&manifest_url("https://rmm.example.com")).unwrap();
    }
}
