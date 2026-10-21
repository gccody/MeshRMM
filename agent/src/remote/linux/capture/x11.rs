//! X11 capture: the root window's part under one monitor, read into shared
//! memory with MIT-SHM, polled at the frame rate. A frame identical to the
//! previous one is not sent. The X server leaves the pointer out of the
//! image, so it is drawn in from XFixes, which also names its shape.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_protocol::CursorShape;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use super::{Backend, CaptureSettings, FrameSink, Stream};
use crate::remote::linux::display::Output;
use crate::remote::linux::encoder::{PixelLayout, RawFrame};

pub(crate) struct X11 {
    connection: RustConnection,
    root: u32,
}

impl X11 {
    /// Connects to `DISPLAY` with the cookie in `XAUTHORITY`.
    pub(crate) fn connect() -> anyhow::Result<Self> {
        let (connection, screen) =
            RustConnection::connect(None).context("could not connect to the X server")?;
        let root = connection.setup().roots[screen].root;
        Ok(Self { connection, root })
    }
}

impl Backend for X11 {
    fn name(&self) -> &'static str {
        "X11"
    }

    fn outputs(&mut self) -> anyhow::Result<Vec<Output>> {
        let monitors = self
            .connection
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.monitors)
            .unwrap_or_default();
        let mut outputs = Vec::new();
        for monitor in monitors {
            let name = self
                .connection
                .get_atom_name(monitor.name)?
                .reply()
                .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                .unwrap_or_else(|_| format!("Monitor {}", outputs.len() + 1));
            outputs.push(Output {
                name,
                x: monitor.x.into(),
                y: monitor.y.into(),
                width: monitor.width.into(),
                height: monitor.height.into(),
                primary: monitor.primary,
            });
        }
        // Without RandR 1.5 the whole screen is one display.
        if outputs.is_empty() {
            let geometry = self.connection.get_geometry(self.root)?.reply()?;
            outputs.push(Output {
                name: "Screen".to_owned(),
                x: 0,
                y: 0,
                width: geometry.width.into(),
                height: geometry.height.into(),
                primary: true,
            });
        }
        Ok(crate::remote::linux::display::with_all_displays(outputs))
    }

    fn start(
        &mut self,
        output: &Output,
        settings: CaptureSettings,
        sink: FrameSink,
    ) -> anyhow::Result<Box<dyn Stream>> {
        // The capture thread gets its own connection, so a slow X server
        // holds up only capture.
        let capture = X11::connect()?;
        let grabber = Grabber::new(capture, output)?;
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            capture_cursor: AtomicBool::new(settings.capture_cursor),
            cursor: Mutex::new(CursorShape::Default),
            ended: Mutex::new(None),
        });
        let worker = Arc::clone(&shared);
        let interval = Duration::from_secs(1) / settings.frames_per_second.max(1);
        let thread = std::thread::Builder::new()
            .name("meshrmm-x11-capture".into())
            .spawn(move || {
                if let Err(error) = grabber.run(&worker, interval, &sink) {
                    *worker.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                }
            })
            .context("could not start X11 capture")?;
        Ok(Box::new(X11Stream {
            shared,
            thread: Some(thread),
        }))
    }
}

struct Shared {
    stopped: AtomicBool,
    capture_cursor: AtomicBool,
    cursor: Mutex<CursorShape>,
    ended: Mutex<Option<anyhow::Error>>,
}

struct X11Stream {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stream for X11Stream {
    fn poll_ended(&mut self) -> Option<anyhow::Error> {
        self.shared
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn set_cursor_capture(&mut self, enabled: bool) -> bool {
        self.shared.capture_cursor.store(enabled, Ordering::SeqCst);
        true
    }

    fn cursor_shape(&self) -> CursorShape {
        *self.shared.cursor.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for X11Stream {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Shared memory the X server writes images into.
struct Segment {
    pointer: *mut u8,
    length: usize,
}

impl Segment {
    fn new(length: usize) -> anyhow::Result<(Self, OwnedFd)> {
        // SAFETY: memfd_create takes a NUL-terminated name and flags.
        let fd = unsafe { libc::memfd_create(c"meshrmm-x11-capture".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        // SAFETY: memfd_create returned a new descriptor this owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the descriptor is a memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error()).context("ftruncate");
        }
        // SAFETY: mapping the whole new file shared and readable.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap");
        }
        Ok((
            Self {
                pointer: pointer.cast(),
                length,
            },
            fd,
        ))
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `length` bytes and lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.pointer, self.length) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: the mapping came from mmap with this length.
        unsafe { libc::munmap(self.pointer.cast(), self.length) };
    }
}

// SAFETY: the mapping is plain memory used by one thread at a time.
unsafe impl Send for Segment {}

struct Grabber {
    x11: X11,
    area: (i16, i16, u16, u16),
    /// MIT-SHM's segment, or `None` where the X server has no MIT-SHM.
    shm: Option<(u32, Segment)>,
    previous: Vec<u8>,
    frame: Vec<u8>,
}

impl Grabber {
    fn new(x11: X11, output: &Output) -> anyhow::Result<Self> {
        let area = (
            i16::try_from(output.x)?,
            i16::try_from(output.y)?,
            u16::try_from(output.width)?,
            u16::try_from(output.height)?,
        );
        let depth = x11
            .connection
            .setup()
            .roots
            .iter()
            .find(|screen| screen.root == x11.root)
            .map(|screen| screen.root_depth)
            .unwrap_or(24);
        if depth != 24 && depth != 32 {
            bail!("the X server's {depth}-bit screen is not supported");
        }
        let length = usize::from(area.2) * usize::from(area.3) * 4;
        let shm = match x11.connection.shm_query_version()?.reply() {
            Ok(_) => {
                let (segment, fd) = Segment::new(length)?;
                let id = x11.connection.generate_id()?;
                x11.connection.shm_attach_fd(id, fd, false)?.check()?;
                Some((id, segment))
            }
            Err(error) => {
                tracing::info!(%error, "the X server has no MIT-SHM; capturing without it");
                None
            }
        };
        // XFixes 4 tells the cursor's image and name.
        let _ = x11.connection.xfixes_query_version(4, 0)?.reply();
        Ok(Self {
            x11,
            area,
            shm,
            previous: Vec::new(),
            frame: Vec::with_capacity(length),
        })
    }

    fn run(mut self, shared: &Shared, interval: Duration, sink: &FrameSink) -> anyhow::Result<()> {
        let mut next = Instant::now();
        while !shared.stopped.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
            next += interval;
            if next < Instant::now() {
                next = Instant::now() + interval;
            }
            let timestamp_us = crate::remote::linux::monotonic_timestamp_us();
            self.grab()?;
            let cursor = self
                .x11
                .connection
                .xfixes_get_cursor_image_and_name()?
                .reply()
                .ok();
            if let Some(cursor) = &cursor {
                *shared.cursor.lock().unwrap_or_else(|e| e.into_inner()) =
                    crate::remote::linux::cursor::shape_named(&String::from_utf8_lossy(
                        &cursor.name,
                    ));
                if shared.capture_cursor.load(Ordering::SeqCst) {
                    self.draw_cursor(cursor);
                }
            }
            if self.frame == self.previous {
                continue;
            }
            sink(&RawFrame {
                data: &self.frame,
                width: self.area.2.into(),
                height: self.area.3.into(),
                stride: usize::from(self.area.2) * 4,
                layout: PixelLayout::Bgrx,
                timestamp_us,
            });
            std::mem::swap(&mut self.frame, &mut self.previous);
        }
        Ok(())
    }

    /// Reads the monitor's area of the root window into `frame`.
    fn grab(&mut self) -> anyhow::Result<()> {
        let (x, y, width, height) = self.area;
        self.frame.clear();
        match &self.shm {
            Some((segment, memory)) => {
                self.x11
                    .connection
                    .shm_get_image(
                        self.x11.root,
                        x,
                        y,
                        width,
                        height,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        *segment,
                        0,
                    )?
                    .reply()
                    .context("the X server could not copy the screen")?;
                self.frame.extend_from_slice(memory.bytes());
            }
            None => {
                let image = self
                    .x11
                    .connection
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        self.x11.root,
                        x,
                        y,
                        width,
                        height,
                        !0,
                    )?
                    .reply()
                    .context("the X server could not copy the screen")?;
                anyhow::ensure!(
                    image.data.len() >= usize::from(width) * usize::from(height) * 4,
                    "the X server returned a short image"
                );
                self.frame.extend_from_slice(&image.data);
            }
        }
        Ok(())
    }

    /// Blends the pointer's premultiplied ARGB image into the frame.
    fn draw_cursor(&mut self, cursor: &x11rb::protocol::xfixes::GetCursorImageAndNameReply) {
        let (area_x, area_y, width, height) = self.area;
        let left = i32::from(cursor.x) - i32::from(cursor.xhot) - i32::from(area_x);
        let top = i32::from(cursor.y) - i32::from(cursor.yhot) - i32::from(area_y);
        let (width, height) = (i32::from(width), i32::from(height));
        for row in 0..i32::from(cursor.height) {
            let y = top + row;
            if !(0..height).contains(&y) {
                continue;
            }
            for column in 0..i32::from(cursor.width) {
                let x = left + column;
                if !(0..width).contains(&x) {
                    continue;
                }
                let Some(&argb) = cursor
                    .cursor_image
                    .get((row * i32::from(cursor.width) + column) as usize)
                else {
                    continue;
                };
                let alpha = argb >> 24;
                if alpha == 0 {
                    continue;
                }
                let pixel = ((y * width + x) * 4) as usize;
                let destination = &mut self.frame[pixel..pixel + 3];
                for (channel, shift) in [(0, 0), (1, 8), (2, 16)] {
                    let source = (argb >> shift) & 0xff;
                    let below = u32::from(destination[channel]);
                    destination[channel] = (source + below * (255 - alpha) / 255).min(255) as u8;
                }
            }
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        if let Some((segment, _)) = &self.shm {
            let _ = self.x11.connection.shm_detach(*segment);
            let _ = self.x11.connection.flush();
        }
    }
}
