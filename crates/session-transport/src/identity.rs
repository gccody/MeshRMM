//! Persistent local certificates; peers are authorized by authenticated signaling.
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use webrtc::{
    dtls::crypto::{Certificate, CryptoPrivateKey},
    peer_connection::certificate::RTCCertificate,
};

/// How long a generated identity lasts, in both the stored expiry and its
/// certificate's own validity.
const LIFETIME_SECONDS: u64 = 5 * 365 * 86400;
/// Renew this long before expiry. Every session loads the identity, so a
/// device or viewer used at least monthly renews while its identity still
/// works, well clear of the moment WebRTC refuses an expired certificate;
/// one idle for longer renews at its next session instead.
const RENEWAL_WINDOW_SECONDS: u64 = 30 * 86400;
const IDENTITY_FILE: &str = "identity.json";
/// Held while renewing so concurrent loaders renew once. It is never read: on
/// Windows a lock on the identity file itself would fail other loaders' reads.
const LOCK_FILE: &str = "identity.lock";

#[derive(Debug, thiserror::Error)]
#[error("Peer identity verification failed: {0}")]
pub struct IdentityError(pub String);

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    version: u32,
    expires_at: u64,
    pem: String,
}

pub struct PeerIdentity {
    pub certificate: RTCCertificate,
    directory: PathBuf,
}

pub fn viewer_directory() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let root = PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is unavailable")?);
    #[cfg(not(windows))]
    let root = PathBuf::from(std::env::var_os("HOME").context("Home directory is unavailable")?)
        .join("Library/Application Support");
    Ok(root.join("MeshRMM/viewer-identity"))
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_dir(),
            "identity directory must not be a symlink"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

/// Write `contents` to a new owner-only file beside `path`, then hand both
/// names to `publish`. The temporary name is removed either way.
fn publish_with(
    path: &Path,
    contents: &[u8],
    publish: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    // Threads of one process may publish in the same clock tick.
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().context("identity file has no directory")?;
    private_directory(directory)?;
    let temporary = directory.join(format!(
        ".identity-{}-{}-{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> anyhow::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        publish(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

// Publish without overwriting another process's key or a previous trust decision.
fn publish_new(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    publish_with(path, contents, |temporary, path| {
        fs::hard_link(temporary, path)
    })
}

/// Replace `path` atomically: a reader sees the old file or the new one. The
/// rename replaces the directory entry itself, so it never writes through a
/// symlink there.
fn replace(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    publish_with(path, contents, |temporary, path| {
        fs::rename(temporary, path)
    })
}

pub fn normalize_fingerprint(value: &str) -> anyhow::Result<String> {
    let value = value.replace(':', "").to_ascii_lowercase();
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected a SHA-256 certificate fingerprint (64 hexadecimal digits)"
    );
    Ok(value)
}

/// A new identity whose certificate is valid from `now` until exactly the
/// stored expiry. Peers check only the fingerprint signaled for each session,
/// but the two lifetimes agree so that neither outlives the other.
fn generate(now: SystemTime) -> anyhow::Result<StoredIdentity> {
    let issued = now.duration_since(UNIX_EPOCH)?.as_secs();
    let expires_at = issued
        .checked_add(LIFETIME_SECONDS)
        .context("invalid identity expiration")?;
    let mut params = rcgen::CertificateParams::new(vec!["meshrmm-peer".to_owned()])?;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(i64::try_from(issued)?)?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(i64::try_from(expires_at)?)?;
    // The same ECDSA P-256 key `Certificate::generate_self_signed` makes.
    let key_pair = rcgen::KeyPair::generate()?;
    let certificate = params.self_signed(&key_pair)?;
    let certificate = Certificate {
        certificate: vec![certificate.der().to_owned()],
        private_key: CryptoPrivateKey::try_from(&key_pair)?,
    };
    Ok(StoredIdentity {
        version: 1,
        expires_at,
        pem: certificate.serialize_pem(),
    })
}

struct LoadedIdentity {
    certificate: RTCCertificate,
    expires: SystemTime,
}

impl LoadedIdentity {
    fn needs_renewal(&self, now: SystemTime) -> bool {
        // Already expired when `expires` is earlier than `now`.
        !self
            .expires
            .duration_since(now)
            .is_ok_and(|left| left.as_secs() >= RENEWAL_WINDOW_SECONDS)
    }

    fn fingerprint(&self) -> String {
        self.certificate.get_fingerprints()[0].value.clone()
    }
}

/// Read and check the stored identity. A file that is not an owner-only
/// regular file, or does not parse, is refused and never replaced.
fn read_identity(path: &Path) -> anyhow::Result<LoadedIdentity> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "identity must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "identity private key must have owner-only permissions"
        );
    }
    let stored: StoredIdentity = serde_json::from_slice(&fs::read(path)?)
        .context("invalid local identity; refusing to replace its key")?;
    ensure!(stored.version == 1, "unsupported local identity format");
    let expires = UNIX_EPOCH
        .checked_add(Duration::from_secs(stored.expires_at))
        .context("invalid identity expiration")?;
    let certificate = Certificate::from_pem(&stored.pem).context("invalid local certificate")?;
    Ok(LoadedIdentity {
        certificate: RTCCertificate::from_existing(certificate, expires),
        expires,
    })
}

/// Hold the renewal lock until the returned file is dropped. The operating
/// system releases it if the process ends, so a crash leaves no stale lock.
fn lock_renewal(directory: &Path) -> anyhow::Result<fs::File> {
    let path = directory.join(LOCK_FILE);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("could not create the identity lock"),
    }
    ensure!(
        fs::symlink_metadata(&path)?.file_type().is_file(),
        "identity lock must be a regular file"
    );
    // Opened read-only: nothing is ever written through this name.
    let file = fs::File::open(&path)?;
    file.lock().context("could not lock the identity")?;
    Ok(file)
}

/// Replace the identity unless a concurrent loader already has. Sessions that
/// loaded the old certificate keep it. Peers learn the new fingerprint from
/// the next session's authenticated signaling; nothing pins the old one.
fn renew(directory: &Path, path: &Path, now: SystemTime) -> anyhow::Result<LoadedIdentity> {
    let _lock = lock_renewal(directory)?;
    let current = read_identity(path)?;
    if !current.needs_renewal(now) {
        return Ok(current);
    }
    let stored = generate(now)?;
    replace(path, &serde_json::to_vec(&stored)?)?;
    let renewed = read_identity(path)?;
    tracing::info!(
        old_fingerprint = %current.fingerprint(),
        new_fingerprint = %renewed.fingerprint(),
        expires_at = stored.expires_at,
        path = %path.display(),
        "renewed the local session identity before it expired"
    );
    Ok(renewed)
}

impl PeerIdentity {
    pub fn load(directory: &Path) -> anyhow::Result<Self> {
        Self::load_at(directory, SystemTime::now(), &|| {})
    }

    /// Load as of `now`. `before_renewal` runs once a renewal is due, before
    /// the renewal lock is taken.
    fn load_at(
        directory: &Path,
        now: SystemTime,
        before_renewal: &dyn Fn(),
    ) -> anyhow::Result<Self> {
        Self::load_inner(directory, now, before_renewal)
            .map_err(|error| IdentityError(format!("{error:#}")).into())
    }

    fn load_inner(
        directory: &Path,
        now: SystemTime,
        before_renewal: &dyn Fn(),
    ) -> anyhow::Result<Self> {
        private_directory(directory)?;
        let path = directory.join(IDENTITY_FILE);
        if !path.try_exists()? {
            let stored = generate(now)?;
            if let Err(error) = publish_new(&path, &serde_json::to_vec(&stored)?) {
                // A concurrent creator may have won. Never replace its identity.
                if !path.try_exists()? {
                    return Err(error);
                }
            }
        }
        let mut identity = read_identity(&path)?;
        if identity.needs_renewal(now) {
            before_renewal();
            match renew(directory, &path, now) {
                Ok(renewed) => identity = renewed,
                // Still valid: use it, and try again at the next session.
                Err(error) if identity.expires > now => tracing::warn!(
                    error = format!("{error:#}"),
                    path = %path.display(),
                    "could not renew the local session identity; using it until it expires"
                ),
                Err(error) => {
                    return Err(error.context(format!(
                        "local identity at {} expired and could not be renewed",
                        path.display()
                    )));
                }
            }
        }
        Ok(Self {
            certificate: identity.certificate,
            directory: directory.into(),
        })
    }

    pub fn fingerprint(&self) -> String {
        self.certificate.get_fingerprints()[0].value.clone()
    }

    /// Check every SDP fingerprint before handing SDP to WebRTC. WebRTC then
    /// verifies the actual DTLS certificate against that signaled fingerprint. Local pre-enrollment is not required.
    pub fn verify_sdp(&self, sdp: &str) -> anyhow::Result<String> {
        let result = (|| -> anyhow::Result<String> {
            let mut fingerprint = None;
            for line in sdp.lines() {
                if let Some(value) = line.trim_end_matches('\r').strip_prefix("a=fingerprint:") {
                    let fields: Vec<_> = value.split_whitespace().collect();
                    ensure!(
                        fields.len() == 2 && fields[0].eq_ignore_ascii_case("sha-256"),
                        "peer must supply a SHA-256 fingerprint"
                    );
                    let candidate = normalize_fingerprint(fields[1])?;
                    if let Some(previous) = &fingerprint {
                        ensure!(previous == &candidate, "conflicting SDP fingerprints");
                    }
                    fingerprint = Some(candidate);
                }
            }
            let fingerprint =
                fingerprint.context("peer did not supply a certificate fingerprint")?;
            ensure!(
                !self
                    .directory
                    .join("revoked-peers")
                    .join(&fingerprint)
                    .try_exists()?,
                "peer certificate has been explicitly revoked locally"
            );
            tracing::info!(peer_fingerprint = %fingerprint, "accepted peer fingerprint from authenticated signaling");
            Ok(fingerprint)
        })();
        result.map_err(|error| IdentityError(format!("{error:#}")).into())
    }
}

/// Explicit local administration only. These commands never contact a server.
/// Optional local certificate blocking; normal connections require no enrollment.
pub fn handle_command(
    default_directory: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<bool> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        return Ok(false);
    };
    if !matches!(
        command.as_str(),
        "--identity-fingerprint" | "--trust-peer" | "--revoke-peer"
    ) {
        return Ok(false);
    }
    let mut directory = None;
    let mut values = Vec::new();
    let mut iter = args[1..].iter();
    while let Some(value) = iter.next() {
        if value == "--identity-directory" {
            ensure!(directory.is_none(), "duplicate identity directory");
            directory = Some(PathBuf::from(
                iter.next().context("missing identity directory")?,
            ));
        } else {
            values.push(value.as_str());
        }
    }
    let directory = match directory {
        Some(path) => path,
        None => default_directory()?,
    };
    if command == "--identity-fingerprint" {
        ensure!(values.is_empty(), "unexpected fingerprint command argument");
        println!("{}", PeerIdentity::load(&directory)?.fingerprint());
    } else {
        ensure!(values.len() == 1, "supply exactly one SHA-256 fingerprint");
        let fingerprint = normalize_fingerprint(values[0])?;
        let path = directory.join("revoked-peers").join(&fingerprint);
        if command == "--trust-peer" {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("could not remove peer block"),
            }
            println!("Peer {fingerprint} will be accepted through authenticated signaling");
        } else {
            if !path.try_exists()? {
                publish_new(&path, fingerprint.as_bytes())?;
            }
            println!("Revoked peer {fingerprint}. Close any active session to apply immediately.");
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(
                format!("meshrmm-identity-{}-{}-{}", std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)),
            ))
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn sdp(fingerprint: &str) -> String {
        format!("v=0\r\na=fingerprint:sha-256 {fingerprint}\r\n")
    }

    #[test]
    fn persistent_identity_does_not_silently_rotate() {
        let directory = TestDirectory::new();
        let first = PeerIdentity::load(&directory.0).unwrap();
        let second = PeerIdentity::load(&directory.0).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
        let path = directory.0.join("identity.json");
        fs::write(&path, "corrupt").unwrap();
        assert!(PeerIdentity::load(&directory.0).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "corrupt");
    }

    #[test]
    fn new_and_replacement_peers_are_accepted_without_enrollment_but_explicit_blocks_apply() {
        let directory = TestDirectory::new();
        let identity = PeerIdentity::load(&directory.0).unwrap();
        let fingerprint = "12".repeat(32);
        identity.verify_sdp(&sdp(&fingerprint)).unwrap();
        identity.verify_sdp(&sdp(&"34".repeat(32))).unwrap();
        assert!(!directory.0.join("trusted-peers").exists());
        publish_new(
            &directory.0.join("revoked-peers").join(&fingerprint),
            b"revoked",
        )
        .unwrap();
        assert!(identity.verify_sdp(&sdp(&fingerprint)).is_err());
        identity.verify_sdp(&sdp(&"34".repeat(32))).unwrap();
    }

    #[test]
    fn malformed_ambiguous_and_missing_fingerprints_fail_closed() {
        let directory = TestDirectory::new();
        let identity = PeerIdentity::load(&directory.0).unwrap();
        let fingerprint = identity.fingerprint();
        for value in [
            "v=0".into(),
            "a=fingerprint:sha-256 ../path".into(),
            sdp(&fingerprint).replace("sha-256", "sha-1"),
            format!("{}{}", sdp(&fingerprint), sdp(&"34".repeat(32))),
            format!("{} extra", sdp(&fingerprint).trim()),
        ] {
            assert!(identity.verify_sdp(&value).is_err(), "{value}");
        }
        identity
            .verify_sdp(&format!(
                "{}{}",
                sdp(&fingerprint),
                sdp(&fingerprint.to_uppercase())
            ))
            .unwrap();
    }

    const DAY: Duration = Duration::from_secs(86400);
    const LIFETIME: Duration = Duration::from_secs(LIFETIME_SECONDS);

    fn issued() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn expiry() -> SystemTime {
        issued() + LIFETIME
    }

    fn seconds(time: SystemTime) -> i64 {
        time.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn load_at(directory: &Path, now: SystemTime) -> anyhow::Result<PeerIdentity> {
        PeerIdentity::load_at(directory, now, &|| {})
    }

    fn stored(directory: &Path) -> StoredIdentity {
        serde_json::from_slice(&fs::read(directory.join(IDENTITY_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn identity_before_the_renewal_window_is_left_untouched() {
        let directory = TestDirectory::new();
        let first = load_at(&directory.0, issued()).unwrap();
        let path = directory.0.join(IDENTITY_FILE);
        let contents = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let later = load_at(&directory.0, expiry() - 31 * DAY).unwrap();
        assert_eq!(first.fingerprint(), later.fingerprint());
        assert_eq!(fs::read(&path).unwrap(), contents);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert!(!directory.0.join(LOCK_FILE).exists());
    }

    #[test]
    fn identity_inside_the_renewal_window_or_expired_is_replaced() {
        for now in [expiry() - 29 * DAY, expiry(), expiry() + 400 * DAY] {
            let directory = TestDirectory::new();
            let running = load_at(&directory.0, issued()).unwrap();
            let old = running.fingerprint();
            let renewed = load_at(&directory.0, now).unwrap();
            assert_ne!(renewed.fingerprint(), old);
            // A session that loaded the old identity keeps it.
            assert_eq!(running.fingerprint(), old);
            assert_eq!(
                stored(&directory.0).expires_at as i64,
                seconds(now + LIFETIME)
            );
            assert_eq!(
                load_at(&directory.0, now).unwrap().fingerprint(),
                renewed.fingerprint()
            );
        }
    }

    /// Lets `count` threads past only together, failing instead of hanging.
    struct Gate {
        arrived: std::sync::Mutex<usize>,
        all: std::sync::Condvar,
        count: usize,
    }

    impl Gate {
        fn wait(&self) {
            let mut arrived = self.arrived.lock().unwrap();
            *arrived += 1;
            self.all.notify_all();
            let (arrived, timeout) = self
                .all
                .wait_timeout_while(arrived, Duration::from_secs(30), |arrived| {
                    *arrived < self.count
                })
                .unwrap();
            assert!(!timeout.timed_out(), "only {} loaders renewed", *arrived);
        }
    }

    #[test]
    fn concurrent_renewals_agree_on_one_identity() {
        let directory = TestDirectory::new();
        let old = load_at(&directory.0, issued()).unwrap().fingerprint();
        let now = expiry() + DAY;
        // Both loaders have read the expired identity before either renews.
        let gate = Gate {
            arrived: std::sync::Mutex::new(0),
            all: std::sync::Condvar::new(),
            count: 2,
        };
        let [first, second] = std::thread::scope(|scope| {
            let load = || {
                scope.spawn(|| {
                    PeerIdentity::load_at(&directory.0, now, &|| gate.wait())
                        .unwrap()
                        .fingerprint()
                })
            };
            let loaders = [load(), load()];
            loaders.map(|loader| loader.join().unwrap())
        });
        assert_ne!(first, old);
        assert_eq!(first, second);
        assert_eq!(load_at(&directory.0, now).unwrap().fingerprint(), first);
    }

    #[test]
    fn failed_renewal_uses_a_still_valid_identity_and_explains_an_expired_one() {
        let directory = TestDirectory::new();
        let old = load_at(&directory.0, issued()).unwrap().fingerprint();
        let contents = fs::read(directory.0.join(IDENTITY_FILE)).unwrap();
        // The lock cannot be taken, so renewal fails.
        fs::create_dir(directory.0.join(LOCK_FILE)).unwrap();
        assert_eq!(
            load_at(&directory.0, expiry() - DAY).unwrap().fingerprint(),
            old
        );
        let error = load_at(&directory.0, expiry() + DAY).err().unwrap();
        assert!(error.downcast_ref::<IdentityError>().is_some());
        let message = format!("{error:#}");
        assert!(
            message.contains("expired and could not be renewed"),
            "{message}"
        );
        assert!(message.contains(IDENTITY_FILE), "{message}");
        assert_eq!(fs::read(directory.0.join(IDENTITY_FILE)).unwrap(), contents);
    }

    #[test]
    fn certificate_validity_matches_the_stored_expiry() {
        let directory = TestDirectory::new();
        for now in [issued(), expiry()] {
            // The first load creates the identity and the second renews it.
            load_at(&directory.0, now).unwrap();
            let stored = stored(&directory.0);
            let certificate = Certificate::from_pem(&stored.pem).unwrap();
            let (_, x509) =
                x509_parser::parse_x509_certificate(certificate.certificate[0].as_ref()).unwrap();
            assert_eq!(x509.validity().not_before.timestamp(), seconds(now));
            assert_eq!(
                x509.validity().not_after.timestamp(),
                stored.expires_at as i64
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn renewal_keeps_owner_only_files_and_never_writes_through_a_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = TestDirectory::new();
        let path = directory.0.join(IDENTITY_FILE);
        let lock = directory.0.join(LOCK_FILE);
        load_at(&directory.0, issued()).unwrap();
        load_at(&directory.0, expiry()).unwrap();
        for file in [&path, &lock] {
            let mode = fs::symlink_metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{} has mode {mode:o}", file.display());
        }
        let renewed = fs::read(&path).unwrap();
        let expired = expiry() + LIFETIME + DAY;

        // A symlinked identity is refused, and neither it nor its target replaced.
        let elsewhere = TestDirectory::new();
        fs::create_dir_all(&elsewhere.0).unwrap();
        let target = elsewhere.0.join(IDENTITY_FILE);
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(load_at(&directory.0, expired).is_err());
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), renewed);

        // A symlinked lock fails the renewal without creating its target.
        fs::remove_file(&path).unwrap();
        fs::rename(&target, &path).unwrap();
        fs::remove_file(&lock).unwrap();
        let lock_target = elsewhere.0.join(LOCK_FILE);
        symlink(&lock_target, &lock).unwrap();
        assert!(load_at(&directory.0, expired).is_err());
        assert!(!lock_target.exists());
        assert_eq!(fs::read(&path).unwrap(), renewed);
    }

    #[tokio::test]
    async fn fresh_webrtc_peers_exchange_data_without_any_enrollment() {
        // Match the executables' TLS setup; each test must also work in isolation
        // when dependency features enable more than one Rustls provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        use crate::ServiceChannel;
        use std::sync::Arc;
        use webrtc::{api::APIBuilder, peer_connection::configuration::RTCConfiguration};
        let a_directory = TestDirectory::new();
        let b_directory = TestDirectory::new();
        let a_identity = PeerIdentity::load(&a_directory.0).unwrap();
        let b_identity = PeerIdentity::load(&b_directory.0).unwrap();
        let a = APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration {
                certificates: vec![a_identity.certificate.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        let b = APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration {
                certificates: vec![b_identity.certificate.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        let (received, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        b.on_data_channel(Box::new(move |channel| {
            let received = received.clone();
            Box::pin(async move {
                channel.on_message(Box::new(move |message| {
                    received.send(message.data).unwrap();
                    Box::pin(async {})
                }));
            })
        }));
        let channel = ServiceChannel::new(Arc::clone(
            &a.create_data_channel("test", None).await.unwrap(),
        ))
        .await;
        let mut gathered = a.gathering_complete_promise().await;
        a.set_local_description(a.create_offer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        let offer = a.local_description().await.unwrap();
        b_identity.verify_sdp(&offer.sdp).unwrap();
        b.set_remote_description(offer).await.unwrap();
        let mut gathered = b.gathering_complete_promise().await;
        b.set_local_description(b.create_answer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        let answer = b.local_description().await.unwrap();
        a_identity.verify_sdp(&answer.sdp).unwrap();
        a.set_remote_description(answer).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), channel.wait_open())
            .await
            .unwrap()
            .unwrap();
        channel
            .send(&bytes::Bytes::from_static(b"automatic peer payload"))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            b"automatic peer payload"
        );
        a.close().await.unwrap();
        b.close().await.unwrap();
    }
    #[tokio::test]
    async fn signaled_fingerprint_without_matching_private_key_cannot_connect() {
        // Match the executables' TLS setup; each test must also work in isolation
        // when dependency features enable more than one Rustls provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        use webrtc::{
            api::APIBuilder,
            peer_connection::{
                configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
            },
        };
        let directory = TestDirectory::new();
        let identity = PeerIdentity::load(&directory.0).unwrap();
        let attacker = APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap();
        let receiver = APIBuilder::new()
            .build()
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap();
        let (states, mut events) = tokio::sync::mpsc::unbounded_channel();
        receiver.on_peer_connection_state_change(Box::new(move |state| {
            let _ = states.send(state);
            Box::pin(async {})
        }));
        attacker.create_data_channel("forged", None).await.unwrap();
        let mut gathered = attacker.gathering_complete_promise().await;
        attacker
            .set_local_description(attacker.create_offer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        let mut offer = attacker.local_description().await.unwrap();
        let original = offer
            .sdp
            .lines()
            .find_map(|line| line.strip_prefix("a=fingerprint:sha-256 "))
            .unwrap()
            .trim()
            .to_owned();
        offer.sdp = offer.sdp.replace(&original, &identity.fingerprint());
        identity.verify_sdp(&offer.sdp).unwrap();
        receiver.set_remote_description(offer).await.unwrap();
        let mut gathered = receiver.gathering_complete_promise().await;
        receiver
            .set_local_description(receiver.create_answer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        attacker
            .set_remote_description(receiver.local_description().await.unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(state) = events.recv().await {
                assert_ne!(
                    state,
                    RTCPeerConnectionState::Connected,
                    "forged peer connected"
                );
                if state == RTCPeerConnectionState::Failed {
                    return;
                }
            }
            panic!("state channel closed before rejection");
        })
        .await
        .expect("DTLS must reject the actual certificate, not merely stall");
        attacker.close().await.unwrap();
        receiver.close().await.unwrap();
    }
}
