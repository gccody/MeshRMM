use std::path::PathBuf;

use semver::Version;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseConfig {
    version: String,
    signing_public_key: String,
}

/// Overrides `signing_public_key` for builds signed with a development key;
/// see docs/releases.md.
const PUBLIC_KEY_OVERRIDE: &str = "MESHRMM_RELEASE_PUBLIC_KEY";

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../release.json");
    println!("cargo:rerun-if-changed={}", config_path.display());
    println!("cargo:rerun-if-env-changed={PUBLIC_KEY_OVERRIDE}");

    let contents = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", config_path.display()));
    let config: ReleaseConfig = serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", config_path.display()));
    Version::parse(&config.version)
        .unwrap_or_else(|error| panic!("invalid release version {}: {error}", config.version));

    let public_key = std::env::var(PUBLIC_KEY_OVERRIDE)
        .ok()
        .filter(|key| !key.is_empty())
        .unwrap_or(config.signing_public_key)
        .to_ascii_lowercase();
    if public_key.len() != 64 || !public_key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        panic!("the release signing public key must be 32 bytes in hexadecimal");
    }

    println!("cargo:rustc-env=MESHRMM_RELEASE_VERSION={}", config.version);
    println!("cargo:rustc-env=MESHRMM_RELEASE_PUBLIC_KEY={public_key}");
}
