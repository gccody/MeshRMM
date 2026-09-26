//! Why a connection attempt ended, so the session loop can decide whether to
//! retry and the user sees what went wrong.

use std::fmt;

use meshrmm_protocol::SignalErrorCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The signaling connection closed or stopped responding.
    SignalingLost,
    /// WebRTC failed before it ever connected in this attempt.
    PeerNeverConnected,
    /// WebRTC connected, then failed, closed or stayed disconnected.
    PeerConnectionLost,
    /// The peer connected but no video stream arrived in time.
    VideoTimeout,
    /// Decoding, presenting or a data channel failed on this side.
    PresentationFailed,
    /// The Agent reported an error, with its code when it sent one.
    AgentReported(Option<SignalErrorCode>),
    /// The Agent left the session.
    AgentLeft,
}

/// A failed attempt. Its text is the technical detail, so logs read as before.
#[derive(Debug)]
pub struct SessionFailure {
    pub kind: FailureKind,
    pub detail: String,
}

impl SessionFailure {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for SessionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for SessionFailure {}

/// The kind of the attempt failure behind `error`, if it is one.
pub fn failure_kind(error: &anyhow::Error) -> Option<FailureKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SessionFailure>())
        .map(|failure| failure.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_display_their_detail_and_are_found_under_context() {
        let error = anyhow::Error::new(SessionFailure::new(
            FailureKind::AgentLeft,
            "Agent disconnected from the remote session",
        ))
        .context("remote viewer attempt failed");
        assert_eq!(
            format!("{error:#}"),
            "remote viewer attempt failed: Agent disconnected from the remote session"
        );
        assert_eq!(failure_kind(&error), Some(FailureKind::AgentLeft));
        assert_eq!(failure_kind(&anyhow::anyhow!("other")), None);
    }
}
