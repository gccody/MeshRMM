//! Session 0's screen, sized to the background canvas while a workspace is open.
//!
//! Session 0 idles at 1024×768 on its `Winlogon` desktop, so its applications
//! maximize and place themselves for a smaller screen than the canvas. Windows
//! changes the display mode only from the input desktop, so the workspace first
//! makes the background desktop Session 0's input desktop. The console session
//! has its own input desktop and display, and neither changes.
use crate::win32::wide;
use meshrmm_remote_screen::background::{DESKTOP_NAME, HEIGHT, WIDTH};
use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::StationsAndDesktops::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

/// Restores Session 0's input desktop, display mode and work area on drop.
/// Each step that fails is logged and skipped: the workspace still works on a
/// smaller screen, as it did before.
pub(super) struct Screen {
    desktop: Option<String>,
    mode: Option<DEVMODEW>,
    work_area: Option<RECT>,
}

impl Screen {
    /// Call before the workspace creates windows or starts applications, so
    /// nothing is laid out for the old screen.
    pub(super) fn claim(taskbar_height: i32) -> Self {
        let mut screen = Self {
            desktop: None,
            mode: None,
            work_area: None,
        };
        let previous = match input_desktop() {
            Ok(name) => name,
            Err(error) => {
                tracing::warn!(%error, "could not read Session 0's input desktop");
                String::new()
            }
        };
        if let Err(error) = switch_desktop(DESKTOP_NAME) {
            tracing::warn!(%error, "could not make the background desktop Session 0's input desktop");
            return screen;
        }
        // A crashed helper's desktop is destroyed with it, so ours is never the
        // one to return to.
        screen.desktop = (!previous.is_empty() && previous != DESKTOP_NAME).then_some(previous);
        match set_mode(WIDTH, HEIGHT) {
            Ok(previous) => screen.mode = previous,
            Err(error) => tracing::warn!(%error, "could not change Session 0's display mode"),
        }
        let (width, height) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        // Maximized windows fill this area, so they stop above the taskbar.
        let area = RECT {
            left: 0,
            top: 0,
            right: width.min(WIDTH as i32),
            bottom: height.min(HEIGHT as i32 - taskbar_height),
        };
        match set_work_area(area) {
            Ok(previous) => screen.work_area = Some(previous),
            Err(error) => tracing::warn!(%error, "could not set Session 0's work area"),
        }
        tracing::info!(
            session_id = 0,
            width,
            height,
            work_area_bottom = area.bottom,
            "background workspace sized Session 0's screen"
        );
        screen
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // Changing the mode needs our desktop to still be the input desktop.
        if let Some(area) = self.work_area.take()
            && let Err(error) = set_work_area(area)
        {
            tracing::warn!(%error, "could not restore Session 0's work area");
        }
        if let Some(mode) = self.mode.take() {
            let result = unsafe {
                ChangeDisplaySettingsExW(PCWSTR::null(), Some(&mode), None, CDS_TYPE(0), None)
            };
            if result != DISP_CHANGE_SUCCESSFUL {
                tracing::warn!(
                    result = result.0,
                    "could not restore Session 0's display mode"
                );
            }
        }
        if let Some(desktop) = self.desktop.take()
            && let Err(error) = switch_desktop(&desktop)
        {
            tracing::warn!(%error, desktop, "could not restore Session 0's input desktop");
        }
    }
}

fn input_desktop() -> windows::core::Result<String> {
    unsafe {
        let desktop = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)?;
        let mut name = [0_u16; 256];
        let result = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            None,
        );
        let _ = CloseDesktop(desktop);
        result?;
        let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
        Ok(String::from_utf16_lossy(&name[..end]))
    }
}

fn switch_desktop(name: &str) -> windows::core::Result<()> {
    let name = wide(name);
    unsafe {
        let desktop = OpenDesktopW(
            PCWSTR(name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_SWITCHDESKTOP.0,
        )?;
        let result = SwitchDesktop(desktop);
        let _ = CloseDesktop(desktop);
        result
    }
}

/// Changes the mode for this session only, without saving it. Returns the mode
/// to restore, or `None` when it was already the requested size.
fn set_mode(width: u32, height: u32) -> anyhow::Result<Option<DEVMODEW>> {
    let mut current = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut current) }.ok()?;
    if (current.dmPelsWidth, current.dmPelsHeight) == (width, height) {
        return Ok(None);
    }
    current.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    let wanted = DEVMODEW {
        dmPelsWidth: width,
        dmPelsHeight: height,
        ..current
    };
    let result =
        unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), Some(&wanted), None, CDS_TYPE(0), None) };
    anyhow::ensure!(
        result == DISP_CHANGE_SUCCESSFUL,
        "{width}×{height} was refused with {}",
        result.0
    );
    Ok(Some(current))
}

/// Sets the work area for this session only, without saving it. Returns the
/// previous one.
fn set_work_area(mut area: RECT) -> windows::core::Result<RECT> {
    let mut previous = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut previous as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
        // No change broadcast: it would wait on every window on the desktop,
        // and none exist yet at start, while at close they are exiting.
        SystemParametersInfoW(
            SPI_SETWORKAREA,
            0,
            Some((&mut area as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(previous)
}
