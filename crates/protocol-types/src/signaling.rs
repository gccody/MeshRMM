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
    pub idle_policy: IdlePolicy,
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
    pub idle_policy: IdlePolicy,
    #[serde(default)]
    pub blackout_message: String,
    #[serde(default)]
    pub viewer_name: String,
    pub session_id: RemoteSessionId,
    pub signaling_token: String,
    pub expires_at_unix_ms: u64,
    pub ice_servers: Vec<IceServer>,
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
        request.viewer_name = "Zoë 王".into();
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

/// Trusted company policy, supplied separately to both session peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdlePolicy {
    #[serde(default = "default_enabled")]
    pub prevent_idle_lock: bool,
    #[serde(default = "default_enabled")]
    pub allow_override: bool,
}
impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            prevent_idle_lock: true,
            allow_override: true,
        }
    }
}
impl IdlePolicy {
    pub fn effective(self, choice: Option<bool>) -> bool {
        if self.allow_override {
            choice.unwrap_or(self.prevent_idle_lock)
        } else {
            self.prevent_idle_lock
        }
    }
}
#[cfg(test)]
mod idle_policy_tests {
    use super::*;
    #[test]
    fn locked_policies_ignore_both_override_directions() {
        for default in [true, false] {
            for allowed in [true, false] {
                let policy = IdlePolicy {
                    prevent_idle_lock: default,
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
            serde_json::from_str::<IdlePolicy>("{}").unwrap(),
            IdlePolicy::default()
        );
    }
}
