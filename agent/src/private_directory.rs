//! Administrator-only directories for state that the LocalSystem Agent later trusts.
//!
//! Standard users can create folders under ProgramData and then own them, and an owner can
//! rewrite the DACL at any time. These helpers only reuse directories owned by SYSTEM or an
//! administrator, replace the owner and DACL through a handle that never follows reparse points,
//! and verify the stored security descriptor before callers write credentials or executables.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, LUID, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, EqualSid, GetTokenInformation,
    IsWellKnownSid, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME,
    SECURITY_ATTRIBUTES, SetKernelObjectSecurity, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
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
    let wide_path = wide(path.as_os_str());
    // Backup semantics open directories, and with the backup and restore privileges they also
    // bypass a squatter's DACL so its owner can be inspected and replaced.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            (READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES).0,
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
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };
    // With the "object creator" default-owner policy, directories created by an elevated
    // administrator are owned by that user rather than by Administrators.
    Ok(trusted || current_user_owns(owner)?)
}

fn current_user_owns(owner: PSID) -> anyhow::Result<bool> {
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
    Ok(unsafe { EqualSid(owner, user.User.Sid) }.is_ok())
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
}
