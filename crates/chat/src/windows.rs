use super::*;
use ::windows::Win32::Foundation::*;
use ::windows::Win32::Graphics::Gdi::{COLOR_WINDOW, GetSysColorBrush};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Controls::{EM_SCROLLCARET, EM_SETLIMITTEXT, EM_SETSEL};
use ::windows::Win32::UI::HiDpi::GetDpiForWindow;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, SetFocus};
use ::windows::Win32::UI::WindowsAndMessaging::*;
use ::windows::core::{PCWSTR, w};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Scales a 96-DPI length to the DPI of `window`. Windows of processes that
/// are not DPI aware always report 96.
fn scaled(window: HWND, value: i32) -> i32 {
    let dpi = match unsafe { GetDpiForWindow(window) } {
        0 => 96,
        dpi => dpi,
    };
    ((i64::from(value) * i64::from(dpi) + 48) / 96) as i32
}

pub(super) struct Window {
    thread: Option<JoinHandle<()>>,
    id: u32,
}
impl Window {
    pub fn open(state: Arc<Mutex<State>>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("session-chat".into())
            .spawn(move || unsafe {
                match create(state, None, false) {
                    Ok((hwnd, data)) => {
                        if tx.send(Ok(GetCurrentThreadId())).is_ok() {
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                                if !IsDialogMessageW(hwnd, &msg).as_bool() {
                                    let _ = TranslateMessage(&msg);
                                    DispatchMessageW(&msg);
                                }
                            }
                        }
                        let _ = DestroyWindow(hwnd);
                        drop(Box::from_raw(data));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                    }
                }
            })?;
        match rx.recv() {
            Ok(Ok(id)) => Ok(Self {
                thread: Some(thread),
                id,
            }),
            result => {
                let _ = thread.join();
                anyhow::bail!("chat window failed: {result:?}")
            }
        }
    }
    pub fn refresh(&self) {}
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct Ui {
    state: Arc<Mutex<State>>,
    history: HWND,
    entry: HWND,
    send: HWND,
    revision: u64,
    popup: bool,
    banner: bool,
    /// When a banner popup last lost activation while shown.
    dismissed: Option<std::time::Instant>,
}
/// With a parent, creates an owned popup; `banner` keeps it above all windows.
unsafe fn create(
    state: Arc<Mutex<State>>,
    parent: Option<HWND>,
    banner: bool,
) -> ::windows::core::Result<(HWND, *mut Ui)> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MeshRMMChat");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            if banner {
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_CONTROLPARENT | WS_EX_CLIENTEDGE
            } else if parent.is_some() {
                WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT | WS_EX_CLIENTEDGE
            } else {
                WINDOW_EX_STYLE::default()
            },
            class,
            w!("MeshRMM Chat — this session only"),
            if parent.is_some() {
                WS_POPUP | WS_CLIPCHILDREN
            } else {
                WS_OVERLAPPEDWINDOW
            },
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            420,
            parent,
            None,
            Some(instance.into()),
            None,
        )?;
        let controls = (|| {
            let history = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(
                    "Chat with the other computer. Messages are not saved.\r\nMaximum 4 KiB per message. Use Send to submit."
                ),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | WINDOW_STYLE(
                        ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
                    ),
                12,
                12,
                440,
                285,
                Some(hwnd),
                None,
                Some(instance.into()),
                None,
            )?;
            let entry = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                12,
                310,
                345,
                30,
                Some(hwnd),
                None,
                Some(instance.into()),
                None,
            )?;
            SendMessageW(entry, EM_SETLIMITTEXT, Some(WPARAM(4096)), None);
            let send = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                w!("Send"),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
                365,
                310,
                80,
                30,
                Some(hwnd),
                // For a child control this parameter is its numeric ID (IDOK).
                Some(HMENU(std::ptr::without_provenance_mut(1))),
                Some(instance.into()),
                None,
            )?;
            Ok::<_, ::windows::core::Error>((history, entry, send))
        })();
        let (history, entry, send) = match controls {
            Ok(c) => c,
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
        };
        let data = Box::into_raw(Box::new(Ui {
            state,
            history,
            entry,
            send,
            revision: u64::MAX,
            popup: parent.is_some(),
            banner,
            dismissed: None,
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, data as isize);
        SetTimer(Some(hwnd), 1, 200, None);
        if parent.is_none() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok((hwnd, data))
    }
}
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
        if !ptr.is_null() {
            let ui = &mut *ptr;
            match msg {
                WM_ACTIVATE if ui.banner && wp.0 & 0xffff == WA_INACTIVE as usize => {
                    let mut state = ui.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.visible {
                        ui.dismissed = Some(std::time::Instant::now());
                    }
                    state.visible = false;
                    drop(state);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    if ui.popup {
                        ui.state.lock().unwrap_or_else(|e| e.into_inner()).visible = false;
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    } else {
                        let _ = ShowWindow(hwnd, SW_MINIMIZE);
                    }
                    return LRESULT(0);
                }
                WM_SIZE => {
                    let mut rect = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rect);
                    let px = |value| scaled(hwnd, value);
                    let width = rect.right.max(px(240));
                    let height = rect.bottom.max(px(160));
                    let _ = MoveWindow(
                        ui.history,
                        px(12),
                        px(12),
                        width - px(24),
                        height - px(66),
                        true,
                    );
                    let _ = MoveWindow(
                        ui.entry,
                        px(12),
                        height - px(42),
                        width - px(110),
                        px(30),
                        true,
                    );
                    let _ = MoveWindow(
                        ui.send,
                        width - px(90),
                        height - px(42),
                        px(78),
                        px(30),
                        true,
                    );
                    return LRESULT(0);
                }
                WM_COMMAND
                    if ui.popup
                        && (wp.0 >> 16) & 0xffff == EN_CHANGE as usize
                        && lp.0 == ui.entry.0 as isize =>
                {
                    let mut buf = vec![0u16; 4097];
                    let n = GetWindowTextW(ui.entry, &mut buf) as usize;
                    ui.state.lock().unwrap_or_else(|e| e.into_inner()).draft =
                        String::from_utf16_lossy(&buf[..n]);
                    return LRESULT(0);
                }
                WM_COMMAND if wp.0 & 0xffff == 1 => {
                    let mut buf = vec![0u16; 4097];
                    let n = GetWindowTextW(ui.entry, &mut buf) as usize;
                    let text = String::from_utf16_lossy(&buf[..n]);
                    if ui
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .send(text)
                    {
                        let _ = SetWindowTextW(ui.entry, w!(""));
                        let _ = SetWindowTextW(hwnd, w!("MeshRMM Chat — this session only"));
                    } else {
                        let _ = SetWindowTextW(
                            hwnd,
                            w!("Chat — enter 1–4096 UTF-8 bytes, or wait for the send queue"),
                        );
                    }
                    return LRESULT(0);
                }
                WM_TIMER => {
                    let available = ui.state.lock().unwrap_or_else(|e| e.into_inner()).available;
                    if ui.popup && !available {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                    let state = ui.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.revision != ui.revision {
                        ui.revision = state.revision;
                        let text: Vec<u16> = state
                            .text()
                            .replace('\n', "\r\n")
                            .encode_utf16()
                            .chain(Some(0))
                            .collect();
                        let _ = SetWindowTextW(ui.history, PCWSTR(text.as_ptr()));
                        SendMessageW(
                            ui.history,
                            EM_SETSEL,
                            Some(WPARAM(usize::MAX)),
                            Some(LPARAM(-1)),
                        );
                        SendMessageW(ui.history, EM_SCROLLCARET, None, None);
                        if !ui.popup {
                            let _ = FlashWindow(hwnd, true);
                        }
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// A panel owned by the viewer window and placed under its chat button. It
/// is a popup rather than a child window because the viewer's video would
/// cover a child. It has no taskbar button.
pub struct Popup {
    window: HWND,
    data: *mut Ui,
    parent: HWND,
    /// The chat button, in the parent's client coordinates. The viewer's
    /// toolbar draws the button, its unread count and its tooltip. A banner
    /// popup has none and sits under its owner unless given a screen anchor.
    anchor: std::cell::Cell<Option<RECT>>,
    /// A notification-area icon or click point, in screen coordinates.
    screen_anchor: std::cell::Cell<Option<RECT>>,
    session: ChatSession,
    banner: bool,
}
impl Popup {
    /// All methods must be called from the owning viewer UI thread.
    ///
    /// # Safety
    /// `parent` must be a live HWND on the calling thread.
    pub unsafe fn new(parent: HWND, session: ChatSession) -> anyhow::Result<Self> {
        unsafe { Self::create_popup(parent, session, false) }
    }
    /// An attached, titleless popup for the agent's session notice. It opens
    /// from the notification-area icon rather than from the banner itself.
    /// No taskbar entry.
    ///
    /// # Safety
    /// `owner` must be a live HWND on the calling thread.
    pub unsafe fn for_banner(owner: HWND, session: ChatSession) -> anyhow::Result<Self> {
        unsafe { Self::create_popup(owner, session, true) }
    }
    unsafe fn create_popup(
        parent: HWND,
        session: ChatSession,
        banner: bool,
    ) -> anyhow::Result<Self> {
        let (window, data) = unsafe { create(Arc::clone(&session.state), Some(parent), banner) }?;
        let draft = session
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .draft
            .clone();
        let text: Vec<u16> = draft.encode_utf16().chain(Some(0)).collect();
        unsafe {
            SetWindowTextW((*data).entry, PCWSTR(text.as_ptr()))?;
        }
        Ok(Self {
            window,
            data,
            parent,
            anchor: std::cell::Cell::new(None),
            screen_anchor: std::cell::Cell::new(None),
            session,
            banner,
        })
    }
    /// Places the popup under `anchor`, in the parent's client coordinates.
    pub fn set_anchor(&self, anchor: RECT) {
        self.anchor.set(Some(anchor));
        if self.session.visible() {
            self.layout();
        }
    }
    /// Places the popup beside `anchor`, in screen coordinates. It stays on
    /// the anchor's monitor, above or below a taskbar at either screen edge.
    pub fn set_screen_anchor(&self, anchor: RECT) {
        self.screen_anchor.set(Some(anchor));
        if self.session.visible() {
            self.layout();
        }
    }
    /// Whether the popup was just hidden because another window, such as the
    /// taskbar whose icon toggles it, took activation. That click must not
    /// reopen it.
    pub fn recently_dismissed(&self) -> bool {
        let limit = std::time::Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }));
        unsafe { (*self.data).dismissed }.is_some_and(|dismissed| dismissed.elapsed() < limit)
    }
    /// The anchor in screen coordinates.
    fn anchor_on_screen(&self) -> RECT {
        if let Some(anchor) = self.screen_anchor.get() {
            return anchor;
        }
        let mut anchor = RECT::default();
        unsafe {
            match self.anchor.get() {
                Some(rect) => {
                    let mut corners = [
                        POINT {
                            x: rect.left,
                            y: rect.top,
                        },
                        POINT {
                            x: rect.right,
                            y: rect.bottom,
                        },
                    ];
                    let _ = ::windows::Win32::Graphics::Gdi::MapWindowPoints(
                        Some(self.parent),
                        None,
                        &mut corners,
                    );
                    anchor = RECT {
                        left: corners[0].x,
                        top: corners[0].y,
                        right: corners[1].x,
                        bottom: corners[1].y,
                    };
                }
                None => {
                    let _ = GetWindowRect(self.parent, &mut anchor);
                }
            }
        }
        anchor
    }
    /// A click on the chat button toggles the popup rather than dismissing it.
    fn on_anchor(&self, message: &MSG) -> bool {
        if self.anchor.get().is_none() {
            return message.hwnd == self.parent;
        }
        let anchor = self.anchor_on_screen();
        (anchor.left..anchor.right).contains(&message.pt.x)
            && (anchor.top..anchor.bottom).contains(&message.pt.y)
    }
    pub fn refresh(&self) {
        if !self.session.available() {
            unsafe {
                let _ = ShowWindow(self.window, SW_HIDE);
            }
            if !self.banner && self.session.visible() {
                self.close();
            }
        }
    }
    pub fn toggle(&self) {
        if self.session.visible() {
            self.close();
        } else if self.session.available() {
            self.session.set_visible(true);
            self.layout();
            unsafe {
                let _ = ShowWindow(self.window, SW_SHOW);
                let _ = SetForegroundWindow(self.window);
                let _ = SetFocus(Some((*self.data).entry));
            }
        }
        self.refresh();
    }
    pub fn layout(&self) {
        unsafe {
            let anchor = self.anchor_on_screen();
            let monitor = if self.screen_anchor.get().is_some() {
                ::windows::Win32::Graphics::Gdi::MonitorFromRect(
                    &anchor,
                    ::windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
                )
            } else {
                ::windows::Win32::Graphics::Gdi::MonitorFromWindow(
                    self.parent,
                    ::windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
                )
            };
            let mut info = ::windows::Win32::Graphics::Gdi::MONITORINFO {
                cbSize: std::mem::size_of::<::windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
                ..Default::default()
            };
            if !::windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }
            let work = info.rcWork;
            let width = scaled(self.parent, 480).min(work.right - work.left);
            let height = scaled(self.parent, 400).min(work.bottom - work.top);
            let x = (anchor.right - width).clamp(work.left, work.right - width);
            let y = (anchor.bottom + scaled(self.parent, 5)).clamp(work.top, work.bottom - height);
            let _ = SetWindowPos(
                self.window,
                Some(if self.banner { HWND_TOPMOST } else { HWND_TOP }),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
    }
    pub fn close(&self) {
        self.save_draft();
        self.session.set_visible(false);
        unsafe {
            let _ = ShowWindow(self.window, SW_HIDE);
            if GetForegroundWindow() == self.parent {
                let _ = SetFocus(Some(self.parent));
            }
        }
    }
    fn save_draft(&self) {
        unsafe {
            if !IsWindow(Some(self.window)).as_bool() {
                return;
            }
            let mut buf = vec![0u16; 4097];
            let n = GetWindowTextW((*self.data).entry, &mut buf) as usize;
            self.session
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .draft = String::from_utf16_lossy(&buf[..n]);
        }
    }
    pub fn handle_message(&self, message: &MSG) -> bool {
        if !self.session.visible() {
            return false;
        }
        unsafe {
            // Banner popups dismiss through WM_ACTIVATE. Windows may deny
            // foreground activation for an incoming message; the popup must
            // remain visible even when that happens.
            let foreground = GetForegroundWindow();
            if !self.banner && foreground != self.parent && foreground != self.window {
                self.close();
                return false;
            }
            let inside =
                message.hwnd == self.window || IsChild(self.window, message.hwnd).as_bool();
            if message.message == WM_KEYDOWN && message.wParam.0 == 27 {
                self.close();
                return true;
            }
            if matches!(message.message, WM_LBUTTONDOWN | WM_RBUTTONDOWN)
                && !inside
                && !self.on_anchor(message)
            {
                self.close();
                return true; // The dismissal click must not control the remote computer.
            }
            inside && IsDialogMessageW(self.window, message).as_bool()
        }
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        self.save_draft();
        self.session.set_visible(false);
        unsafe {
            if IsWindow(Some(self.window)).as_bool() {
                let _ = DestroyWindow(self.window);
            }
            drop(Box::from_raw(self.data));
        }
    }
}
