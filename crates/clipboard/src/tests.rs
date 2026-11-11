use super::*;
use meshrmm_protocol::{LeftOut, MAX_CLIPBOARD_BYTES};

fn image(width: u32, height: u32) -> ClipboardImage {
    ClipboardImage {
        width,
        height,
        rgba: vec![200; width as usize * height as usize * 4],
    }
}

const IMG: &str = r#"<img src="blob:https://example.com/7c1e" alt="error">"#;

fn copied(
    text: Option<&str>,
    html: Option<&str>,
    image: Option<ClipboardImage>,
) -> ClipboardContent {
    ClipboardContent {
        text: text.map(Into::into),
        html: html.map(Into::into),
        image,
        left_out: None,
    }
}

/// A clipboard in memory that changes its sequence on every copy and can
/// rewrite HTML it is given, as macOS wraps it in a document.
#[derive(Default)]
struct Fake {
    several: bool,
    files: bool,
    wraps_html: bool,
    sequence: u64,
    held: Option<ClipboardContent>,
    writes: Vec<(bool, bool, bool, bool)>,
}

impl Fake {
    fn new(several: bool) -> Self {
        Self {
            several,
            ..Self::default()
        }
    }
    fn copy(&mut self, content: ClipboardContent) {
        self.held = Some(content);
        self.sequence += 1;
    }
}

impl Backend for Fake {
    fn holds_several_formats(&self) -> bool {
        self.several
    }
    fn sequence(&mut self) -> u64 {
        self.sequence
    }
    fn has_files(&mut self) -> bool {
        self.files
    }
    fn read(&mut self) -> anyhow::Result<Option<ClipboardContent>> {
        Ok(self
            .held
            .clone()
            .and_then(|held| content(held.text, held.html, held.image)))
    }
    fn write(&mut self, formats: Formats<'_>) -> anyhow::Result<()> {
        self.writes.push((
            formats.text.is_some(),
            formats.html.is_some(),
            formats.image.is_some(),
            formats.image_first,
        ));
        let html = formats.html.map(|html| {
            if self.wraps_html {
                format!("<html><body>{html}</body></html>")
            } else {
                html.into()
            }
        });
        self.copy(ClipboardContent {
            text: formats.text.map(Into::into),
            html,
            image: formats.image.cloned(),
            left_out: None,
        });
        Ok(())
    }
}

fn polled(fake: Fake) -> (Synchronizer<Fake>, Option<ClipboardContent>) {
    let mut sync = Synchronizer::new(fake, true);
    let content = sync.poll().unwrap();
    (sync, content)
}

/// "Copy image" leaves HTML, an image and often text: all of them are sent.
#[test]
fn a_copied_picture_sends_its_image_html_and_text_together() {
    let mut fake = Fake::new(true);
    fake.copy(copied(Some("error"), Some(IMG), Some(image(4, 3))));
    let (_, content) = polled(fake);
    assert_eq!(
        content,
        Some(copied(Some("error"), Some(IMG), Some(image(4, 3))))
    );
}

#[test]
fn the_formats_a_clipboard_holds_become_the_content() {
    assert_eq!(
        content(Some("a".into()), None, None),
        Some(ClipboardContent::from("a"))
    );
    assert_eq!(
        content(Some("a".into()), Some("<b>a</b>".into()), None),
        Some(copied(Some("a"), Some("<b>a</b>"), None))
    );
    assert_eq!(
        content(None, Some("<b>a</b>".into()), None),
        Some(copied(None, Some("<b>a</b>"), None))
    );
    assert_eq!(
        content(None, None, Some(image(1, 1))),
        Some(copied(None, None, Some(image(1, 1))))
    );
    assert_eq!(
        content(Some(String::new()), Some(String::new()), Some(image(1, 1))),
        Some(copied(None, None, Some(image(1, 1)))),
        "empty text and HTML are no format"
    );
    assert_eq!(content(Some(String::new()), None, None), None);
    assert_eq!(content(None, None, None), None);
}

#[test]
fn a_clipboard_of_several_formats_gets_every_one_and_the_picture_first() {
    let picture = copied(Some("error"), Some(IMG), Some(image(1, 1)));
    let set = formats(&picture, true);
    assert_eq!(set.text, Some("error"));
    assert_eq!(set.html, Some(IMG));
    assert_eq!(set.image, picture.image.as_ref());
    assert!(set.image_first);

    let rich = copied(
        Some("Q3"),
        Some("<p>Q3 <img src=chart.png></p>"),
        Some(image(1, 1)),
    );
    let set = formats(&rich, true);
    assert!(set.text.is_some() && set.html.is_some() && set.image.is_some());
    assert!(!set.image_first, "rich text goes before its rendering");
}

/// Where one update holds one format, an image whose HTML only shows it
/// wins; HTML with words of its own keeps them.
#[test]
fn a_clipboard_of_one_format_prefers_the_image_over_html_that_only_wraps_it() {
    let picture = copied(Some("error"), Some(IMG), Some(image(1, 1)));
    let set = formats(&picture, false);
    assert_eq!((set.text, set.html), (None, None));
    assert_eq!(set.image, picture.image.as_ref());

    let rich = copied(
        Some("Q3"),
        Some("<p>Q3 <img src=chart.png></p>"),
        Some(image(1, 1)),
    );
    let set = formats(&rich, false);
    assert_eq!(
        (set.text, set.html, set.image),
        (Some("Q3"), rich.html.as_deref(), None)
    );

    let image_and_text = copied(Some("C:\\shot.png"), None, Some(image(1, 1)));
    let set = formats(&image_and_text, false);
    assert_eq!((set.text, set.image.is_some()), (None, true));
}

#[test]
fn html_alone_image_alone_and_text_alone_are_set_as_before() {
    for several in [true, false] {
        let html = copied(Some("hi"), Some("<b>hi</b>"), None);
        assert_eq!(
            formats(&html, several),
            Formats {
                text: Some("hi"),
                html: Some("<b>hi</b>"),
                image: None,
                image_first: false
            }
        );
        let picture = copied(None, None, Some(image(2, 2)));
        assert_eq!(
            formats(&picture, several),
            Formats {
                text: None,
                html: None,
                image: picture.image.as_ref(),
                image_first: true
            }
        );
        let text = ClipboardContent::from("hi");
        assert_eq!(
            formats(&text, several),
            Formats {
                text: Some("hi"),
                html: None,
                image: None,
                image_first: false
            }
        );
    }
}

#[test]
fn html_text_of_its_own_is_what_it_shows() {
    for wrapper in [
        IMG,
        "<!--StartFragment--><img src=\"https://example.com/a.png\"><!--EndFragment-->",
        "<html><head><meta charset=\"utf-8\"><title>Shot</title><style>p { x }</style></head><body><img src=a.png></body></html>",
        "<meta http-equiv=\"content-type\" content=\"text/html; charset=utf-8\"><img src=\"a.png\">",
        "<div>&nbsp;<IMG SRC=a.png>\r\n </div>",
        "",
    ] {
        assert!(!has_text_of_its_own(wrapper), "{wrapper}");
    }
    for text in [
        "<p>Hello</p>",
        "<img src=a.png><figcaption>Figure 1</figcaption>",
        "<b>é</b>",
        "plain",
        "<script>x</script>shown",
        "<span>&amp;</span>",
    ] {
        assert!(has_text_of_its_own(text), "{text}");
    }
}

#[test]
fn fingerprints_match_the_same_copy_and_differ_in_any_part() {
    let base = copied(Some("t"), Some("<b>t</b>"), Some(image(2, 1)));
    assert_eq!(fingerprint(&base), fingerprint(&base.clone()));
    let mut pixel = base.clone();
    pixel.image.as_mut().unwrap().rgba[5] = 0;
    let mut shape = base.clone();
    shape.image = Some(ClipboardImage {
        width: 1,
        height: 2,
        rgba: base.image.clone().unwrap().rgba,
    });
    let variants = [
        ClipboardContent {
            text: None,
            ..base.clone()
        },
        ClipboardContent {
            text: Some("u".into()),
            ..base.clone()
        },
        ClipboardContent {
            html: None,
            ..base.clone()
        },
        ClipboardContent {
            html: Some("<i>t</i>".into()),
            ..base.clone()
        },
        ClipboardContent {
            image: None,
            ..base.clone()
        },
        pixel,
        shape,
        ClipboardContent {
            left_out: Some(LeftOut {
                bytes: MAX_CLIPBOARD_BYTES as u64 + 1,
                image: None,
            }),
            ..base.clone()
        },
    ];
    for (index, variant) in variants.iter().enumerate() {
        assert_ne!(fingerprint(&base), fingerprint(variant), "variant {index}");
    }
}

/// What `apply` sets is what the next `poll` reads, whether the clipboard
/// holds every format or one, and when it rewrites the HTML: nothing comes
/// back, and the next real copy still goes.
#[test]
fn applying_a_copy_and_polling_it_sends_nothing_back() {
    for (several, wraps_html) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut fake = Fake::new(several);
        fake.wraps_html = wraps_html;
        let mut sync = Synchronizer::new(fake, false);
        assert_eq!(sync.poll().unwrap(), None);
        for content in [
            copied(Some("error"), Some(IMG), Some(image(4, 3))),
            copied(Some("Q3"), Some("<p>Q3</p>"), Some(image(2, 2))),
            copied(Some("hi"), Some("<b>hi</b>"), None),
            copied(None, None, Some(image(1, 1))),
            ClipboardContent::from("hi"),
        ] {
            sync.apply(content.clone()).unwrap();
            assert_eq!(
                sync.poll().unwrap(),
                None,
                "{several} {wraps_html} {content:?}"
            );
            assert_eq!(sync.poll().unwrap(), None);
        }
        sync.backend.copy(ClipboardContent::from("local"));
        assert_eq!(sync.poll().unwrap(), Some(ClipboardContent::from("local")));
    }
}

#[test]
fn applying_what_the_clipboard_holds_leaves_it_alone() {
    let mut fake = Fake::new(true);
    let picture = copied(Some("error"), Some(IMG), Some(image(1, 1)));
    fake.copy(picture.clone());
    let mut sync = Synchronizer::new(fake, false);
    sync.apply(picture).unwrap();
    assert!(sync.backend.writes.is_empty());
    sync.apply(ClipboardContent::from("new")).unwrap();
    assert_eq!(sync.backend.writes, [(true, false, false, false)]);
}

#[test]
fn copied_files_are_not_sent_here() {
    let mut fake = Fake::new(true);
    fake.files = true;
    fake.copy(ClipboardContent::from("C:\\report.docx"));
    assert_eq!(polled(fake).1, None);
}

/// Too large a copy sends what fits and what it left out, so the other side
/// can tell its user; a note alone does not touch that side's clipboard.
#[test]
fn too_large_a_copy_sends_what_fits_and_says_what_it_left_out() {
    let big = || image(5120, 2880);
    let mut fake = Fake::new(true);
    fake.copy(copied(Some("Q3"), Some("<p>Q3</p>"), Some(big())));
    let sent = polled(fake).1.unwrap();
    assert_eq!(sent.html.as_deref(), Some("<p>Q3</p>"));
    assert_eq!(sent.image, None);
    assert_eq!(sent.left_out.unwrap().image, Some((5120, 2880)));

    let mut fake = Fake::new(true);
    fake.copy(copied(None, None, Some(big())));
    let sent = polled(fake).1.unwrap();
    assert!(!sent.has_formats());
    assert!(sent.left_out_problem().is_some());

    let mut fake = Fake::new(true);
    fake.copy(ClipboardContent::from("kept"));
    let mut sync = Synchronizer::new(fake, false);
    sync.apply(sent).unwrap();
    assert!(sync.backend.writes.is_empty());
    assert_eq!(sync.backend.held, Some(ClipboardContent::from("kept")));
}

#[cfg(windows)]
#[test]
fn native_text_html_and_image_clipboards_round_trip_without_html_errors() {
    let mut sender = ClipboardSync::new(false).unwrap();
    let samples = [
        ClipboardContent::from("MeshRMM clipboard isolation test"),
        copied(Some("MeshRMM test"), Some("<b>MeshRMM test</b>"), None),
        copied(
            None,
            None,
            Some(ClipboardImage {
                width: 1,
                height: 1,
                rgba: vec![10, 20, 30, 255],
            }),
        ),
        copied(
            Some("MeshRMM test"),
            Some("<img src=\"https://example.invalid/meshrmm.png\">"),
            Some(ClipboardImage {
                width: 2,
                height: 1,
                rgba: vec![10, 20, 30, 255, 40, 50, 60, 128],
            }),
        ),
    ];
    for sample in samples {
        sender.apply(sample.clone()).unwrap();
        let mut receiver = ClipboardSync::new(true).unwrap();
        let received = receiver.poll().unwrap().unwrap();
        assert_eq!(received, sample);
        assert!(
            sender.poll().unwrap().is_none(),
            "applying a remote clipboard must not echo it back"
        );
    }
}
