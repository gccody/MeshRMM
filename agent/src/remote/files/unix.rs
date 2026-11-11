//! The file browser on macOS and Linux: accounts, entry flags, volumes,
//! ownership and the Trash.
//!
//! A privileged operation (SYSTEM or root, in the Agent's own account) runs
//! inside folders a local unprivileged user may control, so the copy walk
//! and ownership adoption here never act on a path a second time after
//! checking it. They descend with directory descriptors, opening every
//! component with `O_NOFOLLOW` (and `O_DIRECTORY` for folders), and change
//! an owner with `fchown` on the descriptor of the thing just created, so a
//! link swapped in between a check and the act can't redirect root.
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use meshrmm_protocol::{
    FileError, FileErrorCode, FilePlace, FilePlaceKind, FileRoot, FileRootKind, RunAs,
};

use super::account::{Account, nobody_signed_in};
use super::{Deadline, io_error, paths, timed_out};

/// How much of a file a copy writes between looks at its deadline.
const COPY_PIECE: u64 = 16 * 1024 * 1024;
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

    /// The user's ID.
    pub(super) fn id(&self) -> String {
        self.uid.to_string()
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

/// Opens `path` with `options`, without following a link there.
pub(super) fn open_plain(options: &mut fs::OpenOptions, path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.custom_flags(libc::O_NOFOLLOW).open(path)
}

/// Whether an open file is a regular file with no other names.
pub(super) fn plain_file(file: &fs::File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_file() && metadata.nlink() == 1)
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

pub(super) fn remove_link(path: &Path, _metadata: &Metadata) -> io::Result<()> {
    fs::remove_file(path)
}

pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Whether an operation runs with the Agent's own privileges: as root, in
/// the Agent's own account rather than as the signed-in user. Only then does
/// what it creates take its folder's owner, and only then must the strict
/// descriptor walk guard against a local user swapping a link into a path.
fn privileged(account: &Account) -> bool {
    !account.is_user() && effective_uid() == 0
}

/// Gives what root made the owner and group of the folder it's in, so files
/// put in a user's folder stay theirs. What a user makes is theirs already.
///
/// The owner comes from the opened parent directory, and `fchown` acts on
/// the entry opened without following a link, so a user who swaps the path
/// for a link can't make root hand them another file.
pub(super) fn adopt_parent_owner(account: &Account, path: &Path) {
    if !privileged(account) {
        return;
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let adopt = || -> io::Result<()> {
        let parent = open_dir(parent)?;
        let owner = fstat(parent.as_fd())?;
        let entry = open_no_follow(parent.as_fd(), name)?;
        if !only_name(&fstat(entry.as_fd())?) {
            return Err(io::Error::other("the entry has another name"));
        }
        std::os::unix::fs::fchown(&entry, Some(owner.st_uid), Some(owner.st_gid))
    };
    if let Err(error) = adopt() {
        tracing::debug!(%error, path = %path.display(), "could not give a new entry its folder's owner");
    }
}

/// Whether an opened entry is a folder, or a file with no other name. What
/// the Agent just made has one name; a file with more is a hard link a user
/// put in its place, to another file root would then give them.
// `st_mode` is 32 bits on Linux and 16 on macOS.
#[allow(clippy::unnecessary_cast)]
fn only_name(stat: &libc::stat) -> bool {
    (stat.st_mode as u32) & (libc::S_IFMT as u32) == (libc::S_IFDIR as u32) || stat.st_nlink == 1
}

/// Changes `child`'s owner and group to those of the opened directory
/// `parent`, both held as descriptors, so nothing is reached by a path that
/// could have changed. Used by the copy walk, which already has the
/// descriptors in hand.
fn adopt_owner(parent: BorrowedFd, child: BorrowedFd) {
    let owner = match fstat(parent) {
        Ok(owner) => owner,
        Err(_) => return,
    };
    if let Err(error) = std::os::unix::fs::fchown(child, Some(owner.st_uid), Some(owner.st_gid)) {
        tracing::debug!(%error, "could not give a copied entry its folder's owner");
    }
}

/// A path component's bytes as a C string, for the `*at` system calls.
fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Opens a directory by path, following links in the folders above it (the
/// path a technician chose is trusted); the descent below it never follows a
/// link.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let text = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: the path is NUL-terminated; the flags are constants.
    let fd = unsafe {
        libc::open(
            text.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Opens the directory `name` inside `dir` without following a link in its
/// place: a link, or anything that isn't a directory, fails rather than
/// redirecting the walk.
fn open_dir_at(dir: BorrowedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    // SAFETY: the name is NUL-terminated; the flags are constants.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Opens `name` inside `dir` for reading without following a link in its
/// place. A directory opens too; the caller checks the kind on the
/// descriptor. A link fails with `ELOOP`. The open never waits: a named
/// pipe swapped in would otherwise hold it until someone wrote to the pipe.
/// Reading a regular file is the same either way.
fn open_no_follow(dir: BorrowedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    // SAFETY: the name is NUL-terminated; the flags are constants.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Creates the new file `name` inside `dir`, failing if it already exists or
/// is a link, and opens it for writing.
fn create_file_at(dir: BorrowedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    // SAFETY: the name is NUL-terminated; the flags and mode are plain
    // values. O_EXCL | O_NOFOLLOW means the file is brand new and not a link.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Creates the directory `name` inside `dir`.
fn mkdir_at(dir: BorrowedFd, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: the name is NUL-terminated.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The kind and owner of `name` inside `dir`, seen without following a link
/// in its place.
fn lstat_at(dir: BorrowedFd, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: the name is NUL-terminated; fstatat fills `stat`.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat filled it in.
    Ok(unsafe { stat.assume_init() })
}

/// The kind and owner of an open descriptor.
fn fstat(fd: BorrowedFd) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstat fills `stat` for a valid descriptor.
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat filled it in.
    Ok(unsafe { stat.assume_init() })
}

/// Where the link `name` inside `dir` points, read without following it.
fn read_link_at(dir: BorrowedFd, name: &OsStr) -> io::Result<PathBuf> {
    let c = c_name(name)?;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the name is NUL-terminated; the buffer holds `buffer.len()`
    // bytes for readlinkat to write into.
    let len = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            c.as_ptr(),
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
        )
    };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len as usize);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

/// Creates a link `name` inside `dir` pointing where `points_to` says.
fn symlink_at(points_to: &Path, dir: BorrowedFd, name: &OsStr) -> io::Result<()> {
    let target = CString::new(points_to.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let name = c_name(name)?;
    // SAFETY: both strings are NUL-terminated.
    if unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The names in the directory `dir`, without `.` and `..`, read from the
/// descriptor so nothing is reached by a path.
fn read_dir_names(dir: BorrowedFd) -> io::Result<Vec<OsString>> {
    // fdopendir takes over the descriptor it reads and closedir closes it,
    // so give it a duplicate and keep ours to open the entries through.
    // SAFETY: dup of a valid descriptor.
    let duplicate = unsafe { libc::dup(dir.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the duplicate is a fresh directory descriptor.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir did not take the descriptor, so close it.
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    let mut names = Vec::new();
    let result = loop {
        clear_errno();
        // SAFETY: the stream is open; readdir returns its own static storage
        // or null. closedir below frees the stream.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            break if errno == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(errno))
            };
        }
        // SAFETY: entry points to a valid dirent while the stream is open.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsStr::from_bytes(bytes).to_owned());
    };
    // SAFETY: closes the stream and its duplicate descriptor.
    unsafe { libc::closedir(stream) };
    result.map(|()| names)
}

/// Clears `errno`, so a following `readdir` returning null can be told apart
/// as the end of the directory (`errno` unchanged) or an error.
fn clear_errno() {
    #[cfg(target_os = "linux")]
    // SAFETY: __errno_location returns a valid pointer to this thread's errno.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: __error returns a valid pointer to this thread's errno.
    unsafe {
        *libc::__error() = 0;
    }
}

/// The error for an entry whose kind changed under the copy, as when a user
/// swaps a file or folder for a link while it runs.
fn changed(path: &Path) -> FileError {
    FileError::new(
        FileErrorCode::Changed,
        format!("{} changed while it was being copied.", path.display()),
    )
}

/// A `NOFOLLOW` open that failed because a link or the wrong kind was in the
/// path becomes a "changed" error; anything else keeps its own meaning.
fn changed_or_io(error: &io::Error, path: &Path) -> FileError {
    match error.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => changed(path),
        _ => io_error(error, path),
    }
}

/// Copies `source` to `target`, a folder with everything in it, descending
/// with directory descriptors so a link swapped into the walk can't make a
/// privileged copy read or write somewhere the user chose. Links are copied
/// as links, never followed. Stops at the deadline, before an entry or a
/// piece of a file.
pub(super) fn copy_tree(
    account: &Account,
    source: &Path,
    target: &Path,
    deadline: &Deadline,
) -> Result<(), FileError> {
    let (Some(source_parent), Some(source_name)) = (source.parent(), source.file_name()) else {
        return Err(io_error(&io::ErrorKind::InvalidInput.into(), source));
    };
    let (Some(target_parent), Some(target_name)) = (target.parent(), target.file_name()) else {
        return Err(io_error(&io::ErrorKind::InvalidInput.into(), target));
    };
    let source_parent = open_dir(source_parent).map_err(|error| io_error(&error, source))?;
    let target_parent = open_dir(target_parent).map_err(|error| io_error(&error, target))?;
    copy_entry(
        privileged(account),
        source_parent.as_fd(),
        source_name,
        source,
        target_parent.as_fd(),
        target_name,
        target,
        deadline,
    )
}

/// Copies one entry, `source_name` inside `source_parent`, to `target_name`
/// inside `target_parent`, both named by descriptor. `source` and `target`
/// are the paths only for error messages. Recurses into folders through the
/// descriptors it opens. `adopt` gives each new entry its folder's owner.
// The mode constants are `mode_t`, 32 bits on Linux and 16 on macOS, so the
// casts are needed on one and a no-op on the other.
#[allow(clippy::too_many_arguments, clippy::unnecessary_cast)]
fn copy_entry(
    adopt: bool,
    source_parent: BorrowedFd,
    source_name: &OsStr,
    source: &Path,
    target_parent: BorrowedFd,
    target_name: &OsStr,
    target: &Path,
    deadline: &Deadline,
) -> Result<(), FileError> {
    if deadline.passed() {
        return Err(timed_out("The device ran out of time."));
    }
    let stat = lstat_at(source_parent, source_name).map_err(|error| io_error(&error, source))?;
    let kind = (stat.st_mode as u32) & (libc::S_IFMT as u32);
    if kind == (libc::S_IFLNK as u32) {
        let points_to =
            read_link_at(source_parent, source_name).map_err(|error| io_error(&error, source))?;
        return symlink_at(&points_to, target_parent, target_name)
            .map_err(|error| io_error(&error, target));
    }
    if kind == (libc::S_IFDIR as u32) {
        mkdir_at(target_parent, target_name).map_err(|error| io_error(&error, target))?;
        let target_dir = open_dir_at(target_parent, target_name)
            .map_err(|error| changed_or_io(&error, target))?;
        if adopt {
            adopt_owner(target_parent, target_dir.as_fd());
        }
        let source_dir = open_dir_at(source_parent, source_name)
            .map_err(|error| changed_or_io(&error, source))?;
        for name in read_dir_names(source_dir.as_fd()).map_err(|error| io_error(&error, source))? {
            copy_entry(
                adopt,
                source_dir.as_fd(),
                &name,
                &source.join(&name),
                target_dir.as_fd(),
                &name,
                &target.join(&name),
                deadline,
            )?;
        }
        return Ok(());
    }
    if kind == (libc::S_IFREG as u32) {
        let source_file = open_no_follow(source_parent, source_name)
            .map_err(|error| changed_or_io(&error, source))?;
        let mut source_file = fs::File::from(source_file);
        let metadata = source_file
            .metadata()
            .map_err(|error| io_error(&error, source))?;
        // The kind is read again on the open descriptor: a file swapped for a
        // directory passes the NOFOLLOW open but is not a regular file.
        if !metadata.is_file() {
            return Err(changed(source));
        }
        let target_file =
            create_file_at(target_parent, target_name).map_err(|error| io_error(&error, target))?;
        let mut target_file = fs::File::from(target_file);
        loop {
            use std::io::Read;
            let copied = io::copy(&mut source_file.by_ref().take(COPY_PIECE), &mut target_file)
                .map_err(|error| io_error(&error, target))?;
            if copied < COPY_PIECE {
                break;
            }
            if deadline.passed() {
                return Err(timed_out("The device ran out of time."));
            }
        }
        let _ = target_file.set_permissions(metadata.permissions());
        if let Ok(modified) = metadata.modified() {
            let _ = target_file.set_modified(modified);
        }
        if adopt {
            adopt_owner(target_parent, target_file.as_fd());
        }
        return Ok(());
    }
    Err(FileError::new(
        FileErrorCode::Failed,
        format!("{} isn't a regular file, folder or link.", source.display()),
    ))
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

    /// A temporary folder removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("meshrmm-{tag}-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn opening_a_source_file_does_not_follow_a_symlink_swapped_in() {
        let dir = TempDir::new("nofollow");
        let secret = dir.0.join("secret");
        fs::write(&secret, b"SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, dir.0.join("item")).unwrap();
        let parent = open_dir(&dir.0).unwrap();
        // The descriptor walk refuses a link in place of the regular file it
        // meant to read, so a privileged copy can't be made to read it out.
        let refused = open_no_follow(parent.as_fd(), OsStr::new("item"));
        assert_eq!(refused.unwrap_err().raw_os_error(), Some(libc::ELOOP));
        // A plain open by path, as the code did before, would have followed
        // the link and read the secret.
        assert_eq!(fs::read(dir.0.join("item")).unwrap(), b"SECRET");
        // A real regular file opens as before.
        assert!(open_no_follow(parent.as_fd(), OsStr::new("secret")).is_ok());
    }

    #[test]
    fn descending_does_not_follow_a_directory_symlink() {
        let dir = TempDir::new("descend");
        fs::create_dir(dir.0.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.0.join("real"), dir.0.join("link")).unwrap();
        let parent = open_dir(&dir.0).unwrap();
        // A link in place of a subfolder is refused, so the walk never
        // descends into somewhere a user chose.
        assert!(open_dir_at(parent.as_fd(), OsStr::new("link")).is_err());
        // A real directory opens.
        assert!(open_dir_at(parent.as_fd(), OsStr::new("real")).is_ok());
    }

    #[test]
    fn ownership_adoption_decides_from_the_parent_descriptor_and_skips_a_link() {
        let dir = TempDir::new("adopt");
        // The owner to adopt is the parent folder's, read from the opened
        // descriptor rather than a path that could be swapped.
        let parent = open_dir(&dir.0).unwrap();
        let owner = fstat(parent.as_fd()).unwrap();
        let folder = fs::metadata(&dir.0).unwrap();
        assert_eq!((owner.st_uid, owner.st_gid), (folder.uid(), folder.gid()));
        // fchown goes to the entry opened without following a link; a link
        // swapped into its place is refused, so root never chowns the link's
        // target. The chown itself needs root and isn't exercised here.
        fs::write(dir.0.join("made"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.0.join("swapped")).unwrap();
        assert!(open_no_follow(parent.as_fd(), OsStr::new("made")).is_ok());
        assert!(open_no_follow(parent.as_fd(), OsStr::new("swapped")).is_err());
    }

    #[test]
    fn ownership_adoption_skips_a_file_with_another_name() {
        let dir = TempDir::new("names");
        fs::write(dir.0.join("made"), b"x").unwrap();
        fs::create_dir(dir.0.join("folder")).unwrap();
        let parent = open_dir(&dir.0).unwrap();
        let stat = |name: &str| {
            let entry = open_no_follow(parent.as_fd(), OsStr::new(name)).unwrap();
            fstat(entry.as_fd()).unwrap()
        };
        assert!(only_name(&stat("made")));
        // A folder always has more than one link, and can't be hard linked.
        assert!(only_name(&stat("folder")));
        // A hard link a user put where the new file was is another file's
        // second name: root must not give that file away.
        fs::hard_link(dir.0.join("made"), dir.0.join("linked")).unwrap();
        assert!(!only_name(&stat("linked")));
        assert!(!only_name(&stat("made")));
    }

    #[test]
    fn opening_a_named_pipe_swapped_in_for_a_file_does_not_wait() {
        let dir = TempDir::new("pipe");
        let name = CString::new(dir.0.join("pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let parent = open_dir(&dir.0).unwrap();
        // Without O_NONBLOCK this open would wait for a writer forever.
        let pipe = open_no_follow(parent.as_fd(), OsStr::new("pipe")).unwrap();
        assert!(!fs::File::from(pipe).metadata().unwrap().is_file());
    }

    #[test]
    fn hidden_names_start_with_a_dot() {
        let metadata = fs::metadata("/").unwrap();
        assert!(hidden(".profile", &metadata));
        assert!(!hidden("profile", &metadata));
        assert!(!system(&metadata));
    }

    /// Needs root: a thread acting as `nobody` is refused a root-only
    /// folder and sees itself as nobody, while the rest of the process
    /// stays root.
    #[test]
    fn user_threads_are_checked_as_the_user_and_the_rest_stays_root() {
        if effective_uid() != 0 {
            eprintln!("skipped: needs root");
            return;
        }
        let name = CString::new("nobody").unwrap();
        // SAFETY: getpwnam returns static storage or null.
        let entry = unsafe { libc::getpwnam(name.as_ptr()).as_ref() }.expect("a nobody account");
        let (uid, gid) = (entry.pw_uid, entry.pw_gid);
        let folder = std::env::temp_dir().join(format!("meshrmm-as-user-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let account = Account {
            label: "nobody".into(),
            home: None,
            user: Some(User {
                uid,
                gid,
                groups: vec![gid],
            }),
        };
        let denied = account.run(|| {
            fs::read_dir(&folder)
                .map(drop)
                .map_err(|error| io_error(&error, &folder))
        });
        assert_eq!(denied.unwrap_err().code, FileErrorCode::AccessDenied);
        assert_eq!(account.run(|| Ok(effective_uid())).unwrap(), uid);
        assert_eq!(effective_uid(), 0, "the rest of the process is still root");
        assert!(fs::read_dir(&folder).is_ok());
        fs::remove_dir(&folder).unwrap();
    }

    #[test]
    fn root_is_always_a_root() {
        let roots = roots();
        assert_eq!(roots[0].path, "/");
        assert_eq!(roots[0].kind, FileRootKind::Root);
        assert!(roots[0].total_bytes.is_some());
    }
}
