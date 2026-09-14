//! Desktop-owned session notice. The guard closes the UI on stop, cancellation,
//! helper pipe EOF, or capture failure; no network credentials enter this window.
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

pub struct SessionIndicator {
    thread_id: u32,
    #[cfg(test)]
    window: usize,
    thread: Option<JoinHandle<()>>,
}

impl SessionIndicator {
    pub fn show(name: &str, chat: meshrmm_chat::ChatSession) -> anyhow::Result<Self> {
        // Keep untrusted profile text on one line; GDI draws it literally.
        let name: String = name.chars().filter(|c| !c.is_control()).take(256).collect();
        let name = if name.trim().is_empty() {
            "Remote user".to_owned()
        } else {
            name
        };
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("session-indicator".into())
            .spawn(move || {
                let result = unsafe { create_window(name, chat) };
                match result {
                    Ok((hwnd, state)) => {
                        unsafe {
                            // GetMessage wakes as soon as input arrives; no polling delay.
                            if tx.send(Ok((GetCurrentThreadId(), hwnd.0 as usize))).is_ok() {
                                let mut message = MSG::default();
                                while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                                    if (*state)
                                        .popup
                                        .as_ref()
                                        .is_some_and(|popup| popup.handle_message(&message))
                                    {
                                        continue;
                                    }
                                    let _ = TranslateMessage(&message);
                                    DispatchMessageW(&message);
                                }
                            }
                            (*state).popup = None;
                            let _ = DestroyWindow(hwnd);
                            drop(Box::from_raw(state));
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                    }
                }
            })?;
        let ready = rx.recv();
        match ready {
            Ok(Ok((thread_id, _window))) => Ok(Self {
                thread_id,
                #[cfg(test)]
                window: _window,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                anyhow::bail!("session indicator failed to start: {result:?}");
            }
        }
    }
}

impl Drop for SessionIndicator {
    fn drop(&mut self) {
        unsafe {
            // Wake even an idle message loop before joining it.
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct State {
    name: String,
    collapsed: bool,
    work: RECT,
    expanded_width: i32,
    center_x: Option<i32>,
    drag: Option<Drag>,
    chat: meshrmm_chat::ChatSession,
    popup: Option<meshrmm_chat::ChatPopup>,
    chat_status: (bool, usize),
}

struct Drag {
    pointer_x: i32,
    center_x: i32,
    moved: bool,
}

impl State {
    fn left(&self, width: i32) -> i32 {
        let center = self
            .center_x
            .unwrap_or(self.work.left + (self.work.right - self.work.left) / 2);
        (center - width / 2).clamp(
            self.work.left,
            (self.work.right - width).max(self.work.left),
        )
    }

    fn label(&self) -> String {
        if self.collapsed {
            "▾".to_owned()
        } else {
            format!("● {} is connected remotely  ▴", self.name)
        }
    }
}

unsafe fn create_window(
    name: String,
    chat: meshrmm_chat::ChatSession,
) -> windows::core::Result<(HWND, *mut State)> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MeshRMMSessionIndicator");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_HAND)?,
            ..Default::default()
        };
        // A previous session may already have registered this process-wide class.
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            class,
            w!("Remote session — drag sideways; click to collapse or expand"),
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
        let state = Box::into_raw(Box::new(State {
            name,
            collapsed: false,
            work: RECT::default(),
            expanded_width: 320,
            center_x: None,
            drag: None,
            chat,
            popup: None,
            chat_status: (false, 0),
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        refresh_layout(hwnd);
        if let Err(error) = position(hwnd, false) {
            let _ = DestroyWindow(hwnd);
            drop(Box::from_raw(state));
            return Err(error);
        }
        match meshrmm_chat::ChatPopup::for_banner(hwnd, (*state).chat.clone()) {
            Ok(popup) => (*state).popup = Some(popup),
            Err(error) => {
                let _ = DestroyWindow(hwnd);
                drop(Box::from_raw(state));
                tracing::error!(%error, "could not create banner chat popup");
                return Err(windows::core::Error::from_hresult(E_FAIL));
            }
        }
        SetTimer(Some(hwnd), 1, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok((hwnd, state))
    }
}

// Monitor queries and text measurement belong to startup/settings changes, never
// the click path. In particular, avoid synchronous desktop queries while streaming.
unsafe fn refresh_layout(hwnd: HWND) {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        let mut work = RECT::default();
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut work as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            (*state).work = work;
        }
        let text: Vec<u16> = format!("● {} is connected remotely  ▴", (*state).name)
            .encode_utf16()
            .collect();
        let dc = GetDC(Some(hwnd));
        let old = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        let mut size = SIZE::default();
        if GetTextExtentPoint32W(dc, &text, &mut size).as_bool() {
            (*state).expanded_width = (size.cx + 16).min(480);
        }
        SelectObject(dc, old);
        ReleaseDC(Some(hwnd), dc);
    }
}

unsafe fn position(hwnd: HWND, collapsed: bool) -> windows::core::Result<()> {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
        let work = (*state).work;
        let chat_available = (*state).chat.available();
        let desired_width = (if collapsed {
            32
        } else {
            (*state).expanded_width
        }) + if chat_available { 32 } else { 0 };
        let width = desired_width.min((work.right - work.left).max(1));
        let height = if collapsed && !chat_available { 16 } else { 24 };
        // Submit pixels, size, and location as one layered-window update. A
        // separate SetWindowPos/WM_PAINT pair lets capture see resized old pixels.
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        if screen.0.is_null() || dc.0.is_null() || bitmap.0.is_null() {
            let error = windows::core::Error::from_thread();
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            return Err(error);
        }
        let old_bitmap = SelectObject(dc, bitmap.into());
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        let brush = CreateSolidBrush(COLORREF(0x00382b21));
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(brush.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00ffffff));
        let old_font = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        let mut text: Vec<u16> = (&*state).label().encode_utf16().collect();
        let padding = if collapsed { 0 } else { 8 };
        rect.left += padding;
        rect.right -= padding + if chat_available { 32 } else { 0 };
        DrawTextW(
            dc,
            &mut text,
            &mut rect,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);
        if chat_available {
            // A drawn outline and circle avoid missing emoji fonts and clipping.
            let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00ffffff));
            let old_pen = SelectObject(dc, pen.into());
            let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
            let x = width - 25;
            let _ = RoundRect(dc, x, 6, x + 17, 18, 4, 4);
            let _ = MoveToEx(dc, x + 4, 17, None);
            let _ = LineTo(dc, x + 4, 21);
            let _ = LineTo(dc, x + 8, 17);
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            let _ = DeleteObject(pen.into());
            if (*state).chat.unread() > 0 {
                let brush = CreateSolidBrush(COLORREF(0x004545ff));
                let old_brush = SelectObject(dc, brush.into());
                let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
                let _ = Ellipse(dc, width - 12, 1, width - 3, 10);
                SelectObject(dc, old_pen);
                SelectObject(dc, old_brush);
                let _ = DeleteObject(brush.into());
            }
        }
        let destination = POINT {
            x: (&*state).left(width),
            y: work.top,
        };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 220,
            AlphaFormat: 0,
        };
        let result = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            Some(&destination),
            Some(&size),
            Some(dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(dc, old_bitmap);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if let Some(popup) = &(*state).popup {
            popup.layout();
        }
        result
    }
}

// Use signed client coordinates: captured mouse moves can lie outside the tab.
unsafe fn pointer_x(hwnd: HWND, lp: LPARAM) -> i32 {
    unsafe {
        let mut point = POINT {
            x: lp.0 as u16 as i16 as i32,
            y: (lp.0 >> 16) as u16 as i16 as i32,
        };
        let _ = ClientToScreen(hwnd, &mut point);
        point.x
    }
}

unsafe fn drag_to(hwnd: HWND, state: *mut State, x: i32) {
    unsafe {
        let Some(drag) = (*state).drag.as_mut() else {
            return;
        };
        let delta = x - drag.pointer_x;
        // Small hand movements remain a click; dragging never toggles the tab.
        if !drag.moved && delta.abs() < GetSystemMetrics(SM_CXDRAG).max(4) {
            return;
        }
        drag.moved = true;
        (*state).center_x = Some(drag.center_x + delta);
        if let Err(error) = position(hwnd, (*state).collapsed) {
            tracing::warn!(%error, "could not move session banner");
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        match msg {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_CLOSE => return LRESULT(0),
            WM_TIMER if !state.is_null() => {
                if let Some(popup) = &(*state).popup {
                    popup.refresh();
                    if (*state).chat.available()
                        && !(*state).chat.visible()
                        && (*state).chat.unread() > 0
                    {
                        popup.toggle();
                    }
                }
                let status = ((*state).chat.available(), (*state).chat.unread());
                if status != (*state).chat_status {
                    (*state).chat_status = status;
                    let _ = position(hwnd, (*state).collapsed);
                }
                return LRESULT(0);
            }
            WM_LBUTTONDOWN if !state.is_null() => {
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let x = lp.0 as u16 as i16 as i32;
                if (*state).chat.available() && x >= client.right - 32 {
                    if let Some(popup) = &(*state).popup {
                        popup.toggle();
                    }
                    let _ = position(hwnd, (*state).collapsed);
                    return LRESULT(0);
                }
                if let Some(popup) = &(*state).popup {
                    popup.close();
                }
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    (*state).drag = Some(Drag {
                        pointer_x: pointer_x(hwnd, lp),
                        center_x: rect.left + (rect.right - rect.left) / 2,
                        moved: false,
                    });
                    SetCapture(hwnd);
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE if !state.is_null() => {
                drag_to(hwnd, state, pointer_x(hwnd, lp));
                return LRESULT(0);
            }
            WM_LBUTTONUP if !state.is_null() => {
                drag_to(hwnd, state, pointer_x(hwnd, lp));
                let click = (*state).drag.take().is_some_and(|drag| !drag.moved);
                let _ = ReleaseCapture();
                if click {
                    (*state).collapsed = !(*state).collapsed;
                    if let Err(error) = position(hwnd, (*state).collapsed) {
                        tracing::warn!(%error, "could not update session banner");
                    }
                }
                return LRESULT(0);
            }
            WM_CAPTURECHANGED | WM_CANCELMODE if !state.is_null() => {
                (*state).drag = None;
                if msg == WM_CANCELMODE {
                    let _ = ReleaseCapture();
                }
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE if !state.is_null() => {
                refresh_layout(hwnd);
                if let Err(error) = position(hwnd, (*state).collapsed) {
                    tracing::warn!(%error, "could not update session banner");
                }
                return LRESULT(0);
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                // UpdateLayeredWindow owns the complete surface; acknowledge
                // invalidation without drawing into a partially resized window.
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn horizontal_position_stays_on_screen_in_both_states() {
        let mut state = State {
            name: String::new(),
            collapsed: false,
            work: RECT {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1080,
            },
            expanded_width: 320,
            center_x: Some(-3000),
            drag: None,
            chat: meshrmm_chat::ChatSession::default(),
            popup: None,
            chat_status: (false, 0),
        };
        for width in [32, 320] {
            state.center_x = Some(-3000);
            assert_eq!(state.left(width), -1920);
            state.center_x = Some(200);
            assert_eq!(state.left(width), -width);
            state.center_x = Some(-1000);
            assert_eq!(state.left(width), -1000 - width / 2);
        }
    }

    #[test]
    fn incoming_viewer_message_opens_banner_chat_without_reopening_after_dismissal() {
        let chat = meshrmm_chat::ChatSession::with_peer("Viewer");
        chat.set_available(true);
        let indicator = SessionIndicator::show("Chat check", chat.clone()).unwrap();
        let hwnd = HWND(indicator.window as *mut _);
        unsafe {
            let mut initial = RECT::default();
            GetWindowRect(hwnd, &mut initial).unwrap();
            let tick = || {
                assert_ne!(
                    SendMessageTimeoutW(
                        hwnd,
                        WM_TIMER,
                        WPARAM(1),
                        LPARAM(0),
                        SMTO_ABORTIFHUNG,
                        1000,
                        None
                    )
                    .0,
                    0
                );
            };
            tick();
            assert!(!chat.visible());
            let x = initial.right - initial.left - 16;
            let click = || {
                for message in [WM_LBUTTONDOWN, WM_LBUTTONUP] {
                    assert_ne!(
                        SendMessageTimeoutW(
                            hwnd,
                            message,
                            WPARAM(0),
                            LPARAM(((12 << 16) | x) as isize),
                            SMTO_ABORTIFHUNG,
                            1000,
                            None
                        )
                        .0,
                        0
                    );
                }
            };
            chat.receive("Message while closed".into());
            tick();
            assert!(chat.visible());
            assert_eq!(chat.unread(), 0);
            chat.receive("Message while open".into());
            tick();
            assert!(chat.visible());
            assert_eq!(chat.unread(), 0);
            click();
            assert!(!chat.visible());
            tick();
            assert!(
                !chat.visible(),
                "dismissed messages must not reopen the popup"
            );
            chat.receive("Another unread message".into());
            assert_eq!(chat.unread(), 1);
            let mut after = RECT::default();
            GetWindowRect(hwnd, &mut after).unwrap();
            assert_eq!(initial, after);
            tick();
            assert!(chat.visible());
            assert_eq!(chat.unread(), 0);
        }
        drop(indicator);
        assert!(!chat.visible());
    }

    #[test]
    fn toggles_and_repaints_without_waiting_for_idle() {
        let indicator =
            SessionIndicator::show("Banner latency check", meshrmm_chat::ChatSession::default())
                .unwrap();
        let hwnd = HWND(indicator.window as *mut _);
        let mut initial = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut initial).unwrap();
        }
        let mut maximum = Duration::ZERO;
        for click in 0..20 {
            let collapsed = click % 2 == 0;
            let start = Instant::now();
            unsafe {
                PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), LPARAM(0)).unwrap();
                PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), LPARAM(0)).unwrap();
                loop {
                    let mut rect = RECT::default();
                    GetWindowRect(hwnd, &mut rect).unwrap();
                    if (rect.right - rect.left == 32) == collapsed {
                        assert_eq!(rect.top, initial.top, "banner shifted vertically");
                        assert!(
                            ((rect.left + rect.right) - (initial.left + initial.right)).abs() <= 1,
                            "banner shifted away from its center"
                        );
                        assert_eq!(rect.bottom - rect.top, if collapsed { 16 } else { 24 });
                        if !collapsed {
                            assert_eq!(rect.right - rect.left, initial.right - initial.left);
                        }
                        break;
                    }
                    assert!(
                        start.elapsed() < Duration::from_millis(100),
                        "banner click stalled"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                // Wait for the handler, including synchronous repaint, to finish.
                assert_ne!(
                    SendMessageTimeoutW(
                        hwnd,
                        WM_NULL,
                        WPARAM(0),
                        LPARAM(0),
                        SMTO_ABORTIFHUNG,
                        100,
                        None
                    )
                    .0,
                    0
                );
            }
            maximum = maximum.max(start.elapsed());
        }
        unsafe {
            let send = |message, x: i32| {
                assert_ne!(
                    SendMessageTimeoutW(
                        hwnd,
                        message,
                        WPARAM(0),
                        LPARAM(((4i32 << 16) | (x & 0xffff)) as isize),
                        SMTO_ABORTIFHUNG,
                        100,
                        None
                    )
                    .0,
                    0
                );
            };
            // Drag the expanded banner, then collapse it in its new location.
            send(WM_LBUTTONDOWN, 8);
            send(WM_MOUSEMOVE, 58);
            send(WM_LBUTTONUP, 8);
            let mut moved = RECT::default();
            GetWindowRect(hwnd, &mut moved).unwrap();
            assert_eq!(moved.left, initial.left + 50);
            assert_eq!(moved.right - moved.left, initial.right - initial.left);
            assert_eq!(moved.top, initial.top);
            send(WM_LBUTTONDOWN, 8);
            send(WM_LBUTTONUP, 8);
            let mut collapsed = RECT::default();
            GetWindowRect(hwnd, &mut collapsed).unwrap();
            assert_eq!(collapsed.right - collapsed.left, 32);
            assert!(((collapsed.left + collapsed.right) - (moved.left + moved.right)).abs() <= 1);
            // The tiny tab also drags without expanding.
            send(WM_LBUTTONDOWN, 8);
            send(WM_MOUSEMOVE, 58);
            send(WM_LBUTTONUP, 8);
            let mut tab = RECT::default();
            GetWindowRect(hwnd, &mut tab).unwrap();
            assert_eq!(tab.left, collapsed.left + 50);
            assert_eq!(tab.right - tab.left, 32);
            assert_eq!(tab.top, initial.top);
            send(WM_LBUTTONDOWN, 8);
            send(WM_LBUTTONUP, 8);
            let mut expanded = RECT::default();
            GetWindowRect(hwnd, &mut expanded).unwrap();
            assert_eq!(expanded.right - expanded.left, initial.right - initial.left);
            assert!(((expanded.left + expanded.right) - (tab.left + tab.right)).abs() <= 1);
        }
        let closing = Instant::now();
        drop(indicator);
        assert!(
            closing.elapsed() < Duration::from_millis(100),
            "idle UI did not wake to shut down"
        );
        println!("20 banner toggles: maximum click-to-repaint {maximum:?}");
    }
}
