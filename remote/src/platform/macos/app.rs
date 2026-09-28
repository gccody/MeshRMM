use super::keyboard::{self, CommandKey, Keyboard, RemoteKey};
use super::toolbar::ToolbarView;
use super::*;
use crate::input::HeldInput;
use crate::reconnect::ReconnectStatus;
use crate::toolbar::{self, Action, Command};
use meshrmm_protocol::SessionCloseAction;
use objc2::ClassType;
use objc2::runtime::{AnyObject, Sel};
use objc2_app_kit::{
    NSDragOperation, NSDraggingDestination, NSDraggingInfo, NSEventMask, NSMenu, NSMenuItem,
    NSPasteboardTypeFileURL,
};

struct AppDelegateIvars {
    deep_link_tx: Sender<String>,
}

/// How long a replaced viewer waits for its session to end before it starts
/// the replacement anyway. Ending a session retries for up to ~16 seconds.
const REPLACEMENT_TIMEOUT: Duration = Duration::from_secs(20);

/// A later dashboard link, started once this viewer's session has ended.
static REPLACEMENT: Mutex<Option<String>> = Mutex::new(None);

/// Whether the network session is still running, so quitting must end it.
static SESSION_RUNNING: AtomicBool = AtomicBool::new(false);

/// Starts the viewer for a pending replacement link. Returns whether one was
/// pending; each link is launched at most once.
fn launch_replacement() -> bool {
    let Some(link) = REPLACEMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    else {
        return false;
    };
    let launched = std::env::current_exe()
        .context("could not locate the macOS viewer executable")
        .and_then(|executable| {
            std::process::Command::new(executable)
                .env_remove("MESHRMM_SESSION_BOOTSTRAP")
                .env_remove("MESHRMM_UPDATE_READY_FILE")
                .arg(link)
                .spawn()
                .context("could not launch the replacement macOS viewer")
        });
    match launched {
        Ok(_) => tracing::info!("started the macOS viewer for the new dashboard link"),
        Err(error) => tracing::error!(error = %error, "failed to restart the macOS viewer"),
    }
    true
}

define_class!(
    // Safety: NSObject has no subclassing requirements and AppDelegate does
    // not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    struct AppDelegate;

    // Safety: these protocols have no additional safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _application: &NSApplication,
        ) -> objc2_app_kit::NSApplicationTerminateReply {
            if super::presenter::request_user_disconnect() {
                objc2_app_kit::NSApplicationTerminateReply::TerminateCancel
            } else if SESSION_RUNNING.load(Ordering::Acquire) {
                // No window, for example while reconnecting or connecting:
                // end the session so the server releases the device, and
                // stop once the network thread is done.
                end_running_session("the user quit the viewer");
                objc2_app_kit::NSApplicationTerminateReply::TerminateCancel
            } else {
                objc2_app_kit::NSApplicationTerminateReply::TerminateNow
            }
        }
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, _application: &NSApplication, urls: &NSArray<NSURL>) {
            tracing::info!(
                url_count = urls.len(),
                "macOS viewer received a dashboard handoff"
            );
            let Some(url) = urls.firstObject() else {
                return;
            };
            let Some(value) = url.absoluteString() else {
                return;
            };
            let value = value.to_string();
            if let Err(error) = self.ivars().deep_link_tx.send(value) {
                // The launch receiver is intentionally consumed by the first
                // session. A later dashboard handoff means the user is
                // replacing a stale/broken session. End this session first:
                // the server refuses the new handoff while its lease is active.
                if !super::presenter::confirm_session_replacement() {
                    return;
                }
                let first = REPLACEMENT
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .replace(error.0)
                    .is_none();
                tracing::warn!("ending this session to replace it with a new dashboard handoff");
                crate::shutdown::request("a new dashboard link replaces this session");
                if first {
                    let when = dispatch2::DispatchTime::try_from(REPLACEMENT_TIMEOUT)
                        .unwrap_or(dispatch2::DispatchTime::NOW);
                    let scheduled = DispatchQueue::main().after(when, || {
                        if launch_replacement() {
                            tracing::warn!(
                                "the replaced session did not end in time; exiting without its cleanup"
                            );
                            std::process::exit(0);
                        }
                    });
                    if scheduled.is_err() {
                        tracing::warn!("could not schedule the viewer replacement deadline");
                    }
                }
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker, deep_link_tx: Sender<String>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars { deep_link_tx });
        // Safety: this invokes NSObject's parameterless initializer.
        unsafe { msg_send![super(this), init] }
    }
}

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
            let cursor = mac_cursor(self.ivars().control.effective_cursor_shape(*self.ivars().cursor_shape.borrow()));
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
            self.send_button(event, PointerButton::Left, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Left, false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Right, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Right, false);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.send_button(event, mac_button(event), true);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
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

/// Shown over the video while the connection is being restored: the reason,
/// how long it has been down, when the next attempt starts, and "Retry now".
struct ReconnectPanel {
    panel: Retained<NSView>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    retry: Retained<NSButton>,
}

impl ReconnectPanel {
    const WIDTH: f64 = 500.0;
    const HEIGHT: f64 = 120.0;

    /// A hidden panel centered over the video area of a view of `frame`.
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        let panel = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(
                NSPoint::new(
                    ((frame.size.width - Self::WIDTH) / 2.0).max(0.0),
                    ((frame.size.height - VIEWER_TOOLBAR_HEIGHT - Self::HEIGHT) / 2.0).max(0.0),
                ),
                NSSize::new(Self::WIDTH, Self::HEIGHT),
            ),
        );
        panel.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        panel.setWantsLayer(true);
        if let Some(layer) = panel.layer() {
            let background = NSColor::colorWithWhite_alpha(0.04, 0.88).CGColor();
            layer.setBackgroundColor(Some(&background));
            layer.setCornerRadius(10.0);
        }
        let label = |y: f64, height: f64, font: &NSFont, white: f64| {
            let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            label.setAlignment(NSTextAlignment::Center);
            label.setTextColor(Some(&NSColor::colorWithWhite_alpha(white, 1.0)));
            label.setFont(Some(font));
            label.setFrame(NSRect::new(
                NSPoint::new(16.0, y),
                NSSize::new(Self::WIDTH - 32.0, height),
            ));
            panel.addSubview(&label);
            label
        };
        let title = label(78.0, 24.0, &NSFont::boldSystemFontOfSize(16.0), 1.0);
        let detail = label(52.0, 20.0, &NSFont::systemFontOfSize(13.0), 0.8);
        // The target is set once the view exists.
        let retry = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Retry now"),
                None,
                None,
                mtm,
            )
        };
        retry.setFrame(NSRect::new(
            NSPoint::new((Self::WIDTH - 120.0) / 2.0, 12.0),
            NSSize::new(120.0, 28.0),
        ));
        retry.setEnabled(false);
        panel.addSubview(&retry);
        panel.setHidden(true);
        Self {
            panel,
            title,
            detail,
            retry,
        }
    }
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

    /// What the toolbar shows now.
    fn toolbar_state(&self) -> toolbar::State {
        let displays = self.ivars().displays.borrow();
        let active = self.ivars().active_display.borrow();
        let sessions = Display::sessions(&displays);
        let visible = active.session_displays(&displays);
        let control = &self.ivars().control;
        let chat = control.chat();
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
                .ivars()
                .agent_pointer_display
                .get()
                .and_then(|id| visible.iter().position(|display| display.id == id)),
            quality: control.quality_preset(),
            chroma: None,
            credentials: control.credential_state(),
            input_blocked: control.technician_blocked(),
            chat_available: chat.available(),
            chat_unread: chat.unread(),
            file_status: control.files().status(),
            recording: control.recording().active(),
            diagnostics: *self.ivars().debug_visible.borrow(),
            settings_menu: true,
            caption: None,
        }
    }

    fn refresh_toolbar(&self) {
        let state = self.toolbar_state();
        if let Some(toolbar) = self.ivars().toolbar.borrow().as_ref() {
            toolbar.set_state(state);
        }
    }

    /// A click on the toolbar item for `action`, at `rect` in the toolbar.
    pub(super) fn toolbar_action(&self, action: Action, rect: toolbar::Rect) {
        let Some(toolbar_view) = self.ivars().toolbar.borrow().clone() else {
            return;
        };
        match action {
            Action::User | Action::Display | Action::Quality | Action::Credentials => {
                self.release_input();
                toolbar_view.show_menu(&toolbar::menu(action, &toolbar_view.state()), rect);
            }
            Action::Files => {
                self.disable_input();
                toolbar_view.show_menu(&toolbar::menu(action, &toolbar_view.state()), rect);
                self.ivars().control.set_input_enabled(true);
            }
            Action::Recording => self.ivars().control.toggle_recording(),
            Action::SecureAttention => {
                self.release_input();
                self.ivars().control.send_secure_attention();
            }
            Action::TypeClipboard => {
                self.release_input();
                self.ivars()
                    .control
                    .type_clipboard(self.ivars().active_display.borrow().id);
            }
            Action::Chat => {
                self.disable_input();
                let anchor = toolbar_view.convertRect_toView(
                    NSRect::new(
                        NSPoint::new(rect.x, rect.y),
                        NSSize::new(rect.width, rect.height),
                    ),
                    Some(self),
                );
                if let Some(popup) = self.ivars().chat_popup.borrow().as_ref() {
                    popup.toggle(anchor);
                }
            }
            Action::Diagnostics => self.toggle_debug(),
            Action::Settings => self.show_session_controls(rect),
            // The window's own buttons stand in for the caption buttons.
            Action::Minimize | Action::Maximize | Action::Close => {}
        }
        self.refresh_toolbar();
    }

    /// A choice from a toolbar menu.
    pub(super) fn toolbar_command(&self, command: Command) {
        match command {
            Command::Session(index) => {
                let target = {
                    let displays = self.ivars().displays.borrow();
                    let active = self.ivars().active_display.borrow();
                    Display::sessions(&displays)
                        .get(index)
                        .filter(|session| **session != active.session)
                        .and_then(|session| {
                            displays
                                .iter()
                                .find(|d| &d.session == session && d.primary)
                                .or_else(|| displays.iter().find(|d| &d.session == session))
                                .map(|display| display.id)
                        })
                };
                // The selection changes once the agent confirms its stream.
                if let Some(display_id) = target {
                    self.release_input();
                    self.send(SessionMessage::SelectDisplay { display_id });
                }
            }
            Command::Display(index) => {
                let target = {
                    let displays = self.ivars().displays.borrow();
                    let active = self.ivars().active_display.borrow();
                    active
                        .session_displays(&displays)
                        .get(index)
                        .map(|display| display.id)
                        .filter(|id| *id != active.id)
                };
                if let Some(display_id) = target {
                    self.send(SessionMessage::SelectDisplay { display_id });
                }
            }
            Command::Quality(preset) => self.send(SessionMessage::SetQuality { preset }),
            // The macOS viewer decodes 4:2:0 only and does not offer the choice.
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
            Command::SendFiles => self.ivars().control.files().pick(),
            Command::ReceiveFiles => self.ivars().control.files().request_peer_pick(),
        }
        self.refresh_toolbar();
    }

    /// The session controls, which the toolbar's settings item opens: labeled
    /// sections of checkmarked toggles, so each item reads the same whatever
    /// its state.
    fn show_session_controls(&self, anchor: toolbar::Rect) {
        self.release_input();
        let control = &self.ivars().control;
        let maintenance = control.maintenance_state();
        let menu = NSMenu::new(self.mtm());
        menu.setAutoenablesItems(false);

        self.add_menu_header(&menu, "Session");
        menu.addItem(&self.menu_item(
            if control.recording().active() {
                "Stop recording and save"
            } else {
                "Record video to Downloads"
            },
            sel!(toggleRecording:),
            None,
        ));
        menu.addItem(&self.menu_item(
            "Play remote audio",
            sel!(toggleAudio:),
            Some(!control.audio_muted()),
        ));
        let view_only = self.menu_item(
            "View only",
            sel!(toggleTechnicianInput:),
            Some(control.technician_blocked()),
        );
        view_only.setToolTip(Some(&NSString::from_str(
            "Stop sending your keyboard and mouse to the remote computer.",
        )));
        menu.addItem(&view_only);
        menu.addItem(&self.menu_item(
            "Sync clipboard",
            sel!(toggleClipboardSync:),
            Some(control.clipboard_sync()),
        ));
        let idle_minutes = control.idle_disconnect_minutes();
        let idle_choices: Vec<_> = crate::idle_disconnect::choices()
            .map(|choice| {
                (
                    crate::idle_disconnect::label(choice),
                    choice == idle_minutes,
                )
            })
            .collect();
        let idle_disconnect = self.menu_choices(
            &format!(
                "Disconnect when idle: {}{}",
                crate::idle_disconnect::label(idle_minutes),
                if control.allow_idle_disconnect_override() {
                    ""
                } else {
                    " (company managed)"
                }
            ),
            idle_choices
                .iter()
                .map(|(label, selected)| (label.as_str(), *selected)),
            sel!(selectIdleDisconnect:),
        );
        idle_disconnect.setEnabled(control.allow_idle_disconnect_override());
        menu.addItem(&idle_disconnect);

        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        self.add_menu_header(&menu, "Remote computer");
        let agent_input = self.menu_item(
            "Block user's keyboard and mouse",
            sel!(toggleAgentInput:),
            Some(control.agent_blocked()),
        );
        agent_input.setEnabled(maintenance.available && !maintenance.blacked_out);
        menu.addItem(&agent_input);
        let blackout = self.menu_item(
            "Black out screens",
            sel!(toggleBlackout:),
            Some(maintenance.blacked_out),
        );
        blackout.setEnabled(maintenance.available);
        menu.addItem(&blackout);
        let idle = self.menu_item(
            if control.allow_idle_override() {
                "Prevent idle lock"
            } else {
                "Prevent idle lock (company managed)"
            },
            sel!(togglePreventIdleLock:),
            Some(control.prevent_idle_lock()),
        );
        idle.setEnabled(control.allow_idle_override());
        menu.addItem(&idle);
        menu.addItem(&self.menu_item(
            "Highlight the monitor I'm viewing",
            sel!(toggleDisplayBorder:),
            Some(control.display_border()),
        ));
        menu.addItem(&self.menu_item(
            "Hide wallpaper",
            sel!(toggleWallpaper:),
            Some(control.wallpaper_hidden()),
        ));

        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        self.add_menu_header(&menu, "This viewer");
        menu.addItem(&self.menu_item(
            "Show remote cursor",
            sel!(toggleRemoteCursor:),
            Some(control.show_remote_cursor()),
        ));
        menu.addItem(&self.menu_item(
            "Command key sends Ctrl",
            sel!(toggleCommandAsControl:),
            Some(control.command_as_control()),
        ));
        let diagnostics_key = control.shortcut_key(crate::shortcuts::ViewerShortcut::Diagnostics);
        menu.addItem(&self.menu_choices(
            &format!(
                "Diagnostics shortcut: {}",
                diagnostics_key_title(diagnostics_key)
            ),
            crate::shortcuts::ShortcutKey::ALL.map(|key| (key.label(), key == diagnostics_key)),
            sel!(selectDiagnosticsKey:),
        ));

        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        self.add_menu_header(&menu, "When the session ends");
        menu.addItem(&self.menu_item(
            "Ask before disconnecting",
            sel!(toggleDisconnectConfirmation:),
            Some(control.disconnect_confirmation()),
        ));
        let clear_clipboard = self.menu_item(
            if control.allow_clear_clipboard_override() {
                "Clear remote clipboard"
            } else {
                "Clear remote clipboard (company managed)"
            },
            sel!(toggleClearClipboardOnClose:),
            Some(control.clear_clipboard_on_close()),
        );
        clear_clipboard.setEnabled(control.allow_clear_clipboard_override());
        menu.addItem(&clear_clipboard);
        let close_action = control.session_close_action();
        menu.addItem(
            &self.menu_choices(
                &format!("Remote user: {}", close_action_label(close_action)),
                SessionCloseAction::ALL
                    .map(|choice| (close_action_label(choice), choice == close_action)),
                sel!(selectSessionCloseAction:),
            ),
        );

        if let Some(toolbar) = self.ivars().toolbar.borrow().clone() {
            toolbar.pop_up(&menu, anchor);
        }
    }

    /// A session-controls item sending `action` to this view, checkmarked
    /// when `checked` is `Some(true)`.
    fn menu_item(&self, title: &str, action: Sel, checked: Option<bool>) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(title),
                Some(action),
                &NSString::new(),
            )
        };
        unsafe {
            item.setTarget(Some(self));
        }
        if let Some(checked) = checked {
            item.setState(isize::from(checked));
        }
        item
    }

    /// An item titled `title` whose submenu offers `choices`, each sending
    /// `action` with its index as the tag.
    fn menu_choices<'a>(
        &self,
        title: &str,
        choices: impl IntoIterator<Item = (&'a str, bool)>,
        action: Sel,
    ) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(self.mtm());
        item.setTitle(&NSString::from_str(title));
        let submenu = NSMenu::new(self.mtm());
        for (index, (label, selected)) in choices.into_iter().enumerate() {
            let choice = self.menu_item(label, action, Some(selected));
            choice.setTag(index as isize);
            submenu.addItem(&choice);
        }
        item.setSubmenu(Some(&submenu));
        item
    }

    /// Adds a section title: a native section header on macOS 14 and later,
    /// otherwise a disabled item, which AppKit draws dimmed.
    fn add_menu_header(&self, menu: &NSMenu, title: &str) {
        let title = NSString::from_str(title);
        let native: bool = unsafe {
            msg_send![
                NSMenuItem::class(),
                respondsToSelector: sel!(sectionHeaderWithTitle:)
            ]
        };
        let item = if native {
            NSMenuItem::sectionHeaderWithTitle(&title, self.mtm())
        } else {
            let item = NSMenuItem::new(self.mtm());
            item.setTitle(&title);
            item.setEnabled(false);
            item
        };
        menu.addItem(&item);
    }

    fn send(&self, message: SessionMessage) {
        self.ivars().control.send(message);
    }

    /// Shows why and for how long the connection is being restored, or
    /// hides that (`None`); input waits until then. Returns whether the
    /// caller should start refreshing the elapsed time and countdown.
    pub(super) fn set_reconnect_status(&self, status: Option<ReconnectStatus>) -> bool {
        self.ivars().reconnect_status.set(status);
        self.refresh_reconnect_status();
        status.is_some() && !self.ivars().reconnect_ticking.replace(status.is_some())
    }

    /// Redraws the reconnect panel for the current time. Returns whether it
    /// is still shown, so the refresh continues.
    pub(super) fn refresh_reconnect_status(&self) -> bool {
        let panel = &self.ivars().reconnect_panel;
        let Some(status) = self.ivars().reconnect_status.get() else {
            panel.panel.setHidden(true);
            self.ivars().reconnect_ticking.set(false);
            return false;
        };
        let text = status.render(Instant::now());
        panel.title.setStringValue(&NSString::from_str(text.title));
        panel
            .detail
            .setStringValue(&NSString::from_str(&text.detail));
        panel.retry.setEnabled(text.retry_enabled);
        panel.panel.setHidden(false);
        true
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
        mac_cursor(
            self.ivars()
                .control
                .effective_cursor_shape(*self.ivars().cursor_shape.borrow()),
        )
        .set();
    }

    fn send_pointer(&self, event: &NSEvent) {
        let Some((x, y)) = self.pointer_position(event) else {
            return;
        };
        self.send(SessionMessage::Input(RemoteInput::PointerMove {
            display_id: self.ivars().active_display.borrow().id,
            x,
            y,
        }));
    }

    fn pointer_position(&self, event: &NSEvent) -> Option<(u16, u16)> {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let mut bounds = self.bounds();
        bounds.size.height = (bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
        normalized_video_position(
            point,
            bounds,
            self.ivars().video_width.get(),
            self.ivars().video_height.get(),
        )
    }

    fn send_button(&self, event: &NSEvent, button: PointerButton, pressed: bool) {
        let position = self.pointer_position(event);
        if pressed && position.is_some() {
            // Cmd-click and Ctrl-click use the held modifier.
            self.sync_modifiers(event.modifierFlags());
            self.engage_command();
        }
        let display_id = self.ivars().active_display.borrow().id;
        let input = self
            .ivars()
            .held
            .borrow_mut()
            .button(display_id, position, button, pressed);
        if let Some(input) = input {
            self.send(SessionMessage::Input(input));
        }
    }

    fn sync_modifiers(&self, flags: NSEventModifierFlags) {
        let keys = self.ivars().keyboard.borrow_mut().sync(flags.0 as u64);
        self.send_keys(keys);
    }

    /// Sends any held Command key before an action that uses it.
    fn engage_command(&self) {
        let keys = self.ivars().keyboard.borrow_mut().engage_command();
        self.send_keys(keys);
    }

    fn send_key(&self, key_code: u16, pressed: bool) {
        let iso = matches!(key_code, 10 | 50) && keyboard::keyboard_is_iso();
        let Some((scan_code, extended)) = keyboard::scan_code(key_code, iso) else {
            return;
        };
        self.send_scan_code(scan_code, extended, pressed);
    }

    fn send_keys(&self, keys: Vec<RemoteKey>) {
        for key in keys {
            self.send_scan_code(key.scan_code, key.extended, key.pressed);
        }
    }

    fn send_scan_code(&self, scan_code: u16, extended: bool, pressed: bool) {
        let display_id = self.ivars().active_display.borrow().id;
        let input = self
            .ivars()
            .held
            .borrow_mut()
            .key(display_id, scan_code, extended, pressed);
        self.send(SessionMessage::Input(input));
    }

    fn select_adjacent(&self, next: bool) {
        let all_displays = self.ivars().displays.borrow();
        let active = self.ivars().active_display.borrow();
        let displays = active.session_displays(&all_displays);
        if displays.len() < 2 {
            return;
        }
        let current = displays
            .iter()
            .position(|display| display.id == self.ivars().active_display.borrow().id)
            .unwrap_or(0);
        let selected = if next {
            (current + 1) % displays.len()
        } else {
            (current + displays.len() - 1) % displays.len()
        };
        self.send(SessionMessage::SelectDisplay {
            display_id: displays[selected].id,
        });
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
        }
        *self.ivars().active_display.borrow_mut() = display;
        *self.ivars().displays.borrow_mut() = displays;
        self.refresh_toolbar();
        self.ivars().video_width.set(width);
        self.ivars().video_height.set(height);
    }

    pub(super) fn release_input(&self) {
        self.ivars().keyboard.borrow_mut().reset();
        let display_id = self.ivars().active_display.borrow().id;
        let released = self.ivars().held.borrow_mut().release_all(display_id);
        for input in released {
            self.send(SessionMessage::Input(input));
        }
    }

    pub(super) fn disable_input(&self) {
        self.release_input();
        self.ivars().control.set_input_enabled(false);
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

/// The session-close choice as the session controls name it.
fn close_action_label(action: SessionCloseAction) -> &'static str {
    match action {
        SessionCloseAction::NoAction => "Leave signed in",
        SessionCloseAction::Lock => "Lock",
        SessionCloseAction::Logout => "Sign out",
    }
}

/// The diagnostics key for the session controls' item title.
fn diagnostics_key_title(key: crate::shortcuts::ShortcutKey) -> &'static str {
    match key {
        crate::shortcuts::ShortcutKey::Off => "Off",
        key => key.label(),
    }
}

fn command_key(control: &ControlSink) -> CommandKey {
    if control.command_as_control() {
        CommandKey::Control
    } else {
        CommandKey::Windows
    }
}

thread_local! {
    static CONNECTING_WINDOW: RefCell<Option<ConnectingWindow>> = const { RefCell::new(None) };
    static PENDING_ALERTS: RefCell<AlertQueue> = const { RefCell::new(AlertQueue::new()) };
}

/// Informational alerts raised while presenter state is borrowed. `runModal`
/// pumps the main queue, whose blocks borrow that state, so alerts are shown
/// one at a time from a separate main-queue block instead.
struct AlertQueue {
    pending: VecDeque<(&'static str, String)>,
    scheduled: bool,
}

impl AlertQueue {
    const fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            scheduled: false,
        }
    }

    /// Returns whether the caller must schedule a presentation block.
    fn push(&mut self, title: &'static str, message: String) -> bool {
        self.pending.push_back((title, message));
        !std::mem::replace(&mut self.scheduled, true)
    }

    /// Alerts raised while one is open are shown after it by the same block.
    fn next(&mut self) -> Option<(&'static str, String)> {
        let next = self.pending.pop_front();
        self.scheduled = next.is_some();
        next
    }
}

fn queue_alert(title: &'static str, message: String) {
    if PENDING_ALERTS.with(|alerts| alerts.borrow_mut().push(title, message)) {
        DispatchQueue::main().exec_async(present_queued_alerts);
    }
}

fn present_queued_alerts() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    while let Some((title, message)) = PENDING_ALERTS.with(|alerts| alerts.borrow_mut().next()) {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(&message));
        alert.runModal();
    }
}

pub(super) fn activate_application(mtm: MainThreadMarker) {
    tracing::info!("bringing the macOS viewer application to the foreground");
    let application = NSApplication::sharedApplication(mtm);
    // `activate` is newer than the MVP's macOS 12 deployment target.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
}

/// Ends the network session, then the viewer once it is done; exits anyway
/// if the session does not end in time. Used by Quit and Cancel.
fn end_running_session(reason: &'static str) {
    crate::shutdown::request(reason);
    end_after(REPLACEMENT_TIMEOUT, "the session did not end in time");
}

define_class!(
    // Safety: NSObject has no subclassing requirements, and the controller
    // stays on the main thread and does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct ConnectingWindowController;

    // Safety: these protocols have no additional safety requirements.
    unsafe impl NSObjectProtocol for ConnectingWindowController {}

    unsafe impl NSWindowDelegate for ConnectingWindowController {
        /// The close box cancels; the launch closes the window itself.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _window: &NSWindow) -> bool {
            cancel_connection();
            false
        }
    }

    impl ConnectingWindowController {
        #[unsafe(method(cancelConnection:))]
        fn cancel_connection_action(&self, _sender: Option<&AnyObject>) {
            cancel_connection();
        }
    }
);

impl ConnectingWindowController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // Safety: this invokes NSObject's parameterless initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Shown from launch until the first remote display window replaces it. Its
/// label says what the launch is waiting on.
struct ConnectingWindow {
    window: Retained<NSWindow>,
    status: Retained<NSTextField>,
    cancel: Retained<NSButton>,
    /// The Cancel button's target and the window's delegate, which AppKit
    /// holds weakly.
    _controller: Retained<ConnectingWindowController>,
    cancelling: bool,
}

/// Cancel, Esc, or the close box: end the launch without an error.
fn cancel_connection() {
    let first = CONNECTING_WINDOW.with(|state| {
        let mut state = state.borrow_mut();
        let Some(connecting) = state.as_mut() else {
            return false;
        };
        if connecting.cancelling || !connecting.cancel.isEnabled() {
            return false;
        }
        connecting.cancelling = true;
        connecting
            .status
            .setStringValue(&NSString::from_str("Cancelling…"));
        connecting.cancel.setEnabled(false);
        true
    });
    if first {
        end_running_session("the user cancelled the connection");
    }
}

fn show_connecting_window(mtm: MainThreadMarker) -> anyhow::Result<()> {
    tracing::info!("showing macOS viewer connecting window");
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 460.0,
            height: 180.0,
        },
    };
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("MeshRMM Remote"));
    let view = window
        .contentView()
        .context("AppKit connecting window has no content view")?;

    let spinner = NSProgressIndicator::new(mtm);
    spinner.setStyle(NSProgressIndicatorStyle::Spinning);
    spinner.setIndeterminate(true);
    spinner.setDisplayedWhenStopped(true);
    spinner.setFrame(NSRect {
        origin: NSPoint { x: 218.0, y: 128.0 },
        size: NSSize {
            width: 24.0,
            height: 24.0,
        },
    });
    unsafe { spinner.startAnimation(None) };
    view.addSubview(&spinner);

    // Two lines fit the longest status, an update with its restart notice.
    let status = NSTextField::wrappingLabelWithString(
        &NSString::from_str("Connecting to the remote computer…"),
        mtm,
    );
    status.setAlignment(NSTextAlignment::Center);
    status.setFrame(NSRect {
        origin: NSPoint { x: 30.0, y: 70.0 },
        size: NSSize {
            width: 400.0,
            height: 44.0,
        },
    });
    view.addSubview(&status);

    let controller = ConnectingWindowController::new(mtm);
    let cancel = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Cancel"),
            Some(&controller),
            Some(sel!(cancelConnection:)),
            mtm,
        )
    };
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    cancel.setFrame(NSRect {
        origin: NSPoint { x: 185.0, y: 18.0 },
        size: NSSize {
            width: 90.0,
            height: 32.0,
        },
    });
    view.addSubview(&cancel);
    window.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

    window.center();
    activate_application(mtm);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    CONNECTING_WINDOW.with(|state| {
        if let Some(old) = state.borrow_mut().replace(ConnectingWindow {
            window,
            status,
            cancel,
            _controller: controller,
            cancelling: false,
        }) {
            old.window.setDelegate(None);
            old.window.orderOut(None);
        }
    });
    Ok(())
}

/// Shows what the launch is waiting on, and whether it can be cancelled now.
/// Does nothing once the connecting window has closed. After Cancel the
/// window keeps saying so.
pub fn show_launch_status(message: String, cancellable: bool) {
    DispatchQueue::main().exec_async(move || {
        CONNECTING_WINDOW.with(|state| {
            if let Some(connecting) = state.borrow().as_ref()
                && !connecting.cancelling
            {
                connecting
                    .status
                    .setStringValue(&NSString::from_str(&message));
                connecting.cancel.setEnabled(cancellable);
            }
        });
    });
}

pub(super) fn close_connecting_window() {
    CONNECTING_WINDOW.with(|state| {
        if let Some(connecting) = state.borrow_mut().take() {
            tracing::info!("closing macOS viewer connecting window");
            connecting.window.setDelegate(None);
            connecting.window.orderOut(None);
        }
    });
}

/// Exits after `timeout` if the network session is still running then.
fn end_after(timeout: Duration, reason: &'static str) {
    let when = dispatch2::DispatchTime::try_from(timeout).unwrap_or(dispatch2::DispatchTime::NOW);
    let scheduled = DispatchQueue::main().after(when, move || {
        if SESSION_RUNNING.load(Ordering::Acquire) {
            tracing::warn!(reason, "exiting without the session's cleanup");
            std::process::exit(0);
        }
    });
    if scheduled.is_err() {
        tracing::warn!("could not schedule the viewer exit deadline");
    }
}

/// Shows a notice from the network thread, such as where a recording was
/// saved, and waits until the user dismisses it.
pub fn show_notice(title: &'static str, message: &str) {
    let message = message.to_owned();
    DispatchQueue::main().exec_sync(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            activate_application(mtm);
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(title));
            alert.setInformativeText(&NSString::from_str(&message));
            alert.runModal();
        }
    });
}

fn show_connection_error(mtm: MainThreadMarker, error: &str) {
    tracing::error!(%error, "showing macOS viewer connection error");
    close_connecting_window();
    activate_application(mtm);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Remote connection failed"));
    alert.setInformativeText(&NSString::from_str(error));
    alert.runModal();
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

pub fn run_application<F>(network: F) -> anyhow::Result<()>
where
    F: FnOnce(Option<String>) -> anyhow::Result<()> + Send + 'static,
{
    let mtm = MainThreadMarker::new().context("MeshRMM must start on the macOS main thread")?;
    let application = NSApplication::sharedApplication(mtm);
    let (deep_link_tx, deep_link_rx) = std::sync::mpsc::channel();
    let delegate = AppDelegate::new(mtm, deep_link_tx);
    application.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    application.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let menu = NSMenu::new(mtm);
    let item = NSMenuItem::new(mtm);
    let submenu = NSMenu::new(mtm);
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit MeshRMM Remote"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    unsafe {
        quit.setTarget(Some(&application));
    }
    submenu.addItem(&quit);
    item.setSubmenu(Some(&submenu));
    menu.addItem(&item);
    application.setMainMenu(Some(&menu));
    application.finishLaunching();
    show_connecting_window(mtm)?;

    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let has_command_line_session = std::env::args_os()
        .skip(1)
        .any(|argument| !argument.to_string_lossy().starts_with("-psn_"))
        || std::env::var_os("MESHRMM_HANDOFF_TOKEN").is_some();
    SESSION_RUNNING.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("meshrmm-network".into())
        .spawn(move || {
            let deep_link = receive_launch_link(deep_link_rx, has_command_line_session);
            let result = network(deep_link);
            match &result {
                Ok(()) => tracing::info!("macOS viewer network session finished cleanly"),
                Err(error) => {
                    tracing::error!(error = ?error, "macOS viewer network session failed")
                }
            }
            let error = result.as_ref().err().map(crate::errors::user_message);
            let _ = result_tx.send(result);
            DispatchQueue::main().exec_async(move || {
                SESSION_RUNNING.store(false, Ordering::Release);
                if let Some(mtm) = MainThreadMarker::new() {
                    // A replaced session starts its successor instead of
                    // reporting how its own cleanup went.
                    let replaced = launch_replacement();
                    match error.as_deref() {
                        Some(error) if !replaced => {
                            show_connection_error(mtm, error);
                            // A link opened while the error was shown.
                            launch_replacement();
                        }
                        _ => close_connecting_window(),
                    }
                    let application = NSApplication::sharedApplication(mtm);
                    application.stop(None);
                    // stop: changes the run-loop flag but does not wake an
                    // outstanding nextEvent wait. The network finishes on a
                    // dispatch callback, so enqueue a harmless event to let
                    // run() return even when the user provides no more input.
                    if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                        objc2_app_kit::NSEventType::ApplicationDefined,
                        NSPoint::new(0.0, 0.0),
                        NSEventModifierFlags::empty(),
                        0.0,
                        0,
                        None,
                        0,
                        0,
                        0,
                    ) {
                        application.postEvent_atStart(&event, true);
                    }
                }
            });
        })
        .context("failed to start macOS network runtime")?;

    activate_application(mtm);
    application.run();
    drop(delegate);
    result_rx
        .recv()
        .context("macOS network runtime exited without a result")?
}

fn receive_launch_link(
    receiver: std::sync::mpsc::Receiver<String>,
    command_line: bool,
) -> Option<String> {
    if command_line {
        None
    } else {
        receiver.recv_timeout(Duration::from_secs(5)).ok()
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    #[test]
    fn later_links_trigger_replacement_for_both_launch_modes() {
        for command_line in [false, true] {
            let (sender, receiver) = std::sync::mpsc::channel();
            sender.send("first".to_owned()).unwrap();
            let link = receive_launch_link(receiver, command_line);
            assert_eq!(link.is_some(), !command_line);
            assert!(sender.send("second".to_owned()).is_err());
        }
    }
}

#[cfg(test)]
mod alert_tests {
    use super::*;

    #[test]
    fn alerts_raised_while_one_is_pending_share_one_presentation_block() {
        let mut alerts = AlertQueue::new();
        assert!(alerts.push("first", "one".into()));
        assert!(!alerts.push("second", "two".into()));
        assert_eq!(alerts.next(), Some(("first", "one".into())));
        // Raised while the first modal pumps the main queue.
        assert!(!alerts.push("third", "three".into()));
        assert_eq!(alerts.next(), Some(("second", "two".into())));
        assert_eq!(alerts.next(), Some(("third", "three".into())));
        assert_eq!(alerts.next(), None);
        assert!(alerts.push("fourth", "four".into()));
    }
}
