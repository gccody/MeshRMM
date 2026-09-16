use super::*;
use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

#[derive(Clone)]
pub struct Player {
    sender: std::sync::mpsc::SyncSender<(u64, Vec<u8>)>,
    state: PlaybackState,
}
impl Player {
    pub fn new(state: PlaybackState) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<(u64, Vec<u8>)>(8);
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
    pub fn receive(&self, bytes: &[u8]) {
        if Packet::decode(bytes).is_some()
            && let Ok(buffer) = self.state.0.lock()
        {
            let _ = self.sender.try_send((buffer.generation, bytes.to_vec()));
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
    receiver: std::sync::mpsc::Receiver<(u64, Vec<u8>)>,
) -> anyhow::Result<()> {
    let mut output = None::<Output>;
    let mut retry = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let mut resampler = Resampler::default();
    let mut received = false;
    let mut generation = 0;
    while let Ok((packet_generation, bytes)) = receiver.recv() {
        let Some(packet) = Packet::decode(&bytes) else {
            continue;
        };
        if !received {
            tracing::info!(
                rate = packet.rate,
                channels = packet.channels,
                muted = state.muted(),
                "system audio stream received"
            );
            received = true;
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
        let samples = resampler.convert(
            packet,
            output.config.sample_rate.0,
            usize::from(output.config.channels),
        );
        let mut buffer = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if buffer.muted || buffer.generation != packet_generation {
            continue;
        }
        // Bound accumulated latency to 100 ms, even when output is stalled.
        let limit = output.config.sample_rate.0 as usize * usize::from(output.config.channels) / 10;
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
