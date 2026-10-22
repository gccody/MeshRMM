//! The file browser on macOS and Linux: accounts, entry flags, volumes,
//! ownership and the Trash.
use std::ffi::CString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use meshrmm_protocol::{
    FileError, FileErrorCode, FilePlace, FilePlaceKind, FileRoot, FileRootKind, RunAs,
};

use super::account::{Account, nobody_signed_in};
use super::{io_error, paths};
use crate::remote::config::ExecutionMode;
use crate::remote::user_database::UserEntry;

/// The signed-in user an operation acts as.
pub(super) struct User {
    uid: u32,
    gid: u32,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    groups: Vec<libc::gid_t>,
}

impl User {
    pub(super) fn uid(&self) -> u32 {
        self.uid
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// The user signed in on the console.
fn console_user() -> Option<u32> {
    #[cfg(target_os = "macos")]
    return crate::remote::macos::session_close::console_user();
    #[cfg(target_os = "linux")]
    return crate::remote::linux::session_close::console_user();
}

fn entry(uid: u32) -> Result<UserEntry, FileError> {
    UserEntry::of(uid).ok_or_else(|| {
        FileError::new(
            FileErrorCode::Failed,
            format!("The account with user ID {uid} isn't in the user database."),
        )
    })
}

fn existing(folder: PathBuf) -> Option<PathBuf> {
    folder.is_dir().then_some(folder)
}

pub(super) fn account(mode: ExecutionMode, run_as: RunAs) -> Result<Account, FileError> {
    let own = effective_uid();
    // A development Agent has only its own account.
    if own != 0 || mode == ExecutionMode::Console || run_as == RunAs::System {
        let entry = entry(own)?;
        return Ok(Account {
            label: entry.name,
            home: existing(entry.home),
            user: None,
        });
    }
    let uid = console_user().ok_or_else(nobody_signed_in)?;
    let entry = entry(uid)?;
    #[cfg(target_os = "linux")]
    let groups = entry.groups();
    #[cfg(target_os = "macos")]
    let groups = vec![entry.gid];
    Ok(Account {
        user: Some(User {
            uid,
            gid: entry.gid,
            groups,
        }),
        label: entry.name,
        home: existing(entry.home),
    })
}

/// The thread acts as the user until it ends; there is no going back.
pub(super) struct Acting;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// Gives the calling thread `uid` and `gid`, with the user's groups
    /// resolved by the directory service, until it ends.
    fn pthread_setugid_np(uid: libc::uid_t, gid: libc::gid_t) -> libc::c_int;
}

/// Makes the calling thread `user`. Only a new thread that ends after the
/// operation calls this.
pub(super) fn act_as(user: &User) -> io::Result<Acting> {
    #[cfg(target_os = "linux")]
    {
        // The raw system calls change only this thread; libc's wrappers
        // would change every thread of the Agent. Groups first, while the
        // thread may still change them. Dropping root's user ID drops its
        // capabilities, so the kernel checks every access as the user's.
        let check = |result: libc::c_long| {
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        // SAFETY: the group list holds `len` entries; the IDs are plain
        // values.
        unsafe {
            check(libc::syscall(
                libc::SYS_setgroups,
                user.groups.len(),
                user.groups.as_ptr(),
            ))?;
            check(libc::syscall(
                libc::SYS_setresgid,
                user.gid,
                user.gid,
                user.gid,
            ))?;
            check(libc::syscall(
                libc::SYS_setresuid,
                user.uid,
                user.uid,
                user.uid,
            ))?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the IDs are plain values; the thread ends with them.
        if unsafe { pthread_setugid_np(user.uid, user.gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Acting)
}

/// Why access to `path` was denied, with the way around macOS's privacy
/// protection when that's what refused it.
pub(super) fn denied(error: &io::Error, path: &Path) -> String {
    if cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EPERM) {
        return format!(
            "macOS's privacy protection kept the Agent out of {}. Give MeshRMM Agent Full Disk Access in System Settings → Privacy & Security, or with a configuration profile.",
            path.display()
        );
    }
    format!("Access to {} is denied.", path.display())
}

pub(super) fn hidden(name: &str, metadata: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        if metadata.st_flags() & libc::UF_HIDDEN != 0 {
            return true;
        }
    }
    let _ = metadata;
    name.starts_with('.')
}

pub(super) fn system(_metadata: &Metadata) -> bool {
    false
}

pub(super) fn read_only(metadata: &Metadata) -> bool {
    metadata.mode() & 0o222 == 0
}

/// Hides an upload's partial file; on macOS and Linux its name starts with
/// a dot.
pub(super) fn hide(_path: &Path) {}

/// Shows a finished upload; nothing to do where a name hides a file.
pub(super) fn unhide(_path: &Path) {}

/// Whether `a` and `b` are the same entry, as when a name differs only in
/// case on a volume that ignores case.
pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Copies the link `source` as a link to the same place.
pub(super) fn copy_link(source: &Path, target: &Path) -> Result<(), FileError> {
    let points_to = fs::read_link(source).map_err(|error| io_error(&error, source))?;
    std::os::unix::fs::symlink(points_to, target).map_err(|error| io_error(&error, target))
}

pub(super) fn remove_link(path: &Path, _metadata: &Metadata) -> io::Result<()> {
    fs::remove_file(path)
}

pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Gives what root made the owner and group of the folder it's in, so files
/// put in a user's folder stay theirs. What a user makes is theirs already.
pub(super) fn adopt_parent_owner(account: &Account, path: &Path) {
    if account.is_user() || effective_uid() != 0 {
        return;
    }
    let Some(parent) = path.parent() else { return };
    let Ok(folder) = fs::metadata(parent) else {
        return;
    };
    if let Err(error) = std::os::unix::fs::lchown(path, Some(folder.uid()), Some(folder.gid())) {
        tracing::debug!(%error, path = %path.display(), "could not give a new entry its folder's owner");
    }
}

/// `time` on the device's clock: year, month, day, hour, minute, second.
pub(super) fn local_civil(time: std::time::SystemTime) -> Option<(u16, u8, u8, u8, u8, u8)> {
    let seconds = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let seconds = libc::time_t::try_from(seconds).ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are valid; localtime_r fills `local`.
    if unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: localtime_r filled it in.
    let local = unsafe { local.assume_init() };
    Some((
        u16::try_from(local.tm_year + 1900).ok()?,
        (local.tm_mon + 1) as u8,
        local.tm_mday as u8,
        local.tm_hour as u8,
        local.tm_min as u8,
        local.tm_sec.min(59) as u8,
    ))
}

// statvfs's fields are 32 bits on macOS and 64 on Linux.
#[allow(clippy::unnecessary_cast)]
fn space(path: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(text) = CString::new(path.as_os_str().as_bytes()) else {
        return (None, None);
    };
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: the path is NUL-terminated and the buffer is a statvfs.
    if unsafe { libc::statvfs(text.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return (None, None);
    }
    // SAFETY: statvfs filled it in.
    let stats = unsafe { stats.assume_init() };
    let block = stats.f_frsize as u64;
    (
        Some(stats.f_blocks as u64 * block),
        Some(stats.f_bavail as u64 * block),
    )
}

fn root(path: PathBuf, label: String, kind: FileRootKind) -> FileRoot {
    let (total_bytes, free_bytes) = space(&path);
    FileRoot {
        path: paths::text(&path),
        label,
        kind,
        total_bytes,
        free_bytes,
    }
}

/// `/`, and the volumes mounted for people: those in `/Volumes` on a Mac,
/// and under `/media`, `/mnt` and `/run/media` on Linux.
pub(super) fn roots() -> Vec<FileRoot> {
    let mut roots = vec![root(PathBuf::from("/"), "/".into(), FileRootKind::Root)];
    roots.extend(volumes());
    roots
}

#[cfg(target_os = "macos")]
fn volumes() -> Vec<FileRoot> {
    let Ok(listing) = fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    let mut volumes = listing
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        // The startup disk appears there as a link to `/`.
        .filter(|path| fs::canonicalize(path).is_ok_and(|real| real != Path::new("/")))
        .filter(|path| path.is_dir())
        .map(|path| {
            let local = CString::new(path.as_os_str().as_bytes())
                .ok()
                .and_then(|text| {
                    let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
                    // SAFETY: the path is NUL-terminated and the buffer a statfs.
                    (unsafe { libc::statfs(text.as_ptr(), stats.as_mut_ptr()) } == 0).then(|| {
                        // SAFETY: statfs filled it in.
                        unsafe { stats.assume_init() }.f_flags & libc::MNT_LOCAL as u32 != 0
                    })
                })
                .unwrap_or(true);
            let label = paths::name(&path);
            let kind = if local {
                FileRootKind::Fixed
            } else {
                FileRootKind::Network
            };
            root(path, label, kind)
        })
        .collect::<Vec<_>>();
    volumes.sort_by(|a, b| a.label.cmp(&b.label));
    volumes
}

#[cfg(target_os = "linux")]
fn volumes() -> Vec<FileRoot> {
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut volumes = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split(' ');
        let (Some(device), Some(point), Some(kind)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let point = unescape_mount(point);
        let personal = ["/media/", "/mnt/", "/run/media/"]
            .iter()
            .any(|prefix| point.starts_with(prefix));
        let network = matches!(
            kind,
            "nfs" | "nfs4" | "cifs" | "smb3" | "sshfs" | "fuse.sshfs"
        );
        if !personal || !(device.starts_with("/dev/") || network) {
            continue;
        }
        let path = PathBuf::from(&point);
        if volumes.iter().any(|known: &FileRoot| known.path == point) {
            continue;
        }
        let label = paths::name(&path);
        let kind = if network {
            FileRootKind::Network
        } else if kind == "iso9660" || kind == "udf" {
            FileRootKind::Optical
        } else if point.starts_with("/media/") || point.starts_with("/run/media/") {
            FileRootKind::Removable
        } else {
            FileRootKind::Fixed
        };
        volumes.push(root(path, label, kind));
    }
    volumes
}

/// A mount point as `/proc/self/mounts` writes it, with `\040` for a
/// space and the like.
#[cfg(target_os = "linux")]
fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = bytes[index + 1..index + 4]
                .iter()
                .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
            out.push(value as u8);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The folders of the user signed in on the console: home, Desktop,
/// Documents and Downloads, those that exist.
pub(super) fn signed_in_places(_mode: ExecutionMode) -> Vec<FilePlace> {
    let Some(entry) = console_user().and_then(UserEntry::of) else {
        return Vec::new();
    };
    let mut places = vec![(FilePlaceKind::Home, entry.home.clone())];
    for (kind, key, fallback) in [
        (FilePlaceKind::Desktop, "XDG_DESKTOP_DIR", "Desktop"),
        (FilePlaceKind::Documents, "XDG_DOCUMENTS_DIR", "Documents"),
        (FilePlaceKind::Downloads, "XDG_DOWNLOAD_DIR", "Downloads"),
    ] {
        let folder = user_dir(&entry.home, key).unwrap_or_else(|| entry.home.join(fallback));
        places.push((kind, folder));
    }
    places
        .into_iter()
        .filter(|(_, path)| path.is_dir())
        .map(|(kind, path)| FilePlace {
            path: paths::text(&path),
            kind,
            user: entry.name.clone(),
        })
        .collect()
}

/// A folder from the user's `~/.config/user-dirs.dirs` on Linux, such as
/// `XDG_DOWNLOAD_DIR="$HOME/Downloads"`, which a localized desktop names in
/// its language.
fn user_dir(home: &Path, key: &str) -> Option<PathBuf> {
    if cfg!(not(target_os = "linux")) {
        return None;
    }
    let text = fs::read_to_string(home.join(".config/user-dirs.dirs")).ok()?;
    let value = text.lines().find_map(|line| {
        let (name, value) = line.trim().split_once('=')?;
        (name == key).then(|| value.trim().trim_matches('"').to_owned())
    })?;
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // `$HOME/` alone means the folder is turned off.
    (path != home && path.is_absolute()).then_some(path)
}

/// Moves `path` into the Trash of the user it belongs to now: their home's
/// on the home's volume, the volume's own elsewhere. Runs as the user.
pub(super) fn trash(account: &Account, path: &Path) -> Result<(), FileError> {
    let (Some(user), Some(home)) = (&account.user, &account.home) else {
        return Err(unavailable("SYSTEM and root have no Trash"));
    };
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(&error, path))?;
    let home_device = fs::metadata(home)
        .map_err(|error| io_error(&error, home))?
        .dev();
    let name = paths::name(path);
    #[cfg(target_os = "linux")]
    return linux_trash(user.uid(), home, home_device, path, &metadata, &name);
    #[cfg(target_os = "macos")]
    return mac_trash(user.uid(), home, home_device, path, &metadata, &name);
}

fn unavailable(reason: &str) -> FileError {
    FileError::new(
        FileErrorCode::TrashUnavailable,
        format!("{reason}; delete it permanently instead."),
    )
}

/// Creates `folder` for the user alone, or checks that it's a real folder
/// of theirs.
fn private_folder(folder: &Path, uid: u32) -> Result<(), FileError> {
    match fs::DirBuilder::new().mode(0o700).create(folder) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata =
                fs::symlink_metadata(folder).map_err(|error| io_error(&error, folder))?;
            if metadata.is_dir() && metadata.uid() == uid {
                Ok(())
            } else {
                Err(unavailable(&format!(
                    "{} isn't a folder of the user's",
                    folder.display()
                )))
            }
        }
        Err(error) => Err(io_error(&error, folder)),
    }
}

/// The folder the volume holding `device` is mounted on, found by walking
/// up from `path` until the volume changes.
fn mount_point(path: &Path, device: u64) -> PathBuf {
    let mut point = path.to_path_buf();
    while let Some(parent) = point.parent() {
        if fs::metadata(parent).map(|m| m.dev()).ok() != Some(device) {
            break;
        }
        point = parent.to_path_buf();
    }
    point
}

/// The freedesktop.org Trash: the home Trash in `~/.local/share/Trash`, or
/// `.Trash-<uid>` at the top of another volume. Each item has a
/// `.trashinfo` file saying where it came from, which file managers restore
/// it to.
#[cfg(target_os = "linux")]
fn linux_trash(
    uid: u32,
    home: &Path,
    home_device: u64,
    path: &Path,
    metadata: &Metadata,
    name: &str,
) -> Result<(), FileError> {
    let (trash, recorded) = if metadata.dev() == home_device {
        let share = home.join(".local/share");
        fs::create_dir_all(&share).map_err(|error| io_error(&error, &share))?;
        let trash = share.join("Trash");
        private_folder(&trash, uid)?;
        (trash, paths::text(path))
    } else {
        let top = mount_point(path, metadata.dev());
        let trash = top.join(format!(".Trash-{uid}"));
        private_folder(&trash, uid)?;
        let relative = path.strip_prefix(&top).unwrap_or(path);
        (trash, paths::text(relative))
    };
    let (files, info) = (trash.join("files"), trash.join("info"));
    private_folder(&files, uid)?;
    private_folder(&info, uid)?;
    let deleted = local_time_text();
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            name.to_owned()
        } else {
            format!("{name}.{attempt}")
        };
        let info_path = info.join(format!("{candidate}.trashinfo"));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_path);
        let mut file = match file {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(&error, &info_path)),
        };
        use std::io::Write;
        let written = write!(
            file,
            "[Trash Info]\nPath={}\nDeletionDate={deleted}\n",
            encode_trash_path(&recorded)
        );
        if let Err(error) = written {
            let _ = fs::remove_file(&info_path);
            return Err(io_error(&error, &info_path));
        }
        let target = files.join(&candidate);
        return match meshrmm_file_transfer::rename_no_replace(path, &target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&info_path);
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                Err(io_error(&error, path))
            }
        };
    }
    Err(unavailable(
        "The Trash already has too many items with this name",
    ))
}

/// A path for a `.trashinfo` file: percent-encoded, keeping `/`.
#[cfg(target_os = "linux")]
fn encode_trash_path(path: &str) -> String {
    path.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Now in local time, as `.trashinfo` files write it: `2026-10-22T09:30:00`.
#[cfg(target_os = "linux")]
fn local_time_text() -> String {
    let (year, month, day, hour, minute, second) =
        local_civil(std::time::SystemTime::now()).unwrap_or((1970, 1, 1, 0, 0, 0));
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
}

/// The Mac's Trash: `~/.Trash` on the home's volume, `.Trashes/<uid>` at
/// the top of another. Finder lists what's there, but without its own
/// records of where an item came from it can't put it back by itself.
#[cfg(target_os = "macos")]
fn mac_trash(
    uid: u32,
    home: &Path,
    home_device: u64,
    path: &Path,
    metadata: &Metadata,
    name: &str,
) -> Result<(), FileError> {
    let trash = if metadata.dev() == home_device {
        let trash = home.join(".Trash");
        private_folder(&trash, uid)?;
        trash
    } else {
        let trashes = mount_point(path, metadata.dev()).join(".Trashes");
        if !trashes.is_dir() {
            return Err(unavailable("This volume has no Trash"));
        }
        let trash = trashes.join(uid.to_string());
        private_folder(&trash, uid)?;
        trash
    };
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 && metadata.is_file() => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for attempt in 1..1000 {
        let candidate = if attempt == 1 {
            name.to_owned()
        } else {
            format!("{stem} {attempt}{extension}")
        };
        match meshrmm_file_transfer::rename_no_replace(path, &trash.join(&candidate)) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(&error, path)),
        }
    }
    Err(unavailable(
        "The Trash already has too many items with this name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_points_and_trash_paths_are_decoded_and_encoded() {
        assert_eq!(
            unescape_mount("/media/ada/My\\040Disk"),
            "/media/ada/My Disk"
        );
        assert_eq!(unescape_mount("/mnt/plain"), "/mnt/plain");
        assert_eq!(
            encode_trash_path("/home/ada/a b%.txt"),
            "/home/ada/a%20b%25.txt"
        );
        assert_eq!(local_time_text().len(), 19);
    }

    #[test]
    fn hidden_names_start_with_a_dot() {
        let metadata = fs::metadata("/").unwrap();
        assert!(hidden(".profile", &metadata));
        assert!(!hidden("profile", &metadata));
        assert!(!system(&metadata));
    }

    #[test]
    fn root_is_always_a_root() {
        let roots = roots();
        assert_eq!(roots[0].path, "/");
        assert_eq!(roots[0].kind, FileRootKind::Root);
        assert!(roots[0].total_bytes.is_some());
    }
}
