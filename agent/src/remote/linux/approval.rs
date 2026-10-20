//! The Linux connection approval prompt: a dialog with the message, the
//! technician's reason, and Accept and Deny buttons, from zenity or kdialog.
//! The policy answers for the user when the session has been locked, or is
//! at the login screen, and idle, or when nobody answers in time.
use std::time::{Duration, Instant};

use crate::remote::connection_approval::{
    ApprovalPrompt, Decision, automatic_decision, remaining_seconds,
};

const TITLE: &str = "Remote connection request";
const POLL: Duration = Duration::from_millis(250);

/// Asks the user of this session to accept the connection. Returns `None`
/// when `cancelled` says the caller gave up.
pub(crate) fn ask(prompt: &ApprovalPrompt, cancelled: impl Fn() -> bool) -> Option<Decision> {
    let started = Instant::now();
    let lock_state = LockState::connect();
    let automatic = || {
        let (locked, idle) = lock_state.read();
        automatic_decision(prompt, started.elapsed(), locked, idle)
    };
    if let Some(decision) = automatic() {
        return Some(decision);
    }
    let mut text = prompt.text.clone();
    if !prompt.reason.is_empty() {
        text.push_str("\n\nReason: ");
        text.push_str(&prompt.reason);
    }
    text.push_str(&format!(
        "\n\nThe connection is accepted automatically in {} seconds.",
        remaining_seconds(prompt.timeout, started.elapsed())
    ));
    let mut dialog = super::ui::dialog::question(TITLE, &text, "Accept", "Deny")
        .inspect_err(
            |error| tracing::warn!(error = ?error, "could not show the connection approval prompt"),
        )
        .ok();
    let answer = loop {
        if let Some(child) = &mut dialog
            && let Ok(Some(status)) = child.try_wait()
        {
            dialog = None;
            // Closing the dialog is not an answer; the policy decides then.
            match status.code() {
                Some(0) => break Some(Decision::Accepted),
                Some(1) => break Some(Decision::Declined),
                _ => {}
            }
        }
        if cancelled() {
            break None;
        }
        if let Some(decision) = automatic() {
            break Some(decision);
        }
        std::thread::sleep(POLL);
    };
    if let Some(mut child) = dialog {
        let _ = child.kill();
        let _ = child.wait();
    }
    answer
}

/// Reads whether this session is locked, and how long it has had no input.
/// A login screen counts as locked, as on Windows. logind has the lock and
/// idle hints the screen locker and the desktop set, but a login screen sets
/// neither, and a desktop's idle hint starts only after its own idle delay,
/// so an X11 session uses the X server's idle time instead.
struct LockState {
    session: Option<zbus::blocking::Proxy<'static>>,
    x11: Option<(x11rb::rust_connection::RustConnection, u32)>,
}

impl LockState {
    fn connect() -> Self {
        use x11rb::connection::Connection;
        let session = std::env::var("XDG_SESSION_ID")
            .ok()
            .and_then(|id| logind_session(id).inspect_err(|error| tracing::debug!(error = ?error, "could not read the session from logind")).ok());
        let x11 = (std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("x11"))
            .then(|| x11rb::rust_connection::RustConnection::connect(None).ok())
            .flatten()
            .map(|(connection, screen)| {
                let root = connection.setup().roots[screen].root;
                (connection, root)
            });
        Self { session, x11 }
    }

    fn read(&self) -> (bool, Duration) {
        let (locked, logind_idle) = self
            .session
            .as_ref()
            .and_then(|session| read_logind(session).ok())
            .unwrap_or((false, Duration::ZERO));
        let idle = self.x11_idle().unwrap_or(logind_idle);
        (locked, idle)
    }

    fn x11_idle(&self) -> Option<Duration> {
        use x11rb::protocol::screensaver::ConnectionExt;
        let (connection, root) = self.x11.as_ref()?;
        let info = connection
            .screensaver_query_info(*root)
            .ok()?
            .reply()
            .ok()?;
        Some(Duration::from_millis(info.ms_since_user_input.into()))
    }
}

fn logind_session(id: String) -> anyhow::Result<zbus::blocking::Proxy<'static>> {
    let bus = zbus::blocking::Connection::system()?;
    let manager = zbus::blocking::Proxy::new(
        &bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSession", &(id,))?;
    Ok(zbus::blocking::Proxy::new_owned(
        bus,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )?)
}

/// Whether logind has the session locked or at a login screen, and how long
/// it has had the session idle.
fn read_logind(session: &zbus::blocking::Proxy<'_>) -> anyhow::Result<(bool, Duration)> {
    let locked: bool = session.get_property("LockedHint")?;
    let class: String = session.get_property("Class")?;
    let idle: bool = session.get_property("IdleHint")?;
    let since: u64 = session.get_property("IdleSinceHintMonotonic")?;
    let idle_for = if idle && since > 0 {
        Duration::from_micros(super::monotonic_timestamp_us().saturating_sub(since))
    } else {
        Duration::ZERO
    };
    Ok((locked || class == "greeter", idle_for))
}
