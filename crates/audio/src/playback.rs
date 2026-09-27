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
fn run(
    state: PlaybackState,
    receiver: std::sync::mpsc::Receiver<(u64, Incoming)>,
) -> anyhow::Result<()> {
    let mut output = None::<Output>;
    let mut retry = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let mut resampler = Resampler::default();
    let mut decoder = None::<super::opus::OpusDecoder>;
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
                resampler = Resampler::default();
                match Output::open(state.clone()) {
                    Ok(stream) => output = Some(stream),
                    Err(error) => tracing::debug!(%error, "audio playback unavailable; retrying"),
                }
            }
        }
        let Some(output) = &output else {
            continue;
        };
        let buffer = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if buffer.muted || buffer.generation != packet_generation {
            resampler = Resampler::default();
            continue;
        }
        drop(buffer);
        if generation != packet_generation {
            resampler = Resampler::default();
            generation = packet_generation;
        }
        let rate_out = output.config.sample_rate.0;
        let channels_out = usize::from(output.config.channels);
        let samples = match incoming {
            Incoming::Pcm(bytes) => match Packet::decode(&bytes) {
                Some(packet) => resampler.convert(packet, rate_out, channels_out),
                None => continue,
            },
            Incoming::Opus(bytes) => {
                if decoder.is_none() {
                    match super::opus::OpusDecoder::new() {
                        Ok(created) => decoder = Some(created),
                        Err(error) => {
                            tracing::warn!(%error, "Opus decoder unavailable");
                            continue;
                        }
                    }
                }
                let Some(decoder) = decoder.as_mut() else {
                    continue;
                };
                let decoded = decoder.decode(&bytes);
                resampler.convert_samples(
                    super::opus::RATE,
                    super::opus::CHANNELS,
                    &decoded,
                    rate_out,
                    channels_out,
                )
            }
        };
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
