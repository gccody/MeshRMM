/// Company templates use this literal placeholder for the authenticated viewer.
pub const DEFAULT_BLACKOUT_MESSAGE: &str = "This machine is under maintenance by {user_name}.";
pub const MAX_BLACKOUT_MESSAGE_BYTES: usize = 2048;

pub fn valid_blackout_message(message: &str) -> bool {
    !message.trim().is_empty() && message.len() <= MAX_BLACKOUT_MESSAGE_BYTES
        && !message.chars().any(|c| c.is_control() && c != '\n')
}

pub fn session_viewer_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(256).collect();
    if name.trim().is_empty() { "Remote user".into() } else { name }
}

pub fn render_blackout_message(template: &str, viewer_name: &str) -> String {
    let template = if valid_blackout_message(template) { template } else { DEFAULT_BLACKOUT_MESSAGE };
    let mut rendered = template.replace("{user_name}", &session_viewer_name(viewer_name));
    let mut end = rendered.len().min(8192);
    while !rendered.is_char_boundary(end) { end -= 1; }
    rendered.truncate(end);
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn templates_render_names_literally_and_fall_back_for_old_sessions() {
        assert_eq!(render_blackout_message("", "Zoë 王"), "This machine is under maintenance by Zoë 王.");
        assert_eq!(render_blackout_message("Hi {user_name}\nPlease wait", "\n"), "Hi Remote user\nPlease wait");
        assert_eq!(render_blackout_message("{user_name}", "{user_name}"), "{user_name}");
        assert!(!valid_blackout_message("bad\0text"));
        assert!(!valid_blackout_message(&"é".repeat(1025)));
        assert!(valid_blackout_message(&"é".repeat(1024)));
    }
}
