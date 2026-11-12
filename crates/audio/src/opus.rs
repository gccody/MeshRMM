//! Opus v1: `[sequence: u16 LE][one Opus packet]`, 48 kHz stereo, 20 ms.
use super::*;
use anyhow::Context;

pub(crate) const RATE: u32 = 48_000;
pub(crate) const CHANNELS: usize = 2;
/// Samples (all channels) in one 20 ms frame.
const FRAME: usize = 960 * CHANNELS;
/// The largest Opus packet for one frame.
const MAX_PAYLOAD: usize = 1275;
/// Up to this many lost packets are concealed; a larger jump starts the
/// decoder over.
const MAX_CONCEALED: u16 = 5;
/// Samples (all channels) in the longest packet Opus allows, 120 ms.
const MAX_DECODED: usize = 5760 * CHANNELS;

/// Whether `bytes` is shaped like an Opus channel packet.
pub(crate) fn valid_packet(bytes: &[u8]) -> bool {
    (3..=2 + MAX_PAYLOAD).contains(&bytes.len())
}

/// Encodes PCM16 v1 packets of any rate, channel count, and length into
/// 20 ms Opus channel packets.
pub struct OpusEncoder {
    encoder: ::opus::Encoder,
    resampler: Resampler,
    pending: Vec<f32>,
    sequence: u16,
    /// Whether anything was encoded since the last reset.
    started: bool,
}
impl OpusEncoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut encoder =
            ::opus::Encoder::new(RATE, ::opus::Channels::Stereo, ::opus::Application::Audio)?;
        encoder.set_bitrate(::opus::Bitrate::Bits(96_000))?;
        encoder.set_vbr(true)?;
        encoder.set_vbr_constraint(true)?;
        encoder.set_complexity(5)?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(5)?;
        Ok(Self {
            encoder,
            resampler: Resampler::default(),
            pending: Vec::with_capacity(FRAME * 2),
            sequence: 0,
            started: false,
        })
    }
    /// Returns the Opus channel packets completed by one PCM16 v1 packet.
    pub fn encode(&mut self, pcm: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
        let packet = Packet::decode(pcm).context("malformed PCM16 packet")?;
        self.pending
            .extend(self.resampler.convert(packet, RATE, CHANNELS));
        let mut packets = Vec::new();
        while self.pending.len() >= FRAME {
            let mut bytes = vec![0; 2 + MAX_PAYLOAD];
            bytes[..2].copy_from_slice(&self.sequence.to_le_bytes());
            let length = self
                .encoder
                .encode_float(&self.pending[..FRAME], &mut bytes[2..])?;
            bytes.truncate(2 + length);
            self.pending.drain(..FRAME);
            self.sequence = self.sequence.wrapping_add(1);
            self.started = true;
            packets.push(bytes);
        }
        Ok(packets)
    }
    /// Starts a new stream after a gap in capture. The sequence skips ahead
    /// so the viewer's decoder starts over too, rather than concealing.
    pub fn reset(&mut self) {
        if !self.started {
            return;
        }
        if let Err(error) = self.encoder.reset_state() {
            tracing::warn!(%error, "could not reset the Opus encoder");
        }
        self.resampler = Resampler::default();
        self.pending.clear();
        self.sequence = self.sequence.wrapping_add(MAX_CONCEALED + 1);
        self.started = false;
    }
}

/// Decodes Opus channel packets to 48 kHz stereo, concealing short losses.
pub(crate) struct OpusDecoder {
    decoder: ::opus::Decoder,
    expected: Option<u16>,
    output: Vec<f32>,
}
impl OpusDecoder {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            decoder: ::opus::Decoder::new(RATE, ::opus::Channels::Stereo)?,
            expected: None,
            output: vec![0.0; MAX_DECODED],
        })
    }
    /// Returns interleaved stereo samples for `bytes`, preceded by any
    /// concealment for packets lost just before it.
    pub(crate) fn decode(&mut self, bytes: &[u8]) -> Vec<f32> {
        if !valid_packet(bytes) {
            return Vec::new();
        }
        let sequence = u16::from_le_bytes([bytes[0], bytes[1]]);
        let payload = &bytes[2..];
        let mut samples = Vec::new();
        if let Some(expected) = self.expected {
            let missing = sequence.wrapping_sub(expected);
            if missing > u16::MAX / 2 {
                // Older than one already played.
                return samples;
            }
            if missing > MAX_CONCEALED {
                self.reset();
            } else if missing > 0 {
                // Conceal all but the last lost packet; this packet's
                // in-band FEC restores the last one.
                for _ in 1..missing {
                    self.decode_into(&[], false, &mut samples);
                }
                self.decode_into(payload, true, &mut samples);
            }
        }
        self.decode_into(payload, false, &mut samples);
        self.expected = Some(sequence.wrapping_add(1));
        samples
    }
    fn decode_into(&mut self, payload: &[u8], fec: bool, samples: &mut Vec<f32>) {
        // Concealment and FEC produce exactly one lost frame.
        let size = if payload.is_empty() || fec {
            FRAME
        } else {
            MAX_DECODED
        };
        match self
            .decoder
            .decode_float(payload, &mut self.output[..size], fec)
        {
            Ok(frames) => samples.extend_from_slice(&self.output[..frames * CHANNELS]),
            Err(error) => {
                tracing::debug!(%error, "discarding undecodable Opus packet");
                self.reset();
            }
        }
    }
    fn reset(&mut self) {
        if let Err(error) = self.decoder.reset_state() {
            tracing::warn!(%error, "could not reset the Opus decoder");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT_RATE: u32 = 44_100;
    const TONE: f64 = 997.0;

    fn tone(rate: u32, start: usize, frames: usize) -> Vec<i16> {
        (start..start + frames)
            .flat_map(|frame| {
                let t = frame as f64 / f64::from(rate);
                let left = (0.5 * (2.0 * std::f64::consts::PI * TONE * t).sin() * 32767.0) as i16;
                [left, left / 2]
            })
            .collect()
    }
    fn pcm(rate: u32, samples: &[i16]) -> Vec<u8> {
        let mut bytes = rate.to_le_bytes().to_vec();
        bytes.extend(2_u16.to_le_bytes());
        bytes.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        bytes
    }
    /// One second of tone at 44.1 kHz in uneven packets around 10 ms.
    fn encoded_tone(encoder: &mut OpusEncoder) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut frame = 0;
        for size in [441, 300, 582].into_iter().cycle() {
            if frame >= INPUT_RATE as usize {
                break;
            }
            packets.extend(
                encoder
                    .encode(&pcm(INPUT_RATE, &tone(INPUT_RATE, frame, size)))
                    .unwrap(),
            );
            frame += size;
        }
        packets
    }

    #[test]
    fn opus_round_trip_keeps_the_signal() {
        let mut encoder = OpusEncoder::new().unwrap();
        let packets = encoded_tone(&mut encoder);
        assert!(packets.len() >= 45);
        assert!(packets.iter().all(|p| valid_packet(p)));
        let bits: usize = packets.iter().map(|p| p.len() * 8).sum();
        let seconds = packets.len() as f64 * 0.02;
        assert!(
            bits as f64 / seconds < 110_000.0,
            "{} bit/s",
            bits as f64 / seconds
        );
        let mut decoder = OpusDecoder::new().unwrap();
        let decoded: Vec<f32> = packets.iter().flat_map(|p| decoder.decode(p)).collect();
        assert_eq!(decoded.len(), packets.len() * FRAME);
        // Compare the left channel against the tone after the codec's delay,
        // skipping the start-up.
        let left: Vec<f64> = decoded.iter().step_by(2).map(|&s| f64::from(s)).collect();
        let reference = |n: usize, lag: usize| {
            let t = (n as f64 - lag as f64) / f64::from(RATE);
            0.5 * (2.0 * std::f64::consts::PI * TONE * t).sin()
        };
        let window = 4_800..left.len() - 960;
        let snr = (0..1_000)
            .map(|lag| {
                let (signal, noise) = window.clone().fold((0.0, 0.0), |(s, e), n| {
                    let r = reference(n, lag);
                    (s + r * r, e + (r - left[n]).powi(2))
                });
                10.0 * (signal / noise).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(snr > 30.0, "SNR {snr:.1} dB");
    }

    #[test]
    fn short_losses_are_concealed_and_long_ones_reset() {
        let mut encoder = OpusEncoder::new().unwrap();
        let packets = encoded_tone(&mut encoder);
        let mut decoder = OpusDecoder::new().unwrap();
        assert_eq!(decoder.decode(&packets[0]).len(), FRAME);
        // One lost packet: FEC for it, then this one.
        assert_eq!(decoder.decode(&packets[2]).len(), 2 * FRAME);
        // Four lost: three concealed, one from FEC, then this one.
        assert_eq!(decoder.decode(&packets[7]).len(), 5 * FRAME);
        // A packet older than one already played is dropped.
        assert!(decoder.decode(&packets[5]).is_empty());
        // Ten lost: start over rather than conceal 200 ms.
        assert_eq!(decoder.decode(&packets[18]).len(), FRAME);
        assert!(decoder.decode(&[0, 0]).is_empty());
        assert!(decoder.decode(&vec![0; 2 + MAX_PAYLOAD + 1]).is_empty());
        // A malformed payload resets the decoder rather than failing it.
        let mut malformed = packets[19].clone();
        malformed.truncate(3);
        malformed[2] = 0xff;
        decoder.decode(&malformed);
        assert_eq!(decoder.decode(&packets[20]).len(), FRAME);
    }

    #[test]
    fn an_encoder_reset_makes_the_decoder_start_over() {
        let mut encoder = OpusEncoder::new().unwrap();
        let first = encoded_tone(&mut encoder);
        encoder.reset();
        // Resetting twice without new audio skips ahead only once.
        encoder.reset();
        let second = encoded_tone(&mut encoder);
        let sequence = |p: &[u8]| u16::from_le_bytes([p[0], p[1]]);
        assert_eq!(
            sequence(&second[0]),
            sequence(first.last().unwrap()).wrapping_add(MAX_CONCEALED + 2)
        );
        let mut decoder = OpusDecoder::new().unwrap();
        decoder.decode(first.last().unwrap());
        assert_eq!(decoder.decode(&second[0]).len(), FRAME);
        assert!(encoder.encode(&[1, 2, 3]).is_err());
    }
}
