use meshrmm_protocol::ChromaMode;

use super::{Action, Badge, Icon, Item, State, Tone, quality_label};

/// The toolbar's items, from the left.
pub fn items(state: &State) -> Vec<Item> {
    let mut items = Vec::with_capacity(16);
    push_view_items(&mut items, state);
    if state.recording {
        let mut recording = Item::new(
            Action::Recording,
            Icon::Record,
            "Recording to Downloads. Click to stop and save.",
            1,
        );
        recording.label = Some("REC".into());
        recording.tone = Tone::Recording;
        items.push(recording);
    }
    push_remote_control_items(&mut items, state);
    push_tool_items(&mut items, state);
    push_viewer_items(&mut items, state);
    if let Some(maximized) = state.caption {
        push_caption_items(&mut items, maximized);
    }
    items
}

/// The leading user session, monitor and quality items.
fn push_view_items(items: &mut Vec<Item>, state: &State) {
    let session = state
        .sessions
        .get(state.session)
        .cloned()
        .unwrap_or_default();
    let mut user = Item::new(
        Action::User,
        Icon::User,
        format!("User session: {session}"),
        0,
    );
    user.trailing = false;
    user.menu = state.sessions.len() > 1;
    user.chevron = user.menu;
    user.enabled = user.menu;
    if state.sessions.len() > 1 {
        user.label = Some(session);
    }
    items.push(user);

    let display = state
        .displays
        .get(state.display)
        .cloned()
        .unwrap_or_default();
    let mut monitor = Item::new(
        Action::Display,
        Icon::Display,
        format!("Monitor: {display}"),
        0,
    );
    monitor.trailing = false;
    monitor.label = Some(short_display_label(&display, state.display));
    monitor.menu = state.displays.len() > 1;
    monitor.chevron = monitor.menu;
    monitor.enabled = monitor.menu;
    items.push(monitor);

    let mut quality_tip = format!("Quality: {}", quality_label(state.quality));
    if let Some((ChromaMode::Yuv444, _)) = state.chroma {
        quality_tip.push_str(", 4:4:4 color");
    }
    let mut quality = Item::new(Action::Quality, Icon::Quality, quality_tip, 0).with_menu();
    quality.trailing = false;
    items.push(quality);
}

/// Credentials, Ctrl+Alt+Del, restart and typing the clipboard.
fn push_remote_control_items(items: &mut Vec<Item>, state: &State) {
    let credentials = &state.credentials;
    let mut key = Item::new(
        Action::Credentials,
        Icon::Key,
        if credentials.message.is_empty() {
            "Credentials".to_owned()
        } else {
            format!("Credentials: {}", credentials.message)
        },
        2,
    )
    .with_menu();
    key.enabled = !state.input_blocked;
    key.active = credentials.prompt_active;
    key.badge = credentials.can_autofill.then_some(Badge::Attention);
    items.push(key);
    if !state.device_is_mac {
        let mut secure_attention = Item::new(
            Action::SecureAttention,
            Icon::Keyboard,
            "Send Ctrl+Alt+Del",
            2,
        );
        secure_attention.enabled = !state.input_blocked;
        items.push(secure_attention);
    }
    // Not input: view-only sessions can restart too.
    let mut power = Item::new(
        Action::Power,
        Icon::Power,
        match state.power {
            None => "Restart is unavailable until the agent connects",
            Some(false) if state.device_is_mac => "Restart the remote Mac",
            Some(false) => "Restart the remote computer",
            Some(true) => "Restart the remote computer, which is in Safe Mode",
        },
        2,
    )
    .with_menu();
    power.enabled = state.power.is_some();
    power.label = (state.power == Some(true)).then(|| "Safe Mode".to_owned());
    items.push(power);
    let mut type_clipboard = Item::new(
        Action::TypeClipboard,
        Icon::Clipboard,
        "Type clipboard text into the remote computer",
        2,
    );
    type_clipboard.enabled = !state.input_blocked;
    items.push(type_clipboard);
}

/// Annotation, files, the toolbox and chat.
fn push_tool_items(items: &mut Vec<Item>, state: &State) {
    // Not input: view-only sessions annotate too.
    let mut annotate = Item::new(
        Action::Annotate,
        Icon::Pen,
        if !state.annotation_available {
            "Annotations are unavailable on the background desktop"
        } else if state.annotating {
            "Stop annotating and erase the drawing"
        } else {
            "Annotate: drag to draw on the remote screen, right-click to erase"
        },
        3,
    );
    annotate.enabled = state.annotation_available;
    annotate.active = state.annotating;
    items.push(annotate);

    items.push(
        Item::new(
            Action::Files,
            Icon::Folder,
            if state.file_status.is_empty() {
                "Send or receive files".to_owned()
            } else {
                format!("Files: {}", state.file_status)
            },
            3,
        )
        .with_menu(),
    );
    let mut toolbox = Item::new(
        Action::Toolbox,
        Icon::Toolbox,
        if !state.toolbox_available {
            "The toolbox is unavailable until the session connects".to_owned()
        } else if state.toolbox_status.is_empty() {
            "Toolbox: run scripts and send files".to_owned()
        } else {
            format!("Toolbox: {}", state.toolbox_status)
        },
        3,
    )
    .with_menu();
    toolbox.enabled = state.toolbox_available;
    toolbox.active = state.toolbox_busy;
    items.push(toolbox);
    let mut chat = Item::new(
        Action::Chat,
        Icon::Chat,
        if !state.chat_available {
            "Chat is unavailable until the agent connects".to_owned()
        } else if state.chat_unread > 0 {
            format!(
                "Chat: {} unread {}",
                state.chat_unread,
                if state.chat_unread == 1 {
                    "message"
                } else {
                    "messages"
                }
            )
        } else {
            "Chat".to_owned()
        },
        3,
    );
    chat.enabled = state.chat_available;
    chat.badge = (state.chat_unread > 0).then_some(Badge::Unread);
    items.push(chat);
}

/// Diagnostics and settings.
fn push_viewer_items(items: &mut Vec<Item>, state: &State) {
    let mut diagnostics = Item::new(Action::Diagnostics, Icon::Pulse, "Diagnostics", 4);
    diagnostics.active = state.diagnostics;
    items.push(diagnostics);
    let mut settings = Item::new(
        Action::Settings,
        Icon::Gear,
        if state.settings_menu {
            "Session controls"
        } else {
            "Settings"
        },
        4,
    );
    settings.menu = state.settings_menu;
    items.push(settings);
}

/// The window's minimize, maximize or restore, and close buttons.
fn push_caption_items(items: &mut Vec<Item>, maximized: bool) {
    for (action, icon, tooltip, tone) in [
        (Action::Minimize, Icon::Minimize, "Minimize", Tone::Caption),
        (
            Action::Maximize,
            if maximized {
                Icon::Restore
            } else {
                Icon::Maximize
            },
            if maximized { "Restore" } else { "Maximize" },
            Tone::Caption,
        ),
        (Action::Close, Icon::Close, "Close", Tone::CloseCaption),
    ] {
        let mut caption = Item::new(action, icon, tooltip, 5);
        caption.tone = tone;
        items.push(caption);
    }
}

/// "1" for "Display 1", and the first word of other labels.
fn short_display_label(label: &str, index: usize) -> String {
    match label.strip_prefix("Display ") {
        Some(_) => (index + 1).to_string(),
        None => label
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}
