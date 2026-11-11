//! Ends a remote session after the technician has been idle in the viewer
//! for the chosen time. The company sets the default and whether technicians
//! may choose another time; a choice lasts only for the current session.

use std::time::{Duration, Instant};

use meshrmm_protocol::{IDLE_DISCONNECT_MINUTES, IdleDisconnectPolicy, SessionMessage};

/// How often the session checks whether it has been idle too long.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub struct IdleDisconnect {
    policy: IdleDisconnectPolicy,
    /// This session's choice: `None` follows the policy, `Some(None)` never
    /// disconnects.
    choice: Option<Option<u32>>,
    last_activity: Instant,
}

impl Default for IdleDisconnect {
    fn default() -> Self {
        Self {
            policy: IdleDisconnectPolicy::default(),
            choice: None,
            last_activity: Instant::now(),
        }
    }
}

impl IdleDisconnect {
    pub fn set_policy(&mut self, policy: IdleDisconnectPolicy) {
        self.policy = policy;
    }

    pub fn allow_override(&self) -> bool {
        self.policy.allow_override
    }

    /// The idle time in force, in minutes; `None` never disconnects.
    pub fn minutes(&self) -> Option<u32> {
        self.policy.effective(self.choice)
    }

    /// Chooses the idle time for the rest of this session, when the company
    /// allows it. Choosing counts as activity.
    pub fn choose(&mut self, minutes: Option<u32>, now: Instant) {
        if self.policy.allow_override {
            self.choice = Some(minutes);
            self.last_activity = now;
        }
    }

    pub fn note_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// The idle time that has run out by `now`, if one has.
    pub fn expired(&self, now: Instant) -> Option<u32> {
        let minutes = self.minutes()?;
        let limit = Duration::from_secs(u64::from(minutes) * 60);
        (now.saturating_duration_since(self.last_activity) >= limit).then_some(minutes)
    }
}

/// Whether the viewer window sending `message` shows that a person is using
/// the session, so that it restarts the idle time. Only a person's action
/// sends these: keyboard and pointer input, drawing, and the toolbar, menu
/// and settings commands. What the viewer sends by itself (keyframe
/// requests, bitrate feedback, statistics, clipboard synchronisation, file
/// transfer traffic, negotiation) is not activity, and neither is anything
/// only the Agent sends. When the viewer sends one of the person's message
/// types on its own, such as releasing held keys, it does so through
/// [`ControlSink::send_automatic`](crate::platform::ControlSink::send_automatic).
pub fn is_activity(message: &SessionMessage) -> bool {
    match message {
        SessionMessage::Input(_)
        | SessionMessage::Annotate(_)
        | SessionMessage::SelectDisplay { .. }
        | SessionMessage::SetQuality { .. }
        | SessionMessage::SetChroma { .. }
        | SessionMessage::Chat { .. }
        | SessionMessage::SetAgentInputBlocked { .. }
        | SessionMessage::SetBlackout { .. }
        | SessionMessage::SendSecureAttention
        | SessionMessage::SetCursorCapture { .. }
        | SessionMessage::SetWallpaperHidden { .. }
        | SessionMessage::SetDisplayBorder { .. }
        | SessionMessage::SetPreventIdleLock { .. }
        | SessionMessage::SetRecording { .. }
        | SessionMessage::SetSessionCloseAction { .. }
        | SessionMessage::SetClearClipboardOnClose { .. }
        | SessionMessage::PromptForCredentials
        | SessionMessage::AutofillCredentials
        | SessionMessage::ForgetCredentials
        | SessionMessage::SetAudio { .. }
        | SessionMessage::Restart { .. }
        | SessionMessage::StopTyping
        | SessionMessage::SetHeadlessResolution { .. } => true,
        // The viewer sends these by itself.
        SessionMessage::RequestKeyframe { .. }
        | SessionMessage::SetBitrate { .. }
        | SessionMessage::Stats(_)
        | SessionMessage::Start { .. }
        | SessionMessage::Stop { .. }
        // Minimising a window is not using the session, so a session left
        // minimised still reaches its idle limit.
        | SessionMessage::SetVideoPaused { .. }
        | SessionMessage::ViewerCapabilities { .. }
        | SessionMessage::VideoProfileRejected { .. }
        | SessionMessage::ServiceChannelReady
        | SessionMessage::ChatAvailable
        | SessionMessage::Clipboard { .. }
        | SessionMessage::ClipboardChunk { .. }
        | SessionMessage::FileTransfer(_) => false,
        // Only the Agent sends these.
        SessionMessage::Accepted { .. }
        | SessionMessage::Rejected { .. }
        | SessionMessage::StreamConfiguration { .. }
        | SessionMessage::DisplayConfiguration { .. }
        | SessionMessage::DisplayCatalog { .. }
        | SessionMessage::CursorShape { .. }
        | SessionMessage::MaintenanceState { .. }
        | SessionMessage::MaintenanceError { .. }
        | SessionMessage::AgentPointerDisplay { .. }
        | SessionMessage::CredentialState(_)
        | SessionMessage::DeviceState { .. }
        | SessionMessage::Typing { .. } => false,
    }
}

/// The idle times a technician can choose from, in menu order.
pub fn choices() -> impl Iterator<Item = Option<u32>> {
    std::iter::once(None).chain(IDLE_DISCONNECT_MINUTES.map(Some))
}

pub fn label(minutes: Option<u32>) -> String {
    match minutes {
        None => "Never".to_owned(),
        Some(60) => "1 hour".to_owned(),
        Some(minutes) if minutes % 60 == 0 => format!("{} hours", minutes / 60),
        Some(minutes) => format!("{minutes} minutes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(minutes: Option<u32>, allow_override: bool, now: Instant) -> IdleDisconnect {
        let mut idle = IdleDisconnect {
            last_activity: now,
            ..IdleDisconnect::default()
        };
        idle.set_policy(IdleDisconnectPolicy {
            minutes,
            allow_override,
        });
        idle
    }

    #[test]
    fn expires_after_the_idle_time_and_activity_restarts_it() {
        let start = Instant::now();
        let mut idle = session(Some(15), true, start);
        let fifteen = Duration::from_secs(15 * 60);
        assert_eq!(idle.expired(start + fifteen - Duration::from_secs(1)), None);
        assert_eq!(idle.expired(start + fifteen), Some(15));
        idle.note_activity(start + fifteen);
        assert_eq!(
            idle.expired(start + fifteen + Duration::from_secs(60)),
            None
        );
        assert_eq!(idle.expired(start + fifteen * 2), Some(15));
    }

    #[test]
    fn only_messages_a_person_causes_are_activity() {
        use meshrmm_protocol::{
            Annotation, AudioFormat, ChromaMode, Codec, ConnectionStats, CredentialState,
            CursorShape, DesktopSession, DevicePlatform, Display, DisplayId, FileMessage,
            HeadlessResolution, PixelFormat, PointerButton, QualityPreset, RemoteInput,
            RemoteSessionId, SessionCloseAction, VideoFormat, VideoProfile, VideoStreamId,
        };
        let stream_id = VideoStreamId(1);
        let display_id = DisplayId(1);
        let format = VideoFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 60,
            codec: Codec::H264,
            pixel_format: PixelFormat::Nv12,
            bitrate_bits_per_second: 8_000_000,
        };
        let profile = format.profile();
        let display = Display {
            session: DesktopSession::Console,
            id: display_id,
            name: "Display 1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let resolution = HeadlessResolution::PRESETS[0];
        let key = RemoteInput::Key {
            display_id,
            scan_code: 30,
            extended: false,
            pressed: true,
        };
        let pointer = RemoteInput::PointerMove {
            display_id,
            x: 10,
            y: 10,
        };
        let click = RemoteInput::PointerButtonAt {
            display_id,
            x: 10,
            y: 10,
            button: PointerButton::Left,
            pressed: true,
        };
        let person = [
            SessionMessage::Input(key),
            SessionMessage::Input(pointer),
            SessionMessage::Input(click),
            SessionMessage::Annotate(Annotation::Clear),
            SessionMessage::SelectDisplay { display_id },
            SessionMessage::SetQuality {
                preset: QualityPreset::BestQuality,
            },
            SessionMessage::SetChroma {
                mode: ChromaMode::Yuv444,
            },
            SessionMessage::Chat { text: "hi".into() },
            SessionMessage::SetAgentInputBlocked { blocked: true },
            SessionMessage::SetBlackout { enabled: true },
            SessionMessage::SendSecureAttention,
            SessionMessage::SetCursorCapture { enabled: true },
            SessionMessage::SetWallpaperHidden { hidden: true },
            SessionMessage::SetDisplayBorder { enabled: false },
            SessionMessage::SetPreventIdleLock { enabled: true },
            SessionMessage::SetRecording { enabled: true },
            SessionMessage::SetSessionCloseAction {
                action: SessionCloseAction::Lock,
            },
            SessionMessage::SetClearClipboardOnClose { enabled: true },
            SessionMessage::PromptForCredentials,
            SessionMessage::AutofillCredentials,
            SessionMessage::ForgetCredentials,
            SessionMessage::SetAudio {
                enabled: true,
                formats: vec![AudioFormat::Opus],
            },
            SessionMessage::Restart { safe_mode: false },
            SessionMessage::SetHeadlessResolution { resolution },
        ];
        let viewer = [
            SessionMessage::RequestKeyframe { stream_id },
            SessionMessage::SetBitrate {
                bits_per_second: 4_000_000,
            },
            SessionMessage::Stats(ConnectionStats::default()),
            SessionMessage::Start {
                session_id: RemoteSessionId::new("session"),
                format,
            },
            SessionMessage::Stop {
                reason: "closed".into(),
            },
            SessionMessage::SetVideoPaused { paused: true },
            SessionMessage::ViewerCapabilities {
                profiles: vec![profile],
                quality: QualityPreset::Balanced,
                chroma: ChromaMode::Yuv420,
                headless_resolution: resolution,
            },
            SessionMessage::VideoProfileRejected {
                profile: VideoProfile {
                    codec: Codec::H265,
                    chroma: ChromaMode::Yuv444,
                },
                reason: "decoder".into(),
            },
            SessionMessage::ServiceChannelReady,
            SessionMessage::ChatAvailable,
            SessionMessage::Clipboard {
                text: "copied".into(),
            },
            SessionMessage::ClipboardChunk {
                offset: 0,
                total: 1,
                data: vec![0],
            },
            SessionMessage::FileTransfer(FileMessage::Chunk {
                id: 1,
                data: vec![0],
            }),
            SessionMessage::FileTransfer(FileMessage::Available),
        ];
        let agent = [
            SessionMessage::Accepted { stream_id, format },
            SessionMessage::Rejected {
                reason: "busy".into(),
            },
            SessionMessage::StreamConfiguration {
                stream_id,
                format,
                codec_config: Vec::new(),
            },
            SessionMessage::DisplayConfiguration {
                displays: vec![display],
                active_display_id: display_id,
                stream_id,
                format,
            },
            SessionMessage::CursorShape {
                shape: CursorShape::Default,
            },
            SessionMessage::MaintenanceState {
                agent_input_blocked: false,
                blacked_out: false,
            },
            SessionMessage::MaintenanceError {
                reason: "failed".into(),
            },
            SessionMessage::AgentPointerDisplay {
                display_id: Some(display_id),
            },
            SessionMessage::CredentialState(CredentialState::default()),
            SessionMessage::DeviceState {
                platform: DevicePlatform::Windows,
                safe_mode: false,
            },
        ];
        for message in &person {
            assert!(is_activity(message), "{message:?} should be activity");
        }
        for message in viewer.iter().chain(&agent) {
            assert!(!is_activity(message), "{message:?} should not be activity");
        }
    }

    #[test]
    fn never_does_not_expire() {
        let start = Instant::now();
        let idle = session(None, true, start);
        assert_eq!(
            idle.expired(start + Duration::from_secs(365 * 24 * 60 * 60)),
            None
        );
    }

    #[test]
    fn a_session_choice_applies_only_when_the_company_allows_it() {
        let start = Instant::now();
        let later = start + Duration::from_secs(10 * 60);
        let mut allowed = session(Some(5), true, start);
        allowed.choose(None, later);
        assert_eq!(allowed.minutes(), None);
        assert_eq!(
            allowed.expired(later + Duration::from_secs(24 * 60 * 60)),
            None
        );
        allowed.choose(Some(30), later);
        assert_eq!(allowed.minutes(), Some(30));
        // Choosing counts as activity, so the new time runs from the choice.
        assert_eq!(allowed.expired(later + Duration::from_secs(29 * 60)), None);

        let mut managed = session(Some(5), false, start);
        managed.choose(None, later);
        assert_eq!(managed.minutes(), Some(5));
        assert_eq!(managed.expired(later), Some(5));
    }

    #[test]
    fn a_new_session_starts_from_the_company_default() {
        let start = Instant::now();
        let mut first = session(Some(15), true, start);
        first.choose(None, start);
        let second = session(Some(15), true, start);
        assert_eq!(second.minutes(), Some(15));
    }

    #[test]
    fn choices_start_with_never_and_labels_read_naturally() {
        let labels: Vec<String> = choices().map(label).collect();
        assert_eq!(
            labels,
            [
                "Never",
                "5 minutes",
                "10 minutes",
                "15 minutes",
                "30 minutes",
                "1 hour",
                "2 hours",
                "4 hours",
                "8 hours"
            ]
        );
    }
}
