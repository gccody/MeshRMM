//! The native file picker, which runs on a thread of its own so the worker
//! keeps answering the session while the dialog is open.
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
    thread::JoinHandle,
    time::Duration,
};

/// Opens a picker and waits until it closes, returning what was chosen
/// (nothing when cancelled). Tests replace the native one.
pub(crate) type Picker = Arc<dyn Fn(&PickCancel) -> anyhow::Result<Vec<PathBuf>> + Send + Sync>;

/// How long a worker that is closing waits for a picker it cancelled to close.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Closes a picker that is open on another thread. Each platform's picker
/// registers how it is closed while it is open; cancelling runs that, or
/// keeps a picker that has not opened yet from opening.
#[derive(Clone, Default)]
pub struct PickCancel(Arc<(Mutex<CancelState>, Condvar)>);
#[derive(Default)]
struct CancelState {
    cancelled: bool,
    close: Option<Box<dyn FnOnce() + Send>>,
}
impl PickCancel {
    fn state(&self) -> MutexGuard<'_, CancelState> {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Records how to close the picker that is opening, unless the pick was
    /// cancelled already, which returns false.
    pub(crate) fn closes_with(&self, close: impl FnOnce() + Send + 'static) -> bool {
        let mut state = self.state();
        if state.cancelled {
            return false;
        }
        state.close = Some(Box::new(close));
        self.0.1.notify_all();
        true
    }
    /// The picker closed. Once this returns, a cancellation no longer runs
    /// what [`closes_with`](Self::closes_with) recorded, which runs under the
    /// same lock, so it never acts on what the picker used after it closed.
    pub(crate) fn closed(&self) {
        self.state().close = None;
    }
    /// Waits until a picker registered how it closes.
    #[cfg(test)]
    pub(crate) fn wait_until_open(&self) {
        let state = self.state();
        drop(
            self.0
                .1
                .wait_while(state, |state| state.close.is_none())
                .unwrap(),
        );
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.state().cancelled
    }
    pub(crate) fn cancel(&self) {
        let mut state = self.state();
        state.cancelled = true;
        if let Some(close) = state.close.take() {
            close();
        }
    }
}

/// Who the open picker chooses files for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickFor {
    /// This side's own Send files.
    Local,
    /// The peer's request, which the pick answers.
    Peer,
}

/// A picker open on its own thread.
pub(crate) struct OpenPick {
    pub(crate) purpose: PickFor,
    cancel: PickCancel,
    result: mpsc::Receiver<anyhow::Result<Vec<PathBuf>>>,
    thread: Option<JoinHandle<()>>,
}
impl OpenPick {
    pub(crate) fn start(picker: &Picker, purpose: PickFor) -> Self {
        let cancel = PickCancel::default();
        let (done, result) = mpsc::sync_channel(1);
        let (picker, closer) = (picker.clone(), cancel.clone());
        let thread = std::thread::Builder::new()
            .name("meshrmm-file-picker".into())
            .spawn(move || {
                let _ = done.send(picker(&closer));
            });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                // The picker never opened; report that as its result.
                let (done, failed) = mpsc::sync_channel(1);
                let _ = done.send(Err(anyhow::Error::new(error).context("could not open")));
                return Self {
                    purpose,
                    cancel,
                    result: failed,
                    thread: None,
                };
            }
        };
        Self {
            purpose,
            cancel,
            result,
            thread,
        }
    }
    /// What was chosen, once the picker closed.
    pub(crate) fn chosen(&mut self) -> Option<anyhow::Result<Vec<PathBuf>>> {
        let result = match self.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("the picker failed")),
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Some(result)
    }
}
impl Drop for OpenPick {
    /// Closes the picker and waits a few seconds for its thread to end. One
    /// that does not close in time is left to end by itself rather than
    /// holding up the session's end.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.cancel.cancel();
        match self.result.recv_timeout(CLOSE_WAIT) {
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!("the file picker did not close; leaving its thread to end");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cancelling closes a picker that is open, keeps one that has not opened
    /// from opening, and does nothing to one that already closed.
    #[test]
    fn cancelling_closes_only_an_open_picker() {
        let cancel = PickCancel::default();
        let (closing, closed) = mpsc::channel();
        let picker = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                assert!(cancel.closes_with(move || closing.send(()).unwrap()));
                closed.recv().unwrap();
                cancel.closed();
            })
        };
        cancel.wait_until_open();
        cancel.cancel();
        picker.join().unwrap();
        assert!(cancel.is_cancelled());
        assert!(!cancel.closes_with(|| panic!("a cancelled pick does not open")));

        let finished = PickCancel::default();
        assert!(finished.closes_with(|| panic!("closed already")));
        finished.closed();
        finished.cancel();
    }
}
