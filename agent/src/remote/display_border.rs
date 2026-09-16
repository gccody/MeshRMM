//! An agent-only outline. Its windows never activate, intercept input, or enter video.
use meshrmm_protocol::Display;
use std::{
    sync::mpsc,
    thread::{self, JoinHandle},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

pub struct DisplayBorder {
    thread_id: u32,
    #[cfg(test)]
    windows: Vec<usize>,
    thread: Option<JoinHandle<()>>,
}

impl DisplayBorder {
    pub fn show(display: &Display) -> anyhow::Result<Self> {
        let displays = if display.id.0 == meshrmm_remote_screen::ALL_MONITORS_ID {
            super::platform::enumerate_displays()?
                .into_iter()
                .filter(|d| d.id.0 != meshrmm_remote_screen::ALL_MONITORS_ID)
                .collect::<Vec<_>>()
        } else {
            vec![display.clone()]
        };
        let (ready, started) = mpsc::sync_channel(1);
        let thread =
            thread::Builder::new()
                .name("display-border".into())
                .spawn(move || unsafe {
                    let mut windows = BorderWindows(Vec::new());
                    let result = (|| -> windows::core::Result<()> {
                        let instance = GetModuleHandleW(None)?;
                        let class = w!("MeshRMMDisplayBorder");
                        RegisterClassW(&WNDCLASSW {
                            lpfnWndProc: Some(window_proc),
                            hInstance: instance.into(),
                            lpszClassName: class,
                            ..Default::default()
                        });
                        for (x, y, width, height) in displays.iter().flat_map(edges) {
                            let hwnd = CreateWindowExW(
                                WS_EX_TOPMOST
                                    | WS_EX_TOOLWINDOW
                                    | WS_EX_NOACTIVATE
                                    | WS_EX_TRANSPARENT
                                    | WS_EX_LAYERED,
                                class,
                                w!("MeshRMM viewed monitor"),
                                WS_POPUP,
                                x,
                                y,
                                width,
                                height,
                                None,
                                None,
                                Some(instance.into()),
                                None,
                            )?;
                            windows.0.push(hwnd);
                            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
                            // Apply before showing: even the first captured frame must exclude it.
                            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
                            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        }
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            if ready
                                .send(Ok((
                                    GetCurrentThreadId(),
                                    windows.0.iter().map(|w| w.0 as usize).collect::<Vec<_>>(),
                                )))
                                .is_ok()
                            {
                                let mut msg = MSG::default();
                                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                                    let _ = TranslateMessage(&msg);
                                    DispatchMessageW(&msg);
                                }
                            }
                        }
                        Err(error) => {
                            let _ = ready.send(Err(error));
                        }
                    }
                })?;
        match started.recv() {
            Ok(Ok((thread_id, _windows))) => Ok(Self {
                #[cfg(test)]
                windows: _windows,
                thread_id,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                anyhow::bail!("display border failed: {result:?}")
            }
        }
    }
}

impl Drop for DisplayBorder {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct BorderWindows(Vec<HWND>);
impl Drop for BorderWindows {
    fn drop(&mut self) {
        for hwnd in &self.0 {
            unsafe {
                let _ = DestroyWindow(*hwnd);
            }
        }
    }
}

fn edges(display: &Display) -> [(i32, i32, i32, i32); 4] {
    let w = display.width.min(i32::MAX as u32) as i32;
    let h = display.height.min(i32::MAX as u32) as i32;
    let t = 3.min(w).min(h);
    let (x, y) = (display.x, display.y);
    [
        (x, y, w, t),
        (x, y + h - t, w, t),
        (x, y, t, h),
        (x + w - t, y, t, h),
    ]
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let brush = CreateSolidBrush(COLORREF(0x003535e5));
                FillRect(dc, &paint.rcPaint, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &paint);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an interactive Windows desktop with DWM"]
    fn native_border_is_excluded_click_through_and_destroyed_on_drop() {
        let d = Display {
            id: meshrmm_protocol::DisplayId(1),
            name: "Test".into(),
            x: 0,
            y: 0,
            width: 200,
            height: 100,
            primary: true,
        };
        let border = DisplayBorder::show(&d).unwrap();
        let handles = border.windows.clone();
        assert_eq!(handles.len(), 4);
        for (handle, (x, y, w, h)) in handles.iter().zip(edges(&d)) {
            unsafe {
                let hwnd = HWND(*handle as *mut _);
                let mut affinity = 0;
                GetWindowDisplayAffinity(hwnd, &mut affinity).unwrap();
                assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
                let mut rect = RECT::default();
                GetWindowRect(hwnd, &mut rect).unwrap();
                assert_eq!(
                    (
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top
                    ),
                    (x, y, w, h)
                );
                let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
                assert_ne!(style & WS_EX_TRANSPARENT.0, 0);
                assert_ne!(style & WS_EX_NOACTIVATE.0, 0);
            }
        }
        drop(border);
        for handle in handles {
            assert!(!unsafe { IsWindow(Some(HWND(handle as *mut _))).as_bool() });
        }
    }

    #[test]
    fn border_follows_negative_monitor_coordinates() {
        let d = Display {
            id: meshrmm_protocol::DisplayId(2),
            name: "Left".into(),
            x: -1920,
            y: -200,
            width: 1920,
            height: 1080,
            primary: false,
        };
        assert_eq!(
            edges(&d),
            [
                (-1920, -200, 1920, 3),
                (-1920, 877, 1920, 3),
                (-1920, -200, 3, 1080),
                (-3, -200, 3, 1080)
            ]
        );
    }
}
