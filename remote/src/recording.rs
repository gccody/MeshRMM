//! Local, video-only MPEG-TS recording. Each stream configuration gets its own
//! independently playable part. No decoding, re-encoding or external tools.
use anyhow::Context;
use meshrmm_protocol::{Codec, EncodedFrame, VideoStreamId};
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    session: Option<Session>,
    stream: Option<VideoStreamId>,
    notice: Option<String>,
}

struct Session {
    sender: Option<mpsc::SyncSender<(EncodedFrame, Codec)>>,
    overloaded: bool,
    worker: JoinHandle<anyhow::Result<usize>>,
    directory: PathBuf,
}

/// The connection owns this guard; closing or losing it drains and saves video.
pub struct RecordingGuard(pub Recorder);
impl Drop for RecordingGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

impl Recorder {
    pub fn active(&self) -> bool {
        self.0.lock().unwrap().session.is_some()
    }

    pub fn take_notice(&self) -> Option<String> {
        // Surface a disk error even when no more video arrives.
        let finished = self
            .0
            .lock()
            .unwrap()
            .session
            .as_ref()
            .is_some_and(|s| s.worker.is_finished());
        if finished {
            self.stop();
        }
        self.0.lock().unwrap().notice.take()
    }

    pub fn toggle(&self) -> Option<VideoStreamId> {
        if self.active() {
            self.stop();
            return None;
        }
        match self.start() {
            Ok(()) => self.0.lock().unwrap().stream,
            Err(error) => {
                self.0.lock().unwrap().notice =
                    Some(format!("Could not start recording: {error:#}"));
                None
            }
        }
    }

    fn start(&self) -> anyhow::Result<()> {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE").context("USERPROFILE is unavailable")?;
        #[cfg(not(windows))]
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        let root = PathBuf::from(home)
            .join("Downloads")
            .join("MeshRMM Recordings");
        std::fs::create_dir_all(&root).context("Cannot create recording folder")?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let mut suffix = 0;
        let directory = loop {
            let path = root.join(format!("session-{stamp}-{suffix}"));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
                Err(error) => return Err(error).context("Cannot create session folder"),
            }
        };
        // Bound memory and never hold up video presentation on a slow disk.
        let (sender, receiver) = mpsc::sync_channel::<(EncodedFrame, Codec)>(8);
        let output = directory.clone();
        let worker = std::thread::Builder::new()
            .name("session-recording".into())
            .spawn(move || write_recording(receiver, output))
            .context("Cannot start recording writer")?;
        self.0.lock().unwrap().session = Some(Session {
            sender: Some(sender),
            overloaded: false,
            worker,
            directory,
        });
        Ok(())
    }

    pub fn stop(&self) {
        let session = self.0.lock().unwrap().session.take();
        if let Some(Session {
            sender,
            overloaded,
            worker,
            directory,
        }) = session
        {
            drop(sender);
            let result = worker
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Recording writer stopped unexpectedly")));
            let notice = match result {
                Ok(0) => {
                    let _ = std::fs::remove_dir(&directory);
                    "Recording stopped before any video was received; no file saved.".into()
                }
                Ok(_) => format!("Recording saved to {}", directory.display()),
                Err(error) => format!(
                    "Recording stopped: {error:#}. Any video already written is in {}",
                    directory.display()
                ),
            };
            let notice = if overloaded {
                format!("Recording stopped because the disk could not keep up. {notice}")
            } else {
                notice
            };
            tracing::info!(%notice);
            self.0.lock().unwrap().notice = Some(notice);
        }
    }

    pub fn receive(&self, frame: &EncodedFrame, codec: Codec) {
        let mut state = self.0.lock().unwrap();
        state.stream = Some(frame.stream_id);
        if let Some(session) = &mut state.session
            && let Some(sender) = &session.sender
            && let Err(error) = sender.try_send((frame.clone(), codec))
        {
            session.overloaded = matches!(error, mpsc::TrySendError::Full(_));
            // Closing the queue lets the worker drain in the background. The UI
            // collects its result once finished; never join on the video callback.
            session.sender = None;
        }
    }
}

fn write_recording(
    receiver: mpsc::Receiver<(EncodedFrame, Codec)>,
    output: PathBuf,
) -> anyhow::Result<usize> {
    let mut part: Option<(VideoStreamId, TransportStream<BufWriter<File>>)> = None;
    let mut count = 0;
    for (frame, codec) in receiver {
        if part.as_ref().is_none_or(|(id, _)| *id != frame.stream_id) {
            if !frame.keyframe {
                continue;
            }
            if let Some((_, mut previous)) = part.take() {
                previous.output.flush()?;
            }
            count += 1;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join(format!("part-{count:03}.ts")))?;
            part = Some((
                frame.stream_id,
                TransportStream::new(BufWriter::new(file), codec, frame.capture_timestamp_us),
            ));
        }
        if let Some((_, writer)) = &mut part {
            writer.frame(&frame)?;
        }
    }
    if let Some((_, mut writer)) = part {
        writer.output.flush()?;
        writer.output.get_ref().sync_all()?;
    }
    Ok(count)
}

/// Single video PID (256), PMT PID (4096), 90 kHz timestamps. The incoming
/// low-latency stream has no B frames, so decode and presentation times coincide.
struct TransportStream<W> {
    output: W,
    codec: Codec,
    origin_us: u64,
    counters: [u8; 3],
}

impl<W: Write> TransportStream<W> {
    fn new(output: W, codec: Codec, origin_us: u64) -> Self {
        Self {
            output,
            codec,
            origin_us,
            counters: [0; 3],
        }
    }

    fn section(&mut self, pid: u16, index: usize, mut section: Vec<u8>) -> std::io::Result<()> {
        let mut crc = 0xffff_ffffu32;
        for byte in &section {
            crc ^= u32::from(*byte) << 24;
            for _ in 0..8 {
                crc = (crc << 1)
                    ^ if crc & 0x8000_0000 != 0 {
                        0x04c1_1db7
                    } else {
                        0
                    };
            }
        }
        section.extend_from_slice(&crc.to_be_bytes());
        let mut packet = [0xff; 188];
        packet[..5].copy_from_slice(&[
            0x47,
            0x40 | (pid >> 8) as u8,
            pid as u8,
            0x10 | self.counters[index],
            0,
        ]);
        self.counters[index] = (self.counters[index] + 1) & 15;
        packet[5..5 + section.len()].copy_from_slice(&section);
        self.output.write_all(&packet)
    }

    fn frame(&mut self, frame: &EncodedFrame) -> std::io::Result<()> {
        // Repeat tables for seeking and recovery; a TS requires no final index.
        self.section(0, 0, vec![0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0])?;
        let kind = match self.codec {
            Codec::H264 => 0x1b,
            Codec::H265 => 0x24,
        };
        self.section(
            4096,
            1,
            vec![
                2, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0, kind, 0xe1, 0, 0xf0, 0,
            ],
        )?;
        let clock = (frame.capture_timestamp_us.saturating_sub(self.origin_us) / 100 * 9)
            & ((1u64 << 33) - 1);
        let pts = (clock + 90_000) & ((1u64 << 33) - 1);
        let mut pes = vec![
            0,
            0,
            1,
            0xe0,
            0,
            0,
            0x80,
            0x80,
            5,
            0x21 | ((pts >> 29) as u8 & 14),
            (pts >> 22) as u8,
            ((pts >> 14) as u8 & 0xfe) | 1,
            (pts >> 7) as u8,
            ((pts << 1) as u8) | 1,
        ];
        // Exactly one AUD must lead each PES access unit. Hardware encoders can
        // already supply an AUD, sometimes after prepended parameter sets.
        // Keeping that AUD as well would create an empty access unit in VLC,
        // consume the PES timestamp and leave the actual frame without timing.
        match self.codec {
            Codec::H264 => pes.extend_from_slice(&[0, 0, 0, 1, 9, 0xf0]),
            Codec::H265 => pes.extend_from_slice(&[0, 0, 0, 1, 0x46, 1, 0x50]),
        }
        for unit in crate::h264::annex_b_units(&frame.data) {
            let is_delimiter = match self.codec {
                Codec::H264 => unit[0] & 0x1f == 9,
                Codec::H265 => (unit[0] >> 1) & 0x3f == 35,
            };
            if !is_delimiter {
                pes.extend_from_slice(&[0, 0, 0, 1]);
                pes.extend_from_slice(unit);
            }
        }
        let mut remaining = pes.as_slice();
        let mut first = true;
        while !remaining.is_empty() {
            let size = remaining.len().min(if first { 176 } else { 184 });
            let padding = 184 - size;
            let mut packet = [0xff; 188];
            packet[..4].copy_from_slice(&[
                0x47,
                1 | if first { 0x40 } else { 0 },
                0,
                if padding > 0 { 0x30 } else { 0x10 } | self.counters[2],
            ]);
            self.counters[2] = (self.counters[2] + 1) & 15;
            if padding > 0 {
                packet[4] = (padding - 1) as u8;
                if padding > 1 {
                    packet[5] = 0;
                }
                if first {
                    packet[5] = 0x10 | if frame.keyframe { 0x40 } else { 0 };
                    packet[6..12].copy_from_slice(&[
                        (clock >> 25) as u8,
                        (clock >> 17) as u8,
                        (clock >> 9) as u8,
                        (clock >> 1) as u8,
                        ((clock & 1) << 7) as u8 | 0x7e,
                        0,
                    ]);
                }
            }
            packet[4 + padding..].copy_from_slice(&remaining[..size]);
            self.output.write_all(&packet)?;
            remaining = &remaining[size..];
            first = false;
        }
        // Keep already captured video usable even after an unexpected exit.
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream: u32, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(stream),
            frame_id: 1,
            capture_timestamp_us: 1_000_000,
            encode_complete_timestamp_us: 1_000_001,
            send_timestamp_us: 1_000_002,
            keyframe,
            data: [vec![0, 0, 0, 1, 0x65], vec![0x55; 600]].concat(),
        }
    }

    #[test]
    fn packetization_preserves_video_and_timestamps_for_both_codecs() {
        for codec in [Codec::H264, Codec::H265] {
            let frame = frame(1, true);
            let mut writer = TransportStream::new(Vec::new(), codec, 500_000);
            writer.frame(&frame).unwrap();
            assert_eq!(writer.output.len() % 188, 0);
            let packets: Vec<_> = writer.output.chunks_exact(188).collect();
            assert!(packets.iter().all(|p| p[0] == 0x47));
            assert_eq!(packets[0][1] & 31, 0); // PAT
            assert_eq!(packets[1][1] & 31, 16); // PMT
            assert_eq!(
                packets[1][17],
                if codec == Codec::H264 { 0x1b } else { 0x24 }
            );
            let mut pes = Vec::new();
            for (i, p) in packets[2..].iter().enumerate() {
                assert_eq!(p[3] & 15, i as u8 & 15);
                let offset = if p[3] & 0x20 != 0 {
                    5 + p[4] as usize
                } else {
                    4
                };
                pes.extend_from_slice(&p[offset..]);
            }
            let pts = (u64::from(pes[9] & 14) << 29)
                | (u64::from(pes[10]) << 22)
                | (u64::from(pes[11] & 254) << 14)
                | (u64::from(pes[12]) << 7)
                | u64::from(pes[13] >> 1);
            assert_eq!(pts, 135_000);
            assert!(pes.ends_with(&frame.data));
            for packet in &packets[..2] {
                let length = (usize::from(packet[6] & 15) << 8) | usize::from(packet[7]);
                let mut crc = 0xffff_ffffu32;
                for byte in &packet[5..8 + length] {
                    crc ^= u32::from(*byte) << 24;
                    for _ in 0..8 {
                        crc = if crc & 0x8000_0000 == 0 {
                            crc << 1
                        } else {
                            (crc << 1) ^ 0x04c1_1db7
                        };
                    }
                }
                assert_eq!(crc, 0);
            }
        }
    }

    fn video_payloads(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        for packet in bytes.chunks_exact(188) {
            let pid = (u16::from(packet[1] & 31) << 8) | u16::from(packet[2]);
            if pid != 256 {
                continue;
            }
            if packet[1] & 0x40 != 0 {
                payloads.push(Vec::new());
            }
            let offset = if packet[3] & 0x20 != 0 {
                5 + usize::from(packet[4])
            } else {
                4
            };
            payloads
                .last_mut()
                .unwrap()
                .extend_from_slice(&packet[offset..]);
        }
        payloads
    }

    #[test]
    fn hardware_delimiters_do_not_create_empty_access_units_or_lose_sparse_timestamps() {
        for codec in [Codec::H264, Codec::H265] {
            let (parameters, aud, slice): (&[u8], &[u8], &[u8]) = match codec {
                Codec::H264 => (&[0x67, 0x64, 0x1f], &[9, 0xf0], &[0x65, 0x55]),
                Codec::H265 => (&[0x40, 1, 0xaa], &[0x46, 1, 0x50], &[0x26, 1, 0x55]),
            };
            let mut writer = TransportStream::new(Vec::new(), codec, 1_000_000);
            // Includes a static-screen pause: fixed-FPS decoding alone masked
            // the lost timestamps in the original playback smoke test.
            for (index, elapsed_us) in [0, 771_400, 788_800, 3_000_000].into_iter().enumerate() {
                let mut frame = frame(1, index == 0);
                frame.capture_timestamp_us += elapsed_us;
                frame.data.clear();
                // MFT keyframes prepend parameter sets before the encoder AUD.
                for nal in [parameters, aud, parameters, slice] {
                    frame.data.extend_from_slice(&[0, 0, 1]);
                    frame.data.extend_from_slice(nal);
                }
                writer.frame(&frame).unwrap();
            }
            let payloads = video_payloads(&writer.output);
            assert_eq!(payloads.len(), 4);
            for (pes, elapsed_us) in payloads.iter().zip([0, 771_400, 788_800, 3_000_000]) {
                let units = crate::h264::annex_b_units(&pes[9 + usize::from(pes[8])..]);
                assert_eq!(units, [aud, parameters, parameters, slice]);
                let pts = (u64::from(pes[9] & 14) << 29)
                    | (u64::from(pes[10]) << 22)
                    | (u64::from(pes[11] & 254) << 14)
                    | (u64::from(pes[12]) << 7)
                    | u64::from(pes[13] >> 1);
                assert_eq!(pts, 90_000 + elapsed_us / 100 * 9);
            }
        }
    }

    #[test]
    fn starts_each_part_at_a_keyframe_and_drains_on_close() {
        let directory = std::env::temp_dir().join(format!(
            "meshrmm-recording-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let (tx, rx) = mpsc::sync_channel(8);
        for (stream, keyframe) in [(1, false), (1, true), (1, false), (2, false), (2, true)] {
            tx.send((frame(stream, keyframe), Codec::H264)).unwrap();
        }
        drop(tx);
        assert_eq!(write_recording(rx, directory.clone()).unwrap(), 2);
        let first = std::fs::read(directory.join("part-001.ts")).unwrap();
        let second = std::fs::read(directory.join("part-002.ts")).unwrap();
        assert_eq!(first.len(), second.len() * 2);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_errors_propagate() {
        struct FullDisk;
        impl Write for FullDisk {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = TransportStream::new(FullDisk, Codec::H264, 0);
        assert!(writer.frame(&frame(1, true)).is_err());
    }

    #[test]
    fn full_queue_stops_accepting_without_blocking_video() {
        let recorder = Recorder::default();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::channel();
        recorder.0.lock().unwrap().session = Some(Session {
            sender: Some(sender),
            overloaded: false,
            directory: PathBuf::from("unused"),
            worker: std::thread::spawn(move || {
                wait.recv().unwrap();
                drop(receiver);
                Ok(1)
            }),
        });
        recorder.receive(&frame(1, true), Codec::H264);
        recorder.receive(&frame(1, false), Codec::H264);
        assert!(
            recorder
                .0
                .lock()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .sender
                .is_none()
        );
        release.send(()).unwrap();
        recorder.stop();
        assert!(
            recorder
                .take_notice()
                .unwrap()
                .contains("could not keep up")
        );
    }
}
