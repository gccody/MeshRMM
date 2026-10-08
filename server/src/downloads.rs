//! The Agent and viewer builds shipped with this server release: the files in
//! the downloads directory that its `artifacts.json` lists, and the update
//! manifest made from it.
//!
//! The manifest points every build at this server, so installed Agents and
//! viewers update from the server they use. Each build carries the release
//! key's signature, which they check before installing it; the server only
//! passes it on.
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, bail};
use axum::{
    body::Body,
    extract::{Path as UrlPath, Request, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use meshrmm_self_update::{MANIFEST_SCHEMA_VERSION, Release, UpdateManifest};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::http::{ApiError, AppState};

pub const ARTIFACTS_FILE: &str = "artifacts.json";
pub const MANIFEST_FILE: &str = "update-manifest.json";
const ARTIFACTS_SCHEMA_VERSION: u32 = 1;
/// Upgrading the server replaces the builds under the same names.
const REVALIDATE: &str = "no-cache";

/// `artifacts.json`, as `scripts/release-artifacts.mjs` writes it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactsFile {
    schema_version: u32,
    version: String,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Debug, Deserialize)]
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
    files: HashSet<String>,
    version: Option<String>,
    /// Targets whose builds installed Agents and viewers won't accept.
    unsigned: Vec<String>,
    manifest: Option<Bytes>,
}

impl Downloads {
    /// Reads `dir`'s `artifacts.json` and checks every build it lists
    /// against its SHA-256. A missing `artifacts.json` means there are no
    /// downloads, as in development; anything else wrong with it is an error.
    pub async fn load(dir: &Path, public_origin: &str) -> anyhow::Result<Self> {
        let (dir, origin) = (dir.to_owned(), public_origin.to_owned());
        tokio::task::spawn_blocking(move || Self::load_blocking(dir, &origin)).await?
    }

    fn load_blocking(dir: PathBuf, public_origin: &str) -> anyhow::Result<Self> {
        let path = dir.join(ARTIFACTS_FILE);
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    inner: Arc::new(Inner {
                        dir,
                        ..Inner::default()
                    }),
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

        let mut files = HashSet::new();
        let mut unsigned = Vec::new();
        let mut releases = BTreeMap::new();
        for (target, artifact) in listed.artifacts {
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
            releases.insert(
                target,
                Release {
                    version: listed.version.clone(),
                    url: format!("{public_origin}/downloads/{}", artifact.file),
                    sha256: actual,
                    signature: artifact.signature,
                },
            );
            files.insert(artifact.file);
        }
        let manifest = serde_json::to_vec_pretty(&UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            releases,
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                files,
                version: Some(listed.version),
                unsigned,
                manifest: Some(Bytes::from(manifest)),
            }),
        })
    }

    /// The release the builds belong to, or `None` without downloads.
    pub fn version(&self) -> Option<&str> {
        self.inner.version.as_deref()
    }

    pub fn file_count(&self) -> usize {
        self.inner.files.len()
    }

    /// Targets whose builds aren't signed with the release key this server
    /// was built with. Agents and viewers built with it won't update to them.
    pub fn unsigned(&self) -> &[String] {
        &self.inner.unsigned
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
    }
}

/// `GET /downloads/{file}`: a listed build, or the update manifest.
pub async fn serve(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    request: Request,
) -> Response {
    let downloads = &state.downloads.inner;
    if file == MANIFEST_FILE
        && let Some(manifest) = &downloads.manifest
    {
        return (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE)),
            ],
            Body::from(manifest.clone()),
        )
            .into_response();
    }
    if !downloads.files.contains(&file) {
        return ApiError::not_found("no such download").into_response();
    }
    match ServeFile::new(downloads.dir.join(&file))
        .oneshot(request)
        .await
    {
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
            tracing::error!(%error, file, "could not serve a download");
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
