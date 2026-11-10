//! A link for tests of the queue estimates: a bottleneck of fixed capacity with an unbounded
//! queue in front of it, and a fixed round trip after it. It reports what webrtc-sctp's
//! `buffered_amount` would: every byte from `send` until its acknowledgement arrives.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct SimulatedLink {
    bytes_per_second: u64,
    round_trip: Duration,
    /// When the bottleneck finishes what is already queued.
    free_at: Instant,
    /// When each unacknowledged write is acknowledged, and its size.
    unacknowledged: VecDeque<(Instant, usize)>,
    buffered: usize,
    acknowledged: u64,
}

impl SimulatedLink {
    pub fn new(start: Instant, bytes_per_second: u64, round_trip: Duration) -> Self {
        Self {
            bytes_per_second: bytes_per_second.max(1),
            round_trip,
            free_at: start,
            unacknowledged: VecDeque::new(),
            buffered: 0,
            acknowledged: 0,
        }
    }

    /// Changes the capacity for bytes sent from now on.
    pub fn set_capacity(&mut self, bytes_per_second: u64) {
        self.bytes_per_second = bytes_per_second.max(1);
    }

    pub fn send(&mut self, now: Instant, bytes: usize) {
        let transmission = Duration::from_nanos(
            u64::try_from(bytes as u128 * 1_000_000_000 / u128::from(self.bytes_per_second))
                .unwrap_or(u64::MAX),
        );
        self.free_at = self.free_at.max(now) + transmission;
        self.unacknowledged
            .push_back((self.free_at + self.round_trip, bytes));
        self.buffered += bytes;
    }

    /// What `buffered_amount` reports at `now`.
    pub fn buffered(&mut self, now: Instant) -> usize {
        while let Some(&(acknowledged_at, bytes)) = self.unacknowledged.front()
            && acknowledged_at <= now
        {
            self.unacknowledged.pop_front();
            self.buffered -= bytes;
            self.acknowledged += bytes as u64;
        }
        self.buffered
    }

    /// Bytes acknowledged by the last `buffered` call.
    pub fn acknowledged(&self) -> u64 {
        self.acknowledged
    }

    /// How long a byte sent at `now` waits before the bottleneck starts on it.
    pub fn queue_delay(&self, now: Instant) -> Duration {
        self.free_at.saturating_duration_since(now)
    }
}
