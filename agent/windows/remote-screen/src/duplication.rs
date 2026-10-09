use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, DxgiDuplicationFrame, Error as DuplicationError,
};
use windows_capture::monitor::Monitor;

use crate::cursor::CursorCompositor;
use crate::desktop::DesktopCapture;
use crate::encoder::VideoEncoder;
use crate::refinement::StaticRefinement;
use crate::software::{Converter, Encoder, Pipeline, PipelineConfig};
use crate::{
    ActiveFormat, ControlState, EncodedAccessUnit, EncodedFrameSink, Error, FramePacer,
    StreamConfig, monotonic_timestamp_us,
};

// Capture and the asynchronous MFT share a protected D3D11 device. A blocking
// AcquireNextFrame can hold the driver's device lock while waiting for desktop
// damage, starving the encoder's worker. Poll DXGI without blocking and wait
// outside the graphics driver so encoding can progress independently.
const ACQUIRE_TIMEOUT_MS: u32 = 0;
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const START_TIMEOUT: Duration = Duration::from_secs(5);

type CaptureStatus = Arc<Mutex<Option<Result<(), String>>>>;

/// GPU capture built on DXGI Desktop Duplication.
///
/// Unlike Windows.Graphics.Capture, Desktop Duplication reports
/// `DXGI_ERROR_ACCESS_LOST` when Windows changes the visible desktop. A
/// LocalSystem caller can then recreate this streamer on `winsta0\\default` or
/// `winsta0\\Winlogon` without tearing down the remote transport.
pub struct WindowsDesktopDuplicationStreamer {
    running: Option<RunningCapture>,
    controls: Arc<ControlState>,
}

impl WindowsDesktopDuplicationStreamer {
    pub fn new() -> Self {
        Self {
            running: None,
            controls: Arc::new(ControlState::default()),
        }
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: u32,
        sink: EncodedFrameSink,
    ) -> Result<ActiveFormat, Error> {
        if self.running.is_some() {
            return Err(Error::AlreadyRunning);
        }
        // The static media type already contains this start's bitrate. Do not
        // replay a runtime request left behind by the previous encoder.
        self.controls
            .capture_cursor
            .store(config.capture_cursor, Ordering::Release);
        self.controls.requested_bitrate.store(0, Ordering::Release);
        self.controls
            .runtime_bitrate_disabled
            .store(false, Ordering::Release);
        self.controls
            .request_keyframe
            .store(false, Ordering::Release);
        let monitor = if display_id == crate::background::DISPLAY_ID {
            None
        } else {
            Some(
                Monitor::enumerate()
                    .map_err(|error| Error::DesktopDuplication(error.to_string()))?
                    .into_iter()
                    .find(|monitor| {
                        display_id == crate::ALL_MONITORS_ID
                            || monitor.index().is_ok_and(|id| id == display_id as usize)
                    })
                    .ok_or_else(|| {
                        Error::DesktopDuplication(format!("display {display_id} is unavailable"))
                    })?,
            )
        };
        let stop = Arc::new(AtomicBool::new(false));
        let status: CaptureStatus = Arc::new(Mutex::new(None));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread_stop = Arc::clone(&stop);
        let thread_status = Arc::clone(&status);
        let controls = Arc::clone(&self.controls);
        let worker = thread::Builder::new()
            .name("meshrmm-desktop-duplication".into())
            .spawn(move || {
                let result = capture_loop(
                    monitor,
                    display_id,
                    config,
                    sink,
                    controls,
                    thread_stop,
                    started_tx,
                );
                if let Err(error) = &result {
                    tracing::warn!(display_id, %error, "desktop capture worker stopped");
                }
                let mut status = thread_status
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                *status = Some(result.map_err(|error| error.to_string()));
            })
            .map_err(|error| Error::DesktopDuplication(error.to_string()))?;

        let format = match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(format)) => format,
            Ok(Err(message)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                return Err(Error::DesktopDuplication(message));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                return Err(Error::DesktopDuplication(
                    "capture did not initialize within 5 seconds".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                return Err(Error::DesktopDuplication(
                    "capture worker exited during initialization".into(),
                ));
            }
        };
        self.running = Some(RunningCapture {
            stop,
            status,
            worker: Some(worker),
        });
        Ok(format)
    }

    pub fn set_cursor_capture(&self, enabled: bool) {
        self.controls
            .capture_cursor
            .store(enabled, Ordering::Release);
    }

    pub fn request_keyframe(&self) -> Result<(), Error> {
        if self.running.is_none() {
            return Err(Error::NotRunning);
        }
        self.controls
            .request_keyframe
            .store(true, Ordering::Release);
        Ok(())
    }

    pub fn set_bitrate(&self, bits_per_second: u32) -> Result<(), Error> {
        if self.running.is_none() {
            return Err(Error::NotRunning);
        }
        if self
            .controls
            .runtime_bitrate_disabled
            .load(Ordering::Acquire)
        {
            return Ok(());
        }
        self.controls
            .requested_bitrate
            .store(bits_per_second.max(1), Ordering::Release);
        Ok(())
    }

    pub fn poll_ended(&mut self) -> Option<Result<(), Error>> {
        let running = self.running.as_ref()?;
        if running
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
        {
            return None;
        }
        let mut running = self.running.take()?;
        let result = running.take_status();
        running.join();
        Some(result.map_err(Error::DesktopDuplication))
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        running.stop.store(true, Ordering::Release);
        running.join();
        Ok(())
    }
}

impl Default for WindowsDesktopDuplicationStreamer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WindowsDesktopDuplicationStreamer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(%error, "failed to stop Desktop Duplication cleanly");
        }
    }
}

struct RunningCapture {
    stop: Arc<AtomicBool>,
    status: CaptureStatus,
    worker: Option<JoinHandle<()>>,
}

impl RunningCapture {
    fn take_status(&self) -> Result<(), String> {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .unwrap_or(Ok(()))
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn capture_loop(
    monitor: Option<Monitor>,
    display_id: u32,
    config: StreamConfig,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
    stop: Arc<AtomicBool>,
    started: mpsc::SyncSender<Result<ActiveFormat, String>>,
) -> Result<(), Error> {
    let mut started = Some(started);
    let result = capture_loop_inner(
        monitor,
        display_id,
        config,
        sink,
        controls,
        stop,
        &mut started,
    );
    if let Some(started) = started.take() {
        let _ = started.send(Err(match &result {
            Ok(()) => "capture stopped before initialization".into(),
            Err(error) => error.to_string(),
        }));
    }
    result
}

fn capture_loop_inner(
    monitor: Option<Monitor>,
    display_id: u32,
    config: StreamConfig,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
    stop: Arc<AtomicBool>,
    started: &mut Option<mpsc::SyncSender<Result<ActiveFormat, String>>>,
) -> Result<(), Error> {
    let CaptureSources {
        mut duplication,
        device,
        context,
        mut desktop,
        cursor_compositor,
        width,
        height,
    } = open_sources(monitor, display_id, &config)?;
    let Pipeline {
        converter,
        encoder,
        frames_per_second,
    } = crate::software::pipeline(
        &device,
        &context,
        &PipelineConfig {
            width,
            height,
            frames_per_second: config.frames_per_second,
            bitrate_bits_per_second: config.bitrate_bits_per_second,
            codec: config.codec,
            pixel_format: config.pixel_format,
            grayscale: config.grayscale,
        },
    )?;
    if let Some(desktop) = desktop.as_mut() {
        desktop.set_frames_per_second(frames_per_second);
    }
    let format = ActiveFormat {
        width,
        height,
        frames_per_second,
        bitrate_bits_per_second: config.bitrate_bits_per_second,
        codec: config.codec,
        pixel_format: config.pixel_format,
    };
    if let Some(started) = started.take() {
        let _ = started.send(Ok(format));
    }

    let mut capture = CaptureLoop {
        stats: CaptureStats::new(monotonic_timestamp_us()?),
        cached_yuv: None,
        keyframe_input_pending: false,
        frame_pacer: FramePacer::new(frames_per_second),
        refinement: StaticRefinement::new(
            width,
            height,
            frames_per_second,
            config.bitrate_bits_per_second,
        ),
        encoded_cursor: None,
        desktop_pending: false,
        desktop_cached: false,
        separate_cursor_visible: false,
        encoder,
        converter,
        cursor_compositor,
        desktop,
        context,
        width,
        height,
    };
    capture.run(duplication.as_mut(), &controls, &stop, &sink)
}

struct CaptureSources {
    duplication: Option<DxgiDuplicationApi>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    desktop: Option<DesktopCapture>,
    cursor_compositor: Option<CursorCompositor>,
    width: u32,
    height: u32,
}

/// Opens Desktop Duplication for a single unrotated monitor, or desktop-region
/// capture for the combined view, the background desktop and rotated outputs.
fn open_sources(
    monitor: Option<Monitor>,
    display_id: u32,
    config: &StreamConfig,
) -> Result<CaptureSources, Error> {
    let origin = match monitor {
        Some(monitor) => crate::display_info(monitor)?,
        None => crate::background::display(),
    };
    let mut duplication =
        if display_id == crate::ALL_MONITORS_ID || display_id == crate::background::DISPLAY_ID {
            None
        } else {
            Some(
                DxgiDuplicationApi::new_options(
                    monitor.ok_or(Error::InvalidDisplayDimensions)?,
                    &[DxgiDuplicationFormat::Bgra8],
                )
                .map_err(duplication_error)?,
            )
        };
    let (device, context) = if let Some(duplication) = duplication.as_ref() {
        (
            duplication.device().clone(),
            duplication.device_context().clone(),
        )
    } else {
        windows_capture::d3d11::create_d3d_device()
            .map_err(|error| Error::DesktopDuplication(error.to_string()))?
    };
    // DXGI surfaces remain in the output's native orientation. A portrait
    // desktop can therefore have a landscape texture; treating that as a mode
    // change causes an endless restart loop. Capture rotated outputs in physical
    // desktop coordinates, using the same region path as the combined view.
    if duplication.as_ref().is_some_and(|capture| {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
        };
        matches!(
            capture.duplication_desc().Rotation,
            DXGI_MODE_ROTATION_ROTATE90
                | DXGI_MODE_ROTATION_ROTATE180
                | DXGI_MODE_ROTATION_ROTATE270
        )
    }) {
        tracing::info!(
            display_id,
            "using desktop-region capture for a rotated monitor"
        );
        duplication = None;
    }
    let desktop = if duplication.is_none() {
        Some(DesktopCapture::new(
            &device,
            config.frames_per_second,
            display_id,
        )?)
    } else {
        None
    };
    let (width, height) = if let Some(desktop) = desktop.as_ref() {
        (desktop.width(), desktop.height())
    } else {
        let duplication = duplication
            .as_ref()
            .ok_or(Error::InvalidDisplayDimensions)?;
        (duplication.width() & !1, duplication.height() & !1)
    };
    if width < 2 || height < 2 {
        return Err(Error::InvalidDisplayDimensions);
    }
    let cursor_compositor = if duplication.is_some() {
        Some(CursorCompositor::new(
            &device, width, height, origin.x, origin.y,
        )?)
    } else {
        None
    };
    Ok(CaptureSources {
        duplication,
        device,
        context,
        desktop,
        cursor_compositor,
        width,
        height,
    })
}

/// The capture loop's pipeline and the state it carries between frames.
/// Fields drop in declaration order, the reverse of their creation.
struct CaptureLoop {
    refinement: StaticRefinement,
    frame_pacer: FramePacer,
    keyframe_input_pending: bool,
    cached_yuv: Option<ID3D11Texture2D>,
    stats: CaptureStats,
    encoded_cursor: Option<bool>,
    desktop_pending: bool,
    desktop_cached: bool,
    separate_cursor_visible: bool,
    encoder: Encoder,
    converter: Converter,
    cursor_compositor: Option<CursorCompositor>,
    desktop: Option<DesktopCapture>,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
}

impl CaptureLoop {
    fn run(
        &mut self,
        mut duplication: Option<&mut DxgiDuplicationApi>,
        controls: &ControlState,
        stop: &AtomicBool,
        sink: &EncodedFrameSink,
    ) -> Result<(), Error> {
        while !stop.load(Ordering::Acquire) {
            // Apply controls and drain output independently of desktop damage.
            // Media Foundation encoders are asynchronous: submit() may return no
            // output and signal it a few milliseconds later. Previously a static
            // desktop meant poll() was never called again, leaving the first IDR
            // frame queued inside the encoder until a mouse/pixel update occurred.
            self.apply_controls(controls)?;

            let capture_cursor = controls.capture_cursor.load(Ordering::Acquire);
            let desktop_texture = match self.desktop.as_mut() {
                Some(desktop) => desktop.capture(&self.context, capture_cursor)?,
                None => None,
            };
            let frame = if let Some(duplication) = duplication.as_deref_mut() {
                match duplication.acquire_next_frame(ACQUIRE_TIMEOUT_MS) {
                    Ok(frame) => Some(frame),
                    Err(DuplicationError::Timeout) => None,
                    Err(error) => return Err(duplication_error(error)),
                }
            } else {
                None
            };
            if let Some(frame) = frame.as_ref()
                && frame.frame_info().LastMouseUpdateTime != 0
            {
                self.separate_cursor_visible = frame.frame_info().PointerPosition.Visible.as_bool();
            }
            let capture_idle = frame.is_none() && desktop_texture.is_none();
            let mut access_units = self.encoder.poll()?;
            if frame
                .as_ref()
                .is_some_and(|frame| frame.width() < self.width || frame.height() < self.height)
            {
                return Err(Error::DesktopDuplication(
                    "captured display dimensions changed".into(),
                ));
            }
            if let Some(frame) = frame.as_ref() {
                self.track_damage(frame, capture_cursor);
            }
            let new_capture = frame.is_some() || desktop_texture.is_some();
            if new_capture {
                self.stats.frames_captured += 1;
            }
            self.encode(
                desktop_texture.as_ref(),
                capture_cursor,
                new_capture,
                &mut access_units,
            )?;
            drop(frame);
            self.deliver(access_units, sink);
            self.stats
                .report(monotonic_timestamp_us()?, self.width, self.height);
            if capture_idle {
                thread::sleep(CAPTURE_POLL_INTERVAL);
            }
        }
        Ok(())
    }

    fn apply_controls(&mut self, controls: &ControlState) -> Result<(), Error> {
        if controls.request_keyframe.swap(false, Ordering::AcqRel) {
            self.encoder.request_keyframe()?;
            self.keyframe_input_pending = true;
        }
        let requested_bitrate = controls.requested_bitrate.swap(0, Ordering::AcqRel);
        if requested_bitrate != 0 {
            match self.encoder.set_bitrate(requested_bitrate) {
                Ok(()) => self.refinement.set_bitrate(requested_bitrate),
                Err(error) => {
                    // A bitrate-control failure must not look like a desktop or GPU
                    // loss. Otherwise the Agent repeatedly recreates the stream,
                    // forcing a new viewer window and a large bootstrap keyframe.
                    tracing::warn!(
                        %error,
                        bits_per_second = requested_bitrate,
                        "encoder rejected a runtime bitrate update; continuing at the previous bitrate"
                    );
                    controls
                        .runtime_bitrate_disabled
                        .store(true, Ordering::Release);
                }
            }
        }
        Ok(())
    }

    fn track_damage(&mut self, frame: &DxgiDuplicationFrame<'_>, capture_cursor: bool) {
        let Some(cursor) = self.cursor_compositor.as_ref() else {
            return;
        };
        let info = frame.frame_info();
        let desktop_changed = !self.desktop_cached || info.LastPresentTime != 0;
        if desktop_changed {
            cursor.update(&self.context, frame.texture());
            self.desktop_cached = true;
        }
        // AcquireNextFrame also wakes for pointer-only movement. Encoding
        // those unchanged desktops advances the GOP and wastes bandwidth
        // even though the viewer draws its own cursor. Keep real damage
        // pending until the encoder accepts it, including visibility changes.
        self.desktop_pending |= needs_pointer_frame(
            desktop_changed,
            info.LastMouseUpdateTime != 0,
            capture_cursor,
        );
    }

    fn encode(
        &mut self,
        desktop_texture: Option<&ID3D11Texture2D>,
        capture_cursor: bool,
        new_capture: bool,
        access_units: &mut Vec<EncodedAccessUnit>,
    ) -> Result<(), Error> {
        // Recompose the cached desktop when cursor visibility changes,
        // including keyboard-only handoffs with no DXGI damage. Retain pending
        // work until the encoder and frame pacer accept it.
        if desktop_texture.is_some()
            || (self.desktop_cached
                && (self.desktop_pending || self.encoded_cursor != Some(capture_cursor)))
        {
            let capture_timestamp_us = monotonic_timestamp_us()?;
            if self.encoder.wants_input() && self.frame_pacer.allow(capture_timestamp_us) {
                let texture = match desktop_texture {
                    Some(texture) => texture,
                    None => self
                        .cursor_compositor
                        .as_ref()
                        .ok_or(Error::InvalidDisplayDimensions)?
                        .compose(&self.context, capture_cursor, self.separate_cursor_visible)?,
                };
                let yuv = self.converter.convert(texture)?;
                self.cached_yuv = Some(yuv.clone());
                access_units.extend(self.encoder.submit(yuv, capture_timestamp_us)?);
                self.keyframe_input_pending = false;
                self.desktop_pending = false;
                self.encoded_cursor = Some(capture_cursor);
            } else if new_capture {
                if self.encoder.wants_input() {
                    self.stats.frames_rate_limited += 1;
                } else {
                    self.stats.frames_encoder_busy += 1;
                }
            }
        } else if self.keyframe_input_pending
            && self.encoder.wants_input()
            && let Some(yuv) = self.cached_yuv.as_ref()
        {
            // A keyframe request must work even when Desktop Duplication has
            // no new damage to report. Re-submit the last GPU surface so a
            // newly created viewer/presenter can recover immediately instead
            // of waiting for the login screen to change a pixel.
            let capture_timestamp_us = monotonic_timestamp_us()?;
            access_units.extend(self.encoder.submit(yuv, capture_timestamp_us)?);
            self.keyframe_input_pending = false;
        } else if self.desktop.is_none()
            && self.refinement.pending()
            && self.encoder.wants_input()
            && let Some(yuv) = self.cached_yuv.as_ref()
        {
            // An idle desktop leaves a keyframe at the quality its single-frame
            // budget allowed. Re-encode the unchanged surface at the stream
            // rate so it sharpens as it would under motion, instead of waiting
            // for new damage. Region capture already submits every interval;
            // refining there would displace its captures.
            let capture_timestamp_us = monotonic_timestamp_us()?;
            if self.frame_pacer.allow(capture_timestamp_us) {
                access_units.extend(self.encoder.submit(yuv, capture_timestamp_us)?);
                self.refinement.refined();
                self.stats.frames_refined += 1;
            }
        }
        Ok(())
    }

    fn deliver(&mut self, access_units: Vec<EncodedAccessUnit>, sink: &EncodedFrameSink) {
        for access_unit in access_units {
            self.stats.frames_encoded += 1;
            self.stats.total_encode_us = self.stats.total_encode_us.saturating_add(
                access_unit
                    .encode_complete_timestamp_us
                    .saturating_sub(access_unit.capture_timestamp_us),
            );
            self.stats.encoded_bytes = self
                .stats
                .encoded_bytes
                .saturating_add(access_unit.data.len() as u64);
            self.refinement.encoded(access_unit.keyframe);
            (sink)(EncodedAccessUnit {
                capture_timestamp_us: access_unit.capture_timestamp_us,
                encode_complete_timestamp_us: access_unit.encode_complete_timestamp_us,
                keyframe: access_unit.keyframe,
                codec_config: access_unit.codec_config,
                data: access_unit.data,
            });
        }
    }
}

/// Counters logged every two seconds.
struct CaptureStats {
    frames_captured: u64,
    frames_encoded: u64,
    frames_rate_limited: u64,
    frames_encoder_busy: u64,
    frames_refined: u64,
    total_encode_us: u64,
    encoded_bytes: u64,
    started_us: u64,
}

impl CaptureStats {
    fn new(started_us: u64) -> Self {
        Self {
            frames_captured: 0,
            frames_encoded: 0,
            frames_rate_limited: 0,
            frames_encoder_busy: 0,
            frames_refined: 0,
            total_encode_us: 0,
            encoded_bytes: 0,
            started_us,
        }
    }

    fn report(&mut self, now_us: u64, width: u32, height: u32) {
        let elapsed_us = now_us.saturating_sub(self.started_us);
        if elapsed_us >= 2_000_000 {
            let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
            tracing::info!(
                capture_fps = self.frames_captured as f64 / elapsed_seconds,
                stream_fps = self.frames_encoded as f64 / elapsed_seconds,
                bitrate_bits_per_second = self.encoded_bytes as f64 * 8.0 / elapsed_seconds,
                frames_rate_limited = self.frames_rate_limited,
                frames_encoder_busy = self.frames_encoder_busy,
                frames_refined = self.frames_refined,
                mean_encode_us = self.total_encode_us / self.frames_encoded.max(1),
                width,
                height,
                "Desktop Duplication capture/encoder statistics"
            );
            *self = Self::new(now_us);
        }
    }
}

fn duplication_error(error: DuplicationError) -> Error {
    Error::DesktopDuplication(error.to_string())
}

// A hidden pointer cannot change the transmitted pixels. Embedded-pointer
// changes still arrive as desktop damage and retain the cursor-free GDI path.
fn needs_pointer_frame(desktop_changed: bool, pointer_changed: bool, capture_cursor: bool) -> bool {
    desktop_changed || (pointer_changed && capture_cursor)
}

#[cfg(test)]
mod tests {
    use super::needs_pointer_frame;

    #[test]
    fn hidden_pointer_motion_does_not_schedule_video_but_desktop_damage_does() {
        assert!(!needs_pointer_frame(false, true, false));
        assert!(!needs_pointer_frame(false, false, true));
        assert!(needs_pointer_frame(false, true, true));
        for capture_cursor in [false, true] {
            for pointer_changed in [false, true] {
                assert!(needs_pointer_frame(true, pointer_changed, capture_cursor));
            }
        }
    }
}
