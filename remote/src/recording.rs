//! Local, video-only Matroska recording. Each stream configuration gets its own
//! independently playable part. No decoding, re-encoding or external tools.
use anyhow::Context;
use meshrmm_protocol::{EncodedFrame, VideoFormat, VideoStreamId};
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
    finishing: Vec<Session>,
}

struct Session {
    sender: Option<mpsc::SyncSender<(EncodedFrame, VideoFormat)>>,
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
        // Bound memory and never hold up video presentation on a slow disk.
        let (sender, receiver) = mpsc::sync_channel::<(EncodedFrame, VideoFormat)>(8);
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
        let mut state = self.0.lock().unwrap();
        if let Some(mut session) = state.session.take() {
            // Never wait for a slow disk on the UI or video thread.
            session.sender = None;
            state.finishing.push(session);
        }
    }

    fn collect(&self, session: Session) {
        let Session {
            sender,
            overloaded,
            worker,
            directory,
        } = session;
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

    pub fn receive(&self, frame: &EncodedFrame, format: VideoFormat) {
        let mut state = self.0.lock().unwrap();
        state.stream = Some(frame.stream_id);
        if let Some(session) = &mut state.session
            && let Some(sender) = &session.sender
            && let Err(error) = sender.try_send((frame.clone(), format))
        {
            session.overloaded = matches!(error, mpsc::TrySendError::Full(_));
            // Closing the queue lets the worker drain in the background. The UI
            // collects its result once finished; never join on the video callback.
            session.sender = None;
        }
    }
}

fn write_recording(
    receiver: mpsc::Receiver<(EncodedFrame, VideoFormat)>,
    output: PathBuf,
) -> anyhow::Result<usize> {
    let mut part: Option<(VideoStreamId, crate::matroska::Matroska<BufWriter<File>>)> = None;
    let mut count = 0;
    for (frame, format) in receiver {
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
    use std::time::Duration;

    #[test]
    fn stop_and_full_queue_never_wait_for_a_stalled_writer() {
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
        let (format, frame) = crate::matroska::tests::sample(Codec::H264, false);
        recorder.receive(&frame, format);
        recorder.receive(&frame, format);
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
        let (stopped, done) = mpsc::channel();
        let other = recorder.clone();
        let stopper = std::thread::spawn(move || {
            other.stop();
            stopped.send(()).unwrap();
        });
        let result = done.recv_timeout(Duration::from_secs(2));
        // Always release the worker, including on failure.
        release.send(()).unwrap();
        stopper.join().unwrap();
        result.expect("Stop blocked on the recording writer");
        assert!(!recorder.active());
        let session = recorder.0.lock().unwrap().finishing.pop().unwrap();
        recorder.collect(session);
        assert!(
            recorder
                .take_notice()
                .unwrap()
                .contains("could not keep up")
        );
    }

    #[test]
    fn splits_at_keyframes_and_writes_to_disk_before_stop() {
        let directory = std::env::temp_dir().join(format!(
            "meshrmm-recording-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let (tx, rx) = mpsc::sync_channel(8);
        let output = directory.clone();
        let worker = std::thread::spawn(move || write_recording(rx, output));
        let (format, mut frame) = crate::matroska::tests::sample(Codec::H264, false);
        for (stream, keyframe) in [(1, false), (1, true), (1, false), (2, false), (2, true)] {
            frame.stream_id = VideoStreamId(stream);
            frame.keyframe = keyframe;
            tx.send((frame.clone(), format)).unwrap();
        }
        let second = directory.join("part-002.mkv");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::fs::metadata(&second).map_or(true, |m| m.len() < frame.data.len() as u64) {
            assert!(
                std::time::Instant::now() < deadline,
                "video was not written while recording"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_finished());
        drop(tx);
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
