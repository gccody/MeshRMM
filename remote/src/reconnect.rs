//! When the viewer tries a failed connection again. Before the remote
//! display first appears, retries are capped so a connection that cannot
//! work ends with an explanation instead of spinning forever; afterwards the
//! viewer keeps reconnecting until the user gives up.

use std::time::{Duration, Instant};

use meshrmm_protocol::SignalErrorCode;

use crate::transport::{FailureKind, failure_kind};

/// Failed attempts allowed before the first frame.
pub const STARTUP_ATTEMPTS: u32 = 3;
/// How long the viewer keeps trying before the first frame. Checked only
/// after a failure, so an attempt in progress is never cut short.
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(60);

/// What the session loop does after an attempt fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Retry,
    /// The startup cap was reached before the first frame.
    GiveUp,
    /// Retrying cannot help.
    Terminal,
}

/// Decides what follows a failed attempt. `startup_failures` counts failed
/// attempts before the first frame, including this one.
pub fn disposition(
    error: &anyhow::Error,
    ever_presented: bool,
    startup_failures: u32,
    startup_elapsed: Duration,
) -> Disposition {
    if crate::signaling::is_terminal_session_error(error) {
        return Disposition::Terminal;
    }
    let code = match failure_kind(error) {
        Some(FailureKind::AgentReported(code)) => code,
        _ => None,
    };
    if code == Some(SignalErrorCode::IdentityMismatch) {
        return Disposition::Terminal;
    }
    if ever_presented {
        return Disposition::Retry;
    }
    if matches!(
        code,
        Some(SignalErrorCode::HardwareEncoderUnavailable | SignalErrorCode::NoMutualProfile)
    ) {
        return Disposition::Terminal;
    }
    if startup_failures >= STARTUP_ATTEMPTS || startup_elapsed >= STARTUP_DEADLINE {
        return Disposition::GiveUp;
    }
    Disposition::Retry
}

/// The error that ends the session after [`Disposition::Terminal`] or
/// [`Disposition::GiveUp`]: its root is the user-facing explanation of
/// `error`, which the fatal-error dialogs show as is.
pub fn stopped_error(error: &anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(crate::errors::UserFacing(crate::errors::user_message(
        error,
    )))
    .context("remote viewer stopped retrying the session")
}

/// Whether the remote display has appeared, in this attempt and ever.
#[derive(Debug, Default)]
pub struct AttemptProgress {
    ever_presented: bool,
    attempt_first_frame_at: Option<Instant>,
}

impl AttemptProgress {
    pub fn begin_attempt(&mut self) {
        self.attempt_first_frame_at = None;
    }

    /// Records a frame handed to the presenter. Returns whether it was this
    /// attempt's first.
    pub fn mark_frame_presented(&mut self, now: Instant) -> bool {
        self.ever_presented = true;
        if self.attempt_first_frame_at.is_some() {
            return false;
        }
        self.attempt_first_frame_at = Some(now);
        true
    }

    pub fn ever_presented(&self) -> bool {
        self.ever_presented
    }

    /// How long this attempt has shown the remote display, if it has.
    pub fn attempt_streamed_for(&self, now: Instant) -> Option<Duration> {
        self.attempt_first_frame_at
            .map(|first| now.saturating_duration_since(first))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SessionFailure;

    fn failure(kind: FailureKind) -> anyhow::Error {
        anyhow::Error::new(SessionFailure::new(kind, "detail"))
    }

    fn agent(code: SignalErrorCode) -> anyhow::Error {
        failure(FailureKind::AgentReported(Some(code)))
    }

    const SOON: Duration = Duration::from_secs(5);

    #[test]
    fn terminal_codes_end_startup_but_not_an_established_session() {
        for code in [
            SignalErrorCode::HardwareEncoderUnavailable,
            SignalErrorCode::NoMutualProfile,
        ] {
            assert_eq!(
                disposition(&agent(code), false, 1, SOON),
                Disposition::Terminal
            );
            assert_eq!(disposition(&agent(code), true, 0, SOON), Disposition::Retry);
        }
        for code in [
            SignalErrorCode::CaptureUnavailable,
            SignalErrorCode::Unknown,
        ] {
            assert_eq!(
                disposition(&agent(code), false, 1, SOON),
                Disposition::Retry
            );
        }
    }

    #[test]
    fn startup_gives_up_after_three_failures_or_a_minute() {
        let timeout = || failure(FailureKind::VideoTimeout);
        assert_eq!(disposition(&timeout(), false, 1, SOON), Disposition::Retry);
        assert_eq!(disposition(&timeout(), false, 2, SOON), Disposition::Retry);
        assert_eq!(disposition(&timeout(), false, 3, SOON), Disposition::GiveUp);
        assert_eq!(
            disposition(&timeout(), false, 2, Duration::from_secs(61)),
            Disposition::GiveUp
        );
        // Old Agents send no code; the cap still applies.
        assert_eq!(
            disposition(&failure(FailureKind::AgentReported(None)), false, 3, SOON),
            Disposition::GiveUp
        );
    }

    #[test]
    fn an_established_session_retries_indefinitely() {
        assert_eq!(
            disposition(
                &failure(FailureKind::PeerConnectionLost),
                true,
                100,
                Duration::from_secs(3600)
            ),
            Disposition::Retry
        );
    }

    #[test]
    fn identity_errors_are_always_terminal() {
        let identity: anyhow::Error =
            meshrmm_session_transport::identity::IdentityError("mismatch".into()).into();
        assert_eq!(disposition(&identity, true, 0, SOON), Disposition::Terminal);
        assert_eq!(
            disposition(&agent(SignalErrorCode::IdentityMismatch), true, 0, SOON),
            Disposition::Terminal
        );
    }

    #[test]
    fn the_stopped_error_shows_its_explanation_verbatim() {
        let error = stopped_error(&failure(FailureKind::VideoTimeout));
        let expected = crate::errors::user_message(&failure(FailureKind::VideoTimeout));
        assert!(expected.contains("did not send its screen within 30 seconds"));
        assert_eq!(crate::errors::user_message(&error), expected);
        assert_eq!(
            crate::errors::user_message(&error.context("remote session failed")),
            expected
        );
    }

    #[test]
    fn attempt_progress_tracks_the_first_frame_of_each_attempt() {
        let start = Instant::now();
        let mut progress = AttemptProgress::default();
        progress.begin_attempt();
        assert!(!progress.ever_presented());
        assert_eq!(progress.attempt_streamed_for(start), None);
        assert!(progress.mark_frame_presented(start));
        assert!(!progress.mark_frame_presented(start + SOON));
        assert_eq!(
            progress.attempt_streamed_for(start + Duration::from_secs(20)),
            Some(Duration::from_secs(20))
        );
        progress.begin_attempt();
        assert!(progress.ever_presented());
        assert_eq!(progress.attempt_streamed_for(start + SOON), None);
    }
}
