//! The Agent and viewer builds shipped with this server release: the files in
//! the downloads directory that its `artifacts.json` lists, the update
//! manifest made from it, and, when the company configures it, the macOS
//! builds signed with its own Developer ID.
//!
//! Releases sign the macOS builds only ad hoc, so nobody can take a
//! MeshRMM build vouched for by someone else's Developer ID. Each company's
//! server signs them with its own (see [`developer_id`]).
//!
//! The server serves each build under three names:
//!
//! - `/downloads/release/<file>`: the build exactly as the release shipped
//!   it, which the release key's signature covers;
//! - `/downloads/developer-id/<file>`: a macOS build signed with the
//!   company's Developer ID;
//! - `/downloads/<file>`: the build to install, which for macOS is the
//!   Developer ID build when signing is configured.
//!
//! The manifest points every build at this server, so installed Agents and
//! viewers update from the server they use.
pub mod developer_id;

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, bail};
use axum::{
    body::Body,
    extract::{Path as UrlPath, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use meshrmm_self_update::{
    AGENT_MACOS, Build, CLIENT_MACOS_ARM64, CLIENT_MACOS_X64, MANIFEST_SCHEMA_VERSION, Release,
    UpdateManifest,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    config::Config,
    http::{ApiError, AppState},
};
pub use developer_id::SigningStatus;

pub const ARTIFACTS_FILE: &str = "artifacts.json";
pub const MANIFEST_FILE: &str = "update-manifest.json";
const RELEASE_PREFIX: &str = "release/";
const DEVELOPER_ID_PREFIX: &str = "developer-id/";
const ARTIFACTS_SCHEMA_VERSION: u32 = 1;
/// Upgrading the server replaces the builds under the same names.
const REVALIDATE: &str = "no-cache";
const MACOS_TARGETS: &[&str] = &[AGENT_MACOS, CLIENT_MACOS_ARM64, CLIENT_MACOS_X64];

/// `artifacts.json`, as `scripts/release-artifacts.mjs` writes it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactsFile {
    schema_version: u32,
    version: String,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    file: String,
    sha256: String,
    signature: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Downloads {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    dir: PathBuf,
    public_origin: String,
    /// The release's version and builds, by target.
    release: Option<(String, BTreeMap<String, Artifact>)>,
    /// Targets whose builds installed Agents and viewers won't accept.
    unsigned: Vec<String>,
    /// Present when the company signs its macOS builds.
    developer_id: Option<developer_id::Signer>,
}

/// Where a request for a download leads.
enum Found {
    File(PathBuf),
    /// The build exists but can't be served yet, or at all.
    Unavailable(String),
}

impl Downloads {
    /// Reads the downloads directory's `artifacts.json` and checks every
    /// build it lists against its SHA-256. A missing `artifacts.json` means
    /// there are no downloads, as in development; anything else wrong with it
    /// is an error. With macOS signing configured, it also picks up the
    /// builds already signed for this release; [`Downloads::start_signing`]
    /// signs the rest.
    pub async fn load(config: &Config) -> anyhow::Result<Self> {
        let dir = config.downloads.dir.clone();
        let public_origin = config.public_origin();
        let signing = config.downloads.macos_signing.clone();
        let developer_id_dir = config.data_dir.join(developer_id::DIR);
        tokio::task::spawn_blocking(move || {
            let mut inner = Inner::load(dir, public_origin)?;
            if let Some(signing) = signing {
                let builds = inner
                    .release
                    .as_ref()
                    .map_or_else(BTreeMap::new, |(_, builds)| {
                        builds
                            .iter()
                            .filter(|(target, _)| MACOS_TARGETS.contains(&target.as_str()))
                            .map(|(target, artifact)| {
                                (
                                    target.clone(),
                                    developer_id::Source {
                                        path: inner.dir.join(&artifact.file),
                                        file: artifact.file.clone(),
                                        sha256: artifact.sha256.clone(),
                                    },
                                )
                            })
                            .collect()
                    });
                inner.developer_id = Some(developer_id::Signer::load(
                    signing,
                    developer_id_dir,
                    builds,
                )?);
            }
            Ok(Self {
                inner: Arc::new(inner),
            })
        })
        .await?
    }

    /// The release the builds belong to, or `None` without downloads.
    pub fn version(&self) -> Option<&str> {
        self.inner
            .release
            .as_ref()
            .map(|(version, _)| version.as_str())
    }

    pub fn file_count(&self) -> usize {
        self.inner
            .release
            .as_ref()
            .map_or(0, |(_, builds)| builds.len())
    }

    /// Targets whose builds aren't signed with the release key this server
    /// was built with. Agents and viewers built with it won't update to them.
    pub fn unsigned(&self) -> &[String] {
        &self.inner.unsigned
    }

    /// Where signing the macOS builds with the company's Developer ID stands,
    /// or `None` when it isn't configured.
    pub fn macos_signing(&self) -> Option<SigningStatus> {
        self.inner
            .developer_id
            .as_ref()
            .map(developer_id::Signer::status)
    }

    /// Signs the macOS builds with the company's Developer ID in the
    /// background, unless they're signed already or signing isn't
    /// configured. Until it finishes, their installers answer 503.
    pub fn start_signing(&self) {
        if self
            .inner
            .developer_id
            .as_ref()
            .is_some_and(|signer| matches!(signer.status(), SigningStatus::Signing))
        {
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || {
                if let Some(signer) = &inner.developer_id {
                    signer.sign();
                }
            });
        }
    }

    /// Logs what's wrong with the downloads an operator should know about.
    pub fn warn_about_problems(&self) {
        let Some(version) = self.version() else {
            tracing::warn!(
                dir = %self.inner.dir.display(),
                "no {ARTIFACTS_FILE} in the downloads directory; Agent and viewer downloads and updates are unavailable"
            );
            return;
        };
        if version != meshrmm_self_update::CURRENT_VERSION {
            tracing::warn!(
                downloads = version,
                server = meshrmm_self_update::CURRENT_VERSION,
                "the downloads belong to a different release than this server"
            );
        }
        if !self.unsigned().is_empty() {
            tracing::warn!(
                targets = self.unsigned().join(", "),
                "these downloads aren't signed with the release key; installed Agents and viewers won't update to them"
            );
        }
        let has_macos = self.inner.release.as_ref().is_some_and(|(_, builds)| {
            builds
                .keys()
                .any(|target| MACOS_TARGETS.contains(&target.as_str()))
        });
        if has_macos && self.inner.developer_id.is_none() {
            tracing::info!(
                "the macOS Agent and viewer are signed only ad hoc; set downloads.macos_signing to sign them with your Developer ID"
            );
        }
    }

    /// The update manifest, or `None` without downloads.
    fn manifest(&self) -> Option<Vec<u8>> {
        let inner = &self.inner;
        let (version, builds) = inner.release.as_ref()?;
        let signed = inner
            .developer_id
            .as_ref()
            .and_then(developer_id::Signer::builds);
        let releases = builds
            .iter()
            .map(|(target, artifact)| {
                let developer_id =
                    signed
                        .as_ref()
                        .and_then(|signed| signed.get(target))
                        .map(|build| Build {
                            url: format!(
                                "{}/downloads/{DEVELOPER_ID_PREFIX}{}",
                                inner.public_origin, build.file
                            ),
                            sha256: build.sha256.clone(),
                        });
                let release = Release {
                    version: version.clone(),
                    url: format!(
                        "{}/downloads/{RELEASE_PREFIX}{}",
                        inner.public_origin, artifact.file
                    ),
                    sha256: artifact.sha256.clone(),
                    signature: artifact.signature.clone(),
                    developer_id,
                };
                (target.clone(), release)
            })
            .collect();
        serde_json::to_vec_pretty(&UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            releases,
        })
        .ok()
    }

    /// The file `path` (below `/downloads/`) names, if any.
    fn find(&self, path: &str) -> Option<Found> {
        let inner = &self.inner;
        let (_, builds) = inner.release.as_ref()?;
        let listed = |file: &str| builds.iter().find(|(_, artifact)| artifact.file == file);
        if let Some(file) = path.strip_prefix(RELEASE_PREFIX) {
            return listed(file).map(|_| Found::File(inner.dir.join(file)));
        }
        if let Some(file) = path.strip_prefix(DEVELOPER_ID_PREFIX) {
            return inner
                .developer_id
                .as_ref()?
                .signed_file(file)
                .map(Found::File);
        }
        let (target, artifact) = listed(path)?;
        match &inner.developer_id {
            Some(signer) if MACOS_TARGETS.contains(&target.as_str()) => {
                Some(match signer.signed_build(target) {
                    Ok(path) => Found::File(path),
                    Err(message) => Found::Unavailable(message),
                })
            }
            _ => Some(Found::File(inner.dir.join(&artifact.file))),
        }
    }
}

impl Inner {
    fn load(dir: PathBuf, public_origin: String) -> anyhow::Result<Self> {
        let path = dir.join(ARTIFACTS_FILE);
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    dir,
                    public_origin,
                    ..Self::default()
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        let listed: ArtifactsFile = serde_json::from_slice(&contents)
            .with_context(|| format!("{} is invalid", path.display()))?;
        if listed.schema_version != ARTIFACTS_SCHEMA_VERSION {
            bail!(
                "{} has schema {}; this server reads {ARTIFACTS_SCHEMA_VERSION}",
                path.display(),
                listed.schema_version
            );
        }
        semver::Version::parse(&listed.version)
            .with_context(|| format!("{} has an invalid version", path.display()))?;

        let mut unsigned = Vec::new();
        let mut builds = BTreeMap::new();
        for (target, mut artifact) in listed.artifacts {
            if !is_plain_name(&artifact.file) || artifact.file == ARTIFACTS_FILE {
                bail!(
                    "{target}'s file {:?} must be a plain file name",
                    artifact.file
                );
            }
            let actual = sha256_file(&dir.join(&artifact.file))
                .with_context(|| format!("could not read {target}'s file {}", artifact.file))?;
            if !actual.eq_ignore_ascii_case(&artifact.sha256) {
                bail!(
                    "{} does not match its SHA-256 in {ARTIFACTS_FILE}; reinstall this release's downloads",
                    artifact.file
                );
            }
            let signed = artifact.signature.as_deref().is_some_and(|signature| {
                meshrmm_self_update::verify_release_signature(
                    &target,
                    &listed.version,
                    &actual,
                    signature,
                )
                .is_ok()
            });
            if !signed {
                unsigned.push(target.clone());
            }
            artifact.sha256 = actual;
            builds.insert(target, artifact);
        }
        Ok(Self {
            dir,
            public_origin,
            release: Some((listed.version, builds)),
            unsigned,
            developer_id: None,
        })
    }
}

/// `GET /downloads/{*path}`: a build, or the update manifest.
pub async fn serve(
    State(state): State<AppState>,
    UrlPath(path): UrlPath<String>,
    request: Request,
) -> Response {
    if path == MANIFEST_FILE
        && let Some(manifest) = state.downloads.manifest()
    {
        return (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE)),
            ],
            Body::from(manifest),
        )
            .into_response();
    }
    let file = match state.downloads.find(&path) {
        Some(Found::File(file)) => file,
        Some(Found::Unavailable(message)) => {
            return ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message).into_response();
        }
        None => return ApiError::not_found("no such download").into_response(),
    };
    match ServeFile::new(file).oneshot(request).await {
        Ok(response) => {
            let mut response = response.map(Body::new);
            if response.status().is_success() {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE));
            }
            response
        }
        Err(error) => {
            tracing::error!(%error, path, "could not serve a download");
            ApiError::internal().into_response()
        }
    }
}

/// A name with no directory part that isn't hidden.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(crate::secrets::hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_are_served() {
        assert!(is_plain_name("meshrmm-agent-windows-x64.exe"));
        for name in [
            "",
            ".hidden",
            "../instance.key",
            "a/b",
            "a\\b",
            "space name",
        ] {
            assert!(!is_plain_name(name), "{name:?}");
        }
    }
}
