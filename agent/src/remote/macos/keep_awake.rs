//! Keeps the Mac awake and unlocked while a technician works, for the session
//! only. A power assertion keeps the display from sleeping, and declared user
//! activity keeps the screen saver, and the lock that follows it, away.
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::bail;
use objc2_core_foundation::CFString;

/// How often activity is declared; well under the shortest screen saver delay.
const ACTIVITY_INTERVAL: Duration = Duration::from_secs(30);
/// kIOPMAssertionLevelOn
const ASSERTION_ON: u32 = 255;
/// kIOPMUserActiveLocal
const USER_ACTIVE_LOCAL: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: &CFString,
        level: u32,
        name: &CFString,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: &CFString, user_type: u32, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

pub(crate) struct KeepAwake {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn set_enabled(current: &mut Option<KeepAwake>, enabled: bool) -> anyhow::Result<()> {
    if enabled && current.is_none() {
        *current = Some(KeepAwake::new()?);
    } else if !enabled {
        *current = None;
    }
    Ok(())
}

impl KeepAwake {
    fn new() -> anyhow::Result<Self> {
        let name = CFString::from_static_str("MeshRMM remote session");
        let mut display = 0;
        // SAFETY: both strings are valid and `display` is a valid out pointer.
        let status = unsafe {
            IOPMAssertionCreateWithName(
                &CFString::from_static_str("PreventUserIdleDisplaySleep"),
                ASSERTION_ON,
                &name,
                &mut display,
            )
        };
        if status != 0 {
            bail!("macOS rejected the keep-awake request ({status:#x})");
        }
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("session-keep-awake".into())
            .spawn(move || {
                tracing::info!("session idle-lock prevention enabled");
                let name = CFString::from_static_str("MeshRMM remote session");
                let mut activity = 0;
                loop {
                    // SAFETY: `name` is valid and `activity` is a valid out
                    // pointer; repeated declarations reuse the assertion.
                    unsafe {
                        IOPMAssertionDeclareUserActivity(&name, USER_ACTIVE_LOCAL, &mut activity)
                    };
                    if stopped.recv_timeout(ACTIVITY_INTERVAL)
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
                // SAFETY: both assertions were created above.
                unsafe {
                    if activity != 0 {
                        IOPMAssertionRelease(activity);
                    }
                    IOPMAssertionRelease(display);
                }
                tracing::info!("session idle-lock prevention disabled");
            });
        match thread {
            Ok(thread) => Ok(Self {
                stop: Some(stop),
                thread: Some(thread),
            }),
            Err(error) => {
                // SAFETY: the assertion was created above.
                unsafe { IOPMAssertionRelease(display) };
                Err(error.into())
            }
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
