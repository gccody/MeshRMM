use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use dispatch2::DispatchQueue;
use meshrmm_protocol::{
    ChromaMode, Codec, CursorShape, Display, EncodedFrame, PointerButton, QualityPreset,
    RemoteInput, SessionMessage, VideoFormat, VideoProfile, VideoStreamId,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOn,
    NSCursor, NSEvent, NSEventModifierFlags, NSFont, NSFontWeightRegular, NSPopUpButton,
    NSProgressIndicator, NSProgressIndicatorStyle, NSTabView, NSTabViewItem, NSTextAlignment,
    NSTextField, NSView, NSWindow, NSWindowDelegate, NSWindowOrderingMode, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspect, AVQueuedSampleBufferRenderingStatus,
    AVSampleBufferDisplayLayer,
};
use objc2_core_foundation::{CFBoolean, CFMutableDictionary, CFRetained, CFString, kCFBooleanTrue};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, kCMSampleAttachmentKey_DisplayImmediately,
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};

use super::ControlSink;
use crate::debug::DebugInfo;
use crate::h264::annex_b_to_length_prefixed;

mod app;
mod presenter;

use app::close_connecting_window;
#[cfg(test)]
use app::normalized_video_position;
pub use app::{monotonic_timestamp_us, run_application};
pub use presenter::Presenter;

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTIsHardwareDecodeSupported(codec_type: u32) -> u8;
}

fn hardware_decode_supported(codec: Codec) -> bool {
    let codec_type = match codec {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::H265 => kCMVideoCodecType_HEVC,
    };
    unsafe { VTIsHardwareDecodeSupported(codec_type) != 0 }
}

pub fn supported_video_profiles(_format: VideoFormat) -> Vec<VideoProfile> {
    [Codec::H265, Codec::H264]
        .into_iter()
        .filter(|codec| hardware_decode_supported(*codec))
        .map(|codec| VideoProfile {
            codec,
            chroma: ChromaMode::Yuv420,
        })
        .collect()
}

static NEXT_PRESENTER_ID: AtomicU64 = AtomicU64::new(1);
const VIEWER_TOOLBAR_HEIGHT: f64 = 36.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_position_uses_aspect_fit_video_rect() {
        let bounds = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 1_000.0,
                height: 1_000.0,
            },
        };

        assert_eq!(
            normalized_video_position(NSPoint { x: 500.0, y: 500.0 }, bounds, 1_920, 1_080,),
            Some((32_768, 32_768))
        );
        assert_eq!(
            normalized_video_position(NSPoint { x: 0.0, y: 500.0 }, bounds, 1_920, 1_080),
            Some((0, 32_768))
        );
        assert_eq!(
            normalized_video_position(
                NSPoint {
                    x: 1_000.0,
                    y: 500.0,
                },
                bounds,
                1_920,
                1_080,
            ),
            Some((65_535, 32_768))
        );
        assert_eq!(
            normalized_video_position(NSPoint { x: 500.0, y: 0.0 }, bounds, 1_920, 1_080),
            None
        );
        assert_eq!(
            normalized_video_position(
                NSPoint {
                    x: 500.0,
                    y: 1_000.0
                },
                bounds,
                1_920,
                1_080
            ),
            None
        );
    }
}
