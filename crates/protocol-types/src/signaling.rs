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
    },
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
