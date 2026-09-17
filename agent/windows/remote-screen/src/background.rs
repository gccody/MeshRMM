//! An off-screen Session 0 desktop. Never switches the console input desktop.
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HWND, LPARAM, RECT, SetLastError};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::StationsAndDesktops::*;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, w};

pub const DISPLAY_ID: u32 = u32::MAX - 2;
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 800;
pub const DESKTOP_NAME: &str = "MeshRMMBackground";
const DESKTOP_RIGHTS: u32 = DESKTOP_READOBJECTS.0
    | DESKTOP_CREATEWINDOW.0
    | DESKTOP_CREATEMENU.0
    | DESKTOP_ENUMERATE.0
    | DESKTOP_WRITEOBJECTS.0;

pub fn display() -> crate::DisplayInfo {
    crate::DisplayInfo {
        id: DISPLAY_ID,
        name: "Background (Session 0 · experimental)".into(),
        x: 0,
        y: 0,
        width: WIDTH,
        height: HEIGHT,
        primary: false,
    }
}

pub fn require_session_zero() -> windows::core::Result<()> {
    let mut session = u32::MAX;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session)? };
    if session != 0 {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_ACCESSDENIED,
            "Background GUI requires the installed service in Session 0",
        ));
    }
    Ok(())
}

/// Retained by each helper until it exits. Default service-token ACLs
/// restrict this desktop; no interactive-user access is added.
pub struct Desktop {
    handle: HDESK,
    previous: Option<HDESK>,
}

impl Desktop {
    pub fn create() -> windows::core::Result<Self> {
        require_session_zero()?;
        unsafe {
            CreateDesktopW(
                w!("MeshRMMBackground"),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS(0),
                DESKTOP_RIGHTS,
                None,
            )
            .map(|handle| Self {
                handle,
                previous: None,
            })
        }
    }

    pub fn bind() -> windows::core::Result<Self> {
        require_session_zero()?;
        let desktop = unsafe {
            OpenDesktopW(
                w!("MeshRMMBackground"),
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_RIGHTS,
            )?
        };
        let previous =
            unsafe { GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId())? };
        let owner = Self {
            handle: desktop,
            previous: Some(previous),
        };
        unsafe { SetThreadDesktop(desktop)? };
        Ok(owner)
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            let _ = unsafe { SetThreadDesktop(previous) };
        }
        let _ = unsafe { CloseDesktop(self.handle) };
    }
}

pub fn desktop_path() -> windows::core::Result<String> {
    let station = unsafe { GetProcessWindowStation()? };
    let mut name = [0_u16; 256];
    unsafe {
        GetUserObjectInformationW(
            HANDLE(station.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            None,
        )?;
    }
    let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    Ok(format!(
        "{}\\{DESKTOP_NAME}",
        String::from_utf16_lossy(&name[..end])
    ))
}

pub fn windows() -> windows::core::Result<Vec<HWND>> {
    unsafe extern "system" fn collect(hwnd: HWND, parameter: LPARAM) -> BOOL {
        unsafe {
            if GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0
                && !IsIconic(hwnd).as_bool()
            {
                let windows = &mut *(parameter.0 as *mut Vec<HWND>);
                if windows.len() < 128 {
                    windows.push(hwnd);
                }
            }
        }
        BOOL(1)
    }
    let mut windows = Vec::new();
    unsafe {
        let desktop = GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId())?;
        SetLastError(ERROR_SUCCESS);
        let result = EnumDesktopWindows(
            Some(desktop),
            Some(collect),
            LPARAM((&mut windows as *mut Vec<HWND>) as isize),
        );
        // Windows returns FALSE with ERROR_SUCCESS for an empty desktop.
        // Capture starts before the launcher, so this is a valid first frame.
        if let Err(error) = result
            && (error.code().0 != 0 || !windows.is_empty())
        {
            return Err(error);
        }
    };
    Ok(windows)
}

/// Per-window backing stores keep slow or failed repaints from erasing a window.
/// Owned by the capture thread and released with its desktop capture session.
#[derive(Default)]
pub struct Renderer {
    windows: Vec<WindowImage>,
    next: usize,
}

struct WindowImage {
    hwnd: HWND,
    thread: u32,
    process: u32,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: i32,
    height: i32,
}

impl WindowImage {
    fn new(hwnd: HWND, width: i32, height: i32, source: HDC) -> windows::core::Result<Self> {
        unsafe {
            let mut image = Self {
                hwnd,
                thread: 0,
                process: 0,
                dc: CreateCompatibleDC(Some(source)),
                bitmap: HBITMAP::default(),
                previous: HGDIOBJ::default(),
                width,
                height,
            };
            if image.dc.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            image.thread = GetWindowThreadProcessId(hwnd, Some(&mut image.process));
            image.bitmap = CreateCompatibleBitmap(source, width, height);
            if image.bitmap.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            image.previous = SelectObject(image.dc, image.bitmap.into());
            if image.previous.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            PatBlt(image.dc, 0, 0, width, height, BLACKNESS).ok()?;
            Ok(image)
        }
    }
}

impl Drop for WindowImage {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

impl Renderer {
    /// PrintWindow remains synchronous; the disposable helper's watchdog handles
    /// stalled applications. Refresh in rotating order, but composite EVERY window
    /// in z-order, even after the refresh budget expires.
    pub fn paint(&mut self, dc: HDC) -> windows::core::Result<()> {
        unsafe {
            let visible = windows()?;
            self.windows.retain(|image| {
                let mut process = 0;
                let thread = GetWindowThreadProcessId(image.hwnd, Some(&mut process));
                let mut rect = RECT::default();
                visible.contains(&image.hwnd)
                    && thread == image.thread
                    && process == image.process
                    && GetWindowRect(image.hwnd, &mut rect).is_ok()
                    && rect.right > 0
                    && rect.bottom > 0
                    && rect.left < WIDTH as i32
                    && rect.top < HEIGHT as i32
            });
            let mut layout = Vec::new();
            for hwnd in visible.into_iter().rev() {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_err()
                    || rect.right <= 0
                    || rect.bottom <= 0
                    || rect.left >= WIDTH as i32
                    || rect.top >= HEIGHT as i32
                {
                    continue;
                }
                let width = (rect.right - rect.left).clamp(1, 8192);
                let height = (rect.bottom - rect.top).clamp(1, 8192);
                self.windows.retain(|image| {
                    image.hwnd != hwnd || (image.width == width && image.height == height)
                });
                // Bound cached GDI bitmaps to 128 MiB even for pathological desktops.
                if !self.windows.iter().any(|image| image.hwnd == hwnd)
                    && self.windows.len() < 32
                    && self
                        .windows
                        .iter()
                        .map(|image| i64::from(image.width) * i64::from(image.height))
                        .sum::<i64>()
                        + i64::from(width) * i64::from(height)
                        <= 32 * 1024 * 1024
                {
                    self.windows
                        .push(WindowImage::new(hwnd, width, height, dc)?);
                }
                layout.push((hwnd, rect));
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            let count = self.windows.len();
            for offset in 0..count {
                let index = (self.next + offset) % count;
                let image = &self.windows[index];
                if !IsHungAppWindow(image.hwnd).as_bool() {
                    // Paint into a scratch bitmap initialized from the last image.
                    // Failed captures cannot corrupt the retained image.
                    let scratch = WindowImage::new(image.hwnd, image.width, image.height, dc)?;
                    BitBlt(
                        scratch.dc,
                        0,
                        0,
                        image.width,
                        image.height,
                        Some(image.dc),
                        0,
                        0,
                        SRCCOPY,
                    )?;
                    // Finish queued copies before handing the bitmap to another
                    // thread/process for painting, and before reading it back.
                    GdiFlush().ok()?;
                    if PrintWindow(image.hwnd, scratch.dc, PRINT_WINDOW_FLAGS(2)).as_bool()
                        || PrintWindow(image.hwnd, scratch.dc, PRINT_WINDOW_FLAGS(0)).as_bool()
                    {
                        GdiFlush().ok()?;
                        BitBlt(
                            image.dc,
                            0,
                            0,
                            image.width,
                            image.height,
                            Some(scratch.dc),
                            0,
                            0,
                            SRCCOPY,
                        )?;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    self.next = (index + 1) % count;
                    break;
                }
            }
            PatBlt(dc, 0, 0, WIDTH as i32, HEIGHT as i32, BLACKNESS).ok()?;
            for (hwnd, rect) in layout {
                if let Some(image) = self.windows.iter().find(|image| image.hwnd == hwnd) {
                    BitBlt(
                        dc,
                        rect.left,
                        rect.top,
                        image.width,
                        image.height,
                        Some(image.dc),
                        0,
                        0,
                        SRCCOPY,
                    )?;
                } else {
                    // Uncached overflow windows still render, without growing the cache.
                    let saved = SaveDC(dc);
                    if saved != 0 {
                        let _ = SetViewportOrgEx(dc, rect.left, rect.top, None);
                        IntersectClipRect(dc, 0, 0, rect.right - rect.left, rect.bottom - rect.top);
                        if !IsHungAppWindow(hwnd).as_bool()
                            && !PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(2)).as_bool()
                        {
                            let _ = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(0));
                        }
                        let _ = RestoreDC(dc, saved);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Capture diagnostic evidence using the same renderer as the video backend.
pub fn snapshot_bmp() -> windows::core::Result<Vec<u8>> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(windows::core::Error::from_thread());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: 40,
                biWidth: WIDTH as i32,
                biHeight: -(HEIGHT as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0) {
            Ok(bitmap) => bitmap,
            Err(error) => {
                let _ = DeleteDC(dc);
                return Err(error);
            }
        };
        let previous = SelectObject(dc, bitmap.into());
        let result = Renderer::default().paint(dc).map(|()| {
            let _ = GdiFlush();
            let count = (WIDTH * HEIGHT * 4) as usize;
            let mut bytes = Vec::with_capacity(54 + count);
            bytes.extend_from_slice(b"BM");
            bytes.extend_from_slice(&(54 + count as u32).to_le_bytes());
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(&54_u32.to_le_bytes());
            bytes.extend_from_slice(&40_u32.to_le_bytes());
            bytes.extend_from_slice(&WIDTH.to_le_bytes());
            bytes.extend_from_slice(&(-(HEIGHT as i32)).to_le_bytes());
            bytes.extend_from_slice(&1_u16.to_le_bytes());
            bytes.extend_from_slice(&32_u16.to_le_bytes());
            bytes.extend_from_slice(&[0; 24]);
            bytes.extend_from_slice(std::slice::from_raw_parts(pixels.cast::<u8>(), count));
            bytes
        });
        SelectObject(dc, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{COLORREF, LRESULT, WPARAM};

    unsafe extern "system" fn slow_window(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if matches!(message, WM_PRINT | WM_PRINTCLIENT | WM_PAINT) {
                let mut paint = PAINTSTRUCT::default();
                let dc = if message == WM_PAINT {
                    BeginPaint(hwnd, &mut paint)
                } else {
                    HDC(wparam.0 as *mut _)
                };
                std::thread::sleep(std::time::Duration::from_millis(110));
                {
                    let brush = CreateSolidBrush(COLORREF(0x332211));
                    FillRect(
                        dc,
                        &RECT {
                            left: 0,
                            top: 0,
                            right: 100,
                            bottom: 100,
                        },
                        brush,
                    );
                    let _ = DeleteObject(brush.into());
                }
                if message == WM_PAINT {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    #[test]
    #[ignore = "Requires a dedicated Session 0 process; creates GUI windows"]
    fn retained_windows_survive_refresh_budget_and_close() -> windows::core::Result<()> {
        std::thread::spawn(|| unsafe {
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
            let _owner = Desktop::create()?;
            let _binding = Desktop::bind()?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(slow_window),
                lpszClassName: w!("MeshRMMRetainedCaptureTest"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let first = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class.lpszClassName,
                w!(""),
                WS_POPUP | WS_VISIBLE,
                10,
                10,
                100,
                100,
                None,
                None,
                None,
                None,
            )?;
            let second = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class.lpszClassName,
                w!(""),
                WS_POPUP | WS_VISIBLE,
                120,
                10,
                100,
                100,
                None,
                None,
                None,
                None,
            )?;
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let screen = GetDC(None);
            let output = WindowImage::new(first, WIDTH as i32, HEIGHT as i32, screen)?;
            ReleaseDC(None, screen);
            let mut renderer = Renderer::default();
            // Each application consumes the whole refresh budget. Both must remain
            // visible once captured, including on frames that refresh its neighbour.
            renderer.paint(output.dc)?;
            renderer.paint(output.dc)?;
            for _ in 0..4 {
                renderer.paint(output.dc)?;
                assert_eq!(GetPixel(output.dc, 20, 20), COLORREF(0x332211));
                assert_eq!(GetPixel(output.dc, 130, 20), COLORREF(0x332211));
                assert_eq!(GetPixel(output.dc, 0, 0), COLORREF(0));
            }
            DestroyWindow(first)?;
            renderer.paint(output.dc)?;
            assert_eq!(GetPixel(output.dc, 20, 20), COLORREF(0));
            assert_eq!(GetPixel(output.dc, 130, 20), COLORREF(0x332211));
            DestroyWindow(second)?;
            Ok(())
        })
        .join()
        .expect("capture regression thread panicked")
    }
}
