//! The session window: the remote desktop under the viewer's toolbar. It
//! sends the technician's keyboard and mouse to the device, and shows the
//! menus, the chat, the reconnect notice and the questions the session asks.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use meshrmm_protocol::{
    CursorShape, DesktopSession, Display, DisplayId, FileDestination, HeadlessResolution,
    PointerButton, RemoteInput, SessionCloseAction, SessionMessage, VideoFormat,
};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{ResizeDirection, Window, WindowAttributes};

use super::app::{SessionCommand, SessionRequest};
use super::gpu::{Canvas, Gpu};
use super::presenter::Shared;
use super::style::{HEADING, WEAK_TEXT};
use super::video::VideoRenderer;
use super::{keymap, menu, toolbar as bar};
use crate::annotation::Annotator;
use crate::debug::DebugInfo;
use crate::input::HeldInput;
use crate::platform::ControlSink;
use crate::reconnect::ReconnectStatus;
use crate::shortcuts::{ShortcutKey, ViewerShortcut};
use crate::toolbar::{self, Action, Command};
use crate::video_layout::{self, VideoRect};
use crate::wheel::WheelNormalizer;

/// The smallest session window, in points: the toolbar's items fit.
const MINIMUM_SIZE: (f64, f64) = (760.0, 420.0);
/// How near a window edge the mouse resizes the window, in points.
const RESIZE_MARGIN: f64 = 4.0;
/// How often the elapsed times and the diagnostics are redrawn.
const TICK: Duration = Duration::from_millis(250);
/// How often the window asks the desktop whether it is minimised, which X11
/// does not announce.
const VISIBILITY_CHECK: Duration = Duration::from_millis(500);
const CHAT_SIZE: egui::Vec2 = egui::vec2(340.0, 360.0);

/// The window's name in the desktop's lists of windows, with the viewer's
/// shortcut keys.
pub(super) fn window_title(display: &Display) -> String {
    let next_display = crate::preferences::shortcut_key(ViewerShortcut::NextDisplay);
    let hint = crate::shortcuts::hint(
        (next_display != ShortcutKey::Off).then(|| next_display.label()),
        crate::preferences::shortcut_key(ViewerShortcut::Diagnostics),
    );
    let mut title = format!("MeshRMM Remote Desktop — {}", display.name);
    if !hint.is_empty() {
        title.push_str(" — ");
        title.push_str(&hint);
    }
    title
}

/// A window that shows a stream of `format` at its size, or as much of the
/// monitor as it may take.
pub(super) fn attributes(
    event_loop: &ActiveEventLoop,
    format: VideoFormat,
    display: &Display,
) -> WindowAttributes {
    let monitor = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next());
    let scale = monitor
        .as_ref()
        .map_or(1.0, |monitor| monitor.scale_factor());
    let toolbar = (toolbar::HEIGHT * scale).round() as i32;
    let (maximum_width, maximum_height) = monitor
        .map(|monitor| monitor.size())
        .filter(|size| size.width > 0 && size.height > 0)
        .map_or((1280, 720), |size| {
            (
                (f64::from(size.width) * 0.85) as i32,
                (f64::from(size.height) * 0.85) as i32 - toolbar,
            )
        });
    let (width, height) = video_layout::fit_within(
        format.width,
        format.height,
        maximum_width,
        maximum_height.max(1),
    );
    let minimum_width = (MINIMUM_SIZE.0 * scale).round() as i32;
    super::app::window_attributes(&window_title(display))
        .with_inner_size(PhysicalSize::new(
            width.max(minimum_width) as u32,
            (height + toolbar) as u32,
        ))
        .with_min_inner_size(LogicalSize::new(MINIMUM_SIZE.0, MINIMUM_SIZE.1))
        // The toolbar has the window's buttons and moves the window.
        .with_decorations(false)
}

/// A question the window asks, over the video.
enum Prompt {
    Disconnect,
    Restart { safe_mode: bool },
    Alert { title: String, message: String },
}

struct Diagnostics {
    text: String,
    refreshed: Instant,
}

/// How often the picture on screen changes, for the diagnostics.
struct Statistics {
    since: Instant,
    presented_since: u32,
    presented: u64,
}

/// The window's state, apart from what draws it.
struct View {
    window: Arc<Window>,
    egui: egui::Context,
    shared: Arc<Shared>,
    control: ControlSink,
    debug: DebugInfo,
    active_display: Display,
    displays: Vec<Display>,
    /// The size of the picture on screen, or of the stream until one is.
    video_size: (u32, u32),
    agent_pointer_display: Option<DisplayId>,
    cursor_shape: CursorShape,
    held: HeldInput,
    annotator: Annotator,
    wheel: WheelNormalizer,
    modifiers: ModifiersState,
    /// Where the mouse is, in the window's physical pixels.
    pointer: Option<PhysicalPosition<f64>>,
    /// Buttons pressed on the controls, whose release is theirs too.
    control_buttons: Vec<MouseButton>,
    /// Where the controls are, in points, as last drawn.
    control_rects: Vec<egui::Rect>,
    focused: bool,
    occluded: bool,
    visibility_checked: Instant,
    reconnect: Option<ReconnectStatus>,
    diagnostics: Option<Diagnostics>,
    prompt: Option<Prompt>,
    alerts: VecDeque<(String, String)>,
    chat_open: bool,
    chat_draft: String,
    /// The chat's text field takes the keyboard when the chat opens.
    chat_focus: bool,
    /// The toolbox listing its open menu offers.
    toolbox_offer: Option<crate::toolbox::Snapshot>,
    /// Finished toolbox runs, each to show in a window of its own.
    outputs: Vec<(String, String)>,
    dropped: Vec<PathBuf>,
    /// The toolbar asked the desktop to move the window.
    moving: bool,
    shows_video: crate::video_pause::VideoWindow,
    statistics: Statistics,
    /// The picture uploaded last has not reached the screen yet.
    picture_pending: bool,
    closed: bool,
}

pub(super) struct SessionWindow {
    canvas: Canvas,
    video: VideoRenderer,
    view: View,
}

fn pointer_button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Left),
        MouseButton::Right => Some(PointerButton::Right),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::Back => Some(PointerButton::Back),
        MouseButton::Forward => Some(PointerButton::Forward),
        MouseButton::Other(_) => None,
    }
}

fn cursor_icon(shape: CursorShape) -> egui::CursorIcon {
    match shape {
        CursorShape::Default | CursorShape::UpArrow | CursorShape::Pin | CursorShape::Person => {
            egui::CursorIcon::Default
        }
        CursorShape::Text => egui::CursorIcon::Text,
        CursorShape::Wait => egui::CursorIcon::Wait,
        CursorShape::Crosshair => egui::CursorIcon::Crosshair,
        CursorShape::ResizeNorthWestSouthEast => egui::CursorIcon::ResizeNwSe,
        CursorShape::ResizeNorthEastSouthWest => egui::CursorIcon::ResizeNeSw,
        CursorShape::ResizeWestEast => egui::CursorIcon::ResizeHorizontal,
        CursorShape::ResizeNorthSouth => egui::CursorIcon::ResizeVertical,
        CursorShape::Move => egui::CursorIcon::Move,
        CursorShape::NotAllowed => egui::CursorIcon::NotAllowed,
        CursorShape::Pointer => egui::CursorIcon::PointingHand,
        CursorShape::Progress => egui::CursorIcon::Progress,
        CursorShape::Help => egui::CursorIcon::Help,
    }
}

fn resize_cursor(direction: ResizeDirection) -> egui::CursorIcon {
    match direction {
        ResizeDirection::East => egui::CursorIcon::ResizeEast,
        ResizeDirection::North => egui::CursorIcon::ResizeNorth,
        ResizeDirection::NorthEast => egui::CursorIcon::ResizeNorthEast,
        ResizeDirection::NorthWest => egui::CursorIcon::ResizeNorthWest,
        ResizeDirection::South => egui::CursorIcon::ResizeSouth,
        ResizeDirection::SouthEast => egui::CursorIcon::ResizeSouthEast,
        ResizeDirection::SouthWest => egui::CursorIcon::ResizeSouthWest,
        ResizeDirection::West => egui::CursorIcon::ResizeWest,
    }
}

/// The edge or corner of a window of `size` that a mouse at `pointer` is on,
/// within `margin` of it, all in physical pixels.
fn resize_direction(
    pointer: PhysicalPosition<f64>,
    size: PhysicalSize<u32>,
    margin: f64,
) -> Option<ResizeDirection> {
    let left = pointer.x < margin;
    let right = pointer.x >= f64::from(size.width) - margin;
    let top = pointer.y < margin;
    let bottom = pointer.y >= f64::from(size.height) - margin;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (_, true, true, _) => Some(ResizeDirection::NorthEast),
        (true, _, _, true) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (true, ..) => Some(ResizeDirection::West),
        (_, true, ..) => Some(ResizeDirection::East),
        (_, _, true, _) => Some(ResizeDirection::North),
        (_, _, _, true) => Some(ResizeDirection::South),
        _ => None,
    }
}

/// The session-close choice as the session controls name it.
fn close_action_label(action: SessionCloseAction) -> &'static str {
    match action {
        SessionCloseAction::NoAction => "Leave signed in",
        SessionCloseAction::Lock => "Lock",
        SessionCloseAction::Logout => "Sign out",
    }
}

/// A shortcut's key for the session controls' item title.
fn shortcut_title(key: ShortcutKey) -> &'static str {
    match key {
        ShortcutKey::Off => "Off",
        key => key.label(),
    }
}

impl View {
    fn send(&self, message: SessionMessage) {
        self.control.send(message);
    }

    fn scale(&self) -> f64 {
        self.window.scale_factor()
    }

    /// The toolbar's height in physical pixels.
    fn toolbar_pixels(&self) -> i32 {
        (toolbar::HEIGHT * self.scale()).round() as i32
    }

    /// Where the video is drawn in a window of `size` physical pixels.
    fn video_rect(&self, size: PhysicalSize<u32>) -> VideoRect {
        let toolbar = self.toolbar_pixels();
        video_layout::letterbox(
            VideoRect {
                left: 0,
                top: toolbar,
                width: size.width as i32,
                height: (size.height as i32 - toolbar).max(1),
            },
            self.video_size.0,
            self.video_size.1,
        )
    }

    /// The mouse's place on the video; `None` off it, unless `clamped`
    /// moves it to the nearest edge, as while a drag leaves the video.
    fn position(&self, clamped: bool) -> Option<(u16, u16)> {
        let pointer = self.pointer?;
        let video = self.video_rect(self.window.inner_size());
        let (x, y) = (pointer.x.floor() as i32, pointer.y.floor() as i32);
        if clamped {
            Some(video_layout::normalize_clamped(video, x, y))
        } else {
            video_layout::normalize(video, x, y)
        }
    }

    /// Whether a menu, the chat or a question has the keyboard and mouse.
    fn controls_open(&self) -> bool {
        self.prompt.is_some() || self.chat_open || egui::Popup::is_any_open(&self.egui)
    }

    /// Whether the mouse is over the toolbar or a panel, or a menu or a
    /// question is open, so that what it does is theirs.
    fn over_controls(&self) -> bool {
        if self.prompt.is_some() || egui::Popup::is_any_open(&self.egui) {
            return true;
        }
        let Some(pointer) = self.pointer else {
            return false;
        };
        let scale = self.scale() as f32;
        let point = egui::pos2(pointer.x as f32 / scale, pointer.y as f32 / scale);
        self.control_rects.iter().any(|rect| rect.contains(point))
    }

    /// The edge the mouse would resize the window by, while no button of a
    /// drag on the video is down.
    fn resize_edge(&self) -> Option<ResizeDirection> {
        if self.window.is_maximized()
            || self.window.fullscreen().is_some()
            || self.held.buttons_held()
        {
            return None;
        }
        resize_direction(
            self.pointer?,
            self.window.inner_size(),
            RESIZE_MARGIN * self.scale(),
        )
    }

    /// Releases every key and button held on the device. The viewer does
    /// this by itself, on losing focus among others, so it is not activity.
    fn release_input(&mut self) {
        for input in self.held.release_all(self.active_display.id) {
            self.control.send_automatic(SessionMessage::Input(input));
        }
    }

    fn disable_input(&mut self) {
        self.annotator.finish();
        self.release_input();
        self.control.set_input_enabled(false);
    }

    /// Lets input through again, unless a question is still open or the
    /// window is in the background.
    fn enable_input(&self) {
        if self.focused && self.prompt.is_none() {
            self.control.set_input_enabled(true);
        }
    }

    fn focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.enable_input();
        } else {
            self.modifiers = ModifiersState::empty();
            self.disable_input();
        }
    }

    /// Tells the session whether the window shows the video: not while it
    /// is minimised or the desktop has hidden it.
    fn update_shows_video(&mut self) {
        self.visibility_checked = Instant::now();
        let hidden = self.occluded || self.window.is_minimized().unwrap_or(false);
        self.shows_video.set_shown(!hidden);
    }

    fn key(&mut self, event: &KeyEvent) {
        let Some((scan_code, extended)) =
            event.physical_key.to_scancode().and_then(keymap::scan_code)
        else {
            return;
        };
        let pressed = event.state == ElementState::Pressed;
        if pressed {
            // Shortcut keys and pasting files send no key to the device,
            // but a person pressed them all.
            self.control.note_activity();
        }
        let shortcut = crate::shortcuts::linux_shortcut(
            scan_code,
            extended,
            self.control.shortcut_key(ViewerShortcut::Diagnostics),
            self.control.shortcut_key(ViewerShortcut::NextDisplay),
        );
        if let Some(shortcut) = shortcut {
            if pressed && !event.repeat {
                match shortcut {
                    ViewerShortcut::Diagnostics => self.toggle_diagnostics(),
                    ViewerShortcut::NextDisplay => self.select_next_display(),
                }
            }
            return;
        }
        if pressed
            && !event.repeat
            && (scan_code, extended) == keymap::V
            && self.modifiers.control_key()
            && self.control.files().paste_files(self.active_display.id)
        {
            // The device does not get the V, and Ctrl comes up with it.
            self.release_input();
            return;
        }
        let input = self.held.key_event(
            self.active_display.id,
            scan_code,
            extended,
            pressed,
            event.repeat,
        );
        if let Some(input) = input {
            self.send(SessionMessage::Input(input));
        }
    }

    fn pointer_moved(&mut self, position: PhysicalPosition<f64>) {
        self.pointer = Some(position);
        let dragging = self.held.buttons_held();
        if !dragging && self.over_controls() {
            return;
        }
        let display_id = self.active_display.id;
        if self.annotator.enabled() {
            // The mouse draws rather than moves the device's pointer.
            let position = self.position(false);
            if let Some(message) = self.annotator.extend(display_id, position) {
                self.send(message);
            }
            return;
        }
        if let Some((x, y)) = self.position(dragging) {
            self.send(SessionMessage::Input(RemoteInput::PointerMove {
                display_id,
                x,
                y,
            }));
        }
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        if pressed {
            if self.over_controls() {
                self.control_buttons.push(button);
                return;
            }
        } else if let Some(index) = self.control_buttons.iter().position(|held| *held == button) {
            self.control_buttons.remove(index);
            return;
        }
        let Some(button) = pointer_button(button) else {
            return;
        };
        let display_id = self.active_display.id;
        let position = self.position(false);
        if self.annotator.enabled() {
            let message = match (button, pressed) {
                (PointerButton::Left, true) => self.annotator.start(display_id, position),
                (PointerButton::Left, false) => {
                    self.annotator.finish();
                    None
                }
                (PointerButton::Right, true) if position.is_some() => self.annotator.clear(),
                _ => None,
            };
            if let Some(message) = message {
                self.send(message);
            }
            return;
        }
        if let Some(input) = self.held.button(display_id, position, button, pressed) {
            self.send(SessionMessage::Input(input));
        }
    }

    fn wheel(&mut self, delta: MouseScrollDelta) {
        let Some((x, y)) = self.position(false) else {
            return;
        };
        // The window reports how far the content moves: right is positive,
        // where the protocol's positive scrolls right, moving it left.
        let (horizontal, vertical) = match delta {
            MouseScrollDelta::LineDelta(columns, lines) => {
                self.wheel
                    .normalize(-f64::from(columns), f64::from(lines), false)
            }
            MouseScrollDelta::PixelDelta(pixels) => {
                let scale = self.scale();
                self.wheel
                    .normalize(-pixels.x / scale, pixels.y / scale, true)
            }
        };
        if horizontal == 0 && vertical == 0 {
            return;
        }
        self.send(SessionMessage::Input(RemoteInput::WheelAt {
            display_id: self.active_display.id,
            x,
            y,
            horizontal,
            vertical,
        }));
    }

    fn select_next_display(&mut self) {
        let displays = self.active_display.session_displays(&self.displays);
        if displays.len() < 2 {
            return;
        }
        let current = displays
            .iter()
            .position(|display| display.id == self.active_display.id)
            .unwrap_or(0);
        let display_id = displays[(current + 1) % displays.len()].id;
        self.send(SessionMessage::SelectDisplay { display_id });
    }

    fn toggle_diagnostics(&mut self) {
        self.diagnostics = match self.diagnostics {
            Some(_) => None,
            None => Some(Diagnostics {
                text: self.debug.render(),
                refreshed: Instant::now(),
            }),
        };
    }

    fn toggle_annotating(&mut self) {
        // Keys and buttons held on the device stay there otherwise.
        self.release_input();
        if let Some(message) = self.annotator.toggle() {
            self.send(message);
        }
    }

    fn set_chat_open(&mut self, open: bool) {
        let chat = self.control.chat();
        if open {
            self.disable_input();
            chat.set_visible(true);
            self.chat_open = chat.visible();
            self.chat_focus = self.chat_open;
        } else {
            chat.set_visible(false);
            self.chat_open = false;
        }
        if !self.chat_open {
            self.enable_input();
        }
    }

    fn alert(&mut self, title: impl Into<String>, message: String) {
        self.alerts.push_back((title.into(), message));
    }

    fn ask(&mut self, prompt: Prompt) {
        self.disable_input();
        // An alert waits for the question that takes its place.
        if let Some(Prompt::Alert { title, message }) = self.prompt.take() {
            self.alerts.push_front((title, message));
        }
        self.prompt = Some(prompt);
    }

    /// The window's close button, or the desktop's request to close it.
    fn request_close(&mut self) {
        if !self.control.disconnect_confirmation() {
            self.close();
        } else if !matches!(self.prompt, Some(Prompt::Disconnect)) {
            self.ask(Prompt::Disconnect);
        }
    }

    fn close(&mut self) {
        self.disable_input();
        self.closed = true;
        self.shared.closed.store(true, Ordering::Release);
        // Ends the session even while it is reconnecting and no transport
        // is watching this window.
        crate::shutdown::request("the viewer window was closed");
    }

    /// Shows what the session reported since the last paint.
    fn collect(&mut self) {
        if let Some(notice) = self.control.recording().take_notice() {
            self.alert("Session recording", notice);
        }
        if let Some(error) = self.control.take_maintenance_error() {
            self.alert("Maintenance control failed", error);
        }
        while let Some(event) = self.control.toolbox().take_event() {
            match event {
                crate::toolbox::Event::RunFinished(run) => {
                    self.outputs.push(crate::toolbox::run_report(&run));
                }
                crate::toolbox::Event::Failed { title, message } => self.alert(title, message),
            }
        }
        if self.chat_open && !self.control.chat().available() {
            self.set_chat_open(false);
        }
        if !self.dropped.is_empty() {
            let paths = std::mem::take(&mut self.dropped);
            tracing::info!(files = paths.len(), "files dropped on the viewer");
            self.control.note_activity();
            self.release_input();
            // The desktop does not say where on the video they landed.
            self.control.files().send(paths, FileDestination::Documents);
        }
        if self.prompt.is_none()
            && let Some((title, message)) = self.alerts.pop_front()
        {
            self.ask(Prompt::Alert { title, message });
        }
        if let Some(diagnostics) = &mut self.diagnostics
            && diagnostics.refreshed.elapsed() >= TICK
        {
            diagnostics.text = self.debug.render();
            diagnostics.refreshed = Instant::now();
        }
    }

    fn configure_display(&mut self, display: Display, displays: Vec<Display>) {
        if self.active_display.id != display.id {
            self.release_input();
            // A stroke belongs to the display it started on.
            self.annotator.finish();
        }
        if display.session == DesktopSession::Background {
            self.annotator.disable();
        }
        self.window.set_title(&window_title(&display));
        self.active_display = display;
        self.displays = displays;
    }

    /// What the toolbar shows now.
    fn toolbar_state(&self) -> toolbar::State {
        let active = &self.active_display;
        let sessions = Display::sessions(&self.displays);
        let visible = active.session_displays(&self.displays);
        let control = &self.control;
        let chat = control.chat();
        let toolbox = control.toolbox().snapshot();
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
                .and_then(|id| visible.iter().position(|display| display.id == id)),
            quality: control.quality_preset(),
            // The viewer decodes 4:2:0 only.
            chroma: None,
            credentials: control.credential_state(),
            input_blocked: control.technician_blocked(),
            typing: control.typing(),
            annotating: self.annotator.enabled(),
            annotation_available: active.session != DesktopSession::Background,
            chat_available: chat.available(),
            chat_unread: chat.unread(),
            power: control.power_state(),
            device: control.device_platform(),
            file_status: control.files().status(),
            toolbox_available: toolbox.available,
            toolbox_busy: toolbox.busy,
            toolbox_status: toolbox.status,
            recording: control.recording().active(),
            diagnostics: self.diagnostics.is_some(),
            settings_menu: true,
            caption: Some(self.window.is_maximized()),
        }
    }

    /// A click on the toolbar item for `action`.
    fn toolbar_action(&mut self, action: Action) {
        self.control.note_activity();
        match action {
            Action::User
            | Action::Display
            | Action::Quality
            | Action::Credentials
            | Action::Power
            | Action::Files
            | Action::Settings => self.release_input(),
            Action::Toolbox => {
                self.release_input();
                self.toolbox_offer = Some(self.control.toolbox().offer());
            }
            Action::Recording => self.control.toggle_recording(),
            Action::Annotate => self.toggle_annotating(),
            Action::SecureAttention => {
                self.release_input();
                self.control.send_secure_attention();
            }
            Action::TypeClipboard => {
                self.release_input();
                self.control.type_clipboard(self.active_display.id);
            }
            Action::Chat => self.set_chat_open(!self.chat_open),
            Action::Diagnostics => self.toggle_diagnostics(),
            Action::Minimize => self.window.set_minimized(true),
            Action::Maximize => self.window.set_maximized(!self.window.is_maximized()),
            Action::Close => self.request_close(),
        }
    }

    /// A choice from a toolbar menu.
    fn toolbar_command(&mut self, command: Command) {
        // A menu can stay open for a while before the choice.
        self.control.note_activity();
        match command {
            Command::Session(index) => {
                let target = Display::sessions(&self.displays)
                    .get(index)
                    .filter(|session| **session != self.active_display.session)
                    .and_then(|session| {
                        self.displays
                            .iter()
                            .find(|display| &display.session == session && display.primary)
                            .or_else(|| {
                                self.displays
                                    .iter()
                                    .find(|display| &display.session == session)
                            })
                            .map(|display| display.id)
                    });
                // The selection changes once the agent confirms its stream.
                if let Some(display_id) = target {
                    self.release_input();
                    self.send(SessionMessage::SelectDisplay { display_id });
                }
            }
            Command::Display(index) => {
                let target = self
                    .active_display
                    .session_displays(&self.displays)
                    .get(index)
                    .map(|display| display.id)
                    .filter(|id| *id != self.active_display.id);
                if let Some(display_id) = target {
                    self.send(SessionMessage::SelectDisplay { display_id });
                }
            }
            Command::Quality(preset) => self.send(SessionMessage::SetQuality { preset }),
            // The viewer decodes 4:2:0 only and does not offer the choice.
            Command::Chroma(_) => {}
            Command::PromptCredentials => {
                self.release_input();
                self.send(SessionMessage::PromptForCredentials);
            }
            Command::AutofillCredentials => {
                self.release_input();
                self.send(SessionMessage::AutofillCredentials);
            }
            Command::ForgetCredentials => self.send(SessionMessage::ForgetCredentials),
            Command::SendFiles => self.control.files().pick(),
            Command::ReceiveFiles => self.control.files().request_peer_pick(),
            Command::Restart { safe_mode } => self.ask(Prompt::Restart { safe_mode }),
            Command::RunScript { index, run_as } => {
                self.control.toolbox().run_script(index, run_as);
            }
            Command::SendToolboxFile(index) => {
                // The background desktop shows Public Documents as Documents.
                let background = self.active_display.session == DesktopSession::Background;
                self.control.toolbox().send_file(index, background);
            }
            Command::RefreshToolbox => self.control.toolbox().refresh(),
        }
    }

    /// The menu the toolbar item for `action` opened, under the item.
    fn menu(&mut self, action: Action, state: &toolbar::State, item: &egui::Response) {
        let popup = egui::Popup::menu(item).gap(5.0);
        let command = match action {
            Action::Settings => {
                popup.show(|ui| self.session_controls(ui));
                None
            }
            Action::Toolbox => {
                let offered = self
                    .toolbox_offer
                    .clone()
                    .unwrap_or_else(|| self.control.toolbox().snapshot());
                let entries = toolbar::toolbox_menu(&offered);
                popup
                    .show(|ui| menu::show(ui, &entries))
                    .and_then(|shown| shown.inner)
            }
            _ => {
                let entries = toolbar::menu(action, state);
                popup
                    .show(|ui| menu::show(ui, &entries))
                    .and_then(|shown| shown.inner)
            }
        };
        if let Some(command) = command {
            self.toolbar_command(command);
        }
    }

    /// The session controls, which the toolbar's settings item opens:
    /// labeled sections of checkmarked toggles, so each item reads the same
    /// whatever its state.
    fn session_controls(&mut self, ui: &mut egui::Ui) {
        let control = self.control.clone();
        let chosen = |ui: &mut egui::Ui, label: &str, checked: bool, enabled: bool| {
            let clicked = menu::row(ui, label, Some(checked), enabled).clicked();
            if clicked {
                control.note_activity();
            }
            clicked
        };

        menu::header(ui, "Session");
        if chosen(
            ui,
            if control.recording().active() {
                "Stop recording and save"
            } else {
                "Record video to Downloads"
            },
            false,
            true,
        ) {
            control.toggle_recording();
        }
        if chosen(ui, "Play remote audio", !control.audio_muted(), true) {
            control.toggle_audio();
        }
        let view_only = menu::row(ui, "View only", Some(control.technician_blocked()), true)
            .on_hover_text("Stop sending your keyboard and mouse to the remote computer.");
        if view_only.clicked() {
            control.note_activity();
            self.release_input();
            control.set_technician_blocked(!control.technician_blocked());
            self.enable_input();
        }
        if chosen(ui, "Sync clipboard", control.clipboard_sync(), true) {
            control.toggle_clipboard_sync();
        }
        let idle_minutes = control.idle_disconnect_minutes();
        let idle_title = format!(
            "Disconnect when idle: {}{}",
            crate::idle_disconnect::label(idle_minutes),
            if control.allow_idle_disconnect_override() {
                ""
            } else {
                " (company managed)"
            }
        );
        menu::submenu(
            ui,
            &idle_title,
            true,
            control.allow_idle_disconnect_override(),
            |ui| {
                for choice in crate::idle_disconnect::choices() {
                    let label = crate::idle_disconnect::label(choice);
                    if chosen(ui, &label, choice == idle_minutes, true) {
                        control.set_idle_disconnect_minutes(choice);
                    }
                }
            },
        );

        ui.separator();
        menu::header(ui, "Remote computer");
        let maintenance = control.maintenance_state();
        if chosen(
            ui,
            "Block user's keyboard and mouse",
            control.agent_blocked(),
            maintenance.available && !maintenance.blacked_out,
        ) {
            self.release_input();
            control.toggle_agent_input();
        }
        if chosen(
            ui,
            "Black out screens",
            maintenance.blacked_out,
            maintenance.available,
        ) {
            self.release_input();
            control.toggle_blackout();
        }
        if chosen(
            ui,
            if control.allow_idle_override() {
                "Prevent idle lock"
            } else {
                "Prevent idle lock (company managed)"
            },
            control.prevent_idle_lock(),
            control.allow_idle_override(),
        ) {
            control.toggle_prevent_idle_lock();
        }
        if chosen(
            ui,
            "Highlight the monitor I'm viewing",
            control.display_border_available() && control.display_border(),
            control.display_border_available(),
        ) {
            control.toggle_display_border();
        }
        if chosen(ui, "Hide wallpaper", control.wallpaper_hidden(), true) {
            control.toggle_wallpaper();
        }
        // Only a Windows Agent adds a virtual monitor.
        if control.windows_device() {
            let headless = crate::preferences::headless_resolution();
            menu::submenu(
                ui,
                &format!("Size without a monitor: {}", headless.label()),
                true,
                true,
                |ui| {
                    for choice in HeadlessResolution::PRESETS {
                        if chosen(ui, &choice.label(), choice == headless, true) {
                            control.set_headless_resolution(choice);
                        }
                    }
                },
            );
        }

        ui.separator();
        menu::header(ui, "This viewer");
        if chosen(ui, "Show remote cursor", control.show_remote_cursor(), true) {
            control.toggle_remote_cursor();
        }
        for (title, shortcut) in [
            ("Next display shortcut", ViewerShortcut::NextDisplay),
            ("Diagnostics shortcut", ViewerShortcut::Diagnostics),
        ] {
            let key = control.shortcut_key(shortcut);
            menu::submenu(
                ui,
                &format!("{title}: {}", shortcut_title(key)),
                true,
                true,
                |ui| {
                    for choice in ShortcutKey::ALL {
                        if chosen(ui, choice.label(), choice == key, true) {
                            control.set_shortcut_key(shortcut, choice);
                            self.window.set_title(&window_title(&self.active_display));
                        }
                    }
                },
            );
        }

        ui.separator();
        menu::header(ui, "When the session ends");
        if chosen(
            ui,
            "Ask before disconnecting",
            control.disconnect_confirmation(),
            true,
        ) {
            control.toggle_disconnect_confirmation();
        }
        if chosen(
            ui,
            if control.allow_clear_clipboard_override() {
                "Clear remote clipboard"
            } else {
                "Clear remote clipboard (company managed)"
            },
            control.clear_clipboard_on_close(),
            control.allow_clear_clipboard_override(),
        ) {
            control.toggle_clear_clipboard_on_close();
        }
        let close_action = control.session_close_action();
        menu::submenu(
            ui,
            &format!("Remote user: {}", close_action_label(close_action)),
            true,
            true,
            |ui| {
                for choice in SessionCloseAction::ALL {
                    if chosen(ui, close_action_label(choice), choice == close_action, true) {
                        control.set_session_close_action(choice);
                    }
                }
            },
        );
    }

    /// Everything the window draws over the video.
    fn ui(&mut self, ui: &mut egui::Ui) {
        let context = ui.ctx().clone();
        let screen = context.content_rect();
        self.control_rects.clear();
        self.control_rects.push(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(screen.width(), toolbar::HEIGHT as f32),
        ));

        // The mouse over the video looks as it does on the device.
        if let Some(edge) = self.resize_edge() {
            context.set_cursor_icon(resize_cursor(edge));
        } else if !self.over_controls() && self.position(false).is_some() {
            context.set_cursor_icon(cursor_icon(if self.annotator.enabled() {
                CursorShape::Crosshair
            } else {
                self.control.effective_cursor_shape(self.cursor_shape)
            }));
        }

        let state = self.toolbar_state();
        let shown = bar::show(ui, &state, screen.width(), &|item| {
            egui::Popup::is_id_open(&context, egui::Popup::default_response_id(item))
        });
        if shown
            .background
            .drag_started_by(egui::PointerButton::Primary)
        {
            self.moving = true;
        }
        if shown.background.double_clicked() {
            self.window.set_maximized(!self.window.is_maximized());
        }
        if shown.background.secondary_clicked()
            && let Some(pointer) = self.pointer
        {
            self.window.show_window_menu(pointer);
        }
        let mut chat_item = None;
        for (item, response) in &shown.items {
            if response.clicked() {
                self.toolbar_action(item.action);
            }
            if item.menu {
                self.menu(item.action, &state, response);
            }
            if item.action == Action::Chat {
                chat_item = Some(response.rect);
            }
        }

        self.diagnostics_overlay(&context);
        self.reconnect_panel(&context);
        self.chat_panel(&context, screen, chat_item);
        self.prompt_window(&context);
    }

    fn diagnostics_overlay(&self, context: &egui::Context) {
        let Some(diagnostics) = &self.diagnostics else {
            return;
        };
        egui::Area::new(egui::Id::unique("diagnostics"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::pos2(12.0, toolbar::HEIGHT as f32 + 12.0))
            // The mouse over it is still the device's.
            .interactable(false)
            .show(context, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(10, 10, 10, 225))
                    .corner_radius(6)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.set_max_width(640.0);
                        ui.label(
                            egui::RichText::new(&diagnostics.text)
                                .monospace()
                                .color(egui::Color32::WHITE),
                        );
                    });
            });
    }

    /// Shown over the video while the connection is being restored: the
    /// reason, how long it has been down, when the next attempt starts, and
    /// "Retry now".
    fn reconnect_panel(&mut self, context: &egui::Context) {
        let Some(status) = self.reconnect else {
            return;
        };
        let text = status.render(Instant::now());
        let panel = egui::Area::new(egui::Id::unique("reconnect"))
            .order(egui::Order::Middle)
            .anchor(
                egui::Align2::CENTER_CENTER,
                egui::vec2(0.0, toolbar::HEIGHT as f32 / 2.0),
            )
            .show(context, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(10, 10, 10, 225))
                    .corner_radius(10)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(460.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(text.title)
                                    .size(HEADING)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            ui.label(egui::RichText::new(&text.detail).color(WEAK_TEXT));
                            ui.add_space(4.0);
                            if ui
                                .add_enabled(text.retry_enabled, egui::Button::new("Retry now"))
                                .clicked()
                            {
                                crate::reconnect::request_retry_now();
                            }
                        });
                    });
            });
        self.control_rects.push(panel.response.rect);
    }

    fn chat_panel(
        &mut self,
        context: &egui::Context,
        screen: egui::Rect,
        item: Option<egui::Rect>,
    ) {
        if !self.chat_open {
            return;
        }
        let chat = self.control.chat();
        // Under the chat item, and inside the window.
        let right = item.map_or(screen.right() - 8.0, |item| item.right());
        let left = (right - CHAT_SIZE.x)
            .min(screen.right() - CHAT_SIZE.x - 8.0)
            .max(8.0);
        let mut close = false;
        let mut send = false;
        let panel = egui::Area::new(egui::Id::unique("chat"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(left, toolbar::HEIGHT as f32 + 6.0))
            .show(context, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(CHAT_SIZE.x - 16.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Chat").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            close = ui.small_button("Close").clicked();
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(CHAT_SIZE.y - 110.0)
                        .min_scrolled_height(CHAT_SIZE.y - 110.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let transcript = chat.transcript();
                            if transcript.is_empty() {
                                ui.label(
                                    egui::RichText::new(
                                        "Messages you send appear on the remote computer.",
                                    )
                                    .color(WEAK_TEXT),
                                );
                            }
                            for message in transcript {
                                ui.add(egui::Label::new(message).wrap().selectable(true));
                            }
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        let send_width = 64.0;
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut self.chat_draft)
                                .hint_text("Message")
                                .desired_width(ui.available_width() - send_width - 8.0),
                        );
                        if std::mem::take(&mut self.chat_focus) {
                            field.request_focus();
                        }
                        let entered = field.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        let clicked = ui
                            .add_sized([send_width, 26.0], egui::Button::new("Send"))
                            .clicked();
                        if entered || clicked {
                            send = true;
                            field.request_focus();
                        }
                    });
                });
            });
        self.control_rects.push(panel.response.rect);
        if send {
            let text = self.chat_draft.trim().to_owned();
            if !text.is_empty() && chat.send(text) {
                self.control.note_activity();
                self.chat_draft.clear();
            }
        }
        if close || context.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.set_chat_open(false);
        }
    }

    fn prompt_window(&mut self, context: &egui::Context) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let (title, detail, confirm) = match prompt {
            Prompt::Disconnect => (
                "Disconnect from this device?".to_owned(),
                "Your remote session will end.".to_owned(),
                Some("Disconnect"),
            ),
            Prompt::Restart { safe_mode } => {
                let (title, detail, confirm) = toolbar::restart_confirmation(*safe_mode);
                (title.to_owned(), detail.to_owned(), Some(confirm))
            }
            Prompt::Alert { title, message } => (title.clone(), message.clone(), None),
        };
        let mut answer = None;
        let shown = egui::Modal::new(egui::Id::unique("prompt")).show(context, |ui| {
            ui.set_width(400.0);
            ui.label(egui::RichText::new(title).size(HEADING).strong());
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(detail).wrap().selectable(true))
                });
            ui.add_space(10.0);
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match confirm {
                    Some(confirm) => {
                        if ui.button(confirm).clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    }
                    None => {
                        if ui.button("OK").clicked() {
                            answer = Some(true);
                        }
                    }
                },
            );
        });
        // Esc, or a click beside the question, declines.
        let Some(confirmed) = answer.or(shown.should_close().then_some(false)) else {
            return;
        };
        self.control.note_activity();
        match self.prompt.take() {
            Some(Prompt::Disconnect) if confirmed => {
                self.close();
                return;
            }
            Some(Prompt::Restart { safe_mode }) if confirmed => self.control.restart(safe_mode),
            _ => {}
        }
        self.enable_input();
    }
}

impl SessionWindow {
    pub(super) fn open(gpu: &Gpu, canvas: Canvas, request: SessionRequest) -> Self {
        let SessionRequest {
            shared,
            format,
            display,
            displays,
        } = request;
        let window = Arc::clone(&canvas.window);
        let control = shared.control.clone();
        let focused = window.has_focus();
        let view = View {
            egui: canvas.egui.clone(),
            debug: shared.debug.clone(),
            // The window is brought to the front as it opens.
            shows_video: control.video_window(true),
            control,
            shared,
            active_display: display,
            displays,
            video_size: (format.width.max(1), format.height.max(1)),
            agent_pointer_display: None,
            cursor_shape: CursorShape::Default,
            held: HeldInput::default(),
            annotator: Annotator::default(),
            wheel: WheelNormalizer::default(),
            modifiers: ModifiersState::empty(),
            pointer: None,
            control_buttons: Vec::new(),
            control_rects: Vec::new(),
            focused,
            occluded: false,
            visibility_checked: Instant::now(),
            reconnect: None,
            diagnostics: None,
            prompt: None,
            alerts: VecDeque::new(),
            chat_open: false,
            chat_draft: String::new(),
            chat_focus: false,
            toolbox_offer: None,
            outputs: Vec::new(),
            dropped: Vec::new(),
            moving: false,
            statistics: Statistics {
                since: Instant::now(),
                presented_since: 0,
                presented: 0,
            },
            picture_pending: false,
            closed: false,
            window,
        };
        view.control.set_input_enabled(focused);
        let video = VideoRenderer::new(gpu, canvas.format());
        canvas.window.focus_window();
        Self {
            canvas,
            video,
            view,
        }
    }

    /// The presenter this window shows.
    pub(super) fn presenter(&self) -> u64 {
        self.view.shared.id
    }

    pub(super) fn window_id(&self) -> winit::window::WindowId {
        self.canvas.window.id()
    }

    /// Whether the technician closed the window.
    pub(super) fn closed(&self) -> bool {
        self.view.closed
    }

    /// Asks for a redraw when one is due. Returns when to ask again.
    pub(super) fn poll(&mut self, now: Instant) -> Instant {
        if now.duration_since(self.view.visibility_checked) >= VISIBILITY_CHECK {
            self.view.update_shows_video();
        }
        self.canvas
            .poll(now)
            .min(self.view.visibility_checked + VISIBILITY_CHECK)
    }

    /// The session changed something the window shows.
    pub(super) fn invalidate(&mut self) {
        self.canvas.invalidate();
    }

    /// The finished toolbox runs to show, each a title and its text.
    pub(super) fn take_outputs(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.view.outputs)
    }

    /// Shows a new connection's stream in this window, which was kept while
    /// the session reconnected.
    pub(super) fn adopt(&mut self, request: SessionRequest) {
        let SessionRequest {
            shared,
            format,
            display,
            displays,
        } = request;
        let view = &mut self.view;
        // The keys and buttons went with the connection that held them.
        view.held = HeldInput::default();
        view.annotator = Annotator::default();
        if view.chat_open {
            view.chat_open = false;
        }
        view.control = shared.control.clone();
        view.debug = shared.debug.clone();
        view.shared = shared;
        view.shows_video = view.control.video_window(true);
        view.update_shows_video();
        view.reconnect = None;
        view.toolbox_offer = None;
        view.picture_pending = false;
        view.configure_display(display, displays);
        if self.video.picture_size().is_none() {
            view.video_size = (format.width.max(1), format.height.max(1));
        }
        view.control.set_input_enabled(false);
        view.enable_input();
        self.invalidate();
    }

    /// What the presenter sent. Returns whether the window is to close.
    pub(super) fn command(&mut self, gpu: &Gpu, command: SessionCommand) -> bool {
        let view = &mut self.view;
        match command {
            SessionCommand::Reset {
                format,
                display,
                displays,
                reply,
            } => {
                view.configure_display(display, displays);
                if self.video.picture_size().is_none() {
                    view.video_size = (format.width.max(1), format.height.max(1));
                }
                let _ = reply.send(Ok(()));
            }
            SessionCommand::AgentPointerDisplay(display_id) => {
                view.agent_pointer_display = display_id;
            }
            SessionCommand::Displays(displays) => view.displays = displays,
            SessionCommand::Refresh => {}
            SessionCommand::ReconnectStatus(status) => {
                if status.is_some() && view.reconnect.is_none() {
                    // Nothing reaches the device until it is back.
                    view.release_input();
                }
                view.reconnect = status;
            }
            SessionCommand::CursorShape(shape) => view.cursor_shape = shape,
            SessionCommand::Picture => self.take_picture(gpu),
            SessionCommand::Close => return true,
        }
        self.invalidate();
        false
    }

    /// Uploads the decoder's newest picture.
    fn take_picture(&mut self, gpu: &Gpu) {
        let shared = &self.view.shared;
        // Cleared first: a picture decoded after this is announced again.
        shared.picture_posted.store(false, Ordering::Release);
        let picture = shared
            .picture
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(picture) = picture else {
            return;
        };
        if VideoRenderer::fits(gpu, picture.width, picture.height) {
            self.video.upload(gpu, &picture);
            self.view.video_size = (picture.width, picture.height);
            self.view.picture_pending = true;
        } else {
            shared.fail(format!(
                "this computer's GPU cannot show a {}x{} picture; its largest is {} pixels a side",
                picture.width,
                picture.height,
                gpu.max_texture_side()
            ));
        }
        shared.recycle(picture);
    }

    pub(super) fn window_event(&mut self, gpu: &Gpu, event: WindowEvent) {
        let window = Arc::clone(&self.canvas.window);
        let to_controls = |canvas: &mut Canvas, event: &WindowEvent| {
            let _ = canvas.input.on_window_event(&window, event);
        };
        match &event {
            WindowEvent::RedrawRequested => {
                self.paint(gpu);
                return;
            }
            WindowEvent::CloseRequested => self.view.request_close(),
            WindowEvent::Resized(_) => {
                self.canvas.resized(gpu);
                self.view.update_shows_video();
            }
            WindowEvent::Focused(focused) => {
                to_controls(&mut self.canvas, &event);
                self.view.focus_changed(*focused);
            }
            WindowEvent::Occluded(occluded) => {
                self.view.occluded = *occluded;
                self.view.update_shows_video();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.view.modifiers = modifiers.state();
                to_controls(&mut self.canvas, &event);
            }
            WindowEvent::KeyboardInput {
                event: key,
                is_synthetic,
                ..
            } => {
                if self.view.controls_open() || self.canvas.egui.egui_wants_keyboard_input() {
                    to_controls(&mut self.canvas, &event);
                } else if !is_synthetic && self.view.reconnect.is_none() {
                    // Keys already down when the window took the keyboard
                    // were pressed for something else.
                    self.view.key(key);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                to_controls(&mut self.canvas, &event);
                self.view.pointer_moved(*position);
            }
            WindowEvent::CursorLeft { .. } => {
                to_controls(&mut self.canvas, &event);
                self.view.pointer = None;
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = *state == ElementState::Pressed;
                if pressed
                    && *button == MouseButton::Left
                    && let Some(edge) = self.view.resize_edge()
                {
                    // The desktop takes the mouse until the button is up.
                    if let Err(error) = window.drag_resize_window(edge) {
                        tracing::debug!(%error, "the desktop did not start resizing the window");
                    }
                } else {
                    to_controls(&mut self.canvas, &event);
                    self.view.button(*button, pressed);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.view.over_controls() {
                    to_controls(&mut self.canvas, &event);
                } else {
                    self.view.wheel(*delta);
                }
            }
            WindowEvent::DroppedFile(path) => self.view.dropped.push(path.clone()),
            _ => to_controls(&mut self.canvas, &event),
        }
        self.invalidate();
    }

    fn paint(&mut self, gpu: &Gpu) {
        let now = Instant::now();
        self.view.collect();
        let view = &mut self.view;
        let video = &self.video;
        let video_size = view.video_size;
        let toolbar = view.toolbar_pixels();
        let painted = self.canvas.paint(
            gpu,
            wgpu::Color::BLACK,
            |ui| view.ui(ui),
            |pass, size| {
                let area = video_layout::letterbox(
                    VideoRect {
                        left: 0,
                        top: toolbar,
                        width: size.width as i32,
                        height: (size.height as i32 - toolbar).max(1),
                    },
                    video_size.0,
                    video_size.1,
                );
                video.draw(pass, area);
            },
        );
        let view = &mut self.view;
        if std::mem::take(&mut view.moving) {
            if let Err(error) = view.window.drag_window() {
                tracing::debug!(%error, "the desktop did not start moving the window");
            }
            // The desktop keeps the button's release to itself.
            let input = self.canvas.input.egui_input_mut();
            input.events.push(egui::Event::PointerButton {
                pos: egui::Pos2::ZERO,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
            view.control_buttons.clear();
        }
        if painted.presented && std::mem::take(&mut view.picture_pending) {
            view.shared.first_presented.get_or_init(Instant::now);
            view.statistics.presented += 1;
            view.statistics.presented_since += 1;
        }
        let elapsed = view.statistics.since.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let decode_fps = f64::from_bits(view.shared.decode_fps.load(Ordering::Relaxed));
            view.debug.update_presentation(
                (decode_fps > 0.0).then_some(decode_fps),
                f64::from(view.statistics.presented_since) / elapsed.as_secs_f64(),
                view.statistics.presented,
                Some(view.shared.dropped.load(Ordering::Relaxed)),
                view.shared.replaced.load(Ordering::Relaxed),
            );
            view.statistics.since = now;
            view.statistics.presented_since = 0;
        }
        if view.reconnect.is_some() || view.diagnostics.is_some() {
            self.canvas.paint_within(TICK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mouse_resizes_the_window_by_the_edge_it_is_on() {
        let size = PhysicalSize::new(800, 600);
        let at = |x: f64, y: f64| resize_direction(PhysicalPosition::new(x, y), size, 4.0);
        assert_eq!(at(400.0, 300.0), None);
        assert_eq!(at(1.0, 300.0), Some(ResizeDirection::West));
        assert_eq!(at(798.0, 300.0), Some(ResizeDirection::East));
        assert_eq!(at(400.0, 0.0), Some(ResizeDirection::North));
        assert_eq!(at(400.0, 599.0), Some(ResizeDirection::South));
        assert_eq!(at(2.0, 2.0), Some(ResizeDirection::NorthWest));
        assert_eq!(at(797.0, 1.0), Some(ResizeDirection::NorthEast));
        assert_eq!(at(0.0, 598.0), Some(ResizeDirection::SouthWest));
        assert_eq!(at(799.0, 599.0), Some(ResizeDirection::SouthEast));
        // Just inside the margin is the window's own.
        assert_eq!(at(4.0, 300.0), None);
        assert_eq!(at(795.9, 300.0), None);
    }

    #[test]
    fn every_remote_cursor_has_a_local_one() {
        assert_eq!(cursor_icon(CursorShape::Text), egui::CursorIcon::Text);
        assert_eq!(
            cursor_icon(CursorShape::ResizeWestEast),
            egui::CursorIcon::ResizeHorizontal
        );
        // The desktop has no cursor for these; the arrow stands in.
        assert_eq!(cursor_icon(CursorShape::Person), egui::CursorIcon::Default);
    }
}
