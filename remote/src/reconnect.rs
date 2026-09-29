//! When the viewer tries a failed connection again. Before the remote
//! display first appears, retries are capped so a connection that cannot
//! work ends with an explanation instead of spinning forever; afterwards the
//! viewer keeps reconnecting until the user gives up, and the session window
//! shows why, for how long, and when the next attempt starts.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use meshrmm_protocol::SignalErrorCode;
use tokio::sync::Notify;

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
    if matches!(
        code,
        Some(SignalErrorCode::IdentityMismatch | SignalErrorCode::ConnectionDeclined)
    ) {
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

/// Why the session window is reconnecting, as far as the viewer can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectReason {
    /// The Agent left, or the server could not reach it.
    RemoteUnavailable,
    /// This computer lost its connection to the MeshRMM service.
    NetworkLost,
    /// The peer-to-peer connection failed while signaling still worked.
    ConnectionInterrupted,
    /// The remote display or this viewer's video path failed.
    VideoRestarting,
}

impl ReconnectReason {
    fn title(self) -> &'static str {
        match self {
            Self::RemoteUnavailable => "The remote computer is restarting or offline",
            Self::NetworkLost => "Network connection lost",
            Self::ConnectionInterrupted => "Connection to the remote computer was interrupted",
            Self::VideoRestarting => "Restarting the remote display…",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectPhase {
    /// Waiting out the backoff; the next attempt starts at `until`.
    Waiting { until: Instant },
    /// Resuming the session or connecting.
    Attempting,
}

/// What the session window shows while the connection is being restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectStatus {
    pub reason: ReconnectReason,
    /// The first failure since the remote display last appeared.
    pub since: Instant,
    pub phase: ReconnectPhase,
}

/// The overlay's text and whether "Retry now" is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectText {
    pub title: &'static str,
    pub detail: String,
    pub retry_enabled: bool,
}

impl ReconnectStatus {
    /// The status after an attempt failed for `reason`. The disconnected
    /// time keeps counting from the first failure in a row.
    pub fn after_failure(
        previous: Option<ReconnectStatus>,
        reason: ReconnectReason,
        now: Instant,
    ) -> Self {
        Self {
            reason,
            since: previous.map_or(now, |previous| previous.since),
            phase: ReconnectPhase::Attempting,
        }
    }

    pub fn render(&self, now: Instant) -> ReconnectText {
        let disconnected = format_elapsed(now.saturating_duration_since(self.since));
        let countdown = match self.phase {
            ReconnectPhase::Waiting { until } => {
                // Round up, so the count reaches 1 s rather than 0 s.
                let remaining = until
                    .saturating_duration_since(now)
                    .as_millis()
                    .div_ceil(1000);
                (remaining > 0).then_some(remaining)
            }
            ReconnectPhase::Attempting => None,
        };
        let detail = match countdown {
            Some(seconds) => format!("Disconnected for {disconnected} · retrying in {seconds} s"),
            None => format!("Disconnected for {disconnected} · reconnecting…"),
        };
        ReconnectText {
            title: self.reason.title(),
            detail,
            retry_enabled: countdown.is_some(),
        }
    }
}

/// `0:05`, `1:02`, or `1:02:03`.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Why a connection attempt failed, for the reconnect overlay.
pub fn classify(error: &anyhow::Error) -> ReconnectReason {
    recognized_reason(error).unwrap_or(ReconnectReason::ConnectionInterrupted)
}

/// What a failed session resume says about the remote computer or the
/// network, if anything. Other failures leave the attempt's reason.
pub fn classify_resume_failure(error: &anyhow::Error) -> Option<ReconnectReason> {
    recognized_reason(error)
}

fn recognized_reason(error: &anyhow::Error) -> Option<ReconnectReason> {
    use tokio_tungstenite::tungstenite::Error as WebSocketError;
    // The server answers a resume for an offline Agent with 409 from the
    // Agent's coordinator, which reaches the viewer as a 500.
    let remote_unavailable = |status: u16| status == 409 || status >= 500;
    for cause in error.chain() {
        if let Some(failure) = cause.downcast_ref::<crate::transport::SessionFailure>() {
            return Some(match failure.kind {
                FailureKind::AgentLeft | FailureKind::VideoTimeout => {
                    ReconnectReason::RemoteUnavailable
                }
                FailureKind::SignalingLost => ReconnectReason::NetworkLost,
                FailureKind::PeerConnectionLost | FailureKind::PeerNeverConnected => {
                    ReconnectReason::ConnectionInterrupted
                }
                FailureKind::PresentationFailed | FailureKind::AgentReported(_) => {
                    ReconnectReason::VideoRestarting
                }
            });
        }
        if let Some(api) = cause.downcast_ref::<crate::errors::ApiError>()
            && remote_unavailable(api.status)
        {
            return Some(ReconnectReason::RemoteUnavailable);
        }
        if let Some(http) = cause.downcast_ref::<reqwest::Error>() {
            if http
                .status()
                .is_some_and(|status| remote_unavailable(status.as_u16()))
            {
                return Some(ReconnectReason::RemoteUnavailable);
            }
            if http.is_connect() || http.is_timeout() {
                return Some(ReconnectReason::NetworkLost);
            }
        }
        if matches!(
            cause.downcast_ref::<WebSocketError>(),
            Some(
                WebSocketError::Io(_)
                    | WebSocketError::Tls(_)
                    | WebSocketError::ConnectionClosed
                    | WebSocketError::AlreadyClosed
            )
        ) || cause.is::<std::io::Error>()
            || cause.is::<tokio::time::error::Elapsed>()
        {
            return Some(ReconnectReason::NetworkLost);
        }
    }
    None
}

/// Bumped by "Retry now". The session loop notes the value when it starts
/// waiting, so a click from before the wait does not skip it.
static RETRY_GENERATION: AtomicU64 = AtomicU64::new(0);

fn retry_notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// Ends the current backoff wait early. Called from the UI thread.
pub fn request_retry_now() {
    RETRY_GENERATION.fetch_add(1, Ordering::AcqRel);
    tracing::info!("the user asked to reconnect now");
    retry_notify().notify_waiters();
}

pub fn retry_generation() -> u64 {
    RETRY_GENERATION.load(Ordering::Acquire)
}

/// Completes once "Retry now" was requested after `generation` was read.
pub async fn wait_for_retry_after(generation: u64) {
    loop {
        let notified = retry_notify().notified();
        tokio::pin!(notified);
        // Register before checking so a concurrent request is not missed.
        notified.as_mut().enable();
        if retry_generation() > generation {
            return;
        }
        notified.await;
    }
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

    /// Records a frame confirmed by the native presentation path. Returns whether it was this
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
    fn a_declined_connection_never_retries() {
        let declined = || agent(SignalErrorCode::ConnectionDeclined);
        assert_eq!(
            disposition(&declined(), false, 1, SOON),
            Disposition::Terminal
        );
        assert_eq!(
            disposition(&declined(), true, 0, SOON),
            Disposition::Terminal
        );
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

    #[test]
    fn failures_map_to_reconnect_reasons() {
        for (kind, reason) in [
            (FailureKind::AgentLeft, ReconnectReason::RemoteUnavailable),
            (
                FailureKind::VideoTimeout,
                ReconnectReason::RemoteUnavailable,
            ),
            (FailureKind::SignalingLost, ReconnectReason::NetworkLost),
            (
                FailureKind::PeerConnectionLost,
                ReconnectReason::ConnectionInterrupted,
            ),
            (
                FailureKind::PeerNeverConnected,
                ReconnectReason::ConnectionInterrupted,
            ),
            (
                FailureKind::PresentationFailed,
                ReconnectReason::VideoRestarting,
            ),
            (
                FailureKind::AgentReported(Some(SignalErrorCode::CaptureUnavailable)),
                ReconnectReason::VideoRestarting,
            ),
            (
                FailureKind::AgentReported(None),
                ReconnectReason::VideoRestarting,
            ),
        ] {
            let error = failure(kind).context("remote viewer attempt failed");
            assert_eq!(classify(&error), reason, "{kind:?}");
        }
        assert_eq!(
            classify(&anyhow::anyhow!("signaling connection closed: None")),
            ReconnectReason::ConnectionInterrupted
        );
        assert_eq!(
            classify_resume_failure(&anyhow::anyhow!("invalid bootstrap")),
            None
        );
    }

    #[test]
    fn network_errors_mean_the_network_was_lost() {
        use tokio_tungstenite::tungstenite::Error as WebSocketError;
        let refused = || std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let websocket = anyhow::Error::new(WebSocketError::Io(refused()))
            .context("signaling WebSocket handshake failed");
        assert_eq!(classify(&websocket), ReconnectReason::NetworkLost);
        let closed = anyhow::Error::new(WebSocketError::ConnectionClosed);
        assert_eq!(classify(&closed), ReconnectReason::NetworkLost);
        assert_eq!(
            classify(&anyhow::Error::new(refused())),
            ReconnectReason::NetworkLost
        );
    }

    #[tokio::test]
    async fn unreachable_servers_and_offline_agents_are_told_apart() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // An unused local port refuses the connection, as a lost network does.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/resume"))
            .send()
            .await
            .unwrap_err();
        assert!(refused.is_connect(), "{refused:?}");
        assert_eq!(
            classify_resume_failure(&anyhow::Error::new(refused).context("resume failed")),
            Some(ReconnectReason::NetworkLost)
        );

        let status = |status: u16| {
            let response = reqwest::Response::from(
                tokio_tungstenite::tungstenite::http::Response::builder()
                    .status(status)
                    .body(String::new())
                    .unwrap(),
            );
            anyhow::Error::new(response.error_for_status().unwrap_err())
                .context("session resume API rejected the session")
        };
        for offline in [409, 500, 503] {
            assert_eq!(
                classify_resume_failure(&status(offline)),
                Some(ReconnectReason::RemoteUnavailable),
                "{offline}"
            );
        }
        assert_eq!(classify_resume_failure(&status(429)), None);
        let api = anyhow::Error::new(crate::errors::ApiError {
            status: 500,
            message: "failed to refresh Agent remote session".into(),
        });
        assert_eq!(classify(&api), ReconnectReason::RemoteUnavailable);
    }

    #[test]
    fn render_shows_the_reason_elapsed_time_and_countdown() {
        let since = Instant::now();
        let at = |seconds: u64| since + Duration::from_secs(seconds);
        let mut status = ReconnectStatus::after_failure(None, ReconnectReason::NetworkLost, since);
        assert_eq!(status.since, since);
        assert_eq!(
            status.render(at(5)),
            ReconnectText {
                title: "Network connection lost",
                detail: "Disconnected for 0:05 · reconnecting…".into(),
                retry_enabled: false,
            }
        );
        status.phase = ReconnectPhase::Waiting { until: at(70) };
        assert_eq!(
            status.render(at(62)),
            ReconnectText {
                title: "Network connection lost",
                detail: "Disconnected for 1:02 · retrying in 8 s".into(),
                retry_enabled: true,
            }
        );
        // Part of a second left still counts as a second.
        assert_eq!(
            status.render(at(69) + Duration::from_millis(100)).detail,
            "Disconnected for 1:09 · retrying in 1 s"
        );
        // Past the deadline, the next attempt is starting.
        let due = status.render(at(70));
        assert_eq!(due.detail, "Disconnected for 1:10 · reconnecting…");
        assert!(!due.retry_enabled);

        let later = ReconnectStatus::after_failure(
            Some(status),
            ReconnectReason::RemoteUnavailable,
            at(3723),
        );
        assert_eq!(later.since, since);
        assert_eq!(later.phase, ReconnectPhase::Attempting);
        let text = later.render(at(3723));
        assert_eq!(text.title, "The remote computer is restarting or offline");
        assert_eq!(text.detail, "Disconnected for 1:02:03 · reconnecting…");
    }

    #[tokio::test]
    async fn retry_now_ends_only_a_wait_that_started_before_it() {
        let short = Duration::from_millis(200);
        // A request from before the wait started does not skip it.
        request_retry_now();
        let generation = retry_generation();
        assert!(
            tokio::time::timeout(short, wait_for_retry_after(generation))
                .await
                .is_err()
        );
        // A request during the wait ends it.
        let waiter = tokio::spawn(wait_for_retry_after(generation));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        request_retry_now();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the wait ended")
            .unwrap();
        // So does one that came between reading the generation and waiting.
        let generation = retry_generation();
        request_retry_now();
        tokio::time::timeout(short, wait_for_retry_after(generation))
            .await
            .expect("a request after the generation was read counts");
    }
}
