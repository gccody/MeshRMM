//! Several formats in one pasteboard change. arboard clears the pasteboard in
//! each of its setters, so this clears it once and gives the item that starts
//! every type arboard would write for each format: the image as TIFF, the HTML
//! wrapped as arboard wraps it, and plain text.

use crate::Formats;
use anyhow::{Context, ensure};
use image::{ExtendedColorType, ImageEncoder, codecs::tiff::TiffEncoder};
use meshrmm_protocol::ClipboardImage;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeHTML, NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSString};

pub(crate) fn write(formats: Formats<'_>) -> anyhow::Result<()> {
    let tiff = formats.image.map(tiff).transpose()?;
    let written = objc2::rc::autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        // One change: the setters below add types to the item this starts.
        pasteboard.clearContents();
        let set_image = || {
            tiff.as_ref().is_none_or(|tiff| {
                pasteboard.setData_forType(Some(&*NSData::with_bytes(tiff)), unsafe {
                    NSPasteboardTypeTIFF
                })
            })
        };
        let set_html_and_text = || {
            formats.html.is_none_or(|html| {
                // Read as UTF-8 rather than Latin-1 by the programs that paste it.
                let html = format!(
                    r#"<html><head><meta http-equiv="content-type" content="text/html; charset=utf-8"></head><body>{html}</body></html>"#
                );
                pasteboard
                    .setString_forType(&NSString::from_str(&html), unsafe { NSPasteboardTypeHTML })
            }) && formats.text.is_none_or(|text| {
                pasteboard
                    .setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
            })
        };
        // Programs are offered the types in the order they were added.
        let image: &dyn Fn() -> bool = &set_image;
        let html_and_text: &dyn Fn() -> bool = &set_html_and_text;
        let [first, second] = if formats.image_first {
            [image, html_and_text]
        } else {
            [html_and_text, image]
        };
        first() && second()
    });
    ensure!(written, "the pasteboard refused the copy");
    Ok(())
}

fn tiff(image: &ClipboardImage) -> anyhow::Result<Vec<u8>> {
    let mut tiff = std::io::Cursor::new(Vec::new());
    TiffEncoder::new(&mut tiff)
        .write_image(
            &image.rgba,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        )
        .context("could not encode the image as TIFF")?;
    Ok(tiff.into_inner())
}
