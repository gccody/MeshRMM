//! The server logs to standard error through [`AsyncLog`]: one thread writes every line, so a
//! slow reader of standard error (journald under load, `docker logs`, a paused terminal, a
//! full pipe to a log shipper) never blocks the runtime's workers. While the reader stalls,
//! lines past the queue are dropped and counted, and the count is logged once it catches up,
//! in the same format as every other line.
use std::io::{self, Write};
use std::time::Duration;

use meshrmm_log_file::{AsyncLog, Notice};
use serde_json::json;
use tracing::Dispatch;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{LogConfig, LogFormat};

/// Lines queued for standard error before new ones are dropped. A reader that keeps up drains
/// far faster than the server logs, so the queue fills only while the reader stalls; this rides
/// out a short stall during a burst such as thousands of Agents reconnecting at once, for a few
/// MiB at a few hundred bytes a line (each line is cut off at 64 KiB).
const QUEUE_LINES: usize = 4096;
/// How long exit and a panic wait for queued lines to reach standard error. Only a stalled
/// reader makes them wait, and one that never resumes must not keep the server from exiting.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// The server's log. Dropping it writes the lines still queued, so keep it until `main` ends.
pub struct Logging(AsyncLog);

impl Drop for Logging {
    fn drop(&mut self) {
        self.0.flush(FLUSH_TIMEOUT);
    }
}

/// Logs to standard error for the rest of the process, as `config` says. A panic writes the
/// lines logged before it, then its message.
pub fn init(config: &LogConfig) -> io::Result<Logging> {
    let (dispatch, log) = dispatch(config, || Ok(io::stderr()))?;
    dispatch.init();
    let panic_log = log.clone();
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        panic_log.flush(FLUSH_TIMEOUT);
        report(info);
    }));
    Ok(Logging(log))
}

/// A subscriber that writes to what `open` returns, through [`AsyncLog`].
fn dispatch<W: Write + Send + 'static>(
    config: &LogConfig,
    open: impl FnMut() -> io::Result<W> + Send + 'static,
) -> io::Result<(Dispatch, AsyncLog)> {
    let filter = EnvFilter::try_new(&config.level).unwrap_or_else(|error| {
        eprintln!(
            "log.level {:?} is invalid ({error}); using info",
            config.level
        );
        EnvFilter::new("info")
    });
    let log = match config.format {
        LogFormat::Text => AsyncLog::new(QUEUE_LINES, open)?,
        LogFormat::Json => AsyncLog::with_notices(QUEUE_LINES, open, json_notice)?,
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(log.clone());
    let dispatch = match config.format {
        LogFormat::Text => Dispatch::new(builder.finish()),
        LogFormat::Json => Dispatch::new(builder.json().finish()),
    };
    Ok((dispatch, log))
}

/// A count of dropped or lost lines as a line of the JSON log, shaped as the JSON formatter
/// writes a `tracing::warn!(count, ...)` from this module, so a collector parses it as any other.
fn json_notice(notice: &Notice) -> Vec<u8> {
    let mut timestamp = String::new();
    // The JSON formatter's own clock, so the timestamp reads as every other line's.
    let _ = SystemTime.format_time(&mut tracing_subscriber::fmt::format::Writer::new(
        &mut timestamp,
    ));
    let mut line = json!({
        "timestamp": timestamp,
        "level": notice.level,
        "fields": { "message": notice.message, "count": notice.count },
        "target": module_path!(),
    })
    .to_string()
    .into_bytes();
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use tracing::instrument::WithSubscriber;

    /// Standard error whose reader has stalled: every write waits until the gate opens.
    struct Stalled {
        written: Arc<Mutex<Vec<u8>>>,
        entered: mpsc::Sender<()>,
        gate: mpsc::Receiver<()>,
    }

    impl Write for Stalled {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.entered.send(());
            // Dropping the gate's sender opens it for good.
            let _ = self.gate.recv();
            self.written.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The test's end of a [`Stalled`] standard error.
    struct Reader {
        written: Arc<Mutex<Vec<u8>>>,
        entered: mpsc::Receiver<()>,
        gate: Option<mpsc::Sender<()>>,
    }

    impl Reader {
        /// Waits until the writer is stuck writing a line.
        fn wait_for_a_write(&self) {
            self.entered.recv_timeout(Duration::from_secs(60)).unwrap();
        }

        /// The reader resumes, for good.
        fn resume(&mut self) {
            self.gate = None;
        }

        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.written.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn stalled(format: LogFormat) -> (Dispatch, AsyncLog, Reader) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (entered, entered_receiver) = mpsc::channel();
        let (gate, gate_receiver) = mpsc::channel();
        let mut stderr = Some(Stalled {
            written: written.clone(),
            entered,
            gate: gate_receiver,
        });
        let config = LogConfig {
            level: "info".into(),
            format,
        };
        let (dispatch, log) = dispatch(&config, move || {
            stderr.take().ok_or_else(|| io::Error::other("reopened"))
        })
        .unwrap();
        let reader = Reader {
            written,
            entered: entered_receiver,
            gate: Some(gate),
        };
        (dispatch, log, reader)
    }

    /// Lines logged into a full queue while standard error stalls are dropped.
    const DROPPED: usize = 100;

    /// What standard error reads after one line stalls it and `QUEUE_LINES + DROPPED` more are
    /// logged by the runtime's workers, which never wait for it.
    fn overflow(format: LogFormat) -> Vec<String> {
        // Declared before the reader, so a failing test opens the gate before the runtime
        // waits for its workers.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let (dispatch, log, mut reader) = stalled(format);
        runtime.spawn(async { tracing::info!("first") }.with_subscriber(dispatch.clone()));
        reader.wait_for_a_write();
        // This thread waits, not the runtime, whose timers stop if logging blocks its workers.
        let (logged, done) = mpsc::channel();
        runtime.spawn(
            async move {
                for line in 0..QUEUE_LINES + DROPPED {
                    tracing::info!(line, "queued");
                }
                let _ = logged.send(());
            }
            .with_subscriber(dispatch),
        );
        done.recv_timeout(Duration::from_secs(60))
            .expect("logging waited for standard error");
        assert!(reader.lines().is_empty());

        reader.resume();
        assert!(log.flush(Duration::from_secs(60)));
        let lines = reader.lines();
        assert_eq!(lines.len(), 2 + QUEUE_LINES);
        lines
    }

    fn json(line: &str) -> serde_json::Value {
        serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}"))
    }

    #[test]
    fn a_stalled_stderr_never_blocks_a_worker_and_the_lines_it_drops_are_counted() {
        let lines = overflow(LogFormat::Json);
        let first = json(&lines[0]);
        assert_eq!(first["fields"]["message"], "first");
        // The count is a line of the JSON log too, with the fields every warning has.
        let notice = json(&lines[1]);
        assert_eq!(notice["level"], "WARN");
        assert_eq!(notice["fields"]["count"], DROPPED);
        assert_eq!(
            notice["fields"]["message"],
            format!("dropped {DROPPED} log records while logging was congested")
        );
        assert!(notice["timestamp"].as_str().is_some_and(|t| !t.is_empty()));
        assert_eq!(notice["target"], "meshrmm_server::logging");
        let keys = |line: &serde_json::Value| {
            line.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&notice), keys(&first));
        // The queued lines come whole and in order; those logged into the full queue are gone.
        for (index, line) in lines[2..].iter().enumerate() {
            let fields = &json(line)["fields"];
            assert_eq!(fields["message"], "queued");
            assert_eq!(fields["line"], index);
        }
    }

    #[test]
    fn the_text_log_counts_dropped_lines_in_plain_text() {
        let lines = overflow(LogFormat::Text);
        assert!(lines[0].ends_with(" first"), "{}", lines[0]);
        assert_eq!(
            lines[1],
            format!("WARN dropped {DROPPED} log records while logging was congested")
        );
        assert!(lines[2..].iter().all(|line| line.contains("queued")));
    }

    #[test]
    fn exit_waits_for_the_queued_lines() {
        let (dispatch, log, mut reader) = stalled(LogFormat::Text);
        tracing::dispatcher::with_default(&dispatch, || {
            for line in 0..10 {
                tracing::info!("shutting down {line}");
            }
        });
        reader.wait_for_a_write();
        let (exited, exit) = mpsc::channel();
        let main = std::thread::spawn(move || {
            drop(Logging(log));
            let _ = exited.send(());
        });
        // Exit waits for the reader, for up to FLUSH_TIMEOUT.
        assert_eq!(
            exit.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        reader.resume();
        exit.recv_timeout(Duration::from_secs(60)).unwrap();
        main.join().unwrap();
        let lines = reader.lines();
        assert_eq!(lines.len(), 10);
        for (index, line) in lines.iter().enumerate() {
            assert!(line.ends_with(&format!(" shutting down {index}")), "{line}");
        }
    }
}
