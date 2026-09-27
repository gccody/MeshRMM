use super::*;
use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};

/// WASAPI loopback captures the system mix, never the microphone.
/// Keep this stream on its owning native thread until the session ends.
pub struct Capture {
    _stream: cpal::Stream,
    name: String,
    failed: Arc<AtomicBool>,
}
impl Capture {
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::Relaxed)
            && cpal::default_host()
                .default_output_device()
                .and_then(|d| d.name().ok())
                .as_deref()
                == Some(self.name.as_str())
    }
}
pub fn capture(send: impl Fn(Vec<u8>) + Send + 'static) -> anyhow::Result<Capture> {
    let device = cpal::default_host()
        .default_output_device()
        .context("no system audio output device")?;
    let name = device.name()?;
    let failed = Arc::new(AtomicBool::new(false));
    let format = device.default_output_config()?;
    let config = format.config();
    anyhow::ensure!(
        (8_000..=192_000).contains(&config.sample_rate.0) && (1..=8).contains(&config.channels),
        "unsupported system audio format"
    );
    let stream = match format.sample_format() {
        cpal::SampleFormat::F32 => input::<f32>(&device, &config, send, failed.clone())?,
        cpal::SampleFormat::I16 => input::<i16>(&device, &config, send, failed.clone())?,
        cpal::SampleFormat::U16 => input::<u16>(&device, &config, send, failed.clone())?,
        other => anyhow::bail!("unsupported capture sample format: {other}"),
    };
    stream.play()?;
    tracing::info!(
        rate = config.sample_rate.0,
        device_channels = config.channels,
        channels = config.channels.min(2),
        "system audio capture started"
    );
    Ok(Capture {
        _stream: stream,
        name,
        failed,
    })
}
fn input<T: cpal::SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    send: impl Fn(Vec<u8>) + Send + 'static,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    let rate = config.sample_rate.0;
    let channels = usize::from(config.channels);
    // Surround is mixed to stereo here, so the wire never carries more.
    let output_channels: u16 = if channels == 1 { 1 } else { 2 };
    let frames_per_packet = (rate as usize / 100)
        .max(1)
        .min((MAX_PACKET - HEADER) / (usize::from(output_channels) * 2));
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            let mut frame = [0.0_f32; 8];
            for samples in data.chunks(frames_per_packet * channels) {
                let frames = samples.len() / channels;
                let mut bytes =
                    Vec::with_capacity(HEADER + frames * usize::from(output_channels) * 2);
                bytes.extend(rate.to_le_bytes());
                bytes.extend(output_channels.to_le_bytes());
                for input in samples.chunks_exact(channels) {
                    for (value, &sample) in frame.iter_mut().zip(input) {
                        *value = f32::from_sample(sample);
                    }
                    let mixed = downmix(&frame[..channels]);
                    for &value in &mixed[..usize::from(output_channels)] {
                        bytes.extend(i16::from_sample(value).to_le_bytes());
                    }
                }
                send(bytes);
            }
        },
        move |error| {
            failed.store(true, Ordering::Relaxed);
            tracing::warn!(%error, "system audio capture failed");
        },
        None,
    )
}
