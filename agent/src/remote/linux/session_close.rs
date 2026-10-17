//! The user on the console and the close actions run in their session:
//! locking the screen, logging out and clearing the clipboard. The installed
//! coordinator asks logind to lock and runs the rest through the session's
//! helper; a console Agent already runs in that session.
use anyhow::Context;

use super::sessions::Logind;
use crate::remote::helper::protocol::Request;

/// The signed-in user on the console, or `None` at a login screen.
pub(crate) fn console_user() -> Option<u32> {
    super::sessions::console_user_session().map(|session| session.uid)
}

/// The console user's ID, a number unique to their sign-in, and their name.
pub(crate) fn console_logon() -> anyhow::Result<(u32, u64, String)> {
    let session =
        super::sessions::console_user_session().context("nobody is signed in on the console")?;
    Ok((session.uid, logon_number(&session.id), session.user))
}

/// logind session IDs are short strings, such as `2` or `c1`, that are not
/// reused until the computer restarts.
fn logon_number(id: &str) -> u64 {
    id.parse().unwrap_or_else(|_| {
        id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    })
}

pub(crate) fn clear_clipboard(uid: u32) -> anyhow::Result<()> {
    in_session(uid, Request::ClearClipboard, clear_clipboard_here)
}

/// Locks `uid`'s console session. Root asks logind, which tells the
/// session's screen locker.
pub(crate) fn lock(uid: u32) -> anyhow::Result<()> {
    if crate::remote::helper::coordinator::registry().is_ok() {
        let session = super::sessions::console_user_session()
            .filter(|session| session.uid == uid)
            .context("the user is no longer on the console")?;
        return Logind::connect()?.lock_session(&session.id);
    }
    lock_here()
}

pub(crate) fn log_out(uid: u32) -> anyhow::Result<()> {
    in_session(uid, Request::LogOut, log_out_here)
}

/// Runs an action in `uid`'s session: through their session helper for the
/// installed coordinator, or in this process, which runs in that session.
fn in_session(uid: u32, request: Request, here: fn() -> anyhow::Result<()>) -> anyhow::Result<()> {
    match crate::remote::helper::coordinator::registry() {
        Ok(registry) => registry.user_helper(uid)?.run(request),
        Err(_) => here(),
    }
}

pub(crate) fn clear_clipboard_here() -> anyhow::Result<()> {
    arboard::Clipboard::new()
        .context("the clipboard is unavailable")?
        .clear()
        .context("could not clear the clipboard")
}

/// This session's logind object.
fn own_session(bus: &zbus::blocking::Connection) -> anyhow::Result<zbus::blocking::Proxy<'static>> {
    let id = std::env::var("XDG_SESSION_ID").context("this process has no logind session")?;
    let manager = zbus::blocking::Proxy::new(
        bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSession", &(id,))?;
    Ok(zbus::blocking::Proxy::new(
        bus,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )?)
}

/// Locks the screen at once, through logind, which tells the screen locker.
pub(crate) fn lock_here() -> anyhow::Result<()> {
    let bus = zbus::blocking::Connection::system()?;
    own_session(&bus)?
        .call_method("Lock", &())
        .context("logind could not lock the session")?;
    Ok(())
}

/// Logs out the way the desktop's own Log Out does without asking, so
/// applications can save their work; where the desktop has no such request,
/// logind ends the session.
pub(crate) fn log_out_here() -> anyhow::Result<()> {
    let session_bus = zbus::blocking::Connection::session().ok();
    if let Some(bus) = &session_bus {
        // GNOME: mode 1 logs out without the confirmation dialog.
        if bus
            .call_method(
                Some("org.gnome.SessionManager"),
                "/org/gnome/SessionManager",
                Some("org.gnome.SessionManager"),
                "Logout",
                &(1_u32,),
            )
            .is_ok()
        {
            return Ok(());
        }
        // KDE Plasma.
        if bus
            .call_method(
                Some("org.kde.Shutdown"),
                "/Shutdown",
                Some("org.kde.Shutdown"),
                "logout",
                &(),
            )
            .is_ok()
        {
            return Ok(());
        }
    }
    // Xfce.
    if std::process::Command::new("xfce4-session-logout")
        .args(["--logout", "--fast"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }
    let bus = zbus::blocking::Connection::system()?;
    own_session(&bus)?
        .call_method("Terminate", &())
        .context("logind could not end the session")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_stable_numbers() {
        assert_eq!(logon_number("12"), 12);
        assert_eq!(logon_number("c1"), logon_number("c1"));
        assert_ne!(logon_number("c1"), logon_number("c2"));
    }
}
