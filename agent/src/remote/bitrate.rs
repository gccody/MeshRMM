//! Video rate control shared by the sender: fragment pacing (which leaves
//! room for audio) and the live AIMD bitrate controller, and the restart
//! ladder for encoders that cannot change bitrate live.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const VIDEO_BUFFER_DRAIN_MS: u32 = 50;
const VIDEO_BUFFER_CONGESTED_MS: u32 = 150;
const BITRATE_DECREASE_INTERVAL_US: u64 = 500_000;
const BITRATE_INCREASE_INTERVAL_US: u64 = 5_000_000;

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

#[derive(Debug)]
pub(super) struct AdaptiveBitrate {
    minimum: u32,
    maximum: u32,
    current: u32,
    last_decrease_us: u64,
    healthy_since_us: u64,
}

impl AdaptiveBitrate {
    pub(super) fn new(maximum: u32) -> Self {
        let minimum = (maximum / 8).max(1_000_000).min(maximum);
        Self {
            minimum,
            maximum,
            current: maximum,
            last_decrease_us: 0,
            healthy_since_us: 0,
        }
    }

    pub(super) fn set_maximum(&mut self, maximum: u32) -> Option<u32> {
        let maximum = maximum.max(1);
        if self.maximum == maximum {
            return None;
        }
        self.maximum = maximum;
        self.minimum = (maximum / 8).max(500_000).min(maximum);
        self.current = maximum;
        self.last_decrease_us = 0;
        self.healthy_since_us = 0;
        Some(maximum)
    }

    pub(super) fn observe(
        &mut self,
        now_us: u64,
        buffered_bytes: usize,
        queued_frames: usize,
        reference_chain_lost: bool,
    ) -> Option<u32> {
        if self.congested(buffered_bytes, queued_frames, reference_chain_lost) {
            self.healthy_since_us = 0;
            if self.last_decrease_us == 0
                || now_us.saturating_sub(self.last_decrease_us) >= BITRATE_DECREASE_INTERVAL_US
            {
                self.last_decrease_us = now_us.max(1);
                let reduced = ((u64::from(self.current) * 3) / 4) as u32;
                let reduced = reduced.max(self.minimum);
                if reduced < self.current {
                    self.current = reduced;
                    return Some(self.current);
                }
            }
            return None;
        }

        if !self.healthy(buffered_bytes, queued_frames) || self.current >= self.maximum {
            self.healthy_since_us = 0;
            return None;
        }
        if self.healthy_since_us == 0 {
            self.healthy_since_us = now_us.max(1);
            return None;
        }
        if now_us.saturating_sub(self.healthy_since_us) >= BITRATE_INCREASE_INTERVAL_US {
            self.healthy_since_us = now_us.max(1);
            let increase = (self.current / 10).max(250_000);
            self.current = self.current.saturating_add(increase).min(self.maximum);
            return Some(self.current);
        }
        None
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
        buffered_bytes: usize,
        queued_frames: usize,
        reference_chain_lost: bool,
    ) -> bool {
        reference_chain_lost
            || buffered_bytes >= self.congested_bytes()
            || queued_frames >= (super::video::MAX_ENCODED_FRAME_QUEUE * 4) / 5
    }

    pub(super) fn healthy(&self, buffered_bytes: usize, queued_frames: usize) -> bool {
        buffered_bytes <= self.drain_bytes() && queued_frames <= 1
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
const LADDER_MINIMUM_BITRATE: u32 = 500_000;
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

    /// A quality change restarts the encoder at the new ceiling.
    pub(super) fn set_maximum(&mut self, maximum: u32) {
        let maximum = maximum.max(1);
        if self.maximum != maximum {
            *self = Self::new(maximum);
        }
    }

    fn steps(&self) -> [u32; LADDER_STEPS_PERCENT.len()] {
        let floor = (self.maximum / 8)
            .max(LADDER_MINIMUM_BITRATE)
            .min(self.maximum);
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

        let healthy_start = 2_000_000;
        assert_eq!(bitrate.observe(healthy_start, 0, 0, false), None);
        assert_eq!(
            bitrate.observe(healthy_start + BITRATE_INCREASE_INTERVAL_US, 0, 0, false),
            Some(7_425_000)
        );
    }

    #[test]
    fn adaptive_bitrate_never_drops_below_its_floor() {
        let mut bitrate = AdaptiveBitrate::new(4_000_000);
        let mut now_us = 1;
        for _ in 0..20 {
            let _ = bitrate.observe(now_us, usize::MAX, usize::MAX, true);
            now_us += BITRATE_DECREASE_INTERVAL_US;
        }
        assert_eq!(bitrate.current, 1_000_000);
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
            vec![700_000, 500_000]
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
