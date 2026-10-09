//! Signing the macOS builds with the company's own Developer ID.
//!
//! Releases sign the macOS Agent and viewer only ad hoc. When
//! `downloads.macos_signing` is set, the server unpacks each one, signs it
//! with rcodesign and the company's Developer ID Application certificate,
//! notarizes it if given an App Store Connect API key, and packs it again in
//! `<data_dir>/developer-id`. It does this once per release and certificate;
//! later starts reuse the signed builds.
//!
//! Installed Agents and viewers signed by a team accept only updates signed
//! by the same team (see `meshrmm_self_update::macos`), so the signed builds
//! need no release signature.
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, RwLock},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use super::sha256_file;
use crate::config::MacosSigningConfig;

/// The data directory's subdirectory for the signed builds.
pub const DIR: &str = "developer-id";
const RECORD: &str = "builds.json";
const WORK: &str = ".work";
/// Apple usually notarizes in minutes, but can take much longer.
const NOTARY_WAIT_SECONDS: &str = "3600";
const DEVELOPER_ID_PROFILE: &str = "DeveloperIdApplication";

/// A release build to sign.
#[derive(Debug)]
pub struct Source {
    pub path: PathBuf,
    pub file: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningStatus {
    Signing,
    Signed,
    Failed(String),
}

impl std::fmt::Display for SigningStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signing => {
                formatter.write_str("not signed yet; the server signs them when it starts")
            }
            Self::Signed => formatter.write_str("signed with your Developer ID"),
            Self::Failed(error) => write!(formatter, "signing failed: {error}"),
        }
    }
}

/// A build signed with the company's Developer ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBuild {
    pub file: String,
    /// The release build it was made from.
    pub source_sha256: String,
    pub sha256: String,
}

/// `builds.json`: what the signed builds were made from.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    certificate_sha256: String,
    notarized: bool,
    builds: BTreeMap<String, SignedBuild>,
}

#[derive(Debug)]
enum State {
    Signing,
    Failed(String),
    Signed(Arc<BTreeMap<String, SignedBuild>>),
}

#[derive(Debug)]
pub struct Signer {
    config: MacosSigningConfig,
    dir: PathBuf,
    certificate_sha256: String,
    sources: BTreeMap<String, Source>,
    state: RwLock<State>,
}

impl Signer {
    /// Picks up the builds already signed from `sources` with this
    /// certificate, if every one is.
    pub fn load(
        config: MacosSigningConfig,
        dir: PathBuf,
        sources: BTreeMap<String, Source>,
    ) -> anyhow::Result<Self> {
        let certificate_sha256 = sha256_file(&config.certificate).with_context(|| {
            format!(
                "could not read downloads.macos_signing.certificate {}",
                config.certificate.display()
            )
        })?;
        let signer = Self {
            config,
            dir,
            certificate_sha256,
            sources,
            state: RwLock::new(State::Signing),
        };
        if let Some(builds) = signer.signed_already() {
            *signer.state.write().expect("lock poisoned") = State::Signed(Arc::new(builds));
        }
        Ok(signer)
    }

    fn signed_already(&self) -> Option<BTreeMap<String, SignedBuild>> {
        let record: Record = serde_json::from_slice(&fs::read(self.dir.join(RECORD)).ok()?).ok()?;
        let current = record.certificate_sha256 == self.certificate_sha256
            && record.notarized == self.config.notary_api_key.is_some()
            && record.builds.len() == self.sources.len()
            && self.sources.iter().all(|(target, source)| {
                record.builds.get(target).is_some_and(|build| {
                    build.file == source.file
                        && build.source_sha256 == source.sha256
                        && sha256_file(&self.dir.join(&build.file))
                            .is_ok_and(|actual| actual == build.sha256)
                })
            });
        current.then_some(record.builds)
    }

    pub fn status(&self) -> SigningStatus {
        match &*self.state.read().expect("lock poisoned") {
            State::Signing => SigningStatus::Signing,
            State::Failed(error) => SigningStatus::Failed(error.clone()),
            State::Signed(_) => SigningStatus::Signed,
        }
    }

    /// The signed builds by target, once signing is done.
    pub fn builds(&self) -> Option<Arc<BTreeMap<String, SignedBuild>>> {
        match &*self.state.read().expect("lock poisoned") {
            State::Signed(builds) => Some(Arc::clone(builds)),
            _ => None,
        }
    }

    /// The signed build named `file`.
    pub fn signed_file(&self, file: &str) -> Option<PathBuf> {
        self.builds()?
            .values()
            .any(|build| build.file == file)
            .then(|| self.dir.join(file))
    }

    /// The signed build of `target`, or why it can't be downloaded.
    pub fn signed_build(&self, target: &str) -> Result<PathBuf, String> {
        match &*self.state.read().expect("lock poisoned") {
            State::Signed(builds) => builds
                .get(target)
                .map(|build| self.dir.join(&build.file))
                .ok_or_else(|| format!("this server has no {target} build")),
            State::Signing => Err(
                "The server is still signing the macOS builds with your Developer ID. Try again in a few minutes."
                    .to_owned(),
            ),
            State::Failed(_) => Err(
                "The server could not sign the macOS builds with your Developer ID. Its log says why."
                    .to_owned(),
            ),
        }
    }

    /// Signs (and notarizes) every build, then serves them. Blocks for as
    /// long as that takes.
    pub fn sign(&self) {
        tracing::info!(
            builds = self.sources.len(),
            notarize = self.config.notary_api_key.is_some(),
            "signing the macOS builds with your Developer ID"
        );
        let state = match self.sign_all() {
            Ok(builds) => {
                tracing::info!("signed the macOS builds with your Developer ID");
                State::Signed(Arc::new(builds))
            }
            Err(error) => {
                let error = format!("{error:#}");
                tracing::error!(
                    %error,
                    "could not sign the macOS builds with your Developer ID; macOS installers stay unavailable until a restart signs them"
                );
                State::Failed(error)
            }
        };
        *self.state.write().expect("lock poisoned") = state;
    }

    fn sign_all(&self) -> anyhow::Result<BTreeMap<String, SignedBuild>> {
        check_certificate(&self.config)?;
        let work = self.dir.join(WORK);
        remove_dir_if_present(&work)?;
        fs::create_dir_all(&work)
            .with_context(|| format!("could not create {}", work.display()))?;
        let mut builds = BTreeMap::new();
        for (target, source) in &self.sources {
            let app = unpack(&source.path, &work.join(target))
                .with_context(|| format!("could not unpack {}", source.file))?;
            tracing::info!(target, "signing");
            rcodesign(
                &self.config,
                [
                    "sign".as_ref(),
                    "--for-notarization".as_ref(),
                    "--p12-file".as_ref(),
                    self.config.certificate.as_os_str(),
                    "--p12-password-file".as_ref(),
                    self.config.certificate_password_file.as_os_str(),
                    app.as_os_str(),
                ],
            )
            .with_context(|| format!("could not sign {target}"))?;
            if let Some(key) = &self.config.notary_api_key {
                tracing::info!(target, "notarizing with Apple");
                rcodesign(
                    &self.config,
                    [
                        "notary-submit".as_ref(),
                        "--api-key-file".as_ref(),
                        key.as_os_str(),
                        "--max-wait-seconds".as_ref(),
                        NOTARY_WAIT_SECONDS.as_ref(),
                        "--staple".as_ref(),
                        app.as_os_str(),
                    ],
                )
                .with_context(|| format!("could not notarize {target}"))?;
            }
            let packed = work.join(&source.file);
            pack(&app, &packed).with_context(|| format!("could not pack {}", source.file))?;
            builds.insert(
                target.clone(),
                SignedBuild {
                    file: source.file.clone(),
                    source_sha256: source.sha256.clone(),
                    sha256: sha256_file(&packed)?,
                },
            );
        }

        // Nothing serves the previous builds while this runs.
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        for build in builds.values() {
            fs::rename(work.join(&build.file), self.dir.join(&build.file))?;
        }
        let record = Record {
            certificate_sha256: self.certificate_sha256.clone(),
            notarized: self.config.notary_api_key.is_some(),
            builds,
        };
        let temporary = self.dir.join(format!("{RECORD}.new"));
        fs::write(&temporary, serde_json::to_vec_pretty(&record)?)?;
        fs::rename(&temporary, self.dir.join(RECORD))?;
        remove_dir_if_present(&work)?;
        Ok(record.builds)
    }
}

/// Checks that rcodesign runs and that `config.certificate` opens with its
/// password and starts with a Developer ID Application certificate. Returns
/// rcodesign's version and the certificate's team.
pub fn check_certificate(config: &MacosSigningConfig) -> anyhow::Result<(String, String)> {
    let version = rcodesign(config, ["--version"])?;
    let analysis = rcodesign(
        config,
        [
            "analyze-certificate".as_ref(),
            "--p12-file".as_ref(),
            config.certificate.as_os_str(),
            "--p12-password-file".as_ref(),
            config.certificate_password_file.as_os_str(),
        ],
    )
    .context(
        "rcodesign could not open the certificate; check its password, and that the file uses legacy encryption (openssl pkcs12 -export -legacy)",
    )?;
    // rcodesign signs with the file's first certificate.
    let field = |name: &str| {
        analysis
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| value.trim().to_owned())
    };
    if field("Guessed Certificate Profile:").as_deref() != Some(DEVELOPER_ID_PROFILE) {
        bail!(
            "the first certificate in {} isn't a Developer ID Application certificate; export the certificate and its private key alone",
            config.certificate.display()
        );
    }
    if let Some(key) = &config.notary_api_key {
        fs::metadata(key).with_context(|| {
            format!(
                "could not read downloads.macos_signing.notary_api_key {}",
                key.display()
            )
        })?;
    }
    Ok((
        version.trim().to_owned(),
        field("Team ID:").unwrap_or_default(),
    ))
}

/// Runs rcodesign and returns what it printed. A failure's error ends with
/// the last lines rcodesign printed.
fn rcodesign<I, S>(config: &MacosSigningConfig, arguments: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(&config.rcodesign)
        .args(arguments)
        .output()
        .with_context(|| format!("could not run {}", config.rcodesign.display()))?;
    let mut printed = String::from_utf8_lossy(&output.stdout).into_owned();
    printed.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        let lines: Vec<&str> = printed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        bail!(
            "rcodesign failed ({}): {}",
            output.status,
            lines[lines.len().saturating_sub(5)..].join(" / ")
        );
    }
    Ok(printed)
}

fn remove_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Unpacks the zip archive `archive` into `into` and returns the one app
/// bundle it holds.
fn unpack(archive: &Path, into: &Path) -> anyhow::Result<PathBuf> {
    let mut archive = ZipArchive::new(fs::File::open(archive)?)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .with_context(|| format!("{:?} is outside the archive", entry.name()))?;
        // ditto keeps extended attributes there.
        if relative.starts_with("__MACOSX") {
            continue;
        }
        let path = into.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }
        if entry.is_symlink() {
            bail!("{} is a symbolic link", entry.name());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(&path)?;
        io::copy(&mut entry, &mut file)?;
        set_mode(&path, entry.unix_mode().unwrap_or(0o644))?;
    }
    let mut apps = Vec::new();
    for entry in fs::read_dir(into)? {
        let path = entry?.path();
        if path.extension() == Some(OsStr::new("app")) && path.is_dir() {
            apps.push(path);
        }
    }
    match <[PathBuf; 1]>::try_from(apps) {
        Ok([app]) => Ok(app),
        Err(_) => bail!("the archive must hold exactly one app bundle"),
    }
}

/// Packs the app bundle `app` into the zip archive `archive`, as
/// `ditto -c -k --keepParent` would.
fn pack(app: &Path, archive: &Path) -> anyhow::Result<()> {
    let root = app.parent().context("the app bundle has no parent")?;
    let mut writer = ZipWriter::new(fs::File::create(archive)?);
    let mut pending = vec![app.to_owned()];
    while let Some(directory) = pending.pop() {
        let name = zip_name(root, &directory)?;
        writer.add_directory(
            format!("{name}/"),
            SimpleFileOptions::default()
                .unix_permissions(0o755)
                .last_modified_time(modified(&directory)),
        )?;
        let mut entries: Vec<fs::DirEntry> =
            fs::read_dir(&directory)?.collect::<io::Result<_>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        let mut directories = Vec::new();
        for entry in entries {
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                directories.push(path);
            } else if kind.is_file() {
                writer.start_file(
                    zip_name(root, &path)?,
                    SimpleFileOptions::default()
                        .compression_method(CompressionMethod::Deflated)
                        .last_modified_time(modified(&path))
                        .unix_permissions(mode(&path)?),
                )?;
                io::copy(&mut fs::File::open(&path)?, &mut writer)?;
            } else {
                bail!("{} is neither a file nor a directory", path.display());
            }
        }
        pending.extend(directories.into_iter().rev());
    }
    writer.finish()?;
    Ok(())
}

/// When `path` last changed, in UTC, as a zip archive records it.
fn modified(path: &Path) -> zip::DateTime {
    use chrono::{Datelike, Timelike};
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| {
            let time = chrono::DateTime::<chrono::Utc>::from(time);
            zip::DateTime::from_date_and_time(
                u16::try_from(time.year()).ok()?,
                time.month() as u8,
                time.day() as u8,
                time.hour() as u8,
                time.minute() as u8,
                time.second() as u8,
            )
            .ok()
        })
        .unwrap_or_default()
}

fn zip_name(root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(root)?;
    let parts: Option<Vec<&str>> = relative.iter().map(OsStr::to_str).collect();
    Ok(parts
        .with_context(|| format!("{} isn't UTF-8", path.display()))?
        .join("/"))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn mode(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode(_: &Path) -> io::Result<u32> {
    Ok(0o644)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn packing_keeps_the_bundle_and_its_executable_bit() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("in/Test.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/Info.plist"), b"plist").unwrap();
        fs::write(app.join("Contents/MacOS/test"), b"binary").unwrap();
        set_mode(&app.join("Contents/MacOS/test"), 0o755).unwrap();
        let archive = dir.path().join("test.zip");
        pack(&app, &archive).unwrap();

        let unpacked = unpack(&archive, &dir.path().join("out")).unwrap();
        assert_eq!(unpacked, dir.path().join("out/Test.app"));
        assert_eq!(
            fs::read(unpacked.join("Contents/MacOS/test")).unwrap(),
            b"binary"
        );
        assert_eq!(mode(&unpacked.join("Contents/MacOS/test")).unwrap(), 0o755);
        assert_eq!(
            mode(&unpacked.join("Contents/Info.plist")).unwrap() & 0o111,
            0
        );
    }

    #[test]
    fn unpacking_refuses_paths_outside_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.zip");
        let mut writer = ZipWriter::new(fs::File::create(&archive).unwrap());
        writer
            .start_file("../escape", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();
        assert!(unpack(&archive, &dir.path().join("out")).is_err());
        assert!(!dir.path().join("escape").exists());
    }
}
