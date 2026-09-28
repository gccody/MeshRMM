use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{
    ChromaMode, Codec, CursorShape, Display, EncodedFrame, PointerButton, QualityPreset,
    RemoteInput, SessionCloseAction, SessionMessage, VideoFormat, VideoProfile,
};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, DEFAULT_GUI_FONT, GetStockObject, HBRUSH, ScreenToClient, SetBkColor, SetTextColor,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, SetFocus,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Interface, PCWSTR, w};

use super::ControlSink;
use crate::debug::DebugInfo;
use crate::reconnect::{ReconnectStatus, ReconnectText};

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

pub use launch_window::{close_launch_status, show_launch_status};
use pipeline::WorkerPipeline;
use renderer::D3d11Renderer;
use window::{create_window, pump_window_messages, set_window_cursor};

const MAX_DECODER_PENDING_FRAMES: usize = 16;
// Moonlight's depacketizer permits 15 queued decode units. This is large
// enough for short delivery/decoder bursts without treating normal jitter as
// reference loss; the worker still presents only the newest decoded surface.
const MAX_PRESENTER_QUEUE_FRAMES: usize = 15;
const DECODER_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the worker redraws the reconnect overlay's elapsed time and
/// countdown. More often than once a second, so the counts do not skip one.
const RECONNECT_REFRESH: Duration = Duration::from_millis(250);
/// How long a stream reset waits for the worker thread before the caller
/// replaces the presenter instead.
const STREAM_RESET_TIMEOUT: Duration = Duration::from_secs(5);
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
    reply: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
}

struct Shared {
    queued: Mutex<VecDeque<QueuedFrame>>,
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
    control: ControlSink,
    debug: DebugInfo,
}

impl Shared {
    fn new(control: ControlSink, debug: DebugInfo) -> Self {
        Self {
            queued: Mutex::new(VecDeque::with_capacity(MAX_PRESENTER_QUEUE_FRAMES)),
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
            control,
            debug,
        }
    }

    fn reset_pending(&self) -> bool {
        self.reset.lock().is_ok_and(|reset| reset.is_some())
    }

    fn set_reconnect_status(&self, status: Option<ReconnectStatus>) {
        if let Ok(mut current) = self.reconnect_status.lock() {
            *current = status;
        }
        self.reconnect_changed.store(true, Ordering::Release);
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
            drop(queued);
            if first_loss {
                self.shared
                    .control
                    .send(SessionMessage::RequestKeyframe { stream_id });
                tracing::warn!(
                    stream_id = stream_id.0,
                    "decoder could not keep up; requesting a recovery keyframe"
                );
            }
            return false;
        }
        if frame.keyframe
            && (self.shared.recovering.load(Ordering::Acquire)
                || queued.len() >= MAX_PRESENTER_QUEUE_FRAMES)
        {
            self.shared
                .replaced_frames
                .fetch_add(queued.len() as u64, Ordering::Relaxed);
            queued.clear();
        }
        queued.push_back(QueuedFrame {
            frame,
            received_at_us,
        });
        self.shared.recovering.store(false, Ordering::Release);
        self.shared.ready.notify_one();
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
    /// applies the reset between window messages. While it shows a modal
    /// message box it cannot, and the reset times out.
    pub fn reset_stream(
        &self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> anyhow::Result<()> {
        self.reset_stream_within(format, display, displays, STREAM_RESET_TIMEOUT)
    }

    fn reset_stream_within(
        &self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
        timeout: Duration,
    ) -> anyhow::Result<()> {
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
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        let Ok(mut pending) = self.shared.reset.lock() else {
            self.shared.resetting.store(false, Ordering::Release);
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
        let result = match response.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The caller falls back to a new presenter; the worker must
                // not apply this reset afterwards.
                if let Ok(mut pending) = self.shared.reset.lock() {
                    pending.take();
                }
                Err(anyhow::anyhow!(
                    "the Windows presentation thread did not reset the stream within {} ms",
                    timeout.as_millis()
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
                "the Windows presentation thread stopped before resetting the stream"
            )),
        };
        self.shared.resetting.store(false, Ordering::Release);
        result
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

    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("Windows decode/presentation worker panicked");
        }
        tracing::info!(
            latest_frames_dropped = self.shared.replaced_frames.load(Ordering::Relaxed),
            "video presenter stopped"
        );
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
    while !shared.stopping.load(Ordering::Acquire) {
        if unsafe { pump_window_messages(pipeline.window()) } {
            break;
        }
        if let Some(PendingReset {
            format,
            display,
            displays,
            reply,
        }) = shared
            .reset
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
        {
            let result = unsafe { pipeline.reset_stream(format, display, displays) };
            // The new decoder has not been offered any input yet.
            decoder_blocked_since = None;
            let _ = reply.send(result);
        }
        unsafe { reconnect_overlay.refresh(pipeline.window(), &shared) };
        if let Some(layout) = unsafe { window::take_resize(pipeline.window()) }
            && let Err(error) = unsafe { pipeline.resize(&layout) }
        {
            tracing::error!(error = %error, "viewer swap chain resize failed");
            if let Ok(mut failure) = shared.failure.lock() {
                *failure = Some(error.to_string());
            }
            break;
        }
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
        if let Err(error) = unsafe { pipeline.poll(shared.replaced_frames.load(Ordering::Relaxed)) }
        {
            tracing::error!(error = %error, "hardware decoder polling failed");
            if let Ok(mut failure) = shared.failure.lock() {
                *failure = Some(error.to_string());
            }
            break;
        }
        if !pipeline.wants_input() {
            let frames_are_waiting = shared.queued.lock().is_ok_and(|queued| !queued.is_empty());
            if frames_are_waiting {
                let blocked_since =
                    decoder_blocked_since.get_or_insert_with(std::time::Instant::now);
                if blocked_since.elapsed() >= DECODER_INPUT_STALL_TIMEOUT {
                    let message = format!(
                        "hardware decoder stopped requesting input for {} seconds while video frames were queued",
                        DECODER_INPUT_STALL_TIMEOUT.as_secs()
                    );
                    tracing::error!(message, "hardware decoder input watchdog expired");
                    if let Ok(mut failure) = shared.failure.lock() {
                        *failure = Some(message);
                    }
                    break;
                }
            } else {
                decoder_blocked_since = None;
            }
            // Keep the native window responsive while an asynchronous
            // Media Foundation decoder is between NeedInput events.
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        decoder_blocked_since = None;
        let queued = {
            let Ok(queued) = shared.queued.lock() else {
                break;
            };
            let Ok((mut queued, _)) =
                shared
                    .ready
                    .wait_timeout_while(queued, Duration::from_millis(4), |queue| {
                        queue.is_empty()
                            && !shared.stopping.load(Ordering::Acquire)
                            && !shared.reset_pending()
                    })
            else {
                break;
            };
            queued.pop_front()
        };
        let Some(queued) = queued else {
            continue;
        };
        let frame_id = queued.frame.frame_id;
        match unsafe { pipeline.process(queued, shared.replaced_frames.load(Ordering::Relaxed)) } {
            Ok(None) => {}
            Ok(Some(queued)) => {
                tracing::warn!(
                    frame_id,
                    "hardware decoder readiness changed before frame submission"
                );
                if let Ok(mut pending) = shared.queued.lock() {
                    pending.push_front(queued);
                }
            }
            Err(error) => {
                tracing::error!(error = %error, frame_id, "hardware decode/presentation failed");
                if let Ok(mut failure) = shared.failure.lock() {
                    *failure = Some(error.to_string());
                }
                break;
            }
        }
    }
    shared.running.store(false, Ordering::Release);
    // A reset stored after the last check fails now instead of timing out.
    if let Ok(mut pending) = shared.reset.lock() {
        pending.take();
    }
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

    #[test]
    fn a_stopped_worker_refuses_a_stream_reset() {
        let presenter = presenter(false);
        assert!(
            presenter
                .reset_stream(FORMAT, display(1), vec![display(1)])
                .is_err()
        );
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
        assert!(!presenter.shared.reset_pending());
    }

    #[test]
    fn a_reset_the_worker_does_not_take_times_out_and_is_withdrawn() {
        // A modal message box on the worker thread blocks its loop.
        let presenter = presenter(true);
        let error = presenter
            .reset_stream_within(
                FORMAT,
                display(1),
                vec![display(1)],
                Duration::from_millis(50),
            )
            .unwrap_err();
        assert!(error.to_string().contains("did not reset"), "{error}");
        // The worker must not apply it once the caller has fallen back.
        assert!(!presenter.shared.reset_pending());
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
    }

    #[test]
    fn the_worker_applies_a_reset_while_frames_are_dropped() {
        let presenter = presenter(true);
        assert!(presenter.publish(frame(true), 0));
        assert_eq!(presenter.first_presented_at(), None);
        let replacement = VideoFormat {
            width: 2560,
            height: 1440,
            ..FORMAT
        };
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                // Waits until the reset is handed over, as `run_worker` is
                // woken for it, then takes it.
                let shared = &presenter.shared;
                let queued = shared.queued.lock().unwrap();
                let (queued, _) = shared
                    .ready
                    .wait_timeout_while(queued, Duration::from_secs(5), |_| !shared.reset_pending())
                    .unwrap();
                // The queued frame belonged to the old stream.
                assert!(queued.is_empty());
                drop(queued);
                let reset = shared.reset.lock().unwrap().take().unwrap();
                // Frames that arrive during the reset are dropped, even
                // keyframes: the caller requests one once it returns.
                assert!(!presenter.publish(frame(true), 0));
                assert!(!presenter.publish(frame(false), 0));
                reset.reply.send(Ok(())).unwrap();
                (reset.format, reset.display.id)
            });
            presenter
                .reset_stream_within(
                    replacement,
                    display(2),
                    vec![display(1), display(2)],
                    Duration::from_secs(5),
                )
                .unwrap();
            assert_eq!(worker.join().unwrap(), (replacement, DisplayId(2)));
        });
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
        // Only a keyframe of the new stream ends the recovery.
        assert!(!presenter.publish(frame(false), 0));
        assert!(presenter.publish(frame(true), 0));
        assert!(presenter.publish(frame(false), 0));
        assert_eq!(presenter.shared.replaced_frames.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn a_failed_reset_is_returned_to_the_caller() {
        let presenter = presenter(true);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let shared = &presenter.shared;
                let queued = shared.queued.lock().unwrap();
                let (queued, _) = shared
                    .ready
                    .wait_timeout_while(queued, Duration::from_secs(5), |_| !shared.reset_pending())
                    .unwrap();
                drop(queued);
                let reset = shared.reset.lock().unwrap().take().unwrap();
                reset
                    .reply
                    .send(Err(anyhow::anyhow!("no hardware decoder")))
                    .unwrap();
            });
            let error = presenter
                .reset_stream_within(FORMAT, display(1), vec![display(1)], Duration::from_secs(5))
                .unwrap_err();
            assert_eq!(error.to_string(), "no hardware decoder");
        });
        assert!(!presenter.shared.resetting.load(Ordering::Acquire));
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
