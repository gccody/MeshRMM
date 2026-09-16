//! Session-scoped power and idle activity. No machine policy or timeout is changed.
use std::{
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::Win32::{
    System::{Power::*, SystemInformation::GetTickCount},
    UI::Input::KeyboardAndMouse::*,
};

pub struct KeepAwake {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

pub fn set_enabled(current: &mut Option<KeepAwake>, enabled: bool) -> anyhow::Result<()> {
    if enabled && current.is_none() {
        *current = Some(KeepAwake::new()?);
    } else if !enabled {
        *current = None;
    }
    Ok(())
}

impl KeepAwake {
    fn new() -> anyhow::Result<Self> {
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("session-keep-awake".into())
            .spawn(move || {
                let previous = unsafe {
                    SetThreadExecutionState(
                        ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED,
                    )
                };
                if previous.0 == 0 {
                    let _ = ready.send(Err("Windows rejected the keep-awake request"));
                    return;
                }
                let _power = PowerRequest;
                tracing::info!("session idle-lock prevention enabled");
                if ready.send(Ok(())).is_ok() {
                    while stopped.recv_timeout(Duration::from_secs(1))
                        == Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        // Zero movement resets user-idle accounting without moving the cursor,
                        // clicking, typing, or claiming input ownership. A locked/secure desktop
                        // can reject injected input; it is never unlocked by this helper.
                        let _ = pulse();
                    }
                }
                tracing::info!("session idle-lock prevention disabled");
            })?;
        match started.recv()? {
            Ok(()) => Ok(Self {
                stop: Some(stop),
                thread: Some(thread),
            }),
            Err(message) => {
                let _ = thread.join();
                anyhow::bail!(message);
            }
        }
    }
}
impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct PowerRequest;
impl Drop for PowerRequest {
    fn drop(&mut self) {
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

fn pulse() -> bool {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return false;
    }
    if unsafe { GetTickCount() }.wrapping_sub(info.dwTime) < 1000 {
        return true;
    }
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_MOVE,
                dwExtraInfo: super::input_block::KEEP_AWAKE_TAG,
                ..Default::default()
            },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an idle interactive Windows desktop; observes the session idle timer"]
    fn live_keep_awake_resets_idle_without_moving_and_stops_on_drop() {
        use windows::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
        fn last_input() -> u32 {
            let mut info = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                ..Default::default()
            };
            assert!(unsafe { GetLastInputInfo(&mut info) }.as_bool());
            info.dwTime
        }
        let mut before = POINT::default();
        unsafe {
            GetCursorPos(&mut before).unwrap();
        }
        let original = last_input();
        let guard = KeepAwake::new().unwrap();
        thread::sleep(Duration::from_millis(2400));
        assert_ne!(last_input(), original);
        let mut after = POINT::default();
        unsafe {
            GetCursorPos(&mut after).unwrap();
        }
        assert_eq!((before.x, before.y), (after.x, after.y));
        drop(guard);
        // Drain any already-queued input before asserting the helper has stopped.
        thread::sleep(Duration::from_millis(100));
        let stopped = last_input();
        thread::sleep(Duration::from_millis(1400));
        assert_eq!(last_input(), stopped);
    }
}
