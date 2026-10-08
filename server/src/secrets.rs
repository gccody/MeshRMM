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

use anyhow::{Context, bail};

const INSTANCE_KEY_FILE: &str = "instance.key";
const INSTANCE_KEY_BYTES: usize = 32;

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
        let mut key = [0; INSTANCE_KEY_BYTES];
        getrandom::fill(&mut key)
            .map_err(|error| anyhow::anyhow!("could not generate the instance key: {error}"))?;
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
    fn debug_output_hides_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = InstanceKey::load_or_create(dir.path()).unwrap();
        assert_eq!(format!("{key:?}"), "InstanceKey(..)");
    }
}
