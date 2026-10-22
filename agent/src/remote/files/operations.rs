//! Changes to files: new folders, renames, moves and deletes, and the
//! checks before a download. None replaces an existing entry, and none
//! follows a link inside a folder it deletes or copies.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use meshrmm_protocol::{
    FileBatch, FileDownloadInfo, FileDownloadKind, FileError, FileErrorCode, FileFailure,
    FileReply, MAX_FILE_BATCH,
};

use super::listing::entry_at;
use super::{Account, io_error, paths, platform, unix_ms};

fn entry_reply(path: &Path) -> Result<FileReply, FileError> {
    Ok(FileReply::Entry {
        path: paths::text(path),
        entry: entry_at(path)?,
    })
}

fn batch_paths(texts: &[String]) -> Result<(), FileError> {
    if texts.is_empty() || texts.len() > MAX_FILE_BATCH {
        return Err(FileError::new(
            FileErrorCode::Failed,
            format!("Choose 1 to {MAX_FILE_BATCH} entries."),
        ));
    }
    Ok(())
}

/// Moves `from` to `to` unless something is there already.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match meshrmm_file_transfer::rename_no_replace(from, to) {
        // Some network volumes lack an atomic no-replace rename.
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            if to.try_exists()? {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        result => result,
    }
}

pub(super) fn create_folder(account: &Account, text: &str) -> Result<FileReply, FileError> {
    let path = paths::parse(text)?;
    paths::check_name(&paths::name(&path))?;
    account.run(|| {
        fs::create_dir(&path).map_err(|error| io_error(&error, &path))?;
        platform::adopt_parent_owner(account, &path);
        entry_reply(&path)
    })
}

/// Creates `folder` and the folders above it that are missing.
pub(super) fn create_parents(account: &Account, folder: &Path) -> Result<(), FileError> {
    let mut missing = Vec::new();
    let mut current = folder;
    loop {
        match fs::metadata(current) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(FileError::new(
                    FileErrorCode::NotADirectory,
                    format!("{} is a file, not a folder.", current.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                match current.parent() {
                    Some(parent) => current = parent,
                    None => return Err(io_error(&error, current)),
                }
            }
            Err(error) => return Err(io_error(&error, current)),
        }
    }
    for folder in missing.into_iter().rev() {
        paths::check_name(&paths::name(&folder))?;
        match fs::create_dir(&folder) {
            Ok(()) => platform::adopt_parent_owner(account, &folder),
            // Another upload into the same new folder made it first.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && folder.is_dir() => {}
            Err(error) => return Err(io_error(&error, &folder)),
        }
    }
    Ok(())
}

pub(super) fn rename(account: &Account, text: &str, name: &str) -> Result<FileReply, FileError> {
    let path = paths::parse(text)?;
    let folder = paths::parent(&path).ok_or_else(|| {
        FileError::new(
            FileErrorCode::InvalidPath,
            "A drive or `/` can't be renamed.",
        )
    })?;
    let target = paths::child(folder, name)?;
    if target == path {
        return account.run(|| entry_reply(&path));
    }
    account.run(|| {
        let renamed = if platform::same_file(&path, &target) {
            // Only the case changes, on a volume that ignores it.
            fs::rename(&path, &target)
        } else {
            rename_no_replace(&path, &target)
        };
        renamed.map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => io_error(&error, &target),
            _ => io_error(&error, &path),
        })?;
        entry_reply(&target)
    })
}

pub(super) fn move_all(
    account: &Account,
    texts: &[String],
    destination: &str,
) -> Result<FileBatch, FileError> {
    batch_paths(texts)?;
    let destination = paths::parse(destination)?;
    account.run(|| {
        let metadata =
            fs::metadata(&destination).map_err(|error| io_error(&error, &destination))?;
        if !metadata.is_dir() {
            return Err(FileError::new(
                FileErrorCode::NotADirectory,
                format!("{} isn't a folder.", destination.display()),
            ));
        }
        let real_destination = fs::canonicalize(&destination).unwrap_or(destination.clone());
        let mut batch = FileBatch::default();
        for text in texts {
            match move_one(account, text, &destination, &real_destination) {
                Ok(()) => batch.done.push(text.clone()),
                Err(error) => batch.failed.push(FileFailure {
                    path: text.clone(),
                    error: error.sanitized(),
                }),
            }
        }
        Ok(batch)
    })
}

fn move_one(
    account: &Account,
    text: &str,
    destination: &Path,
    real_destination: &Path,
) -> Result<(), FileError> {
    let path = paths::parse(text)?;
    let Some(name) = path.file_name() else {
        return Err(FileError::new(
            FileErrorCode::InvalidMove,
            "A drive or `/` can't be moved.",
        ));
    };
    let target = destination.join(name);
    if paths::parent(&path) == Some(destination) {
        return Err(FileError::new(
            FileErrorCode::InvalidMove,
            format!("{} is in that folder already.", path.display()),
        ));
    }
    let real = fs::canonicalize(&path).map_err(|error| io_error(&error, &path))?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&error, &path))?;
    if metadata.is_dir() && (destination.starts_with(&path) || real_destination.starts_with(&real))
    {
        return Err(FileError::new(
            FileErrorCode::InvalidMove,
            format!("{} can't go into a folder inside itself.", path.display()),
        ));
    }
    match rename_no_replace(&path, &target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            // Another volume: copy, then delete the original once the copy
            // is whole. A failed copy is removed and the original kept.
            if target.symlink_metadata().is_ok() {
                return Err(io_error(&io::ErrorKind::AlreadyExists.into(), &target));
            }
            if let Err(error) = copy_tree(account, &path, &target) {
                let _ = delete_tree(&target);
                return Err(error);
            }
            delete_tree(&path)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(io_error(&error, &target))
        }
        Err(error) => Err(io_error(&error, &path)),
    }
}

/// Copies `source` to `target`, a folder with everything in it. Links are
/// copied as links, never followed.
fn copy_tree(account: &Account, source: &Path, target: &Path) -> Result<(), FileError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| io_error(&error, source))?;
    if metadata.file_type().is_symlink() {
        return platform::copy_link(source, target);
    }
    if metadata.is_dir() {
        fs::create_dir(target).map_err(|error| io_error(&error, target))?;
        platform::adopt_parent_owner(account, target);
        for item in fs::read_dir(source).map_err(|error| io_error(&error, source))? {
            let item = item.map_err(|error| io_error(&error, source))?;
            copy_tree(account, &item.path(), &target.join(item.file_name()))?;
        }
        return Ok(());
    }
    // Never replaces: the target was checked, and is new.
    let mut from = fs::File::open(source).map_err(|error| io_error(&error, source))?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|error| io_error(&error, target))?;
    io::copy(&mut from, &mut to).map_err(|error| io_error(&error, target))?;
    let _ = to.set_permissions(metadata.permissions());
    if let Ok(modified) = metadata.modified() {
        let _ = to.set_modified(modified);
    }
    platform::adopt_parent_owner(account, target);
    Ok(())
}

pub(super) fn delete_all(
    account: &Account,
    texts: &[String],
    trash: bool,
) -> Result<FileBatch, FileError> {
    batch_paths(texts)?;
    if trash && !account.is_user() {
        return Err(FileError::new(
            FileErrorCode::TrashUnavailable,
            "SYSTEM and root have no Recycle Bin or Trash; delete permanently instead.",
        ));
    }
    account.run(|| {
        let mut batch = FileBatch::default();
        for text in texts {
            let deleted = paths::parse(text).and_then(|path| {
                if paths::parent(&path).is_none() {
                    return Err(FileError::new(
                        FileErrorCode::InvalidPath,
                        "A drive or `/` can't be deleted.",
                    ));
                }
                if trash {
                    platform::trash(account, &path)
                } else {
                    delete_tree(&path)
                }
            });
            match deleted {
                Ok(()) => batch.done.push(text.clone()),
                Err(error) => batch.failed.push(FileFailure {
                    path: text.clone(),
                    error: error.sanitized(),
                }),
            }
        }
        Ok(batch)
    })
}

/// Deletes `path` for good: a link itself, never what it points to; a
/// folder with everything in it.
pub(super) fn delete_tree(path: &Path) -> Result<(), FileError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(&error, path))?;
    let removed = if metadata.file_type().is_symlink() {
        platform::remove_link(path, &metadata)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        platform::remove_file(path)
    };
    removed.map_err(|error| io_error(&error, path))
}

/// What downloading `texts` will send: one file as itself, or a folder or
/// several entries as an archive. Checks that the account can read it and
/// that it fits in `max_bytes`.
pub(super) fn prepare_download(
    account: &Account,
    texts: &[String],
    max_bytes: u64,
) -> Result<FileDownloadInfo, FileError> {
    batch_paths(texts)?;
    let paths = texts
        .iter()
        .map(|text| paths::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    account.run(|| {
        if let [path] = paths.as_slice() {
            let metadata = fs::metadata(path).map_err(|error| io_error(&error, path))?;
            if metadata.is_file() {
                // Opening it is the only sure test of access.
                fs::File::open(path).map_err(|error| io_error(&error, path))?;
                too_large(path, metadata.len(), max_bytes)?;
                return Ok(FileDownloadInfo {
                    name: paths::name(path),
                    kind: FileDownloadKind::File,
                    size: metadata.len(),
                    modified: metadata.modified().ok().and_then(unix_ms),
                    files: 1,
                });
            }
            if !metadata.is_dir() {
                return Err(FileError::new(
                    FileErrorCode::Failed,
                    format!("{} isn't a file or a folder.", path.display()),
                ));
            }
        }
        let mut totals = Totals::default();
        for path in &paths {
            totals.add(path, max_bytes)?;
        }
        let name = match paths.as_slice() {
            [path] => paths::name(path),
            [first, ..] => paths::parent(first)
                .map(paths::name)
                .unwrap_or_else(|| "Files".into()),
            [] => unreachable!("checked above"),
        };
        Ok(FileDownloadInfo {
            name,
            kind: FileDownloadKind::Archive,
            size: totals.bytes,
            modified: None,
            files: totals.files,
        })
    })
}

#[derive(Default)]
struct Totals {
    bytes: u64,
    files: u64,
}

impl Totals {
    /// Adds what an archive of `top` would hold. Entries inside it that
    /// can't be read are left out, as the archive leaves them; `top` itself
    /// must be readable.
    fn add(&mut self, top: &Path, max_bytes: u64) -> Result<(), FileError> {
        let metadata = fs::metadata(top).map_err(|error| io_error(&error, top))?;
        if metadata.is_file() {
            self.files += 1;
            self.bytes += metadata.len();
            return too_large(top, self.bytes, max_bytes);
        }
        fs::read_dir(top).map_err(|error| io_error(&error, top))?;
        let mut pending: Vec<PathBuf> = vec![top.to_path_buf()];
        while let Some(folder) = pending.pop() {
            let Ok(listing) = fs::read_dir(&folder) else {
                continue;
            };
            for item in listing.flatten() {
                let Ok(metadata) = item.metadata() else {
                    continue;
                };
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    pending.push(item.path());
                } else if metadata.is_file() {
                    self.files += 1;
                    self.bytes += metadata.len();
                    too_large(top, self.bytes, max_bytes)?;
                }
            }
        }
        Ok(())
    }
}

fn too_large(path: &Path, bytes: u64, max_bytes: u64) -> Result<(), FileError> {
    if bytes <= max_bytes {
        return Ok(());
    }
    Err(FileError::new(
        FileErrorCode::TooLarge,
        format!(
            "{} is over this server's {} transfer limit.",
            path.display(),
            format_bytes(max_bytes)
        ),
    ))
}

/// `bytes` in binary units, as people read sizes: `4 GB`, `512 MB`.
pub(super) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value.fract() == 0.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::ExecutionMode;
    use meshrmm_protocol::RunAs;

    struct Folder(PathBuf);

    impl Folder {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("meshrmm-ops-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn text(&self, name: &str) -> String {
            paths::text(&self.0.join(name))
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn own() -> Account {
        Account::resolve(ExecutionMode::Console, RunAs::System).unwrap()
    }

    fn code(result: Result<FileReply, FileError>) -> FileErrorCode {
        result.unwrap_err().code
    }

    #[test]
    fn folders_are_created_and_entries_renamed_without_replacing() {
        let folder = Folder::new();
        let account = own();
        let FileReply::Entry { entry, .. } = create_folder(&account, &folder.text("new")).unwrap()
        else {
            panic!()
        };
        assert_eq!(entry.kind, meshrmm_protocol::FileKind::Directory);
        assert_eq!(
            code(create_folder(&account, &folder.text("new"))),
            FileErrorCode::AlreadyExists
        );
        assert_eq!(
            code(create_folder(&account, &folder.text("missing/new"))),
            FileErrorCode::NotFound
        );
        fs::write(folder.0.join("taken"), b"keep").unwrap();
        assert_eq!(
            code(rename(&account, &folder.text("new"), "taken")),
            FileErrorCode::AlreadyExists
        );
        assert_eq!(fs::read(folder.0.join("taken")).unwrap(), b"keep");
        assert_eq!(
            code(rename(&account, &folder.text("new"), "a/b")),
            FileErrorCode::InvalidName
        );
        let FileReply::Entry { path, .. } =
            rename(&account, &folder.text("new"), "renamed").unwrap()
        else {
            panic!()
        };
        assert_eq!(path, folder.text("renamed"));
        assert!(folder.0.join("renamed").is_dir());
        // Only the case.
        rename(&account, &folder.text("renamed"), "Renamed").unwrap();
        assert!(
            fs::read_dir(&folder.0)
                .unwrap()
                .any(|item| item.unwrap().file_name() == "Renamed")
        );
    }

    #[test]
    fn moves_report_each_entry_and_refuse_loops() {
        let folder = Folder::new();
        let account = own();
        fs::create_dir_all(folder.0.join("a/inner")).unwrap();
        fs::create_dir(folder.0.join("b")).unwrap();
        fs::write(folder.0.join("file.txt"), b"x").unwrap();
        fs::write(folder.0.join("b/file.txt"), b"theirs").unwrap();
        let batch = move_all(
            &account,
            &[
                folder.text("a"),
                folder.text("file.txt"),
                folder.text("ghost"),
            ],
            &folder.text("b"),
        )
        .unwrap();
        assert_eq!(batch.done, [folder.text("a")]);
        let codes = batch
            .failed
            .iter()
            .map(|f| f.error.code)
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [FileErrorCode::AlreadyExists, FileErrorCode::NotFound]
        );
        assert!(folder.0.join("b/a/inner").is_dir());
        assert_eq!(fs::read(folder.0.join("b/file.txt")).unwrap(), b"theirs");
        let looped = move_all(&account, &[folder.text("b")], &folder.text("b/a/inner")).unwrap();
        assert_eq!(looped.failed[0].error.code, FileErrorCode::InvalidMove);
        assert_eq!(
            code(
                move_all(&account, &[folder.text("b")], &folder.text("file.txt"))
                    .map(FileReply::Batch)
            ),
            FileErrorCode::NotADirectory
        );
    }

    #[test]
    fn deletes_remove_trees_but_not_link_targets() {
        let folder = Folder::new();
        let account = own();
        fs::create_dir_all(folder.0.join("tree/deep")).unwrap();
        fs::write(folder.0.join("tree/deep/f"), b"x").unwrap();
        fs::create_dir(folder.0.join("kept")).unwrap();
        fs::write(folder.0.join("kept/precious"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(folder.0.join("kept"), folder.0.join("link")).unwrap();
        let mut targets = vec![folder.text("tree")];
        if cfg!(unix) {
            targets.push(folder.text("link"));
        }
        let batch = delete_all(&account, &targets, false).unwrap();
        assert!(batch.failed.is_empty(), "{batch:?}");
        assert!(!folder.0.join("tree").exists());
        assert!(folder.0.join("kept/precious").exists());
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let refused = delete_all(&account, &[root.to_owned()], false).unwrap();
        assert_eq!(refused.failed[0].error.code, FileErrorCode::InvalidPath);
        assert_eq!(
            code(delete_all(&account, &[folder.text("kept")], true).map(FileReply::Batch)),
            FileErrorCode::TrashUnavailable,
            "the Agent's own account has no Trash"
        );
    }

    #[test]
    fn downloads_are_checked_and_measured() {
        let folder = Folder::new();
        let account = own();
        fs::create_dir_all(folder.0.join("dir/sub")).unwrap();
        fs::write(folder.0.join("dir/a"), vec![0; 100]).unwrap();
        fs::write(folder.0.join("dir/sub/b"), vec![0; 50]).unwrap();
        fs::write(folder.0.join("one.bin"), vec![0; 10]).unwrap();
        let file = prepare_download(&account, &[folder.text("one.bin")], 1000).unwrap();
        assert_eq!(
            (file.kind, file.size, file.name.as_str()),
            (FileDownloadKind::File, 10, "one.bin")
        );
        let archive = prepare_download(&account, &[folder.text("dir")], 1000).unwrap();
        assert_eq!(
            (archive.kind, archive.size, archive.files),
            (FileDownloadKind::Archive, 150, 2)
        );
        assert_eq!(archive.name, "dir");
        let both = prepare_download(
            &account,
            &[folder.text("dir"), folder.text("one.bin")],
            1000,
        )
        .unwrap();
        assert_eq!((both.size, both.files), (160, 3));
        assert_eq!(
            prepare_download(&account, &[folder.text("dir")], 149)
                .unwrap_err()
                .code,
            FileErrorCode::TooLarge
        );
        assert_eq!(
            prepare_download(&account, &[folder.text("nothing")], 10)
                .unwrap_err()
                .code,
            FileErrorCode::NotFound
        );
        assert_eq!(format_bytes(4 * 1024 * 1024 * 1024), "4 GB");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }
}
