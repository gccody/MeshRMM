//! Playback through cpal: WASAPI on Windows and Core Audio on macOS. The
//! device calls back for each period, and the callback never waits.
use super::*;
use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub(super) struct Output {
    _stream: cpal::Stream,
    config: cpal::StreamConfig,
    name: String,
    failed: Arc<std::sync::atomic::AtomicBool>,
}
impl Output {
    pub(super) fn open(state: PlaybackState) -> anyhow::Result<Self> {
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
        state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jitter
            .configure(config.sample_rate.0, usize::from(config.channels));
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
    pub(super) fn rate(&self) -> u32 {
        self.config.sample_rate.0
    }
    pub(super) fn channels(&self) -> usize {
        usize::from(self.config.channels)
    }
    pub(super) fn healthy(&self) -> bool {
        !self.failed.load(std::sync::atomic::Ordering::Relaxed)
            && cpal::default_host()
                .default_output_device()
                .and_then(|d| d.name().ok())
                .as_deref()
                == Some(self.name.as_str())
    }
}
/// Attempts at the lock before an output callback plays silence instead.
/// Other holders only copy a packet in or read a flag, a microsecond or so,
/// so a contended callback usually gets the lock within a few attempts.
const CALLBACK_LOCK_ATTEMPTS: usize = 256;

/// Never waits on another thread: the output callback is real-time.
fn lock_without_waiting(state: &PlaybackState) -> Option<std::sync::MutexGuard<'_, Buffer>> {
    for _ in 0..CALLBACK_LOCK_ATTEMPTS {
        match state.0.try_lock() {
            Ok(buffer) => return Some(buffer),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
    None
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
            let silence = T::from_sample(0.0);
            match lock_without_waiting(&state) {
                Some(mut buffer) if !buffer.muted => {
                    buffer.jitter.fill(data, silence, |s| T::from_sample(s));
                }
                _ => data.fill(silence),
            }
        },
        move |error| {
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(%error, "audio output failed");
        },
        None,
    )
}
