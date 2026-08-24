#![cfg(windows)]

//! Windows.Graphics.Capture + D3D11 + Media Foundation hardware video implementation.
//!
//! COM and GPU objects remain on the capture worker thread. Only compressed
//! encoded access units cross the callback boundary.

mod converter;
mod duplication;
mod encoder;

pub use duplication::WindowsDesktopDuplicationStreamer;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use thiserror::Error;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::converter::BgraToYuvConverter;
use crate::encoder::{MediaFoundationVideoEncoder, VideoEncoder};

pub type EncodedFrameSink = Arc<dyn Fn(EncodedAccessUnit) + Send + Sync + 'static>;

/// Monotonic microseconds in the same QPC clock domain as capture timestamps.
pub fn monotonic_timestamp_us() -> Result<u64, Error> {
    encoder::performance_counter_us().map_err(Error::Encoder)
}

#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub codec: VideoCodec,
    pub pixel_format: VideoPixelFormat,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            frames_per_second: 60,
            bitrate_bits_per_second: 12_000_000,
            codec: VideoCodec::H264,
            pixel_format: VideoPixelFormat::Yuv420,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveFormat {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub codec: VideoCodec,
    pub pixel_format: VideoPixelFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoPixelFormat {
    #[default]
    Yuv420,
    Yuv444,
}

impl VideoPixelFormat {
    pub fn name(self) -> &'static str {
        match self {
            Self::Yuv420 => "4:2:0",
            Self::Yuv444 => "4:4:4",
        }
    }
}

impl std::fmt::Display for VideoPixelFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

impl VideoCodec {
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
        }
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>, Error> {
    Monitor::enumerate()
        .map_err(capture_error)?
        .into_iter()
        .map(display_info)
        .collect()
}

fn display_info(monitor: Monitor) -> Result<DisplayInfo, Error> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe {
        GetMonitorInfoW(
            windows::Win32::Graphics::Gdi::HMONITOR(monitor.as_raw_hmonitor()),
            &mut info,
        )
    }
    .as_bool()
    {
        return Err(Error::CaptureInitialization(
            windows::core::Error::from_thread().to_string(),
        ));
    }
    let width = u32::try_from(info.rcMonitor.right.saturating_sub(info.rcMonitor.left))
        .map_err(|_| Error::InvalidDisplayDimensions)?;
    let height = u32::try_from(info.rcMonitor.bottom.saturating_sub(info.rcMonitor.top))
        .map_err(|_| Error::InvalidDisplayDimensions)?;
    let id = u32::try_from(monitor.index().map_err(capture_error)?)
        .map_err(|_| Error::InvalidDisplayDimensions)?;
    let name = monitor
        .name()
        .or_else(|_| monitor.device_name())
        .unwrap_or_else(|_| format!("Display {id}"));
    Ok(DisplayInfo {
        id,
        name,
        x: info.rcMonitor.left,
        y: info.rcMonitor.top,
        width,
        height,
        primary: info.dwFlags & 1 != 0,
    })
}

fn capture_error(error: impl std::fmt::Display) -> Error {
    Error::CaptureInitialization(error.to_string())
}

#[derive(Debug)]
pub struct EncodedAccessUnit {
    pub capture_timestamp_us: u64,
    pub encode_complete_timestamp_us: u64,
    pub keyframe: bool,
    pub codec_config: Option<Vec<u8>>,
    pub data: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Windows capture initialization failed: {0}")]
    CaptureInitialization(String),
    #[error("D3D11 video conversion failed: {0}")]
    ColorConversion(#[from] converter::Error),
    #[error("Media Foundation hardware encoder failed: {0}")]
    Encoder(#[from] encoder::Error),
    #[error("captured display dimensions must be at least 2x2")]
    InvalidDisplayDimensions,
    #[error("capture is already running")]
    AlreadyRunning,
    #[error("capture is not running")]
    NotRunning,
    #[error("Windows Desktop Duplication failed: {0}")]
    DesktopDuplication(String),
}

#[derive(Clone)]
struct CaptureFlags {
    config: StreamConfig,
    format: ActiveFormat,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
}

#[derive(Default)]
struct ControlState {
    request_keyframe: AtomicBool,
    requested_bitrate: AtomicU32,
    runtime_bitrate_disabled: AtomicBool,
}

struct CaptureHandler {
    converter: BgraToYuvConverter,
    encoder: MediaFoundationVideoEncoder,
    sink: EncodedFrameSink,
    controls: Arc<ControlState>,
    format: ActiveFormat,
    frames_captured: u64,
    frames_encoded: u64,
    encoded_bytes: u64,
    stats_started_us: u64,
    logged_first_capture: bool,
    logged_first_conversion: bool,
    logged_first_submission: bool,
}

// Safety: windows-capture constructs and invokes CaptureHandler exclusively on
// its capture worker. The outer handle only posts stop signals; our public
// controls use atomics and never dereference these COM interfaces off-thread.
unsafe impl Send for CaptureHandler {}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = CaptureFlags;
    type Error = Error;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureFlags {
            config,
            format,
            sink,
            controls,
        } = context.flags;
        let converter = BgraToYuvConverter::new(
            &context.device,
            &context.device_context,
            format.width,
            format.height,
            format.frames_per_second,
            config.pixel_format,
        )?;
        let encoder = MediaFoundationVideoEncoder::new(
            &context.device,
            format.width,
            format.height,
            format.frames_per_second,
            config.bitrate_bits_per_second,
            config.codec,
            config.pixel_format,
        )?;
        Ok(Self {
            converter,
            encoder,
            sink,
            controls,
            format,
            frames_captured: 0,
            frames_encoded: 0,
            encoded_bytes: 0,
            stats_started_us: monotonic_timestamp_us()?,
            logged_first_capture: false,
            logged_first_conversion: false,
            logged_first_submission: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.controls.request_keyframe.swap(false, Ordering::AcqRel) {
            self.encoder.request_keyframe()?;
        }
        let requested_bitrate = self.controls.requested_bitrate.swap(0, Ordering::AcqRel);
        if requested_bitrate != 0
            && let Err(error) = self.encoder.set_bitrate(requested_bitrate)
        {
            // Some hardware MFTs advertise a modifiable bitrate but reject
            // the update at runtime. Keep the current encoder alive: ending
            // this worker makes the transport rebuild the stream and viewer
            // for every adaptive-bitrate attempt.
            tracing::warn!(
                %error,
                bits_per_second = requested_bitrate,
                "hardware encoder rejected a runtime bitrate update; continuing at the previous bitrate"
            );
            self.controls
                .runtime_bitrate_disabled
                .store(true, Ordering::Release);
        }
        let capture_timestamp_us = frame.timestamp()?.Duration.max(0) as u64 / 10;
        let encode_start_us = monotonic_timestamp_us()?;
        let capture_delivery_us = encode_start_us.saturating_sub(capture_timestamp_us);
        self.frames_captured += 1;
        if !self.logged_first_capture {
            self.logged_first_capture = true;
            tracing::info!(capture_delivery_us, "first desktop frame captured on GPU");
        }
        // Poll before converting so a YUV surface is only written when the
        // asynchronous MFT has requested another input. Converting every
        // captured frame would rotate through the fixed surface pool and could
        // overwrite a texture that the encoder was still reading.
        let mut access_units = self.encoder.poll()?;
        if self.encoder.wants_input() {
            let yuv = self.converter.convert(frame.as_raw_texture())?;
            if !self.logged_first_conversion {
                self.logged_first_conversion = true;
                tracing::info!(
                    pixel_format = %self.format.pixel_format,
                    "first desktop frame converted to YUV on GPU"
                );
            }
            let submitted = self.encoder.submit(yuv, capture_timestamp_us)?;
            if !self.logged_first_submission {
                self.logged_first_submission = true;
                tracing::info!(
                    immediate_access_units = submitted.len(),
                    codec = self.format.codec.name(),
                    "first desktop frame submitted to hardware encoder"
                );
            }
            access_units.extend(submitted);
        }
        for access_unit in access_units {
            self.frames_encoded += 1;
            self.encoded_bytes = self
                .encoded_bytes
                .saturating_add(access_unit.data.len() as u64);
            tracing::trace!(
                capture_delivery_us,
                convert_encode_us = access_unit
                    .encode_complete_timestamp_us
                    .saturating_sub(encode_start_us),
                encoded_bytes = access_unit.data.len(),
                keyframe = access_unit.keyframe,
                "desktop frame hardware encoded"
            );
            (self.sink)(access_unit);
        }
        let now_us = monotonic_timestamp_us()?;
        let elapsed_us = now_us.saturating_sub(self.stats_started_us);
        if elapsed_us >= 2_000_000 {
            let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
            tracing::info!(
                capture_fps = self.frames_captured as f64 / elapsed_seconds,
                stream_fps = self.frames_encoded as f64 / elapsed_seconds,
                bitrate_bits_per_second = self.encoded_bytes as f64 * 8.0 / elapsed_seconds,
                frames_captured = self.frames_captured,
                frames_encoded = self.frames_encoded,
                width = self.format.width,
                height = self.format.height,
                codec = self.format.codec.name(),
                "Windows capture/encoder statistics"
            );
            self.frames_captured = 0;
            self.frames_encoded = 0;
            self.encoded_bytes = 0;
            self.stats_started_us = now_us;
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("Windows capture source closed");
        Ok(())
    }
}

impl From<windows::core::Error> for Error {
    fn from(error: windows::core::Error) -> Self {
        Self::CaptureInitialization(error.to_string())
    }
}

pub struct WindowsScreenStreamer {
    control: Option<CaptureControl<CaptureHandler, Error>>,
    active_format: Option<ActiveFormat>,
    controls: Arc<ControlState>,
}

impl WindowsScreenStreamer {
    pub fn new() -> Self {
        Self {
            control: None,
            active_format: None,
            controls: Arc::new(ControlState::default()),
        }
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: u32,
        sink: EncodedFrameSink,
    ) -> Result<ActiveFormat, Error> {
        if self.control.is_some() {
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
            .map_err(capture_error)?
            .into_iter()
            .find(|monitor| monitor.index().is_ok_and(|id| id == display_id as usize))
            .ok_or_else(|| {
                Error::CaptureInitialization(format!("display {display_id} is unavailable"))
            })?;
        // NV12 and the hardware codec profiles require even dimensions.
        // Cropping a possible final odd row/column keeps all normal frame
        // traffic on the GPU.
        let width = monitor
            .width()
            .map_err(|error| Error::CaptureInitialization(error.to_string()))?
            & !1;
        let height = monitor
            .height()
            .map_err(|error| Error::CaptureInitialization(error.to_string()))?
            & !1;
        if width < 2 || height < 2 {
            return Err(Error::InvalidDisplayDimensions);
        }
        let format = ActiveFormat {
            width,
            height,
            frames_per_second: config.frames_per_second,
            bitrate_bits_per_second: config.bitrate_bits_per_second,
            codec: config.codec,
            pixel_format: config.pixel_format,
        };
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(std::time::Duration::from_secs_f64(
                1.0 / f64::from(config.frames_per_second.max(1)),
            )),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureFlags {
                config,
                format,
                sink,
                controls: Arc::clone(&self.controls),
            },
        );
        let control = CaptureHandler::start_free_threaded(settings)
            .map_err(|error| Error::CaptureInitialization(error.to_string()))?;
        self.control = Some(control);
        self.active_format = Some(format);
        Ok(format)
    }

    pub fn request_keyframe(&self) -> Result<(), Error> {
        if self.control.is_none() {
            return Err(Error::NotRunning);
        }
        self.controls
            .request_keyframe
            .store(true, Ordering::Release);
        Ok(())
    }

    pub fn set_bitrate(&self, bits_per_second: u32) -> Result<(), Error> {
        if self.control.is_none() {
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

    pub fn active_format(&self) -> Option<ActiveFormat> {
        self.active_format
    }

    pub fn poll_ended(&mut self) -> Option<Result<(), Error>> {
        if !self.control.as_ref()?.is_finished() {
            return None;
        }
        let control = self.control.take()?;
        self.active_format = None;
        Some(
            control
                .wait()
                .map_err(|error| Error::CaptureInitialization(error.to_string())),
        )
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        let Some(control) = self.control.take() else {
            self.active_format = None;
            return Ok(());
        };
        self.active_format = None;
        control
            .stop()
            .map_err(|error| Error::CaptureInitialization(error.to_string()))
    }
}

impl Default for WindowsScreenStreamer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WindowsScreenStreamer {
    fn drop(&mut self) {
        if let Some(control) = self.control.take()
            && let Err(error) = control.stop()
        {
            tracing::warn!(error = %error, "failed to stop Windows capture cleanly");
        }
    }
}

// These are deliberately kept private to this package. The callback owns them
// and windows-capture invokes it on the single capture/GPU worker thread.
#[allow(dead_code)]
fn _assert_device_types(_: &ID3D11Device, _: &ID3D11DeviceContext) {}
