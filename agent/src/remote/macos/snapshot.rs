//! One still image of the main display, as a JPEG for the dashboard's
//! thumbnail. A short ScreenCaptureKit stream delivers a frame already
//! scaled to the thumbnail's size, which AppKit encodes.
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImageCompressionFactor};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};

use crate::remote::thumbnail::{Capture, Layout, Pixels, Signature, encode_if_changed};

const TIMEOUT: Duration = Duration::from_secs(5);
const JPEG_QUALITY: f64 = 0.7;

type FrameSender = mpsc::SyncSender<CFRetained<CVPixelBuffer>>;

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[name = "MeshRMMSnapshotOutput"]
    #[ivars = Mutex<Option<FrameSender>>]
    struct SnapshotOutput;

    unsafe impl NSObjectProtocol for SnapshotOutput {}

    unsafe impl SCStreamOutput for SnapshotOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle and blank updates carry no image.
            // SAFETY: the sample buffer is valid for the duration of the callback.
            let Some(frame) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            // Only the first frame is wanted.
            if let Some(sender) = self.ivars().lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = sender.try_send(frame);
            }
        }
    }
);

impl SnapshotOutput {
    fn new(sender: FrameSender) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Mutex::new(Some(sender)));
        // SAFETY: NSObject's init is always valid.
        unsafe { msg_send![super(this), init] }
    }
}

/// The main display, scaled to fit the thumbnail box, as a JPEG unless it
/// looks like `previous`.
pub(crate) fn main_display(previous: Option<&Signature>) -> anyhow::Result<Capture> {
    let displays = super::display::enumerate()?;
    let display = super::display::choose(&displays, None)?;
    let (width, height) = super::display::pixel_size(&display);
    let (width, height) = crate::remote::thumbnail::scaled_size(width, height);
    let frame = capture_frame(display.id.0, width, height)?;
    encode_if_changed(signature(&frame)?, previous, || encode_jpeg(&frame))
}

/// What the BGRA frame looks like, read while its pixels are locked.
fn signature(frame: &CVPixelBuffer) -> anyhow::Result<Signature> {
    // SAFETY: the frame is locked for reading while its bytes are borrowed,
    // and the slice covers no more than the buffer holds.
    unsafe {
        if CVPixelBufferLockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly) != 0 {
            bail!("Core Video could not lock the thumbnail frame");
        }
        let base = CVPixelBufferGetBaseAddress(frame).cast::<u8>();
        let (width, height) = (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame));
        let stride = CVPixelBufferGetBytesPerRow(frame);
        let signature = if base.is_null() {
            Err(anyhow::anyhow!("the thumbnail frame has no pixels"))
        } else {
            let length = CVPixelBufferGetDataSize(frame).min(stride.saturating_mul(height));
            let data = std::slice::from_raw_parts(base, length);
            Pixels::new(data, width, height, stride, Layout::BGRX)
                .map(|pixels| Signature::of(&pixels))
        };
        CVPixelBufferUnlockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly);
        signature
    }
}

fn capture_frame(
    display_id: u32,
    width: u32,
    height: u32,
) -> anyhow::Result<CFRetained<CVPixelBuffer>> {
    let filter = super::capture::content_filter(display_id, &[])?;
    let (sender, frames) = mpsc::sync_channel(1);
    let output = SnapshotOutput::new(sender);
    // SAFETY: the stream is created and configured with valid arguments
    // before it starts.
    let stream = unsafe {
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(width as usize);
        configuration.setHeight(height as usize);
        configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
        configuration.setMinimumFrameInterval(CMTime::new(1, 10));
        configuration.setQueueDepth(3);
        configuration.setShowsCursor(false);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            None,
        );
        let queue = DispatchQueue::new("com.meshrmm.agent.snapshot", None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "ScreenCaptureKit rejected the frame output: {}",
                    error.localizedDescription()
                )
            })?;
        stream
    };
    let (started_tx, started) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes a valid error or null.
        let _ = started_tx
            .send(unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string()));
    });
    // SAFETY: the stream is fully configured.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match started.recv_timeout(TIMEOUT) {
        Ok(None) => {}
        Ok(Some(error)) => bail!("ScreenCaptureKit could not start capture: {error}"),
        Err(_) => bail!("ScreenCaptureKit did not start capture in time"),
    }
    let frame = frames.recv_timeout(TIMEOUT);
    let (stopped_tx, stopped) = mpsc::channel();
    let handler = RcBlock::new(move |_error: *mut NSError| {
        let _ = stopped_tx.send(());
    });
    // SAFETY: stopping a started stream is always valid.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    let _ = stopped.recv_timeout(TIMEOUT);
    frame.context("ScreenCaptureKit delivered no frame in time")
}

fn encode_jpeg(frame: &CVPixelBuffer) -> anyhow::Result<Vec<u8>> {
    let mut image: *mut CGImage = std::ptr::null_mut();
    // SAFETY: the frame is a valid pixel buffer and `image` a valid out pointer.
    let status = unsafe {
        objc2_video_toolbox::VTCreateCGImageFromCVPixelBuffer(
            frame,
            None,
            std::ptr::NonNull::from(&mut image),
        )
    };
    let image = std::ptr::NonNull::new(image)
        .filter(|_| status == 0)
        .with_context(|| format!("VideoToolbox could not convert the frame ({status})"))?;
    // SAFETY: the function returns a +1 reference.
    let image = unsafe { CFRetained::from_raw(image) };
    let representation = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &image);
    let quality = NSNumber::new_f64(JPEG_QUALITY);
    // SAFETY: the key is AppKit's and the value an NSNumber, as documented.
    let properties = NSDictionary::from_slices(
        &[unsafe { NSImageCompressionFactor }],
        &[AsRef::<AnyObject>::as_ref(&*quality)],
    );
    // SAFETY: the properties hold only keys this file type accepts.
    let data = unsafe {
        representation.representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties)
    }
    .context("AppKit could not encode the thumbnail")?;
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a display and Screen Recording permission"]
    fn captures_the_main_display_as_a_small_jpeg() {
        let Capture::Changed { jpeg, .. } = main_display(None).unwrap() else {
            panic!("a capture without an image to compare with was not encoded");
        };
        assert!(jpeg.starts_with(&[0xff, 0xd8]), "a JPEG");
        assert!(jpeg.len() > 1000);
        std::fs::write(std::env::temp_dir().join("meshrmm-thumbnail.jpg"), &jpeg).unwrap();
    }
}
