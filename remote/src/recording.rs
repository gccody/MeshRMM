//! Local, video-only Matroska recording. Each stream configuration gets its own
//! independently playable part. No decoding, re-encoding or external tools.
use anyhow::Context;
use meshrmm_protocol::{EncodedFrame, VideoFormat, VideoStreamId};
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How much video may wait for the disk. A write can pause for seconds (an
/// antivirus scan of the new file, a page-cache flush, a disk spinning up, a
/// synced or network folder), so the queue covers 10 seconds of video. At the
/// highest bitrate a session can use, 12 Mbps, that is about 15 MB; the byte
/// bound is four times that, so it holds only when the encoder overshoots its
/// rate, as a burst of large keyframes can.
const QUEUE_LIMITS: Limits = Limits {
    bytes: 64 << 20,
    duration: Duration::from_secs(10),
};

#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<State>>);

type KeyframeRequest = Arc<dyn Fn(VideoStreamId) + Send + Sync>;

#[derive(Default)]
struct State {
    session: Option<Session>,
    stream: Option<VideoStreamId>,
    notice: Option<String>,
    finishing: Vec<Session>,
    on_active: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    /// Asks the device for a keyframe, so a recording that fell behind can
    /// resume without waiting for the encoder's next one.
    request_keyframe: Option<KeyframeRequest>,
}

struct Session {
    queue: Arc<FrameQueue>,
    worker: JoinHandle<anyhow::Result<usize>>,
    directory: PathBuf,
}

impl Recorder {
    pub fn with_activity_callback(callback: impl Fn(bool) + Send + Sync + 'static) -> Self {
        Self(Arc::new(Mutex::new(State {
            on_active: Some(Arc::new(callback)),
            ..State::default()
        })))
    }

    /// Sets how the recorder asks for a keyframe; each connection sets its
    /// own.
    pub fn set_keyframe_request(&self, request: impl Fn(VideoStreamId) + Send + Sync + 'static) {
        self.0.lock().unwrap().request_keyframe = Some(Arc::new(request));
    }

    #[cfg(test)]
    pub fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

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
        let finished = {
            let mut state = self.0.lock().unwrap();
            state
                .finishing
                .iter()
                .position(|s| s.worker.is_finished())
                .map(|index| state.finishing.remove(index))
        };
        if let Some(session) = finished {
            self.collect(session);
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
        let queue = Arc::new(FrameQueue::new(QUEUE_LIMITS));
        let writer_queue = Arc::clone(&queue);
        let output = directory.clone();
        let worker = std::thread::Builder::new()
            .name("session-recording".into())
            .spawn(move || write_recording(&writer_queue, output))
            .context("Cannot start recording writer")?;
        let mut state = self.0.lock().unwrap();
        if let Some(callback) = &state.on_active {
            callback(true);
        }
        state.session = Some(Session {
            queue,
            worker,
            directory,
        });
        Ok(())
    }

    pub fn stop(&self) {
        let mut state = self.0.lock().unwrap();
        if let Some(session) = state.session.take() {
            if let Some(callback) = &state.on_active {
                callback(false);
            }
            // The writer saves what is queued, then ends. Never wait for a
            // slow disk on the UI or video thread.
            session.queue.close(Instant::now());
            state.finishing.push(session);
        }
    }

    /// Ends recording when the session ends, and waits up to `timeout` for
    /// the writer to save what it has queued: a writer still running when the
    /// process exits would leave the last part truncated. Returns the notice
    /// to show, if any.
    pub fn finish(&self, timeout: Duration) -> Option<String> {
        self.stop();
        let deadline = Instant::now() + timeout;
        loop {
            let finished = {
                let mut state = self.0.lock().unwrap();
                let (done, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut state.finishing)
                    .into_iter()
                    .partition(|session| session.worker.is_finished());
                state.finishing = pending;
                done
            };
            for session in finished {
                self.collect(session);
            }
            let waiting = !self.0.lock().unwrap().finishing.is_empty();
            if !waiting || Instant::now() >= deadline {
                if waiting {
                    tracing::warn!("recording writer did not finish before the viewer closed");
                }
                return self.0.lock().unwrap().notice.take();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn collect(&self, session: Session) {
        let Session {
            queue,
            worker,
            directory,
        } = session;
        queue.close(Instant::now());
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
        let notice = match queue.loss() {
            Loss { gaps: 0, .. } => notice,
            Loss {
                gaps,
                frames,
                duration,
            } => {
                let times = if gaps == 1 {
                    "once".to_string()
                } else {
                    format!("{gaps} times")
                };
                format!(
                    "The disk fell behind {times}, so about {:.1} seconds of video ({frames} frames) \
                     are missing from the recording. {notice}",
                    duration.as_secs_f64()
                )
            }
        };
        tracing::info!(%notice);
        self.0.lock().unwrap().notice = Some(notice);
    }

    pub fn receive(&self, frame: &EncodedFrame, format: VideoFormat) {
        let request = {
            let mut state = self.0.lock().unwrap();
            state.stream = Some(frame.stream_id);
            match &state.session {
                Some(session) => {
                    session.queue.push(frame, format, Instant::now()) == Pushed::DroppedAskKeyframe
                }
                None => false,
            }
        };
        if request {
            self.request_keyframe(frame.stream_id);
        }
    }

    /// Asks for a keyframe of `stream_id` the way the current connection set.
    pub fn request_keyframe(&self, stream_id: VideoStreamId) {
        let request = self.0.lock().unwrap().request_keyframe.clone();
        if let Some(request) = request {
            request(stream_id);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    bytes: usize,
    duration: Duration,
}

/// The frames between the video callback and the writer. Adding a frame never
/// waits for the writer, which holds the lock only to take a frame.
struct FrameQueue {
    limits: Limits,
    state: Mutex<QueueState>,
    changed: Condvar,
}

#[derive(Default)]
struct QueueState {
    frames: VecDeque<Queued>,
    bytes: usize,
    /// No more frames are coming: the writer saves the rest and ends.
    closed: bool,
    /// The writer has ended, so a frame added would never be written.
    writer_ended: bool,
    /// Frames are being left out until a keyframe fits.
    gap: Option<Gap>,
    loss: Loss,
}

struct Queued {
    frame: EncodedFrame,
    format: VideoFormat,
    arrived: Instant,
}

struct Gap {
    since: Instant,
    keyframe_requested: bool,
}

/// What a recording lost because the writer fell behind.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Loss {
    gaps: u32,
    frames: u64,
    duration: Duration,
}

#[derive(Debug, PartialEq, Eq)]
enum Pushed {
    Queued,
    Dropped,
    /// Dropped, and the queue has room again: ask the device for a keyframe
    /// to resume from.
    DroppedAskKeyframe,
    Closed,
}

impl FrameQueue {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            state: Mutex::default(),
            changed: Condvar::new(),
        }
    }

    /// Queues `frame`, sharing its bytes, or leaves it out when the queue is over its
    /// bounds. Every frame from then on is left out too until a keyframe
    /// fits: a frame whose reference frames are missing cannot be decoded,
    /// and the writer resumes the part at that keyframe, with a gap in time.
    fn push(&self, frame: &EncodedFrame, format: VideoFormat, now: Instant) -> Pushed {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.writer_ended {
            return Pushed::Closed;
        }
        let fits = state.fits(&self.limits, frame.data.len(), now);
        if fits && (state.gap.is_none() || frame.keyframe) {
            if let Some(gap) = state.gap.take() {
                let missing = now.saturating_duration_since(gap.since);
                state.loss.duration += missing;
                tracing::info!(
                    missing_ms = missing.as_millis() as u64,
                    "recording resumed at a keyframe after the disk fell behind"
                );
            }
            state.bytes += frame.data.len();
            state.frames.push_back(Queued {
                frame: frame.clone(),
                format,
                arrived: now,
            });
            drop(state);
            self.changed.notify_all();
            return Pushed::Queued;
        }
        if state.gap.is_none() {
            tracing::warn!(
                queued_bytes = state.bytes,
                queued_frames = state.frames.len(),
                "the disk fell behind the recording; leaving video out until a keyframe fits"
            );
            state.loss.gaps += 1;
        }
        state.loss.frames += 1;
        let room = state.fits(
            &Limits {
                bytes: self.limits.bytes / 2,
                duration: self.limits.duration / 2,
            },
            0,
            now,
        );
        let gap = state.gap.get_or_insert(Gap {
            since: now,
            keyframe_requested: false,
        });
        if frame.keyframe {
            // This keyframe did not fit; ask for another once there is room.
            gap.keyframe_requested = false;
        }
        // Ask only once the writer is catching up, so a disk that stays stuck
        // costs the session's video no extra keyframes.
        if room && !gap.keyframe_requested {
            gap.keyframe_requested = true;
            Pushed::DroppedAskKeyframe
        } else {
            Pushed::Dropped
        }
    }

    /// The next frame to write, waiting for one; `None` once the queue is
    /// closed and empty.
    fn pop(&self) -> Option<(EncodedFrame, VideoFormat)> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(queued) = state.frames.pop_front() {
                state.bytes -= queued.frame.data.len();
                drop(state);
                self.changed.notify_all();
                return Some((queued.frame, queued.format));
            }
            if state.closed {
                return None;
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Takes no more frames; the writer saves those queued, then ends. A gap
    /// still open runs to `now`.
    fn close(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            state.closed = true;
            if let Some(gap) = state.gap.take() {
                state.loss.duration += now.saturating_duration_since(gap.since);
            }
        }
        drop(state);
        self.changed.notify_all();
    }

    fn writer_ended(&self) {
        let mut state = self.state.lock().unwrap();
        state.writer_ended = true;
        state.frames.clear();
        state.bytes = 0;
    }

    fn loss(&self) -> Loss {
        self.state.lock().unwrap().loss
    }

    #[cfg(test)]
    fn wait_until_empty(&self) {
        let state = self.state.lock().unwrap();
        let _empty = self
            .changed
            .wait_while(state, |state| !state.frames.is_empty())
            .unwrap();
    }
}

impl QueueState {
    /// Whether `bytes` more fit within `limits`. An empty queue takes any
    /// frame, so even a keyframe larger than the bound is recorded.
    fn fits(&self, limits: &Limits, bytes: usize, now: Instant) -> bool {
        self.frames.front().is_none_or(|oldest| {
            self.bytes + bytes <= limits.bytes
                && now.saturating_duration_since(oldest.arrived) < limits.duration
        })
    }
}

/// Marks the writer ended however it ends, a panic included.
struct WriterEnded<'a>(&'a FrameQueue);

impl Drop for WriterEnded<'_> {
    fn drop(&mut self) {
        self.0.writer_ended();
    }
}

fn write_recording(queue: &FrameQueue, output: PathBuf) -> anyhow::Result<usize> {
    let _ended = WriterEnded(queue);
    let mut part: Option<(VideoStreamId, crate::matroska::Matroska<BufWriter<File>>)> = None;
    let mut count = 0;
    while let Some((frame, format)) = queue.pop() {
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
                .open(output.join(format!("part-{count:03}.mkv")))?;
            part = Some((
                frame.stream_id,
                crate::matroska::Matroska::new(BufWriter::new(file), format, &frame)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::Codec;
    use std::sync::mpsc;

    /// A new folder in the system's temporary folder, unique to the test even
    /// when tests start at the same instant.
    fn temporary_directory() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "meshrmm-recording-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    /// A recording whose writer waits for `release` before it writes.
    struct Held {
        recorder: Recorder,
        queue: Arc<FrameQueue>,
        release: mpsc::Sender<()>,
        keyframe_requests: mpsc::Receiver<VideoStreamId>,
        directory: PathBuf,
    }

    fn held(limits: Limits) -> Held {
        let directory = temporary_directory();
        let recorder = Recorder::default();
        let (requests, keyframe_requests) = mpsc::channel();
        recorder.set_keyframe_request(move |stream_id| {
            let _ = requests.send(stream_id);
        });
        let queue = Arc::new(FrameQueue::new(limits));
        let (release, gate) = mpsc::channel();
        let writer_queue = Arc::clone(&queue);
        let output = directory.clone();
        recorder.0.lock().unwrap().session = Some(Session {
            queue: Arc::clone(&queue),
            directory: directory.clone(),
            worker: std::thread::spawn(move || {
                let _ = gate.recv();
                write_recording(&writer_queue, output)
            }),
        });
        Held {
            recorder,
            queue,
            release,
            keyframe_requests,
            directory,
        }
    }

    /// The sample frame `index` of a 60 fps stream, a keyframe if `keyframe`.
    fn frame(index: u64, keyframe: bool) -> (EncodedFrame, VideoFormat) {
        let (format, mut frame) = crate::matroska::tests::sample(Codec::H264, false);
        frame.frame_id = index;
        frame.capture_timestamp_us = 1_000_000 + index * 16_667;
        frame.keyframe = keyframe;
        (frame, format)
    }

    fn receive(recorder: &Recorder, index: u64, keyframe: bool) {
        let (frame, format) = frame(index, keyframe);
        recorder.receive(&frame, format);
    }

    /// The frame indexes in a part, and whether each was written as a
    /// keyframe.
    fn blocks(path: &std::path::Path) -> Vec<(u64, bool)> {
        fn id(bytes: &[u8], at: &mut usize) -> u32 {
            let length = bytes[*at].leading_zeros() as usize + 1;
            let value = bytes[*at..*at + length]
                .iter()
                .fold(0, |value, byte| (value << 8) | u32::from(*byte));
            *at += length;
            value
        }
        fn size(bytes: &[u8], at: &mut usize) -> usize {
            let length = bytes[*at].leading_zeros() as usize + 1;
            let first = u64::from(bytes[*at]) & (0xff >> length);
            let value = bytes[*at + 1..*at + length]
                .iter()
                .fold(first, |value, byte| (value << 8) | u64::from(*byte));
            *at += length;
            usize::try_from(value).unwrap()
        }
        let bytes = std::fs::read(path).unwrap();
        let mut blocks = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let element = id(&bytes, &mut at);
            let length = size(&bytes, &mut at);
            match element {
                // The segment's size is unknown; its children follow.
                0x1853_8067 => continue,
                0x1f43_b675 => {
                    let end = at + length;
                    let mut timestamp_ms = 0;
                    let mut inner = at;
                    while inner < end {
                        let child = id(&bytes, &mut inner);
                        let child_length = size(&bytes, &mut inner);
                        let data = &bytes[inner..inner + child_length];
                        match child {
                            0xe7 => {
                                timestamp_ms = data
                                    .iter()
                                    .fold(0, |value, byte| (value << 8) | u64::from(*byte));
                            }
                            0xa3 => blocks.push((timestamp_ms, data[3] & 0x80 != 0)),
                            _ => {}
                        }
                        inner += child_length;
                    }
                }
                _ => {}
            }
            at += length;
        }
        // Matroska keeps milliseconds from the part's first frame.
        let first = blocks.first().map_or(0, |(timestamp, _)| *timestamp);
        blocks
            .into_iter()
            .map(|(timestamp, keyframe)| (((timestamp - first) * 1000).div_ceil(16_667), keyframe))
            .collect()
    }

    #[test]
    fn a_writer_held_past_many_frames_loses_nothing_and_recording_goes_on() {
        let held = held(QUEUE_LIMITS);
        // A second of 60 fps video, far more than the old eight frames.
        receive(&held.recorder, 0, true);
        for index in 1..60 {
            receive(&held.recorder, index, false);
        }
        assert!(held.recorder.active());
        held.release.send(()).unwrap();
        for index in 60..70 {
            receive(&held.recorder, index, false);
        }
        let notice = held.recorder.finish(Duration::from_secs(10)).unwrap();
        assert!(notice.starts_with("Recording saved to"), "{notice}");
        let expected: Vec<_> = (0..70).map(|index| (index, index == 0)).collect();
        assert_eq!(blocks(&held.directory.join("part-001.mkv")), expected);
        assert!(held.keyframe_requests.try_recv().is_err());
        std::fs::remove_dir_all(&held.directory).unwrap();
    }

    #[test]
    fn the_video_callback_never_waits_for_a_held_writer() {
        let (sample, _) = frame(0, true);
        let held = held(Limits {
            bytes: 4 * sample.data.len(),
            duration: Duration::from_secs(60),
        });
        let recorder = held.recorder.clone();
        let (done, received) = mpsc::channel();
        let video = std::thread::spawn(move || {
            // Within the bound, then far past it.
            for index in 0..1000 {
                receive(&recorder, index, index % 100 == 0);
            }
            done.send(()).unwrap();
        });
        let result = received.recv_timeout(Duration::from_secs(10));
        // Always release the writer, including on failure.
        held.release.send(()).unwrap();
        video.join().unwrap();
        result.expect("the video callback waited for the writer");
        // The recording goes on once the writer catches up.
        held.queue.wait_until_empty();
        receive(&held.recorder, 1000, true);
        assert!(held.recorder.active());
        held.recorder.finish(Duration::from_secs(10)).unwrap();
        assert_eq!(
            blocks(&held.directory.join("part-001.mkv")),
            [(0, true), (1, false), (2, false), (3, false), (1000, true)]
        );
        std::fs::remove_dir_all(&held.directory).unwrap();
    }

    #[test]
    fn past_the_bound_video_is_left_out_up_to_a_keyframe_and_the_loss_reported() {
        let (sample, _) = frame(0, true);
        let held = held(Limits {
            bytes: 4 * sample.data.len(),
            duration: Duration::from_secs(60),
        });
        for index in 0..4 {
            receive(&held.recorder, index, index == 0);
        }
        // Past the bound: left out, and no keyframe asked for while the
        // writer is stuck, not even after a keyframe that did not fit.
        receive(&held.recorder, 4, false);
        receive(&held.recorder, 5, true);
        receive(&held.recorder, 6, false);
        assert!(held.keyframe_requests.try_recv().is_err());
        held.release.send(()).unwrap();
        held.queue.wait_until_empty();
        // The writer caught up: a keyframe is asked for once, and frames
        // that would need the missing ones are still left out until it comes.
        receive(&held.recorder, 7, false);
        receive(&held.recorder, 8, false);
        assert_eq!(held.keyframe_requests.try_recv(), Ok(VideoStreamId(1)));
        assert!(held.keyframe_requests.try_recv().is_err());
        receive(&held.recorder, 9, true);
        receive(&held.recorder, 10, false);
        assert!(held.recorder.active());
        let notice = held.recorder.finish(Duration::from_secs(10)).unwrap();
        assert!(
            notice.starts_with("The disk fell behind once, so about ")
                && notice.contains("(5 frames) are missing from the recording. Recording saved to"),
            "{notice}"
        );
        // One part, decodable throughout: it resumes at a keyframe.
        assert_eq!(
            blocks(&held.directory.join("part-001.mkv")),
            [
                (0, true),
                (1, false),
                (2, false),
                (3, false),
                (9, true),
                (10, false)
            ]
        );
        assert_eq!(std::fs::read_dir(&held.directory).unwrap().count(), 1);
        std::fs::remove_dir_all(&held.directory).unwrap();
    }

    /// The presenter and the recording share a frame's bytes.
    #[test]
    fn a_queued_frame_shares_the_received_bytes() {
        let queue = FrameQueue::new(QUEUE_LIMITS);
        let (key, format) = frame(0, true);
        assert_eq!(queue.push(&key, format, Instant::now()), Pushed::Queued);
        let (queued, _) = queue.pop().unwrap();
        assert_eq!(queued.data.as_ptr(), key.data.as_ptr());
    }

    #[test]
    fn the_queue_covers_a_bounded_time() {
        let queue = FrameQueue::new(Limits {
            bytes: usize::MAX,
            duration: Duration::from_secs(10),
        });
        let start = Instant::now();
        let (key, format) = frame(0, true);
        let (delta, _) = frame(1, false);
        assert_eq!(queue.push(&key, format, start), Pushed::Queued);
        let later = start + Duration::from_millis(9_999);
        assert_eq!(queue.push(&delta, format, later), Pushed::Queued);
        let past = start + Duration::from_secs(10);
        assert_eq!(queue.push(&delta, format, past), Pushed::Dropped);
        // Once the writer takes the oldest frame, the next keyframe fits.
        queue.pop().unwrap();
        assert_eq!(
            queue.push(&key, format, past + Duration::from_secs(2)),
            Pushed::Queued
        );
        queue.close(past + Duration::from_secs(3));
        assert_eq!(
            queue.loss(),
            Loss {
                gaps: 1,
                frames: 1,
                duration: Duration::from_secs(2)
            }
        );
    }

    #[test]
    fn a_failing_writer_still_ends_the_recording_with_its_error() {
        let held = held(QUEUE_LIMITS);
        receive(&held.recorder, 0, true);
        for index in 1..30 {
            receive(&held.recorder, index, false);
        }
        // The folder is removed before the writer opens its first part.
        std::fs::remove_dir_all(&held.directory).unwrap();
        held.release.send(()).unwrap();
        let notice = held.recorder.finish(Duration::from_secs(10)).unwrap();
        assert!(notice.starts_with("Recording stopped: "), "{notice}");
        assert!(!notice.contains("fell behind"), "{notice}");
        assert!(!held.recorder.active());
    }

    #[test]
    fn stop_never_waits_for_a_held_writer_and_writes_what_was_queued() {
        let (activity, changes) = mpsc::channel();
        let held = held(QUEUE_LIMITS);
        held.recorder.0.lock().unwrap().on_active = Some(Arc::new(move |active| {
            activity.send(active).unwrap();
        }));
        receive(&held.recorder, 0, true);
        for index in 1..30 {
            receive(&held.recorder, index, false);
        }
        let (stopped, done) = mpsc::channel();
        let recorder = held.recorder.clone();
        let stopper = std::thread::spawn(move || {
            recorder.stop();
            stopped.send(()).unwrap();
        });
        let result = done.recv_timeout(Duration::from_secs(5));
        // Always release the writer, including on failure.
        held.release.send(()).unwrap();
        stopper.join().unwrap();
        result.expect("Stop blocked on the recording writer");
        assert!(!held.recorder.active());
        assert_eq!(changes.try_recv(), Ok(false));
        held.recorder.stop();
        assert!(changes.try_recv().is_err());
        // Video after the stop is not recorded.
        receive(&held.recorder, 30, false);
        let notice = held.recorder.finish(Duration::from_secs(10)).unwrap();
        assert!(notice.starts_with("Recording saved to"), "{notice}");
        let expected: Vec<_> = (0..30).map(|index| (index, index == 0)).collect();
        assert_eq!(blocks(&held.directory.join("part-001.mkv")), expected);
        std::fs::remove_dir_all(&held.directory).unwrap();
    }

    #[test]
    fn finishing_waits_for_the_writer_and_returns_its_notice() {
        let recorder = Recorder::default();
        assert_eq!(recorder.finish(Duration::from_millis(10)), None);
        let queue = Arc::new(FrameQueue::new(QUEUE_LIMITS));
        let writer_queue = Arc::clone(&queue);
        let (saved_tx, saved) = mpsc::channel();
        recorder.0.lock().unwrap().session = Some(Session {
            queue,
            directory: PathBuf::from("recordings"),
            worker: std::thread::spawn(move || {
                // Drains until the recorder closes the queue, then saves.
                while writer_queue.pop().is_some() {}
                std::thread::sleep(Duration::from_millis(100));
                saved_tx.send(()).unwrap();
                Ok(1)
            }),
        });
        let notice = recorder.finish(Duration::from_secs(5)).unwrap();
        assert!(saved.try_recv().is_ok(), "returned before the writer saved");
        assert!(notice.starts_with("Recording saved to"), "{notice}");
        assert!(!recorder.active());
    }

    #[test]
    fn splits_at_keyframes_and_writes_to_disk_before_stop() {
        let directory = temporary_directory();
        let queue = Arc::new(FrameQueue::new(QUEUE_LIMITS));
        let writer_queue = Arc::clone(&queue);
        let output = directory.clone();
        let worker = std::thread::spawn(move || write_recording(&writer_queue, output));
        let (format, mut frame) = crate::matroska::tests::sample(Codec::H264, false);
        for (stream, keyframe) in [(1, false), (1, true), (1, false), (2, false), (2, true)] {
            frame.stream_id = VideoStreamId(stream);
            frame.keyframe = keyframe;
            assert_eq!(queue.push(&frame, format, Instant::now()), Pushed::Queued);
        }
        let second = directory.join("part-002.mkv");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::metadata(&second).map_or(true, |m| m.len() < frame.data.len() as u64) {
            assert!(
                Instant::now() < deadline,
                "video was not written while recording"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_finished());
        queue.close(Instant::now());
        assert_eq!(worker.join().unwrap().unwrap(), 2);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        assert!(
            std::fs::metadata(directory.join("part-001.mkv"))
                .unwrap()
                .len()
                > std::fs::metadata(second).unwrap().len()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
