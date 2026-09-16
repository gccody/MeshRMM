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
        channels = config.channels,
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
    i16: cpal::FromSample<T>,
{
    use cpal::Sample;
    let rate = config.sample_rate.0;
    let channels = config.channels;
    let chunk = ((rate as usize / 100).max(1) * usize::from(channels))
        .min((MAX_PACKET - HEADER) / (usize::from(channels) * 2) * usize::from(channels));
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            for samples in data.chunks(chunk) {
                let mut bytes = Vec::with_capacity(HEADER + samples.len() * 2);
                bytes.extend(rate.to_le_bytes());
                bytes.extend(channels.to_le_bytes());
                bytes.extend(
                    samples
                        .iter()
                        .flat_map(|&s| i16::from_sample(s).to_le_bytes()),
                );
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
