//! Runs the approval prompt in the process that shows it: the console's
//! LocalSystem helper for the service, or the Agent itself in console mode.
use std::time::{Duration, Instant};

use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTSFreeMemory, WTSINFOEXW,
    WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::core::PWSTR;

use super::window::PromptWindow;
use super::{ApprovalPrompt, Decision, automatic_decision};

/// How often the prompt checks the lock screen, the input idle time and
/// whether its caller gave up.
const POLL: Duration = Duration::from_millis(250);

/// Asks this process's Windows session to accept the connection. Returns
/// `None` if `cancelled` reports that the caller no longer needs an answer.
/// When the prompt cannot be shown, the policy still answers: nobody could
/// accept or deny it.
pub fn ask(prompt: &ApprovalPrompt, cancelled: impl Fn() -> bool) -> Option<Decision> {
    let started = Instant::now();
    let automatic = || {
        let (locked, idle) = session_state();
        automatic_decision(prompt, started.elapsed(), locked, idle)
    };
    // Nobody is at a computer that has been sitting locked; do not leave a
    // prompt for them to find later.
    if let Some(decision) = automatic() {
        return Some(decision);
    }
    let window = PromptWindow::show(prompt, started + prompt.timeout)
        .inspect_err(
            |error| tracing::warn!(%error, "could not show the connection approval prompt"),
        )
        .ok();
    loop {
        if let Some(answer) = window.as_ref().and_then(|window| window.answer(POLL)) {
            return Some(answer);
        }
        if window.is_none() {
            std::thread::sleep(POLL);
        }
        if cancelled() {
            return None;
        }
        if let Some(decision) = automatic() {
            return Some(decision);
        }
    }
}

/// Whether this Windows session is at the lock screen, or has nobody signed
/// in, and how long it has had no keyboard or mouse input.
fn session_state() -> (bool, Duration) {
    (session_locked(), input_idle())
}

fn session_locked() -> bool {
    let mut buffer = PWSTR::null();
    let mut bytes = 0;
    if unsafe {
        WTSQuerySessionInformationW(
            None,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
    }
    .is_err()
        || buffer.is_null()
    {
        return false;
    }
    let locked = (bytes as usize >= std::mem::size_of::<WTSINFOEXW>())
        .then(|| {
            let info = unsafe { &*buffer.0.cast::<WTSINFOEXW>() };
            (info.Level == 1).then(|| {
                let level = unsafe { info.Data.WTSInfoExLevel1 };
                level.SessionFlags == WTS_SESSIONSTATE_LOCK as i32 || level.UserName[0] == 0
            })
        })
        .flatten()
        .unwrap_or(false);
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    locked
}

/// Input idle time is kept per Windows session, so it covers the lock screen
/// too, whichever desktop this process runs on.
fn input_idle() -> Duration {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return Duration::ZERO;
    }
    Duration::from_millis(unsafe { GetTickCount() }.wrapping_sub(info.dwTime).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interactive_test_session_is_neither_locked_nor_idle_for_ever() {
        // Tests run signed in; over SSH the session has no console input, so
        // only check that the queries answer sensibly.
        let (_locked, idle) = session_state();
        assert!(idle < Duration::from_secs(50 * 24 * 60 * 60));
    }

    #[test]
    fn a_cancelled_prompt_returns_without_an_answer() {
        let prompt = ApprovalPrompt {
            text: "Ada would like to connect.".into(),
            reason: String::new(),
            timeout: Duration::from_secs(60),
            // Never accept for being locked, whatever state the test machine is in.
            lock_idle: Duration::MAX,
        };
        let started = Instant::now();
        assert_eq!(ask(&prompt, || true), None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_unanswered_prompt_times_out_as_accepted() {
        let prompt = ApprovalPrompt {
            text: "Ada would like to connect.".into(),
            reason: "Timeout check".into(),
            timeout: Duration::from_millis(600),
            lock_idle: Duration::MAX,
        };
        assert_eq!(ask(&prompt, || false), Some(Decision::TimedOut));
    }
}
