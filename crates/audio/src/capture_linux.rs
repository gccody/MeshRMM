//! Linux system audio: the monitor of the default output, through the
//! PulseAudio protocol that both PulseAudio and PipeWire's `pipewire-pulse`
//! serve. `libpulse-simple` is loaded when capture starts, so the Agent runs
//! on computers without it and only audio is unavailable there. It must run
//! as the user whose sound server it captures, with their environment.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use super::pulse::{BufferAttributes, Direction, Stream, StreamOptions};
use super::{HEADER, MAX_PACKET};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
/// 10 ms, like the other platforms' packets.
const PACKET_BYTES: usize = SAMPLE_RATE as usize / 100 * CHANNELS as usize * 2;

fn open() -> anyhow::Result<Stream> {
    Stream::open(StreamOptions {
        direction: Direction::Record,
        application: c"MeshRMM Agent",
        stream: c"System audio",
        device: Some(c"@DEFAULT_MONITOR@"),
        rate: SAMPLE_RATE,
        channels: CHANNELS,
        // Small fragments keep the latency near one packet; the rest are
        // the server's defaults.
        attributes: BufferAttributes {
            fragsize: PACKET_BYTES as u32,
            ..BufferAttributes::default()
        },
    })
    .context("could not record the sound server's output")
}

/// System audio being captured; capture stops when it is dropped.
pub struct Capture {
    stopped: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl Capture {
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // A read in progress returns within a packet; the thread then stops.
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// Starts capturing the default output's monitor, sending 10 ms PCM16
/// packets to `send`.
pub fn capture(send: impl Fn(Vec<u8>) + Send + 'static) -> anyhow::Result<Capture> {
    let stream = open()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let (thread_stopped, thread_failed) = (Arc::clone(&stopped), Arc::clone(&failed));
    std::thread::Builder::new()
        .name("meshrmm-audio-capture".into())
        .spawn(move || {
            let mut samples = vec![0_u8; PACKET_BYTES];
            while !thread_stopped.load(Ordering::SeqCst) {
                if let Err(error) = stream.read(&mut samples) {
                    tracing::warn!(%error, "system audio capture stopped");
                    thread_failed.store(true, Ordering::SeqCst);
                    break;
                }
                let mut packet = Vec::with_capacity(HEADER + samples.len());
                packet.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
                packet.extend_from_slice(&u16::from(CHANNELS).to_le_bytes());
                packet.extend_from_slice(&samples);
                debug_assert!(packet.len() <= MAX_PACKET);
                send(packet);
            }
        })
        .context("could not start the audio capture thread")?;
    Ok(Capture { stopped, failed })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a desktop session with a PulseAudio or PipeWire sound server.
    #[test]
    #[ignore = "records the sound server's output"]
    fn captures_ten_millisecond_packets() {
        let (sender, packets) = std::sync::mpsc::channel();
        let capture = capture(move |packet| {
            let _ = sender.send(packet);
        })
        .unwrap();
        let packet = packets
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(packet.len(), HEADER + PACKET_BYTES);
        assert_eq!(super::super::pcm_bits_per_second(&packet), Some(1_536_000));
        assert!(capture.healthy());
    }
}
