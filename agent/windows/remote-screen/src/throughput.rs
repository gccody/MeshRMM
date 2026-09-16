use crate::encoder::VideoEncoder;
use crate::{VideoCodec, VideoPixelFormat, converter, encoder};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};

#[test]
#[ignore = "requires a hardware H.264/HEVC encoder; benchmarks 1440p for 10 seconds per codec"]
fn hardware_encode_1440p60() {
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        let (device, context) = windows_capture::d3d11::create_d3d_device().unwrap();
        let mut converter = converter::BgraToYuvConverter::new(
            &device,
            &context,
            2560,
            1440,
            60,
            VideoPixelFormat::Yuv420,
        )
        .unwrap();
        let mut encoder = encoder::MediaFoundationVideoEncoder::new(
            &device,
            2560,
            1440,
            60,
            12_000_000,
            codec,
            VideoPixelFormat::Yuv420,
        )
        .unwrap();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 2560,
            Height: 1440,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        let mut target = None;
        // Safety: texture and view are created on the device used by the encoder.
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .unwrap();
            device
                .CreateRenderTargetView(texture.as_ref().unwrap(), None, Some(&mut target))
                .unwrap();
        }
        let texture = texture.unwrap();
        let target = target.unwrap();
        let start = Instant::now();
        let mut submitted = 0;
        let mut completed = 0;
        let mut latency_us = 0;
        let mut next = start;
        while start.elapsed() < Duration::from_secs(10) {
            let mut output = encoder.poll().unwrap();
            if Instant::now() >= next && encoder.wants_input() {
                // Changing pixels exercises conversion and inter-frame coding without
                // depending on desktop activity, monitor refresh, or an SSH desktop.
                let phase = (submitted % 60) as f32 / 60.0;
                unsafe {
                    context.ClearRenderTargetView(&target, &[phase, 0.2, 1.0 - phase, 1.0]);
                }
                let timestamp = crate::monotonic_timestamp_us().unwrap();
                output.extend(
                    encoder
                        .submit(converter.convert(&texture).unwrap(), timestamp)
                        .unwrap(),
                );
                submitted += 1;
                next += Duration::from_nanos(1_000_000_000 / 60);
                if next < Instant::now() {
                    next = Instant::now();
                }
            }
            for frame in output {
                completed += 1;
                latency_us += frame
                    .encode_complete_timestamp_us
                    .saturating_sub(frame.capture_timestamp_us);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let fps = completed as f64 / start.elapsed().as_secs_f64();
        eprintln!(
            "{codec:?}: submitted={submitted} completed={completed} fps={fps:.2} mean_encode_us={}",
            latency_us / completed.max(1)
        );
        assert!(
            fps >= 30.0,
            "{codec:?} encoder cannot sustain 30 FPS: {fps:.2}"
        );
    }
}

#[test]
#[ignore = "requires an interactive Windows desktop with continuous animation; runs for 10 seconds"]
fn desktop_capture_throughput() {
    use crate::{StreamConfig, WindowsDesktopDuplicationStreamer};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let display = windows_capture::monitor::Monitor::primary()
        .unwrap()
        .index()
        .unwrap() as u32;
    let frames = Arc::new(AtomicU64::new(0));
    let latency = Arc::new(AtomicU64::new(0));
    let count = Arc::clone(&frames);
    let timing = Arc::clone(&latency);
    let mut streamer = WindowsDesktopDuplicationStreamer::new();
    streamer
        .start(
            StreamConfig {
                frames_per_second: 60,
                bitrate_bits_per_second: 12_000_000,
                codec: VideoCodec::H265,
                pixel_format: VideoPixelFormat::Yuv420,
                capture_cursor: true,
            },
            display,
            Arc::new(move |frame| {
                count.fetch_add(1, Ordering::Relaxed);
                timing.fetch_add(
                    frame
                        .encode_complete_timestamp_us
                        .saturating_sub(frame.capture_timestamp_us),
                    Ordering::Relaxed,
                );
            }),
        )
        .unwrap();
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(10));
    assert!(streamer.poll_ended().is_none());
    streamer.stop().unwrap();
    let count = frames.load(Ordering::Relaxed);
    let fps = count as f64 / started.elapsed().as_secs_f64();
    eprintln!(
        "desktop: frames={count} fps={fps:.2} mean_encode_us={}",
        latency.load(Ordering::Relaxed) / count.max(1)
    );
    assert!(
        fps >= 30.0,
        "animated desktop cannot sustain 30 FPS: {fps:.2}"
    );
}
