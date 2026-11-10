//! How much a data channel holds behind the network.
//!
//! webrtc-sctp counts a message in `buffered_amount` from `send` until the peer acknowledges
//! it, so the value includes the bytes in flight: about the send rate times the round trip.
//! It exposes neither the round trip nor what it has not yet sent, so this measures both from
//! the channel itself: every byte written is timed until `buffered_amount` shows it
//! acknowledged, the quickest of those times is the round trip, and the bytes the peer
//! acknowledges, or the sender writes, in one round trip are what the link holds in flight.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use webrtc::data_channel::RTCDataChannel;

/// The most webrtc-sctp leaves unacknowledged: the receive window it advertises
/// (`INITIAL_RECV_BUF_SIZE`, which webrtc-rs does not change). It also bounds what a channel
/// may hold in flight before its writers wait.
pub const MAX_IN_FLIGHT: usize = 1024 * 1024;

/// The quickest delivery within this long is the round trip. A window this long lets a
/// standing queue count as one long after it formed, and a longer route take over after it.
const ROUND_TRIP_WINDOW: Duration = Duration::from_secs(30);
/// Until a byte is acknowledged the round trip is at least the age of the oldest one, up to
/// this; past it a channel that hears nothing back counts everything as queued.
const UNMEASURED_ROUND_TRIP_LIMIT: Duration = Duration::from_secs(1);
/// Bounds the history kept for a channel that is written to much faster than it is polled.
const MAX_HISTORY: usize = 16 * 1024;

/// The bytes of `buffered` beyond `in_flight`, which is capped at [`MAX_IN_FLIGHT`].
pub fn queued_bytes(buffered: usize, in_flight: usize) -> usize {
    buffered.saturating_sub(in_flight.min(MAX_IN_FLIGHT))
}

/// Bytes a channel carries in `round_trip` at `bytes_per_second`.
pub fn bytes_per_round_trip(bytes_per_second: u64, round_trip: Duration) -> usize {
    usize::try_from(u128::from(bytes_per_second) * round_trip.as_micros() / 1_000_000)
        .unwrap_or(usize::MAX)
}

/// What one channel has written and the peer has acknowledged, over time.
#[derive(Debug, Default)]
pub struct SendQueue {
    written: u64,
    acknowledged: u64,
    /// Unacknowledged writes: where each ends in the stream, and when it was sent.
    unacknowledged: VecDeque<(u64, Instant)>,
    /// When each recent write was sent, and the bytes written before it.
    writes: VecDeque<(Instant, u64)>,
    /// When the channel was polled, and the bytes acknowledged by then.
    deliveries: VecDeque<(Instant, u64)>,
    round_trip: RoundTrip,
}

impl SendQueue {
    /// Records `bytes` the channel accepted from a `send` that started at `started`.
    pub fn sent(&mut self, started: Instant, bytes: usize) {
        self.writes.push_back((started, self.written));
        self.written += bytes as u64;
        self.unacknowledged.push_back((self.written, started));
        if self.writes.len() > MAX_HISTORY {
            self.writes.pop_front();
        }
        if self.unacknowledged.len() > MAX_HISTORY {
            self.unacknowledged.pop_front();
        }
    }

    /// Bytes waiting behind the network on a channel whose source sets the rate (video,
    /// audio): what is unacknowledged beyond what the link delivered, or this channel wrote,
    /// in the last round trip. The larger of the two is in flight both while the rate holds
    /// and when the source resumes after a pause.
    pub fn queued(&mut self, now: Instant, buffered: usize) -> usize {
        let in_flight = self.observe(now, buffered).map_or(0, |round_trip| {
            self.delivered(now, round_trip).max(self.recently_written())
        });
        queued_bytes(buffered, in_flight)
    }

    /// Bytes waiting behind the network on a channel whose writer sends whenever there is
    /// room (files, clipboard): what is unacknowledged beyond what the link delivered in the
    /// last round trip. A writer that waits for this to fall leaves in flight what SCTP's
    /// congestion control lets through, adding at most its limit each round trip.
    pub fn backlog(&mut self, now: Instant, buffered: usize) -> usize {
        let in_flight = self
            .observe(now, buffered)
            .map_or(0, |round_trip| self.delivered(now, round_trip));
        queued_bytes(buffered, in_flight)
    }

    /// The measured round trip, if any byte has been acknowledged.
    pub fn round_trip(&self) -> Option<Duration> {
        self.round_trip.estimate()
    }

    /// Takes in what `buffered` shows acknowledged and returns the round trip to judge by.
    fn observe(&mut self, now: Instant, buffered: usize) -> Option<Duration> {
        // `buffered` counts every byte `sent` records, and possibly bytes another writer or
        // webrtc-rs itself wrote, so this never counts a byte acknowledged before it was.
        self.acknowledged = self
            .acknowledged
            .max(self.written.saturating_sub(buffered as u64));
        let mut delivered_at = None;
        while let Some(&(end, started)) = self.unacknowledged.front()
            && end <= self.acknowledged
        {
            delivered_at = Some(started);
            self.unacknowledged.pop_front();
        }
        // The latest write delivered took the least time; this poll may be late, so the
        // time is an upper bound.
        if let Some(started) = delivered_at {
            self.round_trip
                .sample(now, now.saturating_duration_since(started));
        }
        self.deliveries.push_back((now, self.acknowledged));
        let round_trip = self.round_trip.estimate().or_else(|| {
            let (_, oldest) = self.unacknowledged.front()?;
            Some(
                now.saturating_duration_since(*oldest)
                    .min(UNMEASURED_ROUND_TRIP_LIMIT),
            )
        });
        // Keep one delivery at or before a round trip ago to measure the rate from.
        let horizon = round_trip.and_then(|round_trip| now.checked_sub(round_trip));
        while self.deliveries.len() > MAX_HISTORY
            || self.deliveries.len() > 1
                && horizon.is_some_and(|horizon| self.deliveries[1].0 <= horizon)
        {
            self.deliveries.pop_front();
        }
        while let Some(&(started, _)) = self.writes.front()
            && horizon.is_none_or(|horizon| started < horizon)
        {
            self.writes.pop_front();
        }
        round_trip
    }

    /// Bytes acknowledged in the last round trip, at the rate measured over at least one.
    fn delivered(&self, now: Instant, round_trip: Duration) -> usize {
        let Some(&(since, acknowledged)) = self.deliveries.front() else {
            return 0;
        };
        let elapsed = now.saturating_duration_since(since).max(round_trip);
        if elapsed.is_zero() {
            return 0;
        }
        let bytes = u128::from(self.acknowledged - acknowledged);
        usize::try_from(bytes * round_trip.as_nanos() / elapsed.as_nanos()).unwrap_or(usize::MAX)
    }

    /// Bytes this channel wrote in the last round trip, as `observe` left the history.
    fn recently_written(&self) -> usize {
        self.writes.front().map_or(0, |&(_, before)| {
            usize::try_from(self.written - before).unwrap_or(usize::MAX)
        })
    }
}

/// The minimum of the round-trip samples over the last one to two windows. A window with no
/// samples keeps the previous estimate, so an idle channel does not forget it.
#[derive(Debug, Default)]
struct RoundTrip {
    window_started: Option<Instant>,
    current: Option<Duration>,
    previous: Option<Duration>,
}

impl RoundTrip {
    fn sample(&mut self, now: Instant, sample: Duration) {
        let started = *self.window_started.get_or_insert(now);
        if now.saturating_duration_since(started) >= ROUND_TRIP_WINDOW {
            self.previous = self.current.take().or(self.previous);
            self.window_started = Some(now);
        }
        self.current = Some(self.current.map_or(sample, |current| current.min(sample)));
    }

    fn estimate(&self) -> Option<Duration> {
        match (self.current, self.previous) {
            (Some(current), Some(previous)) => Some(current.min(previous)),
            (current, previous) => current.or(previous),
        }
    }
}

/// A data channel that records what it sends, so its queue can be told from its bytes in
/// flight. Clones share the record; a write made on the inner channel directly is not
/// recorded and only makes the estimates more cautious.
#[derive(Clone)]
pub struct MeteredChannel {
    channel: Arc<RTCDataChannel>,
    queue: Arc<Mutex<SendQueue>>,
}

impl std::ops::Deref for MeteredChannel {
    type Target = Arc<RTCDataChannel>;
    fn deref(&self) -> &Self::Target {
        &self.channel
    }
}

impl MeteredChannel {
    pub fn new(channel: Arc<RTCDataChannel>) -> Self {
        Self {
            channel,
            queue: Arc::default(),
        }
    }

    pub async fn send(&self, data: &Bytes) -> webrtc::error::Result<usize> {
        let started = Instant::now();
        let sent = self.channel.send(data).await?;
        self.queue.lock().unwrap().sent(started, data.len());
        Ok(sent)
    }

    /// See [`SendQueue::queued`].
    pub async fn queued(&self) -> usize {
        let buffered = self.channel.buffered_amount().await;
        self.queue.lock().unwrap().queued(Instant::now(), buffered)
    }

    /// The measured round trip, if any byte sent here has been acknowledged.
    pub fn round_trip(&self) -> Option<Duration> {
        self.queue.lock().unwrap().round_trip()
    }

    /// See [`SendQueue::backlog`]. Also returns the buffered amount it was measured from.
    pub async fn backlog(&self) -> (usize, usize) {
        let buffered = self.channel.buffered_amount().await;
        let backlog = self.queue.lock().unwrap().backlog(Instant::now(), buffered);
        (buffered, backlog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SEND_QUEUE_LIMIT;
    use crate::simulated_link::SimulatedLink;

    const ROUND_TRIPS_MS: [u64; 3] = [10, 100, 300];
    const TICK: Duration = Duration::from_millis(1);
    /// 12 Mbit/s, sent a tick at a time and judged once a frame.
    const VIDEO_BYTES_PER_SECOND: u64 = 1_500_000;
    const FRAME_TICKS: u64 = 33;

    fn ms(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    /// Bytes of `bytes_per_second` in `duration`.
    fn bytes_in(bytes_per_second: u64, duration: Duration) -> usize {
        bytes_per_round_trip(bytes_per_second, duration)
    }

    /// Sends video for `ticks` and returns the largest queue the estimate reported, and the
    /// first tick at which it reached `alarm` bytes.
    fn stream(
        queue: &mut SendQueue,
        link: &mut SimulatedLink,
        now: &mut Instant,
        ticks: u64,
        alarm: usize,
    ) -> (usize, Option<u64>) {
        let per_tick = bytes_in(VIDEO_BYTES_PER_SECOND, TICK);
        let mut largest = 0;
        let mut alarmed = None;
        for tick in 0..ticks {
            link.send(*now, per_tick);
            queue.sent(*now, per_tick);
            if tick % FRAME_TICKS == 0 {
                let queued = queue.queued(*now, link.buffered(*now));
                largest = largest.max(queued);
                if queued >= alarm && alarmed.is_none() {
                    alarmed = Some(tick);
                }
            }
            *now += TICK;
        }
        (largest, alarmed)
    }

    #[test]
    fn a_paced_sender_queues_only_what_its_link_cannot_carry_at_any_round_trip() {
        let healthy = bytes_in(VIDEO_BYTES_PER_SECOND, ms(50));
        let congested = bytes_in(VIDEO_BYTES_PER_SECOND, ms(150));
        for round_trip in ROUND_TRIPS_MS.map(ms) {
            let mut now = Instant::now();
            let mut link = SimulatedLink::new(now, 2 * VIDEO_BYTES_PER_SECOND, round_trip);
            let mut queue = SendQueue::default();
            stream(&mut queue, &mut link, &mut now, 2_000, usize::MAX);
            let (largest, _) = stream(&mut queue, &mut link, &mut now, 10_000, usize::MAX);
            assert!(
                largest <= healthy,
                "{round_trip:?}: {largest} bytes queued on a link with room"
            );
            let measured = queue.round_trip().unwrap();
            assert!(
                measured >= round_trip && measured <= round_trip + ms(5),
                "{round_trip:?} measured as {measured:?}"
            );

            // A source that pauses and resumes is in flight again, not queued.
            now += ms(5_000);
            let (largest, _) = stream(&mut queue, &mut link, &mut now, 2_000, usize::MAX);
            assert!(
                largest <= healthy,
                "{round_trip:?}: {largest} bytes queued after a pause"
            );

            link.set_capacity(VIDEO_BYTES_PER_SECOND / 2);
            let (_, alarmed) = stream(&mut queue, &mut link, &mut now, 3_000, congested);
            // Half the rate queues 150 ms of it in 300 ms. The estimate counts what was
            // written in the last round trip as in flight, so it may see that a round trip
            // later.
            let alarmed = alarmed.expect("congestion was never seen");
            let limit = 300 + 2 * round_trip.as_millis() as u64 + FRAME_TICKS;
            assert!(
                alarmed <= limit,
                "{round_trip:?}: congestion seen after {alarmed} ms"
            );
        }
    }

    /// Writes 16 KiB whenever the backlog has room, as file transfers do, for `ticks`, and
    /// returns the longest wait at the bottleneck and the largest buffered amount.
    fn transfer(
        queue: &mut SendQueue,
        link: &mut SimulatedLink,
        now: &mut Instant,
        ticks: u64,
    ) -> (Duration, usize) {
        const CHUNK: usize = 16 * 1024;
        let mut wait = Duration::ZERO;
        let mut largest = 0;
        for _ in 0..ticks {
            loop {
                let buffered = link.buffered(*now);
                largest = largest.max(buffered);
                if queue.backlog(*now, buffered) >= SEND_QUEUE_LIMIT {
                    break;
                }
                link.send(*now, CHUNK);
                queue.sent(*now, CHUNK);
                wait = wait.max(link.queue_delay(*now));
            }
            *now += TICK;
        }
        (wait, largest)
    }

    #[test]
    fn a_transfer_fills_the_link_at_any_round_trip_without_queueing_much_ahead_of_others() {
        const SLOW_LINK: u64 = 100_000;
        // 3 MB/s keeps the bandwidth-delay product at 300 ms under SCTP's receive window.
        const LINK: u64 = 3_000_000;
        for round_trip in ROUND_TRIPS_MS.map(ms) {
            let mut now = Instant::now();
            let mut link = SimulatedLink::new(now, LINK, round_trip);
            let mut queue = SendQueue::default();
            transfer(&mut queue, &mut link, &mut now, 5_000);
            let before = link.acknowledged();
            let (wait, largest) = transfer(&mut queue, &mut link, &mut now, 10_000);
            let throughput = (link.acknowledged() - before) / 10;
            assert!(
                throughput >= LINK * 9 / 10,
                "{round_trip:?}: {throughput} B/s on a {LINK} B/s link"
            );
            // What waits at the bottleneck is what other channels' messages wait behind.
            let waiting = bytes_in(LINK, wait);
            assert!(
                waiting <= 2 * SEND_QUEUE_LIMIT,
                "{round_trip:?}: {waiting} bytes waited ahead of other channels"
            );
            assert!(largest <= SEND_QUEUE_LIMIT + MAX_IN_FLIGHT + 16 * 1024);

            let mut link = SimulatedLink::new(now, SLOW_LINK, round_trip);
            let mut queue = SendQueue::default();
            let (wait, _) = transfer(&mut queue, &mut link, &mut now, 20_000);
            let waiting = bytes_in(SLOW_LINK, wait);
            assert!(
                waiting <= 2 * SEND_QUEUE_LIMIT,
                "{round_trip:?}: {waiting} bytes waited on a slow link"
            );
        }
    }

    #[test]
    fn before_any_acknowledgement_bytes_count_as_in_flight_for_at_most_a_second() {
        let start = Instant::now();
        let mut queue = SendQueue::default();
        queue.sent(start, 4_000);
        queue.sent(start + ms(200), 4_000);
        assert_eq!(queue.queued(start + ms(300), 8_000), 0);
        assert_eq!(queue.backlog(start + ms(300), 8_000), 8_000);
        // Nothing acknowledged after a second: the first write is queued.
        assert_eq!(queue.queued(start + ms(1_100), 8_000), 4_000);
        assert_eq!(queue.round_trip(), None);
    }

    #[test]
    fn the_round_trip_is_the_quickest_delivery_and_survives_idle_windows() {
        let start = Instant::now();
        let mut round_trip = RoundTrip::default();
        round_trip.sample(start, ms(120));
        round_trip.sample(start + ms(10), ms(100));
        round_trip.sample(start + ms(20), ms(400));
        assert_eq!(round_trip.estimate(), Some(ms(100)));
        // Idle for several windows: the estimate stays.
        round_trip.sample(start + 3 * ROUND_TRIP_WINDOW, ms(300));
        assert_eq!(round_trip.estimate(), Some(ms(100)));
        // A longer route takes over once the old samples age out.
        round_trip.sample(start + 4 * ROUND_TRIP_WINDOW, ms(300));
        assert_eq!(round_trip.estimate(), Some(ms(300)));
    }
}
