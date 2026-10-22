//! Uploads: bytes collect in a hidden partial file beside where the file
//! goes, `.meshrmm-upload-<id>`, across as many transfers as it takes. An
//! interrupted upload goes on from the partial file's length. Finishing
//! checks the whole file's SHA-256 against the browser's, then renames it
//! into place, so a file appears whole or not at all. The Agent remembers
//! its partial files on disk and deletes those untouched for a day.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use meshrmm_protocol::{FileError, FileErrorCode, FileReply, valid_sha256_hex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::listing::entry_at;
use super::operations::{create_parents, format_bytes};
use super::{Account, io_error, paths, platform};

/// Partial files untouched this long are deleted.
const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// Space a volume keeps free after an upload.
const FREE_SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const RECORD_FILE: &str = "file-uploads.json";

/// The record of partial files, kept one change at a time.
static RECORD: Mutex<()> = Mutex::new(());

/// Upload IDs go in file names, so they must be plain.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Where `path`'s upload `id` collects, and the checked target.
pub(super) fn partial(id: &str, text: &str) -> Result<(PathBuf, PathBuf), FileError> {
    if !valid_id(id) {
        return Err(FileError::new(
            FileErrorCode::Failed,
            "The upload has an invalid ID.",
        ));
    }
    let path = paths::parse(text)?;
    paths::check_name(&paths::name(&path))?;
    let folder = paths::parent(&path).ok_or_else(|| {
        FileError::new(
            FileErrorCode::InvalidPath,
            "Choose a folder to upload into.",
        )
    })?;
    Ok((folder.join(format!(".meshrmm-upload-{id}")), path))
}

fn gone(path: &Path) -> FileError {
    FileError::new(
        FileErrorCode::NotFound,
        format!(
            "The device no longer has the upload of {}; start it again.",
            path.display()
        ),
    )
}

pub(super) fn prepare(
    account: &Account,
    id: &str,
    text: &str,
    size: u64,
    overwrite: bool,
) -> Result<FileReply, FileError> {
    let (partial_path, path) = partial(id, text)?;
    let folder = paths::parent(&path).expect("checked by partial");
    account.run(|| {
        create_parents(account, folder)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(FileError::new(
                    FileErrorCode::IsADirectory,
                    format!("{} is a folder.", path.display()),
                ));
            }
            Ok(_) if !overwrite => {
                return Err(io_error(&io::ErrorKind::AlreadyExists.into(), &path));
            }
            _ => {}
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&partial_path)
            .map_err(|error| io_error(&error, &path))?;
        let mut received = file
            .metadata()
            .map_err(|error| io_error(&error, &path))?
            .len();
        if received > size {
            file.set_len(0).map_err(|error| io_error(&error, &path))?;
            received = 0;
        }
        let free = meshrmm_file_transfer::available_space(folder)
            .map_err(|error| io_error(&error, folder))?;
        if free < (size - received).saturating_add(FREE_SPACE_RESERVE_BYTES) {
            drop(file);
            if received == 0 {
                let _ = fs::remove_file(&partial_path);
            }
            return Err(FileError::new(
                FileErrorCode::NoSpace,
                format!(
                    "{} has {} free, not enough for {}.",
                    folder.display(),
                    format_bytes(free),
                    format_bytes(size)
                ),
            ));
        }
        platform::hide(&partial_path);
        platform::adopt_parent_owner(account, &partial_path);
        remember(&partial_path);
        Ok(FileReply::Upload { received })
    })
}

pub(super) fn status(account: &Account, id: &str, text: &str) -> Result<FileReply, FileError> {
    let (partial_path, path) = partial(id, text)?;
    account.run(|| match fs::metadata(&partial_path) {
        Ok(metadata) => Ok(FileReply::Upload {
            received: metadata.len(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(gone(&path)),
        Err(error) => Err(io_error(&error, &path)),
    })
}

/// Opens upload `id`'s partial file to write from `offset`, which must be
/// how much it holds, ready to write there.
pub(super) fn open(
    account: &Account,
    id: &str,
    text: &str,
    offset: u64,
    size: u64,
) -> Result<File, FileError> {
    let (partial_path, path) = partial(id, text)?;
    account.run(|| {
        let mut file = match OpenOptions::new().write(true).open(&partial_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(gone(&path)),
            Err(error) => return Err(io_error(&error, &path)),
        };
        let received = file
            .metadata()
            .map_err(|error| io_error(&error, &path))?
            .len();
        if received != offset || offset > size {
            return Err(FileError::new(
                FileErrorCode::OffsetMismatch,
                format!("The device has {received} bytes of this upload, not {offset}."),
            ));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| io_error(&error, &path))?;
        Ok(file)
    })
}

pub(super) fn finish(
    account: &Account,
    id: &str,
    text: &str,
    size: u64,
    sha256: &str,
    overwrite: bool,
) -> Result<FileReply, FileError> {
    if !valid_sha256_hex(sha256) {
        return Err(FileError::new(
            FileErrorCode::Failed,
            "The upload's SHA-256 is invalid.",
        ));
    }
    let (partial_path, path) = partial(id, text)?;
    account.run(|| {
        let mut file = match File::open(&partial_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(gone(&path)),
            Err(error) => return Err(io_error(&error, &path)),
        };
        let received = file.metadata().map_err(|error| io_error(&error, &path))?.len();
        if received != size {
            return Err(FileError::new(
                FileErrorCode::OffsetMismatch,
                format!("The upload is incomplete: the device has {received} of {size} bytes."),
            ));
        }
        let digest = hash(&mut file).map_err(|error| io_error(&error, &path))?;
        drop(file);
        if digest != sha256 {
            let _ = fs::remove_file(&partial_path);
            forget(&partial_path);
            return Err(FileError::new(
                FileErrorCode::ChecksumMismatch,
                format!(
                    "{} arrived different from what was sent (its SHA-256 didn't match), so the device discarded it. Upload it again.",
                    paths::name(&path)
                ),
            ));
        }
        let placed = if overwrite {
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(FileError::new(
                    FileErrorCode::IsADirectory,
                    format!("{} is a folder.", path.display()),
                ));
            }
            fs::rename(&partial_path, &path)
        } else {
            meshrmm_file_transfer::rename_no_replace(&partial_path, &path)
        };
        placed.map_err(|error| io_error(&error, &path))?;
        forget(&partial_path);
        platform::unhide(&path);
        if let Err(error) = meshrmm_file_transfer::mark_received(&path) {
            tracing::debug!(%error, "could not mark an uploaded file as downloaded");
        }
        Ok(FileReply::Entry {
            path: paths::text(&path),
            entry: entry_at(&path)?,
        })
    })
}

pub(super) fn cancel(account: &Account, id: &str, text: &str) -> Result<FileReply, FileError> {
    let (partial_path, path) = partial(id, text)?;
    account.run(|| {
        match fs::remove_file(&partial_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&error, &path)),
        }
        forget(&partial_path);
        Ok(FileReply::Upload { received: 0 })
    })
}

/// The SHA-256 of what's left to read in `file`, in lowercase hex.
pub(super) fn hash(file: &mut impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    partials: Vec<PathBuf>,
}

fn record_path() -> Option<PathBuf> {
    crate::installer::config_directory()
        .ok()
        .map(|folder| folder.join(RECORD_FILE))
}

fn change_record(change: impl FnOnce(&mut Record)) {
    let Some(file) = record_path() else { return };
    let _guard = RECORD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut record = fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .unwrap_or_default();
    change(&mut record);
    let written = serde_json::to_vec(&record)
        .map_err(io::Error::other)
        .and_then(|bytes| fs::write(&file, bytes));
    if let Err(error) = written {
        tracing::debug!(%error, "could not record the device's partial uploads");
    }
}

fn remember(partial: &Path) {
    change_record(|record| {
        if !record.partials.iter().any(|known| known == partial) {
            record.partials.push(partial.to_path_buf());
        }
    });
}

fn forget(partial: &Path) {
    change_record(|record| record.partials.retain(|known| known != partial));
}

/// Deletes partial files nobody has written to for a day, and forgets
/// those already gone.
pub(super) fn sweep() {
    change_record(|record| {
        record.partials.retain(|partial| {
            let Ok(metadata) = fs::symlink_metadata(partial) else {
                return false;
            };
            let idle = metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .unwrap_or_default();
            if idle < ABANDONED_AFTER {
                return true;
            }
            match fs::remove_file(partial) {
                Ok(()) => {
                    tracing::info!(path = %partial.display(), "deleted an abandoned upload");
                    false
                }
                Err(error) => {
                    tracing::warn!(%error, path = %partial.display(), "could not delete an abandoned upload");
                    true
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::ExecutionMode;
    use meshrmm_protocol::RunAs;
    use std::io::Write;

    fn own() -> Account {
        Account::resolve(ExecutionMode::Console, RunAs::System).unwrap()
    }

    #[test]
    fn uploads_resume_verify_and_land_whole() {
        let folder = std::env::temp_dir().join(format!("meshrmm-up-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&folder).unwrap();
        let account = own();
        let target = paths::text(&folder.join("new/sub/data.bin"));
        let id = "0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f70";
        let body = b"hello, device".to_vec();
        let digest = hex(&Sha256::digest(&body));
        assert_eq!(
            prepare(&account, id, &target, body.len() as u64, false).unwrap(),
            FileReply::Upload { received: 0 }
        );
        assert!(folder.join("new/sub").is_dir(), "missing folders are made");
        // Half, then an interrupted transfer, then the rest.
        open(&account, id, &target, 0, body.len() as u64)
            .unwrap()
            .write_all(&body[..5])
            .unwrap();
        assert_eq!(
            open(&account, id, &target, 0, body.len() as u64)
                .unwrap_err()
                .code,
            FileErrorCode::OffsetMismatch
        );
        assert_eq!(
            status(&account, id, &target).unwrap(),
            FileReply::Upload { received: 5 }
        );
        assert_eq!(
            prepare(&account, id, &target, body.len() as u64, false).unwrap(),
            FileReply::Upload { received: 5 },
            "preparing again resumes"
        );
        assert_eq!(
            finish(&account, id, &target, body.len() as u64, &digest, false)
                .unwrap_err()
                .code,
            FileErrorCode::OffsetMismatch
        );
        open(&account, id, &target, 5, body.len() as u64)
            .unwrap()
            .write_all(&body[5..])
            .unwrap();
        let FileReply::Entry { entry, .. } =
            finish(&account, id, &target, body.len() as u64, &digest, false).unwrap()
        else {
            panic!()
        };
        assert_eq!(entry.size, Some(body.len() as u64));
        assert!(!entry.hidden || cfg!(unix) && entry.name.starts_with('.'));
        assert_eq!(fs::read(folder.join("new/sub/data.bin")).unwrap(), body);
        assert!(
            !folder
                .join(format!("new/sub/.meshrmm-upload-{id}"))
                .exists()
        );

        // A file that's there already is kept unless replacing.
        assert_eq!(
            prepare(&account, id, &target, 3, false).unwrap_err().code,
            FileErrorCode::AlreadyExists
        );
        prepare(&account, id, &target, 3, true).unwrap();
        open(&account, id, &target, 0, 3)
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        // A wrong checksum discards the bytes.
        assert_eq!(
            finish(&account, id, &target, 3, &"0".repeat(64), true)
                .unwrap_err()
                .code,
            FileErrorCode::ChecksumMismatch
        );
        assert_eq!(
            status(&account, id, &target).unwrap_err().code,
            FileErrorCode::NotFound
        );
        assert_eq!(fs::read(folder.join("new/sub/data.bin")).unwrap(), body);
        prepare(&account, id, &target, 3, true).unwrap();
        cancel(&account, id, &target).unwrap();
        assert_eq!(
            status(&account, id, &target).unwrap_err().code,
            FileErrorCode::NotFound
        );
        assert!(partial("../x", &target).is_err());
        let _ = fs::remove_dir_all(&folder);
    }
}
