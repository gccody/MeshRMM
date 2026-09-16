use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, Error as DuplicationError,
};
use windows_capture::monitor::Monitor;

use crate::converter::BgraToYuvConverter;
use crate::encoder::{MediaFoundationVideoEncoder, VideoEncoder};
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
        self.controls.requested_bitrate.store(0, Ordering::Release);
        self.controls
            .runtime_bitrate_disabled
            .store(false, Ordering::Release);
        self.controls
            .request_keyframe
            .store(false, Ordering::Release);
        let monitor = Monitor::enumerate()
            .map_err(|error| Error::DesktopDuplication(error.to_string()))?
            .into_iter()
            .find(|monitor| {
                display_id == crate::ALL_MONITORS_ID
                    || monitor.index().is_ok_and(|id| id == display_id as usize)
            })
            .ok_or_else(|| {
                Error::DesktopDuplication(format!("display {display_id} is unavailable"))
            })?;
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
    monitor: Monitor,
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
    monitor: Monitor,
    display_id: u32,
    config: StreamConfig,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
    stop: Arc<AtomicBool>,
    started: &mut Option<mpsc::SyncSender<Result<ActiveFormat, String>>>,
) -> Result<(), Error> {
    let origin = crate::display_info(monitor)?;
    let mut duplication = if display_id == crate::ALL_MONITORS_ID {
        None
    } else {
        Some(
            DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8])
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
    let mut desktop = if duplication.is_none() {
        Some(crate::desktop::DesktopCapture::new(
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
    let cursor_compositor = if config.capture_cursor && duplication.is_some() {
        Some(crate::cursor::CursorCompositor::new(
            &device, width, height, origin.x, origin.y,
        )?)
    } else {
        None
    };
    let mut separate_cursor_visible = false;
    let format = ActiveFormat {
        width,
        height,
        frames_per_second: config.frames_per_second,
        bitrate_bits_per_second: config.bitrate_bits_per_second,
        codec: config.codec,
        pixel_format: config.pixel_format,
    };
    let mut converter = BgraToYuvConverter::new(
        &device,
        &context,
        width,
        height,
        config.frames_per_second,
        config.pixel_format,
    )?;
    let mut encoder = MediaFoundationVideoEncoder::new(
        &device,
        width,
        height,
        config.frames_per_second,
        config.bitrate_bits_per_second,
        config.codec,
        config.pixel_format,
    )?;
    if let Some(started) = started.take() {
        let _ = started.send(Ok(format));
    }

    let mut frames_captured = 0_u64;
    let mut frames_encoded = 0_u64;
    let mut frames_rate_limited = 0_u64;
    let mut frames_encoder_busy = 0_u64;
    let mut total_encode_us = 0_u64;
    let mut encoded_bytes = 0_u64;
    let mut stats_started_us = monotonic_timestamp_us()?;
    let mut cached_yuv = None;
    let mut keyframe_input_pending = false;
    let mut frame_pacer = FramePacer::new(config.frames_per_second);
    while !stop.load(Ordering::Acquire) {
        // Apply controls and drain output independently of desktop damage.
        // Media Foundation encoders are asynchronous: submit() may return no
        // output and signal it a few milliseconds later. Previously a static
        // desktop meant poll() was never called again, leaving the first IDR
        // frame queued inside the encoder until a mouse/pixel update occurred.
        if controls.request_keyframe.swap(false, Ordering::AcqRel) {
            encoder.request_keyframe()?;
            keyframe_input_pending = true;
        }
        let requested_bitrate = controls.requested_bitrate.swap(0, Ordering::AcqRel);
        if requested_bitrate != 0
            && let Err(error) = encoder.set_bitrate(requested_bitrate)
        {
            // A bitrate-control failure must not look like a desktop or GPU
            // loss. Otherwise the Agent repeatedly recreates the stream,
            // forcing a new viewer window and a large bootstrap keyframe.
            tracing::warn!(
                %error,
                bits_per_second = requested_bitrate,
                "hardware encoder rejected a runtime bitrate update; continuing at the previous bitrate"
            );
            controls
                .runtime_bitrate_disabled
                .store(true, Ordering::Release);
        }

        let desktop_texture = match desktop.as_mut() {
            Some(desktop) => desktop.capture(&context, config.capture_cursor)?,
            None => None,
        };
        let frame = if let Some(duplication) = duplication.as_mut() {
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
            separate_cursor_visible = frame.frame_info().PointerPosition.Visible.as_bool();
        }
        let capture_idle = frame.is_none() && desktop_texture.is_none();
        let mut access_units = encoder.poll()?;
        if frame
            .as_ref()
            .is_some_and(|frame| frame.width() < width || frame.height() < height)
        {
            return Err(Error::DesktopDuplication(
                "captured display dimensions changed".into(),
            ));
        }
        if let Some(texture) = desktop_texture
            .as_ref()
            .or_else(|| frame.as_ref().map(|frame| frame.texture()))
        {
            let capture_timestamp_us = monotonic_timestamp_us()?;
            frames_captured += 1;
            if encoder.wants_input() && frame_pacer.allow(capture_timestamp_us) {
                let texture = if separate_cursor_visible {
                    match cursor_compositor.as_ref() {
                        Some(cursor) => cursor.compose(&context, texture)?,
                        None => texture,
                    }
                } else {
                    texture
                };
                let yuv = converter.convert(texture)?;
                cached_yuv = Some(yuv.clone());
                access_units.extend(encoder.submit(yuv, capture_timestamp_us)?);
                keyframe_input_pending = false;
            } else if encoder.wants_input() {
                frames_rate_limited += 1;
            } else {
                frames_encoder_busy += 1;
            }
        } else if keyframe_input_pending
            && encoder.wants_input()
            && let Some(yuv) = cached_yuv.as_ref()
        {
            // A keyframe request must work even when Desktop Duplication has
            // no new damage to report. Re-submit the last GPU surface so a
            // newly created viewer/presenter can recover immediately instead
            // of waiting for the login screen to change a pixel.
            let capture_timestamp_us = monotonic_timestamp_us()?;
            access_units.extend(encoder.submit(yuv, capture_timestamp_us)?);
            keyframe_input_pending = false;
        }
        drop(frame);
        for access_unit in access_units {
            frames_encoded += 1;
            total_encode_us = total_encode_us.saturating_add(
                access_unit
                    .encode_complete_timestamp_us
                    .saturating_sub(access_unit.capture_timestamp_us),
            );
            encoded_bytes = encoded_bytes.saturating_add(access_unit.data.len() as u64);
            (sink)(EncodedAccessUnit {
                capture_timestamp_us: access_unit.capture_timestamp_us,
                encode_complete_timestamp_us: access_unit.encode_complete_timestamp_us,
                keyframe: access_unit.keyframe,
                codec_config: access_unit.codec_config,
                data: access_unit.data,
            });
        }
        let now_us = monotonic_timestamp_us()?;
        let elapsed_us = now_us.saturating_sub(stats_started_us);
        if elapsed_us >= 2_000_000 {
            let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
            tracing::info!(
                capture_fps = frames_captured as f64 / elapsed_seconds,
                stream_fps = frames_encoded as f64 / elapsed_seconds,
                bitrate_bits_per_second = encoded_bytes as f64 * 8.0 / elapsed_seconds,
                frames_rate_limited,
                frames_encoder_busy,
                mean_encode_us = total_encode_us / frames_encoded.max(1),
                width,
                height,
                "Desktop Duplication capture/encoder statistics"
            );
            frames_captured = 0;
            frames_encoded = 0;
            frames_rate_limited = 0;
            frames_encoder_busy = 0;
            total_encode_us = 0;
            encoded_bytes = 0;
            stats_started_us = now_us;
        }
        if capture_idle {
            thread::sleep(CAPTURE_POLL_INTERVAL);
        }
    }
    Ok(())
}

fn duplication_error(error: DuplicationError) -> Error {
    Error::DesktopDuplication(error.to_string())
}
