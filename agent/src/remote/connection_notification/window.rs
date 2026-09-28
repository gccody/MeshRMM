//! A popup in the corner of the primary monitor's work area, above the
//! taskbar. It never takes focus. It closes when clicked, after a while unless
//! the pointer rests on it, or when its guard drops.
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const VISIBLE_FOR: Duration = Duration::from_secs(15);
const CLOSE_TIMER: usize = 1;
const TITLE: &str = "Remote session started";
const DISMISS: &str = "×";
// Layout in 96-DPI pixels.
const WIDTH: i32 = 360;
const MARGIN: i32 = 16;
const PADDING: i32 = 16;
const ACCENT: i32 = 4;
const GAP: i32 = 6;
const DISMISS_WIDTH: i32 = 20;
const MAX_BODY_HEIGHT: i32 = 320;
const TITLE_FONT: i32 = 16;
const BODY_FONT: i32 = 15;
const BACKGROUND: COLORREF = COLORREF(0x00382b21);
const BORDER: COLORREF = COLORREF(0x0063554b);
const ACCENT_COLOR: COLORREF = COLORREF(0x00f16663);
const TITLE_COLOR: COLORREF = COLORREF(0x00ffffff);
const BODY_COLOR: COLORREF = COLORREF(0x00ebe5e2);
const MUTED_COLOR: COLORREF = COLORREF(0x00b8a394);

pub struct NotificationWindow {
    thread_id: u32,
    #[cfg(test)]
    window: usize,
    thread: Option<JoinHandle<()>>,
}

impl NotificationWindow {
    pub fn show(text: &str) -> anyhow::Result<Self> {
        let text = text.to_owned();
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("connection-notification".into())
            .spawn(move || unsafe {
                match create_window(&text) {
                    Ok(window) => {
                        if tx
                            .send(Ok((GetCurrentThreadId(), window.0 as usize)))
                            .is_ok()
                        {
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
            Ok(Ok((thread_id, _window))) => Ok(Self {
                thread_id,
                #[cfg(test)]
                window: _window,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                anyhow::bail!("connection notification failed to open: {result:?}")
            }
        }
    }
}

impl Drop for NotificationWindow {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // The notification may already have closed itself. The unjoined
            // thread keeps its ID from being reused, so this cannot reach
            // another thread.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = thread.join();
        }
    }
}

struct State {
    title: Vec<u16>,
    text: Vec<u16>,
    title_font: HFONT,
    body_font: HFONT,
    dpi: i32,
    title_height: i32,
}

impl State {
    fn px(&self, value: i32) -> i32 {
        value * self.dpi / 96
    }
}

/// Where a `width` by `height` notification sits in the `work` area: its
/// bottom-right corner, inset by `margin` and kept on screen.
fn placement(work: RECT, width: i32, height: i32, margin: i32) -> RECT {
    let right = (work.right - margin).max(work.left + width);
    let bottom = (work.bottom - margin).max(work.top + height);
    RECT {
        left: right - width,
        top: bottom - height,
        right,
        bottom,
    }
}

unsafe fn font(height: i32, weight: FONT_WEIGHT) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(w!("Segoe UI").as_ptr()),
        )
    }
}

unsafe fn create_window(text: &str) -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MeshRMMConnectionNotification");
        // A previous session may already have registered this process-wide class.
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_HAND)?,
            ..Default::default()
        });
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("Remote session started — click to dismiss"),
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
        // Unaware processes get 96 and are scaled by Windows.
        let dpi = GetDpiForSystem().max(96) as i32;
        let state = Box::new(State {
            title: TITLE.encode_utf16().collect(),
            text: text.encode_utf16().collect(),
            title_font: font(TITLE_FONT * dpi / 96, FW_SEMIBOLD),
            body_font: font(BODY_FONT * dpi / 96, FW_NORMAL),
            dpi,
            title_height: 0,
        });
        // WM_NCDESTROY frees the state, including on a failed placement.
        SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(state) as isize);
        if let Err(error) = place(window) {
            let _ = DestroyWindow(window);
            return Err(error);
        }
        SetTimer(
            Some(window),
            CLOSE_TIMER,
            VISIBLE_FOR.as_millis() as u32,
            None,
        );
        Ok(window)
    }
}

/// Sizes the window to its text and shows it in the primary work area.
unsafe fn place(window: HWND) -> windows::core::Result<()> {
    unsafe {
        let state = &mut *(GetWindowLongPtrW(window, GWLP_USERDATA) as *mut State);
        let mut work = RECT::default();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut work as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
        let margin = state.px(MARGIN);
        let width = state
            .px(WIDTH)
            .min(work.right - work.left - 2 * margin)
            .max(state.px(PADDING * 2 + ACCENT + DISMISS_WIDTH * 2));
        let text_width = width - state.px(ACCENT + 2 * PADDING);
        let dc = GetDC(Some(window));
        let old = SelectObject(dc, state.title_font.into());
        let mut title = RECT {
            right: text_width - state.px(DISMISS_WIDTH),
            ..Default::default()
        };
        DrawTextW(
            dc,
            &mut state.title,
            &mut title,
            DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT,
        );
        SelectObject(dc, state.body_font.into());
        let mut body = RECT {
            right: text_width,
            ..Default::default()
        };
        DrawTextW(
            dc,
            &mut state.text,
            &mut body,
            DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL | DT_CALCRECT,
        );
        SelectObject(dc, old);
        ReleaseDC(Some(window), dc);
        state.title_height = title.bottom - title.top;
        let body_height = (body.bottom - body.top)
            .min(state.px(MAX_BODY_HEIGHT))
            .min((work.bottom - work.top) / 2);
        let height = state.px(2 * PADDING + GAP) + state.title_height + body_height;
        let rect = placement(work, width, height, margin);
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )?;
        let _ = InvalidateRect(Some(window), None, true);
        Ok(())
    }
}

unsafe fn paint(window: HWND, state: &mut State) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(window, &mut client);
        let fill = |rect: &RECT, color| {
            let brush = CreateSolidBrush(color);
            FillRect(dc, rect, brush);
            let _ = DeleteObject(brush.into());
        };
        fill(&client, BACKGROUND);
        fill(
            &RECT {
                right: state.px(ACCENT),
                ..client
            },
            ACCENT_COLOR,
        );
        let border = CreateSolidBrush(BORDER);
        FrameRect(dc, &client, border);
        let _ = DeleteObject(border.into());
        SetBkMode(dc, TRANSPARENT);
        let left = state.px(ACCENT + PADDING);
        let right = client.right - state.px(PADDING);
        let top = state.px(PADDING);
        let title_bottom = top + state.title_height;
        let old = SelectObject(dc, state.title_font.into());
        SetTextColor(dc, TITLE_COLOR);
        let dismiss_left = right - state.px(DISMISS_WIDTH);
        DrawTextW(
            dc,
            &mut state.title,
            &mut RECT {
                left,
                top,
                right: dismiss_left,
                bottom: title_bottom,
            },
            DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        SetTextColor(dc, MUTED_COLOR);
        DrawTextW(
            dc,
            &mut DISMISS.encode_utf16().collect::<Vec<_>>(),
            &mut RECT {
                left: dismiss_left,
                top,
                right,
                bottom: title_bottom,
            },
            DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
        );
        SelectObject(dc, state.body_font.into());
        SetTextColor(dc, BODY_COLOR);
        let mut body = RECT {
            left,
            top: title_bottom + state.px(GAP),
            right,
            bottom: client.bottom - state.px(PADDING),
        };
        DrawTextW(
            dc,
            &mut state.text,
            &mut body,
            DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL | DT_END_ELLIPSIS,
        );
        SelectObject(dc, old);
        let _ = EndPaint(window, &ps);
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut State;
        match message {
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            // Clicking dismisses; so does closing it any other way.
            WM_LBUTTONUP | WM_CLOSE => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == CLOSE_TIMER => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            // Resting the pointer on the notification restarts its countdown.
            WM_MOUSEMOVE => {
                SetTimer(
                    Some(window),
                    CLOSE_TIMER,
                    VISIBLE_FOR.as_millis() as u32,
                    None,
                );
                LRESULT(0)
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE if !state.is_null() => {
                if let Err(error) = place(window) {
                    tracing::warn!(%error, "could not move the connection notification");
                }
                LRESULT(0)
            }
            WM_PAINT if !state.is_null() => {
                paint(window, &mut *state);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                if !state.is_null() {
                    let state = Box::from_raw(state);
                    let _ = DeleteObject(state.title_font.into());
                    let _ = DeleteObject(state.body_font.into());
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
    fn placement_uses_the_bottom_right_of_the_work_area_and_stays_on_screen() {
        let work = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1040,
        };
        assert_eq!(
            placement(work, 360, 100, 16),
            RECT {
                left: -376,
                top: 924,
                right: -16,
                bottom: 1024,
            }
        );
        let tiny = RECT {
            left: 0,
            top: 0,
            right: 300,
            bottom: 80,
        };
        let rect = placement(tiny, 360, 100, 16);
        assert_eq!((rect.left, rect.top), (0, 0), "the text must stay visible");
    }

    // IsWindowVisible also checks the desktop, which is hidden when tests run
    // in a non-interactive session (e.g. over SSH), so read the window's style.
    fn visible(window: HWND) -> bool {
        unsafe {
            IsWindow(Some(window)).as_bool()
                && GetWindowLongW(window, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0
        }
    }

    fn wait_until_closed(window: HWND) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { IsWindow(Some(window)) }.as_bool() {
            assert!(
                std::time::Instant::now() < deadline,
                "notification did not close"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn shows_in_the_primary_work_area_without_focus_and_closes_with_its_guard() {
        let notification = NotificationWindow::show(
            "Ada Lovelace has connected to this computer.\nA second line, which is long enough to wrap onto a third line.",
        )
        .unwrap();
        let window = HWND(notification.window as *mut _);
        unsafe {
            assert!(visible(window));
            assert_ne!(GetForegroundWindow(), window, "it must not take focus");
            let mut work = RECT::default();
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&mut work as *mut RECT).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .unwrap();
            let mut rect = RECT::default();
            GetWindowRect(window, &mut rect).unwrap();
            assert!(rect.left >= work.left && rect.right <= work.right);
            assert!(rect.top >= work.top && rect.bottom <= work.bottom);
            assert!(
                rect.right > work.right - 100 && rect.bottom > work.bottom - 100,
                "it belongs in the corner above the notification area"
            );
            let single = NotificationWindow::show("One line").unwrap();
            let mut short = RECT::default();
            GetWindowRect(HWND(single.window as *mut _), &mut short).unwrap();
            assert!(
                rect.bottom - rect.top > short.bottom - short.top,
                "wrapped text must make the notification taller"
            );
        }
        drop(notification);
        assert!(!unsafe { IsWindow(Some(window)) }.as_bool());
    }

    #[test]
    fn a_click_dismisses_it_and_the_guard_still_drops_cleanly() {
        let notification = NotificationWindow::show("Click check").unwrap();
        let window = HWND(notification.window as *mut _);
        unsafe {
            PostMessageW(Some(window), WM_LBUTTONUP, WPARAM(0), LPARAM(0)).unwrap();
        }
        wait_until_closed(window);
        drop(notification);
    }

    #[test]
    fn it_closes_itself_when_its_time_is_up() {
        let notification = NotificationWindow::show("Timer check").unwrap();
        let window = HWND(notification.window as *mut _);
        unsafe {
            PostMessageW(Some(window), WM_TIMER, WPARAM(CLOSE_TIMER), LPARAM(0)).unwrap();
        }
        wait_until_closed(window);
        drop(notification);
    }
}
