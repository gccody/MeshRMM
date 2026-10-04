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
use std::time::Duration;

/// How often the dashboard's image is refreshed.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
/// The image fits in this box. It is sharp at the dashboard's preview size
/// and small enough to be a few dozen KiB as a JPEG.
pub const MAX_WIDTH: u32 = 640;
pub const MAX_HEIGHT: u32 = 400;
/// The server refuses anything larger.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub const MAX_BYTES: usize = 512 * 1024;

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

#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(any(windows, target_os = "macos"))]
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

    use super::{MAX_BYTES, scaled_size};
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

    /// Captures the primary display, scaled to the thumbnail size, as a JPEG.
    pub fn capture_primary_display() -> anyhow::Result<Vec<u8>> {
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
        let jpeg = encode_jpeg(&pixels, scaled_width, scaled_height)?;
        anyhow::ensure!(
            jpeg.len() <= MAX_BYTES,
            "the {} byte thumbnail is larger than {MAX_BYTES} bytes",
            jpeg.len()
        );
        Ok(jpeg)
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

    pub(super) fn capture_for(mode: ExecutionMode) -> anyhow::Result<Vec<u8>> {
        match mode {
            // Local development runs on the developer's own desktop.
            ExecutionMode::Console => capture_primary_display(),
            _ => crate::remote::capture_helper::capture_thumbnail(),
        }
    }
}

/// The schedule and upload, around each platform's capture.
#[cfg(any(windows, target_os = "macos"))]
mod schedule {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use tokio::task::JoinHandle;
    use tokio::time::{Interval, MissedTickBehavior, interval};

    use super::INTERVAL;
    use super::platform::capture_for;
    use crate::remote::config::{Config, ExecutionMode};

    const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
    /// After a failure, such as when the Agent starts before the session it
    /// captures, the next few attempts come sooner than [`INTERVAL`].
    const RETRY: Duration = Duration::from_secs(30);
    const QUICK_RETRIES: u32 = 4;

    /// Refreshes the device's thumbnail every [`INTERVAL`] while the Agent is
    /// connected. One capture and upload runs at a time, off the signaling task.
    pub struct Thumbnails {
        mode: ExecutionMode,
        timer: Interval,
        task: Option<JoinHandle<()>>,
        state: Arc<Mutex<State>>,
    }

    #[derive(Default)]
    struct State {
        /// The last image the server accepted. An identical capture is not sent again.
        uploaded: Option<Vec<u8>>,
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
    }

    impl Thumbnails {
        pub fn new(mode: ExecutionMode) -> Self {
            // The first tick completes at once, so a device that comes online
            // shows its screen without waiting a full interval.
            let mut timer = interval(INTERVAL);
            timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
            Self {
                mode,
                timer,
                task: None,
                state: Arc::default(),
            }
        }

        /// Completes when the next thumbnail is due.
        pub async fn due(&mut self) {
            loop {
                let retry_at = {
                    let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                    state.retry_at()
                };
                if retry_at.is_some_and(|at| at <= Instant::now()) {
                    self.state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .failed_at = None;
                    return;
                }
                // A capture still running can fail and ask for a retry, so
                // this looks again at least once a second.
                let wake = retry_at.map_or(Duration::from_secs(1), |at| {
                    at.saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(1))
                });
                tokio::select! {
                    _ = self.timer.tick() => return,
                    () = tokio::time::sleep(wake) => {}
                }
            }
        }

        /// Starts a capture and upload unless one is still running.
        pub fn refresh(&mut self, config: &Config) {
            if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
                return;
            }
            let (mode, config, state) = (self.mode, config.clone(), Arc::clone(&self.state));
            self.task = Some(tokio::task::spawn_blocking(move || {
                let result =
                    capture_for(mode).and_then(|jpeg| upload_if_changed(&config, &state, jpeg));
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

    /// Returns whether the image was sent: an unchanged screen is not.
    fn upload_if_changed(
        config: &Config,
        state: &Mutex<State>,
        jpeg: Vec<u8>,
    ) -> anyhow::Result<bool> {
        let unchanged = state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .uploaded
            .as_deref()
            == Some(jpeg.as_slice());
        if unchanged {
            return Ok(false);
        }
        upload(config, &jpeg)?;
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .uploaded = Some(jpeg);
        Ok(true)
    }

    fn upload(config: &Config, jpeg: &[u8]) -> anyhow::Result<()> {
        let url = meshrmm_signaling_client::endpoint_url(
            &config.server,
            &["v1", "agents", &config.device_id, "thumbnail"],
            &[],
            false,
        )?;
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(UPLOAD_TIMEOUT))
            .http_status_as_error(true)
            .tls_config(crate::enrollment::https_tls_config())
            .build()
            .new_agent();
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
    }
}

/// A console Agent captures its own session; the installed coordinator asks
/// the console's session helper, which captures the login window too.
#[cfg(target_os = "macos")]
mod macos {
    use crate::remote::config::ExecutionMode;

    pub(super) fn capture_for(mode: ExecutionMode) -> anyhow::Result<Vec<u8>> {
        match mode {
            ExecutionMode::Console => crate::remote::macos::snapshot::main_display_jpeg(),
            _ => crate::remote::macos::helper::coordinator::registry()?.thumbnail(),
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
}
