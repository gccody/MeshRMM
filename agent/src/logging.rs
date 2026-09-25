//! Best-effort bounded logging: a stalled disk must not stall endpoint services.
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

const MAX_RECORD: usize = 64 * 1024;
/// How long exit paths wait for queued records to reach the log.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// After a write error the log is reopened no sooner than this, doubling up to `MAX_RETRY`.
const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(30);

static PROCESS_LOG: OnceLock<AsyncLog> = OnceLock::new();

enum Message {
    Record(Vec<u8>),
    Flush(mpsc::SyncSender<()>),
}

#[derive(Clone)]
pub struct AsyncLog {
    sender: mpsc::SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}
impl AsyncLog {
    /// Opens the log with `open` and writes it on a background thread. After a write error the
    /// thread counts the records it cannot write and calls `open` again, backing off between
    /// attempts, until the log is writable.
    pub fn new<W: Write + Send + 'static>(
        mut open: impl FnMut() -> io::Result<W> + Send + 'static,
    ) -> io::Result<Self> {
        let file = open()?;
        let (sender, receiver) = mpsc::sync_channel::<Message>(1024);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut writer = Writer {
            open,
            file: Some(file),
            retry_at: Instant::now(),
            retry_delay: FIRST_RETRY,
            congested: dropped.clone(),
            lost: 0,
        };
        std::thread::Builder::new()
            .name("meshrmm-log".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        Message::Record(record) => writer.write(&record),
                        // Records ahead of this one in the queue are written and flushed.
                        Message::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self { sender, dropped })
    }

    /// Waits up to `timeout` for the writer to handle the records queued before this call:
    /// written, or counted as lost while the log cannot be written. Returns whether it did.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (done, written) = mpsc::sync_channel(1);
        let mut message = Message::Flush(done);
        loop {
            match self.sender.try_send(message) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) if Instant::now() < deadline => {
                    message = returned;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        }
        written
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
    }
}

/// Makes `log` the log that [`flush`] waits for.
pub fn set_process_log(log: AsyncLog) {
    let _ = PROCESS_LOG.set(log);
}

/// Whether this process writes its log through [`AsyncLog`].
pub fn has_process_log() -> bool {
    PROCESS_LOG.get().is_some()
}

/// Waits briefly for the process log to write what is queued. Call it before the process exits
/// or reports itself stopped, or the last records, often the reason it stopped, are lost.
pub fn flush() {
    if let Some(log) = PROCESS_LOG.get() {
        log.flush(FLUSH_TIMEOUT);
    }
}

struct Writer<W, F> {
    open: F,
    file: Option<W>,
    retry_at: Instant,
    retry_delay: Duration,
    /// Records producers dropped because the queue was full.
    congested: Arc<AtomicU64>,
    /// Records this thread could not write.
    lost: u64,
}

impl<W: Write, F: FnMut() -> io::Result<W>> Writer<W, F> {
    fn write(&mut self, record: &[u8]) {
        self.reopen_if_due();
        let Some(file) = self.file.as_mut() else {
            self.lost += 1;
            return;
        };
        let congested = self.congested.swap(0, Ordering::Relaxed);
        let lost = std::mem::take(&mut self.lost);
        let mut notice = Vec::new();
        if congested > 0 {
            let _ = writeln!(
                notice,
                "WARN dropped {congested} log records while logging was congested"
            );
        }
        if lost > 0 {
            let _ = writeln!(
                notice,
                "WARN lost {lost} log records while the log could not be written"
            );
        }
        let result = file
            .write_all(&notice)
            .and_then(|()| file.write_all(record))
            .and_then(|()| file.flush());
        match result {
            Ok(()) => self.retry_delay = FIRST_RETRY,
            Err(_) => {
                self.congested.fetch_add(congested, Ordering::Relaxed);
                self.lost = lost + 1;
                self.file = None;
                self.back_off();
            }
        }
    }

    /// Reopens a failed log once the retry delay has passed.
    fn reopen_if_due(&mut self) {
        if self.file.is_none() && Instant::now() >= self.retry_at {
            match (self.open)() {
                Ok(file) => self.file = Some(file),
                Err(_) => self.back_off(),
            }
        }
    }

    fn back_off(&mut self) {
        self.retry_at = Instant::now() + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY);
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
                .try_send(Message::Record(std::mem::take(&mut self.bytes)))
                .is_err()
        {
            self.log.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
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

    fn once<W>(writer: W) -> impl FnMut() -> io::Result<W> + Send + 'static
    where
        W: Send + 'static,
    {
        let mut writer = Some(writer);
        move || {
            writer
                .take()
                .ok_or_else(|| io::Error::other("log reopened"))
        }
    }

    /// Appends to a shared buffer, slowly, and fails while `failing` is set.
    #[derive(Clone, Default)]
    struct SharedWriter {
        contents: Arc<Mutex<Vec<u8>>>,
        failing: Arc<AtomicBool>,
        delay: Duration,
    }
    impl Write for SharedWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.delay);
            if self.failing.load(Ordering::SeqCst) {
                return Err(io::Error::other("disk full"));
            }
            self.contents.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.contents.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn stalled_disk_never_blocks_producers_and_memory_is_bounded() {
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let logger = AsyncLog::new(once(StalledWriter(entered, blocked))).unwrap();
        writeln!(logger.make_writer(), "first record").unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        for _ in 0..2048 {
            writeln!(logger.make_writer(), "still responsive").unwrap();
        }
        assert!(logger.dropped.load(Ordering::Relaxed) >= 1024);
        // A stalled disk bounds the wait for a flush too.
        let started = Instant::now();
        assert!(!logger.flush(Duration::from_millis(100)));
        assert!(started.elapsed() < Duration::from_secs(2));
        // Closing this gate releases every write, including queued records.
        drop(release);
    }

    #[test]
    fn flush_waits_for_every_queued_record() {
        let file = SharedWriter {
            delay: Duration::from_millis(5),
            ..Default::default()
        };
        let logger = AsyncLog::new(once(file.clone())).unwrap();
        for index in 0..20 {
            writeln!(logger.make_writer(), "record {index}").unwrap();
        }
        assert!(logger.flush(Duration::from_secs(5)));
        let text = file.text();
        assert_eq!(text.lines().count(), 20);
        assert!(text.ends_with("record 19\n"));
    }

    #[test]
    fn write_errors_reopen_the_log_after_a_backoff_and_report_losses() {
        let file = SharedWriter::default();
        let opens = Arc::new(AtomicU64::new(0));
        let logger = AsyncLog::new({
            let file = file.clone();
            let opens = opens.clone();
            move || {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(file.clone())
            }
        })
        .unwrap();
        writeln!(logger.make_writer(), "before").unwrap();
        assert!(logger.flush(Duration::from_secs(5)));
        file.failing.store(true, Ordering::SeqCst);
        writeln!(logger.make_writer(), "fails").unwrap();
        assert!(logger.flush(Duration::from_secs(5)));
        file.failing.store(false, Ordering::SeqCst);
        // Within the backoff the writer does not reopen the log, so this record is counted.
        writeln!(logger.make_writer(), "during backoff").unwrap();
        assert!(logger.flush(Duration::from_secs(5)));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        std::thread::sleep(FIRST_RETRY + Duration::from_millis(50));
        writeln!(logger.make_writer(), "after").unwrap();
        assert!(logger.flush(Duration::from_secs(5)));
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        assert_eq!(
            file.text(),
            "before\nWARN lost 2 log records while the log could not be written\nafter\n"
        );
    }

    #[test]
    fn failed_reopens_back_off_exponentially() {
        let opens = Arc::new(AtomicU64::new(0));
        let mut writer = Writer {
            open: {
                let opens = opens.clone();
                move || -> io::Result<SharedWriter> {
                    opens.fetch_add(1, Ordering::SeqCst);
                    Err(io::Error::other("still unavailable"))
                }
            },
            file: None,
            retry_at: Instant::now(),
            retry_delay: FIRST_RETRY,
            congested: Arc::new(AtomicU64::new(0)),
            lost: 0,
        };
        for _ in 0..100 {
            writer.write(b"record\n");
        }
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(writer.lost, 100);
        assert_eq!(writer.retry_delay, FIRST_RETRY * 2);
        for _ in 0..20 {
            writer.back_off();
        }
        assert_eq!(writer.retry_delay, MAX_RETRY);
    }
}
