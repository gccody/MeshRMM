//! Tells the Agent's user that a technician connected. Company policy decides
//! whether a session shows it, separately for sessions that view a user's
//! desktop and for sessions on the background desktop, and the viewer cannot
//! suppress it. It appears on the primary monitor once per remote session.
use std::sync::Mutex;

use meshrmm_protocol::{AgentSessionRequest, RemoteSessionId};

#[cfg(windows)]
mod window;
#[cfg(windows)]
pub use window::NotificationWindow;

/// The last session whose user saw its notification. A viewer resume restarts
/// the session with a new streamer, which must not notify the user again.
static NOTIFIED_SESSION: Mutex<Option<RemoteSessionId>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionNotification {
    session_id: RemoteSessionId,
    text: String,
    on_user_desktop: bool,
    on_background_desktop: bool,
}

impl ConnectionNotification {
    /// The rendered notification, when the company enables it for any of
    /// `request`'s desktops.
    pub fn for_request(request: &AgentSessionRequest) -> Option<Self> {
        (request.connection_notification || request.background_connection_notification).then(|| {
            Self {
                session_id: request.session_id.clone(),
                text: meshrmm_protocol::render_connection_notification(
                    &request.connection_notification_message,
                    &request.viewer_name,
                ),
                on_user_desktop: request.connection_notification,
                on_background_desktop: request.background_connection_notification,
            }
        })
    }

    /// Whether the company notifies the user when the technician views the
    /// background desktop, or otherwise a user's desktop. A session that is
    /// not allowed to notify yet may be later, after switching desktops.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn allowed(&self, background: bool) -> bool {
        if background {
            self.on_background_desktop
        } else {
            self.on_user_desktop
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether this session's user has yet to see the notification.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn pending(&self) -> bool {
        NOTIFIED_SESSION
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            != Some(&self.session_id)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn mark_shown(&self) {
        *NOTIFIED_SESSION
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(self.session_id.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(session_id: &str, enabled: bool) -> AgentSessionRequest {
        with_policy(session_id, enabled, false)
    }

    fn with_policy(session_id: &str, user: bool, background: bool) -> AgentSessionRequest {
        AgentSessionRequest {
            start_in_background: false,
            idle_policy: Default::default(),
            clear_clipboard_policy: Default::default(),
            blackout_message: String::new(),
            session_banner: true,
            connection_notification: user,
            background_connection_notification: background,
            connection_notification_message: "{user_name} joined\nSay hi".into(),
            connection_approval: None,
            connection_reason: String::new(),
            viewer_name: "Zoë 王".into(),
            session_id: RemoteSessionId::new(session_id),
            signaling_token: "token".into(),
            expires_at_unix_ms: 1,
            ice_servers: vec![],
        }
    }

    #[test]
    fn disabled_policy_shows_nothing_and_enabled_policy_renders_the_template() {
        assert_eq!(
            ConnectionNotification::for_request(&request("notification-off", false)),
            None
        );
        let notification =
            ConnectionNotification::for_request(&request("notification-on", true)).unwrap();
        assert_eq!(notification.text(), "Zoë 王 joined\nSay hi");
        let mut legacy = request("notification-legacy", true);
        legacy.connection_notification_message.clear();
        assert_eq!(
            ConnectionNotification::for_request(&legacy).unwrap().text(),
            "Zoë 王 has connected to this computer."
        );
    }

    #[test]
    fn each_desktop_follows_its_own_policy() {
        assert_eq!(
            ConnectionNotification::for_request(&with_policy("policy-none", false, false)),
            None
        );
        for (user, background) in [(true, false), (false, true), (true, true)] {
            let notification =
                ConnectionNotification::for_request(&with_policy("policy", user, background))
                    .unwrap();
            assert_eq!(notification.allowed(false), user);
            assert_eq!(notification.allowed(true), background);
        }
    }

    #[test]
    fn a_resumed_session_does_not_notify_again_but_the_next_session_does() {
        let first =
            ConnectionNotification::for_request(&request("notification-first", true)).unwrap();
        let resumed =
            ConnectionNotification::for_request(&request("notification-first", true)).unwrap();
        assert!(first.pending());
        first.mark_shown();
        assert!(!first.pending());
        assert!(
            !resumed.pending(),
            "a resume must not notify the user again"
        );
        let next =
            ConnectionNotification::for_request(&request("notification-next", true)).unwrap();
        assert!(next.pending());
    }
}
