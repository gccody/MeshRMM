//! The file browser on Windows: accounts, attributes, drives, the signed-in
//! user's known folders and the Recycle Bin.
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{
    FileError, FileErrorCode, FilePlace, FilePlaceKind, FileRoot, FileRootKind, RunAs,
};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAGS_AND_ATTRIBUTES, FileTimeToLocalFileTime, GetDiskFreeSpaceExW, GetDriveTypeW,
    GetFileAttributesW, GetLogicalDrives, GetVolumeInformationW, GetVolumePathNameW,
    INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Profile, KNOWN_FOLDER_FLAG,
    SHGetKnownFolderPath,
};
use windows::core::{GUID, PCWSTR};

use super::account::{Account, nobody_signed_in};
use super::{io_error, paths};
use crate::remote::config::ExecutionMode;
use crate::remote::toolbox::windows::{UserLogon, own_account, signed_in_user};
use crate::win32::wide;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;

/// The signed-in user an operation acts as.
pub(super) struct User {
    logon: UserLogon,
}

// SAFETY: the token is only read, by impersonation, while the thread acting
// as the user runs; the Account that owns it outlives that thread.
unsafe impl Sync for User {}

pub(super) fn account(mode: ExecutionMode, run_as: RunAs) -> Result<Account, FileError> {
    match (mode, run_as) {
        // A development Agent has only its own account.
        (ExecutionMode::Console, _) => Ok(Account {
            label: own_account(),
            home: std::env::var_os("USERPROFILE").map(PathBuf::from),
            user: None,
        }),
        (_, RunAs::System) => Ok(Account {
            label: own_account(),
            home: None,
            user: None,
        }),
        (_, RunAs::User) => {
            let logon = signed_in_user(mode).ok_or_else(nobody_signed_in)?;
            Ok(Account {
                label: logon.name.clone(),
                home: Some(logon.profile.clone()),
                user: Some(User { logon }),
            })
        }
    }
}

/// The thread acts as the user until this is dropped.
pub(super) struct Acting;

impl Drop for Acting {
    fn drop(&mut self) {
        if let Err(error) = unsafe { RevertToSelf() } {
            // A thread that can't stop acting as the user must not do
            // anything else as the Agent.
            tracing::error!(%error, "could not stop acting as the signed-in user");
            std::process::abort();
        }
    }
}

pub(super) fn act_as(user: &User) -> io::Result<Acting> {
    unsafe { ImpersonateLoggedOnUser(user.logon.token.0) }.map_err(io::Error::other)?;
    Ok(Acting)
}

pub(super) fn denied(_error: &io::Error, path: &Path) -> String {
    format!("Access to {} is denied.", path.display())
}

pub(super) fn hidden(_name: &str, metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN.0 != 0
}

pub(super) fn system(metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_SYSTEM.0 != 0
}

pub(super) fn read_only(metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_READONLY.0 != 0
}

fn change_attributes(path: &Path, change: impl FnOnce(u32) -> u32) -> io::Result<()> {
    let name = wide(path);
    let current = unsafe { GetFileAttributesW(PCWSTR(name.as_ptr())) };
    if current == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        SetFileAttributesW(
            PCWSTR(name.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(change(current)),
        )
    }
    .map_err(io::Error::other)
}

/// Hides an upload's partial file.
pub(super) fn hide(path: &Path) {
    if let Err(error) = change_attributes(path, |current| current | FILE_ATTRIBUTE_HIDDEN.0) {
        tracing::debug!(%error, "could not hide a partial upload");
    }
}

/// Shows a finished upload, which was hidden while partial.
pub(super) fn unhide(path: &Path) {
    if let Err(error) = change_attributes(path, |current| current & !FILE_ATTRIBUTE_HIDDEN.0) {
        tracing::debug!(%error, "could not show a finished upload");
    }
}

/// New entries inherit their folder's permissions on Windows.
pub(super) fn adopt_parent_owner(_account: &Account, _path: &Path) {}

/// Whether `a` and `b` are the same entry, as when a name differs only in
/// case.
pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Links can't be copied to another drive; the move is undone.
pub(super) fn copy_link(source: &Path, _target: &Path) -> Result<(), FileError> {
    Err(FileError::new(
        FileErrorCode::Failed,
        format!(
            "{} is a link, which can't be moved to another drive.",
            source.display()
        ),
    ))
}

/// Removes a symbolic link or junction itself.
pub(super) fn remove_link(path: &Path, metadata: &Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Removes a file, read-only or not, as Explorer does once it has asked.
pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                && fs::symlink_metadata(path).is_ok_and(|metadata| read_only(&metadata)) =>
        {
            change_attributes(path, |current| current & !FILE_ATTRIBUTE_READONLY.0)?;
            fs::remove_file(path)
        }
        result => result,
    }
}

fn text_of(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// Every drive letter, with its label, kind and space.
pub(super) fn roots() -> Vec<FileRoot> {
    let drives = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|index| drives & (1 << index) != 0)
        .map(|index| {
            let letter = (b'A' + index) as char;
            let path = format!("{letter}:\\");
            let name = wide(&path);
            let kind = match unsafe { GetDriveTypeW(PCWSTR(name.as_ptr())) } {
                DRIVE_FIXED => FileRootKind::Fixed,
                DRIVE_REMOVABLE => FileRootKind::Removable,
                DRIVE_REMOTE => FileRootKind::Network,
                DRIVE_CDROM => FileRootKind::Optical,
                _ => FileRootKind::Other,
            };
            let mut label = [0u16; 261];
            let named = unsafe {
                GetVolumeInformationW(
                    PCWSTR(name.as_ptr()),
                    Some(&mut label),
                    None,
                    None,
                    None,
                    None,
                )
            }
            .is_ok();
            let label = Some(text_of(&label))
                .filter(|label| named && !label.is_empty())
                .unwrap_or_else(|| {
                    match kind {
                        FileRootKind::Fixed => "Local Disk",
                        FileRootKind::Removable => "Removable Disk",
                        FileRootKind::Network => "Network Drive",
                        FileRootKind::Optical => "CD Drive",
                        _ => "Drive",
                    }
                    .into()
                });
            let (mut total, mut free) = (0u64, 0u64);
            let sized = unsafe {
                GetDiskFreeSpaceExW(
                    PCWSTR(name.as_ptr()),
                    None,
                    Some(&mut total),
                    Some(&mut free),
                )
            }
            .is_ok();
            FileRoot {
                label: format!("{label} ({letter}:)"),
                path,
                kind,
                total_bytes: sized.then_some(total),
                free_bytes: sized.then_some(free),
            }
        })
        .collect()
}

fn known_folder(folder: &GUID, logon: &UserLogon) -> Option<PathBuf> {
    let path =
        unsafe { SHGetKnownFolderPath(folder, KNOWN_FOLDER_FLAG(0), Some(logon.token.0)) }.ok()?;
    let value = OsString::from_wide(unsafe { path.as_wide() });
    unsafe { CoTaskMemFree(Some(path.0.cast_const().cast())) };
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// The signed-in user's profile, Desktop, Documents and Downloads, wherever
/// they're redirected to, such as OneDrive.
pub(super) fn signed_in_places(mode: ExecutionMode) -> Vec<FilePlace> {
    let Some(logon) = signed_in_user(mode) else {
        return Vec::new();
    };
    [
        (FilePlaceKind::Home, &FOLDERID_Profile),
        (FilePlaceKind::Desktop, &FOLDERID_Desktop),
        (FilePlaceKind::Documents, &FOLDERID_Documents),
        (FilePlaceKind::Downloads, &FOLDERID_Downloads),
    ]
    .into_iter()
    .filter_map(|(kind, folder)| {
        Some(FilePlace {
            path: paths::text(&known_folder(folder, &logon)?),
            kind,
            user: logon.name.clone(),
        })
    })
    .collect()
}

fn file_time(time: SystemTime) -> Option<FILETIME> {
    // 100-nanosecond intervals since 1601.
    const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    let value = EPOCH_DIFFERENCE + since.as_nanos() as u64 / 100;
    Some(FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    })
}

/// `time` on the device's clock: year, month, day, hour, minute, second.
pub(super) fn local_civil(time: SystemTime) -> Option<(u16, u8, u8, u8, u8, u8)> {
    let utc = file_time(time)?;
    let mut local = FILETIME::default();
    let mut parts = SYSTEMTIME::default();
    unsafe { FileTimeToLocalFileTime(&utc, &mut local) }.ok()?;
    unsafe { FileTimeToSystemTime(&local, &mut parts) }.ok()?;
    Some((
        parts.wYear,
        parts.wMonth as u8,
        parts.wDay as u8,
        parts.wHour as u8,
        parts.wMinute as u8,
        parts.wSecond as u8,
    ))
}

fn unavailable(reason: &str) -> FileError {
    FileError::new(
        FileErrorCode::TrashUnavailable,
        format!("{reason}; delete it permanently instead."),
    )
}

/// The root of the volume `path` is on, such as `C:\`.
fn volume_root(path: &Path) -> Option<PathBuf> {
    let name = wide(path);
    let mut root = [0u16; 261];
    unsafe { GetVolumePathNameW(PCWSTR(name.as_ptr()), &mut root) }.ok()?;
    Some(PathBuf::from(text_of(&root)))
}

/// How much `path` holds, counting at most a large folder's first entries;
/// the Recycle Bin only shows it.
fn size_of(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return metadata.len();
    }
    let mut total = 0;
    let mut seen = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(folder) = pending.pop() {
        for item in fs::read_dir(&folder).into_iter().flatten().flatten() {
            seen += 1;
            if seen > 100_000 {
                return total;
            }
            let Ok(metadata) = item.metadata() else {
                continue;
            };
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(item.path());
            } else {
                total += metadata.len();
            }
        }
    }
    total
}

/// Moves `path` into the signed-in user's Recycle Bin on its drive, the
/// way Explorer does: as `$R…` in `<drive>\$Recycle.Bin\<SID>`, beside a
/// `$I…` record of its size, when it was deleted and where it came from,
/// which Explorer restores it from. Runs as the user, so the user's folder
/// there is theirs. Network drives have no Recycle Bin.
pub(super) fn trash(account: &Account, path: &Path) -> Result<(), FileError> {
    let Some(user) = &account.user else {
        return Err(unavailable("SYSTEM has no Recycle Bin"));
    };
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(&error, path))?;
    let root = volume_root(path).ok_or_else(|| unavailable("This drive has no Recycle Bin"))?;
    let root_name = wide(&root);
    if unsafe { GetDriveTypeW(PCWSTR(root_name.as_ptr())) } == DRIVE_REMOTE {
        return Err(unavailable("Network drives have no Recycle Bin"));
    }
    let bin = root.join("$Recycle.Bin");
    if !bin.is_dir() {
        return Err(unavailable("This drive has no Recycle Bin"));
    }
    let folder = bin.join(user.logon.sid());
    match fs::create_dir(&folder) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(&error, &folder)),
    }
    let extension = if metadata.is_dir() {
        String::new()
    } else {
        path.extension()
            .map(|extension| format!(".{}", extension.to_string_lossy()))
            .unwrap_or_default()
    };
    let original = paths::text(path);
    let mut record = Vec::new();
    record.extend_from_slice(&2u64.to_le_bytes());
    record.extend_from_slice(&size_of(path).to_le_bytes());
    let now = unsafe { GetSystemTimeAsFileTime() };
    record.extend_from_slice(&now.dwLowDateTime.to_le_bytes());
    record.extend_from_slice(&now.dwHighDateTime.to_le_bytes());
    let units = original.encode_utf16().chain([0]).collect::<Vec<_>>();
    record.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        record.extend_from_slice(&unit.to_le_bytes());
    }
    for _ in 0..100 {
        let id = random_id();
        let info = folder.join(format!("$I{id}{extension}"));
        let target = folder.join(format!("$R{id}{extension}"));
        if target.symlink_metadata().is_ok() {
            continue;
        }
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(&error, &info)),
        };
        if let Err(error) = file.write_all(&record) {
            drop(file);
            let _ = fs::remove_file(&info);
            return Err(io_error(&error, &info));
        }
        drop(file);
        return match meshrmm_file_transfer::rename_no_replace(path, &target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&info);
                Err(io_error(&error, path))
            }
        };
    }
    Err(unavailable("The Recycle Bin had no free name"))
}

/// Six letters and digits, as Explorer names Recycle Bin items.
fn random_id() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    uuid::Uuid::new_v4().as_bytes()[..6]
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drives_and_times_are_read() {
        let roots = roots();
        assert!(
            roots
                .iter()
                .any(|root| root.path == "C:\\" && root.kind == FileRootKind::Fixed)
        );
        assert!(roots.iter().all(|root| root.label.ends_with(":)")));
        let (year, ..) = local_civil(SystemTime::now()).unwrap();
        assert!(year >= 2026);
        assert_eq!(random_id().len(), 6);
    }

    #[test]
    fn attributes_hide_and_show_files() {
        let path = std::env::temp_dir().join(format!("meshrmm-attr-{}", uuid::Uuid::new_v4()));
        fs::write(&path, b"x").unwrap();
        hide(&path);
        assert!(hidden("", &fs::metadata(&path).unwrap()));
        unhide(&path);
        assert!(!hidden("", &fs::metadata(&path).unwrap()));
        change_attributes(&path, |current| current | FILE_ATTRIBUTE_READONLY.0).unwrap();
        remove_file(&path).unwrap();
        assert!(!path.exists());
    }
}
