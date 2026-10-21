//! Software H.264 encoding of captured frames with OpenH264, configured like
//! the other platforms' encoders: real time, screen content, no frame
//! reordering and keyframes only on request. Frames are converted to 4:2:0
//! with the BT.709 limited-range matrix the Windows software encoder uses,
//! and encoded on their own thread, newest first: a frame that arrives while
//! another is being encoded replaces any that is still waiting. After a
//! keyframe, a screen that stops changing is encoded again for a while to
//! sharpen it, as on Windows and macOS (`StaticRefinement`).
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;

#[path = "../../../windows/remote-screen/src/refinement.rs"]
mod refinement;
use refinement::StaticRefinement;

/// Software encoding keeps to this rate, as on Windows.
pub(crate) const MAX_FRAMES_PER_SECOND: u32 = 30;
/// OpenH264's largest picture.
const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 2160;

pub(crate) struct EncodedAccessUnit {
    pub data: Vec<u8>,
    /// Always `None`: OpenH264 writes the parameter sets into each keyframe.
    pub codec_config: Option<Vec<u8>>,
    pub keyframe: bool,
    pub capture_timestamp_us: u64,
    pub encode_complete_timestamp_us: u64,
}

pub(crate) type FrameSink = Arc<dyn Fn(EncodedAccessUnit) + Send + Sync + 'static>;

/// The byte order of a captured frame's 32-bit pixels, named as the bytes
/// lie in memory; `x` is unused or alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelLayout {
    Bgrx,
    Rgbx,
    Xrgb,
    Xbgr,
}

impl PixelLayout {
    /// Byte offsets of red, green and blue in a pixel.
    fn offsets(self) -> (usize, usize, usize) {
        match self {
            Self::Bgrx => (2, 1, 0),
            Self::Rgbx => (0, 1, 2),
            Self::Xrgb => (1, 2, 3),
            Self::Xbgr => (3, 2, 1),
        }
    }
}

/// A captured frame, borrowed from the capture until it is converted.
pub(crate) struct RawFrame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub layout: PixelLayout,
    pub timestamp_us: u64,
}

impl RawFrame<'_> {
    /// Whether the frame's rows fit in its data.
    fn valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize * 4
            && self.data.len() >= self.stride * (self.height as usize - 1) + self.width as usize * 4
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EncoderSettings {
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub grayscale: bool,
}

/// A 4:2:0 picture with even dimensions, ready to encode.
#[derive(Default)]
struct Picture {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    timestamp_us: u64,
}

/// The size a display of `width` by `height` is encoded at: cropped by at
/// most a row and a column to even dimensions, and scaled down to fit
/// OpenH264's largest picture, keeping its shape.
pub(crate) fn encoded_size(width: u32, height: u32) -> (u32, u32) {
    let (width, height) = if width <= MAX_WIDTH && height <= MAX_HEIGHT {
        (width, height)
    } else if u64::from(width) * u64::from(MAX_HEIGHT) >= u64::from(height) * u64::from(MAX_WIDTH) {
        (
            MAX_WIDTH,
            (u64::from(height) * u64::from(MAX_WIDTH) / u64::from(width)) as u32,
        )
    } else {
        (
            (u64::from(width) * u64::from(MAX_HEIGHT) / u64::from(height)) as u32,
            MAX_HEIGHT,
        )
    };
    ((width & !1).max(2), (height & !1).max(2))
}

/// How a frame larger than the picture is sampled: for each picture column
/// and row, the two source pixels around its centre and the second one's
/// weight out of 256. Columns are byte offsets into a row.
struct Scale {
    columns: Vec<(usize, usize, u16)>,
    rows: Vec<(usize, usize, u16)>,
}

impl Scale {
    fn new(frame: &RawFrame<'_>, width: usize, height: usize) -> Self {
        let samples = |source: usize, target: usize| {
            (0..target).map(move |index| {
                // The centre of target pixel `index`, in source pixels times
                // 256, less half a pixel.
                let centre = ((2 * index + 1) * source * 256 / (2 * target)).saturating_sub(128);
                let first = (centre >> 8).min(source - 1);
                (first, (first + 1).min(source - 1), (centre & 255) as u16)
            })
        };
        Self {
            columns: samples(frame.width as usize, width)
                .map(|(left, right, weight)| (left * 4, right * 4, weight))
                .collect(),
            rows: samples(frame.height as usize, height).collect(),
        }
    }

    /// Picture row `row` into `output`, in the frame's pixel layout:
    /// blended down from the two source rows into `between`, then across.
    fn row(&self, frame: &RawFrame<'_>, row: usize, between: &mut [u8], output: &mut [u8]) {
        let (above, below, weight) = self.rows[row];
        let length = between.len();
        let line = |row: usize| &frame.data[row * frame.stride..][..length];
        let source = if weight == 0 {
            line(above)
        } else {
            // Byte by byte in 16 bits, which the products fit, so it
            // vectorizes.
            let keep = 256 - weight;
            for ((out, &first), &second) in between.iter_mut().zip(line(above)).zip(line(below)) {
                *out = ((u16::from(first) * keep + u16::from(second) * weight + 128) >> 8) as u8;
            }
            &*between
        };
        for (out, &(left, right, weight)) in output.chunks_exact_mut(4).zip(&self.columns) {
            let keep = 256 - weight;
            let (left, right) = (&source[left..left + 4], &source[right..right + 4]);
            for byte in 0..4 {
                out[byte] = ((u16::from(left[byte]) * keep + u16::from(right[byte]) * weight + 128)
                    >> 8) as u8;
            }
        }
    }
}

impl Picture {
    /// Converts `frame` at its [`encoded_size`], reusing this picture's
    /// buffers. A larger frame is scaled down bilinearly. Bands of rows are
    /// converted in parallel.
    fn convert(&mut self, frame: &RawFrame<'_>, grayscale: bool) {
        let (width, height) = encoded_size(frame.width, frame.height);
        let (width, height) = (width as usize, height as usize);
        self.width = width;
        self.height = height;
        self.timestamp_us = frame.timestamp_us;
        self.y.resize(width * height, 0);
        self.u.resize(width * height / 4, 0);
        self.v.resize(width * height / 4, 0);
        let scale = ((width, height) != (frame.width as usize & !1, frame.height as usize & !1))
            .then(|| Scale::new(frame, width, height));
        let threads = std::thread::available_parallelism().map_or(1, |threads| threads.get());
        // Pairs of rows, as 4:2:0 needs.
        let band = height.div_ceil(2).div_ceil(threads.clamp(1, 8)) * 2;
        let chroma_width = width / 2;
        std::thread::scope(|scope| {
            let mut bands = self
                .y
                .chunks_mut(band * width)
                .zip(self.u.chunks_mut(band / 2 * chroma_width))
                .zip(self.v.chunks_mut(band / 2 * chroma_width))
                .enumerate()
                .map(|(index, ((y, u), v))| {
                    let scale = scale.as_ref();
                    move || convert_band(frame, scale, width, index * band, y, u, v, grayscale)
                });
            // This thread converts the first band while others do the rest.
            let first = bands.next();
            for convert in bands {
                scope.spawn(convert);
            }
            if let Some(mut convert) = first {
                convert();
            }
        });
    }
}

/// Converts picture rows `first_row` onwards into `y`, `u` and `v`, which
/// hold a band of whole rows.
#[allow(clippy::too_many_arguments)]
fn convert_band(
    frame: &RawFrame<'_>,
    scale: Option<&Scale>,
    width: usize,
    first_row: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
    grayscale: bool,
) {
    let offsets = frame.layout.offsets();
    let chroma_width = width / 2;
    // Scaling needs a source row and two picture rows of its own.
    let (mut between, mut lines) = match scale {
        Some(_) => (
            vec![0; frame.width as usize * 4],
            [vec![0; width * 4], vec![0; width * 4]],
        ),
        None => Default::default(),
    };
    let pairs = y
        .chunks_exact_mut(2 * width)
        .zip(u.chunks_exact_mut(chroma_width))
        .zip(v.chunks_exact_mut(chroma_width));
    for (pair, ((y, u), v)) in pairs.enumerate() {
        let row = first_row + pair * 2;
        let source = match scale {
            Some(scale) => {
                let [top, bottom] = &mut lines;
                scale.row(frame, row, &mut between, top);
                scale.row(frame, row + 1, &mut between, bottom);
                [&top[..], &bottom[..]]
            }
            None => [
                &frame.data[row * frame.stride..],
                &frame.data[(row + 1) * frame.stride..],
            ],
        };
        convert_pair(source, offsets, y, u, v, grayscale);
    }
}

/// Converts two rows of pixels, with red, green and blue at `offsets`, into
/// two rows of `y` and a row each of `u` and `v`.
fn convert_pair(
    lines: [&[u8]; 2],
    (red, green, blue): (usize, usize, usize),
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
    grayscale: bool,
) {
    let width = y.len() / 2;
    for column in (0..width).step_by(2) {
        let (mut sum_r, mut sum_g, mut sum_b) = (0_i32, 0_i32, 0_i32);
        for (line, source) in lines.into_iter().enumerate() {
            for x in [column, column + 1] {
                let pixel = &source[x * 4..x * 4 + 4];
                let r = i32::from(pixel[red]);
                let g = i32::from(pixel[green]);
                let b = i32::from(pixel[blue]);
                y[line * width + x] = ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
                sum_r += r;
                sum_g += g;
                sum_b += b;
            }
        }
        let index = column / 2;
        if grayscale {
            u[index] = 128;
            v[index] = 128;
        } else {
            // The sums are four pixels: shift by two more bits.
            u[index] = (((-26 * sum_r - 87 * sum_g + 112 * sum_b + 512) >> 10) + 128) as u8;
            v[index] = (((112 * sum_r - 102 * sum_g - 10 * sum_b + 512) >> 10) + 128) as u8;
        }
    }
}

#[derive(Default)]
struct State {
    /// The newest converted frame, not encoded yet.
    pending: Option<Picture>,
    /// A spare buffer for the next conversion.
    spare: Option<Picture>,
    keyframe: bool,
    bitrate: Option<u32>,
    stopped: bool,
    failure: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// Encodes what a capture submits until it is dropped.
pub(crate) struct VideoEncoder {
    shared: Arc<Shared>,
    settings: EncoderSettings,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VideoEncoder {
    pub(crate) fn start(settings: EncoderSettings, sink: FrameSink) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("meshrmm-h264-encoder".into())
            .spawn(move || {
                if let Err(error) = encode_frames(&worker, settings, &sink) {
                    tracing::warn!(error = ?error, "the H.264 encoder stopped");
                    worker
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .failure = Some(format!("{error:#}"));
                }
            })
            .context("could not start the H.264 encoder")?;
        Ok(Self {
            shared,
            settings,
            thread: Some(thread),
        })
    }

    /// Converts `frame` and queues it in place of any frame still waiting.
    pub(crate) fn submit(&self, frame: &RawFrame<'_>) {
        if !frame.valid() || frame.width < 2 || frame.height < 2 {
            return;
        }
        let mut picture = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spare
            .take()
            .unwrap_or_default();
        picture.convert(frame, self.settings.grayscale);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.spare = state.pending.replace(picture);
        drop(state);
        self.shared.wake.notify_one();
    }

    /// Makes the next frame a keyframe, encoding the last frame again when
    /// the screen is not changing.
    pub(crate) fn request_keyframe(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keyframe = true;
        self.shared.wake.notify_one();
    }

    pub(crate) fn set_bitrate(&self, bits_per_second: u32) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bitrate = Some(bits_per_second.max(1));
        self.shared.wake.notify_one();
    }

    /// Why the encoder stopped, once it has.
    pub(crate) fn failure(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .clone()
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopped = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn configuration(settings: EncoderSettings, bits_per_second: u32) -> EncoderConfig {
    EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bits_per_second))
        .max_frame_rate(FrameRate::from_hz(settings.frames_per_second as f32))
        // Only the viewer asks for keyframes.
        .intra_frame_period(IntraFramePeriod::from_num_frames(0))
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .skip_frames(true)
        // Neither applies to screen content; OpenH264 warns when they are on.
        .adaptive_quantization(false)
        .background_detection(false)
        .num_threads(0)
        .vui(VuiConfig::bt709())
}

fn encode_frames(
    shared: &Shared,
    settings: EncoderSettings,
    sink: &FrameSink,
) -> anyhow::Result<()> {
    let frames_per_second = settings.frames_per_second.clamp(1, MAX_FRAMES_PER_SECOND);
    let interval = Duration::from_secs(1) / frames_per_second;
    let mut bits_per_second = settings.bitrate_bits_per_second.max(1);
    let mut encoder: Option<(Encoder, (usize, usize))> = None;
    let mut refinement = StaticRefinement::new(0, 0, frames_per_second, bits_per_second);
    let mut last = Picture::default();
    let mut next_frame = Instant::now();
    let mut output = Vec::new();
    loop {
        let (picture, keyframe, bitrate) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return Ok(());
                }
                let now = Instant::now();
                // With no new frame, the last one is encoded again for a
                // keyframe or to refine it.
                let ready = state.pending.is_some()
                    || ((state.keyframe || refinement.pending()) && last.width > 0);
                if ready && now >= next_frame {
                    break;
                }
                state = if ready {
                    shared
                        .wake
                        .wait_timeout(state, next_frame - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                } else {
                    shared.wake.wait(state).unwrap_or_else(|e| e.into_inner())
                };
            }
            let keyframe = std::mem::take(&mut state.keyframe);
            let picture = state.pending.take();
            (picture, keyframe, state.bitrate.take())
        };
        let started = Instant::now();
        next_frame = started + interval;
        let refining = picture.is_none() && !keyframe;
        if let Some(picture) = picture {
            let previous = std::mem::replace(&mut last, picture);
            shared.state.lock().unwrap_or_else(|e| e.into_inner()).spare = Some(previous);
        }
        let size = (last.width, last.height);
        if encoder.as_ref().is_none_or(|(_, encoded)| *encoded != size) {
            let created = Encoder::with_api_config(
                openh264::OpenH264API::from_source(),
                configuration(settings, bits_per_second),
            )
            .context("could not create the OpenH264 encoder")?;
            encoder = Some((created, size));
            refinement = StaticRefinement::new(
                size.0 as u32,
                size.1 as u32,
                frames_per_second,
                bits_per_second,
            );
        }
        let (encoder, _) = encoder.as_mut().expect("the encoder was just created");
        if let Some(bitrate) = bitrate
            && bitrate != bits_per_second
        {
            bits_per_second = bitrate;
            set_bitrate(encoder, bits_per_second);
            refinement.set_bitrate(bits_per_second);
        }
        if refining {
            refinement.refined();
        }
        if keyframe {
            encoder.force_intra_frame();
        }
        let source = YUVSlices::new(
            (&last.y, &last.u, &last.v),
            (last.width, last.height),
            (last.width, last.width / 2, last.width / 2),
        );
        let bitstream = encoder
            .encode(&source)
            .context("OpenH264 could not encode a frame")?;
        let frame_type = bitstream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            continue;
        }
        output.clear();
        bitstream.write_vec(&mut output);
        if output.is_empty() {
            continue;
        }
        let encoded_keyframe = matches!(frame_type, FrameType::IDR | FrameType::I);
        refinement.encoded(encoded_keyframe);
        sink(EncodedAccessUnit {
            data: output.clone(),
            codec_config: None,
            keyframe: encoded_keyframe,
            capture_timestamp_us: last.timestamp_us,
            encode_complete_timestamp_us: super::monotonic_timestamp_us(),
        });
    }
}

/// Changes the target bitrate without starting a new stream.
fn set_bitrate(encoder: &mut Encoder, bits_per_second: u32) {
    let mut info = openh264_sys2::SBitrateInfo {
        iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
        iBitrate: bits_per_second.min(i32::MAX as u32) as i32,
    };
    // SAFETY: ENCODER_OPTION_BITRATE takes an SBitrateInfo, which outlives
    // the call; the encoder is initialized.
    let result = unsafe {
        encoder.raw_api().set_option(
            openh264_sys2::ENCODER_OPTION_BITRATE,
            (&raw mut info).cast(),
        )
    };
    if result != 0 {
        tracing::warn!(bits_per_second, result, "OpenH264 refused a bitrate change");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &[u8], width: u32, height: u32, layout: PixelLayout) -> RawFrame<'_> {
        RawFrame {
            data,
            width,
            height,
            stride: width as usize * 4,
            layout,
            timestamp_us: 7,
        }
    }

    #[test]
    fn converts_with_the_bt709_limited_range_matrix() {
        let white = vec![255_u8; 2 * 2 * 4];
        let mut picture = Picture::default();
        picture.convert(&frame(&white, 2, 2, PixelLayout::Bgrx), false);
        assert_eq!(picture.y, [235; 4]);
        // The Windows encoder's rounding gives white this blue difference too.
        assert_eq!((picture.u[0], picture.v[0]), (127, 128));
        // Pure red in each layout.
        for (layout, pixel) in [
            (PixelLayout::Bgrx, [0, 0, 255, 0]),
            (PixelLayout::Rgbx, [255, 0, 0, 0]),
            (PixelLayout::Xrgb, [0, 255, 0, 0]),
            (PixelLayout::Xbgr, [0, 0, 0, 255]),
        ] {
            let red = pixel.repeat(4);
            picture.convert(&frame(&red, 2, 2, layout), false);
            assert_eq!(picture.y[0], 63, "{layout:?}");
            assert_eq!((picture.u[0], picture.v[0]), (102, 240), "{layout:?}");
            picture.convert(&frame(&red, 2, 2, layout), true);
            assert_eq!((picture.u[0], picture.v[0]), (128, 128), "{layout:?}");
        }
    }

    #[test]
    fn odd_sizes_are_cropped_and_short_data_is_refused() {
        let data = vec![0_u8; 3 * 3 * 4];
        let mut picture = Picture::default();
        picture.convert(&frame(&data, 3, 3, PixelLayout::Bgrx), false);
        assert_eq!((picture.width, picture.height), (2, 2));
        assert!(frame(&data, 3, 3, PixelLayout::Bgrx).valid());
        assert!(!frame(&data[..20], 3, 3, PixelLayout::Bgrx).valid());
    }

    #[test]
    fn displays_larger_than_4k_are_scaled_to_fit_keeping_their_shape() {
        assert_eq!(encoded_size(1920, 1080), (1920, 1080));
        assert_eq!(encoded_size(3840, 2160), (3840, 2160));
        assert_eq!(encoded_size(5120, 2880), (3840, 2160));
        assert_eq!(encoded_size(7680, 4320), (3840, 2160));
        // 16:10 is held by its height, an ultrawide by its width.
        assert_eq!(encoded_size(3840, 2400), (3456, 2160));
        assert_eq!(encoded_size(5120, 1440), (3840, 1080));
        assert_eq!(encoded_size(2160, 3840), (1214, 2160));
        assert_eq!(encoded_size(40_000, 3), (3840, 2));
    }

    #[test]
    fn scaling_down_averages_the_pixels_it_covers() {
        // Black and white columns, at twice the width that fits.
        let (width, height) = (7680_u32, 8_u32);
        let mut data = vec![0_u8; width as usize * height as usize * 4];
        for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
            if index % 2 == 1 {
                pixel.copy_from_slice(&[255; 4]);
            }
        }
        let mut picture = Picture::default();
        picture.convert(&frame(&data, width, height, PixelLayout::Bgrx), false);
        assert_eq!((picture.width, picture.height), (3840, 4));
        assert!(
            picture.y.iter().all(|&luma| (125..=126).contains(&luma)),
            "every pixel is mid gray"
        );
        // A white left half stays white and a black right half black, apart
        // from the column on the edge.
        for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
            let white = (index % width as usize) < width as usize / 2;
            pixel.copy_from_slice(&[if white { 255 } else { 0 }; 4]);
        }
        picture.convert(&frame(&data, width, height, PixelLayout::Bgrx), false);
        assert_eq!(picture.y[0], 235);
        assert_eq!(picture.y[1918], 235);
        assert_eq!(picture.y[1921], 16);
        assert_eq!(picture.y[3839], 16);
    }

    #[test]
    fn encodes_a_keyframe_then_answers_keyframe_requests() {
        let (sender, units) = std::sync::mpsc::channel();
        let encoder = VideoEncoder::start(
            EncoderSettings {
                frames_per_second: 30,
                bitrate_bits_per_second: 2_000_000,
                grayscale: false,
            },
            Arc::new(move |unit| {
                let _ = sender.send(unit);
            }),
        )
        .unwrap();
        let (width, height) = (320, 240);
        let mut data = vec![0_u8; width * height * 4];
        for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[(index % 256) as u8, (index / 7 % 256) as u8, 90, 0]);
        }
        encoder.submit(&frame(
            &data,
            width as u32,
            height as u32,
            PixelLayout::Bgrx,
        ));
        let first = units.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first.keyframe);
        assert_eq!(&first.data[..4], &[0, 0, 0, 1]);
        assert_eq!(first.capture_timestamp_us, 7);
        // The unchanged frame is refined for a bounded time: 250 ms at 30 FPS
        // here, a frame of which OpenH264 may skip.
        let refined = std::iter::from_fn(|| units.recv_timeout(Duration::from_millis(500)).ok())
            .collect::<Vec<_>>();
        assert!(
            (1..=7).contains(&refined.len()),
            "{} refined frames",
            refined.len()
        );
        assert!(refined.iter().all(|unit| !unit.keyframe));
        encoder.request_keyframe();
        let again = units.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            again.keyframe,
            "a requested keyframe repeats the last frame"
        );
        encoder.set_bitrate(500_000);
        data[0] = 255;
        encoder.submit(&frame(
            &data,
            width as u32,
            height as u32,
            PixelLayout::Bgrx,
        ));
        let next = units.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!next.keyframe);
        assert!(encoder.failure().is_none());
    }
}
