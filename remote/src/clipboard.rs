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

/// Polls the native clipboard and suppresses updates written by the peer so a
/// clipboard change crosses the control channel exactly once.
pub struct ClipboardSync {
    clipboard: arboard::Clipboard,
    last_fingerprint: Option<ClipboardFingerprint>,
    initialized: bool,
    send_initial: bool,
}

impl ClipboardSync {
    pub fn new(send_initial: bool) -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new().context("native clipboard is unavailable")?,
            last_fingerprint: None,
            initialized: false,
            send_initial,
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
            Err(error) => return Err(error).context("failed to read the native clipboard"),
        };
        let fingerprint = ClipboardFingerprint::of(&text);
        let changed = self.last_fingerprint != Some(fingerprint);
        self.last_fingerprint = Some(fingerprint);
        let first_poll = !self.initialized;
        self.initialized = true;
        if !changed || (first_poll && !self.send_initial) {
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
            .context("failed to write the native clipboard")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_payload_limit_accepts_utf8_by_encoded_size() {
        assert!(validate_text(&"a".repeat(MAX_CLIPBOARD_TEXT_BYTES)).is_ok());
        assert!(validate_text(&"a".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1)).is_err());
        assert!(validate_text(&"é".repeat(MAX_CLIPBOARD_TEXT_BYTES / 2)).is_ok());
    }
}
