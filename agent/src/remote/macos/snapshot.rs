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
use objc2_core_video::{CVPixelBuffer, kCVPixelFormatType_32BGRA};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};

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

/// The main display, scaled to fit the thumbnail box, as a JPEG.
pub(crate) fn main_display_jpeg() -> anyhow::Result<Vec<u8>> {
    let displays = super::display::enumerate()?;
    let display = super::display::choose(&displays, None)?;
    let (width, height) = super::display::pixel_size(&display);
    let (width, height) = crate::remote::thumbnail::scaled_size(width, height);
    let frame = capture_frame(display.id.0, width, height)?;
    encode_jpeg(&frame)
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
    let jpeg = data.to_vec();
    anyhow::ensure!(
        jpeg.len() <= crate::remote::thumbnail::MAX_BYTES,
        "the thumbnail is {} bytes, more than the server accepts",
        jpeg.len()
    );
    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a display and Screen Recording permission"]
    fn captures_the_main_display_as_a_small_jpeg() {
        let jpeg = main_display_jpeg().unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8]), "a JPEG");
        assert!(jpeg.len() > 1000);
        std::fs::write(std::env::temp_dir().join("meshrmm-thumbnail.jpg"), &jpeg).unwrap();
    }
}
