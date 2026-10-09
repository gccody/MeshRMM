use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use meshrmm_protocol::AudioFormat;
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::RTCPeerConnection;

use super::SenderCleanup;
use crate::remote::audio_mode::{AudioEvent, AudioMode, buffered_audio_limit};
use crate::remote::native_task::NativeTask;
use crate::remote::platform::{AudioStream, ScreenInput};

const AUDIO_STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// Without new audio for this long the stream counts as idle: WASAPI
/// loopback delivers nothing while the device is silent.
const AUDIO_IDLE: std::time::Duration = std::time::Duration::from_millis(100);
/// Audio formats this Agent can send.
const SUPPORTED_AUDIO_FORMATS: &[AudioFormat] = &[AudioFormat::Opus, AudioFormat::Pcm16];

pub(super) fn apply_audio_event(mode: &watch::Sender<AudioMode>, event: AudioEvent<'_>) {
    mode.send_if_modified(|mode| {
        let next = mode.next(event, SUPPORTED_AUDIO_FORMATS);
        if next == *mode {
            return false;
        }
        tracing::info!(previous = ?*mode, mode = ?next, "remote audio mode changed");
        *mode = next;
        true
    });
}

pub(super) struct AudioPipeline {
    /// The viewer decides whether and how audio is sent; capture only then.
    pub(super) mode: Arc<watch::Sender<AudioMode>>,
    /// The audio bitrate being streamed now, left out of video pacing.
    pub(super) bits: Arc<AtomicU32>,
}

pub(super) async fn start_audio(
    peer: &RTCPeerConnection,
    input: &Arc<dyn ScreenInput>,
    cleanup: &mut SenderCleanup,
) -> anyhow::Result<AudioPipeline> {
    let audio_channel = peer
        .create_data_channel(
            meshrmm_audio::CHANNEL,
            Some(RTCDataChannelInit {
                ordered: Some(true),
                max_retransmits: Some(0),
                protocol: Some(meshrmm_audio::PROTOCOL.into()),
                ..Default::default()
            }),
        )
        .await?;
    // Created up front: the offer is made before the viewer's version is known.
    let opus_channel = peer
        .create_data_channel(
            meshrmm_audio::OPUS_CHANNEL,
            Some(RTCDataChannelInit {
                ordered: Some(true),
                max_retransmits: Some(0),
                protocol: Some(meshrmm_audio::OPUS_PROTOCOL.into()),
                ..Default::default()
            }),
        )
        .await?;
    let audio_mode = Arc::new(watch::Sender::new(AudioMode::Undetermined));
    let audio_bits = Arc::new(AtomicU32::new(0));
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(8);
    let audio_capture = spawn_audio_capture(Arc::clone(input), audio_mode.subscribe(), audio_tx)?;
    cleanup.workers.push(audio_capture);
    let output = AudioOutput {
        pcm_channel: audio_channel,
        opus_channel,
        opus: None,
        opus_unavailable: false,
    };
    let audio_sender = spawn_audio_sender(
        audio_rx,
        Arc::clone(input),
        audio_mode.subscribe(),
        Arc::clone(&audio_bits),
        output,
    );
    cleanup.tasks.push(audio_sender.abort_handle());
    Ok(AudioPipeline {
        mode: audio_mode,
        bits: audio_bits,
    })
}

fn spawn_audio_capture(
    audio_input: Arc<dyn ScreenInput>,
    mut capture_mode: watch::Receiver<AudioMode>,
    audio_tx: mpsc::Sender<Vec<u8>>,
) -> std::io::Result<NativeTask> {
    NativeTask::spawn("meshrmm-audio-capture", move |mut stop| async move {
        let mut stream: Option<Box<dyn AudioStream>> = None;
        let mut retry = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                // Start or stop at once when the viewer mutes or unmutes.
                Ok(()) = capture_mode.changed() => {}
                _ = retry.tick() => {}
            }
            let mode = *capture_mode.borrow_and_update();
            if !mode.captures() || !audio_input.is_console_session() {
                if stream.take().is_some() {
                    tracing::info!(?mode, "system audio capture stopped");
                }
                continue;
            }
            if stream.as_ref().is_some_and(|stream| stream.healthy()) {
                continue;
            }
            stream = None;
            let sender = audio_tx.clone();
            match audio_input.start_audio(Box::new(move |packet| {
                let _ = sender.try_send(packet);
            })) {
                Ok(capture) => stream = Some(capture),
                Err(error) => tracing::debug!(%error, "system audio unavailable; retrying"),
            }
        }
    })
}

/// Encodes captured audio in the mode the viewer chose and picks the channel
/// that carries that format.
struct AudioOutput {
    pcm_channel: Arc<RTCDataChannel>,
    opus_channel: Arc<RTCDataChannel>,
    opus: Option<meshrmm_audio::OpusEncoder>,
    opus_unavailable: bool,
}

impl AudioOutput {
    fn reset(&mut self) {
        if let Some(encoder) = self.opus.as_mut() {
            encoder.reset();
        }
    }

    /// Returns the format, its channel, the packets to send, and their
    /// nominal bitrate, or `None` when the packet is discarded.
    fn encode(
        &mut self,
        mode: AudioMode,
        packet: Vec<u8>,
    ) -> Option<(AudioFormat, &RTCDataChannel, Vec<Vec<u8>>, u32)> {
        if mode == AudioMode::Opus && self.opus.is_none() && !self.opus_unavailable {
            match meshrmm_audio::OpusEncoder::new() {
                Ok(encoder) => self.opus = Some(encoder),
                Err(error) => {
                    // The viewer that asked for Opus also plays PCM.
                    tracing::warn!(%error, "Opus encoder unavailable; sending PCM audio");
                    self.opus_unavailable = true;
                }
            }
        }
        match self.opus.as_mut().filter(|_| mode == AudioMode::Opus) {
            Some(encoder) => match encoder.encode(&packet) {
                Ok(packets) => Some((
                    AudioFormat::Opus,
                    &self.opus_channel,
                    packets,
                    meshrmm_audio::OPUS_BITS_PER_SECOND,
                )),
                Err(error) => {
                    tracing::debug!(%error, "discarding audio the Opus encoder rejected");
                    None
                }
            },
            None => {
                self.reset();
                let bits = meshrmm_audio::pcm_bits_per_second(&packet)?;
                Some((AudioFormat::Pcm16, &self.pcm_channel, vec![packet], bits))
            }
        }
    }
}

struct AudioStats {
    bytes_sent: u64,
    packets_dropped: u64,
    started: tokio::time::Instant,
}

impl AudioStats {
    fn log_if_due(&mut self, mode: AudioMode, format: AudioFormat, bits: u32) {
        if self.started.elapsed() < AUDIO_STATS_INTERVAL {
            return;
        }
        tracing::info!(
            ?mode,
            ?format,
            nominal_bits_per_second = bits,
            audio_bits_per_second =
                self.bytes_sent as f64 * 8.0 / self.started.elapsed().as_secs_f64(),
            packets_dropped = self.packets_dropped,
            "audio transport statistics"
        );
        self.bytes_sent = 0;
        self.packets_dropped = 0;
        self.started = tokio::time::Instant::now();
    }
}

fn spawn_audio_sender(
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    audio_input: Arc<dyn ScreenInput>,
    sender_mode: watch::Receiver<AudioMode>,
    sender_bits: Arc<AtomicU32>,
    mut output: AudioOutput,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stats = AudioStats {
            bytes_sent: 0,
            packets_dropped: 0,
            started: tokio::time::Instant::now(),
        };
        'send: loop {
            let packet = tokio::select! {
                packet = audio_rx.recv() => match packet {
                    Some(packet) => packet,
                    None => break,
                },
                _ = tokio::time::sleep(AUDIO_IDLE) => {
                    sender_bits.store(0, Ordering::Relaxed);
                    // Encode what follows the gap as a new stream.
                    output.reset();
                    if stats.bytes_sent == 0 {
                        stats.started = tokio::time::Instant::now();
                    }
                    continue;
                }
            };
            let mode = *sender_mode.borrow();
            if !mode.captures() || !audio_input.is_console_session() {
                sender_bits.store(0, Ordering::Relaxed);
                continue;
            }
            let Some((format, channel, packets, bits)) = output.encode(mode, packet) else {
                continue;
            };
            if channel.ready_state() != RTCDataChannelState::Open {
                sender_bits.store(0, Ordering::Relaxed);
                continue;
            }
            sender_bits.store(bits, Ordering::Relaxed);
            for packet in packets {
                if channel.buffered_amount().await >= buffered_audio_limit(bits) {
                    stats.packets_dropped += 1;
                    continue;
                }
                stats.bytes_sent += packet.len() as u64;
                if channel.send(&Bytes::from(packet)).await.is_err() {
                    break 'send;
                }
            }
            stats.log_if_due(mode, format, bits);
        }
        sender_bits.store(0, Ordering::Relaxed);
    })
}
