//! Asks the Agent's user to accept a technician's connection before the
//! session streams anything. Company policy turns it on, and the viewer
//! cannot skip it. The user accepts or denies in a prompt on the console's
//! desktop. Nobody answering accepts the connection when the policy's time is
//! up, and a computer that has been sitting idle at the lock screen accepts at
//! once, since nobody is there to answer.
use std::sync::Mutex;
use std::time::Duration;

use meshrmm_protocol::{AgentSessionRequest, RemoteSessionId};

#[cfg(windows)]
mod prompt;
#[cfg(windows)]
mod signaling;
#[cfg(windows)]
mod window;
#[cfg(windows)]
pub use prompt::ask;
#[cfg(windows)]
pub use signaling::obtain;

/// Refuses sessions that need the user's approval until the macOS Agent can
/// ask for it, rather than streaming without it.
#[cfg(target_os = "macos")]
pub async fn obtain(
    _approval: &ConnectionApproval,
    _signal_url: &url::Url,
    _signaling_token: &str,
    _mode: super::config::ExecutionMode,
) -> anyhow::Result<bool> {
    anyhow::bail!("the macOS Agent cannot ask the user to approve connections yet")
}

/// The last session whose connection was answered, and whether it was
/// accepted. A viewer resume restarts the session with a new streamer, which
/// must not ask the user again, and a declined session stays declined.
static ANSWERED: Mutex<Option<(RemoteSessionId, bool)>> = Mutex::new(None);

/// What the prompt shows and when it answers for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub text: String,
    /// The technician's reason, or empty.
    pub reason: String,
    pub timeout: Duration,
    pub lock_idle: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionApproval {
    session_id: RemoteSessionId,
    prompt: ApprovalPrompt,
}

impl ConnectionApproval {
    /// The approval `request` needs, when its company requires one.
    pub fn for_request(request: &AgentSessionRequest) -> Option<Self> {
        let policy = request.connection_approval.as_ref()?;
        // Out-of-range values come only from a misbehaving server; clamp them
        // so the prompt neither vanishes nor holds the technician for long.
        let timeout = policy.timeout_seconds.clamp(
            meshrmm_protocol::MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS,
            meshrmm_protocol::MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS,
        );
        let lock_idle = policy
            .lock_idle_seconds
            .min(meshrmm_protocol::MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS);
        Some(Self {
            session_id: request.session_id.clone(),
            prompt: ApprovalPrompt {
                text: meshrmm_protocol::render_connection_approval_message(
                    &policy.message,
                    &request.viewer_name,
                ),
                reason: meshrmm_protocol::connection_reason(&request.connection_reason).to_owned(),
                timeout: Duration::from_secs(timeout.into()),
                lock_idle: Duration::from_secs(lock_idle.into()),
            },
        })
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn prompt(&self) -> &ApprovalPrompt {
        &self.prompt
    }

    /// Whether this session's connection was already accepted, if it was
    /// answered.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn previous_answer(&self) -> Option<bool> {
        ANSWERED
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|(session_id, _)| *session_id == self.session_id)
            .map(|(_, accepted)| *accepted)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record(&self, accepted: bool) {
        *ANSWERED.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((self.session_id.clone(), accepted));
    }
}

/// How long an accepted answer kept for a restart stays valid: long enough
/// for Windows to restart and the technician to reconnect.
const RESTART_ANSWER_LIFETIME: Duration = Duration::from_secs(30 * 60);
#[cfg(any(windows, target_os = "macos"))]
const RESTART_ANSWER_FILE: &str = "restart-approval";

/// The session to keep accepted across a restart the technician requested
/// from `session_id`, if its connection was accepted.
fn answer_to_remember(session_id: &RemoteSessionId) -> Option<RemoteSessionId> {
    ANSWERED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .filter(|(answered, accepted)| answered == session_id && *accepted)
        .map(|(answered, _)| answered.clone())
}

/// Accepts the remembered session again after the restart, when the answer
/// is recent.
fn restore_answer(session_id: &str, age: Duration) -> bool {
    if session_id.is_empty() || age > RESTART_ANSWER_LIFETIME {
        return false;
    }
    *ANSWERED.lock().unwrap_or_else(|error| error.into_inner()) =
        Some((RemoteSessionId::new(session_id), true));
    true
}

/// Keeps this session's accepted answer for the Agent that starts after the
/// restart, so resuming the session does not ask the user again. The file
/// sits in the Agent's administrator-only configuration directory.
#[cfg(any(windows, target_os = "macos"))]
pub fn remember_across_restart(session_id: &RemoteSessionId) -> anyhow::Result<()> {
    let path = crate::installer::config_directory()?.join(RESTART_ANSWER_FILE);
    match answer_to_remember(session_id) {
        Some(session_id) => crate::installer::replace_file(&path, session_id.as_str().as_bytes()),
        None => match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        },
    }
}

/// Run once when the coordinator starts.
#[cfg(any(windows, target_os = "macos"))]
pub fn restore_after_restart() {
    let Ok(path) = crate::installer::config_directory().map(|d| d.join(RESTART_ANSWER_FILE)) else {
        return;
    };
    let (Ok(session_id), Ok(metadata)) = (std::fs::read_to_string(&path), path.metadata()) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let age = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .unwrap_or(Duration::MAX);
    if restore_answer(session_id.trim(), age) {
        tracing::info!(%session_id, "kept the connection approval of the session that restarted the computer");
    }
}

/// How a connection was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accepted,
    Declined,
    /// Nobody answered in time.
    TimedOut,
    /// The computer was locked and idle, so nobody could answer.
    LockedAndIdle,
}

impl Decision {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn accepted(self) -> bool {
        self != Self::Declined
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Accepted => 0,
            Self::Declined => 1,
            Self::TimedOut => 2,
            Self::LockedAndIdle => 3,
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Accepted,
            1 => Self::Declined,
            2 => Self::TimedOut,
            3 => Self::LockedAndIdle,
            _ => return None,
        })
    }
}

/// The answer the policy gives for the user `elapsed` into the prompt, if it
/// gives one yet. `locked` covers the lock screen and a console nobody is
/// signed in to; `idle` is how long the console has had no input.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn automatic_decision(
    prompt: &ApprovalPrompt,
    elapsed: Duration,
    locked: bool,
    idle: Duration,
) -> Option<Decision> {
    if locked && idle >= prompt.lock_idle {
        Some(Decision::LockedAndIdle)
    } else if elapsed >= prompt.timeout {
        Some(Decision::TimedOut)
    } else {
        None
    }
}

/// Whole seconds until the connection is accepted for the user, rounded up
/// so the countdown reaches zero only when it is.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn remaining_seconds(timeout: Duration, elapsed: Duration) -> u32 {
    let remaining = timeout.saturating_sub(elapsed);
    u32::try_from(remaining.as_millis().div_ceil(1000)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(session_id: &str) -> AgentSessionRequest {
        AgentSessionRequest {
            start_in_background: false,
            idle_policy: Default::default(),
            clear_clipboard_policy: Default::default(),
            blackout_message: String::new(),
            session_banner: true,
            connection_notification: true,
            background_connection_notification: false,
            connection_notification_message: String::new(),
            connection_approval: Some(meshrmm_protocol::ConnectionApproval {
                message: "{user_name} asks\nOK?".into(),
                timeout_seconds: 30,
                lock_idle_seconds: 60,
            }),
            connection_reason: "  Printer queue\nticket 42 ".into(),
            viewer_name: "Zoë 王".into(),
            session_id: RemoteSessionId::new(session_id),
            signaling_token: "token".into(),
            expires_at_unix_ms: 1,
            ice_servers: vec![],
        }
    }

    /// Tests that change the process-wide answer run one at a time.
    static ANSWER_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn only_a_recent_accepted_answer_survives_a_restart() {
        let _answer = ANSWER_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let approval = ConnectionApproval::for_request(&request("restarting")).unwrap();
        let other = RemoteSessionId::new("other");
        approval.record(false);
        assert_eq!(answer_to_remember(&approval.session_id), None);
        approval.record(true);
        assert_eq!(answer_to_remember(&other), None);
        let remembered = answer_to_remember(&approval.session_id).unwrap();

        *ANSWERED.lock().unwrap() = None;
        assert!(!restore_answer(
            remembered.as_str(),
            RESTART_ANSWER_LIFETIME + Duration::from_secs(1)
        ));
        assert_eq!(approval.previous_answer(), None);
        assert!(restore_answer(remembered.as_str(), Duration::from_secs(90)));
        assert_eq!(approval.previous_answer(), Some(true));
        *ANSWERED.lock().unwrap() = None;
    }

    fn prompt(timeout: u64, lock_idle: u64) -> ApprovalPrompt {
        ApprovalPrompt {
            text: String::new(),
            reason: String::new(),
            timeout: Duration::from_secs(timeout),
            lock_idle: Duration::from_secs(lock_idle),
        }
    }

    #[test]
    fn only_companies_that_require_approval_prompt_and_the_prompt_is_rendered() {
        let mut off = request("approval-off");
        off.connection_approval = None;
        assert_eq!(ConnectionApproval::for_request(&off), None);
        let approval = ConnectionApproval::for_request(&request("approval-on")).unwrap();
        assert_eq!(
            approval.prompt(),
            &ApprovalPrompt {
                text: "Zoë 王 asks\nOK?".into(),
                reason: "Printer queue\nticket 42".into(),
                timeout: Duration::from_secs(30),
                lock_idle: Duration::from_secs(60),
            }
        );
    }

    #[test]
    fn a_missing_reason_or_template_falls_back_and_bad_limits_are_clamped() {
        let mut plain = request("approval-plain");
        plain.connection_reason = "bad\ttext".into();
        let policy = plain.connection_approval.as_mut().unwrap();
        policy.message.clear();
        policy.timeout_seconds = 0;
        policy.lock_idle_seconds = u32::MAX;
        let approval = ConnectionApproval::for_request(&plain).unwrap();
        assert_eq!(approval.prompt().text, "Zoë 王 would like to connect.");
        assert_eq!(approval.prompt().reason, "");
        assert_eq!(approval.prompt().timeout, Duration::from_secs(5));
        assert_eq!(approval.prompt().lock_idle, Duration::from_secs(3600));
        plain.connection_approval.as_mut().unwrap().timeout_seconds = u32::MAX;
        assert_eq!(
            ConnectionApproval::for_request(&plain)
                .unwrap()
                .prompt()
                .timeout,
            Duration::from_secs(300)
        );
    }

    #[test]
    fn a_resumed_session_keeps_its_answer_and_the_next_session_asks_again() {
        let _answer = ANSWER_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let first = ConnectionApproval::for_request(&request("approval-first")).unwrap();
        let resumed = ConnectionApproval::for_request(&request("approval-first")).unwrap();
        assert_eq!(first.previous_answer(), None);
        first.record(false);
        assert_eq!(
            resumed.previous_answer(),
            Some(false),
            "declined stays declined"
        );
        first.record(true);
        assert_eq!(resumed.previous_answer(), Some(true));
        let next = ConnectionApproval::for_request(&request("approval-next")).unwrap();
        assert_eq!(next.previous_answer(), None);
    }

    #[test]
    fn nobody_answering_accepts_when_the_time_is_up() {
        let prompt = prompt(30, 60);
        let idle = Duration::from_secs(600);
        assert_eq!(
            automatic_decision(&prompt, Duration::from_secs(29), false, idle),
            None
        );
        assert_eq!(
            automatic_decision(&prompt, Duration::from_secs(30), false, idle),
            Some(Decision::TimedOut)
        );
    }

    #[test]
    fn a_locked_computer_accepts_at_once_only_after_sitting_idle() {
        let prompt = prompt(30, 60);
        assert_eq!(
            automatic_decision(&prompt, Duration::ZERO, true, Duration::from_secs(59)),
            None,
            "someone may have just locked it"
        );
        assert_eq!(
            automatic_decision(&prompt, Duration::ZERO, true, Duration::from_secs(60)),
            Some(Decision::LockedAndIdle)
        );
        assert_eq!(
            automatic_decision(
                &prompt,
                Duration::from_secs(5),
                false,
                Duration::from_secs(600)
            ),
            None,
            "an idle but unlocked computer still asks"
        );
        assert_eq!(
            automatic_decision(&self::prompt(30, 0), Duration::ZERO, true, Duration::ZERO),
            Some(Decision::LockedAndIdle),
            "zero accepts whenever it is locked"
        );
    }

    #[test]
    fn the_countdown_rounds_up_and_stops_at_zero() {
        let timeout = Duration::from_secs(30);
        assert_eq!(remaining_seconds(timeout, Duration::ZERO), 30);
        assert_eq!(remaining_seconds(timeout, Duration::from_millis(100)), 30);
        assert_eq!(remaining_seconds(timeout, Duration::from_millis(29_001)), 1);
        assert_eq!(remaining_seconds(timeout, Duration::from_secs(31)), 0);
    }

    #[test]
    fn only_a_denial_declines_and_decisions_survive_the_helper_pipe() {
        for decision in [
            Decision::Accepted,
            Decision::Declined,
            Decision::TimedOut,
            Decision::LockedAndIdle,
        ] {
            assert_eq!(decision.accepted(), decision != Decision::Declined);
            assert_eq!(Decision::from_byte(decision.to_byte()), Some(decision));
        }
        assert_eq!(Decision::from_byte(4), None);
    }
}
