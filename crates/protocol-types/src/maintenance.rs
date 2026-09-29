/// Company templates use this literal placeholder for the authenticated viewer.
pub const DEFAULT_BLACKOUT_MESSAGE: &str = "This machine is under maintenance by {user_name}.";
pub const MAX_BLACKOUT_MESSAGE_BYTES: usize = 2048;
/// Shown to the Agent's user when a technician connects. It uses the same
/// placeholder as the blackout message, but fits a small popup.
pub const DEFAULT_CONNECTION_NOTIFICATION_MESSAGE: &str =
    "{user_name} has connected to this computer.";
pub const MAX_CONNECTION_NOTIFICATION_MESSAGE_BYTES: usize = 512;
/// Asks the Agent's user to accept a technician's connection. It uses the same
/// placeholder and limit as the connection notification.
pub const DEFAULT_CONNECTION_APPROVAL_MESSAGE: &str = "{user_name} would like to connect.";
pub const MAX_CONNECTION_APPROVAL_MESSAGE_BYTES: usize = 512;
/// How long the user has to answer before the connection is accepted for them.
pub const DEFAULT_CONNECTION_APPROVAL_TIMEOUT_SECONDS: u32 = 30;
pub const MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS: u32 = 5;
pub const MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS: u32 = 300;
/// A computer at the lock screen with no input for this long accepts at once,
/// since nobody is there to answer. Zero accepts whenever it is locked.
pub const DEFAULT_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS: u32 = 60;
pub const MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS: u32 = 3600;
/// The technician's optional reason for connecting, shown in the approval prompt.
pub const MAX_CONNECTION_REASON_BYTES: usize = 500;

fn valid_template(message: &str, max_bytes: usize) -> bool {
    !message.trim().is_empty()
        && message.len() <= max_bytes
        && !message.chars().any(|c| c.is_control() && c != '\n')
}

pub fn valid_blackout_message(message: &str) -> bool {
    valid_template(message, MAX_BLACKOUT_MESSAGE_BYTES)
}

pub fn valid_connection_notification_message(message: &str) -> bool {
    valid_template(message, MAX_CONNECTION_NOTIFICATION_MESSAGE_BYTES)
}

pub fn valid_connection_approval_message(message: &str) -> bool {
    valid_template(message, MAX_CONNECTION_APPROVAL_MESSAGE_BYTES)
}

pub fn valid_connection_approval_timeout(seconds: u32) -> bool {
    (MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS..=MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS)
        .contains(&seconds)
}

pub fn valid_connection_approval_lock_idle(seconds: u32) -> bool {
    seconds <= MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS
}

/// An empty reason is valid: the prompt then shows only the message.
pub fn valid_connection_reason(reason: &str) -> bool {
    reason.len() <= MAX_CONNECTION_REASON_BYTES
        && !reason.chars().any(|c| c.is_control() && c != '\n')
}

pub fn session_viewer_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(256).collect();
    if name.trim().is_empty() {
        "Remote user".into()
    } else {
        name
    }
}

pub fn render_blackout_message(template: &str, viewer_name: &str) -> String {
    let template = if valid_blackout_message(template) {
        template
    } else {
        DEFAULT_BLACKOUT_MESSAGE
    };
    render_template(template, viewer_name)
}

pub fn render_connection_notification(template: &str, viewer_name: &str) -> String {
    let template = if valid_connection_notification_message(template) {
        template
    } else {
        DEFAULT_CONNECTION_NOTIFICATION_MESSAGE
    };
    render_template(template, viewer_name)
}

pub fn render_connection_approval_message(template: &str, viewer_name: &str) -> String {
    let template = if valid_connection_approval_message(template) {
        template
    } else {
        DEFAULT_CONNECTION_APPROVAL_MESSAGE
    };
    render_template(template, viewer_name)
}

/// The reason as the prompt shows it: trimmed, or empty when it is invalid.
pub fn connection_reason(reason: &str) -> &str {
    if valid_connection_reason(reason) {
        reason.trim()
    } else {
        ""
    }
}

fn render_template(template: &str, viewer_name: &str) -> String {
    let mut rendered = template.replace("{user_name}", &session_viewer_name(viewer_name));
    let mut end = rendered.len().min(8192);
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    rendered.truncate(end);
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn templates_render_names_literally_and_fall_back_for_old_sessions() {
        assert_eq!(
            render_blackout_message("", "Zoë 王"),
            "This machine is under maintenance by Zoë 王."
        );
        assert_eq!(
            render_blackout_message("Hi {user_name}\nPlease wait", "\n"),
            "Hi Remote user\nPlease wait"
        );
        assert_eq!(
            render_blackout_message("{user_name}", "{user_name}"),
            "{user_name}"
        );
        assert!(!valid_blackout_message("bad\0text"));
        assert!(!valid_blackout_message(&"é".repeat(1025)));
        assert!(valid_blackout_message(&"é".repeat(1024)));
    }

    #[test]
    fn connection_notifications_render_like_blackout_messages_with_a_smaller_limit() {
        assert_eq!(
            render_connection_notification("", "Zoë 王"),
            "Zoë 王 has connected to this computer."
        );
        assert_eq!(
            render_connection_notification("{user_name} is here\nSay hi", "{user_name}"),
            "{user_name} is here\nSay hi"
        );
        assert_eq!(
            render_connection_notification("bad\ttext", "Ada"),
            "Ada has connected to this computer."
        );
        assert!(valid_connection_notification_message(&"é".repeat(256)));
        assert!(!valid_connection_notification_message(&"é".repeat(257)));
        assert!(!valid_connection_notification_message(" \n "));
    }

    #[test]
    fn approval_prompts_render_their_own_default_and_limits() {
        assert_eq!(
            render_connection_approval_message("", "Zoë 王"),
            "Zoë 王 would like to connect."
        );
        assert_eq!(
            render_connection_approval_message("{user_name} asks\nOK?", "Ada"),
            "Ada asks\nOK?"
        );
        assert!(valid_connection_approval_message(&"é".repeat(256)));
        assert!(!valid_connection_approval_message(&"é".repeat(257)));
        for (seconds, valid) in [(4, false), (5, true), (300, true), (301, false)] {
            assert_eq!(
                valid_connection_approval_timeout(seconds),
                valid,
                "{seconds}"
            );
        }
        assert!(valid_connection_approval_lock_idle(0));
        assert!(valid_connection_approval_lock_idle(3600));
        assert!(!valid_connection_approval_lock_idle(3601));
    }

    #[test]
    fn reasons_are_optional_trimmed_and_bounded() {
        assert_eq!(connection_reason(""), "");
        assert_eq!(
            connection_reason("  Printer fix\nticket 42 "),
            "Printer fix\nticket 42"
        );
        assert!(valid_connection_reason(&"é".repeat(250)));
        assert!(!valid_connection_reason(&"é".repeat(251)));
        assert!(!valid_connection_reason("bad\ttext"));
        assert_eq!(connection_reason("bad\u{7}text"), "");
    }
}
