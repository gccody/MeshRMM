use anyhow::{Context, ensure};
use meshrmm_protocol::ClipboardContent;
use std::hash::{DefaultHasher, Hash, Hasher};

fn fingerprint(content: &ClipboardContent) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

pub struct ClipboardSync {
    clipboard: arboard::Clipboard,
    last: Option<u64>,
    sequence: Option<u64>,
    initialized: bool,
    send_initial: bool,
}
impl ClipboardSync {
    pub fn new(send_initial: bool) -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new().context("native clipboard unavailable")?,
            last: None,
            sequence: None,
            initialized: false,
            send_initial,
        })
    }
    fn read(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        if meshrmm_file_transfer::clipboard_has_files() {
            return Ok(None);
        }
        let text = optional(self.clipboard.get_text())?;
        if let Some(html) = optional(self.clipboard.get().html())? {
            return Ok(Some(ClipboardContent::Html {
                html,
                text: text.unwrap_or_default(),
            }));
        }
        if let Some(image) = optional(self.clipboard.get_image())? {
            return Ok(Some(ClipboardContent::Image {
                width: image.width.try_into()?,
                height: image.height.try_into()?,
                rgba: image.bytes.into_owned(),
            }));
        }
        Ok(text.map(ClipboardContent::Text))
    }
    pub fn poll(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        let sequence = meshrmm_file_transfer::clipboard_sequence();
        if self.sequence == Some(sequence) {
            return Ok(None);
        }
        let content = self.read()?;
        self.sequence = Some(sequence);
        let next = content.as_ref().map(fingerprint);
        let changed = self.last != next;
        self.last = next;
        let first = !self.initialized;
        self.initialized = true;
        if !changed || (first && !self.send_initial) {
            return Ok(None);
        }
        if let Some(content) = &content {
            ensure!(
                content.valid(),
                "clipboard exceeds size limit or has invalid image dimensions"
            );
        }
        Ok(content)
    }
    pub fn apply(&mut self, content: ClipboardContent) -> anyhow::Result<()> {
        ensure!(content.valid(), "invalid clipboard payload");
        // Read the actual clipboard: local changes may have occurred since the last poll.
        if self.read()?.as_ref() != Some(&content) {
            match &content {
                ClipboardContent::Text(text) => self.clipboard.set_text(text.as_str())?,
                ClipboardContent::Html { html, text } => self
                    .clipboard
                    .set_html(html.as_str(), Some(text.as_str()))?,
                ClipboardContent::Image {
                    width,
                    height,
                    rgba,
                } => self.clipboard.set_image(arboard::ImageData {
                    width: *width as usize,
                    height: *height as usize,
                    bytes: rgba.as_slice().into(),
                })?,
            }
        }
        // Native APIs may normalize HTML or image pixels; remember the published form.
        let sequence = meshrmm_file_transfer::clipboard_sequence();
        self.last = self.read()?.as_ref().map(fingerprint);
        self.sequence = Some(sequence);
        self.initialized = true;
        Ok(())
    }
}
fn optional<T>(result: Result<T, arboard::Error>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(arboard::Error::ContentNotAvailable | arboard::Error::ClipboardNotSupported) => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}
