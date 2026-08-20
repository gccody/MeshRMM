use serde::{Deserialize, Serialize};

use crate::{RemoteSessionId, VideoStreamId};

/// Reliable, low-frequency messages carried by the control data channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionMessage {
    Start {
        session_id: RemoteSessionId,
        format: VideoFormat,
    },
    Accepted {
        stream_id: VideoStreamId,
        format: VideoFormat,
    },
    Rejected {
        reason: String,
    },
    StreamConfiguration {
        stream_id: VideoStreamId,
        format: VideoFormat,
        codec_config: Vec<u8>,
    },
    RequestKeyframe {
        stream_id: VideoStreamId,
    },
    SetBitrate {
        bits_per_second: u32,
    },
    Stats(ConnectionStats),
    Stop {
        reason: String,
    },
}

impl SessionMessage {
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_stdvec(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Nv12,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
    pub codec: Codec,
    pub pixel_format: PixelFormat,
    pub bitrate_bits_per_second: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionStats {
    pub capture_fps: f32,
    pub stream_fps: f32,
    pub bitrate_bits_per_second: u64,
    pub rtt_ms: f32,
    pub packet_loss_percent: f32,
    pub encode_ms: f32,
    pub decode_ms: f32,
    pub render_ms: f32,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub frames_dropped: u64,
    pub incomplete_frames_dropped: u64,
    pub connection_path: ConnectionPath,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionPath {
    Direct,
    Turn,
    #[default]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_message_round_trip() {
        let message = SessionMessage::Start {
            session_id: RemoteSessionId::new("session_opaque_123"),
            format: VideoFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 60,
                codec: Codec::H264,
                pixel_format: PixelFormat::Nv12,
                bitrate_bits_per_second: 12_000_000,
            },
        };

        let encoded = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&encoded).unwrap(), message);
    }
}
