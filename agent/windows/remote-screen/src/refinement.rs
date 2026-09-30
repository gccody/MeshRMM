//! Sharpens a static desktop after a keyframe.
//!
//! The single-frame CBR buffer squeezes a whole-screen keyframe into one
//! frame's budget, so the first picture after a start, display swap, or
//! recovery is heavily quantized. Motion refines it through later P-frames,
//! but Desktop Duplication reports no damage on an idle desktop. Re-encoding
//! the unchanged surface lets rate control spend its per-frame budget on the
//! residual detail instead.
//!
//! CBR hardware encoders keep spending that budget on a static image long
//! after it stops improving, so refinement lasts a bounded, bitrate-scaled
//! number of frames. Ordinary updates never start it: Desktop Duplication
//! reports invalidated rather than changed pixels, and some applications
//! repaint whole windows for each caret blink.

/// Stream budget spent refining each pixel. On NVENC, H.264 and HEVC keyframes
/// at 2560x1440 and 12 Mb/s stopped improving after 1-1.3 bits per pixel (PSNR
/// against the final frame); the remainder is margin for other encoders.
const REFINEMENT_BITS_PER_PIXEL: u64 = 3;
const MIN_REFINEMENT_MS: u64 = 250;
const MAX_REFINEMENT_MS: u64 = 3_000;

#[derive(Debug)]
pub(crate) struct StaticRefinement {
    pixels: u64,
    frames_per_second: u64,
    refinement_frames: u64,
    remaining: u64,
}

impl StaticRefinement {
    pub(crate) fn new(
        width: u32,
        height: u32,
        frames_per_second: u32,
        bits_per_second: u32,
    ) -> Self {
        let mut refinement = Self {
            pixels: u64::from(width) * u64::from(height),
            frames_per_second: u64::from(frames_per_second.max(1)),
            refinement_frames: 0,
            remaining: 0,
        };
        refinement.set_bitrate(bits_per_second);
        refinement
    }

    pub(crate) fn set_bitrate(&mut self, bits_per_second: u32) {
        let frame_budget_bits = (u64::from(bits_per_second) / self.frames_per_second).max(1);
        self.refinement_frames = (self.pixels * REFINEMENT_BITS_PER_PIXEL)
            .div_ceil(frame_budget_bits)
            .clamp(
                self.frames_per_second * MIN_REFINEMENT_MS / 1_000,
                self.frames_per_second * MAX_REFINEMENT_MS / 1_000,
            );
    }

    /// Whether an idle desktop should re-encode its last surface.
    pub(crate) fn pending(&self) -> bool {
        self.remaining > 0
    }

    pub(crate) fn refined(&mut self) {
        self.remaining = self.remaining.saturating_sub(1);
    }

    /// A keyframe replaces the whole picture, whatever input produced it.
    pub(crate) fn encoded(&mut self, keyframe: bool) {
        if keyframe {
            self.remaining = self.refinement_frames;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refine_until_idle(refinement: &mut StaticRefinement) -> u64 {
        let mut frames = 0;
        while refinement.pending() {
            refinement.refined();
            refinement.encoded(false);
            frames += 1;
        }
        frames
    }

    #[test]
    fn keyframe_is_refined_for_a_bitrate_scaled_number_of_frames() {
        let mut refinement = StaticRefinement::new(2560, 1440, 60, 12_000_000);
        assert!(!refinement.pending());
        refinement.encoded(false);
        assert!(!refinement.pending(), "ordinary updates are not refined");
        refinement.encoded(true);
        // 3 bits/pixel of a 200 kbit frame budget.
        assert_eq!(refine_until_idle(&mut refinement), 56);
    }

    #[test]
    fn a_new_keyframe_restarts_refinement() {
        let mut refinement = StaticRefinement::new(1920, 1080, 60, 12_000_000);
        refinement.encoded(true);
        let full = refinement.remaining;
        refinement.refined();
        refinement.refined();
        refinement.encoded(true);
        assert_eq!(refinement.remaining, full);
    }

    #[test]
    fn refinement_length_is_clamped_and_follows_bitrate_changes() {
        let mut refinement = StaticRefinement::new(1920, 1080, 60, 50_000_000);
        refinement.encoded(true);
        assert_eq!(refine_until_idle(&mut refinement), 15);
        refinement.set_bitrate(500_000);
        refinement.encoded(true);
        assert_eq!(refine_until_idle(&mut refinement), 180);
        refinement.set_bitrate(6_000_000);
        refinement.encoded(true);
        assert_eq!(refine_until_idle(&mut refinement), 63);
    }
}
