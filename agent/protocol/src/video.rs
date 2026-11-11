use std::collections::BTreeMap;
use std::time::Duration;

use thiserror::Error;

use crate::VideoStreamId;

const MAGIC: u32 = u32::from_be_bytes(*b"PRVF");
const VERSION: u8 = 1;
const KEYFRAME_FLAG: u8 = 1;
pub const VIDEO_PACKET_HEADER_LEN: usize = 56;
pub const DEFAULT_FRAGMENT_PAYLOAD: usize = 12 * 1024;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_PACKETS: u16 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub stream_id: VideoStreamId,
    pub frame_id: u64,
    /// Monotonic microseconds in the sender's clock domain.
    pub capture_timestamp_us: u64,
    /// Monotonic microseconds in the sender's clock domain.
    pub encode_complete_timestamp_us: u64,
    /// Filled immediately before fragmentation/send.
    pub send_timestamp_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    pub stream_id: VideoStreamId,
    pub frame_id: u64,
    pub capture_timestamp_us: u64,
    pub encode_complete_timestamp_us: u64,
    pub send_timestamp_us: u64,
    pub keyframe: bool,
    pub packet_index: u16,
    pub packet_count: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VideoPacketError {
    #[error("video packet is shorter than its fixed header")]
    Truncated,
    #[error("video packet has an invalid magic value")]
    InvalidMagic,
    #[error("unsupported video packet version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid video packet header length {0}")]
    InvalidHeaderLength(usize),
    #[error("invalid packet index {index} for packet count {count}")]
    InvalidPacketIndex { index: u16, count: u16 },
    #[error("video packet payload length does not match its header")]
    PayloadLengthMismatch,
    #[error("encoded frame is empty")]
    EmptyFrame,
    #[error("encoded frame needs more than {0} fragments")]
    TooManyFragments(usize),
    #[error("fragment payload size must be greater than zero")]
    InvalidFragmentSize,
}

impl VideoPacket {
    pub fn encode(&self) -> Result<Vec<u8>, VideoPacketError> {
        validate_index(self.packet_index, self.packet_count)?;
        let payload_len = u16::try_from(self.payload.len())
            .map_err(|_| VideoPacketError::PayloadLengthMismatch)?;
        let mut bytes = Vec::with_capacity(VIDEO_PACKET_HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&MAGIC.to_be_bytes());
        bytes.push(VERSION);
        bytes.push(u8::from(self.keyframe) * KEYFRAME_FLAG);
        bytes.extend_from_slice(&(VIDEO_PACKET_HEADER_LEN as u16).to_be_bytes());
        bytes.extend_from_slice(&self.stream_id.0.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&self.frame_id.to_be_bytes());
        bytes.extend_from_slice(&self.capture_timestamp_us.to_be_bytes());
        bytes.extend_from_slice(&self.encode_complete_timestamp_us.to_be_bytes());
        bytes.extend_from_slice(&self.send_timestamp_us.to_be_bytes());
        bytes.extend_from_slice(&self.packet_index.to_be_bytes());
        bytes.extend_from_slice(&self.packet_count.to_be_bytes());
        bytes.extend_from_slice(&payload_len.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, VideoPacketError> {
        if bytes.len() < VIDEO_PACKET_HEADER_LEN {
            return Err(VideoPacketError::Truncated);
        }
        if read_u32(bytes, 0) != MAGIC {
            return Err(VideoPacketError::InvalidMagic);
        }
        if bytes[4] != VERSION {
            return Err(VideoPacketError::UnsupportedVersion(bytes[4]));
        }
        let header_len = read_u16(bytes, 6) as usize;
        if header_len != VIDEO_PACKET_HEADER_LEN {
            return Err(VideoPacketError::InvalidHeaderLength(header_len));
        }
        let packet_index = read_u16(bytes, 48);
        let packet_count = read_u16(bytes, 50);
        validate_index(packet_index, packet_count)?;
        let payload_len = read_u16(bytes, 52) as usize;
        if bytes.len() != header_len + payload_len {
            return Err(VideoPacketError::PayloadLengthMismatch);
        }
        Ok(Self {
            stream_id: VideoStreamId(read_u32(bytes, 8)),
            frame_id: read_u64(bytes, 16),
            capture_timestamp_us: read_u64(bytes, 24),
            encode_complete_timestamp_us: read_u64(bytes, 32),
            send_timestamp_us: read_u64(bytes, 40),
            keyframe: bytes[5] & KEYFRAME_FLAG != 0,
            packet_index,
            packet_count,
            payload: bytes[header_len..].to_vec(),
        })
    }
}

pub fn fragment_frame(
    frame: &EncodedFrame,
    max_payload: usize,
) -> Result<Vec<VideoPacket>, VideoPacketError> {
    if max_payload == 0 {
        return Err(VideoPacketError::InvalidFragmentSize);
    }
    if frame.data.is_empty() {
        return Err(VideoPacketError::EmptyFrame);
    }
    let count = frame.data.len().div_ceil(max_payload);
    let packet_count =
        u16::try_from(count).map_err(|_| VideoPacketError::TooManyFragments(count))?;
    frame
        .data
        .chunks(max_payload)
        .enumerate()
        .map(|(index, payload)| {
            Ok(VideoPacket {
                stream_id: frame.stream_id,
                frame_id: frame.frame_id,
                capture_timestamp_us: frame.capture_timestamp_us,
                encode_complete_timestamp_us: frame.encode_complete_timestamp_us,
                send_timestamp_us: frame.send_timestamp_us,
                keyframe: frame.keyframe,
                packet_index: index as u16,
                packet_count,
                payload: payload.to_vec(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct ReassemblyConfig {
    /// An incomplete frame is given up this long after its latest fragment.
    pub stale_after: Duration,
    /// How long a complete frame waits for an older frame that is missing
    /// or incomplete, from when the first newer frame completed.
    pub reorder_wait: Duration,
    pub max_frame_bytes: usize,
    pub max_packets: u16,
    /// Frames held at once: incomplete, or complete and waiting for an
    /// older one.
    pub max_held_frames: usize,
    /// Payload bytes held across them; at least `max_frame_bytes`.
    pub max_held_bytes: usize,
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            // The video channel retransmits a lost chunk once, and the
            // fast retransmission takes a round trip and 3 later packets:
            // within this time up to about a 150 ms round trip at 30 fps.
            stale_after: Duration::from_millis(250),
            reorder_wait: Duration::from_millis(250),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_packets: DEFAULT_MAX_PACKETS,
            // 250 ms at 60 fps is 15 frames.
            max_held_frames: 32,
            max_held_bytes: 2 * DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub completed_frames: u64,
    pub incomplete_frames_dropped: u64,
    pub stale_packets_dropped: u64,
    pub duplicate_packets: u64,
    pub invalid_packets: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReassemblyOutcome {
    Accepted,
    /// The packet completed its frame, which is released at once or when
    /// the frames before it are released or given up.
    Completed,
    Duplicate,
    DroppedStale,
    DroppedInvalid,
}

/// What a packet did, and the frames released for the decoder, in frame
/// order.
#[derive(Debug, PartialEq, Eq)]
pub struct Pushed {
    pub outcome: ReassemblyOutcome,
    pub frames: Vec<EncodedFrame>,
}

#[derive(Debug)]
struct PendingFrame {
    capture_timestamp_us: u64,
    encode_complete_timestamp_us: u64,
    send_timestamp_us: u64,
    keyframe: bool,
    /// When the latest new fragment arrived. A frame paced out over longer
    /// than `stale_after` (a large keyframe at a low bitrate) stays pending
    /// while its fragments keep coming.
    last_packet_at_us: u64,
    total_bytes: usize,
    received: usize,
    fragments: Vec<Option<Vec<u8>>>,
}

#[derive(Debug)]
enum HeldFrame {
    Pending(PendingFrame),
    Complete {
        frame: EncodedFrame,
        completed_at_us: u64,
    },
}

impl HeldFrame {
    fn bytes(&self) -> usize {
        match self {
            Self::Pending(pending) => pending.total_bytes,
            Self::Complete { frame, .. } => frame.data.len(),
        }
    }

    fn keyframe(&self) -> bool {
        match self {
            Self::Pending(pending) => pending.keyframe,
            Self::Complete { frame, .. } => frame.keyframe,
        }
    }
}

/// Reassembles a stream's frames and releases them in frame order.
///
/// The video channel is unordered and retransmits a lost chunk once, so a
/// lost fragment usually arrives a round trip late, after fragments of newer
/// frames. A frame is released as soon as it is complete and the frame
/// before it was released, which adds no latency while nothing is lost; a
/// keyframe needs no frame before it. Newer frames wait for a missing or
/// incomplete older one at most `reorder_wait` after the first of them
/// completed (and an incomplete frame at most `stale_after` after its
/// latest fragment); then it is given up and they go on, after the gap,
/// which the decoder cannot bridge without a keyframe. An incomplete
/// keyframe is given up only when its fragments stop, since the frames
/// after it cannot be decoded without it. A fixed number of frames and
/// bytes is held.
#[derive(Debug)]
pub struct FrameReassembler {
    config: ReassemblyConfig,
    stream_id: Option<VideoStreamId>,
    /// The stream before it, whose late packets are dropped.
    previous_stream_id: Option<VideoStreamId>,
    /// The newest frame released or given up. Frames up to it are late.
    released: Option<u64>,
    held: BTreeMap<u64, HeldFrame>,
    held_bytes: usize,
    stats: ReassemblyStats,
}

impl FrameReassembler {
    pub fn new(config: ReassemblyConfig) -> Self {
        Self {
            config: ReassemblyConfig {
                max_held_frames: config.max_held_frames.max(1),
                max_held_bytes: config.max_held_bytes.max(config.max_frame_bytes),
                ..config
            },
            stream_id: None,
            previous_stream_id: None,
            released: None,
            held: BTreeMap::new(),
            held_bytes: 0,
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// When [`Self::expire`] next gives a frame up, unless packets come
    /// first.
    pub fn deadline_us(&self) -> Option<u64> {
        let (_, head) = self.held.first_key_value()?;
        let first_completed_us = self
            .held
            .values()
            .filter_map(|frame| match frame {
                HeldFrame::Complete {
                    completed_at_us, ..
                } => Some(*completed_at_us),
                HeldFrame::Pending(_) => None,
            })
            .min();
        let reorder_wait_us = self.config.reorder_wait.as_micros() as u64;
        match head {
            HeldFrame::Pending(pending) => {
                let stale_us = pending
                    .last_packet_at_us
                    .saturating_add(self.config.stale_after.as_micros() as u64);
                let waited_us =
                    first_completed_us
                        .filter(|_| !pending.keyframe)
                        .map(|completed_us| {
                            completed_us
                                .max(pending.last_packet_at_us)
                                .saturating_add(reorder_wait_us)
                        });
                Some(waited_us.map_or(stale_us, |waited_us| waited_us.min(stale_us)))
            }
            HeldFrame::Complete { .. } => {
                first_completed_us.map(|completed_us| completed_us.saturating_add(reorder_wait_us))
            }
        }
    }

    /// Gives up the frames whose wait is over, and returns the frames that
    /// this releases.
    pub fn expire(&mut self, now_us: u64) -> Vec<EncodedFrame> {
        let mut frames = Vec::new();
        self.expire_into(now_us, &mut frames);
        frames
    }

    pub fn push(&mut self, packet: VideoPacket, received_at_us: u64) -> Pushed {
        let mut frames = Vec::new();
        let outcome = self.push_into(packet, received_at_us, &mut frames);
        Pushed { outcome, frames }
    }

    fn push_into(
        &mut self,
        packet: VideoPacket,
        received_at_us: u64,
        frames: &mut Vec<EncodedFrame>,
    ) -> ReassemblyOutcome {
        if packet.packet_count == 0
            || packet.packet_count > self.config.max_packets
            || packet.packet_index >= packet.packet_count
            || packet.payload.len() > self.config.max_frame_bytes
        {
            self.stats.invalid_packets += 1;
            return ReassemblyOutcome::DroppedInvalid;
        }

        if self.stream_id != Some(packet.stream_id) {
            if self.previous_stream_id == Some(packet.stream_id) {
                self.stats.stale_packets_dropped += 1;
                return ReassemblyOutcome::DroppedStale;
            }
            // A new stream starts with a keyframe; the old one's frames
            // can no longer be shown.
            self.previous_stream_id = self.stream_id;
            self.stream_id = Some(packet.stream_id);
            self.released = None;
            self.held.clear();
            self.held_bytes = 0;
        }

        self.expire_into(received_at_us, frames);

        if self.is_late(packet.frame_id) {
            self.stats.stale_packets_dropped += 1;
            return ReassemblyOutcome::DroppedStale;
        }
        if !self.held.contains_key(&packet.frame_id) {
            if !self.make_room(&packet, 0, frames) {
                self.stats.stale_packets_dropped += 1;
                return ReassemblyOutcome::DroppedStale;
            }
            self.held.insert(
                packet.frame_id,
                HeldFrame::Pending(PendingFrame {
                    capture_timestamp_us: packet.capture_timestamp_us,
                    encode_complete_timestamp_us: packet.encode_complete_timestamp_us,
                    send_timestamp_us: packet.send_timestamp_us,
                    keyframe: packet.keyframe,
                    last_packet_at_us: received_at_us,
                    total_bytes: 0,
                    received: 0,
                    fragments: vec![None; packet.packet_count as usize],
                }),
            );
        }

        let pending = match self.held.get(&packet.frame_id) {
            Some(HeldFrame::Pending(pending)) => pending,
            Some(HeldFrame::Complete { .. }) => {
                self.stats.duplicate_packets += 1;
                return ReassemblyOutcome::Duplicate;
            }
            None => {
                // Room was made by giving this very frame up.
                self.stats.stale_packets_dropped += 1;
                return ReassemblyOutcome::DroppedStale;
            }
        };
        if pending.fragments.len() != packet.packet_count as usize
            || pending.capture_timestamp_us != packet.capture_timestamp_us
            || pending.encode_complete_timestamp_us != packet.encode_complete_timestamp_us
            || pending.keyframe != packet.keyframe
            || pending.total_bytes.saturating_add(packet.payload.len())
                > self.config.max_frame_bytes
        {
            self.remove(packet.frame_id);
            self.stats.invalid_packets += 1;
            self.release_into(frames);
            return ReassemblyOutcome::DroppedInvalid;
        }
        if pending.fragments[packet.packet_index as usize].is_some() {
            self.stats.duplicate_packets += 1;
            return ReassemblyOutcome::Duplicate;
        }
        if !self.make_room(&packet, packet.payload.len(), frames) {
            self.stats.stale_packets_dropped += 1;
            return ReassemblyOutcome::DroppedStale;
        }
        let Some(HeldFrame::Pending(pending)) = self.held.get_mut(&packet.frame_id) else {
            self.stats.stale_packets_dropped += 1;
            return ReassemblyOutcome::DroppedStale;
        };
        pending.total_bytes += packet.payload.len();
        pending.received += 1;
        pending.last_packet_at_us = received_at_us;
        self.held_bytes += packet.payload.len();
        pending.fragments[packet.packet_index as usize] = Some(packet.payload);

        if pending.received != pending.fragments.len() {
            return ReassemblyOutcome::Accepted;
        }
        let Some(HeldFrame::Pending(pending)) = self.held.remove(&packet.frame_id) else {
            unreachable!("the frame was just found pending");
        };
        let mut data = Vec::with_capacity(pending.total_bytes);
        for fragment in pending.fragments {
            data.extend_from_slice(fragment.as_deref().unwrap_or_default());
        }
        self.held.insert(
            packet.frame_id,
            HeldFrame::Complete {
                frame: EncodedFrame {
                    stream_id: packet.stream_id,
                    frame_id: packet.frame_id,
                    capture_timestamp_us: pending.capture_timestamp_us,
                    encode_complete_timestamp_us: pending.encode_complete_timestamp_us,
                    send_timestamp_us: pending.send_timestamp_us,
                    keyframe: pending.keyframe,
                    data,
                },
                completed_at_us: received_at_us,
            },
        );
        self.stats.completed_frames += 1;
        self.release_into(frames);
        ReassemblyOutcome::Completed
    }

    fn is_late(&self, frame_id: u64) -> bool {
        self.released.is_some_and(|released| frame_id <= released)
    }

    fn remove(&mut self, frame_id: u64) -> Option<HeldFrame> {
        let frame = self.held.remove(&frame_id)?;
        self.held_bytes -= frame.bytes();
        Some(frame)
    }

    /// Makes room for `bytes` more of `packet`'s frame, held already or not,
    /// by giving the oldest frames up; or returns false when the packet is
    /// to be dropped instead.
    fn make_room(
        &mut self,
        packet: &VideoPacket,
        bytes: usize,
        frames: &mut Vec<EncodedFrame>,
    ) -> bool {
        loop {
            let held = self.held.contains_key(&packet.frame_id);
            let frames_full = !held && self.held.len() >= self.config.max_held_frames;
            let bytes_full = self.held_bytes + bytes > self.config.max_held_bytes;
            if !frames_full && !bytes_full {
                return true;
            }
            let Some((&head_id, head)) = self.held.first_key_value() else {
                return true;
            };
            if head_id == packet.frame_id {
                // This frame is the oldest held; the newest frame goes.
                let Some((&newest_id, _)) = self.held.last_key_value() else {
                    return true;
                };
                if newest_id == head_id {
                    return true;
                }
                if let Some(HeldFrame::Pending(_)) = self.remove(newest_id) {
                    self.stats.incomplete_frames_dropped += 1;
                }
                continue;
            }
            // The frames after an incomplete keyframe cannot be decoded
            // without it.
            if head.keyframe() && matches!(head, HeldFrame::Pending(_)) && !packet.keyframe {
                return false;
            }
            self.give_up_head(frames);
            if self.is_late(packet.frame_id) {
                return false;
            }
        }
    }

    fn expire_into(&mut self, now_us: u64, frames: &mut Vec<EncodedFrame>) {
        while self
            .deadline_us()
            .is_some_and(|deadline_us| now_us >= deadline_us)
        {
            self.give_up_head(frames);
        }
    }

    /// Gives up the oldest held frame if it is incomplete, or the missing
    /// frames before it if it is complete, and releases what follows.
    fn give_up_head(&mut self, frames: &mut Vec<EncodedFrame>) {
        let Some((&head_id, head)) = self.held.first_key_value() else {
            return;
        };
        match head {
            HeldFrame::Pending(_) => {
                self.remove(head_id);
                self.stats.incomplete_frames_dropped += 1;
                self.released = Some(head_id);
            }
            HeldFrame::Complete { .. } => {
                if let Some(HeldFrame::Complete { frame, .. }) = self.remove(head_id) {
                    frames.push(frame);
                }
                self.released = Some(head_id);
            }
        }
        self.release_into(frames);
    }

    /// Releases the frames that can go, in order: a complete frame after
    /// the released one, or a complete keyframe, which makes the frames
    /// before it unnecessary.
    fn release_into(&mut self, frames: &mut Vec<EncodedFrame>) {
        loop {
            let Some((&head_id, head)) = self.held.first_key_value() else {
                return;
            };
            let next = self
                .released
                .is_some_and(|released| head_id == released.wrapping_add(1));
            if let HeldFrame::Complete { frame, .. } = head
                && (next || frame.keyframe)
            {
                if let Some(HeldFrame::Complete { frame, .. }) = self.remove(head_id) {
                    self.released = Some(head_id);
                    frames.push(frame);
                }
                continue;
            }
            let keyframe = self.held.iter().find_map(|(&id, frame)| {
                matches!(frame, HeldFrame::Complete { frame, .. } if frame.keyframe).then_some(id)
            });
            let Some(keyframe_id) = keyframe else {
                return;
            };
            // Superseded: not lost, since the keyframe does not need them.
            while let Some((&id, _)) = self.held.first_key_value()
                && id < keyframe_id
            {
                self.remove(id);
            }
        }
    }
}

fn validate_index(index: u16, count: u16) -> Result<(), VideoPacketError> {
    if count == 0 || index >= count {
        Err(VideoPacketError::InvalidPacketIndex { index, count })
    } else {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u64, data: &[u8]) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(7),
            frame_id: id,
            capture_timestamp_us: 100,
            encode_complete_timestamp_us: 110,
            send_timestamp_us: 120,
            keyframe: id == 1,
            data: data.to_vec(),
        }
    }

    fn fragments(frame: &EncodedFrame) -> Vec<VideoPacket> {
        fragment_frame(frame, 3).unwrap()
    }

    fn ids(frames: &[EncodedFrame]) -> Vec<u64> {
        frames.iter().map(|frame| frame.frame_id).collect()
    }

    fn reorder_wait_us() -> u64 {
        ReassemblyConfig::default().reorder_wait.as_micros() as u64
    }

    fn stale_after_us() -> u64 {
        ReassemblyConfig::default().stale_after.as_micros() as u64
    }

    /// Pushes every packet at `now_us` and returns the frames released.
    fn push_all(
        assembler: &mut FrameReassembler,
        packets: &[VideoPacket],
        now_us: u64,
    ) -> Vec<EncodedFrame> {
        packets
            .iter()
            .flat_map(|packet| assembler.push(packet.clone(), now_us).frames)
            .collect()
    }

    #[test]
    fn packet_round_trip() {
        let packet = fragment_frame(&frame(1, b"hello"), 16).unwrap().remove(0);
        assert_eq!(
            VideoPacket::decode(&packet.encode().unwrap()).unwrap(),
            packet
        );
    }

    #[test]
    fn fragments_and_reassembles_out_of_order() {
        let source = frame(1, b"abcdefghij");
        let packets = fragments(&source);
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        for (index, now_us) in [(2, 1), (0, 2), (3, 3)] {
            assert_eq!(
                assembler.push(packets[index].clone(), now_us),
                Pushed {
                    outcome: ReassemblyOutcome::Accepted,
                    frames: Vec::new()
                }
            );
        }
        assert_eq!(
            assembler.push(packets[1].clone(), 4),
            Pushed {
                outcome: ReassemblyOutcome::Completed,
                frames: vec![source]
            }
        );
    }

    /// The video channel retransmits a lost fragment about a round trip
    /// later, after fragments of newer frames.
    #[test]
    fn a_retransmitted_fragment_completes_its_frame_and_frames_go_in_order() {
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        let keyframe = frame(1, b"key");
        let second = frame(2, b"abcdef");
        let third = frame(3, b"ghijkl");
        let (second_packets, third_packets) = (fragments(&second), fragments(&third));
        assert_eq!(
            push_all(&mut assembler, &fragments(&keyframe), 1_000),
            [keyframe]
        );
        // The first fragment of frame 2 is lost; frame 3 completes.
        assert!(push_all(&mut assembler, &second_packets[1..], 2_000).is_empty());
        assert!(push_all(&mut assembler, &third_packets, 18_000).is_empty());
        assert_eq!(assembler.deadline_us(), Some(2_000 + stale_after_us()));

        // Its retransmission a round trip later releases both, in order.
        let released = assembler.push(second_packets[0].clone(), 80_000);
        assert_eq!(released.outcome, ReassemblyOutcome::Completed);
        assert_eq!(released.frames, [second, third]);
        assert_eq!(assembler.deadline_us(), None);
        let stats = assembler.stats();
        assert_eq!(stats.incomplete_frames_dropped, 0);
        assert_eq!(stats.stale_packets_dropped, 0);
        assert_eq!(stats.completed_frames, 3);
    }

    /// An unordered channel can deliver the first fragment of a frame before
    /// the last of the one before it, with nothing lost.
    #[test]
    fn with_nothing_lost_each_frame_is_released_with_its_last_fragment() {
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        let frames: Vec<_> = (1..=4).map(|id| frame(id, b"abcdefgh")).collect();
        let packets: Vec<_> = frames.iter().map(fragments).collect();
        assert_eq!(packets[0].len(), 3);
        let mut now_us = 1_000;
        assert!(
            assembler
                .push(packets[0][0].clone(), now_us)
                .frames
                .is_empty()
        );
        for (index, frame) in frames.iter().enumerate() {
            assert!(
                assembler
                    .push(packets[index][1].clone(), now_us)
                    .frames
                    .is_empty()
            );
            // The next frame starts before this one's last fragment.
            if let Some(next) = packets.get(index + 1) {
                assert!(assembler.push(next[0].clone(), now_us).frames.is_empty());
            }
            now_us += 100;
            assert_eq!(
                assembler.push(packets[index][2].clone(), now_us).frames,
                std::slice::from_ref(frame),
                "frame {} is released with its last fragment",
                index + 1
            );
        }
        assert_eq!(assembler.deadline_us(), None);
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);
        assert_eq!(assembler.stats().stale_packets_dropped, 0);
    }

    #[test]
    fn a_fragment_that_never_arrives_releases_later_frames_after_the_wait() {
        let config = ReassemblyConfig {
            reorder_wait: Duration::from_millis(100),
            ..ReassemblyConfig::default()
        };
        let mut assembler = FrameReassembler::new(config);
        let keyframe = frame(1, b"key");
        let lost = fragments(&frame(2, b"abcdef"));
        let (third, fourth) = (frame(3, b"ghi"), frame(4, b"jkl"));
        push_all(&mut assembler, &fragments(&keyframe), 1_000);
        assert!(push_all(&mut assembler, &lost[1..], 2_000).is_empty());
        assert!(push_all(&mut assembler, &fragments(&third), 18_000).is_empty());
        assert!(push_all(&mut assembler, &fragments(&fourth), 34_000).is_empty());

        // The wait starts when the first newer frame completed.
        let deadline_us = 18_000 + 100_000;
        assert_eq!(assembler.deadline_us(), Some(deadline_us));
        assert!(assembler.expire(deadline_us - 1).is_empty());
        assert_eq!(ids(&assembler.expire(deadline_us)), [3, 4]);
        assert_eq!(assembler.stats().incomplete_frames_dropped, 1);
        assert_eq!(assembler.deadline_us(), None);

        // The gap is reported once; its fragment, when it comes, is late.
        assert_eq!(
            assembler.push(lost[0].clone(), deadline_us + 1).outcome,
            ReassemblyOutcome::DroppedStale
        );
        assert!(assembler.expire(deadline_us + stale_after_us()).is_empty());
        assert_eq!(assembler.stats().incomplete_frames_dropped, 1);
        // A late fragment of a frame released already is late too.
        assert_eq!(
            assembler
                .push(fragments(&third).remove(0), deadline_us + 2)
                .outcome,
            ReassemblyOutcome::DroppedStale
        );
    }

    /// A frame of which nothing arrived is a gap in frame IDs, which the
    /// viewer reports as before.
    #[test]
    fn a_frame_that_never_arrives_releases_later_frames_after_the_wait() {
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        push_all(&mut assembler, &fragments(&frame(1, b"key")), 1_000);
        assert!(push_all(&mut assembler, &fragments(&frame(3, b"abc")), 34_000).is_empty());
        // A packet arriving later gives it up as well.
        let released = push_all(
            &mut assembler,
            &fragments(&frame(4, b"def")),
            34_000 + reorder_wait_us(),
        );
        assert_eq!(ids(&released), [3, 4]);
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);
        // Its retransmission, too late, is dropped.
        assert_eq!(
            assembler
                .push(fragments(&frame(2, b"ghi")).remove(0), 400_000)
                .outcome,
            ReassemblyOutcome::DroppedStale
        );
    }

    #[test]
    fn a_complete_keyframe_goes_at_once_and_supersedes_older_frames() {
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        push_all(&mut assembler, &fragments(&frame(1, b"key")), 1_000);
        let incomplete = fragments(&frame(2, b"abcdef"));
        assembler.push(incomplete[0].clone(), 2_000);
        let mut keyframe = frame(3, b"key");
        keyframe.keyframe = true;
        assert_eq!(
            push_all(&mut assembler, &fragments(&keyframe), 18_000),
            [keyframe]
        );
        // Frame 2 was not needed: no keyframe is asked for because of it.
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);
        assert_eq!(assembler.deadline_us(), None);
        assert_eq!(
            assembler.push(incomplete[1].clone(), 19_000).outcome,
            ReassemblyOutcome::DroppedStale
        );
    }

    /// An unordered channel can deliver delta frames before all fragments
    /// of the bootstrap keyframe; they cannot be decoded without it.
    #[test]
    fn frames_after_an_incomplete_bootstrap_keyframe_wait_for_it() {
        let keyframe = frame(1, b"keyframe");
        let keyframe_packets = fragment_frame(&keyframe, 4).unwrap();
        let (second, third) = (frame(2, b"delta"), frame(3, b"delta"));
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());

        assert_eq!(
            assembler.push(keyframe_packets[0].clone(), 1_000).outcome,
            ReassemblyOutcome::Accepted
        );
        assert!(push_all(&mut assembler, &fragments(&second), 2_000).is_empty());
        assert!(push_all(&mut assembler, &fragments(&third), 3_000).is_empty());
        // However long the deltas wait, the keyframe goes on while its
        // fragments do.
        assert_eq!(assembler.deadline_us(), Some(1_000 + stale_after_us()));
        assert!(assembler.expire(1_000 + stale_after_us() - 1).is_empty());
        let released = assembler.push(keyframe_packets[1].clone(), 1_000 + stale_after_us() - 1);
        assert_eq!(released.frames, [keyframe, second, third]);
    }

    /// Frames come after a keyframe paced out over longer than the wait.
    #[test]
    fn a_long_keyframe_is_kept_while_frames_wait_behind_it() {
        let config = ReassemblyConfig {
            reorder_wait: Duration::from_millis(50),
            ..ReassemblyConfig::default()
        };
        let keyframe = frame(1, &[7; 30]);
        let packets = fragment_frame(&keyframe, 10).unwrap();
        let mut assembler = FrameReassembler::new(config);
        assembler.push(packets[0].clone(), 1_000_000);
        assert!(push_all(&mut assembler, &fragments(&frame(2, b"abc")), 1_010_000).is_empty());
        assembler.push(packets[1].clone(), 1_200_000);
        assert!(assembler.expire(1_399_999).is_empty());
        let released = assembler.push(packets[2].clone(), 1_399_999);
        assert_eq!(ids(&released.frames), [1, 2]);
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);
    }

    #[test]
    fn duplicate_packet_is_ignored() {
        let packets = fragments(&frame(1, b"abcdef"));
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        assert_eq!(
            assembler.push(packets[0].clone(), 1).outcome,
            ReassemblyOutcome::Accepted
        );
        assert_eq!(
            assembler.push(packets[0].clone(), 2).outcome,
            ReassemblyOutcome::Duplicate
        );
        assert_eq!(assembler.stats().duplicate_packets, 1);

        // So is one of a complete frame waiting for an older frame.
        push_all(&mut assembler, &packets[1..], 3);
        let waiting = fragments(&frame(3, b"abc"));
        push_all(&mut assembler, &waiting, 4);
        assert_eq!(
            assembler.push(waiting[0].clone(), 5).outcome,
            ReassemblyOutcome::Duplicate
        );
        assert_eq!(assembler.stats().duplicate_packets, 2);
    }

    #[test]
    fn incomplete_frame_expires() {
        let packet = fragments(&frame(1, b"abcdef")).remove(0);
        let mut assembler = FrameReassembler::new(ReassemblyConfig {
            stale_after: Duration::from_millis(10),
            ..ReassemblyConfig::default()
        });
        assert_eq!(
            assembler.push(packet, 1_000).outcome,
            ReassemblyOutcome::Accepted
        );
        assert_eq!(assembler.deadline_us(), Some(11_000));
        assert!(assembler.expire(11_000).is_empty());
        assert_eq!(assembler.stats().incomplete_frames_dropped, 1);
        assert_eq!(assembler.deadline_us(), None);
    }

    /// A large keyframe paced out at a low bitrate takes far longer than
    /// `stale_after` to arrive; it expires only once its fragments stop.
    #[test]
    fn a_frame_whose_fragments_keep_arriving_does_not_expire() {
        let keyframe = frame(1, &[7; 100]);
        let packets = fragment_frame(&keyframe, 10).unwrap();
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        // A fragment every 100 ms: the last arrives 900 ms after the first.
        let (last, others) = packets.split_last().unwrap();
        for (index, packet) in others.iter().enumerate() {
            let now_us = 1_000_000 + index as u64 * 100_000;
            assert_eq!(
                assembler.push(packet.clone(), now_us).outcome,
                ReassemblyOutcome::Accepted
            );
            assert!(assembler.expire(now_us + 99_000).is_empty());
        }
        assert_eq!(assembler.push(last.clone(), 1_900_000).frames, [keyframe]);
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);

        // One that stops arriving still expires after `stale_after`.
        let stalled = fragment_frame(&frame(2, &[7; 100]), 10).unwrap();
        assembler.push(stalled[0].clone(), 2_000_000);
        assembler.push(stalled[1].clone(), 2_100_000);
        assembler.expire(2_100_000 + stale_after_us() - 1);
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);
        assembler.expire(2_100_000 + stale_after_us());
        assert_eq!(assembler.stats().incomplete_frames_dropped, 1);
    }

    #[test]
    fn a_new_stream_starts_afresh_and_the_old_ones_packets_are_late() {
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        push_all(&mut assembler, &fragments(&frame(1, b"key")), 1_000);
        let old = fragments(&frame(5, b"abcdef"));
        assembler.push(old[0].clone(), 2_000);
        // The new stream's frame IDs need not continue the old one's.
        let mut keyframe = frame(1, b"new");
        keyframe.stream_id = VideoStreamId(8);
        assert_eq!(
            push_all(&mut assembler, &fragments(&keyframe), 3_000),
            [keyframe]
        );
        assert_eq!(assembler.deadline_us(), None);
        assert_eq!(
            assembler.push(old[1].clone(), 4_000).outcome,
            ReassemblyOutcome::DroppedStale
        );
        assert_eq!(assembler.stats().incomplete_frames_dropped, 0);
    }

    /// Frames, packet counts, frame IDs and sizes from a peer sending
    /// garbage never hold more than the limits.
    #[test]
    fn held_frames_and_bytes_stay_within_the_limits_under_garbage() {
        let config = ReassemblyConfig {
            max_frame_bytes: 1_000,
            max_packets: 64,
            max_held_frames: 4,
            max_held_bytes: 2_500,
            ..ReassemblyConfig::default()
        };
        let mut assembler = FrameReassembler::new(config);
        let mut random = 0x1234_5678_9abc_def0_u64;
        let mut next = move |bound: u64| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random % bound
        };
        let mut released = 0;
        for step in 0..200_000_u64 {
            let packet_count = 1 + next(80) as u16;
            let packet = VideoPacket {
                stream_id: VideoStreamId(1 + next(3) as u32),
                frame_id: next(64),
                capture_timestamp_us: next(2),
                encode_complete_timestamp_us: 0,
                send_timestamp_us: 0,
                keyframe: next(8) == 0,
                packet_index: next(u64::from(packet_count) + 1) as u16,
                packet_count,
                payload: vec![0; next(1_200) as usize],
            };
            released += assembler.push(packet, step * 1_000).frames.len();
            assert!(assembler.held.len() <= config.max_held_frames);
            assert!(assembler.held_bytes <= config.max_held_bytes);
            assert_eq!(
                assembler.held_bytes,
                assembler.held.values().map(HeldFrame::bytes).sum::<usize>()
            );
        }
        // Some of it made frames, which went in order.
        assert!(released > 0);
    }

    #[test]
    fn malformed_packets_are_rejected_without_panicking() {
        assert_eq!(VideoPacket::decode(&[]), Err(VideoPacketError::Truncated));
        let mut packet = fragment_frame(&frame(1, b"hello"), 10)
            .unwrap()
            .remove(0)
            .encode()
            .unwrap();
        packet[0] = 0;
        assert_eq!(
            VideoPacket::decode(&packet),
            Err(VideoPacketError::InvalidMagic)
        );
    }
}
