use std::path::PathBuf;

use semver::Version;
use serde::Deserialize;

#[derive(Deserialize)]
struct ReleaseConfig {
    version: String,
    download_origin: String,
}

fn main() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../release.json");
    println!("cargo:rerun-if-changed={}", config_path.display());

    let contents = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", config_path.display()));
    let config: ReleaseConfig = serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", config_path.display()));
    Version::parse(&config.version)
        .unwrap_or_else(|error| panic!("invalid release version {}: {error}", config.version));

    let origin = url::Url::parse(&config.download_origin).unwrap_or_else(|error| {
        panic!(
            "invalid release download origin {}: {error}",
            config.download_origin
        )
    });
    if origin.scheme() != "https" || origin.host_str().is_none() {
        panic!("release download origin must be an HTTPS URL");
    }

    println!("cargo:rustc-env=MESHRMM_RELEASE_VERSION={}", config.version);
    println!(
        "cargo:rustc-env=MESHRMM_UPDATE_MANIFEST_URL={}/downloads/update-manifest.json",
        config.download_origin.trim_end_matches('/')
    );
}
