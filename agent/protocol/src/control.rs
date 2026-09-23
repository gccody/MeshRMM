use serde::{Deserialize, Serialize};

use crate::{CursorShape, DisplayId, RemoteInput, RemoteSessionId, VideoStreamId};

pub const CONTROL_CHANNEL_LABEL: &str = "meshrmm-control-v5";
pub const CONTROL_CHANNEL_PROTOCOL: &str = "meshrmm.control.v5";
/// Maximum UTF-8 payload for a single session chat message.
pub const MAX_CHAT_TEXT_BYTES: usize = 4 * 1024;

pub fn valid_chat_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_CHAT_TEXT_BYTES && !text.contains('\0')
}

/// Maximum plain-text clipboard payload on the reliable control channel.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 60 * 1024;

/// Reliable session-control and input messages carried by the control data channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionMessage {
    Start {
        session_id: RemoteSessionId,
        format: VideoFormat,
    },
    Accepted {
        stream_id: VideoStreamId,
        format: VideoFormat,
    },
    Rejected {
        reason: String,
    },
    StreamConfiguration {
        stream_id: VideoStreamId,
        format: VideoFormat,
        codec_config: Vec<u8>,
    },
    RequestKeyframe {
        stream_id: VideoStreamId,
    },
    SetBitrate {
        bits_per_second: u32,
    },
    Stats(ConnectionStats),
    Stop {
        reason: String,
    },
    /// Announces the displays available on the Agent and the display whose
    /// pixels are carried by `stream_id`. Input for any other display is
    /// rejected by the Agent.
    DisplayConfiguration {
        displays: Vec<Display>,
        active_display_id: DisplayId,
        stream_id: VideoStreamId,
        format: VideoFormat,
    },
    SelectDisplay {
        display_id: DisplayId,
    },
    Input(RemoteInput),
    /// The semantic shape of the cursor currently active on the Agent. Native
    /// viewers map unsupported shapes back to their normal default cursor.
    CursorShape {
        shape: CursorShape,
    },
    /// Replaces the peer's clipboard with UTF-8 plain text (legacy wire format).
    Clipboard {
        text: String,
    },
    /// Hardware video profiles and the initial viewer preferences. Profiles
    /// are ordered from most to least preferred.
    ViewerCapabilities {
        profiles: Vec<VideoProfile>,
        quality: QualityPreset,
        chroma: ChromaMode,
    },
    /// Changes the encoder quality ceiling without changing the network path.
    SetQuality {
        preset: QualityPreset,
    },
    /// Changes chroma fidelity. The sender selects the best mutually supported
    /// codec profile and restarts only the video stream.
    SetChroma {
        mode: ChromaMode,
    },
    /// The viewer advertised a profile but could not initialize it for the
    /// negotiated stream. The sender must try the next compatible profile.
    VideoProfileRejected {
        profile: VideoProfile,
        reason: String,
    },
    /// Session-only plain text chat. Append variants to preserve postcard tags.
    Chat {
        text: String,
    },
    /// Announces support for session chat; both peers must opt in.
    ChatAvailable,
    FileTransfer(crate::FileMessage),
    SetAgentInputBlocked {
        blocked: bool,
    },
    MaintenanceState {
        agent_input_blocked: bool,
        blacked_out: bool,
    },
    SetBlackout {
        enabled: bool,
    },
    MaintenanceError {
        reason: String,
    },
    /// Request the Windows secure attention sequence (Ctrl+Alt+Del).
    /// Keep appended so existing postcard message tags remain stable.
    SendSecureAttention,
    /// Ordered, bounded parts of a rich clipboard payload.
    ClipboardChunk {
        offset: u32,
        total: u32,
        data: Vec<u8>,
    },
    /// Dedicated service-stream capability; appended to preserve existing tags.
    ServiceChannelReady,
    /// Include the Agent's actual cursor in captured video. Appended for wire compatibility.
    SetCursorCapture {
        enabled: bool,
    },
    /// Session wallpaper preference; appended to preserve postcard tags.
    SetWallpaperHidden {
        hidden: bool,
    },
    /// Agent-only monitor outline, excluded from captured video.
    SetDisplayBorder {
        enabled: bool,
    },
    /// Physical monitor containing the agent-side pointer; None while the viewer owns input.
    AgentPointerDisplay {
        display_id: Option<DisplayId>,
    },
    SetPreventIdleLock {
        enabled: bool,
    },
    /// Keep the cursor in encoded video while a viewer records.
    SetRecording {
        enabled: bool,
    },
    /// Action the Agent runs on the viewed Windows session when the remote
    /// session ends. Appended to preserve postcard tags.
    SetSessionCloseAction {
        action: SessionCloseAction,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCloseAction {
    #[default]
    NoAction,
    Lock,
    Logout,
}

impl SessionCloseAction {
    pub const ALL: [Self; 3] = [Self::NoAction, Self::Lock, Self::Logout];

    pub fn label(self) -> &'static str {
        match self {
            Self::NoAction => "No action",
            Self::Lock => "Lock",
            Self::Logout => "Logout",
        }
    }
}

impl SessionMessage {
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_stdvec(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        let message: Self = postcard::from_bytes(bytes)?;
        if let Self::Chat { text } = &message
            && !valid_chat_text(text)
        {
            return Err(postcard::Error::DeserializeBadEncoding);
        }
        if let Self::Input(RemoteInput::TypeText { text, .. }) = &message
            && (text.len() > MAX_CLIPBOARD_TEXT_BYTES || text.contains('\0'))
        {
            return Err(postcard::Error::DeserializeBadEncoding);
        }
        Ok(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopSession {
    Console,
    Background,
    Rdp { id: u32, user: String },
}

impl DesktopSession {
    pub fn label(&self) -> String {
        match self {
            Self::Console => "Console".into(),
            Self::Background => "Background".into(),
            Self::Rdp { id, user } => format!("{user} (RDP {id})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Display {
    pub session: DesktopSession,
    pub id: DisplayId,
    pub name: String,
    /// Desktop-space coordinates. These may be negative when a display is to
    /// the left of or above the primary display.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl Display {
    pub fn session_displays<'a>(&self, displays: &'a [Self]) -> Vec<&'a Self> {
        displays
            .iter()
            .filter(|d| d.session == self.session)
            .collect()
    }

    pub fn sessions(displays: &[Self]) -> Vec<DesktopSession> {
        let mut sessions = Vec::new();
        for display in displays {
            if !sessions.contains(&display.session) {
                sessions.push(display.session.clone());
            }
        }
        sessions
    }

    pub fn selection_label(&self, index: usize) -> String {
        if self.id.0 == u32::MAX - 1 || self.name == "All monitors" {
            "All displays".into()
        } else {
            format!("Display {}", index + 1)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    H265,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromaMode {
    #[default]
    Yuv420,
    Yuv444,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoProfile {
    pub codec: Codec,
    pub chroma: ChromaMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityPreset {
    DataSaver,
    #[default]
    Balanced,
    BestQuality,
    // Append to preserve the existing postcard discriminants.
    UltraDataSaver,
}

pub const MAX_QUALITY_BITRATE_BITS_PER_SECOND: u32 = 12_000_000;

impl QualityPreset {
    pub fn grayscale(self) -> bool {
        self == Self::UltraDataSaver
    }

    /// Never raise a lower administrator-configured capture rate.
    pub fn frames_per_second(self, configured: u32) -> u32 {
        if self.grayscale() {
            configured.min(24)
        } else {
            configured
        }
    }

    /// Applies the preset without exceeding the 12 Mbps application cap or the
    /// lower administrator-configured cap.
    pub fn bitrate(self, configured_maximum: u32) -> u32 {
        let preferred = match self {
            Self::UltraDataSaver => 1_000_000,
            Self::DataSaver => 3_000_000,
            Self::Balanced => 6_000_000,
            Self::BestQuality => MAX_QUALITY_BITRATE_BITS_PER_SECOND,
        };
        preferred.min(configured_maximum).max(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Nv12,
    Ayuv,
}

impl PixelFormat {
    pub fn chroma(self) -> ChromaMode {
        match self {
            Self::Nv12 => ChromaMode::Yuv420,
            Self::Ayuv => ChromaMode::Yuv444,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
    pub codec: Codec,
    pub pixel_format: PixelFormat,
    pub bitrate_bits_per_second: u32,
}

impl VideoFormat {
    pub fn profile(self) -> VideoProfile {
        VideoProfile {
            codec: self.codec,
            chroma: self.pixel_format.chroma(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionStats {
    pub capture_fps: f32,
    pub stream_fps: f32,
    pub bitrate_bits_per_second: u64,
    pub rtt_ms: f32,
    pub packet_loss_percent: f32,
    pub encode_ms: f32,
    pub decode_ms: f32,
    pub render_ms: f32,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub frames_dropped: u64,
    pub incomplete_frames_dropped: u64,
    pub connection_path: ConnectionPath,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionPath {
    Direct,
    Turn,
    #[default]
    Unknown,
}

#[cfg(test)]
mod tests {
    #[test]
    fn recording_override_round_trips_and_preserves_idle_tag() {
        for enabled in [false, true] {
            let message = super::SessionMessage::SetRecording { enabled };
            assert_eq!(
                super::SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
        assert_eq!(
            super::SessionMessage::SetPreventIdleLock { enabled: true }
                .encode()
                .unwrap(),
            vec![31, 1]
        );
    }

    use super::*;

    #[test]
    fn session_close_action_is_appended_and_round_trips() {
        for (index, action) in SessionCloseAction::ALL.into_iter().enumerate() {
            let message = SessionMessage::SetSessionCloseAction { action };
            assert_eq!(message.encode().unwrap(), vec![33, index as u8]);
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
    }

    #[test]
    fn idle_preference_round_trips_without_changing_pointer_tag() {
        for enabled in [true, false] {
            let message = SessionMessage::SetPreventIdleLock { enabled };
            assert_eq!(message.encode().unwrap(), vec![31, u8::from(enabled)]);
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
        assert_eq!(
            SessionMessage::AgentPointerDisplay { display_id: None }
                .encode()
                .unwrap(),
            vec![30, 0]
        );
    }

    #[test]
    fn quality_presets_preserve_wire_tags_and_capture_limits() {
        for (preset, tag) in [
            (QualityPreset::DataSaver, 0),
            (QualityPreset::Balanced, 1),
            (QualityPreset::BestQuality, 2),
            (QualityPreset::UltraDataSaver, 3),
        ] {
            assert_eq!(postcard::to_stdvec(&preset).unwrap(), vec![tag]);
            assert_eq!(
                postcard::from_bytes::<QualityPreset>(&[tag]).unwrap(),
                preset
            );
            assert_eq!(preset.frames_per_second(15), 15);
            assert_eq!(preset.bitrate(500_000), 500_000);
            assert_eq!(preset.grayscale(), tag == 3);
            assert_eq!(preset.frames_per_second(60), if tag == 3 { 24 } else { 60 });
        }
        assert_eq!(QualityPreset::UltraDataSaver.bitrate(12_000_000), 1_000_000);
    }

    #[test]
    fn secure_attention_appends_a_stable_control_message() {
        let message = SessionMessage::SendSecureAttention;
        assert_eq!(message.encode().unwrap(), vec![24]);
        assert_eq!(SessionMessage::decode(&[24]).unwrap(), message);
        assert_eq!(
            SessionMessage::SetBlackout { enabled: true }
                .encode()
                .unwrap(),
            vec![22, 1]
        );
    }

    #[test]
    fn control_message_round_trip() {
        let message = SessionMessage::Start {
            session_id: RemoteSessionId::new("session_opaque_123"),
            format: VideoFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 60,
                codec: Codec::H264,
                pixel_format: PixelFormat::Nv12,
                bitrate_bits_per_second: 12_000_000,
            },
        };

        let encoded = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn display_configuration_round_trip_preserves_negative_coordinates() {
        let format = VideoFormat {
            width: 2_560,
            height: 1_440,
            frames_per_second: 60,
            codec: Codec::H264,
            pixel_format: PixelFormat::Nv12,
            bitrate_bits_per_second: 12_000_000,
        };
        let message = SessionMessage::DisplayConfiguration {
            displays: vec![Display {
                session: DesktopSession::Console,
                id: DisplayId(2),
                name: "Left display".into(),
                x: -2_560,
                y: -180,
                width: 2_560,
                height: 1_440,
                primary: false,
            }],
            active_display_id: DisplayId(2),
            stream_id: VideoStreamId(9),
            format,
        };

        let encoded = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn pointer_monitor_round_trip_preserves_existing_tags() {
        for display_id in [None, Some(DisplayId(2))] {
            let message = SessionMessage::AgentPointerDisplay { display_id };
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
            assert_eq!(message.encode().unwrap()[0], 30);
        }
        assert_eq!(
            SessionMessage::SetDisplayBorder { enabled: true }
                .encode()
                .unwrap(),
            vec![29, 1]
        );
    }

    #[test]
    fn display_border_preserves_wire_tags() {
        for enabled in [true, false] {
            let message = SessionMessage::SetDisplayBorder { enabled };
            assert_eq!(message.encode().unwrap(), vec![29, u8::from(enabled)]);
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
        assert_eq!(
            SessionMessage::SetWallpaperHidden { hidden: true }
                .encode()
                .unwrap(),
            vec![28, 1]
        );
    }

    #[test]
    fn wallpaper_preference_round_trips_and_keeps_existing_tags() {
        for hidden in [false, true] {
            let message = SessionMessage::SetWallpaperHidden { hidden };
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
        assert_eq!(
            SessionMessage::SetCursorCapture { enabled: true }
                .encode()
                .unwrap(),
            vec![27, 1]
        );
    }

    #[test]
    fn cursor_capture_round_trips_without_changing_cursor_shape_messages() {
        for enabled in [false, true] {
            let message = SessionMessage::SetCursorCapture { enabled };
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
        // The existing cursor-shape postcard discriminant must remain unchanged.
        assert_eq!(
            SessionMessage::CursorShape {
                shape: CursorShape::Text
            }
            .encode()
            .unwrap(),
            vec![11, 1]
        );
    }

    #[test]
    fn cursor_shape_round_trips_through_control_channel() {
        let message = SessionMessage::CursorShape {
            shape: CursorShape::Text,
        };

        let encoded = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn clipboard_text_round_trips_through_control_channel() {
        let message = SessionMessage::Clipboard {
            text: "copied on the other computer — 📋".into(),
        };

        let encoded = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn maximum_clipboard_text_fits_the_sctp_message_limit() {
        let message = SessionMessage::Clipboard {
            text: "a".repeat(MAX_CLIPBOARD_TEXT_BYTES),
        };

        assert!(message.encode().unwrap().len() <= 65_536);
    }

    #[test]
    fn viewer_capabilities_round_trip() {
        let message = SessionMessage::ViewerCapabilities {
            profiles: vec![
                VideoProfile {
                    codec: Codec::H265,
                    chroma: ChromaMode::Yuv444,
                },
                VideoProfile {
                    codec: Codec::H265,
                    chroma: ChromaMode::Yuv420,
                },
            ],
            quality: QualityPreset::Balanced,
            chroma: ChromaMode::Yuv444,
        };
        let encoded = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn quality_presets_respect_the_configured_cap() {
        assert_eq!(QualityPreset::DataSaver.bitrate(12_000_000), 3_000_000);
        assert_eq!(QualityPreset::Balanced.bitrate(12_000_000), 6_000_000);
        assert_eq!(QualityPreset::BestQuality.bitrate(12_000_000), 12_000_000);
        assert_eq!(QualityPreset::BestQuality.bitrate(100_000_000), 12_000_000);
        assert_eq!(QualityPreset::Balanced.bitrate(4_000_000), 4_000_000);
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;
    #[test]
    fn unicode_chat_round_trips_and_rejects_invalid_payloads() {
        for text in [
            "Hello 👋\nHow can I help?".to_owned(),
            "é".repeat(MAX_CHAT_TEXT_BYTES / 2),
        ] {
            let message = SessionMessage::Chat { text };
            assert_eq!(
                SessionMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
        for text in [
            "".to_owned(),
            " \n".to_owned(),
            "bad\0text".to_owned(),
            "é".repeat(MAX_CHAT_TEXT_BYTES / 2 + 1),
        ] {
            assert!(
                SessionMessage::decode(&SessionMessage::Chat { text }.encode().unwrap()).is_err()
            );
        }
    }
    #[test]
    fn chat_is_appended_without_changing_existing_wire_tags() {
        assert_eq!(
            SessionMessage::Clipboard { text: "x".into() }
                .encode()
                .unwrap(),
            vec![12, 1, b'x']
        );
        assert_eq!(
            SessionMessage::Chat { text: "x".into() }.encode().unwrap(),
            vec![17, 1, b'x']
        );
        assert_eq!(SessionMessage::ChatAvailable.encode().unwrap(), vec![18]);
    }
}

#[cfg(test)]
mod desktop_session_tests {
    use super::*;

    fn display(id: u32, session: DesktopSession) -> Display {
        Display {
            id: DisplayId(id),
            session,
            name: "Monitor".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: false,
        }
    }

    #[test]
    fn selectors_keep_console_background_and_each_rdp_user_separate() {
        let alice = DesktopSession::Rdp {
            id: 3,
            user: "Alice".into(),
        };
        let bob = DesktopSession::Rdp {
            id: 4,
            user: "Alice".into(),
        };
        let displays = vec![
            display(1, DesktopSession::Console),
            display(2, DesktopSession::Console),
            display(crate::BACKGROUND_DISPLAY_ID.0, DesktopSession::Background),
            display(101, alice.clone()),
            display(102, alice.clone()),
            display(103, alice.clone()),
            display(201, bob.clone()),
        ];
        assert_eq!(
            Display::sessions(&displays),
            vec![
                DesktopSession::Console,
                DesktopSession::Background,
                alice,
                bob
            ]
        );
        assert_eq!(displays[0].session_displays(&displays).len(), 2);
        assert_eq!(displays[2].session_displays(&displays).len(), 1);
        assert_eq!(
            displays[3]
                .session_displays(&displays)
                .iter()
                .map(|d| d.id.0)
                .collect::<Vec<_>>(),
            vec![101, 102, 103]
        );
        assert_eq!(displays[6].session_displays(&displays).len(), 1);
        for (index, display) in displays[3].session_displays(&displays).iter().enumerate() {
            assert_eq!(
                display.selection_label(index),
                format!("Display {}", index + 1)
            );
        }
        let message = SessionMessage::DisplayConfiguration {
            displays,
            active_display_id: DisplayId(102),
            stream_id: VideoStreamId(1),
            format: VideoFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 30,
                bitrate_bits_per_second: 8_000_000,
                codec: Codec::H264,
                pixel_format: PixelFormat::Nv12,
            },
        };
        assert_eq!(
            SessionMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }
}
