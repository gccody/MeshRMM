use crate::{MAX_CLIPBOARD_TEXT_BYTES, SessionMessage};
use serde::{Deserialize, Serialize};

pub const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CLIPBOARD_WIRE_BYTES: usize = MAX_CLIPBOARD_BYTES + 64;
const CHUNK_BYTES: usize = 60 * 1024;

/// HTML carries a plain-text alternative; images use tightly packed RGBA8 pixels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
    Html {
        html: String,
        text: String,
    },
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

impl From<&str> for ClipboardContent {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

impl ClipboardContent {
    pub fn valid(&self) -> bool {
        match self {
            Self::Text(text) => text.len() <= MAX_CLIPBOARD_BYTES,
            Self::Html { html, text } => {
                html.len().saturating_add(text.len()) <= MAX_CLIPBOARD_BYTES
            }
            Self::Image {
                width,
                height,
                rgba,
            } => {
                *width > 0
                    && *height > 0
                    && rgba.len() <= MAX_CLIPBOARD_BYTES
                    && u64::from(*width) * u64::from(*height) == (rgba.len() / 4) as u64
                    && rgba.len().is_multiple_of(4)
            }
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        if !self.valid() {
            return Err(postcard::Error::SerializeBufferFull);
        }
        postcard::to_stdvec(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        if bytes.len() > MAX_CLIPBOARD_WIRE_BYTES {
            return Err(postcard::Error::DeserializeBadEncoding);
        }
        let (content, remaining): (Self, _) = postcard::take_from_bytes(bytes)?;
        if !content.valid() || !remaining.is_empty() {
            return Err(postcard::Error::DeserializeBadEncoding);
        }
        Ok(content)
    }
    pub fn messages(&self) -> Result<Vec<SessionMessage>, postcard::Error> {
        if let Self::Text(text) = self
            && text.len() <= MAX_CLIPBOARD_TEXT_BYTES
        {
            return Ok(vec![SessionMessage::Clipboard { text: text.clone() }]);
        }
        let bytes = self.encode()?;
        Ok(bytes
            .chunks(CHUNK_BYTES)
            .enumerate()
            .map(|(index, data)| SessionMessage::ClipboardChunk {
                offset: (index * CHUNK_BYTES) as u32,
                total: bytes.len() as u32,
                data: data.to_vec(),
            })
            .collect())
    }
}

/// A reliable ordered channel needs only one in-flight clipboard per direction.
#[derive(Default)]
pub struct ClipboardReceiver {
    bytes: Vec<u8>,
    total: usize,
}
impl ClipboardReceiver {
    pub fn receive(
        &mut self,
        message: SessionMessage,
    ) -> Result<Option<ClipboardContent>, postcard::Error> {
        match message {
            SessionMessage::Clipboard { text } => {
                self.bytes.clear();
                self.total = 0;
                if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                    return Err(postcard::Error::DeserializeBadEncoding);
                }
                Ok(Some(ClipboardContent::Text(text)))
            }
            SessionMessage::ClipboardChunk {
                offset,
                total,
                data,
            } => {
                let offset = offset as usize;
                let total = total as usize;
                if offset == 0 {
                    self.bytes.clear();
                    self.total = total;
                }
                if total == 0
                    || total > MAX_CLIPBOARD_WIRE_BYTES
                    || total != self.total
                    || offset != self.bytes.len()
                    || data.is_empty()
                    || data.len() > CHUNK_BYTES
                    || offset.saturating_add(data.len()) > total
                {
                    self.bytes.clear();
                    self.total = 0;
                    return Err(postcard::Error::DeserializeBadEncoding);
                }
                self.bytes.extend(data);
                if self.bytes.len() == total {
                    let result = ClipboardContent::decode(&self.bytes);
                    self.bytes.clear();
                    self.total = 0;
                    result.map(Some)
                } else {
                    Ok(None)
                }
            }
            _ => Err(postcard::Error::DeserializeBadEncoding),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_round_trip_in_bounded_messages() {
        for content in [
            ClipboardContent::from("hello é"),
            ClipboardContent::Html {
                html: "<b>hello</b>".repeat(10000),
                text: "hello".into(),
            },
            ClipboardContent::Image {
                width: 512,
                height: 512,
                rgba: vec![255; 512 * 512 * 4],
            },
        ] {
            let mut receiver = ClipboardReceiver::default();
            let mut received = None;
            for message in content.messages().unwrap() {
                let bytes = message.encode().unwrap();
                assert!(bytes.len() < 65536);
                received = receiver
                    .receive(SessionMessage::decode(&bytes).unwrap())
                    .unwrap();
            }
            assert_eq!(received, Some(content));
        }
    }
    #[test]
    fn interrupted_transfer_is_replaced_by_new_clipboard() {
        let large = ClipboardContent::Html {
            html: "<b>x</b>".repeat(10000),
            text: "x".into(),
        };
        let messages = large.messages().unwrap();
        let mut receiver = ClipboardReceiver::default();
        assert_eq!(receiver.receive(messages[0].clone()).unwrap(), None);
        assert_eq!(
            receiver
                .receive(SessionMessage::Clipboard {
                    text: "newer".into()
                })
                .unwrap(),
            Some("newer".into())
        );
        assert!(receiver.receive(messages[1].clone()).is_err());
        let mut result = None;
        for message in messages {
            result = receiver.receive(message).unwrap();
        }
        assert_eq!(result, Some(large));
    }

    #[test]
    fn payload_validation_checks_encoded_text_size_and_trailing_bytes() {
        assert!(ClipboardContent::Text("é".repeat(MAX_CLIPBOARD_BYTES / 2)).valid());
        assert!(
            !ClipboardContent::Html {
                html: "x".repeat(MAX_CLIPBOARD_BYTES),
                text: "x".into()
            }
            .valid()
        );
        let mut encoded = ClipboardContent::from("text").encode().unwrap();
        encoded.push(0);
        assert!(ClipboardContent::decode(&encoded).is_err());
        assert_eq!(
            ClipboardContent::from("text").messages().unwrap(),
            vec![SessionMessage::Clipboard {
                text: "text".into()
            }]
        );
    }

    #[test]
    fn rejects_invalid_images_and_chunks_and_recovers() {
        assert!(
            !ClipboardContent::Image {
                width: u32::MAX,
                height: u32::MAX,
                rgba: vec![0; 4]
            }
            .valid()
        );
        assert!(
            !ClipboardContent::Image {
                width: 0,
                height: 1,
                rgba: vec![]
            }
            .valid()
        );
        let mut receiver = ClipboardReceiver::default();
        for (offset, total, data) in [
            (1, 2, vec![0]),
            (0, u32::MAX, vec![0]),
            (0, 2, vec![]),
            (0, 1, vec![0, 0]),
        ] {
            assert!(
                receiver
                    .receive(SessionMessage::ClipboardChunk {
                        offset,
                        total,
                        data
                    })
                    .is_err()
            );
        }
        assert_eq!(
            receiver
                .receive(SessionMessage::Clipboard { text: "ok".into() })
                .unwrap(),
            Some("ok".into())
        );
    }
}
