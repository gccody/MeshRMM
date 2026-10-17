//! A JPEG of the main display for the dashboard's thumbnail, from one frame
//! of a short capture.
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::Context;

use super::capture::{self, CaptureSettings};
use super::encoder::{PixelLayout, RawFrame};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const QUALITY: u8 = 70;

/// The main display, scaled to fit the thumbnail box, as a JPEG.
pub(crate) fn main_display_jpeg() -> anyhow::Result<Vec<u8>> {
    let mut backend = capture::detect()?;
    let outputs = backend.outputs()?;
    let output = super::display::choose(&outputs, None)?;
    let (sender, frames) = mpsc::sync_channel(1);
    let sender = Mutex::new(Some(sender));
    let stream = backend.start(
        &output,
        CaptureSettings {
            frames_per_second: 10,
            capture_cursor: false,
        },
        Arc::new(move |frame: &RawFrame<'_>| {
            if let Some(sender) = sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = sender.send(scale(frame));
            }
        }),
    )?;
    let image = frames
        .recv_timeout(FIRST_FRAME_TIMEOUT)
        .context("the display server sent no frame for the thumbnail")?;
    drop(stream);
    let (width, height, rgb) = image;
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, QUALITY)
        .encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb)
        .context("could not encode the thumbnail")?;
    anyhow::ensure!(
        jpeg.len() <= crate::remote::thumbnail::MAX_BYTES,
        "the {} byte thumbnail is too large",
        jpeg.len()
    );
    Ok(jpeg)
}

/// The frame fitted in the thumbnail box, as RGB, by averaging the source
/// pixels under each thumbnail pixel.
fn scale(frame: &RawFrame<'_>) -> (u16, u16, Vec<u8>) {
    let (width, height) = crate::remote::thumbnail::scaled_size(frame.width, frame.height);
    let (red, green, blue) = match frame.layout {
        PixelLayout::Bgrx => (2, 1, 0),
        PixelLayout::Rgbx => (0, 1, 2),
        PixelLayout::Xrgb => (1, 2, 3),
        PixelLayout::Xbgr => (3, 2, 1),
    };
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        let top = y * frame.height / height;
        let bottom = ((y + 1) * frame.height / height).max(top + 1);
        for x in 0..width {
            let left = x * frame.width / width;
            let right = ((x + 1) * frame.width / width).max(left + 1);
            let mut sums = [0_u32; 3];
            for row in top..bottom {
                let line = &frame.data[row as usize * frame.stride..];
                for column in left..right {
                    let pixel = &line[column as usize * 4..column as usize * 4 + 4];
                    sums[0] += u32::from(pixel[red]);
                    sums[1] += u32::from(pixel[green]);
                    sums[2] += u32::from(pixel[blue]);
                }
            }
            let count = (bottom - top) * (right - left);
            rgb.extend(sums.iter().map(|sum| (sum / count) as u8));
        }
    }
    (width as u16, height as u16, rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run in a desktop session; saves the thumbnail as
    /// /tmp/meshrmm-snapshot.jpg to look at.
    #[test]
    #[ignore = "captures the session's screen"]
    fn captures_the_main_display() {
        let jpeg = main_display_jpeg().unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        std::fs::write("/tmp/meshrmm-snapshot.jpg", jpeg).unwrap();
    }

    /// Run in a desktop session: streams the main display through the
    /// encoder for a few seconds.
    #[test]
    #[ignore = "captures the session's screen"]
    fn streams_the_main_display() {
        use crate::remote::linux::encoder::{EncoderSettings, VideoEncoder};

        let mut backend = capture::detect().unwrap();
        let outputs = backend.outputs().unwrap();
        eprintln!("{}: {outputs:?}", backend.name());
        let output = super::super::display::choose(&outputs, None).unwrap();
        let (sender, units) = mpsc::channel();
        let encoder = Arc::new(
            VideoEncoder::start(
                EncoderSettings {
                    frames_per_second: 30,
                    bitrate_bits_per_second: 4_000_000,
                    grayscale: false,
                },
                Arc::new(move |unit| {
                    let _ = sender.send(unit);
                }),
            )
            .unwrap(),
        );
        let frames = Arc::clone(&encoder);
        let mut stream = backend
            .start(
                &output,
                CaptureSettings {
                    frames_per_second: 30,
                    capture_cursor: true,
                },
                Arc::new(move |frame: &RawFrame<'_>| frames.submit(frame)),
            )
            .unwrap();
        let first = units.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(first.keyframe);
        encoder.request_keyframe();
        let seconds = std::env::var("MESHRMM_STREAM_SECONDS")
            .ok()
            .and_then(|seconds| seconds.parse().ok())
            .unwrap_or(3);
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        let (mut count, mut keyframes) = (1, 1);
        while let Ok(unit) =
            units.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            count += 1;
            keyframes += usize::from(unit.keyframe);
        }
        eprintln!("{count} frames, {keyframes} keyframes");
        assert!(keyframes >= 2, "a requested keyframe arrives");
        assert!(stream.poll_ended().is_none());
    }

    #[test]
    fn scales_into_the_thumbnail_box_by_averaging() {
        let (width, height) = (1280_u32, 800_u32);
        let mut data = vec![0_u8; width as usize * height as usize * 4];
        // The left half white, in BGRx.
        for row in data.chunks_exact_mut(width as usize * 4) {
            row[..width as usize * 2].fill(255);
        }
        let (scaled_width, scaled_height, rgb) = scale(&RawFrame {
            data: &data,
            width,
            height,
            stride: width as usize * 4,
            layout: PixelLayout::Bgrx,
            timestamp_us: 0,
        });
        assert_eq!((scaled_width, scaled_height), (640, 400));
        assert_eq!(&rgb[..3], &[255, 255, 255]);
        assert_eq!(&rgb[rgb.len() - 3..], &[0, 0, 0]);
    }
}
