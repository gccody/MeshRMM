//! Video rate control shared by the sender: fragment pacing and the live
//! AIMD bitrate controller.

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
        let congested = reference_chain_lost
            || buffered_bytes >= self.congested_bytes()
            || queued_frames >= (super::video::MAX_ENCODED_FRAME_QUEUE * 4) / 5;
        if congested {
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

        let healthy = buffered_bytes <= self.drain_bytes() && queued_frames <= 1;
        if !healthy || self.current >= self.maximum {
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
}
