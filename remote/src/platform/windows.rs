use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{
    ChromaMode, Codec, CursorShape, Display, EncodedFrame, PointerButton, QualityPreset,
    RemoteInput, SessionCloseAction, SessionMessage, VideoFormat, VideoProfile,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT,
    RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, DEFAULT_GUI_FONT, GetStockObject, HBRUSH, ScreenToClient, SetBkColor, SetTextColor,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, SetFocus,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Interface, PCWSTR, w};

use super::ControlSink;
use crate::debug::DebugInfo;
use crate::reconnect::{ReconnectStatus, ReconnectText};

mod frame_schedule;
mod keyboard_hook;
mod launch_window;
mod pipeline;
#[cfg(test)]
mod reconnect_probe;
mod renderer;
#[cfg(test)]
mod reset_probe;
#[cfg(test)]
mod toolbar_probe;
mod window;

use frame_schedule::FrameSchedule;
pub use launch_window::{close_launch_status, show_launch_status};
use pipeline::WorkerPipeline;
use renderer::D3d11Renderer;
use window::{create_window, pump_window_messages, set_window_cursor};

const MAX_DECODER_PENDING_FRAMES: usize = 16;
// Moonlight's depacketizer permits 15 queued decode units. The worker
// decodes everything queued on each pass, so the queue only fills while the
// decoder refuses input or the worker's thread is held, as by a window drag.
const MAX_PRESENTER_QUEUE_FRAMES: usize = 15;
const DECODER_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the worker redraws the reconnect overlay's elapsed time and
/// countdown. More often than once a second, so the counts do not skip one.
const RECONNECT_REFRESH: Duration = Duration::from_millis(250);
/// How long a stream reset waits for the worker thread before the caller
/// replaces the presenter instead.
const STREAM_RESET_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stopped presenter's worker thread has to end before its window
/// is hidden and the thread is left to end by itself.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const VIEWER_TOOLBAR_HEIGHT: u32 = crate::toolbar::HEIGHT as u32;

struct QueuedFrame {
    frame: EncodedFrame,
    received_at_us: u64,
}

/// A replacement stream for the worker to apply to its window and device.
struct PendingReset {
    format: VideoFormat,
    display: Display,
    displays: Vec<Display>,
    reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
}

/// An auto-reset Win32 event, which the worker can wait on together with
/// the swap chain.
struct WakeEvent(HANDLE);

// SAFETY: an event handle may be signalled and waited on from any thread.
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    fn new() -> Self {
        // Without an event the worker only wakes on its timeouts.
        let handle = unsafe { CreateEventW(None, false, false, None) }.unwrap_or_else(|error| {
            tracing::warn!(error = %error, "could not create the presenter's wake event");
            HANDLE::default()
        });
        Self(handle)
    }

    fn handle(&self) -> HANDLE {
        self.0
    }

    fn signal(&self) {
        if !self.0.is_invalid() {
            let _ = unsafe { SetEvent(self.0) };
        }
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct Shared {
    queued: Mutex<VecDeque<QueuedFrame>>,
    /// The stream the worker should ask a keyframe for, after the queue
    /// overflowed.
    keyframe_wanted: Mutex<Option<meshrmm_protocol::VideoStreamId>>,
    /// Signalled with `ready` when a frame is queued, so the worker also
    /// wakes while it waits for the swap chain.
    frame_queued: WakeEvent,
    first_presented: Arc<OnceLock<std::time::Instant>>,
    cursor_shape: Mutex<Option<CursorShape>>,
    agent_pointer_display: Mutex<Option<Option<meshrmm_protocol::DisplayId>>>,
    ready: Condvar,
    stopping: AtomicBool,
    reconnect_status: Mutex<Option<ReconnectStatus>>,
    reconnect_changed: AtomicBool,
    running: AtomicBool,
    failure: Mutex<Option<String>>,
    replaced_frames: AtomicU64,
    recovering: AtomicBool,
    resetting: AtomicBool,
    reset: Mutex<Option<PendingReset>>,
    /// The worker's window, as an `HWND` (0 when there is none), for hiding
    /// it from another thread.
    window: AtomicUsize,
    /// Set once the worker's thread has ended, however it ended.
    worker_ended: Mutex<bool>,
    worker_ended_changed: Condvar,
    control: ControlSink,
    debug: DebugInfo,
}

impl Shared {
    fn new(control: ControlSink, debug: DebugInfo) -> Self {
        Self {
            queued: Mutex::new(VecDeque::with_capacity(MAX_PRESENTER_QUEUE_FRAMES)),
            keyframe_wanted: Mutex::new(None),
            frame_queued: WakeEvent::new(),
            first_presented: Default::default(),
            cursor_shape: Mutex::new(None),
            agent_pointer_display: Mutex::new(None),
            ready: Condvar::new(),
            stopping: AtomicBool::new(false),
            reconnect_status: Mutex::new(None),
            reconnect_changed: AtomicBool::new(false),
            running: AtomicBool::new(false),
            failure: Mutex::new(None),
            replaced_frames: AtomicU64::new(0),
            recovering: AtomicBool::new(false),
            resetting: AtomicBool::new(false),
            reset: Mutex::new(None),
            window: AtomicUsize::new(0),
            worker_ended: Mutex::new(false),
            worker_ended_changed: Condvar::new(),
            control,
            debug,
        }
    }

    /// Wakes the worker for a queued frame.
    fn wake(&self) {
        self.ready.notify_one();
        self.frame_queued.signal();
    }

    fn reset_pending(&self) -> bool {
        self.reset.lock().is_ok_and(|reset| reset.is_some())
    }

    fn record_failure(&self, message: String) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(message);
        }
    }

    fn set_reconnect_status(&self, status: Option<ReconnectStatus>) {
        if let Ok(mut current) = self.reconnect_status.lock() {
            *current = status;
        }
        self.reconnect_changed.store(true, Ordering::Release);
    }

    /// Hides the worker's window. The request is posted to its thread, so
    /// it takes effect also while a modal loop holds the thread: those
    /// dispatch it.
    fn hide_window(&self) {
        let window = self.window.load(Ordering::Acquire);
        if window != 0 {
            let _ = unsafe { ShowWindowAsync(HWND(window as *mut c_void), SW_HIDE) };
        }
    }
}

/// Marks the worker's thread ended when dropped, as it is when the thread
/// returns or unwinds.
struct WorkerEnded(Arc<Shared>);

impl Drop for WorkerEnded {
    fn drop(&mut self) {
        let mut ended = self
            .0
            .worker_ended
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *ended = true;
        self.0.worker_ended_changed.notify_all();
    }
}

/// The caller's side of a stream reset handed to the worker. Dropping it,
/// answered or not, withdraws a reset the worker has not taken, which it
/// must not apply once the caller has fallen back, and lets frames through
/// again.
struct ResetWait {
    shared: Arc<Shared>,
    response: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
}

impl ResetWait {
    async fn answer(mut self, timeout: Duration) -> anyhow::Result<()> {
        match tokio::time::timeout(timeout, &mut self.response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow::anyhow!(
                "the Windows presentation thread stopped before resetting the stream"
            )),
            Err(_) => Err(anyhow::anyhow!(
                "the Windows presentation thread did not reset the stream within {} ms",
                timeout.as_millis()
            )),
        }
    }
}

impl Drop for ResetWait {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.reset.lock() {
            pending.take();
        }
        self.shared.resetting.store(false, Ordering::Release);
    }
}

/// Keeps a window's reconnect overlay in step with its presenter's status.
struct ReconnectOverlay {
    shown: Option<ReconnectText>,
    rendered: std::time::Instant,
}

impl ReconnectOverlay {
    fn new() -> Self {
        Self {
            shown: None,
            rendered: std::time::Instant::now(),
        }
    }

    /// Redraws the overlay when the status changed, and while it is shown,
    /// when its text changed: about once a second.
    unsafe fn refresh(&mut self, window: HWND, shared: &Shared) {
        let changed = shared.reconnect_changed.swap(false, Ordering::AcqRel);
        if !changed && (self.shown.is_none() || self.rendered.elapsed() < RECONNECT_REFRESH) {
            return;
        }
        self.rendered = std::time::Instant::now();
        let wanted = shared
            .reconnect_status
            .lock()
            .ok()
            .and_then(|status| *status)
            .map(|status| status.render(self.rendered));
        if wanted != self.shown {
            unsafe { window::set_reconnect_text(window, wanted.as_ref()) };
            self.shown = wanted;
        }
    }
}

pub struct Presenter {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Presenter {
    pub fn start(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared::new(control.clone(), debug));
        let worker_shared = Arc::clone(&shared);
        let worker_debug = worker_shared.debug.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("meshrmm-decode-present".into())
            .spawn(move || {
                let _ended = WorkerEnded(Arc::clone(&worker_shared));
                run_worker(
                    worker_shared,
                    format,
                    active_display,
                    displays,
                    control,
                    worker_debug,
                    started_tx,
                )
            })
            .context("failed to spawn Windows decode/presentation worker")?;
        started_rx
            .recv()
            .context("Windows decode/presentation worker exited during startup")??;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn publish(&self, frame: EncodedFrame, received_at_us: u64) -> bool {
        let stream_id = frame.stream_id;
        let Ok(mut queued) = self.shared.queued.lock() else {
            return false;
        };
        if self.shared.resetting.load(Ordering::Acquire) {
            self.shared.replaced_frames.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if self.shared.recovering.load(Ordering::Acquire) && !frame.keyframe {
            self.shared.replaced_frames.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if queued.len() >= MAX_PRESENTER_QUEUE_FRAMES && !frame.keyframe {
            self.shared
                .replaced_frames
                .fetch_add(queued.len() as u64 + 1, Ordering::Relaxed);
            queued.clear();
            let first_loss = !self.shared.recovering.swap(true, Ordering::AcqRel);
            if first_loss {
                // The worker asks once it runs: while its thread is held,
                // the keyframe would only overflow the queue again.
                if let Ok(mut wanted) = self.shared.keyframe_wanted.lock() {
                    *wanted = Some(stream_id);
                }
                tracing::warn!(
                    stream_id = stream_id.0,
                    "video frames queued faster than they were decoded; dropped them"
                );
            }
            drop(queued);
            self.shared.wake();
            return false;
        }
        if frame.keyframe {
            if self.shared.recovering.load(Ordering::Acquire)
                || queued.len() >= MAX_PRESENTER_QUEUE_FRAMES
            {
                self.shared
                    .replaced_frames
                    .fetch_add(queued.len() as u64, Ordering::Relaxed);
                queued.clear();
            }
            // It answers any keyframe the worker has yet to ask for.
            if let Ok(mut wanted) = self.shared.keyframe_wanted.lock() {
                wanted.take();
            }
        }
        queued.push_back(QueuedFrame {
            frame,
            received_at_us,
        });
        self.shared.recovering.store(false, Ordering::Release);
        drop(queued);
        self.shared.wake();
        true
    }

    /// Whether [`Self::reset_stream`] can take `next` in the current window.
    /// The worker recreates the decoder and video processor on its device for
    /// any change, and a reset that fails leaves the caller to replace the
    /// presenter.
    pub fn can_reset_in_place(_current: VideoFormat, _next: VideoFormat) -> bool {
        true
    }

    /// Moves the window, device and swap chain to a replacement stream
    /// instead of opening a new window. The worker thread owns them and
    /// applies the reset between window messages, so a modal loop on it,
    /// such as a drag of the window or a message box, holds the reset up and
    /// it may time out.
    ///
    /// The reset is handed to the worker before this returns; the future
    /// only waits for the answer and borrows nothing, so the caller can
    /// release the presenter first. Frames published meanwhile are dropped.
    pub fn reset_stream(
        &self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        self.reset_stream_within(format, display, displays, STREAM_RESET_TIMEOUT)
    }

    fn reset_stream_within(
        &self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
        timeout: Duration,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        let handed_over = self.hand_over_reset(format, display, displays);
        async move { handed_over?.answer(timeout).await }
    }

    fn hand_over_reset(
        &self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> anyhow::Result<ResetWait> {
        if !self.shared.running.load(Ordering::Acquire) {
            bail!("the Windows decode/presentation worker is not running");
        }
        self.shared.resetting.store(true, Ordering::Release);
        self.shared.recovering.store(true, Ordering::Release);
        if let Ok(mut queued) = self.shared.queued.lock() {
            self.shared
                .replaced_frames
                .fetch_add(queued.len() as u64, Ordering::Relaxed);
            queued.clear();
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        // From here on, dropping it ends the reset.
        let wait = ResetWait {
            shared: Arc::clone(&self.shared),
            response,
        };
        let Ok(mut pending) = self.shared.reset.lock() else {
            bail!("the Windows presenter's reset state is unavailable");
        };
        *pending = Some(PendingReset {
            format,
            display,
            displays,
            reply,
        });
        drop(pending);
        self.shared.ready.notify_all();
        Ok(wait)
    }

    pub fn set_agent_pointer_display(&self, display_id: Option<meshrmm_protocol::DisplayId>) {
        if let Ok(mut pending) = self.shared.agent_pointer_display.lock() {
            *pending = Some(display_id);
        }
        self.shared.ready.notify_one();
    }

    /// The window pump already refreshes controls without waiting for frames.
    pub fn refresh_controls(&self) {}

    /// Shows why the window is waiting for the connection to be restored,
    /// or hides that (`None`).
    pub fn set_reconnect_status(&self, status: Option<ReconnectStatus>) {
        self.shared.set_reconnect_status(status);
    }

    pub fn set_cursor_shape(&self, shape: CursorShape) {
        if let Ok(mut pending) = self.shared.cursor_shape.lock() {
            *pending = Some(shape);
        }
    }

    /// Tells the worker to close its window and end, without waiting for
    /// it: a modal loop can hold its thread for as long as a person drags
    /// the window or leaves a message box open. A thread of its own waits
    /// for it instead; see [`retire_worker`].
    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        self.shared.frame_queued.signal();
        let Some(worker) = self.worker.take() else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        if let Err(error) = std::thread::Builder::new()
            .name("meshrmm-presenter-stop".into())
            .spawn(move || retire_worker(&shared, worker, WORKER_STOP_TIMEOUT))
        {
            tracing::warn!(error = %error, "could not wait for the Windows presentation thread to stop");
        }
    }

    pub fn first_presented_at(&self) -> Option<std::time::Instant> {
        self.shared.first_presented.get().copied()
    }

    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        if self.shared.running.load(Ordering::Acquire) {
            return None;
        }
        let failure = self
            .shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take());
        Some(failure.map_or(Ok(()), Err))
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Waits up to `timeout` for a stopping worker's thread to end, and returns
/// whether it did. A thread held longer by a modal loop ends once that loop
/// returns, since it is stopping; until then its window is hidden, so no
/// stale picture stays on screen, and the thread is left to end by itself.
/// A message box it shows stays up until the person closes it, and a viewer
/// that exits meanwhile ends the thread with it.
fn retire_worker(shared: &Shared, worker: JoinHandle<()>, timeout: Duration) -> bool {
    let ended = shared
        .worker_ended
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (ended, _) = shared
        .worker_ended_changed
        .wait_timeout_while(ended, timeout, |ended| !*ended)
        .unwrap_or_else(|error| error.into_inner());
    let ended = *ended;
    if !ended {
        tracing::warn!(
            timeout_ms = timeout.as_millis(),
            "the Windows presentation thread did not stop in time; hiding its window until it does"
        );
        shared.hide_window();
        return false;
    }
    if worker.join().is_err() {
        tracing::warn!("Windows decode/presentation worker panicked");
    }
    tracing::info!(
        latest_frames_dropped = shared.replaced_frames.load(Ordering::Relaxed),
        "video presenter stopped"
    );
    true
}

fn run_worker(
    shared: Arc<Shared>,
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
    started: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
) {
    let initialized = unsafe {
        WorkerPipeline::new(
            format,
            active_display,
            displays,
            control,
            debug,
            Arc::clone(&shared.first_presented),
        )
    };
    let mut pipeline = match initialized {
        Ok(pipeline) => {
            shared
                .window
                .store(pipeline.window().0 as usize, Ordering::Release);
            shared.running.store(true, Ordering::Release);
            let _ = started.send(Ok(()));
            pipeline
        }
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };

    let mut decoder_blocked_since = None::<std::time::Instant>;
    let mut reconnect_overlay = ReconnectOverlay::new();
    let mut schedule = FrameSchedule::new();
    while !shared.stopping.load(Ordering::Acquire) {
        if unsafe { pump_window_messages(pipeline.window()) } {
            break;
        }
        if unsafe { apply_pending_reset(&shared, &mut pipeline) } {
            // The new decoder has not been offered any input yet, and a
            // picture still waiting belongs to the old stream.
            decoder_blocked_since = None;
            schedule.clear();
        }
        unsafe { reconnect_overlay.refresh(pipeline.window(), &shared) };
        if let Some(layout) = unsafe { window::take_resize(pipeline.window()) }
            && let Err(error) = unsafe { pipeline.resize(&layout) }
        {
            tracing::error!(error = %error, "viewer swap chain resize failed");
            shared.record_failure(error.to_string());
            break;
        }
        unsafe { apply_pending_pointer_state(&shared, &pipeline) };
        if let Err(error) =
            unsafe { schedule.run(&shared, &mut pipeline, monotonic_timestamp_us()) }
        {
            tracing::error!(error = %format!("{error:#}"), "video decode/presentation failed");
            shared.record_failure(format!("{error:#}"));
            break;
        }
        pipeline.update_statistics(shared.replaced_frames.load(Ordering::Relaxed));
        if !pipeline.wants_input() {
            if let Some(message) = decoder_input_stall(&shared, &mut decoder_blocked_since) {
                shared.record_failure(message);
                break;
            }
            // Keep the native window responsive while an asynchronous
            // Media Foundation decoder is between NeedInput events.
            if schedule.picture_waiting() {
                unsafe { pipeline.wait_until_ready_to_present(None, 1) };
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
            continue;
        }
        decoder_blocked_since = None;
        if schedule.picture_waiting() {
            // Until the display takes the picture, or a newer frame arrives.
            if shared.queued.lock().is_ok_and(|queued| queued.is_empty()) {
                unsafe {
                    pipeline.wait_until_ready_to_present(Some(shared.frame_queued.handle()), 4)
                };
            }
            continue;
        }
        let Ok(queued) = shared.queued.lock() else {
            break;
        };
        if shared
            .ready
            .wait_timeout_while(queued, Duration::from_millis(4), |queue| {
                queue.is_empty()
                    && !shared.stopping.load(Ordering::Acquire)
                    && !shared.reset_pending()
            })
            .is_err()
        {
            break;
        }
    }
    shared.running.store(false, Ordering::Release);
    // The window goes with the pipeline.
    shared.window.store(0, Ordering::Release);
    // A reset stored after the last check fails now instead of timing out.
    if let Ok(mut pending) = shared.reset.lock() {
        pending.take();
    }
}

/// Applies a stream reset the caller is waiting for, if there is one, and
/// reports whether it did.
unsafe fn apply_pending_reset(shared: &Shared, pipeline: &mut WorkerPipeline) -> bool {
    let Some(PendingReset {
        format,
        display,
        displays,
        reply,
    }) = shared
        .reset
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
    else {
        return false;
    };
    let result = unsafe { pipeline.reset_stream(format, display, displays) };
    let _ = reply.send(result);
    true
}

/// Shows the latest agent pointer display and cursor shape the session sent.
unsafe fn apply_pending_pointer_state(shared: &Shared, pipeline: &WorkerPipeline) {
    if let Some(display_id) = shared
        .agent_pointer_display
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
    {
        unsafe {
            window::set_agent_pointer_display(pipeline.window(), display_id);
        }
    }
    if let Some(shape) = shared
        .cursor_shape
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
    {
        unsafe { pipeline.set_cursor_shape(shape) };
    }
}

/// Tracks how long the decoder has refused input while frames wait, and
/// returns the failure once that reaches [`DECODER_INPUT_STALL_TIMEOUT`].
fn decoder_input_stall(
    shared: &Shared,
    blocked_since: &mut Option<std::time::Instant>,
) -> Option<String> {
    let frames_are_waiting = shared.queued.lock().is_ok_and(|queued| !queued.is_empty());
    if !frames_are_waiting {
        *blocked_since = None;
        return None;
    }
    let blocked_since = blocked_since.get_or_insert_with(std::time::Instant::now);
    if blocked_since.elapsed() < DECODER_INPUT_STALL_TIMEOUT {
        return None;
    }
    let message = format!(
        "video decoder stopped requesting input for {} seconds while video frames were queued",
        DECODER_INPUT_STALL_TIMEOUT.as_secs()
    );
    tracing::error!(message, "video decoder input watchdog expired");
    Some(message)
}

/// Opts the viewer into per-monitor DPI awareness so Windows does not
/// bitmap-stretch its window, and pointer coordinates are physical pixels.
/// Must run before the first window is created.
pub fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    if let Err(error) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        // Access denied means a manifest or an earlier call already set it.
        tracing::debug!(%error, "could not set per-monitor DPI awareness");
    }
}

/// Attaches to the console of the process that started the viewer, if any,
/// so command-line output reaches a terminal despite the GUI subsystem.
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // Fails when started from Explorer or a browser, which have no console.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Shows a notice after the session, such as where a recording was saved.
pub fn show_notice(title: &str, message: &str) {
    let title = HSTRING::from(title);
    let text = HSTRING::from(message);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        )
    };
}

/// Shows why the viewer stopped. Without a console, this is the only place
/// a fatal error appears apart from the log.
pub fn show_fatal_error(message: &str) {
    let text = HSTRING::from(message);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("MeshRMM Remote"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        )
    };
}

pub fn monotonic_timestamp_us() -> u64 {
    unsafe {
        let mut counter = 0_i64;
        let mut frequency = 0_i64;
        if QueryPerformanceCounter(&mut counter).is_err()
            || QueryPerformanceFrequency(&mut frequency).is_err()
            || counter < 0
            || frequency <= 0
        {
            return 0;
        }
        counter_to_us(counter as u64, frequency as u64)
    }
}

/// Converts QPC ticks to microseconds without overflowing the intermediate
/// product, which a u64 would after ~21 days of uptime at 10 MHz.
fn counter_to_us(counter: u64, frequency: u64) -> u64 {
    u64::try_from(u128::from(counter) * 1_000_000 / u128::from(frequency)).unwrap_or(u64::MAX)
}

pub fn supported_video_profiles(format: VideoFormat) -> Vec<VideoProfile> {
    unsafe { pipeline::supported_video_profiles(format) }
}

/// A control sink that records what the window sends, for tests on a real
/// window without a transport.
#[cfg(test)]
fn test_sink(
    sent: Arc<Mutex<Vec<SessionMessage>>>,
    chat: meshrmm_chat::ChatSession,
) -> ControlSink {
    ControlSink::new(super::ControlSinkParts {
        idle: Default::default(),
        idle_disconnect: Default::default(),
        clear_clipboard: Default::default(),
        display_border: Default::default(),
        files: meshrmm_file_transfer::TransferSession::viewer(|| false),
        chat,
        audio: Default::default(),
        recording: crate::recording::Recorder::with_activity_callback(|_| {}),
        send: Arc::new(move |message| {
            sent.lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(message)
        }),
        set_input_enabled: Arc::new(|_| {}),
        maintenance: Default::default(),
        credentials: Default::default(),
        technician_blocked: Default::default(),
        wallpaper_hidden: Default::default(),
        remote_cursor_hidden: Default::default(),
        session_close_action: Default::default(),
        restarting: Default::default(),
        toolbox: Default::default(),
        quality: Default::default(),
        chroma: Default::default(),
        profiles: Arc::new(vec![
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv420,
            },
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv444,
            },
        ]),
    })
}

/// A stand-in for a presenter's worker thread, for tests: it waits for a
/// stream reset and then stays held, as by a modal loop, until the test
/// answers the reset or drops it.
#[cfg(test)]
pub(crate) struct HeldWorker {
    requested: Option<tokio::sync::oneshot::Receiver<()>>,
    answer: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
    thread: JoinHandle<Option<(VideoFormat, meshrmm_protocol::DisplayId)>>,
}

#[cfg(test)]
impl Presenter {
    /// A presenter whose worker is a [`HeldWorker`].
    pub(crate) fn held_for_test() -> (Self, HeldWorker) {
        let shared = Arc::new(Shared::new(
            test_sink(Default::default(), Default::default()),
            DebugInfo::new("test"),
        ));
        shared.running.store(true, Ordering::Release);
        let (requested_tx, requested) = tokio::sync::oneshot::channel();
        let (answer, answers) = std::sync::mpsc::sync_channel(1);
        let worker_shared = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            let shared = worker_shared;
            // Woken for the reset as `run_worker` is.
            let queued = shared.queued.lock().unwrap();
            let (queued, _) = shared
                .ready
                .wait_timeout_while(queued, Duration::from_secs(10), |_| !shared.reset_pending())
                .unwrap();
            drop(queued);
            if !shared.reset_pending() {
                return None;
            }
            let _ = requested_tx.send(());
            let answer = answers.recv().ok()?;
            // Withdrawn meanwhile when the caller timed out.
            let reset = shared.reset.lock().unwrap().take()?;
            let _ = reset.reply.send(answer);
            Some((reset.format, reset.display.id))
        });
        let presenter = Self {
            shared,
            worker: None,
        };
        let worker = HeldWorker {
            requested: Some(requested),
            answer,
            thread,
        };
        (presenter, worker)
    }
}

#[cfg(test)]
impl HeldWorker {
    /// Waits until the worker sees a reset it was handed.
    pub(crate) async fn reset_requested(&mut self) {
        self.requested
            .take()
            .expect("a held worker takes one reset")
            .await
            .expect("the held worker saw no reset");
    }

    /// Ends the modal loop: the worker answers the reset with `result`.
    pub(crate) fn answer(&self, result: anyhow::Result<()>) {
        self.answer
            .send(result)
            .expect("the held worker is no longer waiting");
    }

    /// The stream and display of the reset the worker answered, if it did.
    pub(crate) fn join(self) -> Option<(VideoFormat, meshrmm_protocol::DisplayId)> {
        drop(self.answer);
        self.thread.join().expect("the held worker panicked")
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{DesktopSession, DisplayId, PixelFormat, VideoStreamId};

    use super::*;

    const FORMAT: VideoFormat = VideoFormat {
        width: 1920,
        height: 1080,
        frames_per_second: 60,
        codec: Codec::H264,
        pixel_format: PixelFormat::Nv12,
        bitrate_bits_per_second: 12_000_000,
    };

    fn display(id: u32) -> Display {
        Display {
            session: DesktopSession::Console,
            id: DisplayId(id),
            name: format!("Display {id}"),
            x: 0,
            y: 0,
            width: FORMAT.width,
            height: FORMAT.height,
            primary: id == 1,
        }
    }

    fn frame(keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(2),
            frame_id: 1,
            capture_timestamp_us: 0,
            encode_complete_timestamp_us: 0,
            send_timestamp_us: 0,
            keyframe,
            data: vec![0, 0, 0, 1],
        }
    }

    /// A presenter whose worker is replaced by the test.
    fn presenter(running: bool) -> Presenter {
        let shared = Shared::new(
            test_sink(Default::default(), Default::default()),
            DebugInfo::new("test"),
        );
        shared.running.store(running, Ordering::Release);
        Presenter {
            shared: Arc::new(shared),
            worker: None,
        }
    }

    #[tokio::test]
    async fn a_stopped_worker_refuses_a_stream_reset() {
        let presenter = presenter(false);
        assert!(
            presenter
                .reset_stream(FORMAT, display(1), vec![display(1)])
                .await
                .is_err()
        );
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
        assert!(!presenter.shared.reset_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reset_the_worker_does_not_take_times_out_and_is_withdrawn() {
        // A modal loop on the worker thread holds up its loop.
        let presenter = presenter(true);
        let error = presenter
            .reset_stream(FORMAT, display(1), vec![display(1)])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the Windows presentation thread did not reset the stream within 5000 ms"
        );
        // The worker must not apply it once the caller has fallen back.
        assert!(!presenter.shared.reset_pending());
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
        assert!(presenter.publish(frame(true), 0));
    }

    #[tokio::test]
    async fn a_held_worker_holds_up_neither_frames_nor_the_runtime() {
        // A current-thread runtime: the reset's wait must leave its one
        // thread to the other tasks.
        let (presenter, mut worker) = Presenter::held_for_test();
        let reset = tokio::spawn(presenter.reset_stream(FORMAT, display(2), vec![display(2)]));
        worker.reset_requested().await;
        assert!(!presenter.publish(frame(true), 0));
        presenter.set_cursor_shape(CursorShape::Pointer);
        presenter.set_agent_pointer_display(Some(DisplayId(2)));
        presenter.set_reconnect_status(None);
        assert_eq!(presenter.poll_ended(), None);
        assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7);
        assert!(!reset.is_finished());
        // The modal loop ends.
        worker.answer(Ok(()));
        reset.await.unwrap().unwrap();
        assert_eq!(worker.join(), Some((FORMAT, DisplayId(2))));
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn the_worker_applies_a_reset_while_frames_are_dropped() {
        let (presenter, mut worker) = Presenter::held_for_test();
        assert!(presenter.publish(frame(true), 0));
        assert_eq!(presenter.first_presented_at(), None);
        let replacement = VideoFormat {
            width: 2560,
            height: 1440,
            ..FORMAT
        };
        let reset = presenter.reset_stream(replacement, display(2), vec![display(1), display(2)]);
        worker.reset_requested().await;
        // The queued frame belonged to the old stream.
        assert!(presenter.shared.queued.lock().unwrap().is_empty());
        // Frames that arrive during the reset are dropped, even keyframes:
        // the caller requests one once it returns.
        assert!(!presenter.publish(frame(true), 0));
        assert!(!presenter.publish(frame(false), 0));
        worker.answer(Ok(()));
        reset.await.unwrap();
        assert_eq!(worker.join(), Some((replacement, DisplayId(2))));
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
        // Only a keyframe of the new stream ends the recovery.
        assert!(!presenter.publish(frame(false), 0));
        assert!(presenter.publish(frame(true), 0));
        assert!(presenter.publish(frame(false), 0));
        assert_eq!(presenter.shared.replaced_frames.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn a_failed_reset_is_returned_to_the_caller() {
        let (presenter, mut worker) = Presenter::held_for_test();
        let reset = presenter.reset_stream(FORMAT, display(1), vec![display(1)]);
        worker.reset_requested().await;
        worker.answer(Err(anyhow::anyhow!("no hardware decoder")));
        let error = reset.await.unwrap_err();
        assert_eq!(error.to_string(), "no hardware decoder");
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
    }

    /// A worker thread that stays in a modal loop until `release` is sent
    /// or dropped.
    fn held_thread(shared: &Arc<Shared>) -> (JoinHandle<()>, std::sync::mpsc::SyncSender<()>) {
        let (release, gate) = std::sync::mpsc::sync_channel(1);
        let shared = Arc::clone(shared);
        let thread = std::thread::spawn(move || {
            let _ended = WorkerEnded(Arc::clone(&shared));
            let _ = gate.recv();
            // Its loop checks this once the modal loop returns.
            assert!(shared.stopping.load(Ordering::Acquire));
        });
        (thread, release)
    }

    fn wait_until_ended(shared: &Shared) -> bool {
        let ended = shared.worker_ended.lock().unwrap();
        let (ended, _) = shared
            .worker_ended_changed
            .wait_timeout_while(ended, Duration::from_secs(10), |ended| !*ended)
            .unwrap();
        *ended
    }

    #[test]
    fn stopping_a_held_worker_does_not_wait_for_it() {
        let mut presenter = presenter(true);
        let (thread, release) = held_thread(&presenter.shared);
        presenter.worker = Some(thread);
        let shared = Arc::clone(&presenter.shared);
        let (stopped_tx, stopped) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            presenter.stop();
            let _ = stopped_tx.send(());
        });
        stopped
            .recv_timeout(Duration::from_secs(10))
            .expect("stop waited for the held worker");
        assert!(!*shared.worker_ended.lock().unwrap());
        // The modal loop ends, and so does the thread.
        release.send(()).unwrap();
        assert!(wait_until_ended(&shared));
    }

    #[test]
    fn retiring_a_worker_joins_one_that_ends_in_time() {
        let shared = Arc::new(Shared::new(
            test_sink(Default::default(), Default::default()),
            DebugInfo::new("test"),
        ));
        shared.stopping.store(true, Ordering::Release);
        let (thread, release) = held_thread(&shared);
        release.send(()).unwrap();
        assert!(retire_worker(&shared, thread, Duration::from_secs(10)));
    }

    #[test]
    fn retiring_a_worker_gives_up_on_one_held_past_the_limit() {
        let shared = Arc::new(Shared::new(
            test_sink(Default::default(), Default::default()),
            DebugInfo::new("test"),
        ));
        shared.stopping.store(true, Ordering::Release);
        let (thread, release) = held_thread(&shared);
        assert!(!retire_worker(&shared, thread, Duration::from_millis(50)));
        // Left to end by itself once its modal loop returns.
        assert!(!*shared.worker_ended.lock().unwrap());
        drop(release);
        assert!(wait_until_ended(&shared));
    }

    #[test]
    fn converts_performance_counter_past_u64_product_range() {
        const FREQUENCY: u64 = 10_000_000;
        // 30 days of uptime: counter * 1_000_000 no longer fits in a u64.
        let counter = 30 * 24 * 60 * 60 * FREQUENCY + 12_345;
        assert!(counter.checked_mul(1_000_000).is_none());
        assert_eq!(counter_to_us(counter, FREQUENCY), 2_592_000_001_234);
        assert_eq!(
            counter_to_us(counter + FREQUENCY, FREQUENCY) - counter_to_us(counter, FREQUENCY),
            1_000_000
        );
        assert_eq!(counter_to_us(u64::MAX, FREQUENCY), u64::MAX / 10);
        assert_eq!(counter_to_us(u64::MAX, 1), u64::MAX);
        assert_eq!(counter_to_us(3, 3_000_000), 1);
    }
}
