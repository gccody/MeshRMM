//! Bounded, independent system-audio transport. v1 carries interleaved PCM16.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const CHANNEL: &str = "meshrmm-audio-v1";
pub const PROTOCOL: &str = "meshrmm.audio.pcm16.v1";
const HEADER: usize = 6;
const MAX_PACKET: usize = 16_000;

#[cfg(any(windows, target_os = "macos"))]
mod playback;
#[cfg(any(windows, target_os = "macos"))]
pub use playback::Player;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
pub use capture::{Capture, capture};

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
        Self(Arc::new(Mutex::new(Buffer {
            muted: true,
            generation: 0,
            samples: VecDeque::new(),
        })))
    }
}
impl PlaybackState {
    pub fn muted(&self) -> bool {
        self.0.lock().map_or(true, |b| b.muted)
    }
    pub fn toggle(&self) {
        if let Ok(mut b) = self.0.lock() {
            b.muted = !b.muted;
            b.generation = b.generation.wrapping_add(1);
            b.samples.clear();
            tracing::info!(muted = b.muted, "viewer audio mute changed");
        }
    }
    pub fn clear(&self) {
        if let Ok(mut b) = self.0.lock() {
            b.samples.clear();
        }
    }
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
        if self.format != (packet.rate, packet.channels) {
            *self = Self {
                format: (packet.rate, packet.channels),
                ..Self::default()
            };
        }
        let frames = packet.samples.len() / (packet.channels * 2);
        let mut output =
            Vec::with_capacity((frames * rate as usize / packet.rate as usize + 1) * channels);
        for frame in packet.samples.chunks_exact(packet.channels * 2) {
            let mut current = [0.0; 8];
            let mut counts = [0; 8];
            for (index, sample) in frame.chunks_exact(2).enumerate() {
                let sample = f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0;
                if packet.channels == 1 {
                    current.fill(sample);
                    counts.fill(1);
                } else {
                    current[index % channels] += sample;
                    counts[index % channels] += 1;
                }
            }
            for (sample, count) in current.iter_mut().zip(counts) {
                if count > 0 {
                    *sample /= count as f32;
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
                self.position += f64::from(packet.rate) / f64::from(rate);
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
    }
    #[test]
    fn muted_by_default_and_toggling_discards_buffered_audio() {
        let state = PlaybackState::default();
        assert!(state.muted());
        state.toggle();
        state.0.lock().unwrap().samples.push_back(1.0);
        state.toggle();
        state.toggle();
        assert!(!state.muted());
        assert!(state.0.lock().unwrap().samples.is_empty());
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
