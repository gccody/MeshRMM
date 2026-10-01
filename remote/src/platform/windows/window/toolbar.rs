//! The toolbar above the video, drawn by the viewer: see [`crate::toolbar`]
//! for its items, layout and icons. A child window paints them, with GDI+
//! for the shapes and GDI for ClearType text, tracks the mouse, shows their
//! menus and tooltips, and lets the empty space between them move the
//! window. The borderless window's caption buttons are toolbar items too.
//! This module also creates the owned popups over the video.

use std::sync::OnceLock;

use super::*;
use crate::toolbar::{self, Action, Command, Item, Layout, MenuEntry, Paint, Rect, Segment};
use windows::Win32::Foundation::{COLORREF, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DrawTextW, EndPaint, FillRect, GetDC,
    GetTextExtentPoint32W, HDC, InvalidateRect, PAINTSTRUCT, ReleaseDC, SRCCOPY, SelectObject,
    SetBkMode, TRANSPARENT,
};
use windows::Win32::Graphics::GdiPlus::{
    DashCapRound, FillModeWinding, GdipAddPathBezier, GdipAddPathLine, GdipClosePathFigure,
    GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush,
    GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDrawPath, GdipFillPath,
    GdipSetPenLineCap197819, GdipSetPenLineJoin, GdipSetPixelOffsetMode, GdipSetSmoothingMode,
    GdipStartPathFigure, GdiplusStartup, GdiplusStartupInput, GpGraphics, GpPath, LineCapRound,
    LineJoinRound, PixelOffsetModeHalf, SmoothingModeAntiAlias, UnitPixel,
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
            file_status: self.control.files().status(),
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

    /// Paints the toolbar into a memory bitmap, then onto the window.
    unsafe fn paint_toolbar(&self, toolbar_window: HWND) {
        let mut paint = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(toolbar_window, &mut paint) };
        let (width, height) = unsafe { client_size(toolbar_window) }.unwrap_or_default();
        let (width, height) = (width as i32, height as i32);
        if width > 0 && height > 0 {
            unsafe {
                let memory = CreateCompatibleDC(Some(dc));
                let bitmap = CreateCompatibleBitmap(dc, width, height);
                let old_bitmap = SelectObject(memory, HGDIOBJ(bitmap.0));
                self.draw_toolbar(memory, width);
                let _ = BitBlt(dc, 0, 0, width, height, Some(memory), 0, 0, SRCCOPY);
                SelectObject(memory, old_bitmap);
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(memory);
            }
        }
        let _ = unsafe { EndPaint(toolbar_window, &paint) };
    }

    unsafe fn draw_toolbar(&self, dc: HDC, width: i32) {
        let scale = self.scale_factor();
        let height = toolbar_height(self.dpi.get());
        unsafe {
            fill_rect(
                dc,
                RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                },
                toolbar::BACKGROUND,
            );
            let border = (scale.round() as i32).max(1);
            fill_rect(
                dc,
                RECT {
                    left: 0,
                    top: height - border,
                    right: width,
                    bottom: height,
                },
                toolbar::BORDER,
            );
        }
        let model = self.toolbar.borrow();
        for x in &model.layout.separators {
            let left = ((x - 0.5) * scale).round() as i32;
            let line = RECT {
                left,
                top: (12.0 * scale).round() as i32,
                right: left + (scale.round() as i32).max(1),
                bottom: (28.0 * scale).round() as i32,
            };
            unsafe { fill_rect(dc, line, toolbar::SEPARATOR) };
        }
        let old_font = unsafe { SelectObject(dc, HGDIOBJ(self.toolbar_font.get().0)) };
        let scaled = |rect: Rect| {
            Rect::new(
                rect.x * scale,
                rect.y * scale,
                rect.width * scale,
                rect.height * scale,
            )
        };
        let mut labels = Vec::new();
        let graphics = unsafe { Graphics::new(dc) };
        for (index, (item, rect)) in model.items.iter().zip(&model.layout.rects).enumerate() {
            let style = toolbar::style(
                item,
                model.hovered == Some(index),
                model.pressed == Some(index),
            );
            let text = model.layout.labels[index]
                .as_deref()
                .map(|label| (label, self.measure(dc, label)));
            let parts = toolbar::parts(item, *rect, text.map_or(0.0, |(_, width)| width));
            if let (Some((label, _)), Some(area)) = (text, parts.label) {
                labels.push((label.to_owned(), area, style.label));
            }
            let Some(graphics) = graphics.as_ref() else {
                continue;
            };
            if let Some(background) = style.background {
                graphics.paint(
                    &toolbar::rounded_rect(scaled(*rect), style.radius * scale),
                    Paint::Fill,
                    background,
                );
            }
            for shape in toolbar::icon(item.icon) {
                let (segments, paint) = toolbar::place(&shape, scaled(parts.icon));
                graphics.paint(&segments, paint, style.foreground);
            }
            if let Some(chevron) = parts.chevron {
                for shape in toolbar::icon(toolbar::Icon::Chevron) {
                    let (segments, paint) = toolbar::place(&shape, scaled(chevron));
                    graphics.paint(&segments, paint, style.foreground);
                }
            }
            if let (Some((x, y)), Some(color)) = (parts.badge, style.badge) {
                let (x, y) = (x * scale, y * scale);
                let ring = (style.badge_radius + 1.5) * scale;
                graphics.paint(&toolbar::circle(x, y, ring), Paint::Fill, style.badge_ring);
                graphics.paint(
                    &toolbar::circle(x, y, style.badge_radius * scale),
                    Paint::Fill,
                    color,
                );
            }
        }
        // GDI draws on the bitmap once GDI+ has finished with it.
        drop(graphics);
        unsafe { SetBkMode(dc, TRANSPARENT) };
        for (label, area, color) in labels {
            let mut text: Vec<u16> = label.encode_utf16().collect();
            let mut area = self.device_rect(area);
            // Rounding must not clip the last glyph.
            area.right += 2;
            unsafe {
                SetTextColor(dc, colorref(color));
                DrawTextW(
                    dc,
                    &mut text,
                    &mut area,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
        }
        unsafe { SelectObject(dc, old_font) };
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

    /// Shows `entries` under the item at `rect` and returns the chosen
    /// command.
    unsafe fn show_menu(&self, window: HWND, entries: &[MenuEntry], rect: Rect) -> Option<Command> {
        let menu = unsafe { CreatePopupMenu() }.ok()?;
        for (index, entry) in entries.iter().enumerate() {
            unsafe {
                let _ = match entry {
                    MenuEntry::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
                    MenuEntry::Item {
                        label,
                        checked,
                        enabled,
                        command,
                    } => {
                        let mut flags = MF_STRING;
                        if *checked {
                            flags |= MF_CHECKED;
                        }
                        if !*enabled || command.is_none() {
                            flags |= MF_GRAYED;
                        }
                        AppendMenuW(menu, flags, index + 1, &HSTRING::from(label))
                    }
                };
            }
        }
        let anchor = self.device_rect(rect);
        let mut point = POINT {
            x: anchor.left,
            y: anchor.bottom + self.px(2),
        };
        let _ = unsafe { ClientToScreen(self.controls().toolbar, &mut point) };
        let chosen = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                point.x,
                point.y,
                None,
                window,
                None,
            )
        }
        .0;
        let _ = unsafe { DestroyMenu(menu) };
        let index = usize::try_from(chosen).ok()?.checked_sub(1)?;
        toolbar::commands(entries).get(index).copied().flatten()
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

fn colorref(color: toolbar::Color) -> COLORREF {
    let toolbar::Color(red, green, blue) = color;
    COLORREF(u32::from(red) | u32::from(green) << 8 | u32::from(blue) << 16)
}

fn argb(color: toolbar::Color) -> u32 {
    let toolbar::Color(red, green, blue) = color;
    0xff00_0000 | u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
}

unsafe fn fill_rect(dc: HDC, rect: RECT, color: toolbar::Color) {
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

/// Starts GDI+ for the process once. It is never shut down: the viewer's
/// windows use it until the process exits.
fn gdiplus_started() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token = 0;
        let status = unsafe { GdiplusStartup(&mut token, &input, ptr::null_mut()) };
        if status.0 != 0 {
            tracing::warn!(
                status = status.0,
                "GDI+ is unavailable; toolbar icons are not drawn"
            );
        }
        status.0 == 0
    })
}

/// Anti-aliased GDI+ drawing on a device context.
struct Graphics(*mut GpGraphics);

impl Graphics {
    unsafe fn new(dc: HDC) -> Option<Self> {
        if !gdiplus_started() {
            return None;
        }
        let mut graphics = ptr::null_mut();
        if unsafe { GdipCreateFromHDC(dc, &mut graphics) }.0 != 0 || graphics.is_null() {
            return None;
        }
        unsafe {
            GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
            GdipSetPixelOffsetMode(graphics, PixelOffsetModeHalf);
        }
        Some(Self(graphics))
    }

    fn paint(&self, segments: &[Segment], paint: Paint, color: toolbar::Color) {
        unsafe {
            let path = path(segments);
            if path.is_null() {
                return;
            }
            match paint {
                Paint::Fill => {
                    let mut brush = ptr::null_mut();
                    if GdipCreateSolidFill(argb(color), &mut brush).0 == 0 {
                        GdipFillPath(self.0, brush.cast(), path);
                        GdipDeleteBrush(brush.cast());
                    }
                }
                Paint::Stroke(width) => {
                    let mut pen = ptr::null_mut();
                    if GdipCreatePen1(argb(color), width as f32, UnitPixel, &mut pen).0 == 0 {
                        GdipSetPenLineCap197819(pen, LineCapRound, LineCapRound, DashCapRound);
                        GdipSetPenLineJoin(pen, LineJoinRound);
                        GdipDrawPath(self.0, pen, path);
                        GdipDeletePen(pen);
                    }
                }
            }
            GdipDeletePath(path);
        }
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.0) };
    }
}

/// A GDI+ path of `segments`, which the caller deletes.
unsafe fn path(segments: &[Segment]) -> *mut GpPath {
    let mut path = ptr::null_mut();
    if unsafe { GdipCreatePath(FillModeWinding, &mut path) }.0 != 0 {
        return ptr::null_mut();
    }
    let mut current = (0.0_f32, 0.0_f32);
    for segment in segments {
        unsafe {
            match *segment {
                Segment::Move(x, y) => {
                    GdipStartPathFigure(path);
                    current = (x as f32, y as f32);
                }
                Segment::Line(x, y) => {
                    let next = (x as f32, y as f32);
                    GdipAddPathLine(path, current.0, current.1, next.0, next.1);
                    current = next;
                }
                Segment::Cubic(ax, ay, bx, by, x, y) => {
                    let next = (x as f32, y as f32);
                    GdipAddPathBezier(
                        path, current.0, current.1, ax as f32, ay as f32, bx as f32, by as f32,
                        next.0, next.1,
                    );
                    current = next;
                }
                Segment::Close => {
                    GdipClosePathFigure(path);
                }
            }
        }
    }
    path
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
