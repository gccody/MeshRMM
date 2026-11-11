//! Desktop-region capture uses GDI so rotated monitors and displays on different GPUs
//! share one physical desktop coordinate space. Encoding is unaffected.
//!
//! GDI reports no damage, so each capture is compared with the one before it.
//! An unchanged capture is not uploaded, converted or encoded, as when Desktop
//! Duplication reports no change, a changed one uploads only the rows that
//! changed, and a desktop that stays unchanged is polled at a reduced rate.
use std::ffi::c_void;
use std::ops::Range;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::*;

use crate::{ALL_MONITORS_ID, DisplayInfo, Error, enumerate_displays};

/// How long the desktop stays unchanged before it is polled at the idle rate.
const IDLE_AFTER: Duration = Duration::from_secs(1);
/// The poll interval of an unchanged desktop, and so the longest a first
/// change after it waits to be captured. GDI's copy of the screen costs a
/// changed and an unchanged desktop alike.
const IDLE_INTERVAL: Duration = Duration::from_millis(200);

pub(crate) struct DesktopCapture {
    background: Option<crate::background::Desktop>,
    renderer: crate::background::Renderer,
    displays: Vec<DisplayInfo>,
    bounds: DisplayInfo,
    screen: HDC,
    memory: HDC,
    previous: HGDIOBJ,
    buffers: CaptureBuffers<Dib>,
    texture: ID3D11Texture2D,
    pacing: CapturePacing,
    next_layout_check: Instant,
}

impl DesktopCapture {
    pub fn new(device: &ID3D11Device, fps: u32, display_id: u32) -> Result<Self, Error> {
        let background = (display_id == crate::background::DISPLAY_ID)
            .then(crate::background::Desktop::bind)
            .transpose()?;
        let displays = if background.is_some() {
            vec![crate::background::display()]
        } else {
            enumerate_displays()?
        };
        let mut bounds = displays
            .iter()
            .find(|d| d.id == display_id)
            .cloned()
            .ok_or(Error::InvalidDisplayDimensions)?;
        bounds.width = bounds
            .width
            .checked_add(1)
            .ok_or(Error::InvalidDisplayDimensions)?
            & !1;
        bounds.height = bounds
            .height
            .checked_add(1)
            .ok_or(Error::InvalidDisplayDimensions)?
            & !1;
        if bounds.width > 16384 || bounds.height > 16384 {
            return Err(Error::InvalidDisplayDimensions);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: bounds.width,
            Height: bounds.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // VideoProcessorInputView requires a video-compatible binding.
            // A shader-resource-only GDI upload texture is rejected by some GPUs.
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        }
        let texture = texture.ok_or(Error::InvalidDisplayDimensions)?;
        // Construct the owner before allocating GDI resources so every failure
        // releases the resources already acquired, on this same capture thread.
        let mut capture = Self {
            background,
            renderer: crate::background::Renderer::default(),
            displays,
            bounds,
            screen: HDC::default(),
            memory: HDC::default(),
            previous: HGDIOBJ::default(),
            buffers: CaptureBuffers::new(Dib::default(), Dib::default(), 1),
            texture,
            pacing: CapturePacing::new(fps, Instant::now()),
            next_layout_check: Instant::now(),
        };
        unsafe {
            capture.screen = GetDC(None);
            if capture.screen.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            capture.memory = CreateCompatibleDC(Some(capture.screen));
            if capture.memory.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            let first = Dib::new(capture.screen, capture.width(), capture.height())?;
            let second = Dib::new(capture.screen, capture.width(), capture.height())?;
            capture.buffers = CaptureBuffers::new(first, second, capture.width() as usize * 4);
            capture.previous = SelectObject(capture.memory, capture.buffers.target().object());
            if capture.previous.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
        }
        Ok(capture)
    }

    /// Matches the capture rate to the encoder's, which a software encoder
    /// caps below the requested one.
    pub fn set_frames_per_second(&mut self, fps: u32) {
        self.pacing.set_frames_per_second(fps);
    }

    pub fn width(&self) -> u32 {
        self.bounds.width
    }
    pub fn height(&self) -> u32 {
        self.bounds.height
    }

    /// Captures the desktop when a capture is due. Returns whether it
    /// changed; a changed capture is uploaded and pending until
    /// [`Self::submitted`].
    pub fn capture(
        &mut self,
        context: &ID3D11DeviceContext,
        capture_cursor: bool,
    ) -> Result<bool, Error> {
        let now = Instant::now();
        if !self.pacing.due(now) {
            return Ok(false);
        }
        if self.background.is_none() && now >= self.next_layout_check {
            if enumerate_displays()? != self.displays {
                return Err(Error::DesktopDuplication(
                    "desktop monitor layout changed".into(),
                ));
            }
            self.next_layout_check = now + Duration::from_secs(1);
        }
        unsafe {
            // Clear gaps and padding, then copy each physical display. GDI
            // handles rotation and adapter differences before the GPU upload.
            PatBlt(
                self.memory,
                0,
                0,
                self.width() as i32,
                self.height() as i32,
                BLACKNESS,
            )
            .ok()?;
            if self.background.is_some() {
                self.renderer.paint(self.memory)?;
            }
            for display in self.displays.iter().filter(|d| {
                self.background.is_none()
                    && d.id != ALL_MONITORS_ID
                    && (self.bounds.id == ALL_MONITORS_ID || d.id == self.bounds.id)
            }) {
                BitBlt(
                    self.memory,
                    display.x - self.bounds.x,
                    display.y - self.bounds.y,
                    display.width as i32,
                    display.height as i32,
                    Some(self.screen),
                    display.x,
                    display.y,
                    SRCCOPY | CAPTUREBLT,
                )?;
            }
            // The pointer is drawn into the capture, so its movement is a change.
            if capture_cursor && self.background.is_none() {
                crate::cursor::draw_cursor(self.memory, self.bounds.x, self.bounds.y)?;
            }
            GdiFlush().ok()?;
        }
        let changed = self.buffers.take();
        self.pacing.captured(now, changed.is_some());
        let Some(rows) = changed else {
            return Ok(false);
        };
        unsafe {
            if SelectObject(self.memory, self.buffers.target().object()).is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            // The texture holds the previous capture, so only the rows
            // that changed are uploaded.
            let pitch = self.width() * 4;
            context.UpdateSubresource(
                &self.texture,
                0,
                Some(&D3D11_BOX {
                    left: 0,
                    top: rows.start as u32,
                    front: 0,
                    right: self.width(),
                    bottom: rows.end as u32,
                    back: 1,
                }),
                self.buffers
                    .latest()
                    .pixels
                    .add(rows.start * pitch as usize)
                    .cast_const()
                    .cast::<c_void>(),
                pitch,
                0,
            );
        }
        Ok(true)
    }

    /// The latest capture, while the encoder has not taken it.
    pub fn pending_texture(&self) -> Option<&ID3D11Texture2D> {
        self.buffers.pending().then_some(&self.texture)
    }

    /// The encoder took the pending capture.
    pub fn submitted(&mut self) {
        self.buffers.submitted();
    }

    /// A new encoder starts with the latest capture, whether or not the
    /// desktop changes.
    pub fn stream_started(&mut self) {
        self.buffers.resend();
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.memory, self.previous);
            }
            if !self.memory.is_invalid() {
                let _ = DeleteDC(self.memory);
            }
            if !self.screen.is_invalid() {
                ReleaseDC(None, self.screen);
            }
        }
        // The buffers' bitmaps, no longer selected, are deleted with them.
    }
}

/// A top-down 32-bit DIB section a capture is drawn into.
struct Dib {
    bitmap: HBITMAP,
    pixels: *mut u8,
    len: usize,
}

impl Default for Dib {
    fn default() -> Self {
        Self {
            bitmap: HBITMAP::default(),
            pixels: std::ptr::null_mut(),
            len: 0,
        }
    }
}

impl Dib {
    fn new(screen: HDC, width: u32, height: u32) -> Result<Self, Error> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        let bitmap =
            unsafe { CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut pixels, None, 0)? };
        let dib = Self {
            bitmap,
            pixels: pixels.cast(),
            len: width as usize * height as usize * 4,
        };
        if dib.pixels.is_null() {
            return Err(windows::core::Error::from_thread().into());
        }
        Ok(dib)
    }

    fn object(&self) -> HGDIOBJ {
        HGDIOBJ(self.bitmap.0)
    }
}

impl AsRef<[u8]> for Dib {
    fn as_ref(&self) -> &[u8] {
        if self.pixels.is_null() {
            return &[];
        }
        // SAFETY: the section holds `len` bytes until the bitmap is deleted,
        // and GDI is flushed before a capture is read.
        unsafe { std::slice::from_raw_parts(self.pixels, self.len) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        if !self.bitmap.is_invalid() {
            unsafe {
                let _ = DeleteObject(self.object());
            }
        }
    }
}

/// Two capture buffers swapped rather than copied: the target the next
/// capture is drawn into, and the latest capture it is compared with.
pub(crate) struct CaptureBuffers<T> {
    buffers: [T; 2],
    row: usize,
    target: usize,
    /// Whether the other buffer holds a capture.
    captured: bool,
    /// Whether the latest capture has yet to be submitted.
    pending: bool,
}

impl<T: AsRef<[u8]>> CaptureBuffers<T> {
    /// Buffers of rows of `row` bytes.
    pub fn new(first: T, second: T, row: usize) -> Self {
        Self {
            buffers: [first, second],
            row: row.max(1),
            target: 0,
            captured: false,
            pending: false,
        }
    }

    pub fn target(&self) -> &T {
        &self.buffers[self.target]
    }

    pub fn latest(&self) -> &T {
        &self.buffers[self.target ^ 1]
    }

    /// Takes the capture drawn into the target. Returns the rows from the
    /// first to the last that differ from the latest capture, all of them
    /// for the first; the capture then becomes the latest and pending, and
    /// the old latest becomes the target. An unchanged capture costs one
    /// pass over both, a changed one at most that.
    pub fn take(&mut self) -> Option<Range<usize>> {
        let (target, latest) = (self.target().as_ref(), self.latest().as_ref());
        let changed = if self.captured {
            let pairs = || target.chunks(self.row).zip(latest.chunks(self.row));
            let first = pairs().position(|(a, b)| a != b)?;
            let last = pairs().rposition(|(a, b)| a != b).unwrap_or(first);
            first..last + 1
        } else {
            0..target.len().div_ceil(self.row)
        };
        self.target ^= 1;
        self.captured = true;
        self.pending = true;
        Some(changed)
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    pub fn submitted(&mut self) {
        self.pending = false;
    }

    /// Makes the latest capture, if any, pending again.
    pub fn resend(&mut self) {
        self.pending = self.captured;
    }
}

/// When to capture: at the stream rate, and at [`IDLE_INTERVAL`] once the
/// desktop has stayed unchanged for [`IDLE_AFTER`]. A change restores the
/// stream rate at once.
pub(crate) struct CapturePacing {
    interval: Duration,
    next: Instant,
    changed_at: Instant,
}

impl CapturePacing {
    pub fn new(fps: u32, now: Instant) -> Self {
        Self {
            interval: frame_interval(fps),
            next: now,
            changed_at: now,
        }
    }

    pub fn set_frames_per_second(&mut self, fps: u32) {
        self.interval = frame_interval(fps);
    }

    pub fn due(&self, now: Instant) -> bool {
        now >= self.next
    }

    /// Records a capture started at `now` and schedules the next one.
    pub fn captured(&mut self, now: Instant, changed: bool) {
        if changed {
            self.changed_at = now;
        }
        self.next = now + self.interval_at(now);
    }

    fn interval_at(&self, now: Instant) -> Duration {
        if now.saturating_duration_since(self.changed_at) >= IDLE_AFTER {
            self.interval.max(IDLE_INTERVAL)
        } else {
            self.interval
        }
    }
}

fn frame_interval(fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three rows of 64 pixels.
    const ROW: usize = 64 * 4;
    const LEN: usize = ROW * 3;

    fn buffers() -> CaptureBuffers<Vec<u8>> {
        CaptureBuffers::new(vec![0; LEN], vec![0; LEN], ROW)
    }

    /// Draws `image` into the target, as a capture does, and takes it.
    fn take(buffers: &mut CaptureBuffers<Vec<u8>>, image: &[u8]) -> Option<Range<usize>> {
        let target = buffers.target;
        buffers.buffers[target].copy_from_slice(image);
        buffers.take()
    }

    fn draw(buffers: &mut CaptureBuffers<Vec<u8>>, image: &[u8]) -> bool {
        take(buffers, image).is_some()
    }

    #[test]
    fn an_identical_capture_is_unchanged_and_any_changed_byte_is_a_change() {
        let image = vec![0x5A; LEN];
        // The first and last byte of the first row, the last row, the last byte.
        for (index, row) in [(0, 0), (ROW - 1, 0), (LEN - ROW + 5, 2), (LEN - 1, 2)] {
            let mut buffers = buffers();
            assert_eq!(take(&mut buffers, &image), Some(0..3), "the first capture");
            assert_eq!(take(&mut buffers, &image), None);
            assert_eq!(take(&mut buffers, &image), None);
            let mut changed = image.clone();
            changed[index] ^= 1;
            assert_eq!(take(&mut buffers, &changed), Some(row..row + 1), "{index}");
            assert_eq!(take(&mut buffers, &changed), None, "byte {index}");
            assert_eq!(take(&mut buffers, &image), Some(row..row + 1), "{index}");
        }
    }

    #[test]
    fn the_changed_rows_span_the_first_to_the_last_difference() {
        let image = vec![7; LEN];
        let mut buffers = buffers();
        assert!(draw(&mut buffers, &image));
        let mut changed = image.clone();
        changed[3] ^= 1;
        changed[2 * ROW + 9] ^= 1;
        assert_eq!(take(&mut buffers, &changed), Some(0..3));
        changed[2 * ROW + 9] ^= 1;
        changed[ROW + 4] ^= 1;
        assert_eq!(take(&mut buffers, &changed), Some(1..3));
    }

    #[test]
    fn a_change_swaps_the_buffers_without_copying() {
        let mut buffers = buffers();
        let image = vec![1; LEN];
        let first = buffers.target().as_ptr();
        let second = buffers.latest().as_ptr();
        assert!(draw(&mut buffers, &image));
        assert_eq!(buffers.latest().as_ptr(), first);
        assert_eq!(buffers.target().as_ptr(), second);
        assert_eq!(buffers.latest(), &image);
        // An unchanged capture keeps the latest one in its buffer.
        assert!(!draw(&mut buffers, &image));
        assert_eq!(buffers.latest().as_ptr(), first);
        assert_eq!(buffers.target().as_ptr(), second);
        assert!(draw(&mut buffers, &[2; LEN]));
        assert_eq!(buffers.latest().as_ptr(), second);
        assert_eq!(buffers.target().as_ptr(), first);
        assert_eq!(buffers.latest(), &[2; LEN]);
    }

    #[test]
    fn a_change_stays_pending_until_submitted_and_a_new_stream_resends_it() {
        let mut buffers = buffers();
        buffers.resend();
        assert!(!buffers.pending(), "nothing captured to resend");
        let image = vec![3; LEN];
        assert!(draw(&mut buffers, &image));
        // Unchanged captures while the encoder is busy keep it pending.
        assert!(!draw(&mut buffers, &image));
        assert!(buffers.pending());
        buffers.submitted();
        assert!(!draw(&mut buffers, &image));
        assert!(!buffers.pending(), "an unchanged desktop submits nothing");
        // A restarted encoder starts with the image already held.
        buffers.resend();
        assert!(buffers.pending());
        assert_eq!(buffers.latest(), &image);
    }

    #[test]
    fn an_unchanged_desktop_is_polled_at_the_idle_rate_until_it_changes() {
        let start = Instant::now();
        let mut pacing = CapturePacing::new(30, start);
        let frame = frame_interval(30);
        let just_before = |at: Instant| at - Duration::from_millis(1);
        assert!(pacing.due(start));
        let mut now = start;
        pacing.captured(now, true);
        // At the stream rate until the desktop has been unchanged for a second.
        while now.duration_since(start) < IDLE_AFTER {
            assert!(!pacing.due(just_before(now + frame)));
            now += frame;
            assert!(pacing.due(now));
            pacing.captured(now, false);
        }
        for _ in 0..3 {
            assert!(!pacing.due(just_before(now + IDLE_INTERVAL)));
            now += IDLE_INTERVAL;
            assert!(pacing.due(now));
            pacing.captured(now, false);
        }
        // A change restores the stream rate at once.
        pacing.captured(now, true);
        assert!(!pacing.due(just_before(now + frame)));
        assert!(pacing.due(now + frame));
        now += frame;
        pacing.captured(now, false);
        assert!(pacing.due(now + frame));
    }

    #[test]
    fn a_stream_rate_below_the_idle_rate_is_kept() {
        let start = Instant::now();
        let mut pacing = CapturePacing::new(2, start);
        let later = start + Duration::from_secs(5);
        pacing.captured(later, false);
        assert!(!pacing.due(later + Duration::from_millis(499)));
        assert!(pacing.due(later + Duration::from_millis(500)));
    }
}
