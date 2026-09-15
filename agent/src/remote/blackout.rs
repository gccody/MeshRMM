//! A click-through, capture-excluded maintenance notice covering the virtual desktop.
use std::{sync::mpsc, thread};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{Magnification::*, WindowsAndMessaging::*},
    },
    core::{BOOL, PCWSTR, w},
};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(
        version: *mut windows::Win32::System::SystemInformation::OSVERSIONINFOW,
    ) -> i32;
}

pub struct Blackout {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl Blackout {
    pub fn show(text: &str) -> anyhow::Result<Self> {
        let mut version = windows::Win32::System::SystemInformation::OSVERSIONINFOW::default();
        version.dwOSVersionInfoSize = std::mem::size_of_val(&version) as u32;
        anyhow::ensure!(
            unsafe { RtlGetVersion(&mut version) } >= 0 && version.dwBuildNumber >= 19041,
            "Monitor blackout requires Windows 10 version 2004 or newer"
        );
        let text = text.to_owned();
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("maintenance-blackout".into())
            .spawn(move || unsafe {
                let _cursor = match HiddenSystemCursor::hide() {
                    Ok(cursor) => cursor,
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                        return;
                    }
                };
                match create_window(text) {
                    Ok(window) => {
                        if tx.send(Ok(GetCurrentThreadId())).is_ok() {
                            let mut message = MSG::default();
                            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                                let _ = TranslateMessage(&message);
                                DispatchMessageW(&message);
                            }
                        }
                        let _ = DestroyWindow(window);
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                    }
                }
            })?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                anyhow::bail!("Could not black out monitors: {result:?}")
            }
        }
    }
}
impl Drop for Blackout {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// The overlay is click-through, so ShowCursor/WM_SETCURSOR on its thread
// cannot hide the cursor belonging to the application underneath it.
// Keep the magnification runtime and cursor suppression on the overlay thread
// so every exit path (including window creation failure) restores the cursor.
struct HiddenSystemCursor;

impl HiddenSystemCursor {
    fn hide() -> windows::core::Result<Self> {
        unsafe {
            MagInitialize().ok()?;
            let cursor = Self;
            MagShowSystemCursor(false).ok()?;
            Ok(cursor)
        }
    }
}

impl Drop for HiddenSystemCursor {
    fn drop(&mut self) {
        unsafe {
            if !MagShowSystemCursor(true).as_bool() {
                tracing::warn!("Could not restore the system cursor after monitor blackout");
            }
            if !MagUninitialize().as_bool() {
                tracing::warn!("Could not release the monitor blackout magnification runtime");
            }
        }
    }
}

unsafe fn position(window: HWND) -> windows::core::Result<()> {
    unsafe {
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
}

unsafe fn create_window(text: String) -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MeshRMMMaintenanceBlackout");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        });
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class,
            w!("MeshRMM maintenance"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let text: Vec<u16> = text.encode_utf16().collect();
        SetWindowLongPtrW(
            window,
            GWLP_USERDATA,
            Box::into_raw(Box::new(text)) as isize,
        );
        let result = (|| {
            SetLayeredWindowAttributes(window, COLORREF(0), 255, LWA_ALPHA)?;
            SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE)?;
            position(window)?;
            SetTimer(Some(window), 1, 500, None);
            Ok(window)
        })();
        if result.is_err() {
            let _ = DestroyWindow(window);
        }
        result
    }
}

struct PaintContext {
    dc: HDC,
    text: Vec<u16>,
    origin_x: i32,
    origin_y: i32,
}
unsafe extern "system" fn paint_monitor(
    _: HMONITOR,
    _: HDC,
    rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    unsafe {
        let paint = &mut *(data.0 as *mut PaintContext);
        let monitor = *rect;
        let width = monitor.right - monitor.left;
        let height = monitor.bottom - monitor.top;
        let mut bounds = RECT {
            left: monitor.left - paint.origin_x + width / 10,
            right: monitor.right - paint.origin_x - width / 10,
            top: 0,
            bottom: 0,
        };
        let flags = DT_CENTER | DT_WORDBREAK | DT_NOPREFIX;
        // DT_CALCRECT also changes the width. Keep the monitor's symmetric
        // bounds for drawing so every line remains centered on that monitor.
        let mut measured = bounds;
        DrawTextW(paint.dc, &mut paint.text, &mut measured, flags | DT_CALCRECT);
        let text_height = (measured.bottom - measured.top).min(height);
        bounds.top = monitor.top - paint.origin_y + (height - text_height) / 2;
        bounds.bottom = bounds.top + text_height;
        DrawTextW(paint.dc, &mut paint.text, &mut bounds, flags);
        TRUE
    }
}
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_CLOSE => LRESULT(0),
            WM_TIMER | WM_DISPLAYCHANGE => {
                let _ = position(window);
                if message == WM_DISPLAYCHANGE {
                    let _ = InvalidateRect(Some(window), None, true);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut ps);
                let mut rect = RECT::default();
                let _ = GetClientRect(window, &mut rect);
                FillRect(dc, &rect, HBRUSH(GetStockObject(BLACK_BRUSH).0));
                SetTextColor(dc, COLORREF(0x00ffffff));
                SetBkMode(dc, TRANSPARENT);
                let font = CreateFontW(
                    28,
                    0,
                    0,
                    0,
                    FW_NORMAL.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    DEFAULT_PITCH.0 as u32,
                    PCWSTR(w!("Segoe UI").as_ptr()),
                );
                let old = SelectObject(dc, font.into());
                let text = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Vec<u16>;
                if !text.is_null() {
                    let mut paint = PaintContext {
                        dc,
                        text: (*text).clone(),
                        origin_x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                        origin_y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                    };
                    let _ = EnumDisplayMonitors(
                        None,
                        None,
                        Some(paint_monitor),
                        LPARAM(&mut paint as *mut _ as isize),
                    );
                }
                SelectObject(dc, old);
                let _ = DeleteObject(font.into());
                let _ = EndPaint(window, &ps);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let text = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Vec<u16>;
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                if !text.is_null() {
                    drop(Box::from_raw(text));
                }
                DefWindowProcW(window, message, wparam, lparam)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires interactive Windows desktop; briefly blacks out all monitors"]
    fn live_blackout_covers_all_monitors_and_cleans_up() {
        let guard = Blackout::show("This machine is under maintenance by MeshRMM test.").unwrap();
        unsafe {
            let window = FindWindowW(w!("MeshRMMMaintenanceBlackout"), None).unwrap();
            assert!(IsWindowVisible(window).as_bool());
            let mut rect = RECT::default();
            GetWindowRect(window, &mut rect).unwrap();
            assert_eq!(rect.left, GetSystemMetrics(SM_XVIRTUALSCREEN));
            assert_eq!(rect.top, GetSystemMetrics(SM_YVIRTUALSCREEN));
            assert_eq!(rect.right - rect.left, GetSystemMetrics(SM_CXVIRTUALSCREEN));
            assert_eq!(rect.bottom - rect.top, GetSystemMetrics(SM_CYVIRTUALSCREEN));
            let mut affinity = 0;
            GetWindowDisplayAffinity(window, &mut affinity).unwrap();
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            std::thread::sleep(std::time::Duration::from_millis(500));
            drop(guard);
            assert!(!IsWindow(Some(window)).as_bool());
        }
    }
}
