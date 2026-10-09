use super::*;
use crate::toolbar;
use objc2::ClassType;
use objc2::runtime::Sel;
use objc2_app_kit::NSMenu;

impl RemoteView {
    /// The session controls, which the toolbar's settings item opens: labeled
    /// sections of checkmarked toggles, so each item reads the same whatever
    /// its state.
    pub(super) fn show_session_controls(&self, anchor: toolbar::Rect) {
        self.release_input();
        let menu = NSMenu::new(self.mtm());
        menu.setAutoenablesItems(false);
        self.add_session_section(&menu);
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        self.add_remote_computer_section(&menu);
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        self.add_viewer_section(&menu);
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        self.add_session_end_section(&menu);

        if let Some(toolbar) = self.ivars().toolbar.borrow().clone() {
            toolbar.pop_up(&menu, anchor);
        }
    }

    fn add_session_section(&self, menu: &NSMenu) {
        let control = &self.ivars().control;
        self.add_menu_header(menu, "Session");
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
    }

    fn add_remote_computer_section(&self, menu: &NSMenu) {
        let control = &self.ivars().control;
        let maintenance = control.maintenance_state();
        self.add_menu_header(menu, "Remote computer");
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
        let headless = crate::preferences::headless_resolution();
        let headless_labels = HeadlessResolution::PRESETS.map(HeadlessResolution::label);
        let headless_item = self.menu_choices(
            &format!("Size without a monitor: {}", headless.label()),
            headless_labels
                .iter()
                .zip(HeadlessResolution::PRESETS)
                .map(|(label, choice)| (label.as_str(), choice == headless)),
            sel!(selectHeadlessResolution:),
        );
        headless_item.setToolTip(Some(&NSString::from_str(
            "The size of the virtual display the remote computer shows when no monitor is connected to it.",
        )));
        menu.addItem(&headless_item);
    }

    fn add_viewer_section(&self, menu: &NSMenu) {
        let control = &self.ivars().control;
        self.add_menu_header(menu, "This viewer");
        menu.addItem(&self.menu_item(
            "Show remote cursor",
            sel!(toggleRemoteCursor:),
            Some(control.show_remote_cursor()),
        ));
        // A Mac's Command key is Command's own.
        if !control.device_is_mac() {
            menu.addItem(&self.menu_item(
                "Command key sends Ctrl",
                sel!(toggleCommandAsControl:),
                Some(control.command_as_control()),
            ));
        }
        let diagnostics_key = control.shortcut_key(crate::shortcuts::ViewerShortcut::Diagnostics);
        menu.addItem(&self.menu_choices(
            &format!(
                "Diagnostics shortcut: {}",
                diagnostics_key_title(diagnostics_key)
            ),
            crate::shortcuts::ShortcutKey::ALL.map(|key| (key.label(), key == diagnostics_key)),
            sel!(selectDiagnosticsKey:),
        ));
    }

    fn add_session_end_section(&self, menu: &NSMenu) {
        let control = &self.ivars().control;
        self.add_menu_header(menu, "When the session ends");
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
