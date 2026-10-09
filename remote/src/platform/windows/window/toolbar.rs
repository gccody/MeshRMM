//! The toolbar above the video, drawn by the viewer: see [`crate::toolbar`]
//! for its items, layout and icons. A child window paints them, with GDI+
//! for the shapes and GDI for ClearType text, tracks the mouse, shows their
//! menus and tooltips, and lets the empty space between them move the
//! window. The borderless window's caption buttons are toolbar items too.
//! This module also creates the owned popups over the video.

mod menu;
mod paint;

use super::*;
use crate::toolbar::{self, Action, Command, Item, Layout, Rect};
use windows::Win32::Foundation::{POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetTextExtentPoint32W, HDC, InvalidateRect, ReleaseDC, SelectObject,
};
use windows::Win32::UI::Controls::{
    ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, NMHDR, NMTTDISPINFOW,
    TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_DELTOOLW, TTM_SETMAXTIPWIDTH,
    TTN_GETDISPINFOW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::core::PWSTR;

const RETRY_BUTTON_ID: usize = 4030;

/// The toolbar's items, where they are, and the mouse over them.
#[derive(Default)]
pub(super) struct ToolbarModel {
    state: toolbar::State,
    items: Vec<Item>,
    /// In 96-DPI pixels.
    layout: Layout,
    /// The items' tooltips, which the tooltip control reads by item index.
    tooltips: Vec<HSTRING>,
    /// How many tools the tooltip control has.
    tools: usize,
    hovered: Option<usize>,
    pressed: Option<usize>,
    tracking: bool,
}

impl WindowContext {
    /// The native controls that use the message font.
    pub(super) fn toolbar_controls(&self) -> [HWND; 3] {
        let controls = self.controls();
        [
            controls.debug_overlay,
            controls.reconnecting_label,
            controls.retry_button,
        ]
    }

    pub(super) fn set_quality(&self, preset: QualityPreset) {
        self.send(SessionMessage::SetQuality { preset });
        self.show_quality(preset);
    }

    /// Shows `preset` in the toolbar and settings without sending it.
    pub(super) fn show_quality(&self, preset: QualityPreset) {
        for (button, candidate) in self.controls().quality_buttons {
            let state = usize::from(candidate == preset);
            unsafe { SendMessageW(button, BM_SETCHECK, Some(WPARAM(state)), None) };
        }
        self.refresh_toolbar();
    }

    pub(super) fn set_chroma(&self, mode: ChromaMode) {
        if self.control.supports_chroma(mode) {
            self.send(SessionMessage::SetChroma { mode });
        }
        self.show_chroma(self.control.chroma_mode());
    }

    /// Shows `mode` in the toolbar and settings without sending it.
    pub(super) fn show_chroma(&self, mode: ChromaMode) {
        for (button, candidate) in self.controls().chroma_buttons {
            let state = usize::from(candidate == mode);
            unsafe { SendMessageW(button, BM_SETCHECK, Some(WPARAM(state)), None) };
        }
        self.refresh_toolbar();
    }

    /// What the toolbar shows now.
    pub(super) fn toolbar_state(&self) -> toolbar::State {
        let displays = self.displays.borrow();
        let active = self.active_display.borrow();
        let sessions = Display::sessions(&displays);
        let visible = active.session_displays(&displays);
        let chat = self.control.chat();
        let toolbox = self.control.toolbox().snapshot();
        toolbar::State {
            sessions: sessions.iter().map(|session| session.label()).collect(),
            session: sessions
                .iter()
                .position(|session| *session == active.session)
                .unwrap_or(0),
            displays: visible
                .iter()
                .enumerate()
                .map(|(index, display)| display.selection_label(index))
                .collect(),
            display: visible
                .iter()
                .position(|display| display.id == active.id)
                .unwrap_or(0),
            pointer_display: self
                .agent_pointer_display
                .get()
                .and_then(|id| visible.iter().position(|display| display.id == id)),
            quality: self.control.quality_preset(),
            chroma: Some((
                self.control.chroma_mode(),
                self.control.supports_chroma(ChromaMode::Yuv444),
            )),
            credentials: self.control.credential_state(),
            input_blocked: self.control.technician_blocked(),
            annotating: self.annotating(),
            annotation_available: active.session != meshrmm_protocol::DesktopSession::Background,
            chat_available: chat.available(),
            chat_unread: chat.unread(),
            power: self.control.power_state(),
            device_is_mac: self.control.device_is_mac(),
            file_status: self.control.files().status(),
            toolbox_available: toolbox.available,
            toolbox_busy: toolbox.busy,
            toolbox_status: toolbox.status,
            recording: self.control.recording().active(),
            diagnostics: self.debug_visible.get(),
            settings_menu: false,
            caption: Some(unsafe { IsZoomed(self.window.get()) }.as_bool()),
        }
    }

    /// Shows the current state, redrawing only when an item changed.
    pub(super) fn refresh_toolbar(&self) {
        self.update_toolbar(false);
    }

    fn update_toolbar(&self, relayout: bool) {
        let state = self.toolbar_state();
        let changed = {
            let mut model = self.toolbar.borrow_mut();
            if model.state == state && !relayout {
                return;
            }
            let items = toolbar::items(&state);
            model.state = state;
            if model.items == items {
                false
            } else {
                model.tooltips = items
                    .iter()
                    .map(|item| HSTRING::from(&item.tooltip))
                    .collect();
                model.items = items;
                true
            }
        };
        if changed || relayout {
            self.relayout_toolbar();
        }
    }

    fn scale_factor(&self) -> f64 {
        f64::from(self.dpi.get()) / 96.0
    }

    /// A toolbar rectangle in the toolbar's client pixels, which are also
    /// the window's.
    fn device_rect(&self, rect: Rect) -> RECT {
        let scale = self.scale_factor();
        RECT {
            left: (rect.x * scale).round() as i32,
            top: (rect.y * scale).round() as i32,
            right: (rect.right() * scale).round() as i32,
            bottom: (rect.bottom() * scale).round() as i32,
        }
    }

    /// Labels' widths in 96-DPI pixels, measured on `dc` in the toolbar font.
    fn measure(&self, dc: HDC, text: &str) -> f64 {
        let text: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = unsafe { GetTextExtentPoint32W(dc, &text, &mut size) };
        f64::from(size.cx) / self.scale_factor()
    }

    fn relayout_toolbar(&self) {
        let controls = self.controls();
        if controls.toolbar.is_invalid() {
            return;
        }
        let (width, _) = unsafe { client_size(controls.toolbar) }.unwrap_or_default();
        let width = f64::from(width) / self.scale_factor();
        let layout = unsafe {
            let dc = GetDC(Some(controls.toolbar));
            let old = SelectObject(dc, HGDIOBJ(self.toolbar_font.get().0));
            let layout = toolbar::layout(&self.toolbar.borrow().items, width, 0.0, &|text| {
                self.measure(dc, text)
            });
            SelectObject(dc, old);
            ReleaseDC(Some(controls.toolbar), dc);
            layout
        };
        let rects: Vec<RECT> = layout
            .rects
            .iter()
            .map(|rect| self.device_rect(*rect))
            .collect();
        let chat = {
            let mut model = self.toolbar.borrow_mut();
            let chat = model
                .items
                .iter()
                .position(|item| item.action == Action::Chat)
                .map(|index| rects[index]);
            model.layout = layout;
            chat
        };
        unsafe { self.sync_tooltips(&rects) };
        if let (Some(anchor), Some(popup)) = (chat, self.chat_popup.get()) {
            popup.set_anchor(anchor);
        }
        let _ = unsafe { InvalidateRect(Some(controls.toolbar), None, false) };
    }

    /// Gives each item a tooltip tool over its rectangle. The control asks
    /// for the text when it shows a tip.
    unsafe fn sync_tooltips(&self, rects: &[RECT]) {
        let controls = self.controls();
        if controls.tooltip.is_invalid() {
            return;
        }
        let tool = |id: usize, rect: RECT| TTTOOLINFOW {
            // Without the reserved field, which comctl32 before version 6
            // rejects.
            cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32,
            uFlags: TTF_SUBCLASS,
            hwnd: controls.toolbar,
            uId: id,
            rect,
            lpszText: PWSTR(usize::MAX as *mut u16),
            ..Default::default()
        };
        let old_tools = std::mem::replace(&mut self.toolbar.borrow_mut().tools, rects.len());
        for id in 0..old_tools {
            let info = tool(id, RECT::default());
            unsafe {
                SendMessageW(
                    controls.tooltip,
                    TTM_DELTOOLW,
                    None,
                    Some(LPARAM(&info as *const _ as isize)),
                )
            };
        }
        for (id, rect) in rects.iter().enumerate() {
            let info = tool(id, *rect);
            unsafe {
                SendMessageW(
                    controls.tooltip,
                    TTM_ADDTOOLW,
                    None,
                    Some(LPARAM(&info as *const _ as isize)),
                )
            };
        }
    }

    fn toolbar_point(&self, lparam: LPARAM) -> (f64, f64) {
        let scale = self.scale_factor();
        (
            f64::from(signed_low_word(lparam.0)) / scale,
            f64::from(signed_high_word(lparam.0)) / scale,
        )
    }

    fn toolbar_hit(&self, point: (f64, f64)) -> Option<usize> {
        self.toolbar.borrow().layout.hit(point.0, point.1)
    }

    fn set_hovered(&self, toolbar_window: HWND, hovered: Option<usize>) {
        let changed = {
            let mut model = self.toolbar.borrow_mut();
            std::mem::replace(&mut model.hovered, hovered) != hovered
        };
        if changed {
            let _ = unsafe { InvalidateRect(Some(toolbar_window), None, false) };
        }
    }

    fn toolbar_mouse_move(&self, toolbar_window: HWND, lparam: LPARAM) {
        let start_tracking = !std::mem::replace(&mut self.toolbar.borrow_mut().tracking, true);
        if start_tracking {
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: toolbar_window,
                dwHoverTime: 0,
            };
            let _ = unsafe { TrackMouseEvent(&mut track) };
        }
        let hovered = self.toolbar_hit(self.toolbar_point(lparam));
        self.set_hovered(toolbar_window, hovered);
    }

    fn toolbar_mouse_leave(&self, toolbar_window: HWND) {
        self.toolbar.borrow_mut().tracking = false;
        self.set_hovered(toolbar_window, None);
    }

    /// The item under the pointer now, after a menu consumed the mouse.
    fn track_cursor(&self, toolbar_window: HWND) {
        let mut point = POINT::default();
        let hovered = if unsafe { GetCursorPos(&mut point) }.is_ok()
            && unsafe { ScreenToClient(toolbar_window, &mut point) }.as_bool()
        {
            let scale = self.scale_factor();
            self.toolbar_hit((f64::from(point.x) / scale, f64::from(point.y) / scale))
        } else {
            None
        };
        self.set_hovered(toolbar_window, hovered);
    }

    fn toolbar_button_down(&self, toolbar_window: HWND, lparam: LPARAM) {
        let Some(index) = self.toolbar_hit(self.toolbar_point(lparam)) else {
            return;
        };
        let Some(item) = self.toolbar.borrow().items.get(index).cloned() else {
            return;
        };
        if !item.enabled {
            return;
        }
        self.toolbar.borrow_mut().pressed = Some(index);
        let _ = unsafe { InvalidateRect(Some(toolbar_window), None, false) };
        if item.menu {
            // The menu tracks the mouse until it closes.
            self.fire(index);
            self.toolbar.borrow_mut().pressed = None;
            self.track_cursor(toolbar_window);
            let _ = unsafe { InvalidateRect(Some(toolbar_window), None, false) };
        } else {
            let _ = unsafe { SetCapture(toolbar_window) };
        }
    }

    fn toolbar_button_up(&self, toolbar_window: HWND, lparam: LPARAM) {
        let Some(pressed) = self.toolbar.borrow_mut().pressed.take() else {
            return;
        };
        let _ = unsafe { ReleaseCapture() };
        let _ = unsafe { InvalidateRect(Some(toolbar_window), None, false) };
        if self.toolbar_hit(self.toolbar_point(lparam)) == Some(pressed) {
            self.fire(pressed);
        }
    }

    fn fire(&self, index: usize) {
        let target = {
            let model = self.toolbar.borrow();
            model
                .items
                .get(index)
                .map(|item| item.action)
                .zip(model.layout.rects.get(index).copied())
        };
        if let Some((action, rect)) = target {
            self.toolbar_action(action, rect);
        }
    }

    /// A click on the toolbar item for `action`, at `rect` in the toolbar.
    fn toolbar_action(&self, action: Action, rect: Rect) {
        let window = self.window.get();
        match action {
            Action::User
            | Action::Display
            | Action::Quality
            | Action::Credentials
            | Action::Power => {
                self.release_input();
                let entries = toolbar::menu(action, &self.toolbar.borrow().state);
                if let Some(command) = unsafe { self.show_menu(window, &entries, rect) } {
                    self.toolbar_command(command);
                }
            }
            Action::Files => {
                self.release_input();
                self.control.set_input_enabled(false);
                let entries = toolbar::menu(action, &self.toolbar.borrow().state);
                if let Some(command) = unsafe { self.show_menu(window, &entries, rect) } {
                    self.toolbar_command(command);
                }
                self.control.set_input_enabled(true);
            }
            Action::Toolbox => {
                self.release_input();
                self.control.set_input_enabled(false);
                let offered = self.control.toolbox().offer();
                let entries = toolbar::toolbox_menu(&offered);
                if let Some(command) = unsafe { self.show_menu(window, &entries, rect) } {
                    self.toolbar_command(command);
                }
                self.control.set_input_enabled(true);
            }
            Action::Recording => self.control.toggle_recording(),
            Action::Annotate => self.toggle_annotating(),
            Action::SecureAttention => {
                self.release_input();
                self.control.send_secure_attention();
            }
            Action::TypeClipboard => {
                self.release_input();
                self.control.type_clipboard(self.active_display_id());
            }
            Action::Chat => {
                self.release_input();
                self.control.set_input_enabled(false);
                if let Some(chat) = self.chat_popup.get() {
                    chat.toggle();
                }
            }
            Action::Diagnostics => self.toggle_debug(),
            Action::Settings => self.show_settings(),
            Action::Minimize => {
                let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
            }
            Action::Maximize => {
                let command = if unsafe { IsZoomed(window) }.as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                let _ = unsafe { ShowWindow(window, command) };
            }
            Action::Close => {
                // Posted: closing destroys this toolbar, whose click is
                // still being handled.
                let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
        self.refresh_toolbar();
    }

    /// Asks before restarting the remote computer. Input stays off while the
    /// box is up, like the disconnect confirmation.
    fn confirm_restart(&self, safe_mode: bool) -> bool {
        self.control.set_input_enabled(false);
        let (title, detail, _) = toolbar::restart_confirmation(safe_mode);
        let text = HSTRING::from(format!("{title}\n\n{detail}"));
        let confirmed = unsafe {
            MessageBoxW(
                Some(self.window.get()),
                &text,
                w!("Restart remote computer"),
                MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        } == IDOK;
        self.control.set_input_enabled(true);
        confirmed
    }

    /// A choice from a toolbar menu.
    fn toolbar_command(&self, command: Command) {
        match command {
            Command::Session(index) => {
                self.release_input();
                self.select_user(index);
            }
            Command::Display(index) => self.select_display(index),
            Command::Quality(preset) => self.set_quality(preset),
            Command::Chroma(mode) => self.set_chroma(mode),
            Command::PromptCredentials => {
                self.release_input();
                self.control.send(SessionMessage::PromptForCredentials);
            }
            Command::AutofillCredentials => {
                self.release_input();
                self.control.send(SessionMessage::AutofillCredentials);
            }
            Command::ForgetCredentials => self.control.send(SessionMessage::ForgetCredentials),
            Command::SendFiles => self.control.files().pick(),
            Command::ReceiveFiles => self.control.files().request_peer_pick(),
            Command::Restart { safe_mode } => {
                if self.confirm_restart(safe_mode) {
                    self.control.restart(safe_mode);
                }
            }
            Command::RunScript { index, run_as } => {
                self.control.toolbox().run_script(index, run_as)
            }
            Command::SendToolboxFile(index) => {
                // The background desktop shows Public Documents as Documents.
                let background = self.active_display.borrow().session
                    == meshrmm_protocol::DesktopSession::Background;
                self.control.toolbox().send_file(index, background);
            }
            Command::RefreshToolbox => self.control.toolbox().refresh(),
        }
        self.refresh_toolbar();
    }

    /// Places the toolbar, the video and the popups for the window's size.
    pub(super) fn layout_toolbar(&self, window: HWND) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(window, &mut rect) }.is_err() {
            return;
        }
        let controls = self.controls();
        let dpi = self.dpi.get();
        let width = rect.right.saturating_sub(rect.left);
        let place = |control: HWND, x: i32, y: i32, width: i32, height: i32| {
            let _ = unsafe { MoveWindow(control, x, y, width, height, true) };
        };
        place(controls.toolbar, 0, 0, width, toolbar_height(dpi));
        place(
            controls.video_window,
            0,
            toolbar_height(dpi),
            width,
            (rect.bottom - rect.top - toolbar_height(dpi)).max(0),
        );
        self.place_popups(window);
        // Also shows whether the window is maximized.
        self.update_toolbar(true);
    }

    /// Keeps the owned popups over the video when the window moves or resizes.
    pub(super) fn place_popups(&self, window: HWND) {
        let controls = self.controls();
        let mut origin = windows::Win32::Foundation::POINT {
            x: self.px(12),
            y: toolbar_height(self.dpi.get()) + self.px(12),
        };
        if unsafe { ClientToScreen(window, &mut origin) }.as_bool() {
            let _ = unsafe {
                SetWindowPos(
                    controls.debug_overlay,
                    None,
                    origin.x,
                    origin.y,
                    self.px(640),
                    self.px(300),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        let (width, height) = unsafe { client_size(window) }.unwrap_or_default();
        let video = self.video_rect_for(width, height);
        let (panel_width, panel_height) = (self.px(440), self.px(104));
        let mut center = windows::Win32::Foundation::POINT {
            x: video.left + (video.width - panel_width) / 2,
            y: video.top + (video.height - panel_height) / 2,
        };
        if unsafe { ClientToScreen(window, &mut center) }.as_bool() {
            let _ = unsafe {
                SetWindowPos(
                    controls.reconnect_panel,
                    None,
                    center.x,
                    center.y,
                    panel_width,
                    panel_height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            // Two lines of text above the button, in panel coordinates.
            let place = |control: HWND, x: i32, y: i32, width: i32, height: i32| {
                let _ = unsafe { MoveWindow(control, x, y, width, height, true) };
            };
            let margin = self.px(12);
            place(
                controls.reconnecting_label,
                margin,
                margin,
                panel_width - 2 * margin,
                self.px(40),
            );
            let (button_width, button_height) = (self.px(120), self.px(28));
            place(
                controls.retry_button,
                (panel_width - button_width) / 2,
                panel_height - button_height - self.px(14),
                button_width,
                button_height,
            );
        }
        if let Some(chat) = self.chat_popup.get() {
            chat.layout();
        }
    }

    fn select_display(&self, index: usize) {
        let active = self.active_display();
        let displays = self.displays();
        if let Some(display) = active.session_displays(&displays).get(index)
            && display.id != active.id
        {
            self.send(SessionMessage::SelectDisplay {
                display_id: display.id,
            });
        }
    }

    /// The selection changes once the device confirms the new stream.
    fn select_user(&self, index: usize) {
        let active = self.active_display();
        let displays = self.displays();
        let sessions = Display::sessions(&displays);
        if let Some(session) = sessions.get(index)
            && *session != active.session
            && let Some(display) = displays
                .iter()
                .find(|d| &d.session == session && d.primary)
                .or_else(|| displays.iter().find(|d| &d.session == session))
        {
            self.send(SessionMessage::SelectDisplay {
                display_id: display.id,
            });
        }
    }

    pub(super) fn select_next_display(&self) {
        let active = self.active_display();
        let all_displays = self.displays();
        let displays = active.session_displays(&all_displays);
        if displays.len() < 2 {
            return;
        }
        let current = displays.iter().position(|d| d.id == active.id).unwrap_or(0);
        self.send(SessionMessage::SelectDisplay {
            display_id: displays[(current + 1) % displays.len()].id,
        });
    }

    #[cfg(test)]
    pub(super) fn probe_toolbar_items(&self) -> Vec<(Item, RECT)> {
        let model = self.toolbar.borrow();
        model
            .items
            .iter()
            .cloned()
            .zip(
                model
                    .layout
                    .rects
                    .iter()
                    .map(|rect| self.device_rect(*rect)),
            )
            .collect()
    }

    /// Handles WM_COMMAND from a native control. Returns whether it did.
    pub(super) fn command(&self, window: HWND, wparam: WPARAM) -> bool {
        let control_id = wparam.0 & 0xffff;
        let notification = (wparam.0 >> 16) & 0xffff;
        // Forwarded by the reconnect panel.
        if control_id == RETRY_BUTTON_ID && notification == BN_CLICKED as usize {
            // Disabled until the session loop starts its next wait.
            let _ = unsafe { EnableWindow(self.controls().retry_button, false) };
            crate::reconnect::request_retry_now();
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        false
    }
}

/// The toolbar window's procedure. Its owner's context does the work.
unsafe extern "system" fn toolbar_proc(
    toolbar_window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let context = unsafe { GetParent(toolbar_window) }
        .ok()
        .and_then(|window| unsafe { window_context(window) });
    let Some(context) = context else {
        return unsafe { DefWindowProcW(toolbar_window, message, wparam, lparam) };
    };
    match message {
        WM_NCHITTEST => {
            let mut point = POINT {
                x: signed_low_word(lparam.0),
                y: signed_high_word(lparam.0),
            };
            let _ = unsafe { ScreenToClient(toolbar_window, &mut point) };
            let scale = context.scale_factor();
            let item =
                context.toolbar_hit((f64::from(point.x) / scale, f64::from(point.y) / scale));
            // The window moves by the empty space: its own hit test makes
            // that the caption.
            LRESULT(if item.is_some() {
                HTCLIENT as isize
            } else {
                HTTRANSPARENT as isize
            })
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            unsafe { context.paint_toolbar(toolbar_window) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            context.toolbar_mouse_move(toolbar_window, lparam);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            context.toolbar_mouse_leave(toolbar_window);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            context.toolbar_button_down(toolbar_window, lparam);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            context.toolbar_button_up(toolbar_window, lparam);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            if context.toolbar.borrow_mut().pressed.take().is_some() {
                let _ = unsafe { InvalidateRect(Some(toolbar_window), None, false) };
            }
            LRESULT(0)
        }
        // Clicks on the toolbar never reach the remote computer.
        WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN
        | WM_XBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL => LRESULT(0),
        WM_NOTIFY => {
            let header = unsafe { &*(lparam.0 as *const NMHDR) };
            if header.code == TTN_GETDISPINFOW {
                let info = unsafe { &mut *(lparam.0 as *mut NMTTDISPINFOW) };
                // The text lives in the model until the items change.
                if let Some(text) = context.toolbar.borrow().tooltips.get(header.idFrom) {
                    info.lpszText = PWSTR(text.as_ptr().cast_mut());
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(toolbar_window, message, wparam, lparam) },
    }
}

/// Creates the toolbar and the owned popups over the video. Returns the
/// window's controls with the new ones filled in.
pub(super) unsafe fn create_toolbar(
    window: HWND,
    instance: HINSTANCE,
    context: &WindowContext,
    font: HFONT,
) -> anyhow::Result<Controls> {
    let overlay = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!(""),
            WS_POPUP | WS_BORDER,
            0,
            0,
            1,
            1,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("debug overlay creation failed")?;
    let panel_class = w!("MeshRmmReconnectPanel");
    let panel_window_class = WNDCLASSW {
        lpfnWndProc: Some(messages::reconnect_panel_proc),
        hInstance: instance,
        lpszClassName: panel_class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        hbrBackground: HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0),
        ..Default::default()
    };
    unsafe { register_class(&panel_window_class) }
        .context("reconnect panel class registration failed")?;
    let reconnect_panel = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            panel_class,
            w!(""),
            WS_POPUP | WS_BORDER | WS_CLIPCHILDREN,
            0,
            0,
            1,
            1,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("reconnect panel creation failed")?;
    let reconnecting_label = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("Reconnecting to the remote computer…"),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | STATIC_CENTER),
            0,
            0,
            1,
            1,
            Some(reconnect_panel),
            None,
            Some(instance),
            None,
        )
    }
    .context("reconnecting label creation failed")?;
    let retry_button = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("BUTTON"),
            w!("Retry now"),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_DISABLED.0 | BS_PUSHBUTTON as u32),
            0,
            0,
            1,
            1,
            Some(reconnect_panel),
            Some(HMENU(RETRY_BUTTON_ID as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .context("retry button creation failed")?;
    let toolbar_class = w!("MeshRmmViewerToolbar");
    let toolbar_window_class = WNDCLASSW {
        lpfnWndProc: Some(toolbar_proc),
        hInstance: instance,
        lpszClassName: toolbar_class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        ..Default::default()
    };
    unsafe { register_class(&toolbar_window_class) }
        .context("viewer toolbar class registration failed")?;
    let toolbar = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            toolbar_class,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("viewer toolbar creation failed")?;
    unsafe {
        windows::Win32::UI::Shell::DragAcceptFiles(window, true);
    }
    let initialized = unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
        })
    };
    if !initialized.as_bool() {
        tracing::warn!("common controls are unavailable; the toolbar has no tooltips");
    }
    // Tooltips are optional: the toolbar works without them.
    let tooltip = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            w!(""),
            WINDOW_STYLE(WS_POPUP.0 | TTS_ALWAYSTIP | TTS_NOPREFIX),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(toolbar),
            None,
            Some(instance),
            None,
        )
    }
    .unwrap_or_default();
    if !tooltip.is_invalid() {
        unsafe {
            SendMessageW(
                tooltip,
                TTM_SETMAXTIPWIDTH,
                None,
                Some(LPARAM(scale(360, context.dpi.get()) as isize)),
            );
            set_font(tooltip, font);
        }
    }
    for control in [overlay, reconnecting_label, retry_button] {
        unsafe { set_font(control, font) };
    }
    Ok(Controls {
        debug_overlay: overlay,
        reconnect_panel,
        reconnecting_label,
        retry_button,
        toolbar,
        tooltip,
        ..context.controls()
    })
}

/// Registers a window class unless an earlier window already did.
unsafe fn register_class(class: &WNDCLASSW) -> windows::core::Result<()> {
    if unsafe { RegisterClassW(class) } == 0 {
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error);
        }
    }
    Ok(())
}

/// The toolbar font: Segoe UI Semibold at the toolbar's label size. The
/// caller owns the returned font.
pub(super) unsafe fn toolbar_font(dpi: u32) -> HFONT {
    let mut face = [0_u16; 32];
    for (slot, unit) in face.iter_mut().zip("Segoe UI".encode_utf16()) {
        *slot = unit;
    }
    let font = unsafe {
        CreateFontIndirectW(&windows::Win32::Graphics::Gdi::LOGFONTW {
            lfHeight: -((toolbar::FONT_SIZE * f64::from(dpi) / 96.0).round() as i32),
            lfWeight: 600,
            lfQuality: windows::Win32::Graphics::Gdi::CLEARTYPE_QUALITY,
            lfFaceName: face,
            ..Default::default()
        })
    };
    if font.is_invalid() {
        unsafe { message_font(dpi) }
    } else {
        font
    }
}

// Only monitor/ownership transitions reach this path, never individual mouse moves.
pub(in crate::platform::windows) unsafe fn set_agent_pointer_display(
    window: HWND,
    display_id: Option<meshrmm_protocol::DisplayId>,
) {
    if let Some(context) = unsafe { window_context(window) } {
        context.agent_pointer_display.set(display_id);
        context.refresh_toolbar();
    }
}
