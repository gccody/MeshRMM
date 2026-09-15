//! Best-effort bounded logging: a stalled disk must not stall endpoint services.
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

const MAX_RECORD: usize = 64 * 1024;
#[derive(Clone)]
pub struct AsyncLog {
    sender: mpsc::SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
}
impl AsyncLog {
    pub fn new(mut writer: impl Write + Send + 'static) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(1024);
        let dropped = Arc::new(AtomicU64::new(0));
        let losses = dropped.clone();
        std::thread::Builder::new()
            .name("meshrmm-log".into())
            .spawn(move || {
                while let Ok(record) = receiver.recv() {
                    if writer
                        .write_all(&record)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                    let count = losses.swap(0, Ordering::Relaxed);
                    if count > 0 {
                        let _ = writeln!(
                            writer,
                            "WARN dropped {count} log records while logging was congested"
                        );
                    }
                }
            })?;
        Ok(Self { sender, dropped })
    }
}

pub struct Record {
    log: AsyncLog,
    bytes: Vec<u8>,
    oversized: bool,
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for AsyncLog {
    type Writer = Record;
    fn make_writer(&'a self) -> Record {
        Record {
            log: self.clone(),
            bytes: Vec::new(),
            oversized: false,
        }
    }
}
impl Write for Record {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.oversized && self.bytes.len().saturating_add(bytes.len()) <= MAX_RECORD {
            self.bytes.extend_from_slice(bytes);
        } else {
            self.oversized = true;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        if self.oversized
            || self
                .log
                .sender
                .try_send(std::mem::take(&mut self.bytes))
                .is_err()
        {
            self.log.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;
    struct StalledWriter(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl Write for StalledWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let _ = self.0.send(());
            let _ = self.1.recv_timeout(std::time::Duration::from_secs(5));
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn stalled_disk_never_blocks_producers_and_memory_is_bounded() {
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let logger = AsyncLog::new(StalledWriter(entered, blocked)).unwrap();
        writeln!(logger.make_writer(), "first record").unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        for _ in 0..2048 {
            writeln!(logger.make_writer(), "still responsive").unwrap();
        }
        assert!(logger.dropped.load(Ordering::Relaxed) >= 1024);
        // Closing this gate releases every write, including queued records.
        drop(release);
    }
}
