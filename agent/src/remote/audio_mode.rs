//! Which system audio the sender captures and how it sends it, decided from
//! the viewer's control messages. Control messages are ordered, so applying
//! them as they arrive cannot race.

use meshrmm_protocol::AudioFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AudioMode {
    /// The viewer has not said yet; capture nothing.
    Undetermined,
    /// A viewer without `SetAudio`: always capture and send PCM, as before.
    Legacy,
    /// Muted, or no format both peers support.
    Off,
    Pcm,
    Opus,
}

pub(super) enum AudioEvent<'a> {
    SetAudio {
        enabled: bool,
        formats: &'a [AudioFormat],
    },
    ViewerCapabilities,
    /// No viewer message decided the mode in time.
    Backstop,
}

impl AudioMode {
    pub(super) fn captures(self) -> bool {
        matches!(self, Self::Legacy | Self::Pcm | Self::Opus)
    }

    /// `supported` lists the formats this Agent can send.
    pub(super) fn next(self, event: AudioEvent<'_>, supported: &[AudioFormat]) -> Self {
        match event {
            AudioEvent::SetAudio { enabled: false, .. } => Self::Off,
            AudioEvent::SetAudio {
                enabled: true,
                formats,
            } => formats
                .iter()
                .find(|format| supported.contains(format))
                .map_or(Self::Off, |format| match format {
                    AudioFormat::Opus => Self::Opus,
                    AudioFormat::Pcm16 => Self::Pcm,
                    AudioFormat::Unsupported => Self::Off,
                }),
            // A viewer that sends `SetAudio` does so before its capabilities.
            AudioEvent::ViewerCapabilities | AudioEvent::Backstop if self == Self::Undetermined => {
                Self::Legacy
            }
            AudioEvent::ViewerCapabilities | AudioEvent::Backstop => self,
        }
    }
}

/// Queue at most ~150 ms of the active stream in the data channel.
pub(super) fn buffered_audio_limit(bits_per_second: u32) -> usize {
    bits_per_second as usize * 150 / 8_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_buffer_limit_follows_the_stream_rate() {
        assert_eq!(buffered_audio_limit(1_536_000), 28_800);
        assert_eq!(buffered_audio_limit(110_000), 2_062);
    }

    const BOTH: &[AudioFormat] = &[AudioFormat::Opus, AudioFormat::Pcm16];
    const PCM: &[AudioFormat] = &[AudioFormat::Pcm16];

    fn set(enabled: bool, formats: &[AudioFormat]) -> AudioEvent<'_> {
        AudioEvent::SetAudio { enabled, formats }
    }

    #[test]
    fn a_viewer_without_set_audio_gets_legacy_pcm() {
        let mode = AudioMode::Undetermined;
        assert!(!mode.captures());
        assert_eq!(
            mode.next(AudioEvent::ViewerCapabilities, BOTH),
            AudioMode::Legacy
        );
        assert_eq!(mode.next(AudioEvent::Backstop, BOTH), AudioMode::Legacy);
        assert!(AudioMode::Legacy.captures());
    }

    #[test]
    fn set_audio_decides_before_capabilities_arrive() {
        let off = AudioMode::Undetermined.next(set(false, BOTH), BOTH);
        assert_eq!(off, AudioMode::Off);
        assert!(!off.captures());
        assert_eq!(off.next(AudioEvent::ViewerCapabilities, BOTH), off);
        assert_eq!(off.next(AudioEvent::Backstop, BOTH), off);
        let opus = off.next(set(true, &[AudioFormat::Opus]), BOTH);
        assert_eq!(opus, AudioMode::Opus);
        assert_eq!(opus.next(AudioEvent::ViewerCapabilities, BOTH), opus);
        assert_eq!(opus.next(set(true, PCM), BOTH), AudioMode::Pcm);
        assert_eq!(
            AudioMode::Legacy.next(set(false, BOTH), BOTH),
            AudioMode::Off
        );
    }

    #[test]
    fn the_first_format_both_peers_support_wins() {
        let mode = AudioMode::Undetermined;
        assert_eq!(mode.next(set(true, BOTH), BOTH), AudioMode::Opus);
        assert_eq!(mode.next(set(true, BOTH), PCM), AudioMode::Pcm);
        assert_eq!(
            mode.next(set(true, &[AudioFormat::Pcm16, AudioFormat::Opus]), BOTH),
            AudioMode::Pcm
        );
        assert_eq!(
            mode.next(
                set(true, &[AudioFormat::Unsupported, AudioFormat::Opus]),
                BOTH
            ),
            AudioMode::Opus
        );
        assert_eq!(
            mode.next(set(true, &[AudioFormat::Opus]), PCM),
            AudioMode::Off
        );
        assert_eq!(mode.next(set(true, &[]), BOTH), AudioMode::Off);
    }
}
