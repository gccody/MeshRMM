use crate::{MAX_CLIPBOARD_TEXT_BYTES, SessionMessage};
use serde::{Deserialize, Serialize};

pub const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CLIPBOARD_WIRE_BYTES: usize = MAX_CLIPBOARD_BYTES + 64;
const CHUNK_BYTES: usize = 60 * 1024;

/// What one copy put on the clipboard, in every format this carries: a copied
/// picture usually comes with HTML that shows it, rich text with its plain
/// text, and each paste target takes the format it understands. The formats
/// share [`MAX_CLIPBOARD_BYTES`]; `left_out` says what a larger copy lost.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipboardContent {
    pub text: Option<String>,
    pub html: Option<String>,
    pub image: Option<ClipboardImage>,
    pub left_out: Option<LeftOut>,
}

/// Tightly packed RGBA8 pixels, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A copy larger than [`MAX_CLIPBOARD_BYTES`]: its size, and the dimensions of
/// its image when only the image was dropped. A copy that lost everything
/// carries only this, so the side it was meant for can say why nothing came.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LeftOut {
    pub bytes: u64,
    pub image: Option<(u32, u32)>,
}

impl From<String> for ClipboardContent {
    fn from(text: String) -> Self {
        Self {
            text: Some(text),
            ..Self::default()
        }
    }
}

impl From<&str> for ClipboardContent {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl ClipboardImage {
    fn valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.rgba.len().is_multiple_of(4)
            && u64::from(self.width) * u64::from(self.height) == (self.rgba.len() / 4) as u64
    }
}

impl ClipboardContent {
    /// Whether this holds any format to put on a clipboard.
    pub fn has_formats(&self) -> bool {
        self.text.is_some() || self.html.is_some() || self.image.is_some()
    }

    /// The bytes the formats take together.
    pub fn bytes(&self) -> usize {
        [
            self.text.as_ref().map_or(0, String::len),
            self.html.as_ref().map_or(0, String::len),
            self.image.as_ref().map_or(0, |image| image.rgba.len()),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }

    pub fn valid(&self) -> bool {
        (self.has_formats() || self.left_out.is_some())
            && self.bytes() <= MAX_CLIPBOARD_BYTES
            && self.image.as_ref().is_none_or(ClipboardImage::valid)
            && self.left_out.is_none_or(|left_out| {
                left_out.bytes > MAX_CLIPBOARD_BYTES as u64
                    && left_out
                        .image
                        .is_none_or(|(width, height)| width > 0 && height > 0)
            })
    }

    /// This copy as a session can send it: whole when it fits in
    /// [`MAX_CLIPBOARD_BYTES`], else without its image when the rest fits, else
    /// no format at all, with `left_out` saying what was dropped.
    pub fn limited(mut self) -> Self {
        let bytes = self.bytes();
        if bytes <= MAX_CLIPBOARD_BYTES {
            return self;
        }
        let image_bytes = self.image.as_ref().map_or(0, |image| image.rgba.len());
        if bytes - image_bytes <= MAX_CLIPBOARD_BYTES
            && let Some(image) = self.image.take()
        {
            self.left_out = Some(LeftOut {
                bytes: bytes as u64,
                image: Some((image.width, image.height)),
            });
            return self;
        }
        Self {
            left_out: Some(LeftOut {
                bytes: bytes as u64,
                image: None,
            }),
            ..Self::default()
        }
    }

    /// What the copy lost for being too large, to tell whoever expects it.
    pub fn left_out_problem(&self) -> Option<String> {
        let left_out = self.left_out?;
        let limit = MAX_CLIPBOARD_BYTES / (1024 * 1024);
        let size = format!("{:.1} MB", left_out.bytes as f64 / (1024.0 * 1024.0));
        Some(match left_out.image {
            Some((width, height)) if self.has_formats() => format!(
                "the copy is {size}, more than the {limit} MB a session's clipboard carries, so its {width} × {height} image was not sent, only the rest of it"
            ),
            Some((width, height)) => format!(
                "the copied {width} × {height} image is {size}, more than the {limit} MB a session's clipboard carries, so it was not sent"
            ),
            None => format!(
                "the copy is {size}, more than the {limit} MB a session's clipboard carries, so it was not sent"
            ),
        })
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
        if let Self {
            text: Some(text),
            html: None,
            image: None,
            left_out: None,
        } = self
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
                Ok(Some(ClipboardContent::from(text)))
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

    fn image(width: u32, height: u32) -> ClipboardImage {
        ClipboardImage {
            width,
            height,
            rgba: vec![255; width as usize * height as usize * 4],
        }
    }

    fn html(html: &str, text: &str) -> ClipboardContent {
        ClipboardContent {
            text: Some(text.into()),
            html: Some(html.into()),
            ..ClipboardContent::default()
        }
    }

    /// Sends `content` through `messages()` and a receiver, as a session does.
    fn carried(content: &ClipboardContent) -> Option<ClipboardContent> {
        let mut receiver = ClipboardReceiver::default();
        let mut received = None;
        for message in content.messages().unwrap() {
            let bytes = message.encode().unwrap();
            assert!(bytes.len() < 65536);
            received = receiver
                .receive(SessionMessage::decode(&bytes).unwrap())
                .unwrap();
        }
        received
    }

    #[test]
    fn formats_round_trip_in_bounded_messages() {
        for content in [
            ClipboardContent::from("hello é"),
            html(&"<b>hello</b>".repeat(10000), "hello"),
            ClipboardContent {
                image: Some(image(512, 512)),
                ..ClipboardContent::default()
            },
            ClipboardContent {
                image: Some(image(512, 512)),
                ..html(r#"<img src="blob:https://example.com/1">"#, "")
            },
            ClipboardContent {
                left_out: Some(LeftOut {
                    bytes: MAX_CLIPBOARD_BYTES as u64 + 1,
                    image: Some((5120, 2880)),
                }),
                ..ClipboardContent::default()
            },
        ] {
            assert_eq!(carried(&content), Some(content));
        }
    }

    /// A copied picture's image, HTML and text travel together.
    #[test]
    fn an_image_travels_with_its_html_and_text() {
        let content = ClipboardContent {
            text: Some("error dialog".into()),
            html: Some(r#"<img src="https://intranet/error.png" alt="error dialog">"#.into()),
            image: Some(image(3, 2)),
            left_out: None,
        };
        let received = carried(&content).unwrap();
        assert_eq!(received.text.as_deref(), Some("error dialog"));
        assert_eq!(received.image, Some(image(3, 2)));
        assert_eq!(received, content);
    }

    /// The formats share the limit: a copy of exactly the limit travels, and
    /// one byte more is refused by the sender and by the receiver.
    #[test]
    fn the_limit_covers_all_formats_together() {
        let image_bytes = 2048 * 1024 * 4;
        let rest = MAX_CLIPBOARD_BYTES - image_bytes;
        let at_limit = ClipboardContent {
            text: Some("t".repeat(rest / 2)),
            html: Some("h".repeat(rest - rest / 2)),
            image: Some(image(2048, 1024)),
            left_out: None,
        };
        assert_eq!(at_limit.bytes(), MAX_CLIPBOARD_BYTES);
        assert!(at_limit.valid());
        assert_eq!(carried(&at_limit).as_ref(), Some(&at_limit));
        assert_eq!(at_limit.clone().limited(), at_limit);

        let mut over = at_limit;
        over.text.as_mut().unwrap().push('t');
        assert!(!over.valid());
        assert!(over.messages().is_err());
        let forced = postcard::to_stdvec(&over).unwrap();
        assert!(forced.len() <= MAX_CLIPBOARD_WIRE_BYTES);
        assert!(ClipboardContent::decode(&forced).is_err());
    }

    /// A 3840 × 2160 screenshot fits by itself; 5K does not.
    #[test]
    fn a_4k_screenshot_fits_and_a_5k_one_does_not() {
        let screenshot = |width, height| ClipboardContent {
            image: Some(image(width, height)),
            ..ClipboardContent::default()
        };
        assert!(screenshot(3840, 2160).valid());
        assert!(!screenshot(5120, 2880).valid());
    }

    /// Too large a copy drops its image when the rest fits, else everything,
    /// and says what it lost so the other side can tell its user.
    #[test]
    fn too_large_a_copy_says_what_it_left_out() {
        let big = || image(5120, 2880);
        let image_only = ClipboardContent {
            image: Some(big()),
            ..ClipboardContent::default()
        }
        .limited();
        assert!(!image_only.has_formats());
        assert_eq!(
            image_only.left_out,
            Some(LeftOut {
                bytes: 5120 * 2880 * 4,
                image: Some((5120, 2880)),
            })
        );
        assert!(image_only.valid());
        assert_eq!(carried(&image_only).as_ref(), Some(&image_only));
        assert!(
            image_only
                .left_out_problem()
                .unwrap()
                .contains("the copied 5120 × 2880 image is 56.")
        );

        let with_html = ClipboardContent {
            image: Some(big()),
            ..html("<p>report</p>", "report")
        }
        .limited();
        assert_eq!(with_html.html.as_deref(), Some("<p>report</p>"));
        assert_eq!(with_html.text.as_deref(), Some("report"));
        assert_eq!(with_html.image, None);
        assert!(with_html.valid());
        assert!(
            with_html
                .left_out_problem()
                .unwrap()
                .ends_with("image was not sent, only the rest of it")
        );

        let huge_html = ClipboardContent {
            image: Some(image(1, 1)),
            ..html(&"h".repeat(MAX_CLIPBOARD_BYTES + 1), "")
        }
        .limited();
        assert!(!huge_html.has_formats());
        assert_eq!(huge_html.left_out.unwrap().image, None);
        assert!(huge_html.valid());

        assert_eq!(ClipboardContent::from("small").left_out_problem(), None);
        assert!(
            !ClipboardContent {
                left_out: Some(LeftOut {
                    bytes: 1,
                    image: None
                }),
                ..ClipboardContent::default()
            }
            .valid(),
            "a copy within the limit has nothing left out"
        );
    }

    #[test]
    fn interrupted_transfer_is_replaced_by_new_clipboard() {
        let large = html(&"<b>x</b>".repeat(10000), "x");
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
        assert!(ClipboardContent::from("é".repeat(MAX_CLIPBOARD_BYTES / 2)).valid());
        assert!(!html(&"x".repeat(MAX_CLIPBOARD_BYTES), "x").valid());
        assert!(
            !ClipboardContent::default().valid(),
            "a copy carries a format or what it left out"
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
        let with_image = |width, height, rgba| ClipboardContent {
            image: Some(ClipboardImage {
                width,
                height,
                rgba,
            }),
            ..ClipboardContent::default()
        };
        assert!(!with_image(u32::MAX, u32::MAX, vec![0; 4]).valid());
        assert!(!with_image(0, 1, vec![]).valid());
        assert!(!with_image(1, 1, vec![0; 3]).valid());
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
