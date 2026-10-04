//! Bounded, independent system-audio transport. v1 carries interleaved
//! PCM16; the Opus channel carries 48 kHz stereo Opus.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const CHANNEL: &str = "meshrmm-audio-v1";
pub const PROTOCOL: &str = "meshrmm.audio.pcm16.v1";
pub const OPUS_CHANNEL: &str = "meshrmm-audio-opus-v1";
pub const OPUS_PROTOCOL: &str = "meshrmm.audio.opus.v1";
/// The Opus stream's nominal rate: 96 kbps constrained VBR plus framing.
pub const OPUS_BITS_PER_SECOND: u32 = 110_000;
/// Third-party license notices for code built into this crate (libopus).
pub const THIRD_PARTY_NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES.txt");
const HEADER: usize = 6;
const MAX_PACKET: usize = 16_000;

#[cfg(any(windows, target_os = "macos"))]
mod opus;
#[cfg(any(windows, target_os = "macos"))]
pub use opus::OpusEncoder;
#[cfg(any(windows, target_os = "macos"))]
mod playback;
#[cfg(any(windows, target_os = "macos"))]
pub use playback::Player;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
pub use capture::{Capture, capture};

#[cfg(target_os = "macos")]
mod capture_macos;
#[cfg(target_os = "macos")]
pub use capture_macos::{Capture, capture};

struct Packet<'a> {
    rate: u32,
    channels: usize,
    samples: &'a [u8],
}
impl<'a> Packet<'a> {
    fn decode(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() <= HEADER || bytes.len() > MAX_PACKET {
            return None;
        }
        let rate = u32::from_le_bytes(bytes[..4].try_into().ok()?);
        let channels = usize::from(u16::from_le_bytes(bytes[4..6].try_into().ok()?));
        if !(8_000..=192_000).contains(&rate)
            || !(1..=8).contains(&channels)
            || !(bytes.len() - HEADER).is_multiple_of(channels * 2)
        {
            return None;
        }
        Some(Self {
            rate,
            channels,
            samples: &bytes[HEADER..],
        })
    }
}

/// The nominal bitrate of a PCM16 packet's stream, or `None` when the packet
/// is malformed.
pub fn pcm_bits_per_second(bytes: &[u8]) -> Option<u32> {
    let packet = Packet::decode(bytes)?;
    Some(packet.rate * packet.channels as u32 * 16)
}

/// Mute and queue share a lock so toggling cannot replay previously buffered sound.
#[derive(Clone)]
pub struct PlaybackState(Arc<Mutex<Buffer>>);
struct Buffer {
    muted: bool,
    generation: u64,
    samples: VecDeque<f32>,
}
impl Default for PlaybackState {
    fn default() -> Self {
        Self::new(true)
    }
}
impl PlaybackState {
    pub fn new(muted: bool) -> Self {
        Self(Arc::new(Mutex::new(Buffer {
            muted,
            generation: 0,
            samples: VecDeque::new(),
        })))
    }
    pub fn muted(&self) -> bool {
        self.0.lock().map_or(true, |b| b.muted)
    }
    /// Returns the new mute state.
    pub fn toggle(&self) -> bool {
        let Ok(mut b) = self.0.lock() else {
            return true;
        };
        b.muted = !b.muted;
        b.generation = b.generation.wrapping_add(1);
        b.samples.clear();
        tracing::info!(muted = b.muted, "viewer audio mute changed");
        b.muted
    }
    pub fn clear(&self) {
        if let Ok(mut b) = self.0.lock() {
            b.samples.clear();
        }
    }
}

/// Mixes one interleaved frame down to stereo, assuming the WASAPI channel
/// order FL, FR, FC, LFE, BL, BR, SL, SR. The centre reaches both sides
/// equally and LFE is dropped. Layouts without a known mask alternate
/// channels between the sides. Each side is normalised by its gains, and
/// float input above full scale is clamped.
fn downmix(frame: &[f32]) -> [f32; 2] {
    const HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;
    // (channel, gain) pairs for one side.
    type Gains = &'static [(usize, f32)];
    let (left, right): (Gains, Gains) = match frame.len() {
        1 => (&[(0, 1.0)], &[(0, 1.0)]),
        2 => (&[(0, 1.0)], &[(1, 1.0)]),
        // Quad: FL FR BL BR.
        4 => (&[(0, 1.0), (2, HALF)], &[(1, 1.0), (3, HALF)]),
        // 5.1: FL FR FC LFE BL BR (or SL SR).
        6 => (
            &[(0, 1.0), (2, HALF), (4, HALF)],
            &[(1, 1.0), (2, HALF), (5, HALF)],
        ),
        // 7.1: FL FR FC LFE BL BR SL SR.
        8 => (
            &[(0, 1.0), (2, HALF), (4, HALF), (6, HALF)],
            &[(1, 1.0), (2, HALF), (5, HALF), (7, HALF)],
        ),
        _ => {
            let side = |parity: usize| {
                let samples = frame.iter().skip(parity).step_by(2);
                let count = samples.len().max(1) as f32;
                (samples.sum::<f32>() / count).clamp(-1.0, 1.0)
            };
            return [side(0), side(1)];
        }
    };
    let mix = |gains: &[(usize, f32)]| {
        let total: f32 = gains.iter().map(|(_, gain)| gain).sum();
        let sum: f32 = gains.iter().map(|&(index, gain)| frame[index] * gain).sum();
        (sum / total).clamp(-1.0, 1.0)
    };
    [mix(left), mix(right)]
}

/// Stateful linear conversion preserves fractional position across packet boundaries.
#[derive(Default)]
struct Resampler {
    format: (u32, usize),
    previous: [f32; 8],
    initialized: bool,
    position: f64,
}
impl Resampler {
    fn convert(&mut self, packet: Packet<'_>, rate: u32, channels: usize) -> Vec<f32> {
        let samples: Vec<f32> = packet
            .samples
            .chunks_exact(2)
            .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0)
            .collect();
        self.convert_samples(packet.rate, packet.channels, &samples, rate, channels)
    }
    /// Converts interleaved `samples` at `from_rate`/`from_channels` to
    /// `rate`/`channels`. Mono fills every output channel; anything else is
    /// mixed to stereo and fills the first two.
    fn convert_samples(
        &mut self,
        from_rate: u32,
        from_channels: usize,
        samples: &[f32],
        rate: u32,
        channels: usize,
    ) -> Vec<f32> {
        if self.format != (from_rate, from_channels) {
            *self = Self {
                format: (from_rate, from_channels),
                ..Self::default()
            };
        }
        let frames = samples.len() / from_channels;
        let mut output =
            Vec::with_capacity((frames * rate as usize / from_rate as usize + 1) * channels);
        for frame in samples.chunks_exact(from_channels) {
            let mut current = [0.0; 8];
            if from_channels == 1 {
                current.fill(frame[0]);
            } else {
                let [left, right] = downmix(frame);
                if channels == 1 {
                    current[0] = (left + right) / 2.0;
                } else {
                    current[0] = left;
                    current[1] = right;
                }
            }
            if !self.initialized {
                self.initialized = true;
                self.previous = current;
                continue;
            }
            while self.position < 1.0 {
                for (a, b) in self.previous[..channels].iter().zip(&current) {
                    output.push(a + (b - a) * self.position as f32);
                }
                self.position += f64::from(from_rate) / f64::from(rate);
            }
            self.position -= 1.0;
            self.previous = current;
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let mut bytes = rate.to_le_bytes().to_vec();
        bytes.extend(channels.to_le_bytes());
        bytes.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        bytes
    }
    #[test]
    fn rejects_invalid_audio_before_allocation() {
        for bytes in [
            vec![],
            vec![0; MAX_PACKET + 1],
            packet(0, 2, &[0, 0]),
            packet(48_000, 0, &[0]),
            packet(48_000, 2, &[0]),
        ] {
            assert!(Packet::decode(&bytes).is_none());
        }
        assert!(Packet::decode(&packet(48_000, 2, &[1, -1])).is_some());
        assert_eq!(
            pcm_bits_per_second(&packet(48_000, 2, &[1, -1])),
            Some(1_536_000)
        );
        assert_eq!(pcm_bits_per_second(&packet(48_000, 2, &[1])), None);
    }
    #[test]
    fn muted_by_default_and_toggling_discards_buffered_audio() {
        let state = PlaybackState::default();
        assert!(state.muted());
        assert!(!state.toggle());
        state.0.lock().unwrap().samples.push_back(1.0);
        assert!(state.toggle());
        assert!(!state.toggle());
        assert!(!state.muted());
        assert!(state.0.lock().unwrap().samples.is_empty());
        assert!(!PlaybackState::new(false).muted());
    }
    #[test]
    fn downmix_keeps_the_centre_even_and_drops_lfe() {
        const HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(downmix(&[0.5]), [0.5, 0.5]);
        assert_eq!(downmix(&[0.25, -0.5]), [0.25, -0.5]);
        for channels in [4, 6, 8] {
            let mut frame = vec![0.0; channels];
            frame[0] = 1.0;
            let [left, right] = downmix(&frame);
            assert!(left > 0.0 && right == 0.0, "{channels}: FL reaches only L");
            // A full-scale signal on every channel never clips.
            let [left, right] = downmix(&vec![1.0; channels]);
            assert!((left - 1.0).abs() < 1e-6 && (right - 1.0).abs() < 1e-6);
        }
        for channels in [6, 8] {
            let mut centre = vec![0.0; channels];
            centre[2] = 1.0;
            let [left, right] = downmix(&centre);
            assert!(left > 0.0 && left == right, "{channels}: centre is even");
            let mut lfe = vec![0.0; channels];
            lfe[3] = 1.0;
            assert_eq!(downmix(&lfe), [0.0, 0.0], "{channels}: LFE is dropped");
        }
        // 5.1: L = (FL + 0.707 FC + 0.707 BL) / 2.414.
        let [left, right] = downmix(&[1.0, 0.0, 1.0, 1.0, 0.0, 1.0]);
        assert!((left - (1.0 + HALF) / (1.0 + 2.0 * HALF)).abs() < 1e-6);
        assert!((right - 2.0 * HALF / (1.0 + 2.0 * HALF)).abs() < 1e-6);
        // Unknown layouts alternate sides; float input is clamped.
        assert_eq!(downmix(&[1.0, 0.0, 0.5]), [0.75, 0.0]);
        assert_eq!(downmix(&[4.0, -4.0]), [1.0, -1.0]);
    }
    #[test]
    fn resampler_mixes_surround_packets_to_stereo() {
        // 5.1 centre only: equal on both sides, nothing on LFE-only frames.
        let mut samples = Vec::new();
        for _ in 0..8 {
            samples.extend([0, 0, 16_384, 0, 0, 0]);
            samples.extend([0, 0, 0, 16_384, 0, 0]);
        }
        let output = Resampler::default().convert(
            Packet::decode(&packet(48_000, 6, &samples)).unwrap(),
            48_000,
            2,
        );
        assert!(!output.is_empty());
        for frame in output.chunks_exact(2) {
            assert_eq!(frame[0], frame[1]);
        }
        assert!(output.iter().any(|&s| s > 0.0));
        assert!(output.contains(&0.0));
        // Stereo on a mono device averages both sides.
        let output = Resampler::default().convert(
            Packet::decode(&packet(48_000, 2, &[16_384, 0, 16_384, 0])).unwrap(),
            48_000,
            1,
        );
        assert_eq!(output, vec![0.25]);
    }
    #[test]
    fn resampling_is_independent_of_packet_boundaries() {
        let samples: Vec<i16> = (0..1000).collect();
        let whole = packet(44_100, 1, &samples);
        let expected = Resampler::default().convert(Packet::decode(&whole).unwrap(), 48_000, 2);
        let mut resampler = Resampler::default();
        let mut actual = Vec::new();
        for chunk in samples.chunks(37) {
            actual.extend(resampler.convert(
                Packet::decode(&packet(44_100, 1, chunk)).unwrap(),
                48_000,
                2,
            ));
        }
        assert_eq!(actual, expected);
        assert!(actual.chunks_exact(2).all(|c| c[0] == c[1]));
    }
}
