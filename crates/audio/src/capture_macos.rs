//! macOS system audio from ScreenCaptureKit, which mixes everything the Mac
//! plays except this process. It needs the Screen & System Audio Recording
//! permission and a graphical session.
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};

use super::{HEADER, MAX_PACKET};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// 10 ms, like the Windows capture's packets.
const FRAMES_PER_PACKET: usize = SAMPLE_RATE as usize / 100;
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// kAudioFormatFlagIsFloat
const FLOAT: u32 = 1;
/// kAudioFormatFlagIsNonInterleaved
const NON_INTERLEAVED: u32 = 1 << 5;

struct Shared {
    send: Mutex<Box<dyn Fn(Vec<u8>) + Send>>,
    failed: AtomicBool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[name = "MeshRMMAudioOutput"]
    #[ivars = Arc<Shared>]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Audio {
                let shared = self.ivars();
                // SAFETY: the sample buffer is valid for the callback.
                match unsafe { packets(sample) } {
                    Ok(packets) => {
                        let send = shared.send.lock().unwrap_or_else(|e| e.into_inner());
                        for packet in packets {
                            send(packet);
                        }
                    }
                    Err(error) => tracing::debug!(%error, "skipped a system audio buffer"),
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            tracing::warn!(error = %error.localizedDescription(), "system audio capture stopped");
            self.ivars().failed.store(true, Ordering::Relaxed);
        }
    }
);

impl AudioOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's init is always valid.
        unsafe { msg_send![super(this), init] }
    }
}

/// System audio capture; it stops when dropped.
pub struct Capture {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
}

// SAFETY: SCStream is thread-safe; the rest is synchronized.
unsafe impl Send for Capture {}

impl Capture {
    pub fn healthy(&self) -> bool {
        !self.shared.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let (stopped, result) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = stopped.send(());
        });
        // SAFETY: stopping a started stream is always valid.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = result.recv_timeout(START_TIMEOUT);
    }
}

pub fn capture(send: impl Fn(Vec<u8>) + Send + 'static) -> anyhow::Result<Capture> {
    let shared = Arc::new(Shared {
        send: Mutex::new(Box::new(send)),
        failed: AtomicBool::new(false),
    });
    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes valid objects or null.
            let result = match unsafe { (content.as_ref(), error.as_ref()) } {
                (Some(content), _) => unsafe { content.displays() }
                    .firstObject()
                    .context("there is no display to capture system audio with"),
                (None, Some(error)) => Err(anyhow::anyhow!(
                    "ScreenCaptureKit cannot capture system audio: {}",
                    error.localizedDescription()
                )),
                (None, None) => Err(anyhow::anyhow!("ScreenCaptureKit listed no content")),
            };
            let _ = sender.send(result);
        },
    );
    // SAFETY: the handler matches the expected block signature.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    let display = receiver
        .recv_timeout(START_TIMEOUT)
        .context("ScreenCaptureKit did not list the displays in time")??;
    // SAFETY: the objects are created and configured before the stream starts.
    let (stream, output, queue) = unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::new(),
        );
        let configuration = SCStreamConfiguration::new();
        configuration.setCapturesAudio(true);
        configuration.setExcludesCurrentProcessAudio(true);
        configuration.setSampleRate(SAMPLE_RATE as isize);
        configuration.setChannelCount(CHANNELS as isize);
        // The stream must capture video too; keep it as small and slow as possible.
        configuration.setWidth(2);
        configuration.setHeight(2);
        configuration.setMinimumFrameInterval(CMTime::new(1, 1));
        let output = AudioOutput::new(Arc::clone(&shared));
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            Some(ProtocolObject::from_ref(&*output)),
        );
        let queue = DispatchQueue::new("com.meshrmm.agent.audio", None);
        for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    kind,
                    Some(&queue),
                )
                .map_err(|error| {
                    anyhow::anyhow!(
                        "ScreenCaptureKit rejected the audio output: {}",
                        error.localizedDescription()
                    )
                })?;
        }
        (stream, output, queue)
    };
    let (started, result) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes a valid error or null.
        let _ = started
            .send(unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string()));
    });
    // SAFETY: the stream is fully configured.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match result.recv_timeout(START_TIMEOUT) {
        Ok(None) => {}
        Ok(Some(error)) => bail!("ScreenCaptureKit could not capture system audio: {error}"),
        Err(_) => bail!("ScreenCaptureKit did not start capturing system audio in time"),
    }
    tracing::info!(
        rate = SAMPLE_RATE,
        channels = CHANNELS,
        "system audio capture started"
    );
    Ok(Capture {
        stream,
        _output: output,
        _queue: queue,
        shared,
    })
}

/// 32-bit float samples of an audio sample buffer as PCM16 packets.
///
/// # Safety
///
/// `sample` must be a valid audio sample buffer.
unsafe fn packets(sample: &CMSampleBuffer) -> anyhow::Result<Vec<Vec<u8>>> {
    let description = unsafe { sample.format_description() }.context("no audio format")?;
    // SAFETY: the description is an audio format description.
    let format =
        unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description).as_ref() }
            .context("no audio stream description")?;
    anyhow::ensure!(
        format.mFormatFlags & FLOAT != 0 && format.mBitsPerChannel == 32,
        "unsupported system audio format"
    );
    let rate = format.mSampleRate as u32;
    let channels = format.mChannelsPerFrame as usize;
    anyhow::ensure!(
        (8_000..=192_000).contains(&rate) && (1..=8).contains(&channels),
        "unsupported system audio layout"
    );
    let mut size = 0;
    // SAFETY: a size query writes only `size`.
    unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut size,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(
        size >= std::mem::size_of::<AudioBufferList>(),
        "no audio buffers"
    );
    // AudioBufferList holds pointers, so the storage is pointer-aligned.
    let mut storage = vec![0_u64; size.div_ceil(8)];
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: `storage` holds `size` bytes; the retained block buffer keeps
    // the samples alive until it is released below.
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            list,
            size,
            None,
            None,
            0,
            &mut block,
        )
    };
    anyhow::ensure!(status == 0, "could not read the audio buffers ({status})");
    // SAFETY: CoreMedia returned a +1 block buffer.
    let _block = NonNull::new(block)
        .map(|block| unsafe { objc2_core_foundation::CFRetained::from_raw(block) });
    // SAFETY: CoreMedia filled in the list, whose buffers follow its header.
    let buffers = unsafe {
        std::slice::from_raw_parts(
            (&raw const (*list).mBuffers).cast::<AudioBuffer>(),
            (*list).mNumberBuffers as usize,
        )
    };
    let planes = buffers
        .iter()
        .map(|buffer| {
            if buffer.mData.is_null() {
                return &[][..];
            }
            // SAFETY: each buffer holds mDataByteSize bytes of f32 samples.
            unsafe {
                std::slice::from_raw_parts(
                    buffer.mData.cast::<f32>().cast_const(),
                    buffer.mDataByteSize as usize / 4,
                )
            }
        })
        .collect::<Vec<_>>();
    let frames = if format.mFormatFlags & NON_INTERLEAVED != 0 {
        anyhow::ensure!(planes.len() >= channels, "missing audio channels");
        planes[..channels]
            .iter()
            .map(|plane| plane.len())
            .min()
            .unwrap_or(0)
    } else {
        planes.first().map_or(0, |plane| plane.len() / channels)
    };
    let sample_at = |frame: usize, channel: usize| -> f32 {
        if format.mFormatFlags & NON_INTERLEAVED != 0 {
            planes[channel][frame]
        } else {
            planes[0][frame * channels + channel]
        }
    };
    let output_channels: u16 = if channels == 1 { 1 } else { 2 };
    let frames_per_packet = FRAMES_PER_PACKET
        .min((MAX_PACKET - HEADER) / (usize::from(output_channels) * 2))
        .max(1);
    let mut packets = Vec::new();
    for start in (0..frames).step_by(frames_per_packet) {
        let end = (start + frames_per_packet).min(frames);
        let mut bytes =
            Vec::with_capacity(HEADER + (end - start) * usize::from(output_channels) * 2);
        bytes.extend(rate.to_le_bytes());
        bytes.extend(output_channels.to_le_bytes());
        let mut frame = [0.0_f32; 8];
        for index in start..end {
            for (channel, value) in frame.iter_mut().enumerate().take(channels) {
                *value = sample_at(index, channel);
            }
            let mixed = super::downmix(&frame[..channels]);
            for &value in &mixed[..usize::from(output_channels)] {
                bytes.extend(((value.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16).to_le_bytes());
            }
        }
        packets.push(bytes);
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "captures system audio; needs the Screen & System Audio Recording permission"]
    fn captures_what_the_mac_plays() {
        let (sender, packets) = mpsc::channel();
        let capture = capture(move |packet| {
            let _ = sender.send(packet);
        })
        .unwrap();
        let status = std::process::Command::new("/usr/bin/afplay")
            .args(["-v", "0.3", "/System/Library/Sounds/Ping.aiff"])
            .status()
            .unwrap();
        assert!(status.success());
        let mut loudest = 0_i16;
        let mut count = 0;
        // The stream also delivers silence, so it is read for a fixed time.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while let Ok(packet) =
            packets.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            assert_eq!(
                u32::from_le_bytes(packet[..4].try_into().unwrap()),
                SAMPLE_RATE
            );
            assert_eq!(
                u16::from_le_bytes(packet[4..6].try_into().unwrap()),
                CHANNELS
            );
            for sample in packet[HEADER..].chunks_exact(2) {
                loudest = loudest.max(i16::from_le_bytes([sample[0], sample[1]]).saturating_abs());
            }
            count += 1;
        }
        assert!(capture.healthy());
        assert!(count > 10, "only {count} packets");
        assert!(loudest > 100, "the captured audio is silent");
    }
}
