use super::*;
use crate::toolbar::{self, Action, Command};

impl RemoteView {
    /// What the toolbar shows now.
    fn toolbar_state(&self) -> toolbar::State {
        let displays = self.ivars().displays.borrow();
        let active = self.ivars().active_display.borrow();
        let sessions = Display::sessions(&displays);
        let visible = active.session_displays(&displays);
        let control = &self.ivars().control;
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
                .ivars()
                .agent_pointer_display
                .get()
                .and_then(|id| visible.iter().position(|display| display.id == id)),
            quality: control.quality_preset(),
            chroma: None,
            credentials: control.credential_state(),
            input_blocked: control.technician_blocked(),
            annotating: self.annotating(),
            annotation_available: active.session != meshrmm_protocol::DesktopSession::Background,
            chat_available: chat.available(),
            chat_unread: chat.unread(),
            power: control.power_state(),
            device_is_mac: control.device_is_mac(),
            file_status: control.files().status(),
            toolbox_available: toolbox.available,
            toolbox_busy: toolbox.busy,
            toolbox_status: toolbox.status,
            recording: control.recording().active(),
            diagnostics: *self.ivars().debug_visible.borrow(),
            settings_menu: true,
            caption: None,
        }
    }

    pub(super) fn refresh_toolbar(&self) {
        // The device's platform, which decides what Command sends, arrives
        // after the window opens.
        let command = command_key(&self.ivars().control);
        let released = {
            let mut keyboard = self.ivars().keyboard.borrow_mut();
            (keyboard.command_key() != command).then(|| keyboard.set_command_key(command))
        };
        if let Some(keys) = released {
            self.send_keys(keys);
        }
        let state = self.toolbar_state();
        if let Some(toolbar) = self.ivars().toolbar.borrow().as_ref() {
            toolbar.set_state(state);
        }
    }

    /// A click on the toolbar item for `action`, at `rect` in the toolbar.
    pub(in crate::platform::macos) fn toolbar_action(&self, action: Action, rect: toolbar::Rect) {
        let Some(toolbar_view) = self.ivars().toolbar.borrow().clone() else {
            return;
        };
        match action {
            Action::User
            | Action::Display
            | Action::Quality
            | Action::Credentials
            | Action::Power => {
                self.release_input();
                toolbar_view.show_menu(&toolbar::menu(action, &toolbar_view.state()), rect);
            }
            Action::Files => {
                self.disable_input();
                toolbar_view.show_menu(&toolbar::menu(action, &toolbar_view.state()), rect);
                self.ivars().control.set_input_enabled(true);
            }
            Action::Toolbox => {
                self.disable_input();
                let offered = self.ivars().control.toolbox().offer();
                toolbar_view.show_menu(&toolbar::toolbox_menu(&offered), rect);
                self.ivars().control.set_input_enabled(true);
            }
            Action::Recording => self.ivars().control.toggle_recording(),
            Action::Annotate => self.toggle_annotating(),
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
    pub(in crate::platform::macos) fn toolbar_command(&self, command: Command) {
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
            Command::Restart { safe_mode } => {
                if self.confirm_restart(safe_mode) {
                    self.ivars().control.restart(safe_mode);
                }
            }
            Command::RunScript { index, run_as } => {
                self.ivars().control.toolbox().run_script(index, run_as);
            }
            Command::SendToolboxFile(index) => {
                // The background desktop shows Public Documents as Documents.
                let background = self.ivars().active_display.borrow().session
                    == meshrmm_protocol::DesktopSession::Background;
                self.ivars().control.toolbox().send_file(index, background);
            }
            Command::RefreshToolbox => self.ivars().control.toolbox().refresh(),
        }
        self.refresh_toolbar();
    }

    /// Asks before restarting the remote computer. Input stays off while the
    /// alert is up, like the disconnect confirmation.
    fn confirm_restart(&self, safe_mode: bool) -> bool {
        self.disable_input();
        let (title, detail, button) = toolbar::restart_confirmation(safe_mode);
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(detail));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.addButtonWithTitle(&NSString::from_str(button));
        let confirmed = alert.runModal() == 1001;
        if self.window().is_some_and(|window| window.isKeyWindow()) {
            self.ivars().control.set_input_enabled(true);
        }
        confirmed
    }
}
