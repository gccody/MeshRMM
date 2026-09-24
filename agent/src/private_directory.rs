//! Administrator-only directories for state that the LocalSystem Agent later trusts.
//!
//! Standard users can create folders under ProgramData and then own them, and an owner can
//! rewrite the DACL at any time. These helpers only reuse directories owned by SYSTEM or an
//! administrator, replace the owner and DACL through a handle that never follows reparse points,
//! and verify the stored security descriptor before callers write credentials or executables.
//! State left by an older installation is only read when the same accounts control it.

use std::ffi::OsStr;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_ALL,
    GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LUID, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AdjustTokenPrivileges,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, INHERIT_ONLY_ACE,
    InitializeAcl, IsWellKnownSid, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED,
    SE_RESTORE_NAME, SECURITY_ATTRIBUTES, SetKernelObjectSecurity, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenUser, UNPROTECTED_DACL_SECURITY_INFORMATION,
    WinAuthenticatedUserSid, WinBuiltinAdministratorsSid, WinBuiltinUsersSid, WinLocalSystemSid,
    WinWorldSid,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, DELETE, FILE_ACCESS_RIGHTS,
    FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
    GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// Owned by Administrators, full control for SYSTEM and Administrators only, and protected from
/// the ProgramData ACEs that let standard users create and own subfolders.
const PRIVATE_DIRECTORY_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// What directories and files below a private directory inherit from it.
const INHERITED_DIRECTORY_SDDL: &str = "O:BAD:(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)";
const INHERITED_FILE_SDDL: &str = "O:BAD:(A;ID;FA;;;SY)(A;ID;FA;;;BA)";

/// A pre-existing directory owned by another account. Its owner may still hold a handle with
/// WRITE_DAC, so resetting the ACL in place cannot evict them.
#[derive(Debug)]
pub struct UntrustedOwner(pub PathBuf);

impl std::fmt::Display for UntrustedOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} is owned by an account other than SYSTEM or an administrator",
            self.0.display()
        )
    }
}

impl std::error::Error for UntrustedOwner {}

/// Creates `path` with the private descriptor, or takes over an existing directory owned by SYSTEM
/// or an administrator by replacing its owner and DACL. Fails with [`UntrustedOwner`] instead of
/// reusing a directory another account owns. The parent must already be trustworthy.
pub fn secure(path: &Path) -> anyhow::Result<()> {
    let _privileges = Privileges::enable()?;
    match create(path) {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_ALREADY_EXISTS.to_hresult() => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to create {}", path.display()));
        }
    }
    let (handle, information) = open_without_following(path)?;
    ensure_plain_directory(path, &information)?;
    if !owner_is_trusted(&handle, path)? {
        return Err(UntrustedOwner(path.to_owned()).into());
    }
    apply(&handle, path, PRIVATE_DIRECTORY_SDDL)
}

/// Creates a new private directory, failing if anything already exists at `path`.
pub fn create_new(path: &Path) -> anyhow::Result<()> {
    let _privileges = Privileges::enable()?;
    create(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))?;
    let (handle, information) = open_without_following(path)?;
    ensure_plain_directory(path, &information)?;
    verify(&handle, path, PRIVATE_DIRECTORY_SDDL)
}

/// Resets the owner and DACL of every entry below a directory already passed to [`secure`], so
/// files planted before it was protected cannot keep an owner or explicit ACEs. Refuses to walk
/// through reparse points rather than changing security on their targets.
pub fn secure_contents(path: &Path) -> anyhow::Result<()> {
    let _privileges = Privileges::enable()?;
    let mut pending = vec![path.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .with_context(|| format!("failed to list {}", directory.display()))?;
        for entry in entries {
            let path = entry
                .with_context(|| format!("failed to list {}", directory.display()))?
                .path();
            let (handle, information) = open_without_following(&path)?;
            ensure_not_reparse_point(&path, &information)?;
            if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                apply(&handle, &path, INHERITED_DIRECTORY_SDDL)?;
                pending.push(path);
            } else {
                apply(&handle, &path, INHERITED_FILE_SDDL)?;
            }
        }
    }
    Ok(())
}

/// Gives a file moved out of a private directory the ACEs its new parent passes down, as if it had
/// been created there. A rename keeps the private DACL, so an updated Agent staged under
/// ProgramData would stay unreadable to the signed-in users who start its tray. Fails unless
/// Users can then read and execute the file.
pub fn inherit_parent_security(file: &Path) -> anyhow::Result<()> {
    let (handle, information) = open_handle(file, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    ensure_not_reparse_point(file, &information)?;
    let mut empty = ACL::default();
    unsafe { InitializeAcl(&mut empty, std::mem::size_of::<ACL>() as u32, ACL_REVISION) }?;
    let wide_path = wide(file.as_os_str());
    // Unlike the handle-based calls, the named call merges in what the parent passes down.
    unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(wide_path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&empty),
            None,
        )
    }
    .ok()
    .with_context(|| format!("failed to reset the security of {}", file.display()))?;
    if !users_can_read_and_execute(&handle, file)? {
        bail!(
            "{} does not let Users read and execute it after inheriting its directory's security",
            file.display()
        );
    }
    Ok(())
}

/// Whether an ACE for Users, Authenticated Users, or Everyone grants reading and executing the
/// object, and none of them denies it.
fn users_can_read_and_execute(handle: &Handle, path: &Path) -> anyhow::Result<bool> {
    const READ_EXECUTE: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;
    let mut dacl = std::ptr::null_mut::<ACL>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .with_context(|| format!("failed to read the security of {}", path.display()))?;
    let _descriptor = LocalDescriptor(descriptor);
    if dacl.is_null() {
        return Ok(true);
    }
    let (mut allowed, mut denied) = (0, 0);
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut ace = std::ptr::null_mut();
        unsafe { GetAce(dacl, index.into(), &mut ace) }
            .with_context(|| format!("failed to read the DACL of {}", path.display()))?;
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE.0 != 0
            || !matches!(
                header.AceType,
                ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE
            )
        {
            continue;
        }
        // Denied ACEs share the layout of allowed ones.
        let entry = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = PSID((&raw const entry.SidStart).cast_mut().cast());
        let applies = unsafe {
            IsWellKnownSid(sid, WinBuiltinUsersSid).as_bool()
                || IsWellKnownSid(sid, WinAuthenticatedUserSid).as_bool()
                || IsWellKnownSid(sid, WinWorldSid).as_bool()
        };
        if !applies {
            continue;
        }
        let mut rights = entry.Mask;
        if rights & GENERIC_ALL.0 != 0 {
            rights |= FILE_ALL_ACCESS.0;
        }
        if rights & GENERIC_READ.0 != 0 {
            rights |= FILE_GENERIC_READ.0;
        }
        if rights & GENERIC_EXECUTE.0 != 0 {
            rights |= FILE_GENERIC_EXECUTE.0;
        }
        if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
            allowed |= rights;
        } else {
            denied |= rights;
        }
    }
    Ok(allowed & READ_EXECUTE == READ_EXECUTE && denied & READ_EXECUTE == 0)
}

/// Reads `file` only if no account other than SYSTEM or an administrator could have written it or
/// swapped it into place, for state left by an older installation that is not re-secured before
/// it is read. The file and its directory must be owned by those accounts and grant nobody else
/// write access, and the directory's parent (the product folder under ProgramData) must not let
/// anyone else rename or replace the directory. None of them may be a reparse point. Returns
/// `Ok(None)` when any of them is missing and fails with [`UntrustedPath`] when one is untrusted.
pub fn read_protected_file(file: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let directory = file
        .parent()
        .with_context(|| format!("{} has no parent directory", file.display()))?;
    let product = directory
        .parent()
        .with_context(|| format!("{} has no parent directory", directory.display()))?;
    let _privileges = Privileges::enable()?;
    let entries = [
        (product, true, REPLACE_RIGHTS),
        (directory, true, MODIFY_RIGHTS),
        (file, false, MODIFY_RIGHTS),
    ];
    let mut opened = None;
    for (path, is_directory, forbidden) in entries {
        let access = if is_directory {
            READ_CONTROL
        } else {
            READ_CONTROL | FILE_READ_DATA
        };
        let Some((handle, information)) = inspect(path, access)? else {
            return Ok(None);
        };
        let untrusted = |reason| UntrustedPath {
            path: path.to_owned(),
            reason,
        };
        // Report planted links as untrusted rather than failing, since a standard user can
        // create entries in a product folder that inherited the ProgramData ACL.
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(untrusted("is a junction, symbolic link, or other reparse point").into());
        }
        if (information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0) != is_directory {
            return Err(untrusted(if is_directory {
                "is not a directory"
            } else {
                "is a directory"
            })
            .into());
        }
        ensure_protected(&handle, path, forbidden)?;
        opened = Some(handle);
    }
    let handle = opened.context("no file was opened")?;
    // Read through the verified handle so the file cannot be exchanged after the check.
    let mut contents = Vec::new();
    handle
        .into_file()
        .read_to_end(&mut contents)
        .with_context(|| format!("failed to read {}", file.display()))?;
    Ok(Some(contents))
}

/// Why a file from an older installation cannot be trusted.
#[derive(Debug)]
pub struct UntrustedPath {
    path: PathBuf,
    reason: &'static str,
}

impl std::fmt::Display for UntrustedPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.path.display(), self.reason)
    }
}

impl std::error::Error for UntrustedPath {}

/// Rights that let an account change a file or a directory's entries, or take over its security.
const MODIFY_RIGHTS: u32 = FILE_WRITE_DATA.0
    | FILE_APPEND_DATA.0
    | FILE_DELETE_CHILD.0
    | DELETE.0
    | WRITE_DAC.0
    | WRITE_OWNER.0
    | GENERIC_WRITE.0
    | GENERIC_ALL.0;
/// Rights that let an account rename or replace a directory's existing entries. ProgramData
/// passes its "create files and folders" grant for Users down to product folders, which alone
/// cannot move an entry that is already there.
const REPLACE_RIGHTS: u32 =
    FILE_DELETE_CHILD.0 | DELETE.0 | WRITE_DAC.0 | WRITE_OWNER.0 | GENERIC_ALL.0;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_ALLOWED_COMPOUND_ACE_TYPE: u8 = 4;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;

/// Opens `path` without following reparse points, or returns `None` if it does not exist.
fn inspect(
    path: &Path,
    access: FILE_ACCESS_RIGHTS,
) -> anyhow::Result<Option<(Handle, BY_HANDLE_FILE_INFORMATION)>> {
    match open_handle(path, access | FILE_READ_ATTRIBUTES) {
        Ok(opened) => Ok(Some(opened)),
        Err(error)
            if error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|error| {
                    error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                        || error.code() == ERROR_PATH_NOT_FOUND.to_hresult()
                }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Fails with [`UntrustedPath`] unless SYSTEM or an administrator owns the object and no ACE that
/// applies to it grants any `forbidden` right to another account.
fn ensure_protected(handle: &Handle, path: &Path, forbidden: u32) -> anyhow::Result<()> {
    let untrusted = |reason| UntrustedPath {
        path: path.to_owned(),
        reason,
    };
    let mut owner = PSID::default();
    let mut dacl = std::ptr::null_mut::<ACL>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .with_context(|| format!("failed to read the security of {}", path.display()))?;
    let _descriptor = LocalDescriptor(descriptor);
    if !is_trusted_account(owner)? {
        return Err(
            untrusted("is owned by an account other than SYSTEM or an administrator").into(),
        );
    }
    if dacl.is_null() {
        return Err(untrusted("has no DACL, so every account can modify it").into());
    }
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut ace = std::ptr::null_mut();
        unsafe { GetAce(dacl, index.into(), &mut ace) }
            .with_context(|| format!("failed to read the DACL of {}", path.display()))?;
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE.0 != 0 {
            continue;
        }
        let trusted = match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {
                // Callback ACEs only append condition data after the SID.
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                allowed.Mask & forbidden == 0
                    || is_trusted_account(PSID((&raw const allowed.SidStart).cast_mut().cast()))?
            }
            // Object and compound ACEs place the SID elsewhere; Agent files never carry them.
            ACCESS_ALLOWED_COMPOUND_ACE_TYPE
            | ACCESS_ALLOWED_OBJECT_ACE_TYPE
            | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
                unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() }.Mask & forbidden == 0
            }
            // Deny, audit, and label ACEs grant nothing.
            _ => true,
        };
        if !trusted {
            return Err(untrusted(
                "lets an account other than SYSTEM or an administrator modify it",
            )
            .into());
        }
    }
    Ok(())
}

fn create(path: &Path) -> windows::core::Result<()> {
    let descriptor = LocalDescriptor::parse(PRIVATE_DIRECTORY_SDDL)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    let path = wide(path.as_os_str());
    unsafe { CreateDirectoryW(PCWSTR(path.as_ptr()), Some(&attributes)) }
}

fn open_without_following(path: &Path) -> anyhow::Result<(Handle, BY_HANDLE_FILE_INFORMATION)> {
    open_handle(
        path,
        READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
    )
}

fn open_handle(
    path: &Path,
    access: FILE_ACCESS_RIGHTS,
) -> anyhow::Result<(Handle, BY_HANDLE_FILE_INFORMATION)> {
    let wide_path = wide(path.as_os_str());
    // Backup semantics open directories, and with the backup and restore privileges they also
    // bypass a squatter's DACL so its owner can be inspected and replaced.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            access.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map(Handle)
    .with_context(|| format!("failed to open {}", path.display()))?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle.0, &mut information) }
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    Ok((handle, information))
}

fn ensure_not_reparse_point(
    path: &Path,
    information: &BY_HANDLE_FILE_INFORMATION,
) -> anyhow::Result<()> {
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        bail!(
            "refusing to use {}: it is a junction, symbolic link, or other reparse point",
            path.display()
        );
    }
    Ok(())
}

fn ensure_plain_directory(
    path: &Path,
    information: &BY_HANDLE_FILE_INFORMATION,
) -> anyhow::Result<()> {
    ensure_not_reparse_point(path, information)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        bail!("refusing to use {}: it is not a directory", path.display());
    }
    Ok(())
}

fn owner_is_trusted(handle: &Handle, path: &Path) -> anyhow::Result<bool> {
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .with_context(|| format!("failed to read the owner of {}", path.display()))?;
    let _descriptor = LocalDescriptor(descriptor);
    is_trusted_account(owner)
}

fn is_trusted_account(sid: PSID) -> anyhow::Result<bool> {
    let trusted = unsafe {
        IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
    };
    // With the "object creator" default-owner policy, directories created by an elevated
    // administrator are owned by that user rather than by Administrators.
    Ok(trusted || is_current_user(sid)?)
}

fn is_current_user(sid: PSID) -> anyhow::Result<bool> {
    let token = process_token(TOKEN_QUERY)?;
    let mut length = 0;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
    let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }
    .context("failed to read the Agent process user")?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    Ok(unsafe { EqualSid(sid, user.User.Sid) }.is_ok())
}

fn apply(handle: &Handle, path: &Path, sddl: &str) -> anyhow::Result<()> {
    store(handle, sddl).with_context(|| format!("failed to secure {}", path.display()))?;
    verify(handle, path, sddl)
}

/// Stores exactly this owner and DACL, including the descriptor's protection bit. Unlike
/// SetSecurityInfo, the kernel call does not propagate into existing children, which could
/// otherwise follow a planted junction.
fn store(handle: &Handle, sddl: &str) -> windows::core::Result<()> {
    let descriptor = LocalDescriptor::parse(sddl)?;
    unsafe {
        SetKernelObjectSecurity(
            handle.0,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            descriptor.0,
        )
    }
}

fn verify(handle: &Handle, path: &Path, expected: &str) -> anyhow::Result<()> {
    let actual = sddl(handle).with_context(|| format!("failed to verify {}", path.display()))?;
    if actual != expected {
        bail!(
            "{} has security descriptor {actual} instead of {expected}",
            path.display()
        );
    }
    Ok(())
}

fn sddl(handle: &Handle) -> anyhow::Result<String> {
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            information,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    }
    .ok()?;
    let descriptor = LocalDescriptor(descriptor);
    let mut string = PWSTR::null();
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            SDDL_REVISION_1,
            information,
            &mut string,
            None,
        )
    }?;
    let text = unsafe { string.to_string() };
    unsafe {
        LocalFree(Some(HLOCAL(string.0.cast())));
    }
    Ok(text?)
}

fn process_token(access: windows::Win32::Security::TOKEN_ACCESS_MASK) -> anyhow::Result<Handle> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) }
        .context("failed to open the Agent process token")?;
    Ok(Handle(token))
}

/// Temporarily enables the backup and restore privileges that elevated administrators and
/// LocalSystem hold, restoring their previous state on drop. A missing privilege only means a
/// hostile DACL can make the later open fail. Privileges belong to the whole process token, so
/// guards are serialized to keep one caller from disabling them while another still needs them.
struct Privileges {
    token: Handle,
    previous: Vec<TOKEN_PRIVILEGES>,
    _serialized: MutexGuard<'static, ()>,
}

static PRIVILEGE_LOCK: Mutex<()> = Mutex::new(());

impl Privileges {
    fn enable() -> anyhow::Result<Self> {
        let serialized = PRIVILEGE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let token = process_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)?;
        let mut previous = Vec::new();
        for name in [SE_BACKUP_NAME, SE_RESTORE_NAME] {
            let mut luid = LUID::default();
            if unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) }.is_err() {
                continue;
            }
            let enabled = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut old = TOKEN_PRIVILEGES::default();
            let mut length = 0;
            if unsafe {
                AdjustTokenPrivileges(
                    token.0,
                    false,
                    Some(&enabled),
                    std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                    Some(&mut old),
                    Some(&mut length),
                )
            }
            .is_ok()
                && old.PrivilegeCount > 0
            {
                previous.push(old);
            }
        }
        Ok(Self {
            token,
            previous,
            _serialized: serialized,
        })
    }
}

impl Drop for Privileges {
    fn drop(&mut self) {
        for old in self.previous.iter().rev() {
            let _ = unsafe { AdjustTokenPrivileges(self.token.0, false, Some(old), 0, None, None) };
        }
    }
}

struct Handle(HANDLE);

impl Handle {
    fn into_file(self) -> std::fs::File {
        let handle = std::mem::ManuallyDrop::new(self);
        unsafe { std::fs::File::from_raw_handle(handle.0.0) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A descriptor allocated by the security APIs with LocalAlloc.
struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
}

impl LocalDescriptor {
    fn parse(sddl: &str) -> windows::core::Result<Self> {
        let sddl = wide(OsStr::new(sddl));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }?;
        Ok(Self(descriptor))
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use windows::Win32::Security::{TOKEN_ELEVATION, TokenElevation};

    pub const PRIVATE: &str = PRIVATE_DIRECTORY_SDDL;

    /// Replacing owners and bypassing hostile DACLs needs an elevated administrator.
    pub fn elevated() -> bool {
        let Ok(token) = process_token(TOKEN_QUERY) else {
            return false;
        };
        let mut elevation = TOKEN_ELEVATION::default();
        let mut length = 0;
        let queried = unsafe {
            GetTokenInformation(
                token.0,
                TokenElevation,
                Some((&raw mut elevation).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut length,
            )
        };
        if queried.is_ok() && elevation.TokenIsElevated != 0 {
            true
        } else {
            eprintln!("skipping: private directory tests require an elevated administrator");
            false
        }
    }

    pub fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "meshrmm-private-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    pub fn sddl_of(path: &Path) -> String {
        let _privileges = Privileges::enable().unwrap();
        let (handle, _) = open_without_following(path).unwrap();
        sddl(&handle).unwrap()
    }

    /// Stores `descriptor` exactly, as a squatter or an older installer might have left it.
    pub fn set_sddl(path: &Path, descriptor: &str) {
        let _privileges = Privileges::enable().unwrap();
        let (handle, _) = open_without_following(path).unwrap();
        store(&handle, descriptor).unwrap();
    }

    pub fn remove(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    const FOREIGN_OWNER: &str = "O:BUD:(A;OICI;FA;;;BU)(A;OICI;FA;;;BA)";

    #[test]
    fn takes_over_administrator_directory_and_resets_planted_contents() {
        if !elevated() {
            return;
        }
        let root = scratch("takeover");
        let credential = root.join("agent.json");
        let updates = root.join("updates");
        let helper = updates.join("update-helper.exe");
        std::fs::write(&credential, b"{}").unwrap();
        std::fs::create_dir(&updates).unwrap();
        std::fs::write(&helper, b"MZ").unwrap();
        // Everyone was granted access explicitly, and the planted entries are owned by Users
        // and deny Administrators, as an account racing an older installer could leave them.
        set_sddl(
            &root,
            "O:BAD:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)",
        );
        set_sddl(&helper, "O:BUD:P(D;;FA;;;BA)(A;;FA;;;BU)");
        set_sddl(&updates, "O:BUD:P(D;OICI;FA;;;BA)(A;OICI;FA;;;BU)");
        set_sddl(&credential, "O:BUD:P(D;;FA;;;BA)(A;;FA;;;BU)");

        secure(&root).unwrap();
        assert_eq!(sddl_of(&root), PRIVATE_DIRECTORY_SDDL);
        secure_contents(&root).unwrap();
        assert_eq!(sddl_of(&credential), INHERITED_FILE_SDDL);
        assert_eq!(sddl_of(&updates), INHERITED_DIRECTORY_SDDL);
        assert_eq!(sddl_of(&helper), INHERITED_FILE_SDDL);
        // Securing an already private directory is idempotent.
        secure(&updates).unwrap();
        assert_eq!(sddl_of(&updates), PRIVATE_DIRECTORY_SDDL);
        remove(&root);
    }

    #[test]
    fn refuses_directory_owned_by_another_account() {
        if !elevated() {
            return;
        }
        let root = scratch("foreign");
        set_sddl(&root, FOREIGN_OWNER);
        let before = sddl_of(&root);
        assert!(before.starts_with("O:BU"), "{before}");
        let error = secure(&root).unwrap_err();
        assert!(
            error.downcast_ref::<UntrustedOwner>().is_some(),
            "{error:#}"
        );
        assert_eq!(sddl_of(&root), before);
        set_sddl(&root, PRIVATE_DIRECTORY_SDDL);
        remove(&root);
    }

    #[test]
    fn creates_missing_directories_and_new_ones_must_not_exist() {
        if !elevated() {
            return;
        }
        let root = scratch("create");
        let missing = root.join("Agent");
        secure(&missing).unwrap();
        assert_eq!(sddl_of(&missing), PRIVATE_DIRECTORY_SDDL);
        assert!(create_new(&missing).is_err());
        let fresh = root.join("staging");
        create_new(&fresh).unwrap();
        assert_eq!(sddl_of(&fresh), PRIVATE_DIRECTORY_SDDL);
        remove(&root);
    }

    #[test]
    fn reads_legacy_files_only_when_administrators_control_them() {
        if !elevated() {
            return;
        }
        let root = scratch("legacy");
        let product = root.join("PulseRMM");
        let directory = product.join("Agent");
        let file = directory.join("agent.json");
        assert!(read_protected_file(&file).unwrap().is_none());
        std::fs::create_dir_all(&directory).unwrap();
        assert!(read_protected_file(&file).unwrap().is_none());
        std::fs::write(&file, b"{}").unwrap();
        // As a legacy installation leaves them: the product folder keeps what ProgramData passes
        // down, including the Users grant to create entries, and icacls protected the Agent folder.
        const PRODUCT: &str =
            "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;CI;0x116;;;BU)(A;OICIIO;FA;;;CO)";
        const DIRECTORY: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
        const FILE: &str = "O:BAD:(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;;FR;;;BU)";
        set_sddl(&product, PRODUCT);
        set_sddl(&directory, DIRECTORY);
        set_sddl(&file, FILE);
        assert_eq!(read_protected_file(&file).unwrap().unwrap(), b"{}");

        let untrusted = |path: &Path, descriptor: &str, original: &str| {
            set_sddl(path, descriptor);
            let error = read_protected_file(&file).unwrap_err();
            let reason = error
                .downcast_ref::<UntrustedPath>()
                .unwrap_or_else(|| panic!("{error:#}"));
            assert_eq!(reason.path, path);
            set_sddl(path, original);
        };
        untrusted(&file, "O:BUD:(A;;FA;;;SY)(A;;FA;;;BA)", FILE);
        untrusted(
            &file,
            "O:BAD:(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x12019f;;;BU)",
            FILE,
        );
        untrusted(&file, "O:BAD:NO_ACCESS_CONTROL", FILE);
        untrusted(
            &directory,
            "O:BUD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            DIRECTORY,
        );
        untrusted(
            &directory,
            "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;CI;0x116;;;BU)",
            DIRECTORY,
        );
        untrusted(&product, "O:BUD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)", PRODUCT);
        untrusted(
            &product,
            "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x40;;;BU)",
            PRODUCT,
        );
        assert_eq!(read_protected_file(&file).unwrap().unwrap(), b"{}");

        // A planted junction is reported as untrusted without reading through it.
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("agent.json"), b"{}").unwrap();
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&directory)
            .arg(&target)
            .output()
            .unwrap()
            .status;
        assert!(status.success());
        let error = read_protected_file(&file).unwrap_err();
        let reason = error.downcast_ref::<UntrustedPath>().unwrap();
        assert_eq!(reason.path, directory);
        remove(&root);
    }

    #[test]
    fn refuses_junctions_without_changing_their_targets() {
        if !elevated() {
            return;
        }
        let root = scratch("junction");
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        let original = sddl_of(&target);
        let junction = |link: &Path| {
            let status = std::process::Command::new("cmd.exe")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(link)
                .arg(&target)
                .output()
                .unwrap()
                .status;
            assert!(status.success());
        };

        let link = root.join("link");
        junction(&link);
        let error = secure(&link).unwrap_err();
        assert!(format!("{error:#}").contains("reparse point"), "{error:#}");

        let private = root.join("private");
        secure(&private).unwrap();
        junction(&private.join("identity"));
        let error = secure_contents(&private).unwrap_err();
        assert!(format!("{error:#}").contains("reparse point"), "{error:#}");
        assert_eq!(sddl_of(&target), original);
        remove(&root);
    }

    #[test]
    fn installed_files_inherit_their_new_directory_security() {
        if !elevated() {
            return;
        }
        // Like Program Files, the install directory lets Users read and execute its files.
        let install = scratch("install");
        set_sddl(
            &install,
            "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        );
        let agent = install.join("meshrmm-agent.exe");
        std::fs::write(&agent, b"MZ").unwrap();
        // A file renamed out of the private update directory keeps what it inherited there.
        set_sddl(&agent, INHERITED_FILE_SDDL);
        inherit_parent_security(&agent).unwrap();
        let installed = sddl_of(&agent);
        assert!(installed.contains("(A;ID;0x1200a9;;;BU)"), "{installed}");
        assert!(!installed.contains("D:P"), "{installed}");

        let private = scratch("private-install");
        set_sddl(&private, PRIVATE_DIRECTORY_SDDL);
        let hidden = private.join("meshrmm-agent.exe");
        std::fs::write(&hidden, b"MZ").unwrap();
        let error = inherit_parent_security(&hidden).unwrap_err();
        assert!(format!("{error:#}").contains("Users read"), "{error:#}");
        remove(&install);
        remove(&private);
    }
}
