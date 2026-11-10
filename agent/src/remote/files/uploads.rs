//! Uploads: bytes collect in a hidden partial file beside where the file
//! goes, `.meshrmm-upload-<id>`, across as many transfers as it takes. An
//! interrupted upload goes on from the partial file's length. Finishing
//! checks the whole file's SHA-256 against the browser's, then renames it
//! into place, so a file appears whole or not at all.
//!
//! The Agent keeps a record of its partial files in its configuration
//! directory and deletes those untouched for a day. Only the Agent's own
//! account can reach that directory, so the record always changes as the
//! Agent, outside [`Account::run`], and a partial file is recorded before it
//! is made: one the Agent can't record isn't made. An entry remembers the
//! signed-in user a partial file was made as, for the sweep to reach it as
//! them when the Agent's own account can't, as on a user's network drive.
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{FileError, FileErrorCode, FileReply, RunAs, valid_sha256_hex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::listing::entry_at;
use super::operations::{create_parents, format_bytes};
use super::{Account, io_error, paths, platform};
use crate::remote::config::ExecutionMode;

/// Partial files untouched this long are deleted.
const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// A recorded partial file the sweep can't examine this long, as on a
/// network drive that's gone, is forgotten.
const UNREACHABLE_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Most partial files the record holds; uploads beyond them don't start.
const MAX_PARTIALS: usize = 1000;
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
    // Recorded first, as the Agent, so the partial file is never there
    // unrecorded; settling forgets it again if it isn't made.
    remember(&partial_path, account.user_id())?;
    settle(account, &partial_path, || {
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
        let file = open_partial(
            OpenOptions::new().write(true).create(true).truncate(false),
            &partial_path,
        )
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
        let mut file = match open_partial(OpenOptions::new().write(true), &partial_path) {
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
    settle(account, &partial_path, || {
        let mut file = match open_partial(OpenOptions::new().read(true), &partial_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(gone(&path)),
            Err(error) => return Err(io_error(&error, &path)),
        };
        let received = file
            .metadata()
            .map_err(|error| io_error(&error, &path))?
            .len();
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
    settle(account, &partial_path, || {
        match fs::remove_file(&partial_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&error, &path)),
        }
        Ok(FileReply::Upload { received: 0 })
    })
}

/// Runs `work` on an upload as the account, then, as the Agent, forgets its
/// partial file if it's no longer there: placed, discarded or never made.
fn settle<T: Send>(
    account: &Account,
    partial: &Path,
    work: impl FnOnce() -> Result<T, FileError> + Send,
) -> Result<T, FileError> {
    let (result, there) = account.run(|| {
        let result = work();
        let there = !fs::symlink_metadata(partial)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
        Ok((result, there))
    })?;
    if !there {
        forget(partial);
    }
    result
}

/// Opens an upload's partial file, which must be a plain file of its own:
/// never a link, and never a second name of another file. It sits in a
/// folder that may be a user's, so as root the user could otherwise swap it
/// for a link to a system file for root to write into.
fn open_partial(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    let file = platform::open_plain(options, path)?;
    if !platform::plain_file(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the upload's partial file was replaced by a link",
        ));
    }
    Ok(file)
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
    partials: Vec<Partial>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Partial {
    path: PathBuf,
    /// The signed-in user it was made as ([`Account::user_id`]); none for
    /// the Agent's own account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    /// When it was last known to be in use, in seconds since the Unix
    /// epoch: when it was recorded, or found recently written to.
    seen: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn record_path() -> Option<PathBuf> {
    #[cfg(test)]
    return tests::RECORD_AT.with_borrow(Clone::clone);
    #[cfg(not(test))]
    crate::installer::config_directory()
        .ok()
        .map(|folder| folder.join(RECORD_FILE))
}

/// Changes the record, as the Agent. A record that can't be read is left
/// as it is; one that isn't a record is started again.
fn change_record<T>(change: impl FnOnce(&mut Record) -> io::Result<T>) -> io::Result<T> {
    let changed = (|| {
        #[cfg(test)]
        if super::account::working::now() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the record isn't the account's to change",
            ));
        }
        let file = record_path()
            .ok_or_else(|| io::Error::other("the Agent has no configuration directory"))?;
        let _guard = RECORD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut record = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "the record of partial uploads was unreadable; starting it again");
                Record::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Record::default(),
            Err(error) => return Err(error),
        };
        let result = change(&mut record)?;
        // Written whole, then renamed into place, so stopping halfway
        // loses nothing.
        let mut staged = file.clone().into_os_string();
        staged.push(".new");
        let staged = PathBuf::from(staged);
        fs::write(
            &staged,
            serde_json::to_vec(&record).map_err(io::Error::other)?,
        )?;
        fs::rename(&staged, &file)?;
        Ok(result)
    })();
    if let Err(error) = &changed {
        tracing::warn!(%error, "could not change the record of partial uploads");
    }
    changed
}

/// Records the partial file `partial`, made as `user`, before it's made.
/// An upload whose partial file can't be recorded doesn't start, since the
/// file would stay on the disk for good if the upload were abandoned. The
/// record is in the Agent's own configuration directory, so wherever the
/// Agent runs it can be written but for a full or failing disk.
fn remember(partial: &Path, user: Option<String>) -> Result<(), FileError> {
    change_record(|record| {
        let entry = Partial {
            path: partial.to_path_buf(),
            user,
            seen: now(),
        };
        let full = record.partials.len() >= MAX_PARTIALS;
        match record.partials.iter_mut().find(|known| known.path == partial) {
            Some(known) => *known = entry,
            None if full => return Err(io::Error::new(io::ErrorKind::QuotaExceeded, "full")),
            None => record.partials.push(entry),
        }
        Ok(())
    })
    .map_err(|error| {
        let message = if error.kind() == io::ErrorKind::QuotaExceeded {
            format!(
                "The device has {MAX_PARTIALS} unfinished uploads already. Finish or cancel some first; the device removes those abandoned for a day."
            )
        } else {
            "The Agent could not record the upload's partial file, so it didn't start it.".into()
        };
        FileError::new(FileErrorCode::Failed, message)
    })
}

/// Forgets a partial file that's gone. One that can't be forgotten now
/// is forgotten by the sweep, which finds it gone.
fn forget(partial: &Path) {
    let _ = change_record(|record| {
        record.partials.retain(|known| known.path != partial);
        Ok(())
    });
}

/// What the sweep found of a recorded partial file.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Found {
    /// Written to within a day.
    InUse,
    /// Untouched for a day, and deleted.
    Deleted,
    /// Not there.
    Gone,
    /// Something other than a plain file, which someone else put there:
    /// left as it is.
    Replaced,
    /// Couldn't be examined or deleted now.
    Unreachable,
}

/// Looks at a recorded partial file, and deletes it if it's untouched for
/// a day. Nothing is followed: the partial file is examined and deleted
/// itself, and anything but a plain file in its place is left alone:
/// neither a link put there nor what it points to is deleted. Its folder
/// may be a user's, who could swap the folder for a link to another one,
/// but only a file with this upload's partial file's name could be deleted
/// there, and only the Agent makes those.
fn examine(partial: &Path) -> Found {
    let metadata = match look(partial) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Found::Gone,
        Err(error) => {
            tracing::info!(%error, path = %partial.display(), "could not examine a partial upload; keeping it");
            return Found::Unreachable;
        }
    };
    if !metadata.is_file() {
        tracing::warn!(path = %partial.display(), "a partial upload was replaced by something else; leaving it");
        return Found::Replaced;
    }
    let idle = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or_default();
    if idle < ABANDONED_AFTER {
        return Found::InUse;
    }
    match fs::remove_file(partial) {
        Ok(()) => {
            tracing::info!(path = %partial.display(), "deleted an abandoned upload");
            Found::Deleted
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Found::Gone,
        Err(error) => {
            tracing::warn!(%error, path = %partial.display(), "could not delete an abandoned upload");
            Found::Unreachable
        }
    }
}

/// The partial file's own metadata, not what a link there points to.
fn look(partial: &Path) -> io::Result<fs::Metadata> {
    #[cfg(test)]
    if tests::UNREACHABLE.with_borrow(|unreachable| unreachable.iter().any(|path| path == partial))
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    fs::symlink_metadata(partial)
}

/// Deletes partial files nobody has written to for a day, and forgets
/// those gone. One made as the signed-in user is examined as them while
/// they're signed in, since it may be where only they reach (their network
/// drive, a home folder root can't read), and as the Agent otherwise. One
/// that can't be examined is kept until it has been out of reach for 30
/// days. `mode` is the Agent's, once a file request has told it.
pub(super) fn sweep(mode: Option<ExecutionMode>) {
    let Ok(partials) = change_record(|record| Ok(record.partials.clone())) else {
        return;
    };
    if partials.is_empty() {
        return;
    }
    let signed_in = mode
        .and_then(|mode| Account::resolve(mode, RunAs::User).ok())
        .filter(Account::is_user);
    let user = signed_in.as_ref().and_then(Account::user_id);
    let (theirs, ours): (Vec<&Partial>, Vec<&Partial>) = partials
        .iter()
        .partition(|partial| partial.user.is_some() && partial.user == user);
    let mut found = ours
        .iter()
        .map(|partial| (&partial.path, (partial.seen, examine(&partial.path))))
        .collect::<HashMap<_, _>>();
    if let Some(account) = signed_in.filter(|_| !theirs.is_empty()) {
        let examined = account
            .run(|| {
                Ok(theirs
                    .iter()
                    .map(|partial| examine(&partial.path))
                    .collect())
            })
            .unwrap_or_else(|_| vec![Found::Unreachable; theirs.len()]);
        for (partial, examined) in theirs.iter().zip(examined) {
            found.insert(&partial.path, (partial.seen, examined));
        }
    }
    let now = now();
    let since = |seen: u64| Duration::from_secs(now.saturating_sub(seen));
    let _ = change_record(|record| {
        record.partials.retain_mut(|partial| {
            // An entry recorded again since it was examined stays as it is.
            let Some(&(seen, examined)) = found.get(&partial.path) else {
                return true;
            };
            if partial.seen != seen {
                return true;
            }
            match examined {
                Found::InUse => {
                    partial.seen = now;
                    true
                }
                Found::Deleted | Found::Replaced => false,
                // One not there yet may be one an upload is making.
                Found::Gone => since(seen) < ABANDONED_AFTER,
                Found::Unreachable if since(seen) < UNREACHABLE_FOR => true,
                Found::Unreachable => {
                    tracing::warn!(path = %partial.path.display(), "forgot a partial upload out of reach for 30 days");
                    false
                }
            }
        });
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;

    thread_local! {
        /// Where the test's record of partial files is kept.
        pub(super) static RECORD_AT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        /// Partial files the test's sweep can't examine, as on a network
        /// drive that's gone.
        pub(super) static UNREACHABLE: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    fn own() -> Account {
        Account::resolve(ExecutionMode::Console, RunAs::System).unwrap()
    }

    /// A folder of the test's own, which keeps its record.
    fn scratch() -> PathBuf {
        let folder = std::env::temp_dir().join(format!("meshrmm-up-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&folder).unwrap();
        RECORD_AT.set(Some(folder.join(RECORD_FILE)));
        folder
    }

    fn recorded() -> Vec<PathBuf> {
        change_record(|record| Ok(record.partials.iter().map(|p| p.path.clone()).collect()))
            .unwrap()
    }

    /// Moves when the record last knew `partial` in use to `ago`.
    fn seen_ago(partial: &Path, ago: Duration) {
        change_record(|record| {
            for known in &mut record.partials {
                if known.path == partial {
                    known.seen = now() - ago.as_secs();
                }
            }
            Ok(())
        })
        .unwrap();
    }

    fn untouched_for(path: &Path, ago: Duration) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - ago)
            .unwrap();
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn uploads_resume_verify_and_land_whole() {
        let folder = scratch();
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

    /// A partial file swapped for a link to another file, or for a second
    /// name of one, is refused rather than written through.
    #[cfg(unix)]
    #[test]
    fn partial_files_swapped_for_links_are_refused() {
        let folder = scratch();
        let account = own();
        let target = paths::text(&folder.join("data.bin"));
        let id = "0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f71";
        let victim = folder.join("victim");
        fs::write(&victim, b"keep").unwrap();
        prepare(&account, id, &target, 4, false).unwrap();
        let (partial_path, _) = partial(id, &target).unwrap();
        fs::remove_file(&partial_path).unwrap();
        std::os::unix::fs::symlink(&victim, &partial_path).unwrap();
        assert!(open(&account, id, &target, 0, 4).is_err());
        assert!(prepare(&account, id, &target, 4, false).is_err());
        fs::remove_file(&partial_path).unwrap();
        fs::hard_link(&victim, &partial_path).unwrap();
        assert!(open(&account, id, &target, 0, 4).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        let _ = fs::remove_dir_all(&folder);
    }

    /// The record changes as the Agent, never as the account: as the
    /// signed-in user, the Agent's configuration directory is out of reach,
    /// which `account::working` stands for here.
    #[test]
    fn uploads_are_recorded_and_forgotten_as_the_agent() {
        let folder = scratch();
        let account = own();
        let target = paths::text(&folder.join("data.bin"));
        let id = "0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f72";
        let (partial_path, _) = partial(id, &target).unwrap();
        let digest = hex(&Sha256::digest(b"abc"));

        prepare(&account, id, &target, 3, false).unwrap();
        assert_eq!(recorded(), [partial_path.as_path()]);
        // A failure that keeps the partial file keeps its entry.
        open(&account, id, &target, 0, 3)
            .unwrap()
            .write_all(b"a")
            .unwrap();
        finish(&account, id, &target, 3, &digest, false).unwrap_err();
        assert_eq!(recorded(), [partial_path.as_path()]);
        open(&account, id, &target, 1, 3)
            .unwrap()
            .write_all(b"bc")
            .unwrap();
        finish(&account, id, &target, 3, &digest, false).unwrap();
        assert!(recorded().is_empty(), "finishing forgets it");

        prepare(&account, id, &target, 3, true).unwrap();
        assert_eq!(recorded(), [partial_path.as_path()]);
        cancel(&account, id, &target).unwrap();
        assert!(recorded().is_empty(), "cancelling forgets it");

        prepare(&account, id, &target, 3, true).unwrap();
        open(&account, id, &target, 0, 3)
            .unwrap()
            .write_all(b"xyz")
            .unwrap();
        finish(&account, id, &target, 3, &digest, true).unwrap_err();
        assert!(recorded().is_empty(), "a discarded upload is forgotten");

        // Recorded before it's made, and forgotten when it isn't made.
        assert_eq!(
            prepare(&account, id, &target, 3, false).unwrap_err().code,
            FileErrorCode::AlreadyExists
        );
        assert!(recorded().is_empty());
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_sweep_deletes_abandoned_partial_files_and_keeps_the_rest() {
        let folder = scratch();
        let account = own();
        let start = |id: &str| {
            let target = paths::text(&folder.join(format!("{id}.bin")));
            prepare(&account, id, &target, 3, false).unwrap();
            partial(id, &target).unwrap().0
        };
        let abandoned = start("0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f73");
        let in_use = start("0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f74");
        let out_of_reach = start("0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f75");
        untouched_for(&abandoned, DAY + Duration::from_secs(60));
        untouched_for(&out_of_reach, 2 * DAY);
        UNREACHABLE.with_borrow_mut(|unreachable| unreachable.push(out_of_reach.clone()));

        sweep(None);
        assert!(!abandoned.exists(), "untouched for a day, it's deleted");
        assert!(in_use.exists());
        assert!(out_of_reach.exists());
        assert_eq!(recorded(), [in_use.as_path(), out_of_reach.as_path()]);

        // Out of reach for 30 days, it's forgotten.
        seen_ago(&out_of_reach, UNREACHABLE_FOR - Duration::from_secs(60));
        sweep(None);
        assert_eq!(recorded(), [in_use.as_path(), out_of_reach.as_path()]);
        seen_ago(&out_of_reach, UNREACHABLE_FOR + Duration::from_secs(60));
        sweep(None);
        assert_eq!(recorded(), [in_use.as_path()]);

        // One gone is forgotten once it can't be one an upload is making.
        fs::remove_file(&in_use).unwrap();
        sweep(None);
        assert_eq!(recorded(), [in_use.as_path()]);
        seen_ago(&in_use, DAY + Duration::from_secs(60));
        sweep(None);
        assert!(recorded().is_empty());
        let _ = fs::remove_dir_all(&folder);
    }

    /// A link put in a partial file's place is left alone, and so is what
    /// it points to, however long untouched.
    #[cfg(unix)]
    #[test]
    fn the_sweep_leaves_a_link_in_a_partial_files_place_alone() {
        use std::os::unix::ffi::OsStrExt;
        let folder = scratch();
        let account = own();
        let target = paths::text(&folder.join("data.bin"));
        let id = "0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f76";
        prepare(&account, id, &target, 3, false).unwrap();
        let (partial_path, _) = partial(id, &target).unwrap();
        let victim = folder.join("victim");
        fs::write(&victim, b"keep").unwrap();
        untouched_for(&victim, 2 * DAY);
        fs::remove_file(&partial_path).unwrap();
        std::os::unix::fs::symlink(&victim, &partial_path).unwrap();
        let name = std::ffi::CString::new(partial_path.as_os_str().as_bytes()).unwrap();
        let old = libc::timespec {
            tv_sec: (now() - 2 * DAY.as_secs()) as libc::time_t,
            tv_nsec: 0,
        };
        // SAFETY: the name is NUL-terminated and there are two times.
        let aged = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                name.as_ptr(),
                [old, old].as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(aged, 0);

        sweep(None);
        assert!(fs::symlink_metadata(&partial_path).unwrap().is_symlink());
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(recorded().is_empty(), "it's no longer the Agent's");
        let _ = fs::remove_dir_all(&folder);
    }

    /// An upload whose partial file can't be recorded doesn't start: not
    /// beyond the record's cap, nor when the record can't be written.
    #[test]
    fn uploads_the_record_cant_hold_dont_start() {
        let folder = scratch();
        let account = own();
        change_record(|record| {
            record.partials = (0..MAX_PARTIALS)
                .map(|index| Partial {
                    path: folder.join(format!(".meshrmm-upload-{index}")),
                    user: None,
                    seen: now(),
                })
                .collect();
            Ok(())
        })
        .unwrap();
        let target = paths::text(&folder.join("data.bin"));
        let id = "0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f77";
        let (partial_path, _) = partial(id, &target).unwrap();
        assert_eq!(
            prepare(&account, id, &target, 3, false).unwrap_err().code,
            FileErrorCode::Failed
        );
        assert!(!partial_path.exists());
        assert_eq!(recorded().len(), MAX_PARTIALS);
        // One recorded already goes on.
        prepare(
            &account,
            "0",
            &paths::text(&folder.join("zero.bin")),
            3,
            false,
        )
        .unwrap();
        assert_eq!(recorded().len(), MAX_PARTIALS);

        RECORD_AT.set(Some(folder.join("missing").join(RECORD_FILE)));
        assert_eq!(
            prepare(&account, id, &target, 3, false).unwrap_err().code,
            FileErrorCode::Failed
        );
        assert!(!partial_path.exists());
        let _ = fs::remove_dir_all(&folder);
    }
}
