//! What the transport sees of the session window: it publishes encoded
//! frames, which a decoder thread turns into pictures, and sends the window
//! its changes through the event loop.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_protocol::{
    Codec, CursorShape, Display, DisplayId, EncodedFrame, PixelFormat, SessionMessage, VideoFormat,
    VideoStreamId,
};

use super::app::{self, SessionCommand, SessionRequest, UiEvent};
use super::decoder::{self, Picture};
use crate::debug::DebugInfo;
use crate::platform::ControlSink;
use crate::reconnect::ReconnectStatus;

static NEXT_PRESENTER_ID: AtomicU64 = AtomicU64::new(1);

/// Frames waiting for the decoder. More means it has fallen behind, and the
/// stream starts over from a keyframe.
const MAX_QUEUED_FRAMES: usize = 15;
/// How long the event loop's thread has to open or reset the window.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct QueuedFrame {
    pub(super) frame: EncodedFrame,
    pub(super) received_at_us: u64,
}

/// The decoder's input, which the transport fills.
#[derive(Default)]
pub(super) struct Input {
    pub(super) frames: VecDeque<QueuedFrame>,
    /// Changes when the stream is replaced, so the decoder starts over.
    pub(super) stream: u64,
    pub(super) stopped: bool,
}

/// What a presenter, its decoder thread and its window share.
pub(super) struct Shared {
    pub(super) id: u64,
    pub(super) control: ControlSink,
    pub(super) debug: DebugInfo,
    pub(super) input: Mutex<Input>,
    pub(super) wake: Condvar,
    /// The newest picture the window has not drawn yet.
    pub(super) picture: Mutex<Option<Picture>>,
    /// Pictures the window has finished with, whose buffers are used again.
    pub(super) spare: Mutex<Vec<Picture>>,
    /// Whether the window has been told of a picture it has not taken.
    pub(super) picture_posted: AtomicBool,
    pub(super) failure: Mutex<Option<String>>,
    /// The technician closed the window.
    pub(super) closed: AtomicBool,
    pub(super) first_presented: OnceLock<Instant>,
    /// Frames dropped before decoding: a full queue, or waiting for a keyframe.
    pub(super) dropped: AtomicU64,
    /// Pictures decoded but replaced by a newer one before they were drawn.
    pub(super) replaced: AtomicU64,
    pub(super) decoded: AtomicU64,
    /// The decoder's recent rate, as the bits of an `f64`; zero until known.
    pub(super) decode_fps: AtomicU64,
    /// Frames are dropped until the keyframe that was asked for arrives.
    recovering: AtomicBool,
    /// The window is moving to a replacement stream.
    resetting: AtomicBool,
}

impl Shared {
    fn input(&self) -> std::sync::MutexGuard<'_, Input> {
        self.input.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Asks for a keyframe, once, and drops frames until it arrives.
    pub(super) fn begin_recovery(&self, stream_id: VideoStreamId) {
        if !self.recovering.swap(true, Ordering::AcqRel) {
            self.control
                .send(SessionMessage::RequestKeyframe { stream_id });
            tracing::warn!(
                stream_id = stream_id.0,
                "the viewer dropped a video reference; requesting a recovery keyframe"
            );
        }
    }

    pub(super) fn fail(&self, failure: String) {
        let mut current = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.is_none() {
            *current = Some(failure);
        }
    }

    fn count_dropped(&self, frames: usize) {
        self.dropped.fetch_add(frames as u64, Ordering::Relaxed);
    }

    /// Hands the window's finished picture back for its buffers.
    pub(super) fn recycle(&self, picture: Picture) {
        let mut spare = self.spare.lock().unwrap_or_else(|error| error.into_inner());
        if spare.len() < 3 {
            spare.push(picture);
        }
    }

    fn send(&self, command: SessionCommand) {
        app::post(UiEvent::Session {
            id: self.id,
            command,
        });
    }
}

/// Lets frames through again when dropped, as a reset ends, answered or not.
struct Resetting(Arc<Shared>);

impl Drop for Resetting {
    fn drop(&mut self) {
        self.0.resetting.store(false, Ordering::Release);
    }
}

pub struct Presenter {
    shared: Arc<Shared>,
    stopped: bool,
}

/// Whether the viewer shows a stream of `format`: software H.264 in 4:2:0.
pub(super) fn format_supported(format: VideoFormat) -> bool {
    format.codec == Codec::H264 && format.pixel_format == PixelFormat::Nv12
}

impl Presenter {
    pub fn start(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        if !format_supported(format) {
            bail!(
                "the Linux viewer decodes H.264 4:2:0 only, not {:?} {:?}",
                format.codec,
                format.pixel_format
            );
        }
        let id = NEXT_PRESENTER_ID.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            presenter_id = id,
            display_id = active_display.id.0,
            width = format.width,
            height = format.height,
            fps = format.frames_per_second,
            bitrate_bits_per_second = format.bitrate_bits_per_second,
            codec = ?format.codec,
            "starting the Linux video presenter"
        );
        let shared = Arc::new(Shared {
            id,
            control,
            debug,
            input: Mutex::new(Input::default()),
            wake: Condvar::new(),
            picture: Mutex::new(None),
            spare: Mutex::new(Vec::new()),
            picture_posted: AtomicBool::new(false),
            failure: Mutex::new(None),
            closed: AtomicBool::new(false),
            first_presented: OnceLock::new(),
            dropped: AtomicU64::new(0),
            replaced: AtomicU64::new(0),
            decoded: AtomicU64::new(0),
            decode_fps: AtomicU64::new(0),
            recovering: AtomicBool::new(false),
            resetting: AtomicBool::new(false),
        });
        let (reply, opened) = std::sync::mpsc::sync_channel(1);
        if !app::post(UiEvent::OpenSession(
            Box::new(SessionRequest {
                shared: Arc::clone(&shared),
                format,
                display: active_display,
                displays,
            }),
            reply,
        )) {
            bail!("the viewer's window thread is not running");
        }
        opened
            .recv_timeout(WINDOW_TIMEOUT)
            .context("the viewer's window thread did not open the session window")??;
        decoder::spawn(Arc::clone(&shared))?;
        Ok(Self {
            shared,
            stopped: false,
        })
    }

    pub fn publish(&self, frame: EncodedFrame, received_at_us: u64) -> bool {
        let shared = &self.shared;
        let stream_id = frame.stream_id;
        let mut input = shared.input();
        if shared.resetting.load(Ordering::Acquire)
            || (shared.recovering.load(Ordering::Acquire) && !frame.keyframe)
        {
            shared.count_dropped(1);
            return false;
        }
        if input.frames.len() >= MAX_QUEUED_FRAMES {
            shared.count_dropped(input.frames.len());
            input.frames.clear();
            if !frame.keyframe {
                shared.count_dropped(1);
                drop(input);
                shared.begin_recovery(stream_id);
                return false;
            }
        } else if frame.keyframe && shared.recovering.load(Ordering::Acquire) {
            // The decoder starts over from this keyframe.
            shared.count_dropped(input.frames.len());
            input.frames.clear();
        }
        input.frames.push_back(QueuedFrame {
            frame,
            received_at_us,
        });
        shared.recovering.store(false, Ordering::Release);
        drop(input);
        shared.wake.notify_one();
        true
    }

    /// Whether [`Self::reset_stream`] can take `next` in the current window.
    pub fn can_reset_in_place(current: VideoFormat, next: VideoFormat) -> bool {
        // The decoder reads the size from the replacement keyframe, and the
        // window fits whatever picture it is given.
        current.codec == next.codec
            && current.pixel_format == next.pixel_format
            && format_supported(next)
    }

    /// Moves the window to a replacement stream. The event loop's thread
    /// applies it, so a busy one holds the reset up and it may time out.
    ///
    /// The reset is handed over before this returns; the future only waits
    /// for the answer and borrows nothing, so the caller can release the
    /// presenter first. Frames published meanwhile are dropped.
    pub fn reset_stream(
        &self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        let shared = &self.shared;
        shared.resetting.store(true, Ordering::Release);
        shared.recovering.store(true, Ordering::Release);
        {
            let mut input = shared.input();
            shared.count_dropped(input.frames.len());
            input.frames.clear();
            input.stream += 1;
        }
        let resetting = Resetting(Arc::clone(shared));
        let (reply, reset) = tokio::sync::oneshot::channel();
        shared.send(SessionCommand::Reset {
            format,
            display,
            displays,
            reply,
        });
        async move {
            let _resetting = resetting;
            tokio::time::timeout(WINDOW_TIMEOUT, reset)
                .await
                .context("the viewer's window thread did not reset the video stream")?
                .context("the viewer window is no longer available")?
        }
    }

    pub fn set_agent_pointer_display(&self, display_id: Option<DisplayId>) {
        self.shared
            .send(SessionCommand::AgentPointerDisplay(display_id));
    }

    /// Replaces the displays the window offers, leaving the stream and its
    /// decoder alone.
    pub fn set_displays(&self, displays: Vec<Display>) {
        self.shared.send(SessionCommand::Displays(displays));
    }

    /// Agent status can change on a static desktop that sends no frames.
    pub fn refresh_controls(&self) {
        self.shared.send(SessionCommand::Refresh);
    }

    /// Shows why the window is waiting for the connection to be restored,
    /// or hides that (`None`).
    pub fn set_reconnect_status(&self, status: Option<ReconnectStatus>) {
        self.shared.send(SessionCommand::ReconnectStatus(status));
    }

    pub fn set_cursor_shape(&self, shape: CursorShape) {
        self.shared.send(SessionCommand::CursorShape(shape));
    }

    pub fn first_presented_at(&self) -> Option<Instant> {
        self.shared.first_presented.get().copied()
    }

    /// `Some(Ok(()))` once the technician closed the window, or the failure
    /// that ended the video.
    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Some(Ok(()));
        }
        self.shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take())
            .map(Err)
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.shared.input().stopped = true;
        self.shared.wake.notify_all();
        self.shared.send(SessionCommand::Close);
        tracing::info!(
            presenter_id = self.shared.id,
            frames_dropped = self.shared.dropped.load(Ordering::Relaxed),
            pictures_replaced = self.shared.replaced.load(Ordering::Relaxed),
            frames_decoded = self.shared.decoded.load(Ordering::Relaxed),
            "the Linux video presenter stopped"
        );
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Refreshes the open session window's controls, from any thread. Unlike
/// [`Presenter::refresh_controls`] it needs no presenter, so state that
/// outlives reconnects can show its changes without a frame arriving.
pub fn refresh_open_controls() {
    app::post(UiEvent::RefreshSession);
}
