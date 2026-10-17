//! The Linux connection approval prompt: a dialog with the message, the
//! technician's reason, and Accept and Deny buttons, from zenity or kdialog.
//! The policy answers for the user when the session has been locked and
//! idle, or when nobody answers in time, as logind reports both.
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
    let automatic = || {
        let (locked, idle) = lock_state();
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

/// Whether this session is locked, and how long it has had no input, from
/// logind's hints, which the screen locker and the display server set.
fn lock_state() -> (bool, Duration) {
    let Some(session) = std::env::var("XDG_SESSION_ID").ok() else {
        return (false, Duration::ZERO);
    };
    let read = || -> anyhow::Result<(bool, Duration)> {
        let bus = zbus::blocking::Connection::system()?;
        let manager = zbus::blocking::Proxy::new(
            &bus,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )?;
        let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSession", &(session,))?;
        let session = zbus::blocking::Proxy::new(
            &bus,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )?;
        let locked: bool = session.get_property("LockedHint")?;
        let idle: bool = session.get_property("IdleHint")?;
        let since: u64 = session.get_property("IdleSinceHintMonotonic")?;
        let idle_for = if idle && since > 0 {
            Duration::from_micros(super::monotonic_timestamp_us().saturating_sub(since))
        } else {
            Duration::ZERO
        };
        Ok((locked, idle_for))
    };
    read().unwrap_or((false, Duration::ZERO))
}
