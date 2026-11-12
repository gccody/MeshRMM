//! Software H.264 decoding with the bundled OpenH264, on a thread of its
//! own so a large frame never holds up the window. It decodes every frame,
//! since each depends on the one before, and keeps only the newest picture
//! for the window to draw.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Context;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

use super::app::{self, SessionCommand, UiEvent};
use super::presenter::{QueuedFrame, Shared};

/// Keyframes in a row the decoder may refuse before the stream is given up.
const MAX_FAILED_KEYFRAMES: u32 = 3;

/// A decoded picture: full-size luma and half-size chroma planes, studio
/// range BT.709, each row packed to its width.
#[derive(Default)]
pub(super) struct Picture {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) chroma_width: u32,
    pub(super) chroma_height: u32,
    pub(super) y: Vec<u8>,
    pub(super) u: Vec<u8>,
    pub(super) v: Vec<u8>,
    /// When its frame arrived, in the viewer's monotonic microseconds.
    pub(super) received_at_us: u64,
}

/// Copies `rows` rows of `width` bytes out of a plane `stride` bytes wide.
fn copy_plane(target: &mut Vec<u8>, source: &[u8], stride: usize, width: usize, rows: usize) {
    target.clear();
    target.reserve(width * rows);
    for row in source.chunks(stride).take(rows) {
        target.extend_from_slice(&row[..width.min(row.len())]);
    }
}

/// Decodes one access unit into `picture`. `Ok(false)` when the decoder
/// has no picture for it yet.
pub(super) fn decode(
    decoder: &mut Decoder,
    data: &[u8],
    picture: &mut Picture,
) -> anyhow::Result<bool> {
    let Some(decoded) = decoder
        .decode(data)
        .map_err(|error| anyhow::anyhow!("{error}"))?
    else {
        return Ok(false);
    };
    let (width, height) = decoded.dimensions();
    let (luma_stride, chroma_stride, _) = decoded.strides();
    anyhow::ensure!(
        width > 0 && height > 0 && luma_stride >= width && chroma_stride > 0,
        "the decoder returned a {width}x{height} picture it cannot have"
    );
    // Planes with an odd size round their chroma up; never read past what
    // the decoder gave.
    let chroma_width = width.div_ceil(2).min(chroma_stride);
    let chroma_height = height
        .div_ceil(2)
        .min(decoded.u().len() / chroma_stride)
        .min(decoded.v().len() / chroma_stride);
    anyhow::ensure!(
        decoded.y().len() >= luma_stride * (height - 1) + width && chroma_height > 0,
        "the decoder returned planes smaller than its {width}x{height} picture"
    );
    copy_plane(&mut picture.y, decoded.y(), luma_stride, width, height);
    copy_plane(
        &mut picture.u,
        decoded.u(),
        chroma_stride,
        chroma_width,
        chroma_height,
    );
    copy_plane(
        &mut picture.v,
        decoded.v(),
        chroma_stride,
        chroma_width,
        chroma_height,
    );
    picture.width = width as u32;
    picture.height = height as u32;
    picture.chroma_width = chroma_width as u32;
    picture.chroma_height = chroma_height as u32;
    Ok(true)
}

pub(super) fn new_decoder() -> anyhow::Result<Decoder> {
    Decoder::new()
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("could not create the OpenH264 decoder")
}

/// Starts the decoder thread, which ends when the presenter stops.
pub(super) fn spawn(shared: Arc<Shared>) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("meshrmm-video-decoder".into())
        .spawn(move || run(&shared))
        .context("could not start the video decoder thread")?;
    Ok(())
}

/// Waits for the next frame. `None` once the presenter has stopped.
fn next_frame(shared: &Shared) -> Option<(QueuedFrame, u64)> {
    let mut input = shared
        .input
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    loop {
        if input.stopped {
            return None;
        }
        if let Some(frame) = input.frames.pop_front() {
            return Some((frame, input.stream));
        }
        input = shared
            .wake
            .wait(input)
            .unwrap_or_else(|error| error.into_inner());
    }
}

/// Decoding statistics for the diagnostics overlay and the log.
struct Statistics {
    started: Instant,
    decoded: u64,
    decode_time: Duration,
    log: crate::debug::StatisticsLog,
}

impl Statistics {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            decoded: 0,
            decode_time: Duration::ZERO,
            log: Default::default(),
        }
    }

    /// The decode rate over the last two seconds, once they have passed.
    fn record(&mut self, shared: &Shared, took: Duration) {
        self.decoded += 1;
        self.decode_time += took;
        let elapsed = self.started.elapsed();
        if elapsed < Duration::from_secs(2) {
            return;
        }
        let decode_fps = self.decoded as f64 / elapsed.as_secs_f64();
        shared
            .decode_fps
            .store(decode_fps.to_bits(), Ordering::Relaxed);
        if self.log.due() {
            tracing::info!(
                decode_fps,
                average_decode_us = (self.decode_time / self.decoded.max(1) as u32).as_micros(),
                frames_dropped = shared.dropped.load(Ordering::Relaxed),
                pictures_replaced = shared.replaced.load(Ordering::Relaxed),
                "Linux software decode statistics"
            );
        }
        *self = Self {
            log: std::mem::take(&mut self.log),
            ..Self::new()
        };
    }
}

fn run(shared: &Shared) {
    let mut decoder = None::<Decoder>;
    let mut stream = 0;
    let mut waiting_for_keyframe = true;
    let mut failed_keyframes = 0;
    let mut statistics = Statistics::new();
    while let Some((queued, frame_stream)) = next_frame(shared) {
        if frame_stream != stream {
            // A replacement stream has its own parameter sets and references.
            stream = frame_stream;
            decoder = None;
            waiting_for_keyframe = true;
        }
        let frame = &queued.frame;
        if waiting_for_keyframe && !frame.keyframe {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let active = match &mut decoder {
            Some(active) => active,
            None => match new_decoder() {
                Ok(created) => decoder.insert(created),
                Err(error) => {
                    shared.fail(format!("{error:#}"));
                    return;
                }
            },
        };
        let mut picture = shared
            .spare
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .unwrap_or_default();
        let started = Instant::now();
        match decode(active, &frame.data, &mut picture) {
            Ok(true) => {
                statistics.record(shared, started.elapsed());
                waiting_for_keyframe = false;
                failed_keyframes = 0;
                picture.received_at_us = queued.received_at_us;
                shared.decoded.fetch_add(1, Ordering::Relaxed);
                let replaced = shared
                    .picture
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .replace(picture);
                if let Some(replaced) = replaced {
                    shared.replaced.fetch_add(1, Ordering::Relaxed);
                    shared.recycle(replaced);
                }
                if !shared.picture_posted.swap(true, Ordering::AcqRel) {
                    app::post(UiEvent::Session {
                        id: shared.id,
                        command: SessionCommand::Picture,
                    });
                }
            }
            Ok(false) => shared.recycle(picture),
            Err(error) => {
                shared.recycle(picture);
                tracing::warn!(
                    %error,
                    frame_id = frame.frame_id,
                    keyframe = frame.keyframe,
                    bytes = frame.data.len(),
                    "the H.264 decoder refused a frame"
                );
                if frame.keyframe {
                    failed_keyframes += 1;
                    if failed_keyframes >= MAX_FAILED_KEYFRAMES {
                        shared.fail(format!(
                            "the H.264 software decoder cannot decode this stream: {error:#}"
                        ));
                        return;
                    }
                }
                // What follows refers to the frame that was lost.
                decoder = None;
                waiting_for_keyframe = true;
                shared.begin_recovery(frame.stream_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X264: &[u8] = include_bytes!("../../../tests/fixtures/yuv420p.h264");
    const MEDIA_FOUNDATION: &[u8] = include_bytes!("../../../tests/fixtures/yuv420p-software.h264");

    /// Both fixtures are one 64×48 frame of blue.
    fn assert_blue(picture: &Picture) {
        assert_eq!((picture.width, picture.height), (64, 48));
        assert_eq!((picture.chroma_width, picture.chroma_height), (32, 24));
        assert_eq!(picture.y.len(), 64 * 48);
        assert_eq!(picture.u.len(), 32 * 24);
        assert_eq!(picture.v.len(), 32 * 24);
        let average = |plane: &[u8]| {
            plane.iter().map(|&value| u32::from(value)).sum::<u32>() / plane.len() as u32
        };
        // Blue is dark, with much blue-difference and little red-difference.
        assert!(average(&picture.y) < 64, "luma {}", average(&picture.y));
        assert!(average(&picture.u) > 200, "Cb {}", average(&picture.u));
        assert!(average(&picture.v) < 128, "Cr {}", average(&picture.v));
    }

    #[test]
    fn decodes_the_windows_agents_software_stream_without_waiting_for_more() {
        let mut decoder = new_decoder().unwrap();
        let mut picture = Picture::default();
        assert!(decode(&mut decoder, MEDIA_FOUNDATION, &mut picture).unwrap());
        assert_blue(&picture);
    }

    #[test]
    fn decodes_a_high_profile_stream() {
        let mut decoder = new_decoder().unwrap();
        let mut picture = Picture::default();
        assert!(decode(&mut decoder, X264, &mut picture).unwrap());
        assert_blue(&picture);
    }

    #[test]
    fn a_corrupt_frame_is_an_error_and_a_new_decoder_recovers() {
        let mut decoder = new_decoder().unwrap();
        let mut picture = Picture::default();
        let mut corrupt = MEDIA_FOUNDATION.to_vec();
        let middle = corrupt.len() / 2;
        corrupt.truncate(middle);
        // A truncated access unit either fails or yields no picture.
        if let Ok(true) = decode(&mut decoder, &corrupt, &mut picture) {
            panic!("half a frame decoded to a picture");
        }
        let mut decoder = new_decoder().unwrap();
        assert!(decode(&mut decoder, MEDIA_FOUNDATION, &mut picture).unwrap());
        assert_blue(&picture);
    }

    #[test]
    fn rows_are_packed_to_the_picture_width() {
        let source = [1, 2, 3, 9, 4, 5, 6, 9, 7, 8];
        let mut target = vec![0xff; 2];
        copy_plane(&mut target, &source, 4, 3, 3);
        // The last row may end without its padding.
        assert_eq!(target, [1, 2, 3, 4, 5, 6, 7, 8]);
        copy_plane(&mut target, &source, 4, 3, 2);
        assert_eq!(target, [1, 2, 3, 4, 5, 6]);
    }
}
