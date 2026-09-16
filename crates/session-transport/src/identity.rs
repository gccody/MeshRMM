//! Persistent local certificates; peers are authorized by authenticated signaling.
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use webrtc::{dtls::crypto::Certificate, peer_connection::certificate::RTCCertificate};

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

pub fn agent_directory() -> anyhow::Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("ProgramData").context("ProgramData is unavailable")?)
            .join("MeshRMM/Agent/identity"),
    )
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

// Publish without overwriting another process's key or a previous trust decision.
fn publish_new(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let directory = path.parent().context("identity file has no directory")?;
    private_directory(directory)?;
    let temporary = directory.join(format!(
        ".identity-{}-{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
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
        fs::hard_link(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

pub fn normalize_fingerprint(value: &str) -> anyhow::Result<String> {
    let value = value.replace(':', "").to_ascii_lowercase();
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected a SHA-256 certificate fingerprint (64 hexadecimal digits)"
    );
    Ok(value)
}

impl PeerIdentity {
    pub fn load(directory: &Path) -> anyhow::Result<Self> {
        Self::load_inner(directory).map_err(|error| IdentityError(format!("{error:#}")).into())
    }

    fn load_inner(directory: &Path) -> anyhow::Result<Self> {
        private_directory(directory)?;
        let path = directory.join("identity.json");
        if !path.try_exists()? {
            let certificate = Certificate::generate_self_signed(vec!["meshrmm-peer".into()])?;
            let stored = StoredIdentity {
                version: 1,
                expires_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
                    + 5 * 365 * 86400,
                pem: certificate.serialize_pem(),
            };
            if let Err(error) = publish_new(&path, &serde_json::to_vec(&stored)?) {
                // A concurrent creator may have won. Never replace its identity.
                if !path.try_exists()? {
                    return Err(error);
                }
            }
        }
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_file(),
            "identity must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
                "identity private key must have owner-only permissions"
            );
        }
        let stored: StoredIdentity = serde_json::from_slice(&fs::read(&path)?)
            .context("invalid local identity; refusing to replace its key")?;
        ensure!(stored.version == 1, "unsupported local identity format");
        let expires = UNIX_EPOCH
            .checked_add(Duration::from_secs(stored.expires_at))
            .context("invalid identity expiration")?;
        ensure!(
            expires > SystemTime::now(),
            "local identity expired; administrator rotation required"
        );
        let certificate =
            Certificate::from_pem(&stored.pem).context("invalid local certificate")?;
        Ok(Self {
            certificate: RTCCertificate::from_existing(certificate, expires),
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

    #[test]
    fn expired_keys_are_rejected() {
        let directory = TestDirectory::new();
        PeerIdentity::load(&directory.0).unwrap();
        let path = directory.0.join("identity.json");
        let mut stored: StoredIdentity = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        stored.expires_at = 1;
        fs::write(path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(PeerIdentity::load(&directory.0).is_err());
    }

    #[tokio::test]
    async fn fresh_webrtc_peers_exchange_data_without_any_enrollment() {
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
