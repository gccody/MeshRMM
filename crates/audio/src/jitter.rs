//! The viewer's playback queue: a small jitter buffer with a target fill
//! level, kept apart from the output device so it can be driven by sample
//! counts alone.
//!
//! Packets arrive with network jitter, 20 ms apart for Opus and about 10 ms
//! for PCM16, and the output device takes one callback's worth (about 10 ms)
//! at a time. Playback starts, and after an underrun starts again, only once
//! the target is buffered:
//!
//! - The target starts 10 ms above its floor of one 20 ms packet plus the
//!   longest callback: 40 ms with 10 ms callbacks.
//! - An underrun the buffer could have covered raises it by 20 ms, to at most
//!   100 ms; a gap longer than that, a stall, leaves it as it was.
//! - After 10 s in which the queue never fell below 15 ms it comes down 5 ms,
//!   to no less than the floor.
//! - More than 40 ms above the target, the oldest audio is dropped down to the
//!   target, so the queue never holds more than 140 ms.
//!
//! The two sound cards' clocks differ slightly, so the average level is held
//! at the target by playing up to 0.2% faster or slower (`stretch`, applied by
//! the resampler): far below an audible change of pitch, and ten times the
//! 100 ppm a typical pair of clocks differs by.
#![cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
use std::collections::VecDeque;

/// The longest packet expected, an Opus frame.
const PACKET_MS: usize = 20;
/// The shortest output callback assumed before one is seen.
const PERIOD_MS: usize = 10;
/// How far above the floor a new buffer starts.
const START_SPARE_MS: usize = 10;
const MAX_TARGET_MS: usize = 100;
const UNDERRUN_STEP_MS: usize = 20;
/// The target comes down this much after `QUIET_MS` with `SPARE_MS` unused.
const SHAVE_STEP_MS: usize = 5;
const SPARE_MS: usize = 15;
const QUIET_MS: usize = 10_000;
/// How far above the target the queue may grow before the excess is dropped.
const OVERFLOW_MS: usize = 40;
/// The time constant of the average level the speed control follows.
const AVERAGE_MS: f64 = 500.0;
/// No speed change within this distance of the target.
const DEADBAND_MS: f64 = 2.0;
/// The speed change per millisecond beyond the dead band, and its limit.
const STRETCH_PER_MS: f64 = 0.000_1;
const MAX_STRETCH: f64 = 0.002;

#[derive(Default)]
pub(crate) struct JitterBuffer {
    /// Output frames per second, or 0 before an output is configured.
    rate: usize,
    channels: usize,
    /// Interleaved output samples.
    samples: VecDeque<f32>,
    playing: bool,
    /// Frames to buffer before playing.
    target: usize,
    /// The largest callback seen, in frames.
    period: usize,
    /// The average level before a callback, in frames.
    average: f64,
    /// Frames played since the target last changed or playback started.
    quiet: usize,
    /// The lowest level after a callback during `quiet`.
    low: usize,
    /// Silent frames since an underrun, until the next packet arrives.
    starved: Option<usize>,
    underruns: u64,
    dropped: u64,
}

impl JitterBuffer {
    fn ms(&self, ms: usize) -> usize {
        self.rate * ms / 1000
    }

    fn floor(&self) -> usize {
        (self.period.max(self.ms(PERIOD_MS)) + self.ms(PACKET_MS)).min(self.ms(MAX_TARGET_MS))
    }

    /// Starts over for an output of `rate` and `channels`.
    pub(crate) fn configure(&mut self, rate: u32, channels: usize) {
        *self = Self {
            rate: rate as usize,
            channels,
            ..Self::default()
        };
        self.target = self.floor() + self.ms(START_SPARE_MS);
        // Room for a burst well past the bound, so pushing never allocates
        // while the output callback waits for the lock.
        self.samples
            .reserve(self.ms(MAX_TARGET_MS + OVERFLOW_MS + 250) * channels);
        self.clear();
    }

    /// Discards the queue; playback starts again once the target is buffered.
    pub(crate) fn clear(&mut self) {
        self.samples.clear();
        self.playing = false;
        self.starved = None;
        self.restart_window();
    }

    fn restart_window(&mut self) {
        self.quiet = 0;
        self.low = usize::MAX;
    }

    /// Buffered frames.
    pub(crate) fn level(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }

    pub(crate) fn target_ms(&self) -> usize {
        (self.target * 1000).checked_div(self.rate).unwrap_or(0)
    }

    pub(crate) fn underruns(&self) -> u64 {
        self.underruns
    }

    pub(crate) fn dropped_ms(&self) -> u64 {
        (self.dropped * 1000)
            .checked_div(self.rate as u64)
            .unwrap_or(0)
    }

    /// Queues interleaved samples in the output's format.
    pub(crate) fn push(&mut self, samples: &[f32]) {
        if self.channels == 0 {
            return;
        }
        if let Some(starved) = self.starved.take()
            && starved <= self.ms(MAX_TARGET_MS)
        {
            // A late packet a larger target would have covered; a stall
            // longer than any target is not a reason to add latency.
            self.target = (self.target + self.ms(UNDERRUN_STEP_MS)).min(self.ms(MAX_TARGET_MS));
        }
        let whole = samples.len() - samples.len() % self.channels;
        self.samples.extend(&samples[..whole]);
        let level = self.level();
        if level > self.target + self.ms(OVERFLOW_MS) {
            let excess = level - self.target;
            self.samples.drain(..excess * self.channels);
            self.dropped += excess as u64;
        }
        if !self.playing && self.level() >= self.target {
            self.playing = true;
            self.average = self.level() as f64;
            self.restart_window();
        }
    }

    /// Fills one output callback, with `silence` while buffering.
    pub(crate) fn fill<T: Copy>(&mut self, out: &mut [T], silence: T, convert: impl Fn(f32) -> T) {
        if self.channels == 0 {
            out.fill(silence);
            return;
        }
        let frames = out.len() / self.channels;
        if frames > self.period {
            self.period = frames;
            self.target = self.target.max(self.floor());
        }
        if !self.playing {
            if let Some(starved) = &mut self.starved {
                *starved += frames;
            }
            out.fill(silence);
            return;
        }
        let level = self.level();
        let weight = (frames as f64 / (self.rate as f64 * AVERAGE_MS / 1000.0)).min(1.0);
        self.average += (level as f64 - self.average) * weight;
        let played = frames.min(level);
        let count = played * self.channels;
        for (slot, sample) in out.iter_mut().zip(self.samples.drain(..count)) {
            *slot = convert(sample);
        }
        out[count..].fill(silence);
        if played < frames {
            self.underruns += 1;
            self.playing = false;
            self.starved = Some(frames - played);
            self.restart_window();
            return;
        }
        self.low = self.low.min(level - played);
        self.quiet += frames;
        if self.quiet >= self.ms(QUIET_MS) {
            if self.low >= self.ms(SPARE_MS) {
                self.target = (self.target - self.ms(SHAVE_STEP_MS)).max(self.floor());
            }
            self.restart_window();
        }
    }

    /// How much faster than nominal to play: positive when the queue holds
    /// more than the target.
    pub(crate) fn stretch(&self) -> f64 {
        if !self.playing || self.rate == 0 {
            return 0.0;
        }
        let error = (self.average - self.target as f64) * 1000.0 / self.rate as f64;
        let beyond = error.abs() - DEADBAND_MS;
        if beyond <= 0.0 {
            return 0.0;
        }
        (beyond * STRETCH_PER_MS).min(MAX_STRETCH).copysign(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = 48_000;
    const FRAMES_PER_MS: usize = RATE / 1000;
    /// A 10 ms output callback.
    const PERIOD: usize = 10 * FRAMES_PER_MS;
    const OPUS: usize = 20 * FRAMES_PER_MS;
    const PCM: usize = 10 * FRAMES_PER_MS;

    fn buffer() -> JitterBuffer {
        let mut buffer = JitterBuffer::default();
        buffer.configure(RATE as u32, 1);
        buffer
    }
    /// Plays one 10 ms callback and returns how many frames were silent.
    fn play(buffer: &mut JitterBuffer) -> usize {
        let mut out = [0.0; PERIOD];
        buffer.fill(&mut out, 0.0, |s| s);
        out.iter().filter(|&&s| s == 0.0).count()
    }

    /// The queue before the jitter buffer: plays from the first sample and
    /// clears everything past 100 ms.
    #[derive(Default)]
    struct Bare {
        samples: VecDeque<f32>,
        dropped: u64,
    }

    trait Queue {
        fn push(&mut self, frames: usize);
        /// Returns the silent frames.
        fn play(&mut self) -> usize;
        fn level(&self) -> usize;
        fn stretch(&self) -> f64;
        fn dropped(&self) -> u64;
    }
    impl Queue for Bare {
        fn push(&mut self, frames: usize) {
            let limit = RATE / 10;
            if self.samples.len() + frames > limit {
                self.dropped += self.samples.len() as u64;
                self.samples.clear();
            }
            self.samples
                .extend(std::iter::repeat_n(0.5, frames.min(limit)));
        }
        fn play(&mut self) -> usize {
            let played = PERIOD.min(self.samples.len());
            self.samples.drain(..played);
            PERIOD - played
        }
        fn level(&self) -> usize {
            self.samples.len()
        }
        fn stretch(&self) -> f64 {
            0.0
        }
        fn dropped(&self) -> u64 {
            self.dropped
        }
    }
    impl Queue for JitterBuffer {
        fn push(&mut self, frames: usize) {
            JitterBuffer::push(self, &vec![0.5; frames]);
        }
        fn play(&mut self) -> usize {
            play(self)
        }
        fn level(&self) -> usize {
            JitterBuffer::level(self)
        }
        fn stretch(&self) -> f64 {
            JitterBuffer::stretch(self)
        }
        fn dropped(&self) -> u64 {
            self.dropped
        }
    }

    #[derive(Clone, Copy)]
    struct Link {
        name: &'static str,
        /// Frames per packet at the sender's clock.
        packet: usize,
        /// Each packet's delay varies uniformly by up to this much either way.
        jitter_ms: f64,
        /// Every this many seconds nothing arrives for `stall_ms`, then
        /// everything held back arrives at once.
        stall_every_s: f64,
        stall_ms: f64,
        /// How much faster the sender's clock runs than the viewer's.
        drift_ppm: f64,
        /// Picks the jitter drawn.
        seed: u64,
    }
    const fn link(name: &'static str, packet: usize, jitter_ms: f64) -> Link {
        Link {
            name,
            packet,
            jitter_ms,
            stall_every_s: 0.0,
            stall_ms: 0.0,
            drift_ppm: 0.0,
            seed: 0,
        }
    }

    #[derive(Debug)]
    struct Outcome {
        /// Times sound gave way to silence, per minute.
        dropouts: f64,
        /// Dropouts in the first minute.
        first_minute: usize,
        /// Silence after the first sound, ms per minute.
        silent_ms: f64,
        /// Audio discarded, ms per minute.
        dropped_ms: f64,
        /// The queue before each callback once sound started, in ms.
        max_ms: f64,
        mean_ms: f64,
        /// The mean over the last minute.
        last_minute_ms: f64,
    }

    /// Delivers `link`'s packets in order (the channel is ordered, so a late
    /// packet holds back those after it) to `queue`, drained by 10 ms
    /// callbacks, for `seconds`.
    fn simulate(queue: &mut impl Queue, link: Link, seconds: usize) -> Outcome {
        // xorshift64*, so every run sees the same jitter.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64 ^ link.seed.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let mut uniform = move || {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            (seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1_u64 << 53) as f64 * 2.0
                - 1.0
        };
        let frame = |ms: f64| ms * FRAMES_PER_MS as f64;
        let spacing = link.packet as f64 * (1.0 - link.drift_ppm * 1e-6);
        let mut arrival = |index: usize, previous: f64| {
            let mut at = index as f64 * spacing + frame(link.jitter_ms * (1.0 + uniform()));
            if link.stall_every_s > 0.0 {
                let every = frame(link.stall_every_s * 1000.0);
                let start = (at / every).floor() * every;
                if start > 0.0 && at < start + frame(link.stall_ms) {
                    at = start + frame(link.stall_ms);
                }
            }
            at.max(previous)
        };
        let callbacks = seconds * RATE / PERIOD;
        const MINUTE: usize = 60 * RATE / PERIOD;
        let last_minute = callbacks.saturating_sub(MINUTE);
        let (mut index, mut next, mut resampled) = (0, arrival(0, 0.0), 0.0);
        let (mut started, mut sounding) = (false, false);
        let (mut dropouts, mut silent, mut levels, mut max, mut count) = (0, 0, 0, 0, 0);
        let (mut last_levels, mut last_count, mut first_minute) = (0, 0, 0);
        for callback in 0..callbacks {
            let now = (callback * PERIOD) as f64;
            while next <= now {
                // The resampler turns a packet into fewer frames when the
                // queue plays faster.
                resampled += link.packet as f64 / (1.0 + queue.stretch());
                let frames = resampled.floor();
                resampled -= frames;
                queue.push(frames as usize);
                index += 1;
                next = arrival(index, next);
            }
            let level = queue.level();
            let quiet = queue.play();
            if started {
                silent += quiet;
                levels += level;
                max = max.max(level);
                count += 1;
                if callback >= last_minute {
                    last_levels += level;
                    last_count += 1;
                }
                if quiet > 0 && sounding {
                    dropouts += 1;
                    if callback < MINUTE {
                        first_minute += 1;
                    }
                }
            }
            sounding = quiet == 0;
            started |= quiet < PERIOD;
        }
        let minutes = seconds as f64 / 60.0;
        let ms = |frames: f64| frames / FRAMES_PER_MS as f64;
        Outcome {
            dropouts: dropouts as f64 / minutes,
            first_minute,
            silent_ms: ms(silent as f64) / minutes,
            dropped_ms: ms(queue.dropped() as f64) / minutes,
            max_ms: ms(max as f64),
            mean_ms: ms(levels as f64 / count.max(1) as f64),
            last_minute_ms: ms(last_levels as f64 / last_count.max(1) as f64),
        }
    }

    /// Within the starting target, but more than the bare queue absorbs.
    const STEADY: Link = link("Opus, ±10 ms", OPUS, 10.0);
    const JITTERY: Link = link("Opus, ±20 ms", OPUS, 20.0);
    const STALLING: Link = Link {
        stall_every_s: 10.0,
        stall_ms: 150.0,
        ..link("Opus, ±2 ms, 150 ms stall every 10 s", OPUS, 2.0)
    };
    const FAST: Link = Link {
        drift_ppm: 100.0,
        ..link("Opus, ±2 ms, sender 100 ppm fast", OPUS, 2.0)
    };
    const SLOW: Link = Link {
        drift_ppm: -100.0,
        ..link("Opus, ±2 ms, sender 100 ppm slow", OPUS, 2.0)
    };
    const LINKS: [Link; 8] = [
        link("Opus, no jitter", OPUS, 0.0),
        link("Opus, ±5 ms", OPUS, 5.0),
        STEADY,
        JITTERY,
        link("PCM16 10 ms, ±20 ms", PCM, 20.0),
        STALLING,
        FAST,
        SLOW,
    ];

    #[test]
    fn nothing_plays_until_the_target_is_buffered() {
        let mut buffer = buffer();
        assert_eq!(buffer.target_ms(), 40);
        assert_eq!(play(&mut buffer), PERIOD, "nothing queued yet");
        buffer.push(&[0.5; OPUS]);
        assert_eq!(play(&mut buffer), PERIOD, "20 ms of 40 ms");
        assert_eq!(buffer.level(), OPUS, "buffering does not drain");
        buffer.push(&[0.5; OPUS]);
        assert_eq!(play(&mut buffer), 0, "the target is buffered");
        assert_eq!(buffer.level(), 2 * OPUS - PERIOD);
    }

    #[test]
    fn steady_arrival_with_jitter_inside_the_target_never_underruns() {
        let mut bare = 0.0;
        for seed in 0..10 {
            let link = Link { seed, ..STEADY };
            let mut buffer = buffer();
            let outcome = simulate(&mut buffer, link, 120);
            assert_eq!(buffer.underruns, 0, "{outcome:?}");
            assert_eq!(outcome.silent_ms, 0.0, "{outcome:?}");
            assert_eq!(outcome.dropped_ms, 0.0, "{outcome:?}");
            bare += simulate(&mut Bare::default(), link, 120).dropouts;
        }
        assert!(bare > 0.0, "the bare queue underruns at the same jitter");
    }

    #[test]
    fn an_underrun_rebuffers_to_a_higher_target() {
        let mut buffer = buffer();
        buffer.push(&[0.5; 2 * OPUS]);
        for _ in 0..4 {
            assert_eq!(play(&mut buffer), 0);
        }
        // A packet 15 ms late: the queue runs dry halfway through a callback.
        buffer.push(&[0.5; PERIOD / 2]);
        assert_eq!(play(&mut buffer), PERIOD / 2);
        assert_eq!(buffer.underruns, 1);
        assert_eq!(play(&mut buffer), PERIOD);
        // The late packet arrives, and the next: not yet the raised target.
        buffer.push(&[0.5; OPUS]);
        assert_eq!(buffer.target_ms(), 60);
        assert_eq!(play(&mut buffer), PERIOD);
        buffer.push(&[0.5; OPUS]);
        assert_eq!(play(&mut buffer), PERIOD);
        buffer.push(&[0.5; OPUS]);
        assert_eq!(buffer.level(), 3 * OPUS);
        assert_eq!(play(&mut buffer), 0, "60 ms buffered");
        // A stall longer than any target does not raise it further.
        for _ in 0..5 {
            play(&mut buffer);
        }
        for _ in 0..20 {
            play(&mut buffer);
        }
        buffer.push(&[0.5; 3 * OPUS]);
        assert_eq!(buffer.underruns, 2);
        assert_eq!(buffer.target_ms(), 60);
        // Unused headroom brings the target back down 5 ms every 10 s.
        for _ in 0..1_000 {
            buffer.push(&[0.5; PERIOD]);
            assert_eq!(play(&mut buffer), 0);
        }
        assert_eq!(buffer.target_ms(), 55);
    }

    #[test]
    fn a_burst_after_a_stall_drops_only_the_excess() {
        let mut buffer = buffer();
        buffer.push(&[0.5; 2 * OPUS]);
        play(&mut buffer);
        // 150 ms held back by the network: the queue runs dry and buffers
        // again, then the burst arrives packet by packet, oldest first.
        for _ in 0..15 {
            play(&mut buffer);
        }
        let mut sample = 0.0;
        for _ in 0..8 {
            let packet: Vec<f32> = (0..OPUS)
                .map(|_| {
                    sample += 1.0;
                    sample
                })
                .collect();
            buffer.push(&packet);
            assert!(buffer.level() <= ms_frames(40 + OVERFLOW_MS));
            assert!(buffer.level() >= OPUS);
        }
        assert!(buffer.dropped_ms() > 0);
        // The newest audio is kept and plays at once, without a gap.
        let mut out = [0.0; PERIOD];
        let first = (8 * OPUS - buffer.level() + 1) as f32;
        buffer.fill(&mut out, 0.0, |s| s);
        let expected: Vec<f32> = (0..PERIOD).map(|i| first + i as f32).collect();
        assert_eq!(out.to_vec(), expected);
        // An output that stops draining is bounded the same way, and the
        // queue never empties.
        for _ in 0..50 {
            buffer.push(&[0.5; OPUS]);
            assert!(buffer.level() <= ms_frames(40 + OVERFLOW_MS));
            assert!(buffer.level() >= ms_frames(40));
        }
        assert_eq!(play(&mut buffer), 0);
    }
    fn ms_frames(ms: usize) -> usize {
        ms * FRAMES_PER_MS
    }

    #[test]
    fn latency_stays_within_the_bound() {
        for link in LINKS {
            let mut buffer = buffer();
            let outcome = simulate(&mut buffer, link, 300);
            assert!(
                outcome.max_ms <= (MAX_TARGET_MS + OVERFLOW_MS) as f64,
                "{}: {outcome:?}",
                link.name
            );
            assert!(buffer.target_ms() <= MAX_TARGET_MS, "{}", link.name);
            assert!(outcome.mean_ms <= 80.0, "{}: {outcome:?}", link.name);
        }
        // A clean link stays near the floor.
        let mut buffer = buffer();
        let outcome = simulate(&mut buffer, STEADY, 300);
        assert!(outcome.mean_ms <= 45.0, "{outcome:?}");
    }

    #[test]
    fn clock_drift_is_corrected_without_underruns_or_drops() {
        for link in [FAST, SLOW] {
            let mut buffer = buffer();
            let outcome = simulate(&mut buffer, link, 1_200);
            assert_eq!(buffer.underruns, 0, "{}: {outcome:?}", link.name);
            assert_eq!(buffer.dropped, 0, "{}: {outcome:?}", link.name);
            let target = buffer.target_ms() as f64;
            assert!(
                (outcome.last_minute_ms - target).abs() <= 8.0,
                "{}: target {target}, {outcome:?}",
                link.name
            );
            assert!(buffer.stretch().abs() <= MAX_STRETCH);
        }
        // 100 ppm moves the bare queue 6 ms a minute, unchecked: a fast
        // sender fills it until it is cleared, a slow one runs it dry.
        let outcome = simulate(&mut Bare::default(), FAST, 1_200);
        assert!(outcome.dropped_ms > 0.0, "{outcome:?}");
        let outcome = simulate(&mut Bare::default(), SLOW, 1_200);
        assert!(outcome.dropouts > 0.0, "{outcome:?}");
    }

    #[test]
    fn stretch_follows_the_average_level() {
        let mut buffer = buffer();
        assert_eq!(buffer.stretch(), 0.0, "not playing");
        buffer.push(&[0.5; 2 * OPUS]);
        assert_eq!(buffer.stretch(), 0.0, "at the target");
        buffer.average = ms_frames(60) as f64;
        assert!((buffer.stretch() - 0.0018).abs() < 1e-9);
        buffer.average = ms_frames(30) as f64;
        assert!((buffer.stretch() + 0.0008).abs() < 1e-9);
        buffer.average = ms_frames(500) as f64;
        assert_eq!(buffer.stretch(), MAX_STRETCH);
    }

    #[test]
    fn a_long_callback_raises_the_floor() {
        let mut buffer = buffer();
        let mut out = [0.0; 40 * FRAMES_PER_MS];
        buffer.fill(&mut out, 0.0, |s| s);
        assert_eq!(buffer.target_ms(), 60);
        buffer.push(&[0.5; 3 * OPUS]);
        buffer.fill(&mut out, 0.0, |s| s);
        assert!(out.iter().all(|&s| s == 0.5));
    }

    #[test]
    fn clearing_rebuffers_and_keeps_the_target() {
        let mut buffer = buffer();
        buffer.push(&[0.5; 2 * OPUS]);
        buffer.push(&[0.5; PERIOD / 2]);
        for _ in 0..5 {
            play(&mut buffer);
        }
        buffer.push(&[0.5; OPUS]);
        assert_eq!(buffer.target_ms(), 60);
        buffer.clear();
        assert_eq!(buffer.level(), 0);
        buffer.push(&[0.5; 2 * OPUS]);
        assert_eq!(play(&mut buffer), PERIOD, "buffering to 60 ms again");
        // A new output starts over.
        buffer.configure(44_100, 2);
        assert_eq!(buffer.target_ms(), 40);
        buffer.push(&[0.5; 3]);
        assert_eq!(buffer.level(), 1, "only whole frames are queued");
    }

    #[test]
    fn simulation_before_and_after() {
        let mut lines = Vec::new();
        for link in LINKS {
            let seconds = if link.drift_ppm == 0.0 { 600 } else { 1_200 };
            let before = simulate(&mut Bare::default(), link, seconds);
            let after = simulate(&mut buffer(), link, seconds);
            let row = |o: &Outcome| {
                format!(
                    "{} / {:.2} / {:.0} / {:.0} / {:.0} / {:.0}",
                    o.first_minute, o.dropouts, o.silent_ms, o.dropped_ms, o.mean_ms, o.max_ms
                )
            };
            lines.push(format!(
                "| {} | {} | {} |",
                link.name,
                row(&before),
                row(&after)
            ));
        }
        println!(
            "| link | before: first-minute dropouts / dropouts per min / silent ms per min / dropped ms per min / mean queue ms / max queue ms | after |"
        );
        for line in lines {
            println!("{line}");
        }
        // Each start learns the link: over ten different draws of ±20 ms jitter.
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for seed in 1..=10 {
            let link = Link { seed, ..JITTERY };
            before.push(simulate(&mut Bare::default(), link, 120).first_minute);
            after.push(simulate(&mut buffer(), link, 120).first_minute);
        }
        println!("first-minute dropouts at ±20 ms, ten draws: before {before:?}, after {after:?}");
        // Over those 20 minutes: 0.85 a minute before, 0.05 after.
        let per_minute = |counts: Vec<usize>| counts.iter().sum::<usize>() as f64 / 20.0;
        assert!(per_minute(before) >= 0.5);
        assert!(per_minute(after) <= 0.1);
    }
}
