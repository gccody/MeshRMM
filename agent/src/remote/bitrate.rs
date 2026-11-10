//! Video rate control shared by the sender: fragment pacing (which leaves
//! room for audio) and the live AIMD bitrate controller, and the restart
//! ladder for encoders that cannot change bitrate live.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// Video queued behind the network, in time at the current bitrate (see
// `MeteredChannel::queued`): bytes in flight are not counted, so these mean the
// same at any round trip.
const VIDEO_BUFFER_DRAIN_MS: u32 = 50;
const VIDEO_BUFFER_CONGESTED_MS: u32 = 150;
const BITRATE_DECREASE_INTERVAL_US: u64 = 500_000;
const BITRATE_INCREASE_INTERVAL_US: u64 = 1_000_000;

// The lowest rate at which full-colour desktop video at usual resolutions
// stays legible; ceilings below 2 Mbit/s (Ultra Data Saver's grayscale) get a
// quarter of their ceiling instead, so every ceiling has room to adapt down.
const MINIMUM_VIDEO_BITRATE: u32 = 500_000;

/// The lowest rate the bitrate controller and the restart ladder go to under
/// `maximum`: an eighth of it, at least 500 kbit/s, and never more than a
/// quarter of it.
fn bitrate_floor(maximum: u32) -> u32 {
    (maximum / 8)
        .max(MINIMUM_VIDEO_BITRATE)
        .min(maximum / 4)
        .max(1)
}

// Encoder CBR is a target, not a transport limit. Pace video fragments even
// when the network is fast enough to hide encoder overshoot from congestion
// control. Allow a small burst, but never accumulate credit while idle.
const VIDEO_PACING_BURST_US: u64 = 20_000;

#[derive(Default)]
pub(super) struct VideoPacer {
    next_send_us: u64,
}

impl VideoPacer {
    pub(super) fn reserve(&mut self, now_us: u64, bytes: usize, bits_per_second: u32) -> u64 {
        let duration_us = (bytes as u64)
            .saturating_mul(8_000_000)
            .div_ceil(u64::from(bits_per_second.max(1)));
        self.next_send_us = self.next_send_us.max(now_us).saturating_add(duration_us);
        self.next_send_us
            .saturating_sub(now_us.saturating_add(VIDEO_PACING_BURST_US))
    }
}

/// The rate video fragments are paced at: the quality ceiling less the audio
/// the sender is currently streaming, but never below half the ceiling.
pub(super) fn video_pacing_bitrate(ceiling: u32, audio_bits_per_second: u32) -> u32 {
    ceiling
        .saturating_sub(audio_bits_per_second)
        .max(ceiling / 2)
}

// Frames the encoder makes while the sender paces out a large keyframe wait
// behind it. As many as are sent in this long at the pacing rate may wait
// (see `LatestFrameSlot::set_byte_budget`): the latency they add once the
// keyframe is through.
const VIDEO_BACKLOG_MS: u32 = 250;

/// The bytes of waiting frames the sender sends in `VIDEO_BACKLOG_MS` at
/// `pacing_bits_per_second` (see [`video_pacing_bitrate`]).
pub(super) fn video_backlog_bytes(pacing_bits_per_second: u32) -> usize {
    bitrate_duration_bytes(pacing_bits_per_second, VIDEO_BACKLOG_MS)
}

/// Cuts the bitrate by a quarter at most every 500 ms while the video queue
/// is congested, and raises it after each healthy second: by a quarter while
/// it is below 90% of the rate the last cut was made from, by 5% from there to
/// 125% of that rate, and by a quarter again above it, where the link has
/// evidently improved. A link that clears comes back from the floor to the
/// ceiling in 10 to 15 s, and one whose capacity lies between them holds the
/// rate between about 75% and 105% of that capacity.
#[derive(Debug)]
pub(super) struct AdaptiveBitrate {
    minimum: u32,
    maximum: u32,
    current: u32,
    /// The rate the last cut was made from, or 0 once the rate has climbed
    /// well past it.
    congested_bitrate: u32,
    last_decrease_us: u64,
    healthy_since_us: u64,
}

impl AdaptiveBitrate {
    pub(super) fn new(maximum: u32) -> Self {
        let maximum = maximum.max(1);
        Self {
            minimum: bitrate_floor(maximum),
            maximum,
            current: maximum,
            congested_bitrate: 0,
            last_decrease_us: 0,
            healthy_since_us: 0,
        }
    }

    /// A new quality ceiling. A rate the network held below the old ceiling
    /// stays in force, within the new ceiling and floor, and climbs from
    /// there; a rate at the old ceiling moves to the new one at once.
    /// [`StartBitrate`] starts encoders by the same rule.
    pub(super) fn set_maximum(&mut self, maximum: u32) -> Option<u32> {
        let maximum = maximum.max(1);
        if self.maximum == maximum {
            return None;
        }
        let adapted = self.current < self.maximum;
        self.maximum = maximum;
        self.minimum = bitrate_floor(maximum);
        self.current = if adapted {
            self.current.clamp(self.minimum, maximum)
        } else {
            maximum
        };
        self.last_decrease_us = 0;
        self.healthy_since_us = 0;
        Some(self.current)
    }

    pub(super) fn observe(
        &mut self,
        now_us: u64,
        queued_bytes: usize,
        queued_frames: usize,
        reference_chain_lost: bool,
    ) -> Option<u32> {
        if self.congested(queued_bytes, queued_frames, reference_chain_lost) {
            self.healthy_since_us = 0;
            if self.last_decrease_us == 0
                || now_us.saturating_sub(self.last_decrease_us) >= BITRATE_DECREASE_INTERVAL_US
            {
                self.last_decrease_us = now_us.max(1);
                let reduced = ((u64::from(self.current) * 3) / 4) as u32;
                let reduced = reduced.max(self.minimum);
                if reduced < self.current {
                    self.congested_bitrate = self.current;
                    self.current = reduced;
                    return Some(self.current);
                }
            }
            return None;
        }

        if !self.healthy(queued_bytes, queued_frames) || self.current >= self.maximum {
            self.healthy_since_us = 0;
            return None;
        }
        if self.healthy_since_us == 0 {
            self.healthy_since_us = now_us.max(1);
            return None;
        }
        if now_us.saturating_sub(self.healthy_since_us) >= BITRATE_INCREASE_INTERVAL_US {
            self.healthy_since_us = now_us.max(1);
            self.current = self.increased().min(self.maximum);
            return Some(self.current);
        }
        None
    }

    fn increased(&mut self) -> u32 {
        let current = u64::from(self.current);
        let congested = u64::from(self.congested_bitrate);
        let quick = current * 5 / 4;
        let increased = if congested == 0 || current >= congested * 5 / 4 {
            self.congested_bitrate = 0;
            quick
        } else if current < congested * 9 / 10 {
            quick.min(congested * 9 / 10)
        } else {
            current + current / 20
        };
        u32::try_from(increased.max(current + 1)).unwrap_or(u32::MAX)
    }

    /// Follows an encoder whose bitrate is fixed at start, so thresholds track
    /// the rate actually being sent instead of an adjustment never applied.
    pub(super) fn rebase(&mut self, bits_per_second: u32) {
        let bits_per_second = bits_per_second.max(1);
        if self.current == bits_per_second {
            return;
        }
        self.current = bits_per_second;
        self.last_decrease_us = 0;
        self.healthy_since_us = 0;
    }

    pub(super) fn congested(
        &self,
        queued_bytes: usize,
        queued_frames: usize,
        reference_chain_lost: bool,
    ) -> bool {
        reference_chain_lost
            || queued_bytes >= self.congested_bytes()
            || queued_frames >= (super::video::MAX_ENCODED_FRAME_QUEUE * 4) / 5
    }

    pub(super) fn healthy(&self, queued_bytes: usize, queued_frames: usize) -> bool {
        queued_bytes <= self.drain_bytes() && queued_frames <= 1
    }

    pub(super) fn drain_bytes(&self) -> usize {
        bitrate_duration_bytes(self.current, VIDEO_BUFFER_DRAIN_MS)
    }

    pub(super) fn congested_bytes(&self) -> usize {
        bitrate_duration_bytes(self.current, VIDEO_BUFFER_CONGESTED_MS)
    }
}

fn bitrate_duration_bytes(bits_per_second: u32, duration_ms: u32) -> usize {
    usize::try_from(
        u64::from(bits_per_second)
            .saturating_mul(u64::from(duration_ms))
            .div_ceil(8_000),
    )
    .unwrap_or(usize::MAX)
    .max(16 * 1024)
}

// HEVC encoders only change bitrate by restarting (see `RestartLadder`).
const LADDER_STEPS_PERCENT: [u64; 5] = [100, 70, 50, 35, 25];
const LADDER_CONGESTED_US: u64 = 2_500_000;
// Frame drops and keyframe recovery briefly empty the buffer during a
// congestion episode; short clear gaps do not end it.
const LADDER_CONGESTION_GAP_US: u64 = 1_000_000;
const LADDER_RESTART_SPACING_US: u64 = 5_000_000;
const LADDER_INITIAL_HOLD_US: u64 = 20_000_000;
const LADDER_MAXIMUM_HOLD_US: u64 = 120_000_000;
const LADDER_FAILED_STEP_UP_US: u64 = 10_000_000;
const LADDER_STABLE_STEP_UP_US: u64 = 60_000_000;

/// Chooses static bitrates for encoders whose bitrate cannot change live.
///
/// Several hardware HEVC encoders accept a live CodecAPI bitrate change and
/// then terminate asynchronously, so HEVC adapts by restarting one step lower
/// or higher. Each restart costs an IDR, so steps need sustained congestion,
/// are spaced apart, and step-ups wait for a hold that grows when a previous
/// step-up congested soon after.
#[derive(Debug)]
pub(super) struct RestartLadder {
    maximum: u32,
    congested_since_us: u64,
    last_congested_us: u64,
    healthy_since_us: u64,
    last_restart_us: u64,
    last_step_up_us: u64,
    hold_us: u64,
}

impl RestartLadder {
    pub(super) fn new(maximum: u32) -> Self {
        Self {
            maximum: maximum.max(1),
            congested_since_us: 0,
            last_congested_us: 0,
            healthy_since_us: 0,
            last_restart_us: 0,
            last_step_up_us: 0,
            hold_us: LADDER_INITIAL_HOLD_US,
        }
    }

    /// A quality change restarts the encoder by [`StartBitrate`]'s rule.
    pub(super) fn set_maximum(&mut self, maximum: u32) {
        let maximum = maximum.max(1);
        if self.maximum != maximum {
            *self = Self::new(maximum);
        }
    }

    fn steps(&self) -> [u32; LADDER_STEPS_PERCENT.len()] {
        let floor = bitrate_floor(self.maximum);
        LADDER_STEPS_PERCENT
            .map(|percent| ((u64::from(self.maximum) * percent / 100) as u32).max(floor))
    }

    /// Returns the bitrate to restart the encoder at, given the bitrate it is
    /// actually running at.
    pub(super) fn observe(
        &mut self,
        now_us: u64,
        encoder_bitrate: u32,
        congested: bool,
        healthy: bool,
        recording: bool,
    ) -> Option<u32> {
        let now_us = now_us.max(1);
        let steps = self.steps();
        let spaced = self.last_restart_us == 0
            || now_us.saturating_sub(self.last_restart_us) >= LADDER_RESTART_SPACING_US;
        if congested {
            self.healthy_since_us = 0;
            if self.congested_since_us == 0
                || now_us.saturating_sub(self.last_congested_us) > LADDER_CONGESTION_GAP_US
            {
                self.congested_since_us = now_us;
            }
            self.last_congested_us = now_us;
            if now_us.saturating_sub(self.congested_since_us) < LADDER_CONGESTED_US {
                return None;
            }
            if self.last_step_up_us != 0 {
                if self.congested_since_us.saturating_sub(self.last_step_up_us)
                    < LADDER_FAILED_STEP_UP_US
                {
                    self.hold_us = (self.hold_us * 2).min(LADDER_MAXIMUM_HOLD_US);
                }
                self.last_step_up_us = 0;
            }
            if !spaced {
                return None;
            }
            let lower = steps.into_iter().find(|&step| step < encoder_bitrate)?;
            self.last_restart_us = now_us;
            self.congested_since_us = 0;
            self.last_congested_us = 0;
            return Some(lower);
        }

        if self.last_step_up_us != 0
            && now_us.saturating_sub(self.last_step_up_us) >= LADDER_STABLE_STEP_UP_US
        {
            self.hold_us = LADDER_INITIAL_HOLD_US;
            self.last_step_up_us = 0;
        }
        let Some(higher) = steps.into_iter().rev().find(|&step| step > encoder_bitrate) else {
            self.healthy_since_us = 0;
            return None;
        };
        // Each restart starts a new recording part, so recordings only step down.
        if !healthy || recording {
            self.healthy_since_us = 0;
            return None;
        }
        if self.healthy_since_us == 0 {
            self.healthy_since_us = now_us;
            return None;
        }
        if now_us.saturating_sub(self.healthy_since_us) < self.hold_us || !spaced {
            return None;
        }
        self.healthy_since_us = 0;
        self.last_restart_us = now_us;
        self.last_step_up_us = now_us;
        Some(higher)
    }
}

/// The bitrate an encoder starts at: the quality ceiling, or the rate the
/// network last held the session to while that is below the ceiling, so a
/// capture restart (a display, codec or quality change, a desktop or helper
/// restart) carries on at the adapted rate instead of bursting at the ceiling.
#[derive(Debug, Clone, Copy)]
pub(super) struct StartBitrate {
    ceiling: u32,
    adapted: Option<u32>,
}

impl StartBitrate {
    pub(super) fn new(ceiling: u32) -> Self {
        Self {
            ceiling: ceiling.max(1),
            adapted: None,
        }
    }

    /// The quality ceiling. An adapted rate stays, as it does in
    /// [`AdaptiveBitrate::set_maximum`].
    pub(super) fn set_ceiling(&mut self, ceiling: u32) {
        self.ceiling = ceiling.max(1);
    }

    /// The rate the network allows; None when it allows the ceiling.
    pub(super) fn adapt(&mut self, bits_per_second: Option<u32>) {
        self.adapted = bits_per_second.map(|value| value.max(1));
    }

    pub(super) fn bits_per_second(&self) -> u32 {
        self.adapted.map_or(self.ceiling, |adapted| {
            adapted.clamp(bitrate_floor(self.ceiling), self.ceiling)
        })
    }
}

/// The running encoder's configuration, published by capture control after
/// every start and read by the video sender for each frame.
#[derive(Debug, Default)]
pub(super) struct EncoderStatus {
    bits_per_second: AtomicU32,
    live_bitrate: AtomicBool,
    recording: AtomicBool,
}

impl EncoderStatus {
    pub(super) fn publish(&self, bits_per_second: u32, live_bitrate: bool, recording: bool) {
        self.bits_per_second
            .store(bits_per_second.max(1), Ordering::Release);
        self.live_bitrate.store(live_bitrate, Ordering::Release);
        self.recording.store(recording, Ordering::Release);
    }

    pub(super) fn bits_per_second(&self) -> u32 {
        self.bits_per_second.load(Ordering::Acquire).max(1)
    }

    /// Whether the encoder applies bitrate changes without a restart.
    pub(super) fn live_bitrate(&self) -> bool {
        self.live_bitrate.load(Ordering::Acquire)
    }

    pub(super) fn recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::{MAX_QUALITY_BITRATE_BITS_PER_SECOND, QualityPreset};
    use meshrmm_session_transport::SendQueue;
    use meshrmm_session_transport::simulated_link::SimulatedLink;
    use std::time::{Duration, Instant};

    #[test]
    fn video_pacing_bounds_sustained_overshoot_and_idle_bursts() {
        let mut pacer = VideoPacer::default();
        let mut now = 1_000_000;
        let started = now;
        let mut bytes = 0;
        // Model an encoder producing far more than Data Saver's 3 Mbps.
        for _ in 0..1000 {
            now += pacer.reserve(now, 12_000, 3_000_000);
            bytes += 12_000_u64;
            assert!(bytes * 8_000_000 <= (now - started + VIDEO_PACING_BURST_US) * 3_000_000);
        }
        now += 60_000_000;
        assert_eq!(pacer.reserve(now, 12_000, 3_000_000), 12_000);
    }

    #[test]
    fn video_pacing_applies_quality_changes_to_the_next_fragment() {
        let mut pacer = VideoPacer::default();
        assert_eq!(pacer.reserve(1_000_000, 12_000, 12_000_000), 0);
        assert_eq!(pacer.reserve(1_008_000, 12_000, 3_000_000), 12_000);
        assert_eq!(pacer.reserve(2_000_000, 12_000, 6_000_000), 0);
        assert_eq!(pacer.reserve(2_000_000, 12_000, 6_000_000), 12_000);
    }

    #[test]
    fn video_pacing_leaves_room_for_audio_down_to_half_the_ceiling() {
        assert_eq!(video_pacing_bitrate(3_000_000, 0), 3_000_000);
        assert_eq!(video_pacing_bitrate(3_000_000, 110_000), 2_890_000);
        assert_eq!(video_pacing_bitrate(1_000_000, 1_536_000), 500_000);
        assert_eq!(video_pacing_bitrate(2, u32::MAX), 1);
    }

    #[test]
    fn adaptive_bitrate_uses_aimd_without_oscillating() {
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        let congested_bytes = bitrate.congested_bytes();

        assert_eq!(
            bitrate.observe(1_000, congested_bytes, 0, false),
            Some(9_000_000)
        );
        let congested_bytes = bitrate.congested_bytes();
        assert_eq!(
            bitrate.observe(2_000, congested_bytes, 0, false),
            None,
            "decreases are rate limited"
        );
        let congested_bytes = bitrate.congested_bytes();
        assert_eq!(
            bitrate.observe(
                1_000 + BITRATE_DECREASE_INTERVAL_US,
                congested_bytes,
                0,
                false,
            ),
            Some(6_750_000)
        );

        // The cut was made from 9 Mbit/s: the rate rises quickly to 90% of
        // that, then probes.
        let healthy_start = 2_000_000;
        assert_eq!(bitrate.observe(healthy_start, 0, 0, false), None);
        assert_eq!(
            bitrate.observe(healthy_start + BITRATE_INCREASE_INTERVAL_US, 0, 0, false),
            Some(8_100_000)
        );
        assert_eq!(
            bitrate.observe(
                healthy_start + 2 * BITRATE_INCREASE_INTERVAL_US,
                0,
                0,
                false
            ),
            Some(8_505_000)
        );
    }

    const PRESETS: [QualityPreset; 4] = [
        QualityPreset::UltraDataSaver,
        QualityPreset::DataSaver,
        QualityPreset::Balanced,
        QualityPreset::BestQuality,
    ];

    fn ceiling(preset: QualityPreset) -> u32 {
        preset.bitrate(MAX_QUALITY_BITRATE_BITS_PER_SECOND)
    }

    /// Congests the controller every 500 ms until it can cut no further.
    fn congest_to_floor(bitrate: &mut AdaptiveBitrate, now_us: &mut u64) {
        for _ in 0..40 {
            let _ = bitrate.observe(*now_us, usize::MAX, usize::MAX, true);
            *now_us += BITRATE_DECREASE_INTERVAL_US;
        }
    }

    #[test]
    fn every_preset_has_one_floor_well_below_its_ceiling_from_start_or_mid_session() {
        let floors = [250_000, 500_000, 750_000, 1_500_000];
        for (preset, floor) in PRESETS.into_iter().zip(floors) {
            let maximum = ceiling(preset);
            let mut from_start = AdaptiveBitrate::new(maximum);
            let mut now_us = 1;
            congest_to_floor(&mut from_start, &mut now_us);
            assert_eq!(from_start.current, floor, "{preset:?}");
            assert!(floor <= maximum / 4, "{preset:?}");

            for other in PRESETS.into_iter().filter(|&other| other != preset) {
                let mut mid_session = AdaptiveBitrate::new(ceiling(other));
                assert!(mid_session.set_maximum(maximum).is_some());
                let mut now_us = 1;
                congest_to_floor(&mut mid_session, &mut now_us);
                assert_eq!(mid_session.current, floor, "{preset:?} after {other:?}");
            }
        }
    }

    /// Clears the link at the floor reached from the ceiling and returns the
    /// seconds the rate takes to climb back, judged every 100 ms.
    fn recovery_seconds(bitrate: &mut AdaptiveBitrate) -> u64 {
        let cleared_us = 1_000_000_000;
        let mut now_us = cleared_us;
        loop {
            let _ = bitrate.observe(now_us, 0, 0, false);
            if bitrate.current >= bitrate.maximum {
                return (now_us - cleared_us).div_ceil(1_000_000);
            }
            now_us += 100_000;
            assert!(now_us - cleared_us < 600_000_000, "never recovered");
        }
    }

    #[test]
    fn a_cleared_link_climbs_from_the_floor_to_the_ceiling_within_15_seconds() {
        for preset in PRESETS {
            let mut bitrate = AdaptiveBitrate::new(ceiling(preset));
            let mut now_us = 1;
            congest_to_floor(&mut bitrate, &mut now_us);
            assert_eq!(bitrate.current, bitrate.minimum);
            let seconds = recovery_seconds(&mut bitrate);
            assert!(seconds <= 15, "{preset:?} took {seconds} s");

            // With no congestion to remember, the rate rises by a quarter a
            // second all the way.
            let mut bitrate = AdaptiveBitrate::new(ceiling(preset));
            bitrate.rebase(bitrate.minimum);
            let seconds = recovery_seconds(&mut bitrate);
            assert!(seconds <= 10, "{preset:?} took {seconds} s");
        }
    }

    #[test]
    fn a_link_between_floor_and_ceiling_settles_near_its_capacity() {
        for preset in PRESETS {
            let maximum = ceiling(preset);
            for capacity in [maximum * 3 / 10, maximum / 2, maximum * 7 / 10] {
                let mut bitrate = AdaptiveBitrate::new(maximum);
                assert!(bitrate.minimum < capacity);
                let mut rates = Vec::new();
                // Congested whenever the rate is above the link's capacity.
                for tick in 0..3_000_u64 {
                    let queued = if bitrate.current > capacity {
                        bitrate.congested_bytes()
                    } else {
                        0
                    };
                    let _ = bitrate.observe(1 + tick * 100_000, queued, 0, false);
                    rates.push(u64::from(bitrate.current));
                }
                // The last minute stays near the capacity.
                let settled = &rates[rates.len() - 600..];
                let capacity = u64::from(capacity);
                let lowest = *settled.iter().min().unwrap();
                let highest = *settled.iter().max().unwrap();
                let mean = settled.iter().sum::<u64>() / settled.len() as u64;
                let context = format!("{preset:?} at {capacity}: {lowest}..{highest}, {mean}");
                assert!(lowest * 100 >= capacity * 70, "{context}");
                assert!(highest * 100 <= capacity * 106, "{context}");
                assert!(mean * 100 >= capacity * 85, "{context}");
            }
        }
    }

    #[test]
    fn a_quality_change_keeps_a_rate_the_network_held_down() {
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        let mut now_us = 1;
        congest_to_floor(&mut bitrate, &mut now_us);
        assert_eq!(bitrate.current, 1_500_000);
        // Within the new ceiling and floor.
        assert_eq!(bitrate.set_maximum(6_000_000), Some(1_500_000));
        assert_eq!(bitrate.set_maximum(3_000_000), Some(1_500_000));
        assert_eq!(bitrate.set_maximum(12_000_000), Some(1_500_000));
        // A ceiling below the adapted rate holds it at that ceiling, which a
        // later ceiling then replaces.
        assert_eq!(bitrate.set_maximum(1_000_000), Some(1_000_000));
        assert_eq!(bitrate.set_maximum(6_000_000), Some(6_000_000));
    }

    #[test]
    fn a_restarted_encoder_starts_at_the_rate_the_controller_adapted_to() {
        // Capture control hands every live change to later starts as well.
        fn apply(start: &mut StartBitrate, bits_per_second: u32, ceiling: u32) {
            let value = bits_per_second.min(ceiling);
            start.adapt((value < ceiling).then_some(value));
        }
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        let mut start = StartBitrate::new(12_000_000);
        assert_eq!(start.bits_per_second(), 12_000_000);
        let mut now_us = 1;
        for _ in 0..5 {
            if let Some(value) = bitrate.observe(now_us, usize::MAX, 0, false) {
                apply(&mut start, value, 12_000_000);
            }
            now_us += BITRATE_DECREASE_INTERVAL_US;
        }
        assert!(bitrate.current < 4_000_000);
        assert_eq!(start.bits_per_second(), bitrate.current);

        // A quality change starts the encoder where the controller goes.
        for maximum in [6_000_000, 12_000_000, 3_000_000] {
            start.set_ceiling(maximum);
            if let Some(value) = bitrate.set_maximum(maximum) {
                assert_eq!(start.bits_per_second(), value, "at {maximum}");
                apply(&mut start, value, maximum);
            }
            assert_eq!(start.bits_per_second(), bitrate.current, "at {maximum}");
        }

        // Once the rate is back at the ceiling, starts use the ceiling.
        while bitrate.current < 3_000_000 {
            if let Some(value) = bitrate.observe(now_us, 0, 0, false) {
                apply(&mut start, value, 3_000_000);
            }
            now_us += 100_000;
        }
        assert_eq!(start.bits_per_second(), 3_000_000);
        start.set_ceiling(12_000_000);
        assert_eq!(start.bits_per_second(), 12_000_000);
        assert_eq!(bitrate.set_maximum(12_000_000), Some(12_000_000));
    }

    #[test]
    fn adaptive_bitrate_never_drops_below_its_floor() {
        let mut bitrate = AdaptiveBitrate::new(4_000_000);
        let mut now_us = 1;
        for _ in 0..20 {
            let _ = bitrate.observe(now_us, usize::MAX, usize::MAX, true);
            now_us += BITRATE_DECREASE_INTERVAL_US;
        }
        assert_eq!(bitrate.current, 500_000);
    }

    #[test]
    fn quality_ceiling_change_takes_effect_immediately() {
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        assert_eq!(bitrate.set_maximum(3_000_000), Some(3_000_000));
        assert_eq!(bitrate.current, 3_000_000);
        assert_eq!(bitrate.maximum, 3_000_000);

        assert_eq!(bitrate.set_maximum(6_000_000), Some(6_000_000));
        assert_eq!(bitrate.current, 6_000_000);
        assert_eq!(bitrate.set_maximum(6_000_000), None);
    }

    #[test]
    fn transport_buffer_thresholds_represent_time_not_a_fixed_byte_count() {
        let low = AdaptiveBitrate::new(3_000_000);
        let high = AdaptiveBitrate::new(12_000_000);

        assert_eq!(low.drain_bytes(), 18_750);
        assert_eq!(low.congested_bytes(), 56_250);
        assert_eq!(high.drain_bytes(), 75_000);
        assert_eq!(high.congested_bytes(), 225_000);
    }

    /// Streams at the controller's bitrate over `link` for `seconds`, judging once a
    /// frame, and returns each bitrate change with the time it was made.
    fn stream_over(
        bitrate: &mut AdaptiveBitrate,
        queue: &mut SendQueue,
        link: &mut SimulatedLink,
        start: Instant,
        now: &mut Instant,
        seconds: u64,
    ) -> Vec<(Duration, u32)> {
        let mut changes = Vec::new();
        for tick in 0..seconds * 1_000 {
            let bytes = (bitrate.current / 8_000) as usize;
            link.send(*now, bytes);
            queue.sent(*now, bytes);
            if tick % 33 == 0 {
                let queued = queue.queued(*now, link.buffered(*now));
                let now_us = (*now - start).as_micros() as u64;
                if let Some(bits_per_second) = bitrate.observe(now_us, queued, 0, false) {
                    changes.push((*now - start, bits_per_second));
                }
            }
            *now += Duration::from_millis(1);
        }
        changes
    }

    #[test]
    fn adaptive_bitrate_judges_the_queue_the_same_at_any_round_trip() {
        const MAXIMUM: u32 = 12_000_000;
        let roomy = u64::from(MAXIMUM / 8) * 2;
        for round_trip in [10, 100, 300].map(Duration::from_millis) {
            let start = Instant::now();
            let mut now = start;
            let mut link = SimulatedLink::new(now, roomy, round_trip);
            let mut queue = SendQueue::default();
            let mut bitrate = AdaptiveBitrate::new(MAXIMUM);
            let changes = stream_over(&mut bitrate, &mut queue, &mut link, start, &mut now, 20);
            assert!(
                changes.is_empty(),
                "{round_trip:?}: a link with room changed the bitrate: {changes:?}"
            );

            // A link at half the bitrate is congestion, seen within a second.
            link.set_capacity(roomy / 4);
            let congested_at = now - start;
            let changes = stream_over(&mut bitrate, &mut queue, &mut link, start, &mut now, 5);
            let (first_cut, _) = *changes.first().unwrap_or_else(|| {
                panic!("{round_trip:?}: a link at half the bitrate was not congestion")
            });
            assert!(
                first_cut - congested_at <= Duration::from_secs(1),
                "{round_trip:?}: the first cut came after {:?}",
                first_cut - congested_at
            );
            assert!(bitrate.current < MAXIMUM / 2);

            // Once the link has room again the bitrate climbs back to its maximum.
            link.set_capacity(roomy);
            stream_over(&mut bitrate, &mut queue, &mut link, start, &mut now, 150);
            assert_eq!(
                bitrate.current, MAXIMUM,
                "{round_trip:?}: the bitrate did not recover"
            );
        }
    }

    const SAMPLE_US: u64 = 100_000;

    // Feeds one observation per 100 ms and applies each restart immediately.
    fn drive(
        ladder: &mut RestartLadder,
        encoder: &mut u32,
        from_us: u64,
        until_us: u64,
        state: (bool, bool, bool),
    ) -> Vec<(u64, u32)> {
        let (congested, healthy, recording) = state;
        let mut restarts = Vec::new();
        let mut now_us = from_us;
        while now_us < until_us {
            if let Some(bits_per_second) =
                ladder.observe(now_us, *encoder, congested, healthy, recording)
            {
                *encoder = bits_per_second;
                restarts.push((now_us, bits_per_second));
            }
            now_us += SAMPLE_US;
        }
        restarts
    }

    const CONGESTED: (bool, bool, bool) = (true, false, false);
    const HEALTHY: (bool, bool, bool) = (false, true, false);
    const RECORDING: (bool, bool, bool) = (false, true, true);

    #[test]
    fn restart_ladder_ignores_congestion_shorter_than_its_window() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        assert!(drive(&mut ladder, &mut encoder, 1_000_000, 3_400_000, CONGESTED).is_empty());
        assert!(drive(&mut ladder, &mut encoder, 3_400_000, 4_600_000, HEALTHY).is_empty());
        assert!(drive(&mut ladder, &mut encoder, 4_600_000, 7_000_000, CONGESTED).is_empty());
        assert_eq!(encoder, 20_000_000);
    }

    #[test]
    fn restart_ladder_bridges_short_recovery_gaps_in_one_episode() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        assert!(drive(&mut ladder, &mut encoder, 1_000_000, 2_500_000, CONGESTED).is_empty());
        assert!(drive(&mut ladder, &mut encoder, 2_500_000, 3_000_000, HEALTHY).is_empty());
        assert_eq!(
            drive(&mut ladder, &mut encoder, 3_000_000, 4_000_000, CONGESTED),
            vec![(3_500_000, 14_000_000)]
        );
    }

    #[test]
    fn restart_ladder_steps_once_then_waits_for_restart_spacing() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        assert_eq!(
            drive(&mut ladder, &mut encoder, 1_000_000, 10_000_000, CONGESTED),
            vec![(3_500_000, 14_000_000), (8_500_000, 10_000_000)]
        );
    }

    #[test]
    fn restart_ladder_stops_at_its_floor() {
        let mut ladder = RestartLadder::new(1_000_000);
        let mut encoder = 1_000_000;
        let restarts = drive(&mut ladder, &mut encoder, 1, 120_000_000, CONGESTED);
        assert_eq!(
            restarts
                .into_iter()
                .map(|(_, bits_per_second)| bits_per_second)
                .collect::<Vec<_>>(),
            vec![700_000, 500_000, 350_000, 250_000]
        );

        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        drive(&mut ladder, &mut encoder, 1, 120_000_000, CONGESTED);
        assert_eq!(encoder, 5_000_000);
    }

    #[test]
    fn restart_ladder_steps_up_after_the_hold() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        drive(&mut ladder, &mut encoder, 1_000_000, 3_600_000, CONGESTED);
        assert_eq!(encoder, 14_000_000);
        assert_eq!(
            drive(&mut ladder, &mut encoder, 3_600_000, 30_000_000, HEALTHY),
            vec![(23_600_000, 20_000_000)]
        );
        assert!(drive(&mut ladder, &mut encoder, 30_000_000, 200_000_000, HEALTHY).is_empty());
    }

    #[test]
    fn restart_ladder_doubles_its_hold_after_a_failed_step_up_and_resets_when_stable() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        drive(&mut ladder, &mut encoder, 1_000_000, 3_600_000, CONGESTED);
        drive(&mut ladder, &mut encoder, 3_600_000, 23_700_000, HEALTHY);
        assert_eq!(encoder, 20_000_000);

        // The step-up congests within 10 s, so the next hold is 40 s.
        assert_eq!(
            drive(&mut ladder, &mut encoder, 25_000_000, 29_000_000, CONGESTED),
            vec![(28_600_000, 14_000_000)]
        );
        assert_eq!(
            drive(&mut ladder, &mut encoder, 29_000_000, 80_000_000, HEALTHY),
            vec![(69_000_000, 20_000_000)]
        );

        // That step-up holds for 60 s, restoring the 20 s hold.
        drive(&mut ladder, &mut encoder, 80_000_000, 130_000_000, HEALTHY);
        assert_eq!(
            drive(
                &mut ladder,
                &mut encoder,
                130_000_000,
                133_000_000,
                CONGESTED
            ),
            vec![(132_500_000, 14_000_000)]
        );
        assert_eq!(
            drive(&mut ladder, &mut encoder, 133_000_000, 160_000_000, HEALTHY),
            vec![(153_000_000, 20_000_000)]
        );
    }

    #[test]
    fn restart_ladder_hold_is_capped() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        let mut now_us = 1_000_000;
        let mut holds = Vec::new();
        for _ in 0..5 {
            drive(
                &mut ladder,
                &mut encoder,
                now_us,
                now_us + 5_000_000,
                CONGESTED,
            );
            assert_eq!(encoder, 14_000_000);
            now_us += 5_000_000;
            let healthy_since_us = now_us;
            while ladder
                .observe(now_us, encoder, false, true, false)
                .is_none()
            {
                now_us += SAMPLE_US;
            }
            encoder = 20_000_000;
            holds.push((now_us - healthy_since_us) / 1_000_000);
            // Each step-up congests one second later.
            now_us += 1_000_000;
        }
        assert_eq!(holds, vec![20, 40, 80, 120, 120]);
    }

    #[test]
    fn restart_ladder_resets_when_the_quality_ceiling_changes() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        drive(&mut ladder, &mut encoder, 1_000_000, 3_600_000, CONGESTED);
        assert_eq!(encoder, 14_000_000);

        // The quality change restarts the encoder at the new ceiling; the
        // ladder starts over instead of waiting out the previous spacing.
        ladder.set_maximum(10_000_000);
        encoder = 10_000_000;
        assert_eq!(
            drive(&mut ladder, &mut encoder, 3_600_000, 7_000_000, CONGESTED),
            vec![(6_100_000, 7_000_000)]
        );

        ladder.set_maximum(10_000_000);
        assert!(drive(&mut ladder, &mut encoder, 7_000_000, 8_000_000, CONGESTED).is_empty());
    }

    #[test]
    fn restart_ladder_only_steps_down_while_recording() {
        let mut ladder = RestartLadder::new(20_000_000);
        let mut encoder = 20_000_000;
        drive(&mut ladder, &mut encoder, 1_000_000, 3_600_000, CONGESTED);
        assert!(drive(&mut ladder, &mut encoder, 3_600_000, 200_000_000, RECORDING).is_empty());
        let recording_congested = (true, false, true);
        assert_eq!(
            drive(
                &mut ladder,
                &mut encoder,
                200_000_000,
                203_000_000,
                recording_congested
            ),
            vec![(202_500_000, 10_000_000)]
        );
        assert_eq!(
            drive(&mut ladder, &mut encoder, 203_000_000, 230_000_000, HEALTHY),
            vec![(223_000_000, 14_000_000)]
        );
    }

    #[test]
    fn rebase_keeps_transport_thresholds_proportional_to_the_encoder() {
        let mut bitrate = AdaptiveBitrate::new(12_000_000);
        bitrate.rebase(3_000_000);
        let low = AdaptiveBitrate::new(3_000_000);
        assert_eq!(bitrate.drain_bytes(), low.drain_bytes());
        assert_eq!(bitrate.congested_bytes(), low.congested_bytes());
        assert!(bitrate.congested(low.congested_bytes(), 0, false));
        assert!(!AdaptiveBitrate::new(12_000_000).congested(low.congested_bytes(), 0, false));

        bitrate.rebase(6_000_000);
        assert_eq!(bitrate.drain_bytes(), 2 * low.drain_bytes());
        assert_eq!(bitrate.congested_bytes(), 2 * low.congested_bytes());
        assert!(bitrate.healthy(bitrate.drain_bytes(), 1));
        assert!(!bitrate.healthy(bitrate.drain_bytes() + 1, 1));
    }

    #[test]
    fn encoder_status_reports_the_latest_start() {
        let status = EncoderStatus::default();
        status.publish(20_000_000, false, false);
        assert_eq!(
            (
                status.bits_per_second(),
                status.live_bitrate(),
                status.recording()
            ),
            (20_000_000, false, false)
        );
        status.publish(3_000_000, true, true);
        assert_eq!(
            (
                status.bits_per_second(),
                status.live_bitrate(),
                status.recording()
            ),
            (3_000_000, true, true)
        );
    }
}
