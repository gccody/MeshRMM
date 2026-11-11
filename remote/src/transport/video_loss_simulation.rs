//! The video channel on a lossy link, on a simulated clock: an encoder that
//! makes a frame every frame interval and a keyframe for each request, the
//! Agent's fragmentation, SCTP as webrtc-sctp 0.13 retransmits a lost DATA
//! chunk, and the viewer's own reassembly, frame acceptance and keyframe
//! requests.
//!
//! The SCTP model: a fragment of `n` bytes is `n / 1180` DATA packets, paced
//! one after another. A lost fragment lost one of them. Its retransmission
//! is a fast retransmission, sent when the SACKs of 3 later packets have come
//! back (the receiver acknowledges at once while a gap is open), so it arrives
//! a round trip and 3 packets after the original would have. While one fast
//! recovery is in progress (until its retransmission is acknowledged) a later
//! loss is not fast retransmitted and waits for the T3 timer, whose minimum
//! is 1 s; so does a loss without 3 later packets within that time. A
//! retransmission is lost as often as the original, and is the only one: the
//! channel abandons the chunk after it.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use meshrmm_protocol::{
    DEFAULT_FRAGMENT_PAYLOAD, EncodedFrame, VideoPacket, VideoStreamId, fragment_frame,
};

use super::super::keyframe::KeyframeRequests;
use super::{RECOVERY_POLL_INTERVAL, VideoReceiveState};

const STREAM: VideoStreamId = VideoStreamId(1);
/// DATA chunk payload in one SCTP packet at webrtc-sctp's 1228-byte MTU.
const SCTP_PAYLOAD: usize = 1180;
/// webrtc-sctp's minimum retransmission timeout.
const RTO_US: u64 = 1_000_000;
const SECONDS: u64 = 60;

#[derive(Debug, Clone, Copy)]
pub(super) struct Scenario {
    pub(super) frames_per_second: u64,
    pub(super) round_trip_us: u64,
    /// Fragments lost, in parts per thousand.
    pub(super) fragment_loss_per_mille: u64,
    /// Whether the channel retransmits a lost chunk once.
    pub(super) retransmission: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Tally {
    pub(super) frames: u64,
    pub(super) shown: u64,
    pub(super) keyframes: u64,
    pub(super) keyframe_requests: u64,
    pub(super) incomplete_frames_dropped: u64,
    pub(super) fragments_lost: u64,
    /// The most and the total time a shown frame waited after it was
    /// complete.
    pub(super) max_added_latency_us: u64,
    pub(super) total_added_latency_us: u64,
}

impl Tally {
    pub(super) fn per_minute(value: u64) -> f64 {
        value as f64 * 60.0 / SECONDS as f64
    }

    pub(super) fn frames_lost(&self) -> u64 {
        self.frames - self.shown
    }

    pub(super) fn mean_added_latency_us(&self) -> u64 {
        self.total_added_latency_us / self.shown.max(1)
    }
}

/// What the stream sends: about 4 Mbit/s of changing 1080p content, paced
/// at 8 Mbit/s.
struct Stream {
    delta_bytes: usize,
    keyframe_bytes: usize,
    pacing_bytes_per_second: u64,
}

fn stream(frames_per_second: u64) -> Stream {
    Stream {
        delta_bytes: 480_000 / frames_per_second as usize,
        keyframe_bytes: 120_000,
        pacing_bytes_per_second: 1_000_000,
    }
}

#[derive(Debug)]
enum Event {
    Encode,
    Arrive(Vec<u8>),
    /// The viewer's recovery task: every [`RECOVERY_POLL_INTERVAL`].
    Poll,
    KeyframeRequest,
    /// The recovery task woken at the reassembler's deadline.
    Expire,
}

struct Loss {
    packet: Vec<u8>,
    sent_us: u64,
    later_packets: u32,
}

struct Simulation {
    scenario: Scenario,
    stream: Stream,
    random: u64,
    now_us: u64,
    sequence: u64,
    events: BinaryHeap<Reverse<(u64, u64, usize)>>,
    payloads: HashMap<usize, Event>,
    next_payload: usize,
    // Agent.
    next_frame_id: u64,
    keyframe_wanted: bool,
    sender_free_us: u64,
    losses: Vec<Loss>,
    fast_recovery_until_us: u64,
    // Viewer.
    viewer: VideoReceiveState,
    requests: KeyframeRequests,
    complete_at: HashMap<u64, u64>,
    remaining: HashMap<u64, usize>,
    tally: Tally,
    counted: std::collections::HashSet<u64>,
    end_us: u64,
    expire_at_us: Option<u64>,
}

impl Simulation {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            stream: stream(scenario.frames_per_second),
            random: 0x9e37_79b9_7f4a_7c15
                ^ scenario.round_trip_us
                ^ (scenario.fragment_loss_per_mille << 32)
                ^ (scenario.frames_per_second << 48),
            now_us: 1_000_000,
            sequence: 0,
            events: BinaryHeap::new(),
            payloads: HashMap::new(),
            next_payload: 0,
            next_frame_id: 1,
            keyframe_wanted: true,
            sender_free_us: 0,
            losses: Vec::new(),
            fast_recovery_until_us: 0,
            viewer: VideoReceiveState::new(),
            requests: KeyframeRequests::default(),
            complete_at: HashMap::new(),
            remaining: HashMap::new(),
            tally: Tally::default(),
            counted: Default::default(),
            end_us: 1_000_000 + SECONDS * 1_000_000,
            expire_at_us: None,
        }
    }

    fn lost(&mut self) -> bool {
        // xorshift64*
        self.random ^= self.random >> 12;
        self.random ^= self.random << 25;
        self.random ^= self.random >> 27;
        let value = self.random.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (value >> 11) % 1000 < self.scenario.fragment_loss_per_mille
    }

    fn below(&mut self, bound: usize) -> usize {
        self.lost();
        (self.random >> 17) as usize % bound
    }

    fn at(&mut self, time_us: u64, event: Event) {
        let payload = self.next_payload;
        self.next_payload += 1;
        self.payloads.insert(payload, event);
        self.sequence += 1;
        self.events.push(Reverse((time_us, self.sequence, payload)));
    }

    fn run(mut self) -> Tally {
        let start_us = self.now_us;
        let end_us = self.end_us;
        self.at(start_us, Event::Encode);
        self.at(
            start_us + RECOVERY_POLL_INTERVAL.as_micros() as u64,
            Event::Poll,
        );
        while let Some(Reverse((time_us, _, payload))) = self.events.pop() {
            if time_us >= end_us {
                break;
            }
            self.now_us = time_us;
            match self.payloads.remove(&payload).unwrap() {
                Event::Encode => {
                    self.encode();
                    let interval_us = 1_000_000 / self.scenario.frames_per_second;
                    self.at(time_us + interval_us, Event::Encode);
                }
                Event::Arrive(packet) => self.arrive(packet),
                Event::Poll => {
                    self.poll();
                    self.at(
                        time_us + RECOVERY_POLL_INTERVAL.as_micros() as u64,
                        Event::Poll,
                    );
                }
                Event::KeyframeRequest => self.keyframe_wanted = true,
                Event::Expire => {
                    let (frames, request) = viewer_poll(&mut self.viewer, self.now_us);
                    self.deliver(frames, request);
                }
            }
        }
        self.tally
    }

    fn encode(&mut self) {
        let keyframe = std::mem::take(&mut self.keyframe_wanted);
        let frame = EncodedFrame {
            stream_id: STREAM,
            frame_id: self.next_frame_id,
            capture_timestamp_us: self.now_us,
            encode_complete_timestamp_us: self.now_us,
            send_timestamp_us: self.now_us,
            keyframe,
            data: vec![
                0;
                if keyframe {
                    self.stream.keyframe_bytes
                } else {
                    self.stream.delta_bytes
                }
            ],
        };
        self.next_frame_id += 1;
        // Frames of the last second may still be on their way at the end.
        let counted = self.now_us < self.end_us - 1_000_000;
        if counted {
            self.counted.insert(frame.frame_id);
        }
        self.tally.frames += u64::from(counted);
        self.tally.keyframes += u64::from(keyframe);
        let packets = fragment_frame(&frame, DEFAULT_FRAGMENT_PAYLOAD).unwrap();
        self.remaining.insert(frame.frame_id, packets.len());
        let mut send_us = self.sender_free_us.max(self.now_us);
        for packet in packets {
            let bytes = packet.encode().unwrap();
            let chunks = bytes.len().div_ceil(SCTP_PAYLOAD);
            let lost_chunk = self.lost().then(|| self.below(chunks));
            let mut chunk_sent_us = send_us;
            for chunk in 0..chunks {
                let chunk_bytes = SCTP_PAYLOAD.min(bytes.len() - chunk * SCTP_PAYLOAD);
                chunk_sent_us = send_us;
                send_us += chunk_bytes as u64 * 1_000_000 / self.stream.pacing_bytes_per_second;
                if lost_chunk == Some(chunk) {
                    self.losses.push(Loss {
                        packet: bytes.clone(),
                        sent_us: chunk_sent_us,
                        later_packets: 0,
                    });
                } else {
                    self.acknowledged(chunk_sent_us);
                }
            }
            if lost_chunk.is_some() {
                self.tally.fragments_lost += 1;
            } else {
                self.at(
                    chunk_sent_us + self.scenario.round_trip_us / 2,
                    Event::Arrive(bytes),
                );
            }
        }
        self.sender_free_us = send_us;
    }

    /// A packet sent at `sent_us` arrived, and its SACK reports every loss
    /// before it missing.
    fn acknowledged(&mut self, sent_us: u64) {
        let round_trip_us = self.scenario.round_trip_us;
        let mut index = 0;
        while index < self.losses.len() {
            let loss = &mut self.losses[index];
            if loss.sent_us >= sent_us {
                index += 1;
                continue;
            }
            loss.later_packets += 1;
            if loss.later_packets < 3 && sent_us < loss.sent_us + RTO_US {
                index += 1;
                continue;
            }
            let loss = self.losses.swap_remove(index);
            if !self.scenario.retransmission {
                continue;
            }
            let sack_us = sent_us + round_trip_us;
            let resent_us = if loss.later_packets >= 3
                && sack_us < loss.sent_us + RTO_US
                && sack_us >= self.fast_recovery_until_us
            {
                self.fast_recovery_until_us = sack_us + round_trip_us;
                sack_us
            } else {
                loss.sent_us + RTO_US
            };
            if !self.lost() {
                let arrival_us = (resent_us + round_trip_us / 2).max(self.now_us);
                self.at(arrival_us, Event::Arrive(loss.packet));
            }
        }
    }

    fn arrive(&mut self, bytes: Vec<u8>) {
        let packet = VideoPacket::decode(&bytes).unwrap();
        if let Some(remaining) = self.remaining.get_mut(&packet.frame_id) {
            *remaining -= 1;
            if *remaining == 0 {
                self.complete_at.insert(packet.frame_id, self.now_us);
            }
        }
        let (frames, request) = viewer_receive(&mut self.viewer, packet, self.now_us);
        self.deliver(frames, request);
    }

    fn poll(&mut self) {
        let (frames, request) = viewer_poll(&mut self.viewer, self.now_us);
        self.deliver(frames, request);
        if let Some(stream_id) = self.requests.poll(self.now_us) {
            self.send_request(stream_id);
        }
    }

    fn deliver(&mut self, frames: Vec<EncodedFrame>, request: bool) {
        // The recovery task wakes at the reassembler's deadline.
        let deadline_us = self.viewer.reassembler.deadline_us();
        if let Some(deadline_us) = deadline_us
            && self.expire_at_us != Some(deadline_us)
        {
            self.at(deadline_us.max(self.now_us), Event::Expire);
        }
        self.expire_at_us = deadline_us;
        self.tally.incomplete_frames_dropped =
            self.viewer.reassembler.stats().incomplete_frames_dropped;
        if request && self.requests.request(STREAM, self.now_us) {
            self.send_request(STREAM);
        }
        for frame in frames {
            if frame.keyframe {
                self.requests.keyframe_arrived(frame.stream_id);
            }
            if !self.counted.contains(&frame.frame_id) {
                continue;
            }
            self.tally.shown += 1;
            let added = self
                .now_us
                .saturating_sub(self.complete_at[&frame.frame_id]);
            self.tally.max_added_latency_us = self.tally.max_added_latency_us.max(added);
            self.tally.total_added_latency_us += added;
        }
    }

    fn send_request(&mut self, _stream_id: VideoStreamId) {
        self.tally.keyframe_requests += 1;
        self.at(
            self.now_us + self.scenario.round_trip_us / 2,
            Event::KeyframeRequest,
        );
    }
}

/// As the video channel's message handler: the frames to show and whether
/// to ask for a keyframe.
fn viewer_receive(
    state: &mut VideoReceiveState,
    packet: VideoPacket,
    now_us: u64,
) -> (Vec<EncodedFrame>, bool) {
    let received = state.receive(packet, now_us);
    (received.frames, received.request_keyframe)
}

/// As the video channel's recovery task.
fn viewer_poll(state: &mut VideoReceiveState, now_us: u64) -> (Vec<EncodedFrame>, bool) {
    let received = state.poll_recovery(now_us);
    (received.frames, received.request_keyframe)
}

pub(super) fn simulate(scenario: Scenario) -> Tally {
    Simulation::new(scenario).run()
}

pub(super) fn grid(retransmission: bool) -> Vec<(Scenario, Tally)> {
    let mut results = Vec::new();
    for frames_per_second in [30, 60] {
        for round_trip_ms in [20, 60, 150] {
            for fragment_loss_per_mille in [10, 20, 50] {
                let scenario = Scenario {
                    frames_per_second,
                    round_trip_us: round_trip_ms * 1_000,
                    fragment_loss_per_mille,
                    retransmission,
                };
                results.push((scenario, simulate(scenario)));
            }
        }
    }
    results
}

pub(super) fn print(results: &[(Scenario, Tally)]) {
    println!(
        "fps  rtt  loss  rexmit | frames/min lost/min  keyframe-requests/min  keyframes/min  incomplete/min  added-latency mean/max ms"
    );
    for (scenario, tally) in results {
        println!(
            "{:>3} {:>4} {:>4}% {:>6} | {:>10.0} {:>8.0} {:>22.1} {:>14.1} {:>15.1} {:>10.1}/{:.0}",
            scenario.frames_per_second,
            scenario.round_trip_us / 1_000,
            scenario.fragment_loss_per_mille as f64 / 10.0,
            scenario.retransmission,
            Tally::per_minute(tally.frames),
            Tally::per_minute(tally.frames_lost()),
            Tally::per_minute(tally.keyframe_requests),
            Tally::per_minute(tally.keyframes),
            Tally::per_minute(tally.incomplete_frames_dropped),
            tally.mean_added_latency_us() as f64 / 1_000.0,
            tally.max_added_latency_us as f64 / 1_000.0,
        );
    }
}

/// Measured with the reassembler that kept only the newest frame, on the
/// channel as it was, which never retransmitted (webrtc-sctp abandons a
/// chunk at its first send when `max_retransmits` is 1): keyframe requests
/// per minute at 1% fragment loss and a 60 ms round trip.
const NEWEST_FRAME_ONLY_REQUESTS_PER_MINUTE: [(u64, f64); 2] = [(30, 28.0), (60, 38.0)];

#[test]
fn a_lost_fragment_is_retransmitted_into_its_frame_instead_of_costing_a_keyframe() {
    let results = grid(true);
    print(&results);
    for (frames_per_second, before) in NEWEST_FRAME_ONLY_REQUESTS_PER_MINUTE {
        let (scenario, tally) = results
            .iter()
            .find(|(scenario, _)| {
                scenario.frames_per_second == frames_per_second
                    && scenario.round_trip_us == 60_000
                    && scenario.fragment_loss_per_mille == 10
            })
            .unwrap();
        let requests = Tally::per_minute(tally.keyframe_requests);
        assert!(
            requests <= before / 10.0,
            "{scenario:?}: {requests} keyframe requests a minute, {before} before"
        );
        // Fragments were lost, and nearly every frame is shown.
        assert!(tally.fragments_lost > 10, "{scenario:?}");
        assert!(
            tally.frames_lost() <= tally.frames / 100,
            "{scenario:?}: {tally:?}"
        );
    }
    // A frame waits for a lost one no longer than the reassembler's wait.
    let config = meshrmm_protocol::ReassemblyConfig::default();
    let bound_us = config.reorder_wait.max(config.stale_after).as_micros() as u64;
    for (scenario, tally) in &results {
        assert!(
            tally.max_added_latency_us <= bound_us,
            "{scenario:?}: {tally:?}"
        );
    }
}

/// Without the retransmission the reassembler has nothing to wait for:
/// the channel's setting is what makes the difference.
#[test]
#[ignore = "prints the table; run with --ignored --nocapture"]
fn print_without_retransmission() {
    print(&grid(false));
}
