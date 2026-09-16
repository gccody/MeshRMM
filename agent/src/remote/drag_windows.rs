//! Temporarily use window outlines while a remote viewer is connected.
use std::{sync::mpsc, thread};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        UI::WindowsAndMessaging::{
            SPI_GETDRAGFULLWINDOWS, SPI_SETDRAGFULLWINDOWS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            SystemParametersInfoW,
        },
    },
    core::{BOOL, w},
};

// Console-mode capture can move between runtime threads. Keep mutex ownership
// and restoration on one thread, just as the service's interactive helper does.
pub struct ConsoleOutlineDragging {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl ConsoleOutlineDragging {
    pub fn new() -> anyhow::Result<Self> {
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("meshrmm-outline-dragging".into())
            .spawn(move || match OutlineDragging::new() {
                Ok(_original) => {
                    let _ = ready.send(Ok(()));
                    let _ = stopped.recv();
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })?;
        match started.recv()? {
            Ok(()) => Ok(Self {
                stop: Some(stop),
                thread: Some(thread),
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}
impl Drop for ConsoleOutlineDragging {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct OutlineDragging {
    original: bool,
    _exclusive: ExclusiveDesktop,
}

impl OutlineDragging {
    pub fn new() -> anyhow::Result<Self> {
        // A replacement helper must not save the previous helper's temporary value.
        let exclusive = ExclusiveDesktop::acquire()?;
        let original = get_full_windows()?;
        set_full_windows(false)?;
        tracing::info!(
            original,
            "window contents while dragging disabled for remote session"
        );
        Ok(Self {
            original,
            _exclusive: exclusive,
        })
    }
}

impl Drop for OutlineDragging {
    fn drop(&mut self) {
        match set_full_windows(self.original) {
            Ok(()) => tracing::info!(original = self.original, "window dragging setting restored"),
            Err(error) => tracing::error!(%error, "could not restore window dragging setting"),
        }
    }
}

fn get_full_windows() -> windows::core::Result<bool> {
    let mut enabled = BOOL::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETDRAGFULLWINDOWS,
            0,
            Some((&raw mut enabled).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(enabled.as_bool())
}

fn set_full_windows(enabled: bool) -> windows::core::Result<()> {
    // Change only the live session setting, without overwriting the user's profile.
    unsafe {
        SystemParametersInfoW(
            SPI_SETDRAGFULLWINDOWS,
            u32::from(enabled),
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
}

struct ExclusiveDesktop(HANDLE);
impl ExclusiveDesktop {
    fn acquire() -> anyhow::Result<Self> {
        unsafe {
            let handle = CreateMutexW(None, false, w!("Local\\MeshRMMOutlineDragging"))?;
            let result = WaitForSingleObject(handle, 10_000);
            if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                let _ = CloseHandle(handle);
                anyhow::bail!("another session is still restoring the window dragging setting");
            }
            Ok(Self(handle))
        }
    }
}
impl Drop for ExclusiveDesktop {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "changes the interactive user's drag setting; run without a remote session"]
    fn live_disconnect_restores_enabled_and_disabled_settings() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                set_full_windows(self.0).unwrap();
            }
        }
        let restore = Restore(get_full_windows().unwrap());
        for original in [true, false] {
            set_full_windows(original).unwrap();
            for _ in 0..2 {
                let session = OutlineDragging::new().unwrap();
                assert!(!get_full_windows().unwrap());
                drop(session); // Same cleanup on Stop, EOF, and helper errors.
                assert_eq!(get_full_windows().unwrap(), original);
            }
            let session = ConsoleOutlineDragging::new().unwrap();
            assert!(!get_full_windows().unwrap());
            drop(session);
            assert_eq!(get_full_windows().unwrap(), original);
        }
        drop(restore);
    }
}
