use super::*;
use ::windows::Win32::Foundation::*;
use ::windows::Win32::Graphics::Gdi::{COLOR_WINDOW, GetSysColorBrush, ScreenToClient};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Controls::{EM_SCROLLCARET, EM_SETLIMITTEXT, EM_SETSEL};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use ::windows::Win32::UI::WindowsAndMessaging::*;
use ::windows::core::{PCWSTR, w};
use std::sync::mpsc;
use std::thread::JoinHandle;

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
    owned: bool,
}
unsafe fn create(
    state: Arc<Mutex<State>>,
    parent: Option<HWND>,
    owned: bool,
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
            if owned {
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_CONTROLPARENT | WS_EX_CLIENTEDGE
            } else if parent.is_some() {
                WS_EX_CONTROLPARENT | WS_EX_CLIENTEDGE
            } else {
                WINDOW_EX_STYLE::default()
            },
            class,
            w!("MeshRMM Chat — this session only"),
            if owned {
                WS_POPUP | WS_CLIPCHILDREN
            } else if parent.is_some() {
                WS_CHILD | WS_CLIPCHILDREN
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
            owned,
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
                WM_ACTIVATE if ui.owned && wp.0 & 0xffff == WA_INACTIVE as usize => {
                    ui.state.lock().unwrap_or_else(|e| e.into_inner()).visible = false;
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
                    let width = rect.right.max(240);
                    let height = rect.bottom.max(160);
                    let _ = MoveWindow(ui.history, 12, 12, width - 24, height - 66, true);
                    let _ = MoveWindow(ui.entry, 12, height - 42, width - 110, 30, true);
                    let _ = MoveWindow(ui.send, width - 90, height - 42, 78, 30, true);
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

/// A child panel in the viewer; it has no separate taskbar window.
pub struct Popup {
    window: HWND,
    data: *mut Ui,
    parent: HWND,
    button: HWND,
    session: ChatSession,
    badge: std::cell::Cell<(bool, usize)>,
    owned: bool,
}
impl Popup {
    /// All methods must be called from the owning viewer UI thread.
    ///
    /// # Safety
    /// `parent` and `button` must be live HWNDs on the calling thread.
    pub unsafe fn new(parent: HWND, button: HWND, session: ChatSession) -> anyhow::Result<Self> {
        unsafe { Self::create_popup(parent, button, session, false) }
    }
    /// An attached, titleless popup for a small agent banner. No taskbar entry.
    ///
    /// # Safety
    /// `owner` must be a live HWND on the calling thread.
    pub unsafe fn for_banner(owner: HWND, session: ChatSession) -> anyhow::Result<Self> {
        unsafe { Self::create_popup(owner, owner, session, true) }
    }
    unsafe fn create_popup(
        parent: HWND,
        button: HWND,
        session: ChatSession,
        owned: bool,
    ) -> anyhow::Result<Self> {
        let (window, data) = unsafe { create(Arc::clone(&session.state), Some(parent), owned) }?;
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
            button,
            session,
            badge: std::cell::Cell::new((false, usize::MAX)),
            owned,
        })
    }
    pub fn refresh(&self) {
        if !self.session.available() {
            unsafe {
                let _ = ShowWindow(self.window, SW_HIDE);
            }
        }
        if self.owned {
            return;
        }
        let unread = self.session.unread();
        let available = self.session.available();
        if self.badge.replace((available, unread)) == (available, unread) {
            return;
        }
        let title = if unread == 0 {
            "💬".to_owned()
        } else {
            format!("💬 {unread}")
        };
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let _ = SetWindowTextW(self.button, PCWSTR(title.as_ptr()));
            let _ = EnableWindow(self.button, self.session.available());
        }
        if !self.session.available() && self.session.visible() {
            self.close();
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
                if self.owned {
                    let _ = SetForegroundWindow(self.window);
                }
                let _ = SetFocus(Some((*self.data).entry));
            }
        }
        self.refresh();
    }
    pub fn layout(&self) {
        unsafe {
            let mut bounds = RECT::default();
            let mut anchor = RECT::default();
            let _ = GetClientRect(self.parent, &mut bounds);
            let _ = GetWindowRect(self.button, &mut anchor);
            let mut point = POINT {
                x: anchor.right,
                y: anchor.bottom,
            };
            if self.owned {
                let monitor = ::windows::Win32::Graphics::Gdi::MonitorFromWindow(
                    self.parent,
                    ::windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
                );
                let mut info = ::windows::Win32::Graphics::Gdi::MONITORINFO {
                    cbSize: std::mem::size_of::<::windows::Win32::Graphics::Gdi::MONITORINFO>()
                        as u32,
                    ..Default::default()
                };
                if !::windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info).as_bool() {
                    return;
                }
                let work = info.rcWork;
                let width = 480.min(work.right - work.left);
                let height = 400.min(work.bottom - work.top);
                let x = (point.x - width).clamp(work.left, work.right - width);
                let y = (point.y + 5).clamp(work.top, work.bottom - height);
                let _ = SetWindowPos(
                    self.window,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
                return;
            }
            let _ = ScreenToClient(self.parent, &mut point);
            let width = 480.min((bounds.right - 16).max(240));
            let height = 400.min((bounds.bottom - point.y - 16).max(160));
            let _ = SetWindowPos(
                self.window,
                Some(HWND_TOP),
                (point.x - width).max(8),
                point.y + 5,
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
            // Owned banner popups dismiss through WM_ACTIVATE. Windows may
            // deny foreground activation for an incoming message; the popup
            // must remain visible even when that happens.
            if !self.owned && GetForegroundWindow() != self.parent {
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
                && message.hwnd != self.button
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
