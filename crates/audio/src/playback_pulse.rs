//! Playback through the sound server, over the PulseAudio protocol that both
//! PulseAudio and PipeWire's `pipewire-pulse` serve. A thread writes one
//! period at a time, and the server's buffer paces it.
use super::*;
use crate::pulse::{BufferAttributes, Direction, Stream, StreamOptions};
use anyhow::Context;
use std::sync::atomic::{AtomicBool, Ordering};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// 10 ms, the period the playback queue assumes.
const PERIOD_FRAMES: usize = RATE as usize / 100;
const PERIOD_BYTES: usize = PERIOD_FRAMES * CHANNELS * 2;

pub(super) struct Output {
    stopped: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl Output {
    pub(super) fn open(state: PlaybackState) -> anyhow::Result<Self> {
        let stream = Stream::open(StreamOptions {
            direction: Direction::Playback,
            application: c"MeshRMM Remote",
            stream: c"Remote audio",
            device: None,
            rate: RATE,
            channels: CHANNELS as u8,
            // The server holds two periods, so a write waits about one
            // period and what the queue buffers is nearly all the latency.
            attributes: BufferAttributes {
                tlength: 2 * PERIOD_BYTES as u32,
                ..BufferAttributes::default()
            },
        })
        .context("no audio output")?;
        state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jitter
            .configure(RATE, CHANNELS);
        let stopped = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (thread_stopped, thread_failed) = (Arc::clone(&stopped), Arc::clone(&failed));
        std::thread::Builder::new()
            .name("meshrmm-audio-output".into())
            .spawn(move || {
                let mut samples = [0.0_f32; PERIOD_FRAMES * CHANNELS];
                let mut bytes = [0_u8; PERIOD_BYTES];
                while !thread_stopped.load(Ordering::Relaxed) {
                    {
                        let mut buffer = state.0.lock().unwrap_or_else(|e| e.into_inner());
                        if buffer.muted {
                            samples.fill(0.0);
                        } else {
                            buffer.jitter.fill(&mut samples, 0.0, |sample| sample);
                        }
                    }
                    for (sample, bytes) in samples.iter().zip(bytes.chunks_exact_mut(2)) {
                        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                        bytes.copy_from_slice(&value.to_le_bytes());
                    }
                    if let Err(error) = stream.write(&bytes) {
                        tracing::warn!(%error, "audio output failed");
                        thread_failed.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            })
            .context("could not start the audio output thread")?;
        tracing::info!(rate = RATE, channels = CHANNELS, "audio playback started");
        Ok(Self { stopped, failed })
    }

    pub(super) fn rate(&self) -> u32 {
        RATE
    }

    pub(super) fn channels(&self) -> usize {
        CHANNELS
    }

    /// The sound server moves the stream itself when the default output
    /// changes, so only a failed write ends it.
    pub(super) fn healthy(&self) -> bool {
        !self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // A write in progress returns within a period; the thread then stops.
        self.stopped.store(true, Ordering::Relaxed);
    }
}
