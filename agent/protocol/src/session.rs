use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteSessionId(String);

impl RemoteSessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RemoteSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VideoStreamId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Idle,
    Requested,
    Signaling,
    Connecting,
    Streaming,
    Closing,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid remote-session transition from {from:?} to {to:?}")]
pub struct InvalidSessionTransition {
    pub from: SessionState,
    pub to: SessionState,
}

impl SessionState {
    pub fn transition(self, next: Self) -> Result<Self, InvalidSessionTransition> {
        let valid = matches!(
            (self, next),
            (Self::Idle, Self::Requested)
                | (Self::Requested, Self::Signaling)
                | (Self::Signaling, Self::Connecting)
                | (Self::Connecting, Self::Streaming)
                | (Self::Streaming, Self::Closing)
                | (Self::Connecting, Self::Closing)
                | (Self::Signaling, Self::Closing)
                | (Self::Requested, Self::Closing)
                | (Self::Closing, Self::Idle)
        );

        valid.then_some(next).ok_or(InvalidSessionTransition {
            from: self,
            to: next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_state_transitions() {
        let state = SessionState::Idle
            .transition(SessionState::Requested)
            .unwrap()
            .transition(SessionState::Signaling)
            .unwrap()
            .transition(SessionState::Connecting)
            .unwrap()
            .transition(SessionState::Streaming)
            .unwrap()
            .transition(SessionState::Closing)
            .unwrap()
            .transition(SessionState::Idle)
            .unwrap();
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn rejects_invalid_state_transition() {
        assert!(
            SessionState::Idle
                .transition(SessionState::Streaming)
                .is_err()
        );
    }
}
