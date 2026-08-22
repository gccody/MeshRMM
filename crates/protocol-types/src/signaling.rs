use serde::{Deserialize, Serialize};

use crate::RemoteSessionId;

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
    pub session_id: RemoteSessionId,
    pub signaling_token: String,
    pub expires_at_unix_ms: u64,
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRequest {
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
    fn agent_lifecycle_messages_use_tagged_json() {
        assert_eq!(
            serde_json::to_string(&AgentCommand::Uninstall).unwrap(),
            r#"{"type":"uninstall"}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentStatusMessage::UninstallScheduled).unwrap(),
            r#"{"type":"uninstall_scheduled"}"#
        );
    }
}
