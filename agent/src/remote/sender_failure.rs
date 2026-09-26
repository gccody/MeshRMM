//! Why a sender stopped, decided where the failure happens, and what the
//! viewer is told about it.
//!
//! Errors lose their types on the way here (the desktop helper reports its
//! failures as text), so capture and encoder failures are coded at the call
//! sites that know what failed, and transport failures the viewer detects on
//! its own are marked so they are never reported.

use std::fmt;

use meshrmm_protocol::{SignalErrorCode, SignalMessage, VideoProfile};

/// The encoder's "no hardware encoder" failure. The desktop helper reports it
/// as text, and `meshrmm_remote_screen`'s encoder error type is private, so the
/// message is the only thing both capture paths share.
const HARDWARE_ENCODER_UNAVAILABLE: &str = "no hardware Media Foundation";

/// A failure with a code the viewer acts on.
#[derive(Debug)]
pub struct CodedSenderError {
    pub code: SignalErrorCode,
    pub message: String,
}

impl fmt::Display for CodedSenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodedSenderError {}

/// Sender failures the viewer is not told about.
#[derive(Debug)]
pub enum SenderFailureKind {
    /// Signaling closed, the viewer left, or WebRTC failed or disconnected.
    /// The viewer sees these itself. When no viewer is connected the server
    /// keeps the last error and replays it to the next connection, where a
    /// stale transport failure would end a fresh attempt.
    Transport(String),
}

impl fmt::Display for SenderFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SenderFailureKind {}

pub fn transport_failure(message: impl Into<String>) -> anyhow::Error {
    SenderFailureKind::Transport(message.into()).into()
}

fn coded(code: SignalErrorCode, error: &anyhow::Error) -> anyhow::Error {
    CodedSenderError {
        code,
        message: format!("{error:#}"),
    }
    .into()
}

fn is_hardware_encoder_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains(HARDWARE_ENCODER_UNAVAILABLE))
}

/// The session's first capture start failed. A missing hardware encoder will
/// not change; anything else may (UAC, the lock screen, RDP switches).
pub fn initial_start_error(error: anyhow::Error) -> anyhow::Error {
    let code = if is_hardware_encoder_unavailable(&error) {
        SignalErrorCode::HardwareEncoderUnavailable
    } else {
        SignalErrorCode::CaptureUnavailable
    };
    coded(code, &error)
}

/// No negotiated profile could start. `failures` holds each candidate tried.
pub fn profile_start_error(failures: Vec<(VideoProfile, anyhow::Error)>) -> anyhow::Error {
    let detail = failures
        .iter()
        .map(|(profile, error)| format!("{profile:?}: {error:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    let error =
        anyhow::anyhow!("no mutually supported hardware video profile could start: {detail}");
    if failures.is_empty() {
        coded(SignalErrorCode::NoMutualProfile, &error)
    } else if failures
        .iter()
        .all(|(_, error)| is_hardware_encoder_unavailable(error))
    {
        coded(SignalErrorCode::HardwareEncoderUnavailable, &error)
    } else {
        error
    }
}

/// What to tell the viewer about `error`, or `None` when it sees the failure
/// itself.
pub fn failure_signal(error: &anyhow::Error) -> Option<SignalMessage> {
    if matches!(
        error.downcast_ref::<SenderFailureKind>(),
        Some(SenderFailureKind::Transport(_))
    ) {
        return None;
    }
    let code = if error
        .downcast_ref::<meshrmm_session_transport::identity::IdentityError>()
        .is_some()
    {
        Some(SignalErrorCode::IdentityMismatch)
    } else {
        error
            .downcast_ref::<CodedSenderError>()
            .map(|coded| coded.code)
    };
    Some(SignalMessage::Error {
        message: format!("{error:#}"),
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::{ChromaMode, Codec};

    const HEVC: VideoProfile = VideoProfile {
        codec: Codec::H265,
        chroma: ChromaMode::Yuv420,
    };
    const H264: VideoProfile = VideoProfile {
        codec: Codec::H264,
        chroma: ChromaMode::Yuv420,
    };

    fn code(error: &anyhow::Error) -> Option<SignalErrorCode> {
        match failure_signal(error) {
            Some(SignalMessage::Error { code, .. }) => code,
            other => panic!("expected an error signal, got {other:?}"),
        }
    }

    /// The text the desktop helper reports when no hardware encoder exists.
    fn helper_encoder_failure(codec: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "desktop helper failed: Media Foundation hardware encoder failed: no hardware Media Foundation {codec} encoder accepts NV12"
        )
    }

    #[test]
    fn no_candidates_means_no_mutual_profile() {
        let error = profile_start_error(Vec::new()).context("capture worker");
        assert_eq!(code(&error), Some(SignalErrorCode::NoMutualProfile));
    }

    #[test]
    fn every_candidate_lacking_a_hardware_encoder_is_coded() {
        let error = profile_start_error(vec![
            (HEVC, helper_encoder_failure("H265")),
            (
                H264,
                helper_encoder_failure("H264").context("could not start capture"),
            ),
        ])
        .context("capture worker");
        assert_eq!(
            code(&error),
            Some(SignalErrorCode::HardwareEncoderUnavailable)
        );
        let Some(SignalMessage::Error { message, .. }) = failure_signal(&error) else {
            unreachable!()
        };
        assert!(message.starts_with("capture worker: no mutually supported"));
        assert!(message.contains("H265 encoder accepts NV12"));

        let mixed = profile_start_error(vec![
            (HEVC, helper_encoder_failure("H265")),
            (
                H264,
                anyhow::anyhow!("desktop helper did not start within 5 seconds"),
            ),
        ]);
        assert_eq!(code(&mixed), None);
    }

    #[test]
    fn initial_start_failures_are_coded() {
        assert_eq!(
            code(&initial_start_error(helper_encoder_failure("H264"))),
            Some(SignalErrorCode::HardwareEncoderUnavailable)
        );
        assert_eq!(
            code(&initial_start_error(anyhow::anyhow!(
                "no interactive desktop is available"
            ))),
            Some(SignalErrorCode::CaptureUnavailable)
        );
    }

    #[test]
    fn identity_failures_are_coded() {
        let error: anyhow::Error =
            meshrmm_session_transport::identity::IdentityError("mismatch".into()).into();
        assert_eq!(code(&error), Some(SignalErrorCode::IdentityMismatch));
    }

    #[test]
    fn transport_failures_are_not_reported() {
        assert!(failure_signal(&transport_failure("signaling connection closed")).is_none());
        assert!(
            failure_signal(
                &transport_failure("WebRTC connection ended in state Failed")
                    .context("sender stopped")
            )
            .is_none()
        );
        assert_eq!(
            code(&anyhow::anyhow!(
                "capture/encoder produced no bootstrap keyframe for 10 seconds"
            )),
            None
        );
    }
}
