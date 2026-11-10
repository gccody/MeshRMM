//! A small image of the console's main display, uploaded for the dashboard
//! every few minutes.
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
use std::time::Duration;

use anyhow::ensure;
use serde::{Deserialize, Serialize};

/// How often the dashboard's image is refreshed, on average.
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
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
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use meshrmm_signaling_client::random_fraction;
    use tokio::task::JoinHandle;

    use super::platform::capture_for;
    use super::{Capture, INTERVAL, Signature};
    use crate::remote::config::{Config, ExecutionMode};

    const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
    /// After a failure, such as when the Agent starts before the session it
    /// captures, the next few attempts come sooner than [`INTERVAL`].
    const RETRY: Duration = Duration::from_secs(30);
    const QUICK_RETRIES: u32 = 4;
    /// The first capture after the Agent starts falls within this, so a newly
    /// enrolled device shows its screen soon, but a fleet that restarts
    /// together does not upload in the same second.
    const STARTUP_SPREAD: Duration = Duration::from_secs(60);
    /// Each interval is up to this fraction longer or shorter than
    /// [`INTERVAL`]. The random phase each connection starts with is what
    /// spreads a fleet out; this only keeps it from falling back into step.
    /// Kept narrow so a new image still arrives before the dashboard's check,
    /// 20 seconds after the next one is due.
    const JITTER: f64 = 0.05;
    /// A capture that looks like the last uploaded image is skipped at most
    /// this many times in a row, about an hour's worth, and then sent anyway:
    /// a change too small to notice is shown within the hour, and a server
    /// that lost the image, as when restored from a backup, gets it back.
    const MAX_SKIPPED: u32 = 11;

    /// Refreshes the device's thumbnail about every [`INTERVAL`] while the
    /// Agent is connected, when the screen changed. One capture and upload
    /// runs at a time, off the signaling task.
    pub struct Thumbnails {
        mode: ExecutionMode,
        /// When the next capture is due.
        next: Instant,
        task: Option<JoinHandle<()>>,
        state: Arc<Mutex<State>>,
        /// One client for the life of the process: the TLS configuration it
        /// builds on the first upload keeps the session cache that lets the
        /// next upload resume the TLS session instead of a full handshake.
        http: ureq::Agent,
    }

    #[derive(Default)]
    struct State {
        /// What the last image the server accepted looks like.
        uploaded: Option<Signature>,
        /// Captures since then that looked the same, so were not sent.
        skipped: u32,
        /// Repeated failures, such as while no display is attached, are logged once.
        failing: bool,
        /// Failures in a row, and when the last one happened.
        failures: u32,
        failed_at: Option<Instant>,
    }

    impl State {
        /// When a quick retry is due, if one is.
        fn retry_at(&self) -> Option<Instant> {
            (self.failures <= QUICK_RETRIES)
                .then_some(self.failed_at)
                .flatten()
                .map(|failed| failed + RETRY)
        }

        /// What a new capture is compared with: nothing, so it is sent,
        /// before the first upload and after [`MAX_SKIPPED`] skipped captures.
        fn baseline(&self) -> Option<Signature> {
            self.uploaded.clone().filter(|_| self.skipped < MAX_SKIPPED)
        }

        /// Records a capture, and returns the image to send, if any. It is
        /// remembered as uploaded only once the server accepts it.
        fn captured(&mut self, capture: Capture) -> Option<(Signature, Vec<u8>)> {
            match capture {
                Capture::Unchanged => {
                    self.skipped = self.skipped.saturating_add(1);
                    None
                }
                Capture::Changed { signature, jpeg } => Some((signature, jpeg)),
            }
        }

        fn uploaded(&mut self, signature: Signature) {
            self.uploaded = Some(signature);
            self.skipped = 0;
        }
    }

    impl Thumbnails {
        pub fn new(mode: ExecutionMode) -> Self {
            Self {
                mode,
                next: Instant::now() + random_part(STARTUP_SPREAD),
                task: None,
                state: Arc::default(),
                http: http_client(),
            }
        }

        /// Called on each connection to the server. A capture that fell due
        /// while the Agent was offline is put at a random point of the next
        /// interval, so a fleet reconnecting after an outage does not upload
        /// together then and every five minutes after.
        pub fn connected(&mut self) {
            let now = Instant::now();
            if self.next <= now {
                self.next = now + random_part(INTERVAL);
            }
        }

        /// Completes when the next thumbnail is due.
        pub async fn due(&mut self) {
            loop {
                let now = Instant::now();
                if self.next <= now {
                    self.next = now + jittered_interval();
                    return;
                }
                let retry_at = {
                    let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                    state.retry_at()
                };
                if retry_at.is_some_and(|at| at <= now) {
                    self.state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .failed_at = None;
                    return;
                }
                // A capture still running can fail and ask for a retry, so
                // this looks again at least once a second.
                let wake = retry_at
                    .map_or(self.next, |at| at.min(self.next))
                    .saturating_duration_since(now)
                    .min(Duration::from_secs(1));
                tokio::time::sleep(wake).await;
            }
        }

        /// Starts a capture and upload unless one is still running.
        pub fn refresh(&mut self, config: &Config) {
            if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
                return;
            }
            let (mode, config, state, http) = (
                self.mode,
                config.clone(),
                Arc::clone(&self.state),
                self.http.clone(),
            );
            self.task = Some(tokio::task::spawn_blocking(move || {
                let result = upload_if_changed(mode, &config, &http, &state);
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                let Err(error) = result.map(|sent| {
                    tracing::debug!(sent, "refreshed the screen thumbnail");
                }) else {
                    state.failures = 0;
                    state.failed_at = None;
                    if std::mem::take(&mut state.failing) {
                        tracing::info!("screen thumbnails are updating again");
                    }
                    return;
                };
                state.failures = state.failures.saturating_add(1);
                state.failed_at = Some(Instant::now());
                if std::mem::replace(&mut state.failing, true) {
                    tracing::debug!(error = ?error, "screen thumbnail still unavailable");
                } else {
                    tracing::warn!(error = ?error, "could not refresh the screen thumbnail; retrying");
                }
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

    /// `of` scaled by a random fraction in `[0, 1)`.
    fn random_part(of: Duration) -> Duration {
        of.mul_f64(random_fraction().unwrap_or(0.5))
    }

    /// [`INTERVAL`], up to [`JITTER`] longer or shorter at random.
    fn jittered_interval() -> Duration {
        INTERVAL.mul_f64(1.0 - JITTER + 2.0 * JITTER * random_fraction().unwrap_or(0.5))
    }

    /// Returns whether an image was sent: one that looks like the last
    /// upload is not, and isn't even encoded.
    fn upload_if_changed(
        mode: ExecutionMode,
        config: &Config,
        http: &ureq::Agent,
        state: &Mutex<State>,
    ) -> anyhow::Result<bool> {
        let lock = || state.lock().unwrap_or_else(|error| error.into_inner());
        let baseline = lock().baseline();
        let capture = capture_for(mode, baseline.as_ref())?;
        let Some((signature, jpeg)) = lock().captured(capture) else {
            return Ok(false);
        };
        upload(config, http, &jpeg)?;
        lock().uploaded(signature);
        Ok(true)
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
        use super::*;

        fn signature(brightness: u8) -> Signature {
            Signature::from_bytes(&[brightness; Signature::BYTES])
        }

        #[test]
        fn failures_retry_soon_a_few_times_then_wait_for_the_interval() {
            let failed = Instant::now();
            let mut state = State::default();
            assert_eq!(state.retry_at(), None);
            state.failed_at = Some(failed);
            for failures in 1..=QUICK_RETRIES {
                state.failures = failures;
                assert_eq!(state.retry_at(), Some(failed + RETRY));
            }
            state.failures = QUICK_RETRIES + 1;
            assert_eq!(state.retry_at(), None);
        }

        #[test]
        fn an_unchanged_screen_is_sent_again_after_about_an_hour() {
            let mut state = State::default();
            // The first capture is always sent.
            assert_eq!(state.baseline(), None);
            state.uploaded(signature(40));
            for _ in 0..MAX_SKIPPED {
                assert_eq!(state.baseline(), Some(signature(40)));
                assert_eq!(state.captured(Capture::Unchanged), None);
            }
            assert_eq!(state.baseline(), None);
            assert!(INTERVAL * (MAX_SKIPPED + 1) <= Duration::from_secs(60 * 60));
            // Sending it starts the count again.
            state.uploaded(signature(41));
            assert_eq!(state.baseline(), Some(signature(41)));
        }

        #[test]
        fn a_failed_upload_is_not_remembered_as_uploaded() {
            let mut state = State::default();
            state.uploaded(signature(40));
            state.skipped = 3;
            let changed = Capture::Changed {
                signature: signature(200),
                jpeg: vec![0xff, 0xd8],
            };
            assert_eq!(
                state.captured(changed),
                Some((signature(200), vec![0xff, 0xd8]))
            );
            // Until the server accepts it, captures still compare with the
            // image it has.
            assert_eq!(state.baseline(), Some(signature(40)));
            assert_eq!(state.skipped, 3);
        }

        #[test]
        fn intervals_average_about_the_interval() {
            let low = INTERVAL.mul_f64(1.0 - JITTER);
            let high = INTERVAL.mul_f64(1.0 + JITTER);
            for _ in 0..100 {
                assert!((low..=high).contains(&jittered_interval()));
                assert!(random_part(STARTUP_SPREAD) < STARTUP_SPREAD);
            }
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
        // Without an image to compare with, as when the last was sent an
        // hour ago, it is encoded.
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
