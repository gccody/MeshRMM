//! What the Windows viewer rebuilds when the device replaces its video stream
//! and the viewer keeps its window: the presenter decides this from the old
//! and new stream alone, so the rules are tested on every platform.

use meshrmm_protocol::{DisplayId, VideoFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetPlan {
    /// The video processor is created for one input size, frame rate and
    /// pixel format.
    pub recreate_processor: bool,
    /// The frame kept for redraws no longer fits the new processor input.
    /// The swap chain keeps showing it until the replacement keyframe.
    pub drop_last_frame: bool,
    /// Held keys and buttons were pressed on the old display; they are
    /// released before input goes to the new one.
    pub display_changed: bool,
}

impl ResetPlan {
    pub fn between(
        old_format: VideoFormat,
        new_format: VideoFormat,
        old_display: DisplayId,
        new_display: DisplayId,
    ) -> Self {
        let resized =
            old_format.width != new_format.width || old_format.height != new_format.height;
        let pixels_changed = old_format.pixel_format != new_format.pixel_format;
        Self {
            recreate_processor: resized
                || pixels_changed
                || old_format.frames_per_second != new_format.frames_per_second,
            drop_last_frame: resized || pixels_changed,
            display_changed: old_display != new_display,
        }
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{Codec, PixelFormat};

    use super::*;

    const CURRENT: VideoFormat = VideoFormat {
        width: 1920,
        height: 1080,
        frames_per_second: 60,
        codec: Codec::H264,
        pixel_format: PixelFormat::Nv12,
        bitrate_bits_per_second: 12_000_000,
    };

    fn plan(next: VideoFormat) -> ResetPlan {
        ResetPlan::between(CURRENT, next, DisplayId(1), DisplayId(1))
    }

    #[test]
    fn an_unchanged_stream_keeps_the_processor_and_the_last_frame() {
        assert_eq!(
            plan(CURRENT),
            ResetPlan {
                recreate_processor: false,
                drop_last_frame: false,
                display_changed: false,
            }
        );
    }

    #[test]
    fn codec_and_bitrate_changes_only_replace_the_decoder() {
        // HEVC bitrate steps and codec fallbacks decode to the same surfaces.
        assert_eq!(
            plan(VideoFormat {
                codec: Codec::H265,
                bitrate_bits_per_second: 4_000_000,
                ..CURRENT
            }),
            plan(CURRENT)
        );
    }

    #[test]
    fn a_new_resolution_recreates_the_processor_and_drops_the_last_frame() {
        for (width, height) in [(2560, 1440), (1080, 1920), (1920, 1200)] {
            let plan = plan(VideoFormat {
                width,
                height,
                ..CURRENT
            });
            assert!(plan.recreate_processor, "{width}x{height}");
            assert!(plan.drop_last_frame, "{width}x{height}");
        }
    }

    #[test]
    fn a_new_frame_rate_recreates_the_processor_but_keeps_the_last_frame() {
        let plan = plan(VideoFormat {
            frames_per_second: 30,
            ..CURRENT
        });
        assert!(plan.recreate_processor);
        assert!(!plan.drop_last_frame);
    }

    #[test]
    fn a_chroma_change_recreates_the_processor_and_drops_the_last_frame() {
        // A 4:2:0 surface cannot feed a processor created for 4:4:4 input.
        let plan = plan(VideoFormat {
            pixel_format: PixelFormat::Ayuv,
            ..CURRENT
        });
        assert!(plan.recreate_processor);
        assert!(plan.drop_last_frame);
    }

    #[test]
    fn only_a_new_display_id_releases_held_input() {
        assert!(ResetPlan::between(CURRENT, CURRENT, DisplayId(1), DisplayId(2)).display_changed);
        assert!(!ResetPlan::between(CURRENT, CURRENT, DisplayId(2), DisplayId(2)).display_changed);
    }
}
