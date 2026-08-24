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
    pub stale_after: Duration,
    pub max_frame_bytes: usize,
    pub max_packets: u16,
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            // Allow one bounded SCTP retransmission on typical WAN RTTs before
            // declaring an access unit incomplete.
            stale_after: Duration::from_millis(250),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_packets: DEFAULT_MAX_PACKETS,
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
    Duplicate,
    DroppedStale,
    DroppedInvalid,
    Completed(EncodedFrame),
}

#[derive(Debug)]
struct PendingFrame {
    stream_id: VideoStreamId,
    frame_id: u64,
    capture_timestamp_us: u64,
    encode_complete_timestamp_us: u64,
    send_timestamp_us: u64,
    keyframe: bool,
    first_packet_at_us: u64,
    total_bytes: usize,
    received: usize,
    fragments: Vec<Option<Vec<u8>>>,
}

/// Reassembles only the newest frame. Arrival of a newer frame immediately drops
/// any incomplete older frame so network loss cannot create a latency backlog.
#[derive(Debug)]
pub struct FrameReassembler {
    config: ReassemblyConfig,
    pending: Option<PendingFrame>,
    newest_frame_id: Option<u64>,
    stats: ReassemblyStats,
}

impl FrameReassembler {
    pub fn new(config: ReassemblyConfig) -> Self {
        Self {
            config,
            pending: None,
            newest_frame_id: None,
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    pub fn expire_stale(&mut self, now_us: u64) -> bool {
        let expired = self.pending.as_ref().is_some_and(|pending| {
            now_us.saturating_sub(pending.first_packet_at_us)
                >= self.config.stale_after.as_micros() as u64
        });
        if expired {
            self.pending = None;
            self.stats.incomplete_frames_dropped += 1;
        }
        expired
    }

    pub fn push(&mut self, packet: VideoPacket, received_at_us: u64) -> ReassemblyOutcome {
        if packet.packet_count == 0
            || packet.packet_count > self.config.max_packets
            || packet.packet_index >= packet.packet_count
            || packet.payload.len() > self.config.max_frame_bytes
        {
            self.stats.invalid_packets += 1;
            return ReassemblyOutcome::DroppedInvalid;
        }

        if self.expire_stale(received_at_us) {
            // Continue accepting this packet: it may begin the next useful frame.
        }

        // An unordered channel can deliver delta-frame fragments before all
        // fragments of the bootstrap keyframe. Preserve that one incomplete
        // keyframe until it completes or expires; deltas cannot be decoded
        // without it and therefore are not more useful merely because newer.
        if self.pending.as_ref().is_some_and(|pending| {
            pending.keyframe && !packet.keyframe && packet.frame_id > pending.frame_id
        }) {
            self.stats.stale_packets_dropped += 1;
            return ReassemblyOutcome::DroppedStale;
        }

        let is_late_for_completed_frame = self.pending.is_none()
            && self
                .newest_frame_id
                .is_some_and(|newest| packet.frame_id == newest);
        if is_late_for_completed_frame
            || self
                .newest_frame_id
                .is_some_and(|newest| packet.frame_id < newest)
        {
            self.stats.stale_packets_dropped += 1;
            return ReassemblyOutcome::DroppedStale;
        }

        let is_new = self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.frame_id != packet.frame_id);
        if is_new {
            if self.pending.take().is_some() {
                self.stats.incomplete_frames_dropped += 1;
            }
            self.newest_frame_id = Some(packet.frame_id);
            self.pending = Some(PendingFrame {
                stream_id: packet.stream_id,
                frame_id: packet.frame_id,
                capture_timestamp_us: packet.capture_timestamp_us,
                encode_complete_timestamp_us: packet.encode_complete_timestamp_us,
                send_timestamp_us: packet.send_timestamp_us,
                keyframe: packet.keyframe,
                first_packet_at_us: received_at_us,
                total_bytes: 0,
                received: 0,
                fragments: vec![None; packet.packet_count as usize],
            });
        }

        let Some(pending) = self.pending.as_mut() else {
            self.stats.invalid_packets += 1;
            return ReassemblyOutcome::DroppedInvalid;
        };
        if pending.fragments.len() != packet.packet_count as usize
            || pending.stream_id != packet.stream_id
            || pending.capture_timestamp_us != packet.capture_timestamp_us
            || pending.encode_complete_timestamp_us != packet.encode_complete_timestamp_us
            || pending.keyframe != packet.keyframe
            || pending.total_bytes.saturating_add(packet.payload.len())
                > self.config.max_frame_bytes
        {
            self.pending = None;
            self.stats.invalid_packets += 1;
            return ReassemblyOutcome::DroppedInvalid;
        }

        let slot = &mut pending.fragments[packet.packet_index as usize];
        if slot.is_some() {
            self.stats.duplicate_packets += 1;
            return ReassemblyOutcome::Duplicate;
        }
        pending.total_bytes += packet.payload.len();
        pending.received += 1;
        *slot = Some(packet.payload);

        if pending.received != pending.fragments.len() {
            return ReassemblyOutcome::Accepted;
        }

        let Some(pending) = self.pending.take() else {
            self.stats.invalid_packets += 1;
            return ReassemblyOutcome::DroppedInvalid;
        };
        let mut data = Vec::with_capacity(pending.total_bytes);
        for fragment in pending.fragments {
            data.extend_from_slice(fragment.as_deref().unwrap_or_default());
        }
        self.stats.completed_frames += 1;
        ReassemblyOutcome::Completed(EncodedFrame {
            stream_id: pending.stream_id,
            frame_id: pending.frame_id,
            capture_timestamp_us: pending.capture_timestamp_us,
            encode_complete_timestamp_us: pending.encode_complete_timestamp_us,
            send_timestamp_us: pending.send_timestamp_us,
            keyframe: pending.keyframe,
            data,
        })
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
        let packets = fragment_frame(&source, 3).unwrap();
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        assert_eq!(
            assembler.push(packets[2].clone(), 1),
            ReassemblyOutcome::Accepted
        );
        assert_eq!(
            assembler.push(packets[0].clone(), 2),
            ReassemblyOutcome::Accepted
        );
        assert_eq!(
            assembler.push(packets[3].clone(), 3),
            ReassemblyOutcome::Accepted
        );
        assert_eq!(
            assembler.push(packets[1].clone(), 4),
            ReassemblyOutcome::Completed(source)
        );
    }

    #[test]
    fn incomplete_keyframe_is_not_replaced_by_newer_delta_packets() {
        let keyframe = frame(1, b"keyframe");
        let mut delta = frame(2, b"delta");
        delta.keyframe = false;
        let keyframe_packets = fragment_frame(&keyframe, 4).unwrap();
        let delta_packet = fragment_frame(&delta, 16).unwrap().remove(0);
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());

        assert_eq!(
            assembler.push(keyframe_packets[0].clone(), 1),
            ReassemblyOutcome::Accepted
        );
        assert_eq!(
            assembler.push(delta_packet, 2),
            ReassemblyOutcome::DroppedStale
        );
        assert_eq!(
            assembler.push(keyframe_packets[1].clone(), 3),
            ReassemblyOutcome::Completed(keyframe)
        );
    }

    #[test]
    fn duplicate_packet_is_ignored() {
        let packets = fragment_frame(&frame(1, b"abcdef"), 3).unwrap();
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        assert_eq!(
            assembler.push(packets[0].clone(), 1),
            ReassemblyOutcome::Accepted
        );
        assert_eq!(
            assembler.push(packets[0].clone(), 2),
            ReassemblyOutcome::Duplicate
        );
        assert_eq!(assembler.stats().duplicate_packets, 1);
    }

    #[test]
    fn newer_frame_drops_incomplete_older_frame() {
        let old = fragment_frame(&frame(10, b"abcdef"), 3).unwrap();
        let new = fragment_frame(&frame(11, b"new"), 3).unwrap();
        let mut assembler = FrameReassembler::new(ReassemblyConfig::default());
        assert_eq!(
            assembler.push(old[0].clone(), 1),
            ReassemblyOutcome::Accepted
        );
        assert!(matches!(
            assembler.push(new[0].clone(), 2),
            ReassemblyOutcome::Completed(_)
        ));
        assert_eq!(assembler.stats().incomplete_frames_dropped, 1);
        assert_eq!(
            assembler.push(old[1].clone(), 3),
            ReassemblyOutcome::DroppedStale
        );
    }

    #[test]
    fn incomplete_frame_expires() {
        let packet = fragment_frame(&frame(1, b"abcdef"), 3).unwrap().remove(0);
        let mut assembler = FrameReassembler::new(ReassemblyConfig {
            stale_after: Duration::from_millis(10),
            ..ReassemblyConfig::default()
        });
        assert_eq!(assembler.push(packet, 1_000), ReassemblyOutcome::Accepted);
        assert!(assembler.expire_stale(11_000));
        assert_eq!(assembler.stats().incomplete_frames_dropped, 1);
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
