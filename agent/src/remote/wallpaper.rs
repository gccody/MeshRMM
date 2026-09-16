//! Temporarily suppress wallpaper without replacing image paths or slideshow settings.
use std::{sync::mpsc, thread};
use windows::{
    Win32::{
        Foundation::{COLORREF, CloseHandle, S_FALSE, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::{
            Com::{
                CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        },
        UI::Shell::{DesktopWallpaper, IDesktopWallpaper},
    },
    core::{Interface, w},
};

pub fn set_hidden(current: &mut Option<HiddenWallpaper>, hidden: bool) -> anyhow::Result<()> {
    if hidden && current.is_none() {
        *current = Some(HiddenWallpaper::new()?);
    } else if !hidden {
        *current = None;
    }
    Ok(())
}

pub struct HiddenWallpaper {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HiddenWallpaper {
    fn new() -> anyhow::Result<Self> {
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("meshrmm-wallpaper".into())
            .spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    // Serialize replacement sessions until their predecessor has restored
                    // the desktop. The agent permits only one active viewer session.
                    let _exclusive = ExclusiveDesktop::acquire()?;
                    unsafe {
                        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
                    }
                    let _com = ComApartment;
                    let original = unsafe { OriginalWallpaper::hide()? };
                    tracing::info!("remote wallpaper hidden");
                    let _ = ready.send(Ok(()));
                    let _ = stopped.recv(); // Includes EOF when the session/helper exits.
                    drop(original);
                    Ok(())
                })();
                if let Err(error) = result {
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

impl Drop for HiddenWallpaper {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct ExclusiveDesktop(windows::Win32::Foundation::HANDLE);
impl ExclusiveDesktop {
    fn acquire() -> anyhow::Result<Self> {
        unsafe {
            let handle = CreateMutexW(None, false, w!("Local\\MeshRMMWallpaper"))?;
            let result = WaitForSingleObject(handle, 10_000);
            if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                let _ = CloseHandle(handle);
                anyhow::bail!("another session is still restoring the wallpaper");
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

struct OriginalWallpaper {
    desktop: IDesktopWallpaper,
    color: COLORREF,
    enabled: bool,
}
impl OriginalWallpaper {
    unsafe fn hide() -> anyhow::Result<Self> {
        unsafe {
            let desktop: IDesktopWallpaper = CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)?;
            let color = desktop.GetBackgroundColor()?;
            // The generated Enable wrapper discards S_FALSE, which tells us
            // wallpaper was already disabled and must remain so on restoration.
            let result = (desktop.vtable().Enable)(desktop.as_raw(), false.into());
            result.ok()?;
            let original = Self {
                desktop,
                color,
                enabled: result != S_FALSE,
            };
            original.desktop.SetBackgroundColor(COLORREF(0))?;
            Ok(original)
        }
    }
}
impl Drop for OriginalWallpaper {
    fn drop(&mut self) {
        unsafe {
            let color = self.desktop.SetBackgroundColor(self.color);
            let enabled = self.desktop.Enable(self.enabled);
            if let Err(error) = color.and(enabled) {
                tracing::error!(%error, "could not restore remote wallpaper");
            } else {
                tracing::info!("remote wallpaper restored");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "changes the interactive user's wallpaper; run on an unlocked desktop"]
    fn live_wallpaper_toggle_and_disconnect_restore_original() {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _com = ComApartment;
            let desktop: IDesktopWallpaper =
                CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL).unwrap();
            let color = desktop.GetBackgroundColor().unwrap();
            let status = desktop.GetStatus().unwrap();
            let mut guard = None;
            set_hidden(&mut guard, false).unwrap();
            for _ in 0..2 {
                set_hidden(&mut guard, true).unwrap();
                set_hidden(&mut guard, true).unwrap();
                assert_eq!(desktop.GetBackgroundColor().unwrap(), COLORREF(0));
                // S_FALSE confirms the wallpaper was already disabled by us.
                assert_eq!(
                    (desktop.vtable().Enable)(desktop.as_raw(), false.into()),
                    S_FALSE
                );
                set_hidden(&mut guard, false).unwrap();
                assert_eq!(desktop.GetBackgroundColor().unwrap(), color);
                assert_eq!(desktop.GetStatus().unwrap(), status);
            }
            set_hidden(&mut guard, true).unwrap();
            drop(guard); // Connection/helper EOF cleanup, without a toggle command.
            assert_eq!(desktop.GetBackgroundColor().unwrap(), color);
            assert_eq!(desktop.GetStatus().unwrap(), status);
        }
    }
}
