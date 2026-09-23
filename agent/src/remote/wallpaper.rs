//! Temporarily suppress wallpaper without replacing image paths or slideshow settings.
//!
//! Windows persists these settings in the user's profile, so the original state is
//! journaled there too. A helper killed by sign-out, restart, or a desktop switch
//! cannot restore it; the next helper or the user's tray restores the journal.
use anyhow::Context;
use std::{sync::mpsc, thread};
use windows::{
    Win32::{
        Foundation::{
            COLORREF, CloseHandle, ERROR_FILE_NOT_FOUND, S_FALSE, WAIT_ABANDONED, WAIT_OBJECT_0,
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Registry::{
                HKEY_CURRENT_USER, REG_QWORD, RRF_RT_REG_QWORD, RegDeleteKeyValueW, RegGetValueW,
                RegSetKeyValueW,
            },
            Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        },
        UI::Shell::{DesktopWallpaper, IDesktopWallpaper},
    },
    core::{Interface, PCWSTR, w},
};

const JOURNAL_KEY: PCWSTR = w!("Software\\MeshRMM\\Agent");
const JOURNAL_VALUE: PCWSTR = w!("HiddenWallpaper");

pub fn set_hidden(current: &mut Option<HiddenWallpaper>, hidden: bool) -> anyhow::Result<()> {
    if hidden && current.is_none() {
        *current = Some(HiddenWallpaper::new()?);
    } else if !hidden && current.take().is_none() {
        // Restoration here is housekeeping for an earlier helper, not the viewer's request.
        if let Err(error) = restore_interrupted() {
            tracing::warn!(error = %format!("{error:#}"), "could not restore interrupted wallpaper");
        }
    }
    Ok(())
}

/// Restores wallpaper left hidden by a helper that exited without restoring it.
/// A live helper in this session owns the wallpaper and restores it itself.
pub fn restore_interrupted() -> anyhow::Result<()> {
    if Journal::load()?.is_none() {
        return Ok(());
    }
    // The mutex belongs to the thread that waits on it, so use a dedicated
    // thread that also owns its COM apartment.
    thread::scope(|scope| {
        scope
            .spawn(|| -> anyhow::Result<()> {
                let Some(_exclusive) = ExclusiveDesktop::wait(0)? else {
                    return Ok(());
                };
                let Some((color, enabled)) = Journal::load()? else {
                    return Ok(());
                };
                unsafe {
                    CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
                }
                let _com = ComApartment;
                let desktop = unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)? };
                restore(&desktop, color, enabled)?;
                tracing::info!("restored wallpaper left hidden by an interrupted remote session");
                Ok(())
            })
            .join()
            .map_err(|_| anyhow::anyhow!("wallpaper restoration panicked"))?
    })
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
        Self::wait(10_000)?.context("another session is still restoring the wallpaper")
    }

    /// An abandoned mutex means its owner died without restoring; take it over.
    fn wait(timeout_ms: u32) -> windows::core::Result<Option<Self>> {
        unsafe {
            let handle = CreateMutexW(None, false, w!("Local\\MeshRMMWallpaper"))?;
            let result = WaitForSingleObject(handle, timeout_ms);
            if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                let _ = CloseHandle(handle);
                return Ok(None);
            }
            Ok(Some(Self(handle)))
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
            // A journal means an earlier helper hid the wallpaper and never
            // restored it, so the current black desktop is not the original.
            let journaled = Journal::load()?;
            let color = desktop.GetBackgroundColor()?;
            // The generated Enable wrapper discards S_FALSE, which tells us
            // wallpaper was already disabled and must remain so on restoration.
            let result = (desktop.vtable().Enable)(desktop.as_raw(), false.into());
            result.ok()?;
            let original = match journaled {
                Some((color, enabled)) => {
                    tracing::info!("reusing wallpaper state journaled by an interrupted session");
                    Self {
                        desktop,
                        color,
                        enabled,
                    }
                }
                None => {
                    let original = Self {
                        desktop,
                        color,
                        enabled: result != S_FALSE,
                    };
                    // Dropping `original` on failure restores the wallpaper just disabled.
                    Journal::save(original.color, original.enabled)?;
                    original
                }
            };
            original.desktop.SetBackgroundColor(COLORREF(0))?;
            Ok(original)
        }
    }
}
impl Drop for OriginalWallpaper {
    fn drop(&mut self) {
        match restore(&self.desktop, self.color, self.enabled) {
            Ok(()) => tracing::info!("remote wallpaper restored"),
            // The journal remains for the next helper or the user's tray.
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "could not restore remote wallpaper")
            }
        }
    }
}

fn restore(desktop: &IDesktopWallpaper, color: COLORREF, enabled: bool) -> anyhow::Result<()> {
    unsafe {
        desktop.SetBackgroundColor(color)?;
        desktop.Enable(enabled)?;
    }
    Journal::clear()
}

/// The original wallpaper state, kept in the user's profile while it is hidden.
struct Journal;
impl Journal {
    fn load() -> anyhow::Result<Option<(COLORREF, bool)>> {
        let mut value = 0u64;
        let mut size = std::mem::size_of_val(&value) as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                JOURNAL_KEY,
                JOURNAL_VALUE,
                RRF_RT_REG_QWORD,
                None,
                Some((&raw mut value).cast()),
                Some(&mut size),
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        result
            .ok()
            .context("could not read the wallpaper journal")?;
        Ok(Some(decode(value)))
    }

    fn save(color: COLORREF, enabled: bool) -> anyhow::Result<()> {
        let value = encode(color, enabled);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                JOURNAL_KEY,
                JOURNAL_VALUE,
                REG_QWORD.0,
                Some((&raw const value).cast()),
                std::mem::size_of_val(&value) as u32,
            )
        }
        .ok()
        .context("could not journal the original wallpaper")
    }

    fn clear() -> anyhow::Result<()> {
        let result = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, JOURNAL_KEY, JOURNAL_VALUE) };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        result.ok().context("could not clear the wallpaper journal")
    }
}

fn encode(color: COLORREF, enabled: bool) -> u64 {
    u64::from(color.0) | u64::from(enabled) << 32
}

fn decode(value: u64) -> (COLORREF, bool) {
    (COLORREF(value as u32), value >> 32 & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trips_color_and_enabled_state() {
        for (color, enabled) in [(0, false), (0x00ff_8040, true), (u32::MAX, true)] {
            assert_eq!(
                decode(encode(COLORREF(color), enabled)),
                (COLORREF(color), enabled)
            );
        }
    }

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

    #[test]
    #[ignore = "changes the interactive user's wallpaper; run on an unlocked desktop"]
    fn live_killed_helper_is_restored_from_journal() {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _com = ComApartment;
            let desktop: IDesktopWallpaper =
                CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL).unwrap();
            let color = desktop.GetBackgroundColor().unwrap();
            let status = desktop.GetStatus().unwrap();
            // Leaking the guard leaves the desktop hidden, as a killed helper does.
            std::mem::forget(OriginalWallpaper::hide().unwrap());
            assert!(Journal::load().unwrap().is_some());
            // A replacement helper must restore the journaled state, not black.
            let mut guard = None;
            set_hidden(&mut guard, true).unwrap();
            set_hidden(&mut guard, false).unwrap();
            assert_eq!(desktop.GetBackgroundColor().unwrap(), color);
            assert_eq!(desktop.GetStatus().unwrap(), status);
            assert!(Journal::load().unwrap().is_none());
            // Sign-in recovery and a helper told "not hidden" restore without hiding.
            std::mem::forget(OriginalWallpaper::hide().unwrap());
            set_hidden(&mut None, false).unwrap();
            assert_eq!(desktop.GetBackgroundColor().unwrap(), color);
            assert!(Journal::load().unwrap().is_none());
            std::mem::forget(OriginalWallpaper::hide().unwrap());
            restore_interrupted().unwrap();
            assert_eq!(desktop.GetBackgroundColor().unwrap(), color);
            assert_eq!(desktop.GetStatus().unwrap(), status);
            assert!(Journal::load().unwrap().is_none());
        }
    }
}
