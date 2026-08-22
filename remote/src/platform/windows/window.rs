use super::*;

struct WindowContext {
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
    cursor_shape: CursorShape,
    debug: DebugInfo,
    debug_overlay: HWND,
    debug_visible: bool,
    debug_refreshed: std::time::Instant,
}

impl WindowContext {
    fn send(&self, message: SessionMessage) {
        self.control.send(message);
    }

    fn pointer_position(&self, window: HWND, lparam: LPARAM) -> Option<(u16, u16)> {
        self.normalized_client_position(
            window,
            signed_low_word(lparam.0),
            signed_high_word(lparam.0),
        )
    }

    fn normalized_client_position(&self, window: HWND, x: i32, y: i32) -> Option<(u16, u16)> {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(window, &mut rect) }.is_err() {
            return None;
        }
        let width = rect.right.saturating_sub(rect.left).max(1);
        let height = rect.bottom.saturating_sub(rect.top).max(1);
        if x < 0 || x >= width || y < 0 || y >= height {
            return None;
        }
        Some((
            (i64::from(x) * 65_535 / i64::from((width - 1).max(1))) as u16,
            (i64::from(y) * 65_535 / i64::from((height - 1).max(1))) as u16,
        ))
    }

    fn move_pointer(&self, window: HWND, lparam: LPARAM) {
        if let Some((x, y)) = self.pointer_position(window, lparam) {
            self.send(SessionMessage::Input(RemoteInput::PointerMove {
                display_id: self.active_display.id,
                x,
                y,
            }));
        }
    }

    fn button(&mut self, window: HWND, lparam: LPARAM, button: PointerButton, pressed: bool) {
        let position = self.pointer_position(window, lparam);
        match position {
            Some((x, y)) => self.send(SessionMessage::Input(RemoteInput::PointerButtonAt {
                display_id: self.active_display.id,
                x,
                y,
                button,
                pressed,
            })),
            None if !pressed && self.pressed_buttons.contains(&button) => {
                // Finish a drag that began over the video without moving the
                // remote pointer to an out-of-bounds/clamped position.
                self.send(SessionMessage::Input(RemoteInput::PointerButton {
                    display_id: self.active_display.id,
                    button,
                    pressed: false,
                }));
            }
            None => return,
        }
        if pressed {
            self.pressed_buttons.insert(button);
            let _ = unsafe { SetCapture(window) };
        } else {
            self.pressed_buttons.remove(&button);
            if self.pressed_buttons.is_empty() {
                let _ = unsafe { ReleaseCapture() };
            }
        }
    }

    fn release_input(&mut self) {
        for (scan_code, extended) in self.pressed_keys.drain().collect::<Vec<_>>() {
            self.send(SessionMessage::Input(RemoteInput::Key {
                display_id: self.active_display.id,
                scan_code,
                extended,
                pressed: false,
            }));
        }
        for button in self.pressed_buttons.drain().collect::<Vec<_>>() {
            self.send(SessionMessage::Input(RemoteInput::PointerButton {
                display_id: self.active_display.id,
                button,
                pressed: false,
            }));
        }
    }

    fn select_next_display(&self) {
        if self.displays.len() < 2 {
            return;
        }
        let current = self
            .displays
            .iter()
            .position(|display| display.id == self.active_display.id)
            .unwrap_or(0);
        let next = self.displays[(current + 1) % self.displays.len()].id;
        self.send(SessionMessage::SelectDisplay { display_id: next });
    }

    fn toggle_debug(&mut self) {
        self.debug_visible = !self.debug_visible;
        let command = if self.debug_visible { SW_SHOW } else { SW_HIDE };
        let _ = unsafe { ShowWindow(self.debug_overlay, command) };
        if self.debug_visible {
            self.refresh_debug(true);
        }
    }

    fn refresh_debug(&mut self, force: bool) {
        if !self.debug_visible
            || (!force && self.debug_refreshed.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.debug_refreshed = std::time::Instant::now();
        let text = HSTRING::from(self.debug.render().replace('\n', "\r\n"));
        let _ = unsafe { SetWindowTextW(self.debug_overlay, PCWSTR(text.as_ptr())) };
    }
}

unsafe fn window_context(window: HWND) -> Option<&'static mut WindowContext> {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowContext;
    unsafe { pointer.as_mut() }
}

fn signed_low_word(value: isize) -> i32 {
    i32::from(value as u16 as i16)
}

fn signed_high_word(value: isize) -> i32 {
    i32::from(((value as usize >> 16) as u16) as i16)
}

pub(super) unsafe fn create_window(
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
) -> anyhow::Result<HWND> {
    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe {
                SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            }
        }
        let context = unsafe { window_context(window) };
        match message {
            WM_MOUSEMOVE => {
                if let Some(context) = context {
                    context.move_pointer(window, lparam);
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if let Some(context) = context
                    && (lparam.0 as u32 & 0xffff) == HTCLIENT
                {
                    unsafe { apply_cursor(context.cursor_shape) };
                    return LRESULT(1);
                }
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if let Some(context) = context {
                    let button = match message {
                        WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Left,
                        WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Right,
                        WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
                        _ if ((wparam.0 >> 16) as u16) == XBUTTON1 => PointerButton::Back,
                        _ => PointerButton::Forward,
                    };
                    let pressed = matches!(
                        message,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                    );
                    context.button(window, lparam, button, pressed);
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if let Some(context) = context {
                    let mut point = windows::Win32::Foundation::POINT {
                        x: signed_low_word(lparam.0),
                        y: signed_high_word(lparam.0),
                    };
                    if unsafe { ScreenToClient(window, &mut point) }.as_bool()
                        && let Some((x, y)) =
                            context.normalized_client_position(window, point.x, point.y)
                    {
                        let delta = ((wparam.0 >> 16) as u16) as i16;
                        context.send(SessionMessage::Input(RemoteInput::WheelAt {
                            display_id: context.active_display.id,
                            x,
                            y,
                            horizontal: if message == WM_MOUSEHWHEEL { delta } else { 0 },
                            vertical: if message == WM_MOUSEWHEEL { delta } else { 0 },
                        }));
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                if let Some(context) = context {
                    let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
                    if wparam.0 as u16 == VK_F8.0 && pressed {
                        if lparam.0 & (1 << 30) == 0 {
                            context.select_next_display();
                        }
                        return LRESULT(0);
                    }
                    if wparam.0 as u16 == VK_F8.0 {
                        return LRESULT(0);
                    }
                    if wparam.0 as u16 == VK_F12.0 {
                        if pressed && lparam.0 & (1 << 30) == 0 {
                            context.toggle_debug();
                        }
                        return LRESULT(0);
                    }
                    let scan_code = ((lparam.0 >> 16) & 0xff) as u16;
                    let extended = lparam.0 & (1 << 24) != 0;
                    if scan_code != 0 {
                        context.send(SessionMessage::Input(RemoteInput::Key {
                            display_id: context.active_display.id,
                            scan_code,
                            extended,
                            pressed,
                        }));
                        if pressed {
                            context.pressed_keys.insert((scan_code, extended));
                        } else {
                            context.pressed_keys.remove(&(scan_code, extended));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_SETFOCUS => {
                if let Some(context) = context {
                    context.control.set_input_enabled(true);
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC => unsafe {
                SetTextColor(
                    windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                    windows::Win32::Foundation::COLORREF(0x00f4_f4f4),
                );
                SetBkColor(
                    windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                    windows::Win32::Foundation::COLORREF(0x0014_1414),
                );
                LRESULT(GetStockObject(BLACK_BRUSH).0 as isize)
            },
            WM_KILLFOCUS => {
                if let Some(context) = context {
                    context.release_input();
                    context.control.set_input_enabled(false);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if let Some(context) = context {
                    context.release_input();
                    context.control.set_input_enabled(false);
                }
                let _ = unsafe { DestroyWindow(window) };
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let pointer =
                    unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowContext;
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                if !pointer.is_null() {
                    drop(unsafe { Box::from_raw(pointer) });
                }
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    let module =
        unsafe { GetModuleHandleW(None) }.context("application module handle unavailable")?;
    let instance = HINSTANCE(module.0);
    let class = w!("PulseRmmRemoteDesktopWindow");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        // A second session in the same process may find the class registered.
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("remote desktop window class registration failed");
        }
    }
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: format.width as i32,
        bottom: format.height as i32,
    };
    unsafe { AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false) }
        .context("remote window bounds calculation failed")?;
    let title = HSTRING::from(format!(
        "PulseRMM Remote Desktop — {} ({}) — F8 display · F12 diagnostics",
        active_display.name,
        if active_display.primary {
            "primary"
        } else {
            "secondary"
        }
    ));
    let context = Box::new(WindowContext {
        active_display,
        displays,
        control,
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
        cursor_shape: CursorShape::Default,
        debug,
        debug_overlay: HWND::default(),
        debug_visible: false,
        debug_refreshed: std::time::Instant::now(),
    });
    let context = Box::into_raw(context);
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance),
            Some(context.cast()),
        )
    };
    let window = match window {
        Ok(window) => window,
        Err(error) => {
            drop(unsafe { Box::from_raw(context) });
            return Err(error).context("native remote desktop window creation failed");
        }
    };
    let overlay = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_BORDER,
            12,
            12,
            640,
            300,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("debug overlay creation failed")?;
    if let Some(context) = unsafe { window_context(window) } {
        context.debug_overlay = overlay;
    }
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    Ok(window)
}

pub(super) unsafe fn set_window_cursor(window: HWND, shape: CursorShape) {
    if let Some(context) = unsafe { window_context(window) } {
        context.cursor_shape = shape;
        unsafe { apply_cursor(shape) };
    }
}

unsafe fn apply_cursor(shape: CursorShape) {
    let resource = match shape {
        CursorShape::Default => IDC_ARROW,
        CursorShape::Text => IDC_IBEAM,
        CursorShape::Wait => IDC_WAIT,
        CursorShape::Crosshair => IDC_CROSS,
        CursorShape::UpArrow => IDC_UPARROW,
        CursorShape::ResizeNorthWestSouthEast => IDC_SIZENWSE,
        CursorShape::ResizeNorthEastSouthWest => IDC_SIZENESW,
        CursorShape::ResizeWestEast => IDC_SIZEWE,
        CursorShape::ResizeNorthSouth => IDC_SIZENS,
        CursorShape::Move => IDC_SIZEALL,
        CursorShape::NotAllowed => IDC_NO,
        CursorShape::Pointer => IDC_HAND,
        CursorShape::Progress => IDC_APPSTARTING,
        CursorShape::Help => IDC_HELP,
        CursorShape::Pin => IDC_PIN,
        CursorShape::Person => IDC_PERSON,
    };
    let cursor =
        unsafe { LoadCursorW(None, resource) }.or_else(|_| unsafe { LoadCursorW(None, IDC_ARROW) });
    if let Ok(cursor) = cursor {
        unsafe { SetCursor(Some(cursor)) };
    }
}

pub(super) unsafe fn pump_window_messages(window: HWND) -> bool {
    if let Some(context) = unsafe { window_context(window) } {
        context.refresh_debug(false);
    }
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return true;
        }
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    false
}
