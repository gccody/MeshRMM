use serde::{Deserialize, Serialize};

use crate::RemoteSessionId;

pub fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBootstrap {
    #[serde(default)]
    pub start_in_background: bool,
    #[serde(default)]
    pub idle_policy: TogglePolicy,
    #[serde(default)]
    pub idle_disconnect: IdleDisconnectPolicy,
    #[serde(default)]
    pub clear_clipboard_policy: TogglePolicy,
    #[serde(default = "default_enabled")]
    pub display_border: bool,
    pub session_id: RemoteSessionId,
    pub signaling_token: String,
    pub expires_at_unix_ms: u64,
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRequest {
    #[serde(default)]
    pub start_in_background: bool,
    #[serde(default)]
    pub idle_policy: TogglePolicy,
    /// Company policy for emptying the viewed session's clipboard when the
    /// remote session ends. The Agent enforces it over the viewer's choice.
    #[serde(default)]
    pub clear_clipboard_policy: TogglePolicy,
    #[serde(default)]
    pub blackout_message: String,
    /// Company policy for the Agent's on-screen connection banner. Only the
    /// server sets it, so viewers cannot hide the banner.
    #[serde(default = "default_enabled")]
    pub session_banner: bool,
    /// Company policy for the popup on the Agent's primary monitor when a
    /// technician connects, and its template. `connection_notification`
    /// covers sessions that view a user's desktop, and
    /// `background_connection_notification` covers sessions on the background
    /// desktop. Only the server sets them, so viewers cannot suppress the
    /// notification. Older servers never sent one.
    #[serde(default)]
    pub connection_notification: bool,
    #[serde(default)]
    pub background_connection_notification: bool,
    #[serde(default)]
    pub connection_notification_message: String,
    /// Company policy that makes the Agent's user accept the connection
    /// before the technician sees or controls anything. Only the server sets
    /// it, so viewers cannot skip the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_approval: Option<ConnectionApproval>,
    /// Why the technician is connecting, as they entered it in the dashboard.
    /// Empty when they gave no reason.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub connection_reason: String,
    #[serde(default)]
    pub viewer_name: String,
    pub session_id: RemoteSessionId,
    pub signaling_token: String,
    pub expires_at_unix_ms: u64,
    pub ice_servers: Vec<IceServer>,
}

/// How the Agent asks its user to accept a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionApproval {
    /// The prompt's template; `{user_name}` is the technician.
    pub message: String,
    /// The connection is accepted when the user has not answered by then.
    pub timeout_seconds: u32,
    /// The connection is accepted at once while the computer is locked and
    /// has had no input for this long.
    pub lock_idle_seconds: u32,
}

/// Commands sent over the authenticated Agent coordinator connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentCommand {
    Uninstall,
    RotateToken { token: String },
    EndSession { session_id: RemoteSessionId },
    StartBackgroundSession { request: AgentSessionRequest },
}

/// Agent-to-coordinator lifecycle notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentStatusMessage {
    UninstallScheduled,
    /// Sent just before the Agent stops to install an automatic update, so the
    /// dashboard can say why it went offline.
    Updating {
        version: String,
    },
}

/// Whether `version` has only what a semantic version can contain, so an announced update is
/// safe to display.
pub fn is_release_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    Ready,
    Activity,
    EndSession,
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sdp_mline_index: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username_fragment: Option<String>,
    },
    IceComplete,
    PeerLeft,
    /// The Agent is asking its user to accept the connection, which it
    /// accepts for them after `remaining_seconds`. Sent in answer to the
    /// viewer's `Ready` until the prompt is answered.
    AwaitingApproval {
        remaining_seconds: u32,
    },
    Error {
        message: String,
        /// What failed, for peers that act on it. Older peers send and
        /// expect no code; the message stays the human-readable detail.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<SignalErrorCode>,
    },
}

/// Why a peer stopped the session, carried by [`SignalMessage::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalErrorCode {
    /// The remote computer has no usable hardware encoder. Terminal before
    /// the first frame.
    HardwareEncoderUnavailable,
    /// The peers share no video profile. Terminal before the first frame.
    NoMutualProfile,
    /// A peer's identity did not match the one trusted. Always terminal.
    IdentityMismatch,
    /// Capture could not start; this is often transient (UAC, lock screen,
    /// RDP switches), so it is retried.
    CaptureUnavailable,
    /// The Agent's user declined the connection. Always terminal.
    ConnectionDeclined,
    /// A code added by a newer peer.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_defaults_on_for_legacy_bootstraps_and_preserves_false() {
        let mut value = serde_json::json!({"session_id":"s", "signaling_token":"t", "expires_at_unix_ms":1, "ice_servers":[]});
        assert!(
            serde_json::from_value::<SessionBootstrap>(value.clone())
                .unwrap()
                .display_border
        );
        value["display_border"] = serde_json::json!(false);
        let bootstrap: SessionBootstrap = serde_json::from_value(value).unwrap();
        assert!(!bootstrap.display_border);
        assert_eq!(
            serde_json::from_str::<SessionBootstrap>(&serde_json::to_string(&bootstrap).unwrap())
                .unwrap(),
            bootstrap
        );
    }

    #[test]
    fn session_names_survive_transport_and_legacy_requests_still_decode() {
        let legacy = serde_json::json!({
            "session_id": "session-123", "signaling_token": "token",
            "expires_at_unix_ms": 123, "ice_servers": []
        });
        let mut request: AgentSessionRequest = serde_json::from_value(legacy).unwrap();
        assert!(!request.start_in_background);
        assert!(request.viewer_name.is_empty());
        assert!(request.session_banner, "legacy servers keep the banner");
        assert!(!request.connection_notification);
        assert!(!request.background_connection_notification);
        assert!(request.connection_notification_message.is_empty());
        assert_eq!(request.connection_approval, None);
        assert!(request.connection_reason.is_empty());
        request.viewer_name = "Zoë 王".into();
        request.session_banner = false;
        request.connection_notification = true;
        request.background_connection_notification = true;
        request.connection_notification_message = "{user_name} is here".into();
        request.connection_approval = Some(ConnectionApproval {
            message: "{user_name} would like to connect.".into(),
            timeout_seconds: 30,
            lock_idle_seconds: 0,
        });
        request.connection_reason = "Printer queue\nticket 42".into();
        let decoded: AgentSessionRequest =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn agent_lifecycle_messages_use_tagged_json() {
        assert_eq!(
            serde_json::to_string(&AgentCommand::Uninstall).unwrap(),
            r#"{"type":"uninstall"}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentStatusMessage::UninstallScheduled).unwrap(),
            r#"{"type":"uninstall_scheduled"}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentStatusMessage::Updating {
                version: "0.3.1".to_owned(),
            })
            .unwrap(),
            r#"{"type":"updating","version":"0.3.1"}"#
        );
    }

    #[test]
    fn signal_errors_without_a_code_still_decode() {
        let legacy: SignalMessage =
            serde_json::from_str(r#"{"type":"error","message":"capture failed"}"#).unwrap();
        assert_eq!(
            legacy,
            SignalMessage::Error {
                message: "capture failed".into(),
                code: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&legacy).unwrap(),
            r#"{"type":"error","message":"capture failed"}"#
        );
    }

    #[test]
    fn signal_error_codes_round_trip_and_unknown_codes_decode() {
        let coded = SignalMessage::Error {
            message: "no encoder".into(),
            code: Some(SignalErrorCode::HardwareEncoderUnavailable),
        };
        let json = serde_json::to_string(&coded).unwrap();
        assert_eq!(
            json,
            r#"{"type":"error","message":"no encoder","code":"hardware_encoder_unavailable"}"#
        );
        assert_eq!(serde_json::from_str::<SignalMessage>(&json).unwrap(), coded);
        for code in [
            SignalErrorCode::NoMutualProfile,
            SignalErrorCode::IdentityMismatch,
            SignalErrorCode::CaptureUnavailable,
            SignalErrorCode::ConnectionDeclined,
        ] {
            let message = SignalMessage::Error {
                message: String::new(),
                code: Some(code),
            };
            let decoded: SignalMessage =
                serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
            assert_eq!(decoded, message);
        }
        let newer: SignalMessage =
            serde_json::from_str(r#"{"type":"error","message":"later","code":"something_new"}"#)
                .unwrap();
        assert_eq!(
            newer,
            SignalMessage::Error {
                message: "later".into(),
                code: Some(SignalErrorCode::Unknown),
            }
        );
    }

    #[test]
    fn peers_built_before_error_codes_decode_coded_errors() {
        // The shape of `SignalMessage` before `code` existed.
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum OldSignalMessage {
            Error { message: String },
        }
        let json = serde_json::to_string(&SignalMessage::Error {
            message: "no encoder".into(),
            code: Some(SignalErrorCode::HardwareEncoderUnavailable),
        })
        .unwrap();
        assert_eq!(
            serde_json::from_str::<OldSignalMessage>(&json).unwrap(),
            OldSignalMessage::Error {
                message: "no encoder".into()
            }
        );
    }

    #[test]
    fn awaiting_approval_uses_tagged_json() {
        let waiting = SignalMessage::AwaitingApproval {
            remaining_seconds: 25,
        };
        let json = serde_json::to_string(&waiting).unwrap();
        assert_eq!(
            json,
            r#"{"type":"awaiting_approval","remaining_seconds":25}"#
        );
        assert_eq!(
            serde_json::from_str::<SignalMessage>(&json).unwrap(),
            waiting
        );
    }

    #[test]
    fn accepts_only_release_versions() {
        assert!(is_release_version("0.3.1-rc.1+build.5"));
        assert!(!is_release_version(""));
        assert!(!is_release_version("1.0 <b>"));
        assert!(!is_release_version(&"1".repeat(65)));
        assert_eq!(
            serde_json::to_string(&AgentCommand::EndSession {
                session_id: RemoteSessionId::new("session-123"),
            })
            .unwrap(),
            r#"{"type":"end_session","session_id":"session-123"}"#
        );
    }
}

/// A company default for a per-session toggle, and whether viewers may change
/// it. Trusted policy, supplied separately to both session peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TogglePolicy {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_enabled")]
    pub allow_override: bool,
}
impl Default for TogglePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_override: true,
        }
    }
}
impl TogglePolicy {
    pub fn effective(self, choice: Option<bool>) -> bool {
        if self.allow_override {
            choice.unwrap_or(self.enabled)
        } else {
            self.enabled
        }
    }
}

/// The idle times, in minutes, after which a remote session can be
/// disconnected. Company policy and viewer choices are limited to these.
pub const IDLE_DISCONNECT_MINUTES: [u32; 8] = [5, 10, 15, 30, 60, 120, 240, 480];

pub fn valid_idle_disconnect_minutes(minutes: u32) -> bool {
    IDLE_DISCONNECT_MINUTES.contains(&minutes)
}

/// Trusted company policy for ending a remote session after the technician
/// has been idle in the viewer. Only the viewer enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleDisconnectPolicy {
    /// The company default; `None` never disconnects.
    #[serde(default)]
    pub minutes: Option<u32>,
    /// Whether the technician may choose another time for their session.
    #[serde(default = "default_enabled")]
    pub allow_override: bool,
}
impl Default for IdleDisconnectPolicy {
    fn default() -> Self {
        Self {
            minutes: None,
            allow_override: true,
        }
    }
}
impl IdleDisconnectPolicy {
    /// The idle time in force, given the technician's choice for this
    /// session: `None` for no choice, `Some(None)` for never.
    pub fn effective(self, choice: Option<Option<u32>>) -> Option<u32> {
        match choice {
            Some(choice) if self.allow_override => choice,
            _ => self.minutes,
        }
    }
}

#[cfg(test)]
mod idle_disconnect_tests {
    use super::*;
    #[test]
    fn locked_policies_ignore_the_session_choice() {
        for minutes in [None, Some(15)] {
            for allowed in [true, false] {
                let policy = IdleDisconnectPolicy {
                    minutes,
                    allow_override: allowed,
                };
                assert_eq!(policy.effective(None), minutes);
                for choice in [None, Some(5), Some(480)] {
                    assert_eq!(
                        policy.effective(Some(choice)),
                        if allowed { choice } else { minutes }
                    );
                }
            }
        }
        assert_eq!(
            serde_json::from_str::<IdleDisconnectPolicy>("{}").unwrap(),
            IdleDisconnectPolicy::default()
        );
    }

    #[test]
    fn only_listed_idle_times_are_valid() {
        assert!(
            IDLE_DISCONNECT_MINUTES
                .into_iter()
                .all(valid_idle_disconnect_minutes)
        );
        for minutes in [0, 1, 7, 481, u32::MAX] {
            assert!(!valid_idle_disconnect_minutes(minutes));
        }
    }
}

#[cfg(test)]
mod toggle_policy_tests {
    use super::*;
    #[test]
    fn locked_policies_ignore_both_override_directions() {
        for default in [true, false] {
            for allowed in [true, false] {
                let policy = TogglePolicy {
                    enabled: default,
                    allow_override: allowed,
                };
                assert_eq!(policy.effective(None), default);
                assert_eq!(
                    policy.effective(Some(!default)),
                    if allowed { !default } else { default }
                );
            }
        }
        assert_eq!(
            serde_json::from_str::<TogglePolicy>("{}").unwrap(),
            TogglePolicy::default()
        );
    }
}
