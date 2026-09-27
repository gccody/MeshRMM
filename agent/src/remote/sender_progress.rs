//! When the current sender attempt started streaming, so the session loop's
//! reconnect backoff restarts only after a stable connection.

use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Default)]
pub struct SenderProgress {
    streaming_since: Mutex<Option<Instant>>,
}

impl SenderProgress {
    /// Records that the attempt's WebRTC connection opened at `at`.
    pub fn mark_streaming(&self, at: Instant) {
        *self
            .streaming_since
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(at);
    }

    /// When the attempt that just ended started streaming, if it did.
    /// Clears the record for the next attempt.
    pub fn take(&self) -> Option<Instant> {
        self.streaming_since
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn take_returns_the_streaming_start_once() {
        let progress = SenderProgress::default();
        assert_eq!(progress.take(), None);
        let started = Instant::now() - Duration::from_secs(30);
        progress.mark_streaming(started);
        let taken = progress.take().expect("the attempt streamed");
        assert_eq!(taken, started);
        assert!(taken.elapsed() >= Duration::from_secs(30));
        // The next attempt starts without a record.
        assert_eq!(progress.take(), None);
    }
}
