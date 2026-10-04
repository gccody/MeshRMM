//! The Mac user on the console and the close actions run in their session:
//! locking the screen, logging out and clearing the clipboard. The installed
//! coordinator runs them through the user's session helper; a console Agent
//! already runs in that session.
use std::ffi::CStr;

use anyhow::{Context, bail};

/// The signed-in user on the console, or `None` at the login window.
pub(crate) fn console_user() -> Option<u32> {
    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCopyConsoleUser(
            store: *const std::ffi::c_void,
            uid: *mut u32,
            gid: *mut u32,
        ) -> Option<std::ptr::NonNull<objc2_core_foundation::CFString>>;
    }
    let mut uid = 0;
    // SAFETY: a null store is allowed and both out pointers are valid.
    let name =
        unsafe { SCDynamicStoreCopyConsoleUser(std::ptr::null(), &mut uid, std::ptr::null_mut()) }?;
    // SAFETY: the function returns a +1 reference.
    let name = unsafe { objc2_core_foundation::CFRetained::from_raw(name) };
    (name.to_string() != "loginwindow" && uid != 0).then_some(uid)
}

/// The console user's ID, a number unique to their sign-in, and their name.
pub(crate) fn console_logon() -> anyhow::Result<(u32, u64, String)> {
    let uid = console_user().context("nobody is signed in on the console")?;
    let started = loginwindow_start(uid).context("the console user has no loginwindow")?;
    Ok((uid, started, user_name(uid)))
}

/// When `uid`'s loginwindow, which lives as long as their sign-in, started.
fn loginwindow_start(uid: u32) -> Option<u64> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0 as libc::pid_t; usize::try_from(count).ok()? + 64];
    // SAFETY: the buffer holds as many bytes as passed.
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    pids.truncate(usize::try_from(count).ok()?);
    pids.into_iter().find_map(|pid| {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` holds `size` bytes.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        // SAFETY: proc_pidinfo filled in the structure.
        let info = unsafe { info.assume_init() };
        // SAFETY: pbi_comm is NUL-terminated within its array.
        let command = unsafe { CStr::from_ptr(info.pbi_comm.as_ptr()) };
        (info.pbi_uid == uid && command.to_bytes() == b"loginwindow")
            .then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    })
}

fn user_name(uid: u32) -> String {
    // SAFETY: getpwuid returns a pointer to static storage or null.
    let entry = unsafe { libc::getpwuid(uid) };
    // SAFETY: a non-null entry has a NUL-terminated name.
    unsafe { entry.as_ref() }
        .map(|entry| {
            unsafe { CStr::from_ptr(entry.pw_name) }
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| format!("user {uid}"))
}

pub(crate) fn clear_clipboard(uid: u32) -> anyhow::Result<()> {
    in_session(
        uid,
        super::helper::protocol::Request::ClearClipboard,
        clear_clipboard_here,
    )
}

pub(crate) fn lock(uid: u32) -> anyhow::Result<()> {
    in_session(uid, super::helper::protocol::Request::LockScreen, lock_here)
}

pub(crate) fn log_out(uid: u32) -> anyhow::Result<()> {
    in_session(uid, super::helper::protocol::Request::LogOut, log_out_here)
}

/// Runs an action in `uid`'s session: through their session helper for the
/// installed coordinator, or in this process, which runs in that session.
fn in_session(
    uid: u32,
    request: super::helper::protocol::Request,
    here: fn() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match super::helper::coordinator::registry() {
        Ok(registry) => registry.user_helper(uid)?.run(request),
        Err(_) => here(),
    }
}

pub(crate) fn clear_clipboard_here() -> anyhow::Result<()> {
    super::on_main(|_| {
        objc2_app_kit::NSPasteboard::generalPasteboard().clearContents();
    });
    Ok(())
}

/// Locks the screen at once, as the Lock Screen menu command does.
pub(crate) fn lock_here() -> anyhow::Result<()> {
    const LOGIN: &CStr =
        c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login";
    // SAFETY: dlopen and dlsym only read the named library and symbol.
    unsafe {
        let library = libc::dlopen(LOGIN.as_ptr(), libc::RTLD_LAZY);
        if library.is_null() {
            bail!("macOS has no screen lock interface");
        }
        let symbol = libc::dlsym(library, c"SACLockScreenImmediate".as_ptr());
        if symbol.is_null() {
            bail!("macOS has no screen lock interface");
        }
        let lock: extern "C" fn() -> i32 = std::mem::transmute(symbol);
        let status = lock();
        anyhow::ensure!(status == 0, "macOS could not lock the screen ({status})");
    }
    Ok(())
}

/// Logs out without the confirmation dialog, as Option-Log Out does.
/// Applications with unsaved changes can still ask the user first.
pub(crate) fn log_out_here() -> anyhow::Result<()> {
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", "tell application \"loginwindow\" to «event aevtrlgo»"])
        .output()
        .context("could not start the log out command")?;
    anyhow::ensure!(
        output.status.success(),
        "macOS could not log out: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a signed-in console session"]
    fn finds_the_console_users_sign_in() {
        // The test runs in a signed-in session.
        let (uid, logon, user) = console_logon().unwrap();
        assert_eq!(uid, unsafe { libc::getuid() });
        assert!(logon > 0);
        assert!(!user.is_empty());
        assert_eq!(
            console_logon().unwrap().1,
            logon,
            "a sign-in keeps its number"
        );
    }
}
