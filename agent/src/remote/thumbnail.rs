//! A small image of the console's main display, for the website: refreshed
//! every few minutes while a website tab shows the device, about hourly
//! otherwise, and uploaded when the screen changed ([`Thumbnails`]).
//!
//! The coordinator runs in Session 0, which cannot see the console. A
//! LocalSystem desktop helper captures and encodes the image on the console's
//! input desktop, so the sign-in and lock screens are captured too. The
//! coordinator sends the image straight to the control plane over HTTPS; it
//! never crosses the signaling WebSocket.
//!
//! On a Mac, the console's session helper captures the image for the root
//! coordinator; see docs/screen-thumbnails.md.
//!
//! Whoever captures compares the pixels with the last uploaded image's
//! [`Signature`] before encoding, so a screen that looks the same costs
//! neither a JPEG nor an upload.
#![cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
use anyhow::ensure;
use serde::{Deserialize, Serialize};

/// The image fits in this box. It is sharp at the dashboard's preview size
/// and small enough to be a few dozen KiB as a JPEG.
pub const MAX_WIDTH: u32 = 640;
pub const MAX_HEIGHT: u32 = 400;
/// The server refuses anything larger.
pub const MAX_BYTES: usize = 512 * 1024;

/// The signature is a grid of average brightness this many cells wide and
/// tall. A cell of a 640x400 thumbnail is 20 pixels square: about 60 by 54
/// pixels of a 1920x1080 display, 1/640 of the screen.
const COLUMNS: usize = 32;
const ROWS: usize = 20;
/// A cell has changed when its average brightness moves by more than this,
/// of 255, and the image when at least [`CHANGED_CELLS`] cells have. A
/// clock in the taskbar or menu bar redraws a digit or two a minute, a few
/// dozen pixels, which moves the cells under it by a few levels; even when
/// every digit changes it reaches only the two to four cells the clock
/// spans. A blinking text cursor is a line a pixel or two wide in one cell.
/// A different window, a dialog, a notification or the lock screen repaints
/// a block of many cells, usually by tens of levels.
const CELL_TOLERANCE: u8 = 10;
const CHANGED_CELLS: usize = 5;

/// How a capture's bytes hold each pixel.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    bytes_per_pixel: usize,
    red: usize,
    green: usize,
    blue: usize,
}

impl Layout {
    #[cfg(any(windows, target_os = "macos", test))]
    pub const BGRX: Self = Self {
        bytes_per_pixel: 4,
        red: 2,
        green: 1,
        blue: 0,
    };
    #[cfg(any(target_os = "linux", test))]
    pub const RGB: Self = Self {
        bytes_per_pixel: 3,
        red: 0,
        green: 1,
        blue: 2,
    };
}

/// A captured image, top row first, before it is encoded.
pub struct Pixels<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
    layout: Layout,
}

impl<'a> Pixels<'a> {
    /// `data` holds `height` rows `stride` bytes apart.
    pub fn new(
        data: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
        layout: Layout,
    ) -> anyhow::Result<Self> {
        let fits = width
            .checked_mul(layout.bytes_per_pixel)
            .is_some_and(|row| {
                row <= stride
                    && (height == 0
                        || stride
                            .checked_mul(height - 1)
                            .and_then(|last| last.checked_add(row))
                            .is_some_and(|end| end <= data.len()))
            });
        ensure!(
            fits,
            "a {width}x{height} capture does not fit in {} bytes",
            data.len()
        );
        Ok(Self {
            data,
            width,
            height,
            stride,
            layout,
        })
    }
}

/// A coarse grayscale picture of an image: the average brightness of each
/// cell of a [`COLUMNS`] by [`ROWS`] grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature(Box<[[u8; COLUMNS]; ROWS]>);

impl Signature {
    pub fn of(image: &Pixels<'_>) -> Self {
        let mut sums = [[0_u64; COLUMNS]; ROWS];
        let mut counts = [[0_u64; COLUMNS]; ROWS];
        let columns: Vec<usize> = (0..image.width)
            .map(|x| x * COLUMNS / image.width)
            .collect();
        let Layout {
            bytes_per_pixel,
            red,
            green,
            blue,
        } = image.layout;
        for y in 0..image.height {
            let row = y * ROWS / image.height;
            let line = &image.data[y * image.stride..];
            for (x, &column) in columns.iter().enumerate() {
                let pixel = &line[x * bytes_per_pixel..];
                sums[row][column] += luma(pixel[red], pixel[green], pixel[blue]);
                counts[row][column] += 1;
            }
        }
        let mut cells = [[0; COLUMNS]; ROWS];
        for ((cells, sums), counts) in cells.iter_mut().zip(&sums).zip(&counts) {
            for ((cell, sum), count) in cells.iter_mut().zip(sums).zip(counts) {
                *cell = sum.checked_div(*count).unwrap_or(0) as u8;
            }
        }
        Self(Box::new(cells))
    }

    /// Whether the two images look the same at a glance; see
    /// [`CELL_TOLERANCE`].
    pub fn looks_like(&self, other: &Self) -> bool {
        let changed = self
            .0
            .as_flattened()
            .iter()
            .zip(other.0.as_flattened())
            .filter(|(this, other)| this.abs_diff(**other) > CELL_TOLERANCE)
            .count();
        changed < CHANGED_CELLS
    }
}

/// The Windows desktop helper's pipe carries signatures as bytes.
#[cfg(any(windows, test))]
impl Signature {
    pub const BYTES: usize = COLUMNS * ROWS;

    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        bytes.copy_from_slice(self.0.as_flattened());
        bytes
    }

    pub fn from_bytes(bytes: &[u8; Self::BYTES]) -> Self {
        let mut cells = [[0; COLUMNS]; ROWS];
        cells.as_flattened_mut().copy_from_slice(bytes);
        Self(Box::new(cells))
    }
}

/// Brightness as the eye sees it, with the BT.601 weights in 8-bit fixed
/// point.
fn luma(red: u8, green: u8, blue: u8) -> u64 {
    (77 * u64::from(red) + 150 * u64::from(green) + 29 * u64::from(blue) + 128) >> 8
}

/// A capture compared with the last uploaded image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capture {
    /// It looks like that image, so it was not encoded.
    Unchanged,
    Changed {
        signature: Signature,
        jpeg: Vec<u8>,
    },
}

/// A capture that has `signature` as a JPEG made by `encode`, unless it
/// looks like `previous`.
pub fn encode_if_changed(
    signature: Signature,
    previous: Option<&Signature>,
    encode: impl FnOnce() -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<Capture> {
    if previous.is_some_and(|previous| signature.looks_like(previous)) {
        return Ok(Capture::Unchanged);
    }
    let jpeg = encode()?;
    ensure!(
        jpeg.len() <= MAX_BYTES,
        "the {} byte thumbnail is larger than {MAX_BYTES} bytes",
        jpeg.len()
    );
    Ok(Capture::Changed { signature, jpeg })
}

/// The thumbnail size for a `width` by `height` display: its aspect ratio,
/// fitted in the [`MAX_WIDTH`] by [`MAX_HEIGHT`] box, never enlarged.
pub fn scaled_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let (width, height) = (u64::from(width), u64::from(height));
    let (max_width, max_height) = (u64::from(MAX_WIDTH), u64::from(MAX_HEIGHT));
    let (scaled_width, scaled_height) = if width <= max_width && height <= max_height {
        (width, height)
    } else if width * max_height >= height * max_width {
        (max_width, (height * max_width).div_ceil(width))
    } else {
        ((width * max_height).div_ceil(height), max_height)
    };
    (scaled_width.max(1) as u32, scaled_height.max(1) as u32)
}

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub use self::schedule::Thumbnails;
#[cfg(windows)]
use self::windows as platform;
#[cfg(windows)]
pub use self::windows::{capture_primary_display, follow_input_desktop};

#[cfg(windows)]
mod windows {
    use anyhow::Context;
    use windows::Win32::Foundation::{GENERIC_ALL, POINT};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, GdiFlush, GetDC, GetMonitorInfoW, HALFTONE, HBITMAP, HDC, HGDIOBJ,
        MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, ReleaseDC, SRCCOPY, SelectObject,
        SetBrushOrgEx, SetStretchBltMode, StretchBlt,
    };
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
        GUID_WICPixelFormat32bppBGR, IWICImagingFactory, WICBitmapEncoderNoCache,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPBAG2;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
    };
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
    };
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VT_R4};
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use windows::core::w;

    use super::{Capture, Layout, Pixels, Signature, encode_if_changed, scaled_size};
    use crate::remote::config::ExecutionMode;

    const JPEG_QUALITY: f32 = 0.7;

    /// Moves the calling thread to the desktop that currently receives input,
    /// such as the lock screen while the user's desktop is launched but
    /// hidden. Keeps the launch desktop if Windows refuses.
    pub fn follow_input_desktop() {
        // A helper runs as LocalSystem, which may open every desktop. The
        // handle stays open for the helper's short life.
        match unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )
        } {
            Ok(desktop) => {
                if let Err(error) = unsafe { SetThreadDesktop(desktop) } {
                    tracing::debug!(%error, "thumbnail capture stays on its launch desktop");
                }
            }
            Err(error) => tracing::debug!(%error, "could not open the input desktop"),
        }
    }

    /// Captures the primary display, scaled to the thumbnail size, as a JPEG
    /// unless it looks like `previous`.
    pub fn capture_primary_display(previous: Option<&Signature>) -> anyhow::Result<Capture> {
        // Physical pixels. A process that already chose a DPI context gets
        // access denied and keeps it.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetMonitorInfoW(monitor, &mut info) }
            .ok()
            .context("could not find the primary display")?;
        let bounds = info.rcMonitor;
        let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
        anyhow::ensure!(width > 0 && height > 0, "the primary display has no area");
        let (scaled_width, scaled_height) = scaled_size(width as u32, height as u32);
        let pixels = capture(
            (bounds.left, bounds.top, width, height),
            scaled_width,
            scaled_height,
        )?;
        let (width, height) = (scaled_width as usize, scaled_height as usize);
        let image = Pixels::new(&pixels, width, height, width * 4, Layout::BGRX)?;
        encode_if_changed(Signature::of(&image), previous, || {
            encode_jpeg(&pixels, scaled_width, scaled_height)
        })
    }

    struct ScreenDc(HDC);

    impl Drop for ScreenDc {
        fn drop(&mut self) {
            unsafe { ReleaseDC(None, self.0) };
        }
    }

    struct MemoryDc(HDC);

    impl Drop for MemoryDc {
        fn drop(&mut self) {
            let _ = unsafe { DeleteDC(self.0) };
        }
    }

    /// A bitmap selected into a memory DC, deselected and freed when dropped.
    struct Selected<'a> {
        dc: &'a MemoryDc,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
    }

    impl Drop for Selected<'_> {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc.0, self.previous);
                let _ = DeleteObject(self.bitmap.into());
            }
        }
    }

    /// Scales the `(x, y, width, height)` screen area into a top-down 32-bit
    /// BGRX buffer. GDI's halftone mode averages the source pixels, so text
    /// shrinks without shimmering.
    fn capture(
        (x, y, width, height): (i32, i32, i32, i32),
        scaled_width: u32,
        scaled_height: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let screen = ScreenDc(unsafe { GetDC(None) });
        anyhow::ensure!(!screen.0.is_invalid(), "could not open the screen");
        let memory = MemoryDc(unsafe { CreateCompatibleDC(Some(screen.0)) });
        anyhow::ensure!(!memory.0.is_invalid(), "could not create a drawing surface");
        let header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: scaled_width as i32,
                biHeight: -(scaled_height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(Some(memory.0), &header, DIB_RGB_COLORS, &mut bits, None, 0)
        }
        .context("could not create the thumbnail bitmap")?;
        let selected = Selected {
            dc: &memory,
            bitmap,
            previous: unsafe { SelectObject(memory.0, bitmap.into()) },
        };
        unsafe {
            SetStretchBltMode(memory.0, HALFTONE);
            let _ = SetBrushOrgEx(memory.0, 0, 0, None);
            StretchBlt(
                memory.0,
                0,
                0,
                scaled_width as i32,
                scaled_height as i32,
                Some(screen.0),
                x,
                y,
                width,
                height,
                SRCCOPY,
            )
            .ok()
            .context("could not copy the screen")?;
            let _ = GdiFlush();
        }
        let length = scaled_width as usize * scaled_height as usize * 4;
        let pixels = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), length) }.to_vec();
        drop(selected);
        Ok(pixels)
    }

    /// Encodes top-down BGRX pixels with the Windows Imaging Component.
    fn encode_jpeg(pixels: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let result = (|| unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .context("could not start the Windows Imaging Component")?;
            let bitmap = factory.CreateBitmapFromMemory(
                width,
                height,
                &GUID_WICPixelFormat32bppBGR,
                width * 4,
                pixels,
            )?;
            let stream = SHCreateMemStream(None).context("could not create an image stream")?;
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let (mut frame, mut options) = (None, None);
            encoder.CreateNewFrame(&mut frame, &mut options)?;
            let frame = frame.context("the JPEG encoder returned no frame")?;
            let options = options.context("the JPEG encoder returned no options")?;
            let quality = PROPBAG2 {
                pstrName: windows::core::PWSTR(w!("ImageQuality").as_ptr().cast_mut()),
                ..Default::default()
            };
            let mut value = VARIANT::default();
            value.Anonymous.Anonymous = std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_R4,
                ..Default::default()
            });
            (*value.Anonymous.Anonymous).Anonymous.fltVal = JPEG_QUALITY;
            options.Write(1, &quality, &value)?;
            frame.Initialize(&options)?;
            frame.SetSize(width, height)?;
            let mut format = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut format)?;
            frame.WriteSource(&bitmap, std::ptr::null())?;
            frame.Commit()?;
            encoder.Commit()?;

            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME)?;
            let length = usize::try_from(stat.cbSize)?;
            let mut jpeg = vec![0; length];
            stream.Seek(0, STREAM_SEEK_SET, None)?;
            let mut read = 0;
            stream
                .Read(jpeg.as_mut_ptr().cast(), length as u32, Some(&mut read))
                .ok()?;
            jpeg.truncate(read as usize);
            Ok(jpeg)
        })();
        if initialized {
            unsafe { CoUninitialize() };
        }
        result
    }

    pub(super) fn capture_for(
        mode: ExecutionMode,
        previous: Option<&Signature>,
    ) -> anyhow::Result<Capture> {
        match mode {
            // Local development runs on the developer's own desktop.
            ExecutionMode::Console => capture_primary_display(previous),
            _ => crate::remote::capture_helper::capture_thumbnail(previous),
        }
    }
}

/// The schedule and upload, around each platform's capture.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod schedule {
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context;
    use meshrmm_protocol::{THUMBNAIL_IDLE_INTERVAL_SECONDS, THUMBNAIL_WATCHED_INTERVAL_SECONDS};
    use tokio::task::JoinHandle;
    use tokio::time::Instant;

    use super::platform::capture_for;
    use super::{Capture, Signature};
    use crate::remote::config::{Config, ExecutionMode};

    /// How often the image is refreshed on a connection until the server
    /// says otherwise: what it asks for while no website tab shows the
    /// device.
    pub const IDLE_INTERVAL: Duration = Duration::from_secs(THUMBNAIL_IDLE_INTERVAL_SECONDS);
    /// The shortest interval the Agent refreshes at, whatever it is told:
    /// what the server asks for while a tab shows the device.
    const SHORTEST_INTERVAL: Duration = Duration::from_secs(THUMBNAIL_WATCHED_INTERVAL_SECONDS);
    /// The longest, so an image is never more than a few hours old.
    const LONGEST_INTERVAL: Duration = Duration::from_secs(3 * 60 * 60);
    /// A capture that finished this recently is fresh enough for someone
    /// who just started looking, so a tab reloading or a wall scrolled back
    /// and forth does not have the device capture again and again.
    const FRESH: Duration = Duration::from_secs(60);
    /// A capture that looks like the last image sent is sent anyway once
    /// that one is this old, so a change too small to notice is shown within
    /// a few hours. The first capture on each connection is always sent, so
    /// a server that lost the image, as in a crash or a restore from a
    /// backup, has it back once the Agent reconnects.
    const RESEND_AFTER: Duration = Duration::from_secs(3 * 60 * 60);
    const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
    /// After a failure, such as when the Agent starts before the session it
    /// captures, the next few attempts come sooner than the interval.
    const RETRY: Duration = Duration::from_secs(30);
    const QUICK_RETRIES: u32 = 4;
    /// The first capture after the Agent starts falls within this, so a newly
    /// enrolled device shows its screen soon, but a fleet that restarts
    /// together does not upload in the same second.
    const STARTUP_SPREAD: Duration = Duration::from_secs(60);
    /// Each interval is up to this fraction longer or shorter than the one
    /// set. The random phase a longer interval starts at is what spreads a
    /// fleet out; this only keeps it from falling back into step.
    const JITTER: f64 = 0.05;

    /// Captures the screen and uploads it unless it looks like the image
    /// given, and returns the signature of the one it sent, if it sent one.
    type Refresh =
        dyn Fn(&Config, Option<&Signature>) -> anyhow::Result<Option<Signature>> + Send + Sync;
    type Fraction = Box<dyn FnMut() -> f64 + Send>;
    /// A refresh's result, with the connection it started on.
    type Finished = (u64, anyhow::Result<Option<Signature>>);

    /// Refreshes the device's thumbnail every interval the server set while
    /// the Agent is connected, when the screen changed: about hourly, at a
    /// random phase, while no website tab shows the device, and every few
    /// minutes while one does, starting at once. One capture and upload runs
    /// at a time, off the signaling task.
    pub struct Thumbnails {
        interval: Duration,
        /// When the next capture is due.
        next: Instant,
        refresh: Arc<Refresh>,
        /// A random fraction of 1, for phases and jitter.
        fraction: Fraction,
        task: Option<JoinHandle<Finished>>,
        /// Counts connections, so an upload that finishes after a reconnect
        /// does not count as the new connection's first.
        connection: u64,
        /// The last image the server accepted on this connection, and when.
        sent: Option<(Signature, Instant)>,
        /// When the last capture finished, whether or not it was sent.
        captured_at: Option<Instant>,
        /// Repeated failures, such as while no display is attached, are
        /// logged once.
        failing: bool,
        /// Failures in a row, and when the last one happened.
        failures: u32,
        failed_at: Option<Instant>,
    }

    impl Thumbnails {
        pub fn new(mode: ExecutionMode) -> Self {
            // One client for the life of the process: the TLS configuration
            // it builds on the first upload keeps the session cache that lets
            // the next upload resume the TLS session instead of a full
            // handshake.
            let http = http_client();
            Self::with(
                move |config, baseline| upload_if_changed(mode, config, &http, baseline),
                || meshrmm_signaling_client::random_fraction().unwrap_or(0.5),
            )
        }

        fn with(
            refresh: impl Fn(&Config, Option<&Signature>) -> anyhow::Result<Option<Signature>>
            + Send
            + Sync
            + 'static,
            fraction: impl FnMut() -> f64 + Send + 'static,
        ) -> Self {
            let mut thumbnails = Self {
                interval: IDLE_INTERVAL,
                next: Instant::now(),
                refresh: Arc::new(refresh),
                fraction: Box::new(fraction),
                task: None,
                connection: 0,
                sent: None,
                captured_at: None,
                failing: false,
                failures: 0,
                failed_at: None,
            };
            let delay = STARTUP_SPREAD.mul_f64(thumbnails.fraction());
            thumbnails.next += delay;
            thumbnails
        }

        /// A random fraction in `[0, 1]`.
        fn fraction(&mut self) -> f64 {
            let fraction = (self.fraction)();
            if fraction.is_finite() {
                fraction.clamp(0.0, 1.0)
            } else {
                0.5
            }
        }

        /// The interval, up to [`JITTER`] longer or shorter at random.
        fn jittered_interval(&mut self) -> Duration {
            let fraction = self.fraction();
            self.interval
                .mul_f64(1.0 - JITTER + 2.0 * JITTER * fraction)
        }

        /// The interval the Agent refreshes at now.
        #[cfg(test)]
        fn interval(&self) -> Duration {
            self.interval
        }

        /// Called on each connection to the server: it refreshes at
        /// [`IDLE_INTERVAL`] until the server says otherwise, and sends its
        /// first capture whatever it looks like. A capture that fell due
        /// while the Agent was offline is put at a random point of the
        /// interval, so a fleet reconnecting after an outage does not upload
        /// together.
        pub fn connected(&mut self) {
            self.connection += 1;
            self.sent = None;
            let now = Instant::now();
            if self.interval != IDLE_INTERVAL {
                self.set_interval(IDLE_INTERVAL);
            } else if self.next <= now {
                self.next = now + self.interval.mul_f64(self.fraction());
            }
        }

        /// Refreshes every `seconds` the server asked for, within what the
        /// Agent accepts. The interval it already has changes nothing.
        pub fn set_interval_seconds(&mut self, seconds: u32) {
            let interval =
                Duration::from_secs(u64::from(seconds)).clamp(SHORTEST_INTERVAL, LONGEST_INTERVAL);
            self.set_interval(interval);
        }

        /// A shorter interval means someone started looking, so the next
        /// capture is now, unless one finished within [`FRESH`]: then it is
        /// an interval after that one. A longer one starts at a random part
        /// of itself, so a fleet told the same in the same second, as when
        /// the server restarts, does not capture together.
        fn set_interval(&mut self, interval: Duration) {
            if interval == self.interval {
                return;
            }
            tracing::debug!(?interval, "screen thumbnail interval changed");
            let now = Instant::now();
            let sooner = interval < self.interval;
            self.interval = interval;
            self.next = if sooner {
                match self.captured_at.filter(|at| now < *at + FRESH) {
                    Some(at) => self.next.min(at + interval),
                    None => now,
                }
            } else {
                now + interval.mul_f64(self.fraction())
            };
        }

        /// When a quick retry is due, if one is.
        fn retry_at(&self) -> Option<Instant> {
            (self.failures <= QUICK_RETRIES)
                .then_some(self.failed_at)
                .flatten()
                .map(|failed| failed + RETRY)
        }

        /// Completes when the next thumbnail is due. Cancel safe: a refresh
        /// that finishes meanwhile is recorded by the next call.
        pub async fn due(&mut self) {
            loop {
                let retry_at = self.retry_at();
                let wake = retry_at.map_or(self.next, |at| at.min(self.next));
                let running = async {
                    match &mut self.task {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    finished = running => {
                        self.task = None;
                        self.finished(finished);
                    }
                    () = tokio::time::sleep_until(wake) => {
                        let now = Instant::now();
                        if self.next <= now {
                            self.next = now + self.jittered_interval();
                            return;
                        }
                        if retry_at.is_some_and(|at| at <= now) {
                            self.failed_at = None;
                            return;
                        }
                    }
                }
            }
        }

        fn finished(&mut self, finished: Result<Finished, tokio::task::JoinError>) {
            let now = Instant::now();
            let error = match finished {
                Ok((connection, Ok(sent))) => {
                    tracing::debug!(sent = sent.is_some(), "refreshed the screen thumbnail");
                    self.captured_at = Some(now);
                    if let Some(signature) = sent
                        && connection == self.connection
                    {
                        self.sent = Some((signature, now));
                    }
                    self.failures = 0;
                    self.failed_at = None;
                    if std::mem::take(&mut self.failing) {
                        tracing::info!("screen thumbnails are updating again");
                    }
                    return;
                }
                Ok((_, Err(error))) => error,
                Err(error) => error.into(),
            };
            self.failures = self.failures.saturating_add(1);
            self.failed_at = Some(now);
            if std::mem::replace(&mut self.failing, true) {
                tracing::debug!(error = ?error, "screen thumbnail still unavailable");
            } else {
                tracing::warn!(error = ?error, "could not refresh the screen thumbnail; retrying");
            }
        }

        /// What a new capture is compared with: the last image sent on this
        /// connection, until it is [`RESEND_AFTER`] old. Nothing, so the
        /// capture is sent, before then and after.
        fn baseline(&self) -> Option<Signature> {
            let now = Instant::now();
            self.sent
                .as_ref()
                .filter(|(_, at)| now < *at + RESEND_AFTER)
                .map(|(signature, _)| signature.clone())
        }

        /// Starts a capture and upload unless one is still running.
        pub fn refresh(&mut self, config: &Config) {
            if self.task.is_some() {
                return;
            }
            let (refresh, config, baseline, connection) = (
                Arc::clone(&self.refresh),
                config.clone(),
                self.baseline(),
                self.connection,
            );
            self.task = Some(tokio::task::spawn_blocking(move || {
                (connection, refresh(&config, baseline.as_ref()))
            }));
        }
    }

    impl Drop for Thumbnails {
        fn drop(&mut self) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    /// Returns the signature of the image sent: one that looks like
    /// `baseline` is not sent, and isn't even encoded.
    fn upload_if_changed(
        mode: ExecutionMode,
        config: &Config,
        http: &ureq::Agent,
        baseline: Option<&Signature>,
    ) -> anyhow::Result<Option<Signature>> {
        match capture_for(mode, baseline)? {
            Capture::Unchanged => Ok(None),
            Capture::Changed { signature, jpeg } => {
                upload(config, http, &jpeg)?;
                Ok(Some(signature))
            }
        }
    }

    /// Keeps no connection open between uploads, minutes apart: an idle
    /// connection per Agent would cost the server a socket each, more than
    /// the resumed handshake that replaces it.
    fn http_client() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(UPLOAD_TIMEOUT))
            .http_status_as_error(true)
            // ureq prunes its pool only when a connection is returned to it,
            // so any idle allowance would keep the socket until the next upload.
            .max_idle_connections(0)
            .tls_config(crate::enrollment::https_tls_config())
            .build()
            .new_agent()
    }

    fn upload(config: &Config, http: &ureq::Agent, jpeg: &[u8]) -> anyhow::Result<()> {
        let url = meshrmm_signaling_client::endpoint_url(
            &config.server,
            &["v1", "agents", &config.device_id, "thumbnail"],
            &[],
            false,
        )?;
        http.put(url.as_str())
            .header("Authorization", &format!("Bearer {}", config.agent_token))
            .content_type("image/jpeg")
            .send(jpeg)
            .context("the server did not accept the screen thumbnail")?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Mutex;

        use super::*;

        const WATCHED: u32 = 300;
        const IDLE: u32 = 3_600;

        fn signature(brightness: u8) -> Signature {
            Signature::from_bytes(&[brightness; Signature::BYTES])
        }

        fn at(minutes: u64, seconds: u64) -> Duration {
            Duration::from_secs(minutes * 60 + seconds)
        }

        /// A fake screen: what each capture sees, and what became of it.
        struct Screen {
            /// The brightness the screen shows, or none for a capture that
            /// fails.
            shows: Option<u8>,
            /// Each capture's time since the test started, and whether it
            /// was sent.
            captures: Vec<(Duration, bool)>,
            /// When captures failed.
            failures: Vec<Duration>,
        }

        struct Fake {
            thumbnails: Thumbnails,
            screen: Arc<Mutex<Screen>>,
            config: Config,
        }

        /// Thumbnails of a fake screen, with `fraction` for every random
        /// part. Its upload sends what the comparison says.
        fn fake(fraction: f64) -> Fake {
            let screen = Arc::new(Mutex::new(Screen {
                shows: Some(40),
                captures: Vec::new(),
                failures: Vec::new(),
            }));
            let seen = Arc::clone(&screen);
            let start = Instant::now();
            let thumbnails = Thumbnails::with(
                move |_, baseline| {
                    let mut screen = seen.lock().unwrap();
                    let at = start.elapsed();
                    let Some(shows) = screen.shows else {
                        screen.failures.push(at);
                        anyhow::bail!("no display");
                    };
                    let image = signature(shows);
                    let sent = !baseline.is_some_and(|baseline| baseline.looks_like(&image));
                    screen.captures.push((at, sent));
                    Ok(sent.then_some(image))
                },
                move || fraction,
            );
            Fake {
                thumbnails,
                screen,
                config: Config {
                    config_path: "agent.json".into(),
                    server: "https://rmm.example.com".into(),
                    device_id: "device".into(),
                    agent_token: "token".into(),
                    frames_per_second: 30,
                    bitrate_bits_per_second: 1_000_000,
                    json_logs: false,
                },
            }
        }

        impl Fake {
            /// Runs the coordinator's loop for `wait`.
            async fn run(&mut self, wait: Duration) {
                let (thumbnails, config) = (&mut self.thumbnails, &self.config);
                let _ = tokio::time::timeout(wait, async {
                    loop {
                        thumbnails.due().await;
                        thumbnails.refresh(config);
                    }
                })
                .await;
            }

            /// The captures since the last call, as the time since the test
            /// started, and whether each was sent.
            fn captures(&self) -> Vec<(Duration, bool)> {
                std::mem::take(&mut self.screen.lock().unwrap().captures)
            }

            fn show(&self, brightness: Option<u8>) {
                self.screen.lock().unwrap().shows = brightness;
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a_connection_starts_hourly_after_the_startup_spread() {
            let mut fake = fake(0.5);
            fake.thumbnails.connected();
            assert_eq!(fake.thumbnails.interval(), IDLE_INTERVAL);
            fake.run(at(200, 0)).await;
            // Half the spread in, then an hour (the jitter's middle) after
            // each. The screen did not change, so it is sent again only
            // three hours after the first.
            assert_eq!(
                fake.captures(),
                [
                    (at(0, 30), true),
                    (at(60, 30), false),
                    (at(120, 30), false),
                    (at(180, 30), true)
                ]
            );
        }

        #[tokio::test(start_paused = true)]
        async fn becoming_watched_captures_at_once_then_every_five_minutes() {
            let mut fake = fake(0.5);
            fake.thumbnails.connected();
            fake.run(at(10, 0)).await;
            assert_eq!(fake.captures(), [(at(0, 30), true)]);
            fake.show(Some(200));
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(16, 0)).await;
            assert_eq!(
                fake.captures(),
                [
                    (at(10, 0), true),
                    (at(15, 0), false),
                    (at(20, 0), false),
                    (at(25, 0), false)
                ]
            );
            // Told the same again, the timer stays where it was.
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(5, 0)).await;
            assert_eq!(fake.captures(), [(at(30, 0), false)]);
        }

        #[tokio::test(start_paused = true)]
        async fn becoming_watched_again_within_the_minute_does_not_capture_again() {
            let mut fake = fake(0.5);
            fake.thumbnails.connected();
            fake.run(at(2, 0)).await;
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(0, 10)).await;
            assert_eq!(fake.captures(), [(at(0, 30), true), (at(2, 0), false)]);
            // Unwatched and watched again, as a wall scrolled back and
            // forth: the image is half a minute old, so the next is five
            // minutes after it.
            fake.thumbnails.set_interval_seconds(IDLE);
            fake.run(at(0, 20)).await;
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(0, 10)).await;
            assert_eq!(fake.captures(), []);
            fake.run(at(5, 0)).await;
            assert_eq!(fake.captures(), [(at(7, 0), false)]);
            // Watched again minutes later: at once.
            fake.thumbnails.set_interval_seconds(IDLE);
            fake.run(at(2, 0)).await;
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(0, 1)).await;
            assert_eq!(fake.captures(), [(at(9, 40), false)]);
        }

        #[tokio::test(start_paused = true)]
        async fn unwatched_refreshes_hourly_from_a_random_phase() {
            for (fraction, first) in [(0.0, at(5, 0)), (0.25, at(20, 0)), (0.75, at(50, 0))] {
                let mut fake = fake(fraction);
                fake.thumbnails.connected();
                fake.thumbnails.set_interval_seconds(WATCHED);
                fake.run(at(5, 0)).await;
                fake.captures();
                // Unwatched five minutes in: a random part of the hour on.
                fake.thumbnails.set_interval_seconds(IDLE);
                fake.run(at(110, 0)).await;
                let captures: Vec<Duration> =
                    fake.captures().into_iter().map(|(at, _)| at).collect();
                let hour = IDLE_INTERVAL.mul_f64(1.0 - JITTER + 2.0 * JITTER * fraction);
                assert_eq!(captures, [first, first + hour], "fraction {fraction}");
            }
        }

        #[tokio::test(start_paused = true)]
        async fn the_interval_is_kept_within_what_the_agent_accepts() {
            let mut fake = fake(0.0);
            fake.thumbnails.set_interval_seconds(0);
            assert_eq!(fake.thumbnails.interval(), SHORTEST_INTERVAL);
            fake.thumbnails.set_interval_seconds(u32::MAX);
            assert_eq!(fake.thumbnails.interval(), LONGEST_INTERVAL);
            fake.thumbnails.set_interval_seconds(IDLE);
            assert_eq!(fake.thumbnails.interval(), IDLE_INTERVAL);
        }

        #[tokio::test(start_paused = true)]
        async fn an_unchanged_screen_is_skipped_and_sent_again_after_three_hours() {
            let mut fake = fake(0.0);
            fake.thumbnails.connected();
            fake.thumbnails.set_interval_seconds(WATCHED);
            // Every 4 m 45 s, with the jitter at its shortest.
            let every = at(4, 45);
            fake.run(RESEND_AFTER + every).await;
            let captures = fake.captures();
            assert_eq!(captures.len(), 39);
            let sent: Vec<Duration> = captures
                .iter()
                .filter(|(_, sent)| *sent)
                .map(|(at, _)| *at)
                .collect();
            // The first, and the first three hours after it.
            assert!(every * 37 < RESEND_AFTER && every * 38 >= RESEND_AFTER);
            assert_eq!(sent, [Duration::ZERO, every * 38]);
            // A change is sent at once.
            fake.show(Some(200));
            fake.run(every).await;
            assert_eq!(fake.captures(), [(every * 39, true)]);
        }

        #[tokio::test(start_paused = true)]
        async fn a_new_connection_is_hourly_until_told_and_sends_its_first_capture() {
            let mut fake = fake(0.5);
            fake.thumbnails.connected();
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(6, 0)).await;
            assert_eq!(fake.captures(), [(at(0, 0), true), (at(5, 0), false)]);
            // Reconnected: hourly from a random phase, and the image is sent
            // though the screen looks the same.
            fake.thumbnails.connected();
            assert_eq!(fake.thumbnails.interval(), IDLE_INTERVAL);
            fake.run(at(60, 0)).await;
            assert_eq!(fake.captures(), [(at(36, 0), true)]);
            // Reconnected and told it is watched: at once, and sent.
            fake.thumbnails.connected();
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(0, 1)).await;
            assert_eq!(fake.captures(), [(at(66, 0), true)]);
        }

        #[tokio::test(start_paused = true)]
        async fn failures_retry_soon_a_few_times_then_wait_for_the_interval() {
            let mut fake = fake(0.0);
            fake.show(None);
            fake.thumbnails.connected();
            fake.thumbnails.set_interval_seconds(WATCHED);
            fake.run(at(9, 0)).await;
            // At once, four quick retries, then at the interval.
            assert_eq!(
                fake.screen.lock().unwrap().failures,
                [
                    at(0, 0),
                    at(0, 30),
                    at(1, 0),
                    at(1, 30),
                    at(2, 0),
                    at(4, 45)
                ]
            );
            fake.show(Some(40));
            fake.run(at(1, 0)).await;
            assert_eq!(fake.captures(), [(at(9, 30), true)]);
            assert_eq!(fake.thumbnails.failures, 0);
        }
    }
}

/// A console Agent captures its own session; the installed coordinator asks
/// the console's session helper, which captures the login window too.
#[cfg(target_os = "macos")]
mod macos {
    use super::{Capture, Signature};
    use crate::remote::config::ExecutionMode;

    pub(super) fn capture_for(
        mode: ExecutionMode,
        previous: Option<&Signature>,
    ) -> anyhow::Result<Capture> {
        match mode {
            ExecutionMode::Console => crate::remote::macos::snapshot::main_display(previous),
            _ => crate::remote::helper::coordinator::registry()?.thumbnail(previous),
        }
    }
}

/// As on a Mac: the console session's helper captures it. A computer with
/// nobody at a graphical login, such as a server, has no thumbnail.
#[cfg(target_os = "linux")]
mod linux {
    use super::{Capture, Signature};
    use crate::remote::config::ExecutionMode;

    pub(super) fn capture_for(
        mode: ExecutionMode,
        previous: Option<&Signature>,
    ) -> anyhow::Result<Capture> {
        match mode {
            ExecutionMode::Console => crate::remote::linux::snapshot::main_display(previous),
            _ => crate::remote::helper::coordinator::registry()?.thumbnail(previous),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_keep_the_aspect_ratio_inside_the_box() {
        assert_eq!(scaled_size(1920, 1080), (640, 360));
        assert_eq!(scaled_size(2560, 1600), (640, 400));
        assert_eq!(scaled_size(3840, 2160), (640, 360));
        // Portrait and ultrawide displays are bounded by the other side.
        assert_eq!(scaled_size(1080, 1920), (225, 400));
        assert_eq!(scaled_size(5120, 1440), (640, 180));
        assert_eq!(scaled_size(1280, 1024), (500, 400));
    }

    #[test]
    fn small_or_degenerate_displays_are_not_enlarged() {
        assert_eq!(scaled_size(640, 400), (640, 400));
        assert_eq!(scaled_size(320, 200), (320, 200));
        assert_eq!(scaled_size(100_000, 1), (640, 1));
        assert_eq!(scaled_size(0, 1080), (0, 0));
    }

    const WIDTH: usize = MAX_WIDTH as usize;
    const HEIGHT: usize = MAX_HEIGHT as usize;

    /// A light gray RGB thumbnail with a little texture.
    fn screen() -> Vec<u8> {
        let mut rgb = Vec::with_capacity(WIDTH * HEIGHT * 3);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                rgb.extend([220 + ((x * 7 + y * 13) % 11) as u8; 3]);
            }
        }
        rgb
    }

    /// Paints the `(x, y, width, height)` rectangle `value`.
    fn paint(rgb: &mut [u8], (left, top, width, height): (usize, usize, usize, usize), value: u8) {
        for y in top..top + height {
            for x in left..left + width {
                let at = (y * WIDTH + x) * 3;
                rgb[at..at + 3].fill(value);
            }
        }
    }

    fn signature(rgb: &[u8]) -> Signature {
        Signature::of(&Pixels::new(rgb, WIDTH, HEIGHT, WIDTH * 3, Layout::RGB).unwrap())
    }

    #[test]
    fn identical_images_match() {
        let image = signature(&screen());
        assert_eq!(image, signature(&screen()));
        assert!(image.looks_like(&image));
        assert_eq!(Signature::from_bytes(&image.to_bytes()), image);
    }

    #[test]
    fn a_clock_ticking_or_a_cursor_blinking_matches() {
        let before = screen();
        // A digit of a 1080p taskbar clock is about 3 by 4 thumbnail pixels.
        let mut digit = before.clone();
        paint(&mut digit, (600, 385, 3, 4), 20);
        assert!(signature(&digit).looks_like(&signature(&before)));
        let mut cursor = before.clone();
        paint(&mut cursor, (100, 50, 1, 6), 0);
        assert!(signature(&cursor).looks_like(&signature(&before)));
        // Even a whole clock turned dark, as no redraw does, changes only the
        // four cells it spans.
        let mut clock = before.clone();
        paint(&mut clock, (590, 375, 24, 8), 20);
        assert!(signature(&clock).looks_like(&signature(&before)));
        // Slight changes everywhere, such as dithering.
        let shifted: Vec<u8> = before.iter().map(|value| value - 4).collect();
        assert!(signature(&shifted).looks_like(&signature(&before)));
    }

    #[test]
    fn a_dialog_a_window_or_the_lock_screen_does_not_match() {
        let before = signature(&screen());
        // A 400 by 150 dialog on a 1080p display, barely brighter.
        let mut dialog = screen();
        paint(&mut dialog, (250, 175, 133, 50), 250);
        assert!(!signature(&dialog).looks_like(&before));
        // A window opened in one corner.
        let mut window = screen();
        paint(&mut window, (0, 0, 200, 120), 40);
        assert!(!signature(&window).looks_like(&before));
        let lock_screen = vec![60; WIDTH * HEIGHT * 3];
        assert!(!signature(&lock_screen).looks_like(&before));
    }

    #[test]
    fn signatures_read_each_pixel_layout_and_any_size() {
        let rgb = screen();
        let bgrx: Vec<u8> = rgb
            .chunks(3)
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 0])
            .collect();
        let pixels = Pixels::new(&bgrx, WIDTH, HEIGHT, WIDTH * 4, Layout::BGRX).unwrap();
        assert_eq!(Signature::of(&pixels), signature(&rgb));
        // Rows may be padded.
        let mut padded = Vec::new();
        for row in rgb.chunks(WIDTH * 3) {
            padded.extend_from_slice(row);
            padded.extend([255; 9]);
        }
        let pixels = Pixels::new(&padded, WIDTH, HEIGHT, WIDTH * 3 + 9, Layout::RGB).unwrap();
        assert_eq!(Signature::of(&pixels), signature(&rgb));
        // Smaller than the grid, as on an ultrawide display.
        let line = vec![128; 640 * 3];
        let pixels = Pixels::new(&line, 640, 1, 640 * 3, Layout::RGB).unwrap();
        assert!(
            Signature::of(&pixels).to_bytes()[..COLUMNS]
                .iter()
                .all(|&cell| cell == 128)
        );
        assert!(Pixels::new(&line, 640, 2, 640 * 3, Layout::RGB).is_err());
        assert!(Pixels::new(&line, 641, 1, 640 * 3, Layout::RGB).is_err());
    }

    #[test]
    fn only_a_changed_capture_is_encoded() {
        let previous = signature(&screen());
        let unchanged = encode_if_changed(previous.clone(), Some(&previous), || {
            panic!("an unchanged capture was encoded")
        });
        assert_eq!(unchanged.unwrap(), Capture::Unchanged);
        // Without an image to compare with, as the first on a connection
        // or hours after the last was sent, it is encoded.
        let first = encode_if_changed(previous.clone(), None, || Ok(vec![0xff, 0xd8])).unwrap();
        assert_eq!(
            first,
            Capture::Changed {
                signature: previous.clone(),
                jpeg: vec![0xff, 0xd8]
            }
        );
        let black = vec![0; WIDTH * HEIGHT * 3];
        let changed = encode_if_changed(signature(&black), Some(&previous), || {
            Ok(vec![0; MAX_BYTES + 1])
        });
        assert!(changed.is_err(), "too large for the server");
    }
}
