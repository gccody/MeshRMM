use anyhow::{Context, ensure};
use meshrmm_protocol::{ClipboardContent, ClipboardImage};
use std::hash::{DefaultHasher, Hash, Hasher};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

fn fingerprint(content: &ClipboardContent) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// The formats one clipboard update sets, in the order it offers them: some
/// programs paste the first format on the clipboard they understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Formats<'a> {
    text: Option<&'a str>,
    html: Option<&'a str>,
    image: Option<&'a ClipboardImage>,
    /// The image goes before the HTML and text: the copy is a picture.
    image_first: bool,
}

/// What a clipboard holding these formats has copied, without empty text or
/// HTML; `None` when it holds none of them.
fn content(
    text: Option<String>,
    html: Option<String>,
    image: Option<ClipboardImage>,
) -> Option<ClipboardContent> {
    let content = ClipboardContent {
        text: text.filter(|text| !text.is_empty()),
        html: html.filter(|html| !html.is_empty()),
        image,
        left_out: None,
    };
    content.has_formats().then_some(content)
}

/// Whether the copy is its image: any HTML beside it shows no text of its own,
/// like the `<img>` that "Copy image" in a browser puts there.
fn image_is_the_copy(content: &ClipboardContent) -> bool {
    content.image.is_some()
        && content
            .html
            .as_deref()
            .is_none_or(|html| !has_text_of_its_own(html))
}

/// Whether HTML shows any text, outside its tags, comments and the elements
/// whose content is not shown.
fn has_text_of_its_own(html: &str) -> bool {
    const HIDDEN: [&str; 4] = ["script", "style", "title", "template"];
    let mut rest = html;
    let mut hidden: Option<String> = None;
    while !rest.is_empty() {
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
        } else if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |end| end + 1);
            let tag = rest[1..end].trim_end_matches('>');
            rest = &rest[end..];
            let closing = tag.starts_with('/');
            let name = tag
                .trim_start_matches('/')
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            if closing && hidden.as_ref() == Some(&name) {
                hidden = None;
            } else if !closing && hidden.is_none() && HIDDEN.contains(&name.as_str()) {
                hidden = Some(name);
            }
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = rest[..end]
                .replace("&nbsp;", " ")
                .replace("&#160;", " ")
                .replace("&#xa0;", " ")
                .replace("&#xA0;", " ");
            if hidden.is_none() && !text.trim().is_empty() {
                return true;
            }
            rest = &rest[end..];
        }
    }
    false
}

/// The formats to set for `content`. A clipboard that holds several formats in
/// one update gets all of them; one that holds one gets the image when it is
/// the copy, else the HTML with its text, else the text.
fn formats(content: &ClipboardContent, several: bool) -> Formats<'_> {
    let image_first = image_is_the_copy(content);
    let all = Formats {
        text: content.text.as_deref(),
        html: content.html.as_deref(),
        image: content.image.as_ref(),
        image_first,
    };
    match (several, image_first) {
        (true, _) => all,
        (false, true) => Formats {
            text: None,
            html: None,
            ..all
        },
        (false, false) => Formats { image: None, ..all },
    }
}

/// A native clipboard, as synchronization uses it.
trait Backend {
    /// Whether one update holds text, HTML and an image together.
    fn holds_several_formats(&self) -> bool;
    /// Changes whenever the clipboard's contents change.
    fn sequence(&mut self) -> u64;
    /// Copied files travel as file transfers, not here.
    fn has_files(&mut self) -> bool;
    fn read(&mut self) -> anyhow::Result<Option<ClipboardContent>>;
    /// Replaces the clipboard's contents with `formats` in one update.
    fn write(&mut self, formats: Formats<'_>) -> anyhow::Result<()>;
}

struct Synchronizer<B> {
    backend: B,
    last: Option<u64>,
    sequence: Option<u64>,
    initialized: bool,
    send_initial: bool,
}

impl<B: Backend> Synchronizer<B> {
    fn new(backend: B, send_initial: bool) -> Self {
        Self {
            backend,
            last: None,
            sequence: None,
            initialized: false,
            send_initial,
        }
    }

    fn read(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        if self.backend.has_files() {
            return Ok(None);
        }
        self.backend.read()
    }

    fn poll(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        let sequence = self.backend.sequence();
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
        // Too large a copy still goes, as what fits and a note of the rest, so
        // the other side can say why its paste lacks it.
        let content = content.map(ClipboardContent::limited);
        if let Some(content) = &content {
            ensure!(content.valid(), "clipboard image has invalid dimensions");
        }
        Ok(content)
    }

    fn apply(&mut self, content: ClipboardContent) -> anyhow::Result<()> {
        ensure!(content.valid(), "invalid clipboard payload");
        // A copy too large to send carries only a note: keep what is here.
        if !content.has_formats() {
            return Ok(());
        }
        // Read the actual clipboard: local changes may have occurred since the last poll.
        let held = self.read()?.is_some_and(|held| {
            held.text == content.text && held.html == content.html && held.image == content.image
        });
        if !held {
            let several = self.backend.holds_several_formats();
            self.backend.write(formats(&content, several))?;
        }
        // Native APIs may normalize HTML or image pixels and a clipboard may
        // hold fewer formats than were sent: remember what it now holds, read
        // the same way `poll` reads it, so the next poll does not send it back.
        let sequence = self.backend.sequence();
        self.last = self.read()?.as_ref().map(fingerprint);
        self.sequence = Some(sequence);
        self.initialized = true;
        Ok(())
    }
}

struct Native {
    clipboard: arboard::Clipboard,
}

impl Native {
    fn html(&mut self) -> anyhow::Result<Option<String>> {
        // arboard's Windows HTML reader reports Unknown (rather than
        // ContentNotAvailable) when the registered HTML format is absent.
        // Probe availability so ordinary text/image clipboards remain readable.
        #[cfg(windows)]
        if !clipboard_win::register_format("HTML Format")
            .is_some_and(|format| clipboard_win::is_format_avail(format.get()))
        {
            return Ok(None);
        }
        optional(self.clipboard.get().html())
    }
}

impl Backend for Native {
    fn holds_several_formats(&self) -> bool {
        // arboard sets one format family per update; Windows and macOS are
        // written directly. Linux keeps one (see `formats`).
        cfg!(any(windows, target_os = "macos"))
    }

    fn sequence(&mut self) -> u64 {
        meshrmm_file_transfer::clipboard_sequence()
    }

    fn has_files(&mut self) -> bool {
        meshrmm_file_transfer::clipboard_has_files()
    }

    fn read(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        let text = optional(self.clipboard.get_text())?;
        let html = self.html()?;
        let image = optional(self.clipboard.get_image())?
            .map(|image| {
                anyhow::Ok(ClipboardImage {
                    width: image.width.try_into()?,
                    height: image.height.try_into()?,
                    rgba: image.bytes.into_owned(),
                })
            })
            .transpose()?;
        Ok(content(text, html, image))
    }

    #[cfg(windows)]
    fn write(&mut self, formats: Formats<'_>) -> anyhow::Result<()> {
        windows::write(formats)
    }

    #[cfg(target_os = "macos")]
    fn write(&mut self, formats: Formats<'_>) -> anyhow::Result<()> {
        macos::write(formats)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn write(&mut self, formats: Formats<'_>) -> anyhow::Result<()> {
        if let Some(image) = formats.image {
            self.clipboard.set_image(arboard::ImageData {
                width: image.width as usize,
                height: image.height as usize,
                bytes: image.rgba.as_slice().into(),
            })?;
        } else if let Some(html) = formats.html {
            self.clipboard.set_html(html, formats.text)?;
        } else if let Some(text) = formats.text {
            self.clipboard.set_text(text)?;
        }
        Ok(())
    }
}

/// Keeps the native clipboard in step with a session's other side: `poll`
/// returns what was copied here, in every format the clipboard holds, and
/// `apply` puts what was copied there on this clipboard without it coming
/// back from the next `poll`.
pub struct ClipboardSync(Synchronizer<Native>);

impl ClipboardSync {
    pub fn new(send_initial: bool) -> anyhow::Result<Self> {
        let clipboard = arboard::Clipboard::new().context("native clipboard unavailable")?;
        Ok(Self(Synchronizer::new(Native { clipboard }, send_initial)))
    }

    /// Read plain text explicitly, independently of clipboard synchronization.
    pub fn text(&mut self) -> anyhow::Result<String> {
        self.0
            .backend
            .clipboard
            .get_text()
            .context("clipboard does not contain text")
    }

    pub fn poll(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        self.0.poll()
    }

    pub fn apply(&mut self, content: ClipboardContent) -> anyhow::Result<()> {
        self.0.apply(content)
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

#[cfg(test)]
mod tests;
