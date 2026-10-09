mod alerts;
mod connecting;
mod input;
mod launch;
mod reconnect_panel;
mod session_controls;
mod toolbar_actions;

use super::keyboard::{self, CommandKey, Keyboard};
use super::toolbar::ToolbarView;
use super::*;
use crate::input::HeldInput;
use crate::reconnect::ReconnectStatus;
use meshrmm_protocol::{HeadlessResolution, SessionCloseAction};
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSDragOperation, NSDraggingDestination, NSDraggingInfo, NSEventMask, NSMenuItem,
    NSPasteboardTypeFileURL,
};

use alerts::queue_alert;
pub use alerts::show_notice;
pub(super) use connecting::close_connecting_window;
pub use connecting::show_launch_status;
pub use launch::run_application;
use reconnect_panel::ReconnectPanel;

#[derive(Default)]
pub(super) struct VideoHostViewIvars;

define_class!(
    // Safety: NSView is designed for subclassing; this view remains on the
    // AppKit main thread and owns no resources requiring Drop.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = VideoHostViewIvars]
    pub(super) struct VideoHostView;

    unsafe impl NSObjectProtocol for VideoHostView {}

    impl VideoHostView {
        /// Let the parent RemoteView receive pointer input over the video while
        /// the toolbar controls retain normal hit testing.
        #[unsafe(method_id(hitTest:))]
        #[unsafe(method_family = none)]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<Self>> {
            None
        }
    }
);

impl VideoHostView {
    pub(super) fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(VideoHostViewIvars);
        // Safety: NSView's frame initializer is the designated initializer for
        // a programmatically created view.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub(super) struct RemoteViewIvars {
    active_display: RefCell<Display>,
    displays: RefCell<Vec<Display>>,
    video_width: std::cell::Cell<u32>,
    video_height: std::cell::Cell<u32>,
    toolbar: RefCell<Option<Retained<ToolbarView>>>,
    confirming_disconnect: std::cell::Cell<bool>,
    // A miniaturized window or hidden application also reports
    // `isVisible == false`, so only this flag means the session window closed.
    window_closed: std::cell::Cell<bool>,
    chat_popup: RefCell<Option<meshrmm_chat::ChatPopup>>,
    control: ControlSink,
    held: RefCell<HeldInput>,
    annotator: RefCell<crate::annotation::Annotator>,
    keyboard: RefCell<Keyboard>,
    /// The system's key-down count at the last `flagsChanged:` event.
    key_downs: std::cell::Cell<u32>,
    key_up_monitor: RefCell<Option<Retained<AnyObject>>>,
    wheel_normalizer: RefCell<WheelNormalizer>,
    cursor_shape: RefCell<CursorShape>,
    agent_pointer_display: std::cell::Cell<Option<meshrmm_protocol::DisplayId>>,
    debug: DebugInfo,
    debug_label: Retained<NSTextField>,
    reconnect_panel: ReconnectPanel,
    reconnect_status: std::cell::Cell<Option<ReconnectStatus>>,
    /// Whether a refresh of the elapsed time and countdown is scheduled.
    reconnect_ticking: std::cell::Cell<bool>,
    debug_visible: RefCell<bool>,
    debug_refreshed: RefCell<Instant>,
}

define_class!(
    // Safety: NSView is designed for subclassing; all instances remain on the
    // AppKit main thread.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = RemoteViewIvars]
    pub(super) struct RemoteView;

    unsafe impl NSObjectProtocol for RemoteView {}

    unsafe impl NSDraggingDestination for RemoteView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let paths = meshrmm_file_transfer::macos::paths_from_pasteboard(&sender.draggingPasteboard());
            tracing::info!(files = paths.len(), "native file drag entered viewer");
            if paths.is_empty() { NSDragOperation::None } else { NSDragOperation::Copy }
        }
        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            if meshrmm_file_transfer::macos::paths_from_pasteboard(&sender.draggingPasteboard()).is_empty() { NSDragOperation::None } else { NSDragOperation::Copy }
        }
        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_drag(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool { true }
        #[unsafe(method(performDragOperation:))]
        fn perform_drag(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let paths = meshrmm_file_transfer::macos::paths_from_pasteboard(&sender.draggingPasteboard());
            if paths.is_empty() { false } else {
            tracing::info!(files = paths.len(), "native file dropped on viewer");
            let point = self.convertPoint_fromView(sender.draggingLocation(), None);
            let mut bounds = self.bounds(); bounds.size.height = (bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
            let destination = normalized_video_position(point, bounds, self.ivars().video_width.get(), self.ivars().video_height.get())
                .map(|(x,y)| meshrmm_protocol::FileDestination::Drop { display_id: self.ivars().active_display.borrow().id, x, y })
                .unwrap_or(meshrmm_protocol::FileDestination::Documents);
            self.release_input();
            self.ivars().control.files().send(paths, destination); true
            }
        }
    }

    unsafe impl NSWindowDelegate for RemoteView {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _window: &NSWindow) -> bool {
            self.confirm_disconnect()
        }
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.ivars().window_closed.set(true);
            // Ends the session even while it is reconnecting and no transport
            // is watching this window.
            crate::shutdown::request("the viewer window was closed");
        }
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            self.ivars().control.set_input_enabled(true);
            if let Some(window) = self.window()
                && !window.makeFirstResponder(Some(self))
            {
                tracing::warn!("macOS viewer could not restore remote-input focus");
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.disable_input();
        }
    }

    impl RemoteView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            false
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let cursor = mac_cursor(self.video_cursor());
            let mut bounds = self.bounds();
            bounds.size.height = (bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
            self.addCursorRect_cursor(bounds, &cursor);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.annotating() {
                let display_id = self.ivars().active_display.borrow().id;
                let message = self
                    .ivars()
                    .annotator
                    .borrow_mut()
                    .extend(display_id, self.pointer_position(event));
                if let Some(message) = message {
                    self.send(message);
                }
                return;
            }
            self.send_pointer(event);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.annotating() {
                let display_id = self.ivars().active_display.borrow().id;
                let message = self
                    .ivars()
                    .annotator
                    .borrow_mut()
                    .start(display_id, self.pointer_position(event));
                if let Some(message) = message {
                    self.send(message);
                }
                return;
            }
            self.send_button(event, PointerButton::Left, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if self.annotating() {
                self.ivars().annotator.borrow_mut().finish();
                return;
            }
            self.send_button(event, PointerButton::Left, false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            if self.annotating() {
                if self.pointer_position(event).is_some() {
                    let message = self.ivars().annotator.borrow_mut().clear();
                    if let Some(message) = message {
                        self.send(message);
                    }
                }
                return;
            }
            self.send_button(event, PointerButton::Right, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            if self.annotating() {
                return;
            }
            self.send_button(event, PointerButton::Right, false);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            if self.annotating() {
                return;
            }
            self.send_button(event, mac_button(event), true);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            if self.annotating() {
                return;
            }
            self.send_button(event, mac_button(event), false);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let Some((x, y)) = self.pointer_position(event) else {
                return;
            };
            let (horizontal, vertical) = self.ivars().wheel_normalizer.borrow_mut().normalize(
                event.scrollingDeltaX(),
                event.scrollingDeltaY(),
                event.hasPreciseScrollingDeltas(),
            );
            if horizontal == 0 && vertical == 0 {
                return;
            }
            self.sync_modifiers(event.modifierFlags());
            self.engage_command();
            self.send(SessionMessage::Input(RemoteInput::WheelAt {
                display_id: self.ivars().active_display.borrow().id,
                x,
                y,
                horizontal,
                vertical,
            }));
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.sync_modifiers(event.modifierFlags());
            if self.is_diagnostics_key(event) {
                if !event.isARepeat() {
                    self.toggle_debug();
                }
                return;
            }
            let modifiers = event.modifierFlags();
            if event.keyCode() == 9 && (modifiers.contains(NSEventModifierFlags::Control) || modifiers.contains(NSEventModifierFlags::Command))
                && self.ivars().control.files().paste_files(self.ivars().active_display.borrow().id) {
                self.release_input(); return;
            }
            if modifiers.contains(NSEventModifierFlags::Control)
                && modifiers.contains(NSEventModifierFlags::Option)
                && matches!(event.keyCode(), 123 | 124)
            {
                self.select_adjacent(event.keyCode() == 124);
                return;
            }
            self.engage_command();
            self.send_key(event.keyCode(), true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.handle_key_up(event);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let key_downs = keyboard::system_key_downs();
            let (keys, tap) = {
                let mut keyboard = self.ivars().keyboard.borrow_mut();
                // Keys AppKit delivers engage Command themselves; a count that
                // moved anyway means the system took one, as in Cmd-Tab.
                if self.ivars().key_downs.replace(key_downs) != key_downs {
                    keyboard.cancel_command_tap();
                }
                let keys = keyboard.flags_changed(event.keyCode(), event.modifierFlags().0 as u64);
                (keys, keyboard.take_command_tap())
            };
            self.send_keys(keys);
            if let Some((scan_code, extended)) = tap {
                self.send_scan_code(scan_code, extended, true);
                self.send_scan_code(scan_code, extended, false);
            }
        }

        #[unsafe(method(retryReconnect:))]
        fn retry_reconnect(&self, _sender: &NSButton) {
            // Disabled until the session loop starts its next wait.
            self.ivars().reconnect_panel.retry.setEnabled(false);
            crate::reconnect::request_retry_now();
        }

        #[unsafe(method(toggleTechnicianInput:))]
        fn toggle_technician_input(&self, _sender: &NSMenuItem) {
            self.release_input();
            let blocked = !self.ivars().control.technician_blocked();
            self.ivars().control.set_technician_blocked(blocked);
            self.refresh_cursor();
            self.ivars().control.set_input_enabled(true);
        }

        #[unsafe(method(togglePreventIdleLock:))]
        fn toggle_prevent_idle_lock(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_prevent_idle_lock();
        }

        #[unsafe(method(toggleDisconnectConfirmation:))]
        fn toggle_disconnect_confirmation(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_disconnect_confirmation();
        }

        #[unsafe(method(toggleCommandAsControl:))]
        fn toggle_command_as_control(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_command_as_control();
            let keys = self
                .ivars()
                .keyboard
                .borrow_mut()
                .set_command_key(command_key(&self.ivars().control));
            self.send_keys(keys);
        }

        #[unsafe(method(selectSessionCloseAction:))]
        fn select_session_close_action(&self, sender: &NSMenuItem) {
            if let Some(action) = usize::try_from(sender.tag()).ok().and_then(|index| SessionCloseAction::ALL.get(index)) {
                self.ivars().control.set_session_close_action(*action);
            }
        }

        #[unsafe(method(selectHeadlessResolution:))]
        fn select_headless_resolution(&self, sender: &NSMenuItem) {
            if let Some(resolution) = usize::try_from(sender.tag()).ok().and_then(|index| HeadlessResolution::PRESETS.get(index)) {
                self.ivars().control.set_headless_resolution(*resolution);
            }
        }

        #[unsafe(method(selectIdleDisconnect:))]
        fn select_idle_disconnect(&self, sender: &NSMenuItem) {
            if let Some(minutes) = usize::try_from(sender.tag()).ok().and_then(|index| crate::idle_disconnect::choices().nth(index)) {
                self.ivars().control.set_idle_disconnect_minutes(minutes);
            }
        }

        #[unsafe(method(selectDiagnosticsKey:))]
        fn select_diagnostics_key(&self, sender: &NSMenuItem) {
            if let Some(key) = usize::try_from(sender.tag()).ok().and_then(|index| crate::shortcuts::ShortcutKey::ALL.get(index)) {
                self.ivars().control.set_shortcut_key(crate::shortcuts::ViewerShortcut::Diagnostics, *key);
                if let Some(window) = self.window() {
                    window.setTitle(&NSString::from_str(&super::presenter::window_title(&self.ivars().active_display.borrow().name)));
                }
            }
        }

        #[unsafe(method(toggleClipboardSync:))]
        fn toggle_clipboard_sync(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_clipboard_sync();
        }

        #[unsafe(method(toggleClearClipboardOnClose:))]
        fn toggle_clear_clipboard_on_close(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_clear_clipboard_on_close();
        }

        #[unsafe(method(toggleDisplayBorder:))]
        fn toggle_display_border(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_display_border();
        }

        #[unsafe(method(toggleWallpaper:))]
        fn toggle_wallpaper(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_wallpaper();
        }

        #[unsafe(method(toggleRemoteCursor:))]
        fn toggle_remote_cursor(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_remote_cursor();
        }

        #[unsafe(method(toggleRecording:))]
        fn toggle_recording(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_recording();
        }

        #[unsafe(method(toggleAudio:))]
        fn toggle_audio(&self, _sender: &NSMenuItem) {
            self.ivars().control.toggle_audio();
        }

        #[unsafe(method(toggleBlackout:))]
        fn toggle_blackout(&self, _sender: &NSMenuItem) {
            self.release_input();
            self.ivars().control.toggle_blackout();
        }

        #[unsafe(method(toggleAgentInput:))]
        fn toggle_agent_input(&self, _sender: &NSMenuItem) {
            self.release_input();
            self.ivars().control.toggle_agent_input();
        }
    }
);

const WINDOWS_WHEEL_DELTA: f64 = 120.0;
const PRECISE_SCROLL_POINTS_PER_NOTCH: f64 = 60.0;
const PRECISE_SCROLL_STEPS_PER_NOTCH: f64 = 8.0;

/// Converts high-frequency trackpad motion into small, high-resolution Windows
/// wheel steps. Accumulation prevents every tiny AppKit event from becoming a
/// scroll action, while subdivisions avoid the jump caused by full notches.
#[derive(Default)]
pub(super) struct WheelNormalizer {
    horizontal_remainder: f64,
    vertical_remainder: f64,
}

impl WheelNormalizer {
    pub(super) fn normalize(
        &mut self,
        horizontal: f64,
        vertical: f64,
        precise: bool,
    ) -> (i16, i16) {
        if precise {
            (
                precise_wheel_delta(horizontal, &mut self.horizontal_remainder),
                precise_wheel_delta(vertical, &mut self.vertical_remainder),
            )
        } else {
            self.horizontal_remainder = 0.0;
            self.vertical_remainder = 0.0;
            (coarse_wheel_delta(horizontal), coarse_wheel_delta(vertical))
        }
    }
}

fn precise_wheel_delta(delta: f64, remainder: &mut f64) -> i16 {
    if !delta.is_finite() || delta == 0.0 {
        return 0;
    }
    if *remainder != 0.0 && remainder.signum() != delta.signum() {
        *remainder = 0.0;
    }
    *remainder += delta;
    let points_per_step = PRECISE_SCROLL_POINTS_PER_NOTCH / PRECISE_SCROLL_STEPS_PER_NOTCH;
    let wheel_delta_per_step = WINDOWS_WHEEL_DELTA / PRECISE_SCROLL_STEPS_PER_NOTCH;
    let steps = (*remainder / points_per_step).trunc();
    *remainder -= steps * points_per_step;
    clamp_wheel_delta(steps * wheel_delta_per_step)
}

fn coarse_wheel_delta(delta: f64) -> i16 {
    clamp_wheel_delta(delta * WINDOWS_WHEEL_DELTA)
}

fn clamp_wheel_delta(delta: f64) -> i16 {
    delta
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

impl RemoteView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        active_display: Display,
        displays: Vec<Display>,
        video_width: u32,
        video_height: u32,
        control: ControlSink,
        debug: DebugInfo,
    ) -> Retained<Self> {
        let debug_label =
            NSTextField::wrappingLabelWithString(&NSString::from_str("MeshRMM diagnostics"), mtm);
        debug_label.setFrame(NSRect {
            origin: NSPoint {
                x: 12.0,
                y: (frame.size.height - VIEWER_TOOLBAR_HEIGHT - 312.0).max(12.0),
            },
            size: NSSize {
                width: (frame.size.width - 24.0).clamp(300.0, 640.0),
                height: 300.0,
            },
        });
        debug_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        debug_label.setDrawsBackground(true);
        debug_label.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.04, 0.88)));
        debug_label.setTextColor(Some(&NSColor::whiteColor()));
        debug_label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
            12.0,
            unsafe { NSFontWeightRegular },
        )));
        debug_label.setHidden(true);
        let reconnect_panel = ReconnectPanel::new(mtm, frame);
        let command = command_key(&control);
        let this = Self::alloc(mtm).set_ivars(RemoteViewIvars {
            active_display: RefCell::new(active_display),
            displays: RefCell::new(displays),
            video_width: std::cell::Cell::new(video_width),
            video_height: std::cell::Cell::new(video_height),
            toolbar: RefCell::new(None),
            chat_popup: RefCell::new(None),
            confirming_disconnect: std::cell::Cell::new(false),
            window_closed: std::cell::Cell::new(false),
            control,
            held: RefCell::new(HeldInput::default()),
            annotator: RefCell::new(crate::annotation::Annotator::default()),
            keyboard: RefCell::new(Keyboard::new(command)),
            key_downs: std::cell::Cell::new(keyboard::system_key_downs()),
            key_up_monitor: RefCell::new(None),
            wheel_normalizer: RefCell::new(WheelNormalizer::default()),
            cursor_shape: RefCell::new(CursorShape::Default),
            agent_pointer_display: std::cell::Cell::new(None),
            debug,
            debug_label,
            reconnect_panel,
            reconnect_status: std::cell::Cell::new(None),
            reconnect_ticking: std::cell::Cell::new(false),
            debug_visible: RefCell::new(false),
            debug_refreshed: RefCell::new(Instant::now()),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.addSubview(&this.ivars().debug_label);
        this.addSubview(&this.ivars().reconnect_panel.panel);
        // Button targets are weak, so this does not keep the view alive.
        unsafe {
            this.ivars().reconnect_panel.retry.setTarget(Some(&this));
            this.ivars()
                .reconnect_panel
                .retry
                .setAction(Some(sel!(retryReconnect:)));
        }
        this.install_toolbar(mtm, frame);
        this.install_key_up_monitor();
        this
    }

    /// AppKit does not send `keyUp:` for keys released while Command is held,
    /// which would leave Cmd-L's L down on the remote. A local monitor sees
    /// those events first and hands them to this view.
    fn install_key_up_monitor(&self) {
        let view = objc2::rc::Weak::new(self);
        let handler = block2::RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // Safety: AppKit passes a valid event for the handler's duration.
            let event_ref = unsafe { event.as_ref() };
            if let Some(view) = view.load()
                && view.takes_command_key_up(event_ref)
            {
                view.handle_key_up(event_ref);
                return ptr::null_mut();
            }
            event.as_ptr()
        });
        // Safety: the handler returns the event it was given or null.
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyUp, &handler)
        };
        *self.ivars().key_up_monitor.borrow_mut() = monitor;
    }

    pub(super) fn remove_key_up_monitor(&self) {
        if let Some(monitor) = self.ivars().key_up_monitor.borrow_mut().take() {
            // Safety: the monitor came from addLocalMonitorForEventsMatchingMask.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }

    fn takes_command_key_up(&self, event: &NSEvent) -> bool {
        if !event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
        {
            return false;
        }
        let Some(window) = self.window() else {
            return false;
        };
        let ours = event
            .window(self.mtm())
            .is_some_and(|target| Retained::as_ptr(&target) == Retained::as_ptr(&window));
        let focused = window.firstResponder().is_some_and(|responder| {
            ptr::eq(
                Retained::as_ptr(&responder).cast::<u8>(),
                (self as *const Self).cast::<u8>(),
            )
        });
        ours && focused && window.isKeyWindow()
    }

    fn handle_key_up(&self, event: &NSEvent) {
        self.sync_modifiers(event.modifierFlags());
        if self.is_diagnostics_key(event) {
            return;
        }
        self.send_key(event.keyCode(), false);
    }

    fn install_toolbar(&self, mtm: MainThreadMarker, frame: NSRect) {
        let toolbar = ToolbarView::new(
            mtm,
            NSRect::new(
                NSPoint::new(0.0, (frame.size.height - VIEWER_TOOLBAR_HEIGHT).max(0.0)),
                NSSize::new(frame.size.width, VIEWER_TOOLBAR_HEIGHT),
            ),
            self,
        );
        toolbar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        self.addSubview(&toolbar);
        *self.ivars().toolbar.borrow_mut() = Some(toolbar);
        self.registerForDraggedTypes(&NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]));
        let control = self.ivars().control.clone();
        *self.ivars().chat_popup.borrow_mut() = Some(meshrmm_chat::ChatPopup::new(
            control.chat(),
            self,
            move |enabled| control.set_input_enabled(enabled),
        ));
        self.refresh_toolbar();
    }

    fn send(&self, message: SessionMessage) {
        self.ivars().control.send(message);
    }

    pub(super) fn window_closed(&self) -> bool {
        self.ivars().window_closed.get()
    }

    pub(super) fn confirm_disconnect(&self) -> bool {
        self.disable_input();
        if !self.ivars().control.disconnect_confirmation() {
            return true;
        }
        if self.ivars().confirming_disconnect.replace(true) {
            return false;
        }
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str("Disconnect from this device?"));
        alert.setInformativeText(&NSString::from_str("Your remote session will end."));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.addButtonWithTitle(&NSString::from_str("Disconnect"));
        let confirmed = alert.runModal() == 1001;
        self.ivars().confirming_disconnect.set(false);
        if !confirmed && self.window().is_some_and(|window| window.isKeyWindow()) {
            self.ivars().control.set_input_enabled(true);
        }
        confirmed
    }

    pub(super) fn set_agent_pointer_display(
        &self,
        display_id: Option<meshrmm_protocol::DisplayId>,
    ) {
        self.ivars().agent_pointer_display.set(display_id);
        self.refresh_toolbar();
    }

    pub(super) fn set_cursor_shape(&self, shape: CursorShape) {
        if *self.ivars().cursor_shape.borrow() == shape {
            return;
        }
        *self.ivars().cursor_shape.borrow_mut() = shape;
        self.refresh_cursor();
    }

    fn refresh_cursor(&self) {
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        mac_cursor(self.video_cursor()).set();
    }

    /// The cursor over the video: a crosshair while annotating.
    fn video_cursor(&self) -> CursorShape {
        if self.annotating() {
            CursorShape::Crosshair
        } else {
            self.ivars()
                .control
                .effective_cursor_shape(*self.ivars().cursor_shape.borrow())
        }
    }

    fn annotating(&self) -> bool {
        self.ivars().annotator.borrow().enabled()
    }

    fn toggle_annotating(&self) {
        // Keys and buttons held on the device stay there otherwise.
        self.release_input();
        let message = self.ivars().annotator.borrow_mut().toggle();
        if let Some(message) = message {
            self.send(message);
        }
        self.refresh_cursor();
    }

    fn is_diagnostics_key(&self, event: &NSEvent) -> bool {
        crate::shortcuts::macos_toggles_diagnostics(
            event.keyCode(),
            self.ivars()
                .control
                .shortcut_key(crate::shortcuts::ViewerShortcut::Diagnostics),
        )
    }

    fn toggle_debug(&self) {
        let visible = !*self.ivars().debug_visible.borrow();
        *self.ivars().debug_visible.borrow_mut() = visible;
        self.ivars().debug_label.setHidden(!visible);
        if visible {
            self.refresh_debug(true);
        }
        self.refresh_toolbar();
    }

    pub(super) fn refresh_debug(&self, force: bool) {
        if let Some(notice) = self.ivars().control.recording().take_notice() {
            queue_alert("Session recording", notice);
        }
        self.refresh_toolbar();
        if let Some(error) = self.ivars().control.take_maintenance_error() {
            queue_alert("Maintenance control failed", error);
        }
        while let Some(event) = self.ivars().control.toolbox().take_event() {
            match event {
                crate::toolbox::Event::RunFinished(run) => {
                    let (title, text) = crate::toolbox::run_report(&run);
                    super::script_output::show(self.mtm(), &title, &text);
                }
                crate::toolbox::Event::Failed { title, message } => {
                    queue_alert(title, message);
                }
            }
        }
        if !*self.ivars().debug_visible.borrow()
            || (!force
                && self.ivars().debug_refreshed.borrow().elapsed() < Duration::from_millis(250))
        {
            return;
        }
        *self.ivars().debug_refreshed.borrow_mut() = Instant::now();
        self.ivars()
            .debug_label
            .setStringValue(&NSString::from_str(&self.ivars().debug.render()));
    }

    pub(super) fn configure_display(
        &self,
        display: Display,
        displays: Vec<Display>,
        width: u32,
        height: u32,
    ) {
        if self.ivars().active_display.borrow().id != display.id {
            self.release_input();
            // A stroke belongs to the display it started on.
            self.ivars().annotator.borrow_mut().finish();
        }
        if display.session == meshrmm_protocol::DesktopSession::Background {
            self.ivars().annotator.borrow_mut().disable();
            self.refresh_cursor();
        }
        *self.ivars().active_display.borrow_mut() = display;
        *self.ivars().displays.borrow_mut() = displays;
        self.refresh_toolbar();
        self.ivars().video_width.set(width);
        self.ivars().video_height.set(height);
    }
}

pub(super) fn normalized_video_position(
    point: NSPoint,
    bounds: NSRect,
    video_width: u32,
    video_height: u32,
) -> Option<(u16, u16)> {
    let bounds_width = bounds.size.width.max(1.0);
    let bounds_height = bounds.size.height.max(1.0);
    let video_width = f64::from(video_width.max(1));
    let video_height = f64::from(video_height.max(1));
    let scale = (bounds_width / video_width).min(bounds_height / video_height);
    let presented_width = (video_width * scale).max(1.0);
    let presented_height = (video_height * scale).max(1.0);
    let presented_x = bounds.origin.x + (bounds_width - presented_width) / 2.0;
    let presented_y = bounds.origin.y + (bounds_height - presented_height) / 2.0;
    if point.x < presented_x
        || point.x > presented_x + presented_width
        || point.y < presented_y
        || point.y > presented_y + presented_height
    {
        return None;
    }
    let normalized_x = ((point.x - presented_x) / presented_width).clamp(0.0, 1.0);
    let normalized_y = ((point.y - presented_y) / presented_height).clamp(0.0, 1.0);
    Some((
        (normalized_x * 65_535.0).round() as u16,
        ((1.0 - normalized_y) * 65_535.0).round() as u16,
    ))
}

fn mac_button(event: &NSEvent) -> PointerButton {
    match event.buttonNumber() {
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        _ => PointerButton::Forward,
    }
}

#[allow(deprecated)]
fn mac_cursor(shape: CursorShape) -> Retained<NSCursor> {
    match shape {
        CursorShape::Text => NSCursor::IBeamCursor(),
        CursorShape::Crosshair => NSCursor::crosshairCursor(),
        CursorShape::ResizeWestEast => NSCursor::resizeLeftRightCursor(),
        CursorShape::ResizeNorthSouth => NSCursor::resizeUpDownCursor(),
        CursorShape::Move => NSCursor::openHandCursor(),
        CursorShape::NotAllowed => NSCursor::operationNotAllowedCursor(),
        CursorShape::Pointer => NSCursor::pointingHandCursor(),
        // AppKit has no public equivalent for these Windows system cursors.
        _ => NSCursor::arrowCursor(),
    }
}

/// What Command sends: Command itself to a Mac, and to Windows the Windows
/// key unless the technician prefers Ctrl.
fn command_key(control: &ControlSink) -> CommandKey {
    if control.command_as_control() && !control.device_is_mac() {
        CommandKey::Control
    } else {
        CommandKey::Windows
    }
}

pub(super) fn activate_application(mtm: MainThreadMarker) {
    tracing::info!("bringing the macOS viewer application to the foreground");
    let application = NSApplication::sharedApplication(mtm);
    // `activate` is newer than the MVP's macOS 12 deployment target.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
}

pub fn monotonic_timestamp_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}
