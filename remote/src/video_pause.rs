//! Whether the device can stop sending video: no window on this computer
//! shows the session's video, and nothing keeps its frames. Each of the
//! session's windows says whether it shows the video (a minimised Windows
//! window, a miniaturised macOS window or a hidden application does not),
//! and the recorder says whether it records. Each connection tells the
//! device once its video is up: after [`PAUSE_DELAY`] without a window that
//! shows it, so that a quick minimise and restore changes nothing, and at
//! once when a window shows it again or a recording starts.
//!
//! Only what is minimised or hidden counts, not a window covered by others.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::watch;
use tokio::time::{Duration, Instant};

/// How long no window may show the video before the device pauses it.
pub const PAUSE_DELAY: Duration = Duration::from_secs(2);

/// What on this computer shows or keeps the session's video. It lasts for
/// the session, across reconnects.
#[derive(Clone)]
pub struct VideoPause(Arc<Shared>);

struct Shared {
    viewing: watch::Sender<Viewing>,
    next_window: AtomicU64,
}

impl Default for VideoPause {
    fn default() -> Self {
        Self(Arc::new(Shared {
            viewing: watch::Sender::new(Viewing::default()),
            next_window: AtomicU64::new(0),
        }))
    }
}

/// What shows or keeps the video now.
#[derive(Clone, Debug, Default)]
pub(crate) struct Viewing {
    /// Each window of the session that can show its video, and whether it
    /// does.
    windows: BTreeMap<u64, bool>,
    /// Since when no window has shown the video, while that is so.
    hidden_since: Option<Instant>,
    recording: bool,
}

impl Viewing {
    fn windows_changed(&mut self, now: Instant) {
        let hidden = !self.windows.is_empty() && self.windows.values().all(|shown| !shown);
        self.hidden_since = hidden.then(|| self.hidden_since.unwrap_or(now));
    }

    /// Whether the device may pause the video at `now`, and when it may if
    /// it may not yet.
    fn pause_at(&self, now: Instant) -> (bool, Option<Instant>) {
        match self.hidden_since {
            Some(since) if !self.recording => {
                let due = since + PAUSE_DELAY;
                (now >= due, (now < due).then_some(due))
            }
            _ => (false, None),
        }
    }
}

impl VideoPause {
    /// A window of the session that can show its video, and shows it now
    /// when `shown`. It counts until it is dropped.
    pub fn window(&self, shown: bool) -> VideoWindow {
        let id = self.0.next_window.fetch_add(1, Ordering::Relaxed);
        self.0.viewing.send_modify(|viewing| {
            viewing.windows.insert(id, shown);
            viewing.windows_changed(Instant::now());
        });
        VideoWindow {
            pause: self.clone(),
            id,
        }
    }

    /// A recording keeps the video's frames, so the device must send them.
    pub fn set_recording(&self, recording: bool) {
        self.0.viewing.send_if_modified(|viewing| {
            let changed = viewing.recording != recording;
            viewing.recording = recording;
            changed
        });
    }

    pub(crate) fn watch(&self) -> watch::Receiver<Viewing> {
        self.0.viewing.subscribe()
    }
}

/// One of the session's windows, as [`VideoPause`] counts it.
pub struct VideoWindow {
    pause: VideoPause,
    id: u64,
}

impl VideoWindow {
    /// The window shows the video (`shown`), or is minimised or hidden.
    pub fn set_shown(&self, shown: bool) {
        self.pause.0.viewing.send_if_modified(|viewing| {
            let Some(current) = viewing.windows.get_mut(&self.id) else {
                return false;
            };
            if *current == shown {
                return false;
            }
            *current = shown;
            viewing.windows_changed(Instant::now());
            true
        });
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        self.pause.0.viewing.send_modify(|viewing| {
            viewing.windows.remove(&self.id);
            viewing.windows_changed(Instant::now());
        });
    }
}

/// One connection's video, as the device's pause depends on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConnectionVideo {
    /// The window shows the connection's stream: it presented a frame. Until
    /// then the device streams, so the connection's first frame shows and
    /// the reconnect ends.
    pub(crate) presented: bool,
    /// The streams the device configured on this connection.
    pub(crate) streams: u64,
}

/// Tells the device through `send` whether to pause the video, for as long
/// as the connection lasts. The device starts each connection unpaused; the
/// state is sent once the connection's video is up and differs from that,
/// whenever it changes, and again for each new stream. `told` holds what the
/// device was told last.
pub(crate) async fn tell_device(
    mut viewing: watch::Receiver<Viewing>,
    mut video: watch::Receiver<ConnectionVideo>,
    told: Arc<AtomicBool>,
    mut send: impl FnMut(bool),
) {
    let mut paused = false;
    let mut streams = video.borrow().streams;
    loop {
        let now = Instant::now();
        let (wanted, wake) = viewing.borrow_and_update().pause_at(now);
        let current = *video.borrow_and_update();
        let wanted = wanted && current.presented;
        let new_stream = std::mem::replace(&mut streams, current.streams) != current.streams;
        if wanted != paused || (new_stream && current.presented) {
            paused = wanted;
            told.store(paused, Ordering::Release);
            tracing::info!(
                paused,
                "telling the device whether this viewer shows the video"
            );
            send(paused);
        }
        tokio::select! {
            changed = viewing.changed() => if changed.is_err() { return },
            changed = video.changed() => if changed.is_err() { return },
            () = tokio::time::sleep_until(wake.unwrap_or(now)), if wake.is_some() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A connection whose driver tells the device through `sent`.
    struct Connection {
        video: watch::Sender<ConnectionVideo>,
        told: Arc<AtomicBool>,
        sent: Arc<Mutex<Vec<bool>>>,
        driver: tokio::task::JoinHandle<()>,
    }

    impl Connection {
        /// A connection whose video is up when `presented`.
        fn open(pause: &VideoPause, presented: bool) -> Self {
            let video = watch::Sender::new(ConnectionVideo {
                presented,
                streams: 1,
            });
            let told = Arc::new(AtomicBool::new(false));
            let sent = Arc::new(Mutex::new(Vec::new()));
            let driver = tokio::spawn(tell_device(
                pause.watch(),
                video.subscribe(),
                Arc::clone(&told),
                {
                    let sent = Arc::clone(&sent);
                    move |paused| sent.lock().unwrap().push(paused)
                },
            ));
            Self {
                video,
                told,
                sent,
                driver,
            }
        }

        /// What the device was told since the last look.
        fn sent(&self) -> Vec<bool> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }

        fn present(&self) {
            self.video.send_modify(|video| video.presented = true);
        }

        fn new_stream(&self) {
            self.video.send_modify(|video| video.streams += 1);
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            self.driver.abort();
        }
    }

    /// Lets `duration` pass on the test's paused clock, and the driver run.
    async fn wait(duration: Duration) {
        tokio::time::sleep(duration).await;
        tokio::task::yield_now().await;
    }

    const MOMENT: Duration = Duration::from_millis(1);

    #[tokio::test(start_paused = true)]
    async fn a_minimise_shorter_than_the_delay_tells_the_device_nothing() {
        let pause = VideoPause::default();
        let window = pause.window(true);
        let connection = Connection::open(&pause, true);
        wait(MOMENT).await;

        window.set_shown(false);
        wait(PAUSE_DELAY - Duration::from_millis(100)).await;
        window.set_shown(true);
        wait(PAUSE_DELAY * 5).await;
        assert!(connection.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_longer_minimise_pauses_once_and_a_restore_resumes_at_once() {
        let pause = VideoPause::default();
        let window = pause.window(true);
        let connection = Connection::open(&pause, true);
        wait(MOMENT).await;

        window.set_shown(false);
        wait(PAUSE_DELAY - Duration::from_millis(100)).await;
        assert!(connection.sent().is_empty());
        wait(Duration::from_millis(200)).await;
        assert_eq!(connection.sent(), [true]);
        assert!(connection.told.load(Ordering::Acquire));
        wait(Duration::from_secs(600)).await;
        assert!(connection.sent().is_empty(), "the pause is told once");

        window.set_shown(true);
        wait(MOMENT).await;
        assert_eq!(connection.sent(), [false]);
        assert!(!connection.told.load(Ordering::Acquire));
    }

    #[tokio::test(start_paused = true)]
    async fn the_video_pauses_only_while_no_window_of_the_session_shows_it() {
        let pause = VideoPause::default();
        let first = pause.window(true);
        let second = pause.window(true);
        let connection = Connection::open(&pause, true);

        first.set_shown(false);
        wait(PAUSE_DELAY * 2).await;
        assert!(connection.sent().is_empty());
        second.set_shown(false);
        wait(PAUSE_DELAY * 2).await;
        assert_eq!(connection.sent(), [true]);
        // A window that closes leaves the other, which does not show it.
        drop(first);
        wait(PAUSE_DELAY * 2).await;
        assert!(connection.sent().is_empty());
        second.set_shown(true);
        wait(MOMENT).await;
        assert_eq!(connection.sent(), [false]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recording_keeps_the_video_and_one_that_starts_while_paused_resumes_it() {
        let pause = VideoPause::default();
        let window = pause.window(true);
        let connection = Connection::open(&pause, true);
        pause.set_recording(true);
        window.set_shown(false);
        wait(PAUSE_DELAY * 5).await;
        assert!(connection.sent().is_empty(), "a recording blocks the pause");

        // The recording ends long after the window was minimised.
        pause.set_recording(false);
        wait(MOMENT).await;
        assert_eq!(connection.sent(), [true]);
        pause.set_recording(true);
        wait(MOMENT).await;
        assert_eq!(connection.sent(), [false]);
    }

    #[tokio::test(start_paused = true)]
    async fn each_connection_is_told_again_once_its_video_is_up() {
        let pause = VideoPause::default();
        let window = pause.window(true);
        window.set_shown(false);
        let first = Connection::open(&pause, true);
        wait(PAUSE_DELAY * 2).await;
        assert_eq!(first.sent(), [true]);

        // A reconnect while minimised: the new connection's device streams
        // until the connection's first frame is up, then pauses at once.
        drop(first);
        let second = Connection::open(&pause, false);
        wait(PAUSE_DELAY * 5).await;
        assert!(second.sent().is_empty());
        assert!(!second.told.load(Ordering::Acquire));
        second.present();
        wait(MOMENT).await;
        assert_eq!(second.sent(), [true]);
        // A new stream is told the state again.
        second.new_stream();
        wait(MOMENT).await;
        assert_eq!(second.sent(), [true]);

        // Restored while disconnected: the next connection streams, and is
        // told nothing, as the device starts each connection unpaused.
        drop(second);
        window.set_shown(true);
        let third = Connection::open(&pause, false);
        third.present();
        wait(PAUSE_DELAY * 5).await;
        assert!(third.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn no_window_yet_is_not_a_hidden_one() {
        let pause = VideoPause::default();
        let connection = Connection::open(&pause, true);
        wait(PAUSE_DELAY * 5).await;
        assert!(connection.sent().is_empty());
    }
}
