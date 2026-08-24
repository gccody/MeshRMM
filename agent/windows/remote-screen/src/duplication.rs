use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, Error as DuplicationError,
};
use windows_capture::monitor::Monitor;

use crate::converter::BgraToNv12Converter;
use crate::encoder::{MediaFoundationH264Encoder, VideoEncoder};
use crate::{
    ActiveFormat, ControlState, EncodedAccessUnit, EncodedFrameSink, Error, StreamConfig,
    monotonic_timestamp_us,
};

const ACQUIRE_TIMEOUT_MS: u32 = 50;
const START_TIMEOUT: Duration = Duration::from_secs(20);

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
        let monitor = Monitor::enumerate()
            .map_err(|error| Error::DesktopDuplication(error.to_string()))?
            .into_iter()
            .find(|monitor| monitor.index().is_ok_and(|id| id == display_id as usize))
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
                let result = capture_loop(monitor, config, sink, controls, thread_stop, started_tx);
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
                    "capture did not initialize within 20 seconds".into(),
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
    config: StreamConfig,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
    stop: Arc<AtomicBool>,
    started: mpsc::SyncSender<Result<ActiveFormat, String>>,
) -> Result<(), Error> {
    let mut started = Some(started);
    let result = capture_loop_inner(monitor, config, sink, controls, stop, &mut started);
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
    config: StreamConfig,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
    stop: Arc<AtomicBool>,
    started: &mut Option<mpsc::SyncSender<Result<ActiveFormat, String>>>,
) -> Result<(), Error> {
    let mut duplication = DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8])
        .map_err(duplication_error)?;
    let width = duplication.width() & !1;
    let height = duplication.height() & !1;
    if width < 2 || height < 2 {
        return Err(Error::InvalidDisplayDimensions);
    }
    let format = ActiveFormat {
        width,
        height,
        frames_per_second: config.frames_per_second,
        bitrate_bits_per_second: config.bitrate_bits_per_second,
    };
    let mut converter = BgraToNv12Converter::new(
        duplication.device(),
        duplication.device_context(),
        width,
        height,
        config.frames_per_second,
    )?;
    let mut encoder = MediaFoundationH264Encoder::new(
        duplication.device(),
        width,
        height,
        config.frames_per_second,
        config.bitrate_bits_per_second,
    )?;
    if let Some(started) = started.take() {
        let _ = started.send(Ok(format));
    }

    let mut frames_captured = 0_u64;
    let mut frames_encoded = 0_u64;
    let mut encoded_bytes = 0_u64;
    let mut stats_started_us = monotonic_timestamp_us()?;
    while !stop.load(Ordering::Acquire) {
        let frame = match duplication.acquire_next_frame(ACQUIRE_TIMEOUT_MS) {
            Ok(frame) => frame,
            Err(DuplicationError::Timeout) => continue,
            Err(error) => return Err(duplication_error(error)),
        };
        if frame.width() < width || frame.height() < height {
            return Err(Error::DesktopDuplication(
                "captured display dimensions changed".into(),
            ));
        }
        if controls.request_keyframe.swap(false, Ordering::AcqRel) {
            encoder.request_keyframe()?;
        }
        let requested_bitrate = controls.requested_bitrate.swap(0, Ordering::AcqRel);
        if requested_bitrate != 0 {
            encoder.set_bitrate(requested_bitrate)?;
        }
        let capture_timestamp_us = monotonic_timestamp_us()?;
        frames_captured += 1;
        let mut access_units = encoder.poll()?;
        if encoder.wants_input() {
            let nv12 = converter.convert(frame.texture())?;
            access_units.extend(encoder.submit(nv12, capture_timestamp_us)?);
        }
        for access_unit in access_units {
            frames_encoded += 1;
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
                width,
                height,
                "Desktop Duplication capture/encoder statistics"
            );
            frames_captured = 0;
            frames_encoded = 0;
            encoded_bytes = 0;
            stats_started_us = now_us;
        }
    }
    Ok(())
}

fn duplication_error(error: DuplicationError) -> Error {
    Error::DesktopDuplication(error.to_string())
}
