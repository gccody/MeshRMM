//! Capture on wlroots compositors, such as sway and Hyprland, through the
//! wlr-screencopy protocol: the compositor copies the output into a shared
//! memory buffer this process provides. After the first frame, each copy
//! waits for the screen to change.
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use wayland_client::Proxy;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};

use super::wayland::{Client, FrameStatus};
use super::{Backend, CaptureSettings, FrameSink, Stream};
use crate::remote::linux::display::Output;
use crate::remote::linux::encoder::{PixelLayout, RawFrame};

pub(crate) const SCREENCOPY_GLOBAL: &str = "zwlr_screencopy_manager_v1";
const START_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(200);
/// Failed copies in a row that end the capture.
const MAX_FAILURES: u32 = 10;

pub(crate) struct Wlroots;

impl Wlroots {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Backend for Wlroots {
    fn name(&self) -> &'static str {
        "wlroots (screencopy)"
    }

    fn outputs(&mut self) -> anyhow::Result<Vec<Output>> {
        let outputs = Client::connect()?.outputs();
        anyhow::ensure!(!outputs.is_empty(), "the compositor reported no displays");
        Ok(outputs)
    }

    fn start(
        &mut self,
        output: &Output,
        settings: CaptureSettings,
        sink: FrameSink,
    ) -> anyhow::Result<Box<dyn Stream>> {
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            capture_cursor: AtomicBool::new(settings.capture_cursor),
            ended: Mutex::new(None),
        });
        let (started_sender, started) = mpsc::channel();
        let (worker, name) = (Arc::clone(&shared), output.name.clone());
        let interval = Duration::from_secs(1) / settings.frames_per_second.max(1);
        let thread = std::thread::Builder::new()
            .name("meshrmm-screencopy".into())
            .spawn(move || {
                if let Err(error) = capture(&worker, &name, interval, &sink, &started_sender) {
                    let reason = format!("{error:#}");
                    if started_sender.send(Err(reason.clone())).is_err() {
                        *worker.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                    }
                }
            })
            .context("could not start screencopy capture")?;
        let stream = WlrootsStream {
            shared,
            thread: Some(thread),
        };
        match started.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(Box::new(stream)),
            Ok(Err(error)) => bail!(error),
            Err(_) => bail!("the compositor did not copy the screen"),
        }
    }
}

struct Shared {
    stopped: AtomicBool,
    capture_cursor: AtomicBool,
    ended: Mutex<Option<String>>,
}

/// A shared-memory buffer the compositor copies frames into.
struct Buffer {
    _file: OwnedFd,
    pointer: *mut u8,
    length: usize,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    /// Format, width, height and stride.
    shape: (u32, u32, u32, u32),
}

impl Buffer {
    fn new(
        client: &Client,
        shm: &wl_shm::WlShm,
        shape: (u32, u32, u32, u32),
    ) -> anyhow::Result<Self> {
        let (format, width, height, stride) = shape;
        let length = stride as usize * height as usize;
        // SAFETY: memfd_create takes a NUL-terminated name and flags.
        let fd = unsafe { libc::memfd_create(c"meshrmm-screencopy".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        // SAFETY: memfd_create returned a new descriptor this owns.
        let file = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the descriptor is a memfd.
        if unsafe { libc::ftruncate(file.as_raw_fd(), length as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error()).context("ftruncate");
        }
        // SAFETY: mapping the whole new file shared and readable.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap");
        }
        let handle = client.handle();
        let pool = shm.create_pool(file.as_fd(), length as i32, &handle, ());
        let shm_format = wl_shm::Format::try_from(format)
            .map_err(|_| anyhow::anyhow!("unknown shared memory format {format:#x}"))?;
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            shm_format,
            &handle,
            (),
        );
        Ok(Self {
            _file: file,
            pointer: pointer.cast(),
            length,
            pool,
            buffer,
            shape,
        })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `length` bytes and lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.pointer, self.length) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: the mapping came from mmap with this length.
        unsafe { libc::munmap(self.pointer.cast(), self.length) };
    }
}

/// The byte order of a `wl_shm` format, for the 32-bit ones the encoder takes.
fn layout(format: u32) -> Option<PixelLayout> {
    match wl_shm::Format::try_from(format).ok()? {
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => Some(PixelLayout::Bgrx),
        wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => Some(PixelLayout::Rgbx),
        _ => None,
    }
}

/// Copies frames until the capture is stopped.
fn capture(
    shared: &Shared,
    name: &str,
    interval: Duration,
    sink: &FrameSink,
    started: &mpsc::Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    let mut client = Client::connect()?;
    let output = client
        .output_proxy(name)
        .with_context(|| format!("the compositor has no display named {name}"))?;
    let manager = client
        .state
        .screencopy
        .clone()
        .context("the compositor does not offer screencopy")?;
    let shm = client
        .state
        .shm
        .clone()
        .context("the compositor does not offer shared memory")?;
    let version = manager.version();
    let mut buffer: Option<Buffer> = None;
    let mut flipped = Vec::new();
    let mut failures = 0;
    let mut first = true;
    let mut next = Instant::now();
    while !shared.stopped.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next = Instant::now() + interval;
        client.state.frame = FrameStatus::default();
        let handle = client.handle();
        let overlay = i32::from(shared.capture_cursor.load(Ordering::SeqCst));
        let frame = manager.capture_output(overlay, &output, &handle, ());
        // The compositor describes the buffer it wants, finishing with
        // buffer_done from version 3.
        let deadline = Instant::now() + START_TIMEOUT;
        while !(client.state.frame.failed
            || client.state.frame.buffer.is_some()
                && (version < 3 || client.state.frame.buffer_done))
        {
            client.dispatch(POLL)?;
            if Instant::now() >= deadline {
                bail!("the compositor did not describe the screen's buffer");
            }
        }
        let Some(shape) = client
            .state
            .frame
            .buffer
            .filter(|_| !client.state.frame.failed)
        else {
            frame.destroy();
            failures += 1;
            if failures > MAX_FAILURES {
                bail!("the compositor could not copy the screen");
            }
            continue;
        };
        let pixel_layout = layout(shape.0).with_context(|| {
            format!(
                "the compositor's pixel format {:#x} is not supported",
                shape.0
            )
        })?;
        if buffer.as_ref().is_none_or(|buffer| buffer.shape != shape) {
            buffer = Some(Buffer::new(&client, &shm, shape)?);
        }
        let target = buffer.as_ref().expect("the buffer was just created");
        if version >= 2 && !first {
            frame.copy_with_damage(&target.buffer);
        } else {
            frame.copy(&target.buffer);
        }
        while !(client.state.frame.ready || client.state.frame.failed) {
            if shared.stopped.load(Ordering::SeqCst) {
                frame.destroy();
                return Ok(());
            }
            client.dispatch(POLL)?;
        }
        let ready = client.state.frame.ready;
        frame.destroy();
        if !ready {
            failures += 1;
            if failures > MAX_FAILURES {
                bail!("the compositor could not copy the screen");
            }
            continue;
        }
        failures = 0;
        if first {
            first = false;
            let _ = started.send(Ok(()));
        }
        let (_, width, height, stride) = shape;
        let bytes = target.bytes();
        let data = if client.state.frame.y_invert {
            flipped.clear();
            for row in bytes.chunks_exact(stride as usize).rev() {
                flipped.extend_from_slice(row);
            }
            &flipped[..]
        } else {
            bytes
        };
        sink(&RawFrame {
            data,
            width,
            height,
            stride: stride as usize,
            layout: pixel_layout,
            timestamp_us: crate::remote::linux::monotonic_timestamp_us(),
        });
    }
    Ok(())
}

struct WlrootsStream {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stream for WlrootsStream {
    fn poll_ended(&mut self) -> Option<anyhow::Error> {
        self.shared
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(|reason| anyhow::anyhow!(reason))
    }

    fn set_cursor_capture(&mut self, enabled: bool) -> bool {
        self.shared.capture_cursor.store(enabled, Ordering::SeqCst);
        true
    }
}

impl Drop for WlrootsStream {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_memory_formats_map_to_byte_orders() {
        assert_eq!(layout(0), Some(PixelLayout::Bgrx));
        assert_eq!(layout(1), Some(PixelLayout::Bgrx));
        assert_eq!(layout(0x3432_4258), Some(PixelLayout::Rgbx));
        assert_eq!(layout(0x3033_5258), None);
    }
}
