//! The data directory and the instance key kept in it.
//!
//! The instance key encrypts secrets stored in the database (TOTP seeds, the
//! OIDC client secret, the SMTP password) and signs TURN credentials. It is
//! generated on first start; backups of the database are useless without it.
use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

const INSTANCE_KEY_FILE: &str = "instance.key";
const INSTANCE_KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
/// Separates the encryption key from other keys derived from the instance key.
const ENCRYPTION_KEY_LABEL: &[u8] = b"meshrmm secret encryption v1";

#[derive(Clone)]
pub struct InstanceKey([u8; INSTANCE_KEY_BYTES]);

impl InstanceKey {
    pub fn as_bytes(&self) -> &[u8; INSTANCE_KEY_BYTES] {
        &self.0
    }

    /// Reads the key from `data_dir`, generating and saving one (readable by
    /// the server's user only) if there is none.
    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(INSTANCE_KEY_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                return Self::parse(text.trim())
                    .with_context(|| format!("{} is corrupt", path.display()));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        }
        let key = random_bytes::<INSTANCE_KEY_BYTES>();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            // Another process created it first; use theirs.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Self::load_or_create(data_dir);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not create {}", path.display()));
            }
        };
        writeln!(file, "{}", hex(&key))
            .and_then(|()| file.sync_all())
            .with_context(|| format!("could not write {}", path.display()))?;
        tracing::info!(path = %path.display(), "generated the instance key; back it up with the database");
        Ok(Self(key))
    }

    /// Encrypts a secret for storage with AES-256-GCM. `context` names what
    /// the secret is and whose (for example `totp:<user id>`), so a stored
    /// value copied to another row or column no longer decrypts.
    pub fn encrypt(&self, context: &str, plaintext: &[u8]) -> Vec<u8> {
        let nonce = random_bytes::<NONCE_BYTES>();
        let ciphertext = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: context.as_bytes(),
                },
            )
            .expect("AES-GCM encryption of a bounded secret cannot fail");
        [nonce.as_slice(), &ciphertext].concat()
    }

    /// Decrypts a value from [`InstanceKey::encrypt`] with the same `context`.
    pub fn decrypt(&self, context: &str, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < NONCE_BYTES {
            bail!("the encrypted value is truncated");
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_BYTES);
        self.cipher()
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| {
                anyhow::anyhow!("the encrypted value does not match the instance key; was instance.key replaced?")
            })
    }

    /// A key for one purpose, named by `label`, derived from the instance
    /// key so that no two purposes share a key.
    pub fn derive(&self, label: &[u8]) -> [u8; 32] {
        Sha256::new()
            .chain_update(label)
            .chain_update(self.0)
            .finalize()
            .into()
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(&self.derive(ENCRYPTION_KEY_LABEL).into())
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        if text.len() != INSTANCE_KEY_BYTES * 2 {
            bail!("expected {} hex digits", INSTANCE_KEY_BYTES * 2);
        }
        let mut key = [0; INSTANCE_KEY_BYTES];
        for (byte, pair) in key.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).context("not hex")?;
            *byte = u8::from_str_radix(pair, 16).context("not hex")?;
        }
        Ok(Self(key))
    }
}

impl fmt::Debug for InstanceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InstanceKey(..)")
    }
}

/// Creates the data directory, readable by the server's user only, if it
/// does not exist.
pub fn ensure_data_dir(path: &Path) -> anyhow::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(path)
        .with_context(|| format!("could not create the data directory {}", path.display()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Bytes from the operating system's secure random source.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source failed");
    bytes
}

/// A new bearer token: 32 random bytes as 64 hex digits.
pub fn new_token() -> String {
    hex(&random_bytes::<32>())
}

/// The SHA-256 of a token as 64 hex digits. Tokens are stored only as this
/// hash, so a database leak does not reveal usable tokens.
pub fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_created_once_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let created = InstanceKey::load_or_create(dir.path()).unwrap();
        let loaded = InstanceKey::load_or_create(dir.path()).unwrap();
        assert_eq!(created.as_bytes(), loaded.as_bytes());
        assert_ne!(created.as_bytes(), &[0; INSTANCE_KEY_BYTES]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(INSTANCE_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_corrupt_key_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(INSTANCE_KEY_FILE), "not a key\n").unwrap();
        assert!(InstanceKey::load_or_create(dir.path()).is_err());
    }

    #[test]
    fn secrets_decrypt_only_with_the_same_key_and_context() {
        let dir = tempfile::tempdir().unwrap();
        let key = InstanceKey::load_or_create(dir.path()).unwrap();
        let sealed = key.encrypt("totp:user-1", b"seed");
        assert_eq!(key.decrypt("totp:user-1", &sealed).unwrap(), b"seed");
        assert_ne!(sealed, key.encrypt("totp:user-1", b"seed"), "nonces repeat");
        assert!(key.decrypt("totp:user-2", &sealed).is_err());
        assert!(key.decrypt("totp:user-1", &sealed[..8]).is_err());

        let other_dir = tempfile::tempdir().unwrap();
        let other = InstanceKey::load_or_create(other_dir.path()).unwrap();
        assert!(other.decrypt("totp:user-1", &sealed).is_err());
    }

    #[test]
    fn token_hashes_are_sha256_hex() {
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let token = new_token();
        assert_eq!(token.len(), 64);
        assert_ne!(token, new_token());
    }

    #[test]
    fn debug_output_hides_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = InstanceKey::load_or_create(dir.path()).unwrap();
        assert_eq!(format!("{key:?}"), "InstanceKey(..)");
    }
}
