use anyhow::{Context, bail};
use meshrmm_protocol::MAX_CLIPBOARD_TEXT_BYTES;
use std::hash::{DefaultHasher, Hash, Hasher};

#[derive(Clone, Copy, PartialEq, Eq)]
struct ClipboardFingerprint {
    bytes: usize,
    hash: u64,
}

impl ClipboardFingerprint {
    fn of(text: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        Self {
            bytes: text.len(),
            hash: hasher.finish(),
        }
    }
}

/// Polls the interactive Windows clipboard and suppresses peer-originated
/// writes so received text is not immediately echoed back to the viewer.
pub struct ClipboardSync {
    clipboard: arboard::Clipboard,
    last_fingerprint: Option<ClipboardFingerprint>,
    initialized: bool,
}

impl ClipboardSync {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new()
                .context("interactive Windows clipboard is unavailable")?,
            last_fingerprint: None,
            initialized: false,
        })
    }

    pub fn poll(&mut self) -> anyhow::Result<Option<String>> {
        let text = match self.clipboard.get_text() {
            Ok(text) => text,
            Err(arboard::Error::ContentNotAvailable) => {
                self.initialized = true;
                self.last_fingerprint = None;
                return Ok(None);
            }
            Err(error) => return Err(error).context("failed to read the Windows clipboard"),
        };
        let fingerprint = ClipboardFingerprint::of(&text);
        let changed = self.last_fingerprint != Some(fingerprint);
        self.last_fingerprint = Some(fingerprint);
        let first_poll = !self.initialized;
        self.initialized = true;
        if !changed || first_poll {
            return Ok(None);
        }
        validate_text(&text)?;
        Ok(Some(text))
    }

    pub fn apply(&mut self, text: String) -> anyhow::Result<()> {
        validate_text(&text)?;
        let fingerprint = ClipboardFingerprint::of(&text);
        if self.last_fingerprint == Some(fingerprint) {
            return Ok(());
        }
        self.clipboard
            .set_text(text)
            .context("failed to write the Windows clipboard")?;
        self.last_fingerprint = Some(fingerprint);
        self.initialized = true;
        Ok(())
    }
}

fn validate_text(text: &str) -> anyhow::Result<()> {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        bail!(
            "clipboard text is {} bytes; the session limit is {} bytes",
            text.len(),
            MAX_CLIPBOARD_TEXT_BYTES
        );
    }
    Ok(())
}
