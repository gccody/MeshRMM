//! Messages between the root coordinator and a session helper, one
//! length-prefixed postcard message at a time over a Unix socket.
use std::io::{Read, Write};

use anyhow::{Context, bail};
use meshrmm_protocol::{
    Annotation, ClipboardContent, Codec, CursorShape, Display, DisplayId, FileMessage, RemoteInput,
    VideoFormat,
};
use serde::{Deserialize, Serialize};

/// Bumped whenever a message changes; the coordinator refuses other helpers.
pub(crate) const VERSION: u32 = 2;
/// A 2560x1600 keyframe is well under this.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// The helper's first message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub version: u32,
    /// Whether the helper runs in the login window's session rather than a
    /// user's.
    pub login_window: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct StreamSettings {
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub codec: Codec,
    pub capture_cursor: bool,
    pub grayscale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Request {
    /// A remote session starts using this helper.
    BeginSession,
    /// The session no longer uses this helper: undo everything it changed.
    EndSession,
    Start {
        display_id: Option<DisplayId>,
        settings: StreamSettings,
    },
    StopCapture,
    Keyframe,
    SetBitrate(u32),
    SetCursorCapture(bool),
    Input(RemoteInput),
    ReleaseInput,
    Annotate(Annotation),
    SetWallpaperHidden(bool),
    SetPreventIdleLock(bool),
    SetBlackout(bool),
    SetAgentInputBlocked(bool),
    Clipboard(ClipboardContent),
    Files(FileMessage),
    StartChat,
    StopChat,
    Chat(String),
    StartAudio,
    StopAudio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Call {
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Reply {
    Done,
    Started {
        displays: Vec<Display>,
        active_display: Display,
        format: VideoFormat,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Frame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub capture_timestamp_us: u64,
    pub encode_complete_timestamp_us: u64,
}

/// Input state the transport polls for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct InputState {
    pub cursor: CursorShape,
    pub viewer_controls_input: bool,
    pub agent_pointer_display: Option<DisplayId>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            cursor: CursorShape::Default,
            viewer_controls_input: false,
            agent_pointer_display: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Event {
    Reply {
        id: u64,
        result: Result<Reply, String>,
    },
    /// A frame and its sequence number, which shows the coordinator when the
    /// helper had to drop frames.
    Frame(u64, Frame),
    /// Capture stopped on its own, for example when the display went away.
    CaptureEnded(String),
    InputState(InputState),
    Clipboard(ClipboardContent),
    Files(FileMessage),
    Chat(String),
    /// A PCM16 system audio packet.
    Audio(Vec<u8>),
    /// System audio capture stopped on its own.
    AudioEnded,
}

pub(crate) fn write<T: Serialize>(writer: &mut impl Write, message: &T) -> anyhow::Result<()> {
    let bytes = postcard::to_stdvec(message).context("could not encode a helper message")?;
    anyhow::ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "helper message is too large"
    );
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// The next message, or `None` when the other side closed the connection.
pub(crate) fn read<T: for<'de> Deserialize<'de>>(
    reader: &mut impl Read,
) -> anyhow::Result<Option<T>> {
    let mut length = [0; 4];
    match reader.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE_BYTES {
        bail!("helper message of {length} bytes is too large");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(Some(
        postcard::from_bytes(&bytes).context("could not decode a helper message")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_the_framing() {
        let mut buffer = Vec::new();
        let call = Call {
            id: 7,
            request: Request::Input(RemoteInput::TypeText {
                display_id: DisplayId(1),
                text: "hello".into(),
            }),
        };
        write(&mut buffer, &call).unwrap();
        write(
            &mut buffer,
            &Event::Frame(
                0,
                Frame {
                    data: vec![0, 0, 0, 1, 0x65],
                    keyframe: true,
                    capture_timestamp_us: 1,
                    encode_complete_timestamp_us: 2,
                },
            ),
        )
        .unwrap();
        let mut reader = buffer.as_slice();
        let decoded: Call = read(&mut reader).unwrap().unwrap();
        assert_eq!(decoded.id, 7);
        assert!(
            matches!(read::<Event>(&mut reader).unwrap(), Some(Event::Frame(0, frame)) if frame.keyframe)
        );
        assert!(read::<Event>(&mut reader).unwrap().is_none());
    }

    #[test]
    fn oversized_messages_are_refused() {
        let mut bytes = (MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 8]);
        assert!(read::<Event>(&mut bytes.as_slice()).is_err());
    }
}
