use super::keyboard::{self, CommandKey, Keyboard, RemoteKey};
use super::*;
use meshrmm_protocol::SessionCloseAction;
use objc2::runtime::AnyObject;
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
    user_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    display_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    recording_visible: std::cell::Cell<bool>,
    confirming_disconnect: std::cell::Cell<bool>,
    // A miniaturized window or hidden application also reports
    // `isVisible == false`, so only this flag means the session window closed.
    window_closed: std::cell::Cell<bool>,
    session_button: RefCell<Option<Retained<NSButton>>>,
    credential_buttons: RefCell<Vec<Retained<NSButton>>>,
    credential_label: RefCell<Option<Retained<NSTextField>>>,
    chat_popup: RefCell<Option<meshrmm_chat::ChatPopup>>,
    control: ControlSink,
    pressed_keys: RefCell<Vec<(u16, bool)>>,
    keyboard: RefCell<Keyboard>,
    key_up_monitor: RefCell<Option<Retained<AnyObject>>>,
    pressed_buttons: RefCell<Vec<PointerButton>>,
    wheel_normalizer: RefCell<WheelNormalizer>,
    cursor_shape: RefCell<CursorShape>,
    agent_pointer_display: std::cell::Cell<Option<meshrmm_protocol::DisplayId>>,
    debug: DebugInfo,
    debug_label: Retained<NSTextField>,
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
            if event.keyCode() == 111 {
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
            let keys = self
                .ivars()
                .keyboard
                .borrow_mut()
                .flags_changed(event.keyCode(), event.modifierFlags().0 as u64);
            self.send_keys(keys);
        }

        #[unsafe(method(selectDisplayFromToolbar:))]
        fn select_display_from_toolbar(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem();
            if index >= 0
                && let Some(display) = self.ivars().active_display.borrow().session_displays(&self.ivars().displays.borrow()).get(index as usize)
                && display.id != self.ivars().active_display.borrow().id
            {
                self.send(SessionMessage::SelectDisplay {
                    display_id: display.id,
                });
            }
        }

        #[unsafe(method(selectUserFromToolbar:))]
        fn select_user_from_toolbar(&self, sender: &NSPopUpButton) {
            let displays = self.ivars().displays.borrow();
            let sessions = Display::sessions(&displays);
            if let Some(session) = sessions.get(sender.indexOfSelectedItem() as usize)
                && *session != self.ivars().active_display.borrow().session
                && let Some(display) = displays.iter().find(|d| &d.session == session && d.primary)
                    .or_else(|| displays.iter().find(|d| &d.session == session)) {
                self.release_input();
                self.send(SessionMessage::SelectDisplay { display_id: display.id });
            }
            // Selection is committed only once the agent confirms its stream.
            self.refresh_display_selectors();
        }

        #[unsafe(method(showSessionControls:))]
        fn show_session_controls(&self, sender: &NSButton) {
            self.release_input();
            let menu = NSMenu::new(self.mtm());
            menu.setAutoenablesItems(false);
            for (title, action) in [
                (if self.ivars().control.technician_blocked() { "Allow technician input" } else { "Block technician input" }, sel!(toggleTechnicianInput:)),
                (if self.ivars().control.agent_blocked() { "Allow agent input" } else { "Block agent keyboard and mouse" }, sel!(toggleAgentInput:)),
                (if self.ivars().control.maintenance_state().blacked_out { "Restore agent monitors" } else { "Black out all agent monitors" }, sel!(toggleBlackout:)),
                (if self.ivars().control.audio_muted() { "Unmute audio" } else { "Mute audio" }, sel!(toggleAudio:)),
                (if self.ivars().control.allow_idle_override() { "Prevent idle lock" } else { "Prevent idle lock (company managed)" }, sel!(togglePreventIdleLock:)),
                ("Disconnect confirmation", sel!(toggleDisconnectConfirmation:)),
                ("Command key sends Ctrl", sel!(toggleCommandAsControl:)),
                ("Sync clipboard", sel!(toggleClipboardSync:)),
                ("Clear clipboard on session close", sel!(toggleClearClipboardOnClose:)),
                ("Highlight viewed monitor on agent", sel!(toggleDisplayBorder:)),
                ("Hide remote wallpaper", sel!(toggleWallpaper:)),
                ("Show remote cursor", sel!(toggleRemoteCursor:)),
                ("Diagnostics", sel!(toggleDiagnostics:)),
                (if self.ivars().control.recording().active() { "Stop recording and save" } else { "Record video to Downloads" }, sel!(toggleRecording:)),
            ] {
                let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm()), &NSString::from_str(title), Some(action), &NSString::from_str(""),
                ) };
                unsafe { item.setTarget(Some(self)); }
                if action == sel!(toggleAgentInput:) || action == sel!(toggleBlackout:) { item.setEnabled(self.ivars().control.maintenance_state().available); }
                if action == sel!(toggleAgentInput:) && self.ivars().control.maintenance_state().blacked_out { item.setEnabled(false); }
                if action == sel!(togglePreventIdleLock:) { item.setState(isize::from(self.ivars().control.prevent_idle_lock())); item.setEnabled(self.ivars().control.allow_idle_override()); }
                if action == sel!(toggleDisconnectConfirmation:) { item.setState(isize::from(self.ivars().control.disconnect_confirmation())); }
                if action == sel!(toggleCommandAsControl:) { item.setState(isize::from(self.ivars().control.command_as_control())); }
                if action == sel!(toggleClipboardSync:) { item.setState(isize::from(self.ivars().control.clipboard_sync())); }
                if action == sel!(toggleClearClipboardOnClose:) { item.setState(isize::from(self.ivars().control.clear_clipboard_on_close())); }
                if action == sel!(toggleDisplayBorder:) { item.setState(isize::from(self.ivars().control.display_border())); }
                if action == sel!(toggleWallpaper:) { item.setState(isize::from(self.ivars().control.wallpaper_hidden())); }
                if action == sel!(toggleRemoteCursor:) { item.setState(isize::from(self.ivars().control.show_remote_cursor())); }
                menu.addItem(&item);
                if action == sel!(toggleDisconnectConfirmation:) {
                    let close = NSMenuItem::new(self.mtm());
                    close.setTitle(&NSString::from_str("On session close"));
                    let choices = NSMenu::new(self.mtm());
                    let selected = self.ivars().control.session_close_action();
                    for (index, choice) in SessionCloseAction::ALL.into_iter().enumerate() {
                        let choice_item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(self.mtm()), &NSString::from_str(choice.label()), Some(sel!(selectSessionCloseAction:)), &NSString::new(),
                        ) };
                        unsafe { choice_item.setTarget(Some(self)); }
                        choice_item.setTag(index as isize);
                        choice_item.setState(isize::from(choice == selected));
                        choices.addItem(&choice_item);
                    }
                    close.setSubmenu(Some(&choices));
                    menu.addItem(&close);
                }
            }
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0., 0.), Some(sender));
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

        #[unsafe(method(selectQualityFromToolbar:))]
        fn select_quality_from_toolbar(&self, sender: &NSPopUpButton) {
            let preset = match sender.indexOfSelectedItem() {
                0 => QualityPreset::UltraDataSaver,
                1 => QualityPreset::DataSaver,
                3 => QualityPreset::BestQuality,
                _ => QualityPreset::Balanced,
            };
            self.send(SessionMessage::SetQuality { preset });
            if let Some(window) = self.window()
                && !window.makeFirstResponder(Some(self))
            {
                tracing::warn!(
                    "macOS viewer could not restore input focus after changing quality"
                );
            }
        }

        #[unsafe(method(showFiles:))]
        fn show_files(&self, sender: &NSButton) {
            self.disable_input();
            let menu = NSMenu::new(self.mtm());
            for (title, action) in [("Send", sel!(sendFiles:)), ("Receive", sel!(receiveFiles:))] {
                let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(title), Some(action), &NSString::new()) };
                unsafe { item.setTarget(Some(self)); } menu.addItem(&item);
            }
            let status = self.ivars().control.files().status();
            if !status.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
                let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(&status), None, &NSString::new()) };
                item.setEnabled(false); menu.addItem(&item);
            }
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0., 0.), Some(sender));
            self.ivars().control.set_input_enabled(true);
        }
        #[unsafe(method(sendFiles:))]
        fn send_files(&self, _: &NSMenuItem) { self.ivars().control.files().pick(); }
        #[unsafe(method(receiveFiles:))]
        fn receive_files(&self, _: &NSMenuItem) { self.send(SessionMessage::FileTransfer(meshrmm_protocol::FileMessage::Pick)); }

        #[unsafe(method(promptCredentials:))]
        fn prompt_credentials(&self, _: &NSButton) { self.release_input(); self.send(SessionMessage::PromptForCredentials); }
        #[unsafe(method(autofillCredentials:))]
        fn autofill_credentials(&self, _: &NSButton) { self.release_input(); self.send(SessionMessage::AutofillCredentials); }
        #[unsafe(method(forgetCredentials:))]
        fn forget_credentials(&self, _: &NSButton) { self.send(SessionMessage::ForgetCredentials); }

        #[unsafe(method(typeClipboard:))]
        fn type_clipboard(&self, _sender: &NSButton) {
            self.release_input();
            self.ivars().control.type_clipboard(self.ivars().active_display.borrow().id);
        }

        #[unsafe(method(sendSecureAttention:))]
        fn send_secure_attention(&self, _sender: &NSButton) {
            self.release_input();
            self.ivars().control.send_secure_attention();
        }

        #[unsafe(method(toggleChat:))]
        fn toggle_chat_action(&self, _sender: &NSButton) {
            self.disable_input();
            if let Some(popup) = self.ivars().chat_popup.borrow().as_ref() { popup.toggle(); }
        }

        #[unsafe(method(toggleDiagnostics:))]
        fn toggle_diagnostics_action(&self, _sender: &NSButton) {
            self.toggle_debug();
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
        let command = command_key(&control);
        let this = Self::alloc(mtm).set_ivars(RemoteViewIvars {
            active_display: RefCell::new(active_display),
            displays: RefCell::new(displays),
            video_width: std::cell::Cell::new(video_width),
            video_height: std::cell::Cell::new(video_height),
            user_popup: RefCell::new(None),
            display_popup: RefCell::new(None),
            chat_popup: RefCell::new(None),
            session_button: RefCell::new(None),
            credential_buttons: RefCell::new(Vec::new()),
            credential_label: RefCell::new(None),
            recording_visible: std::cell::Cell::new(false),
            confirming_disconnect: std::cell::Cell::new(false),
            window_closed: std::cell::Cell::new(false),
            control,
            pressed_keys: RefCell::new(Vec::new()),
            keyboard: RefCell::new(Keyboard::new(command)),
            key_up_monitor: RefCell::new(None),
            pressed_buttons: RefCell::new(Vec::new()),
            wheel_normalizer: RefCell::new(WheelNormalizer::default()),
            cursor_shape: RefCell::new(CursorShape::Default),
            agent_pointer_display: std::cell::Cell::new(None),
            debug,
            debug_label,
            debug_visible: RefCell::new(false),
            debug_refreshed: RefCell::new(Instant::now()),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.addSubview(&this.ivars().debug_label);
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
        if event.keyCode() == 111 {
            return;
        }
        self.send_key(event.keyCode(), false);
    }

    fn install_toolbar(&self, mtm: MainThreadMarker, frame: NSRect) {
        let toolbar = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect {
                origin: NSPoint {
                    x: 0.0,
                    y: (frame.size.height - VIEWER_TOOLBAR_HEIGHT).max(0.0),
                },
                size: NSSize {
                    width: frame.size.width,
                    height: VIEWER_TOOLBAR_HEIGHT,
                },
            },
        );
        toolbar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        toolbar.setWantsLayer(true);
        if let Some(layer) = toolbar.layer() {
            let background = NSColor::colorWithWhite_alpha(0.08, 0.96).CGColor();
            layer.setBackgroundColor(Some(&background));
        }

        let display_popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect {
                origin: NSPoint { x: 234.0, y: 6.0 },
                size: NSSize {
                    width: 110.0,
                    height: 24.0,
                },
            },
            false,
        );
        unsafe {
            display_popup.setTarget(Some(self));
            display_popup.setAction(Some(sel!(selectDisplayFromToolbar:)));
        }
        display_popup.setToolTip(Some(&NSString::from_str(
            "➤ marks the monitor with the agent-side mouse when the local user controls input.",
        )));
        toolbar.addSubview(&display_popup);
        *self.ivars().display_popup.borrow_mut() = Some(display_popup);

        let user_popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect::new(NSPoint::new(78., 6.), NSSize::new(150., 24.)),
            false,
        );
        user_popup.setToolTip(Some(&NSString::from_str("User session")));
        unsafe {
            user_popup.setTarget(Some(self));
            user_popup.setAction(Some(sel!(selectUserFromToolbar:)));
        }
        toolbar.addSubview(&user_popup);
        *self.ivars().user_popup.borrow_mut() = Some(user_popup);
        self.refresh_display_selectors();

        let quality_popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect {
                origin: NSPoint { x: 356.0, y: 6.0 },
                size: NSSize {
                    width: 154.0,
                    height: 24.0,
                },
            },
            false,
        );
        for title in ["Ultra data saver", "Data saver", "Balanced", "Best quality"] {
            quality_popup.addItemWithTitle(&NSString::from_str(title));
        }
        quality_popup.selectItemAtIndex(quality_index(self.ivars().control.quality_preset()));
        unsafe {
            quality_popup.setTarget(Some(self));
            quality_popup.setAction(Some(sel!(selectQualityFromToolbar:)));
        }
        toolbar.addSubview(&quality_popup);

        let diagnostics = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Session"),
                Some(self),
                Some(sel!(showSessionControls:)),
                mtm,
            )
        };
        diagnostics.setFrame(NSRect {
            origin: NSPoint { x: 516.0, y: 6.0 },
            size: NSSize {
                width: 88.0,
                height: 24.0,
            },
        });
        toolbar.addSubview(&diagnostics);
        *self.ivars().session_button.borrow_mut() = Some(diagnostics);
        self.registerForDraggedTypes(&NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]));
        let file_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("📁"),
                Some(self),
                Some(sel!(showFiles:)),
                self.mtm(),
            )
        };
        file_button.setFrame(NSRect::new(NSPoint::new(654., 6.), NSSize::new(40., 24.)));
        file_button.setToolTip(Some(&NSString::from_str(
            "Send or receive files and folders",
        )));
        toolbar.addSubview(&file_button);
        let chat_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Chat"),
                Some(self),
                Some(sel!(toggleChat:)),
                mtm,
            )
        };
        chat_button.setFrame(NSRect::new(NSPoint::new(610., 6.), NSSize::new(40., 24.)));
        toolbar.addSubview(&chat_button);
        let secure_attention_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Ctrl+Alt+Del"),
                Some(self),
                Some(sel!(sendSecureAttention:)),
                mtm,
            )
        };
        secure_attention_button
            .setFrame(NSRect::new(NSPoint::new(700., 6.), NSSize::new(110., 24.)));
        toolbar.addSubview(&secure_attention_button);
        let type_clipboard_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Type clipboard"),
                Some(self),
                Some(sel!(typeClipboard:)),
                mtm,
            )
        };
        type_clipboard_button.setFrame(NSRect::new(NSPoint::new(816., 6.), NSSize::new(120., 24.)));
        type_clipboard_button.setToolTip(Some(&NSString::from_str(
            "Type local clipboard text into the focused remote field",
        )));
        toolbar.addSubview(&type_clipboard_button);
        for (i, (title, action)) in [
            ("Prompt for credentials", sel!(promptCredentials:)),
            ("Autofill credentials?", sel!(autofillCredentials:)),
            ("Forget credentials", sel!(forgetCredentials:)),
        ]
        .into_iter()
        .enumerate()
        {
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    Some(self),
                    Some(action),
                    mtm,
                )
            };
            button.setFrame(NSRect::new(
                NSPoint::new(80. + i as f64 * 184., 40.),
                NSSize::new(180., 24.),
            ));
            button.setEnabled(false);
            toolbar.addSubview(&button);
            self.ivars().credential_buttons.borrow_mut().push(button);
        }
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(638., 42.),
            NSSize::new((frame.size.width - 646.).max(100.), 20.),
        ));
        label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        toolbar.addSubview(&label);
        *self.ivars().credential_label.borrow_mut() = Some(label);
        let control = self.ivars().control.clone();
        *self.ivars().chat_popup.borrow_mut() = Some(meshrmm_chat::ChatPopup::new(
            control.chat(),
            &chat_button,
            move |enabled| control.set_input_enabled(enabled),
        ));
        self.addSubview(&toolbar);
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
        if let Some(popup) = self.ivars().display_popup.borrow().as_ref() {
            for (index, display) in self
                .ivars()
                .active_display
                .borrow()
                .session_displays(&self.ivars().displays.borrow())
                .iter()
                .enumerate()
            {
                if let Some(item) = popup.itemAtIndex(index as isize) {
                    let title = if display_id == Some(display.id) {
                        format!("➤ {}", display.selection_label(index))
                    } else {
                        display.selection_label(index)
                    };
                    item.setTitle(&NSString::from_str(&title));
                }
            }
        }
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
        let mut buttons = self.ivars().pressed_buttons.borrow_mut();
        match position {
            Some((x, y)) => self.send(SessionMessage::Input(RemoteInput::PointerButtonAt {
                display_id: self.ivars().active_display.borrow().id,
                x,
                y,
                button,
                pressed,
            })),
            None if !pressed && buttons.contains(&button) => {
                // Finish a drag that began over the video without moving the
                // remote pointer to an out-of-bounds/clamped position.
                self.send(SessionMessage::Input(RemoteInput::PointerButton {
                    display_id: self.ivars().active_display.borrow().id,
                    button,
                    pressed: false,
                }));
            }
            None => return,
        }
        if pressed {
            if !buttons.contains(&button) {
                buttons.push(button);
            }
        } else {
            buttons.retain(|candidate| *candidate != button);
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
        self.send(SessionMessage::Input(RemoteInput::Key {
            display_id: self.ivars().active_display.borrow().id,
            scan_code,
            extended,
            pressed,
        }));
        let mut keys = self.ivars().pressed_keys.borrow_mut();
        if pressed {
            if !keys.contains(&(scan_code, extended)) {
                keys.push((scan_code, extended));
            }
        } else {
            keys.retain(|candidate| *candidate != (scan_code, extended));
        }
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

    fn toggle_debug(&self) {
        let visible = !*self.ivars().debug_visible.borrow();
        *self.ivars().debug_visible.borrow_mut() = visible;
        self.ivars().debug_label.setHidden(!visible);
        if visible {
            self.refresh_debug(true);
        }
    }

    pub(super) fn refresh_debug(&self, force: bool) {
        let state = self.ivars().control.credential_state();
        for (i, button) in self.ivars().credential_buttons.borrow().iter().enumerate() {
            button.setEnabled(
                !self.ivars().control.technician_blocked()
                    && match i {
                        0 => state.available && !state.prompt_active,
                        1 => state.can_autofill,
                        _ => state.saved && !state.prompt_active,
                    },
            );
            if i == 1 {
                button.setHidden(!state.can_autofill);
            }
        }
        if let Some(label) = self.ivars().credential_label.borrow().as_ref() {
            label.setStringValue(&NSString::from_str(&state.message));
            label.setToolTip(Some(&NSString::from_str(&state.message)));
        }
        if let Some(notice) = self.ivars().control.recording().take_notice() {
            queue_alert("Session recording", notice);
        }
        let recording = self.ivars().control.recording().active();
        if self.ivars().recording_visible.replace(recording) != recording
            && let Some(button) = self.ivars().session_button.borrow().as_ref()
        {
            button.setTitle(&NSString::from_str(if recording {
                "● REC"
            } else {
                "Controls"
            }));
        }
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

    fn refresh_display_selectors(&self) {
        let displays = self.ivars().displays.borrow();
        let active = self.ivars().active_display.borrow();
        if let Some(popup) = self.ivars().user_popup.borrow().as_ref() {
            popup.removeAllItems();
            let sessions = Display::sessions(&displays);
            for session in &sessions {
                popup.addItemWithTitle(&NSString::from_str(&session.label()));
            }
            popup.selectItemAtIndex(
                sessions
                    .iter()
                    .position(|s| *s == active.session)
                    .unwrap_or(0) as isize,
            );
        }
        if let Some(popup) = self.ivars().display_popup.borrow().as_ref() {
            popup.removeAllItems();
            let visible = active.session_displays(&displays);
            for (index, display) in visible.iter().enumerate() {
                popup.addItemWithTitle(&NSString::from_str(&display.selection_label(index)));
            }
            popup.selectItemAtIndex(
                visible.iter().position(|d| d.id == active.id).unwrap_or(0) as isize
            );
            popup.setEnabled(visible.len() > 1);
        }
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
        self.refresh_display_selectors();
        self.set_agent_pointer_display(self.ivars().agent_pointer_display.get());
        self.ivars().video_width.set(width);
        self.ivars().video_height.set(height);
    }

    pub(super) fn release_input(&self) {
        self.ivars().keyboard.borrow_mut().reset();
        for (scan_code, extended) in self.ivars().pressed_keys.take() {
            self.send(SessionMessage::Input(RemoteInput::Key {
                display_id: self.ivars().active_display.borrow().id,
                scan_code,
                extended,
                pressed: false,
            }));
        }
        for button in self.ivars().pressed_buttons.take() {
            self.send(SessionMessage::Input(RemoteInput::PointerButton {
                display_id: self.ivars().active_display.borrow().id,
                button,
                pressed: false,
            }));
        }
    }

    pub(super) fn disable_input(&self) {
        self.release_input();
        self.ivars().control.set_input_enabled(false);
    }
}

fn quality_index(preset: QualityPreset) -> isize {
    match preset {
        QualityPreset::UltraDataSaver => 0,
        QualityPreset::DataSaver => 1,
        QualityPreset::Balanced => 2,
        QualityPreset::BestQuality => 3,
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

fn command_key(control: &ControlSink) -> CommandKey {
    if control.command_as_control() {
        CommandKey::Control
    } else {
        CommandKey::Windows
    }
}

thread_local! {
    static CONNECTING_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
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

fn show_connecting_window(mtm: MainThreadMarker) -> anyhow::Result<()> {
    tracing::info!("showing macOS viewer connecting window");
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 420.0,
            height: 150.0,
        },
    };
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled,
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
        origin: NSPoint { x: 198.0, y: 82.0 },
        size: NSSize {
            width: 24.0,
            height: 24.0,
        },
    });
    unsafe { spinner.startAnimation(None) };
    view.addSubview(&spinner);

    let label = NSTextField::labelWithString(
        &NSString::from_str("Connecting to the remote computer…"),
        mtm,
    );
    label.setAlignment(NSTextAlignment::Center);
    label.setFrame(NSRect {
        origin: NSPoint { x: 30.0, y: 45.0 },
        size: NSSize {
            width: 360.0,
            height: 24.0,
        },
    });
    view.addSubview(&label);

    window.center();
    activate_application(mtm);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    CONNECTING_WINDOW.with(|state| {
        if let Some(old) = state.borrow_mut().replace(window) {
            old.orderOut(None);
        }
    });
    Ok(())
}

pub(super) fn close_connecting_window() {
    CONNECTING_WINDOW.with(|state| {
        if let Some(window) = state.borrow_mut().take() {
            tracing::info!("closing macOS viewer connecting window");
            window.orderOut(None);
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
            let error = result.as_ref().err().map(|error| format!("{error:#}"));
            let _ = result_tx.send(result);
            DispatchQueue::main().exec_async(move || {
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
