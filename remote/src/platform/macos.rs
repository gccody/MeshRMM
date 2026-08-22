use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSCursor, NSEvent,
    NSEventModifierFlags, NSFont, NSFontWeightRegular, NSProgressIndicator,
    NSProgressIndicatorStyle, NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_av_foundation::{AVLayerVideoGravityResizeAspect, AVSampleBufferDisplayLayer};
use objc2_core_foundation::{CFBoolean, CFMutableDictionary, CFRetained, CFString, kCFBooleanTrue};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMSampleAttachmentKey_DisplayImmediately,
    kCMTimeInvalid,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use objc2_quartz_core::CAAutoresizingMask;
use pulsermm_protocol::{
    Codec, CursorShape, Display, EncodedFrame, PointerButton, RemoteInput, SessionMessage,
    VideoFormat,
};

use super::ControlSink;
use crate::debug::DebugInfo;
use crate::h264::annex_b_to_avcc;

mod app;
mod presenter;

use app::close_connecting_window;
#[cfg(test)]
use app::normalized_video_position;
pub use app::{monotonic_timestamp_us, run_application};
pub use presenter::Presenter;

static NEXT_PRESENTER_ID: AtomicU64 = AtomicU64::new(1);

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
