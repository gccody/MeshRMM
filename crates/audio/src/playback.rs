use super::*;
use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// A received audio packet, tagged with the mute generation it arrived in.
enum Incoming {
    Pcm(Vec<u8>),
    Opus(Vec<u8>),
}

#[derive(Clone)]
pub struct Player {
    sender: std::sync::mpsc::SyncSender<(u64, Incoming)>,
    state: PlaybackState,
}
impl Player {
    pub fn new(state: PlaybackState) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<(u64, Incoming)>(8);
        let playback = state.clone();
        let result = std::thread::Builder::new()
            .name("meshrmm-audio-playback".into())
            .spawn(move || {
                if let Err(error) = run(playback, receiver) {
                    tracing::warn!(%error, "audio playback unavailable");
                }
            });
        if let Err(error) = result {
            tracing::warn!(%error, "could not start audio playback");
        }
        Self { sender, state }
    }
    /// Queues a packet from the PCM16 v1 channel.
    pub fn receive(&self, bytes: &[u8]) {
        if Packet::decode(bytes).is_some() {
            self.queue(Incoming::Pcm(bytes.to_vec()));
        }
    }
    /// Queues a packet from the Opus channel.
    pub fn receive_opus(&self, bytes: &[u8]) {
        if super::opus::valid_packet(bytes) {
            self.queue(Incoming::Opus(bytes.to_vec()));
        }
    }
    fn queue(&self, incoming: Incoming) {
        if let Ok(buffer) = self.state.0.lock() {
            let _ = self.sender.try_send((buffer.generation, incoming));
        }
    }
}
struct Output {
    _stream: cpal::Stream,
    config: cpal::StreamConfig,
    name: String,
    failed: Arc<std::sync::atomic::AtomicBool>,
}
impl Output {
    fn open(state: PlaybackState) -> anyhow::Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("no audio output device")?;
        let name = device.name()?;
        let format = device.default_output_config()?;
        let config = format.config();
        anyhow::ensure!(
            (1..=8).contains(&config.channels) && (8_000..=192_000).contains(&config.sample_rate.0),
            "unsupported audio output format"
        );
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stream = match format.sample_format() {
            cpal::SampleFormat::F32 => output::<f32>(&device, &config, state, failed.clone())?,
            cpal::SampleFormat::I16 => output::<i16>(&device, &config, state, failed.clone())?,
            cpal::SampleFormat::U16 => output::<u16>(&device, &config, state, failed.clone())?,
            other => anyhow::bail!("unsupported output sample format: {other}"),
        };
        stream.play()?;
        tracing::info!(
            rate = config.sample_rate.0,
            channels = config.channels,
            "audio playback started"
        );
        Ok(Self {
            _stream: stream,
            config,
            name,
            failed,
        })
    }
    fn healthy(&self) -> bool {
        !self.failed.load(std::sync::atomic::Ordering::Relaxed)
            && cpal::default_host()
                .default_output_device()
                .and_then(|d| d.name().ok())
                .as_deref()
                == Some(self.name.as_str())
    }
}
/// Codec history belongs to uninterrupted playback, not the network session.
/// Skipped packets while muted or without an output must not leave an old
/// 16-bit Opus sequence number (or samples from the old device) behind.
#[derive(Default)]
struct DecodeState {
    resampler: Resampler,
    opus: Option<super::opus::OpusDecoder>,
}
impl DecodeState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn decode(&mut self, incoming: Incoming, rate_out: u32, channels_out: usize) -> Vec<f32> {
        match incoming {
            Incoming::Pcm(bytes) => match Packet::decode(&bytes) {
                Some(packet) => {
                    self.opus = None;
                    self.resampler.convert(packet, rate_out, channels_out)
                }
                None => Vec::new(),
            },
            Incoming::Opus(bytes) => {
                if self.opus.is_none() {
                    match super::opus::OpusDecoder::new() {
                        Ok(created) => self.opus = Some(created),
                        Err(error) => {
                            tracing::warn!(%error, "Opus decoder unavailable");
                            return Vec::new();
                        }
                    }
                }
                let Some(decoder) = self.opus.as_mut() else {
                    return Vec::new();
                };
                let decoded = decoder.decode(&bytes);
                self.resampler.convert_samples(
                    super::opus::RATE,
                    super::opus::CHANNELS,
                    &decoded,
                    rate_out,
                    channels_out,
                )
            }
        }
    }
}

fn run(
    state: PlaybackState,
    receiver: std::sync::mpsc::Receiver<(u64, Incoming)>,
) -> anyhow::Result<()> {
    let mut output = None::<Output>;
    let mut retry = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let mut decoding = DecodeState::default();
    let mut received = None::<&str>;
    let mut generation = 0;
    while let Ok((packet_generation, incoming)) = receiver.recv() {
        let (format, rate, channels) = match &incoming {
            Incoming::Pcm(bytes) => {
                let Some(packet) = Packet::decode(bytes) else {
                    continue;
                };
                ("pcm16", packet.rate, packet.channels)
            }
            Incoming::Opus(_) => ("opus", super::opus::RATE, super::opus::CHANNELS),
        };
        if received != Some(format) {
            tracing::info!(
                format,
                rate,
                channels,
                muted = state.muted(),
                "system audio stream received"
            );
            received = Some(format);
        }
        if retry.elapsed() >= std::time::Duration::from_secs(2) {
            retry = std::time::Instant::now();
            if !output.as_ref().is_some_and(Output::healthy) {
                output = None;
                state.clear();
                decoding.reset();
                match Output::open(state.clone()) {
                    Ok(stream) => output = Some(stream),
                    Err(error) => tracing::debug!(%error, "audio playback unavailable; retrying"),
                }
            }
        }
        let Some(output) = &output else {
            decoding.reset();
            continue;
        };
        let buffer = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if buffer.muted || buffer.generation != packet_generation {
            decoding.reset();
            continue;
        }
        drop(buffer);
        if generation != packet_generation {
            decoding.reset();
            generation = packet_generation;
        }
        let rate_out = output.config.sample_rate.0;
        let channels_out = usize::from(output.config.channels);
        let samples = decoding.decode(incoming, rate_out, channels_out);
        let mut buffer = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if buffer.muted || buffer.generation != packet_generation {
            continue;
        }
        // Bound accumulated latency to 100 ms, even when output is stalled.
        let limit = rate_out as usize * channels_out / 10;
        if buffer.samples.len() + samples.len() > limit {
            buffer.samples.clear();
        }
        buffer.samples.extend(samples.into_iter().take(limit));
    }
    state.clear();
    Ok(())
}
fn output<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: PlaybackState,
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            // Never wait on the network worker from the real-time audio callback.
            if let Ok(mut buffer) = state.0.try_lock() {
                for sample in data {
                    *sample = T::from_sample(if buffer.muted {
                        0.0
                    } else {
                        buffer.samples.pop_front().unwrap_or(0.0)
                    });
                }
            } else {
                data.fill(T::from_sample(0.0));
            }
        },
        move |error| {
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(%error, "audio output failed");
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus_packet(sequence: u16) -> Incoming {
        let mut encoder =
            ::opus::Encoder::new(48_000, ::opus::Channels::Stereo, ::opus::Application::Audio)
                .unwrap();
        let tone: Vec<f32> = (0..960)
            .flat_map(|i| {
                let value = (i as f32 * 0.1).sin() * 0.25;
                [value, value]
            })
            .collect();
        let mut bytes = vec![0; 1277];
        bytes[..2].copy_from_slice(&sequence.to_le_bytes());
        let length = encoder.encode_float(&tone, &mut bytes[2..]).unwrap();
        bytes.truncate(length + 2);
        Incoming::Opus(bytes)
    }

    #[test]
    fn playback_resumes_after_more_than_half_a_sequence_cycle_was_skipped() {
        let mut decoding = DecodeState::default();
        assert!(!decoding.decode(opus_packet(0), 48_000, 2).is_empty());
        // The sender kept playing while the output device was unavailable.
        // 40,000 packets is 13m20s; the old expected sequence classifies these
        // as late packets until the next wrap, causing minutes of silence.
        decoding.reset();
        let resumed = decoding.decode(opus_packet(40_001), 48_000, 2);
        // Linear resampling retains one stereo frame for interpolation.
        assert_eq!(resumed.len(), 1918);
        assert!(resumed.iter().any(|sample| sample.abs() > 0.01));
        // Reordering protection still applies within uninterrupted playback.
        assert!(decoding.decode(opus_packet(40_001), 48_000, 2).is_empty());
        assert_eq!(decoding.decode(opus_packet(40_002), 48_000, 2).len(), 1920);
    }

    #[test]
    fn resumed_playback_discards_old_codec_and_resampler_history() {
        let mut decoding = DecodeState::default();
        decoding.decode(opus_packet(123), 44_100, 1);
        decoding.reset();
        let actual = decoding.decode(opus_packet(50_000), 44_100, 1);
        let expected = DecodeState::default().decode(opus_packet(50_000), 44_100, 1);
        assert_eq!(actual, expected);
        assert!(!actual.is_empty());
    }
}
