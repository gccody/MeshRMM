//! Several formats in one Windows clipboard update. arboard empties the
//! clipboard in each of its setters, so this opens, empties and fills the
//! clipboard once, with the formats arboard would set for each: Unicode text,
//! "HTML Format", and an image as "PNG" and `CF_DIBV5`, from which Windows
//! makes `CF_DIB` and `CF_BITMAP` for the programs that ask for those.

use crate::Formats;
use anyhow::{Context, anyhow};
use clipboard_win::{formats::CF_DIBV5, options::NoClear, raw};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use meshrmm_protocol::ClipboardImage;

pub(crate) fn write(formats: Formats<'_>) -> anyhow::Result<()> {
    let html_format =
        clipboard_win::register_format("HTML Format").context("no HTML clipboard format")?;
    let png_format = clipboard_win::register_format("PNG").context("no PNG clipboard format")?;
    // Encode first: other programs wait for the clipboard while it is open.
    let image = formats
        .image
        .map(|image| anyhow::Ok((png(image)?, dib_v5(image))))
        .transpose()?;
    let _open = open()?;
    raw::empty().map_err(|error| anyhow!("could not empty the clipboard: {error}"))?;
    let set_image = || -> anyhow::Result<()> {
        if let Some((png, dib)) = &image {
            raw::set_without_clear(png_format.get(), png).map_err(|error| {
                anyhow!("could not put the PNG image on the clipboard: {error}")
            })?;
            raw::set_without_clear(CF_DIBV5, dib)
                .map_err(|error| anyhow!("could not put the image on the clipboard: {error}"))?;
        }
        Ok(())
    };
    if formats.image_first {
        set_image()?;
    }
    if let Some(html) = formats.html {
        raw::set_html(html_format.get(), html)
            .map_err(|error| anyhow!("could not put the HTML on the clipboard: {error}"))?;
    }
    if let Some(text) = formats.text {
        raw::set_string_with(text, NoClear)
            .map_err(|error| anyhow!("could not put the text on the clipboard: {error}"))?;
    }
    if !formats.image_first {
        set_image()?;
    }
    Ok(())
}

/// Opens the clipboard, retrying briefly as arboard does: other programs hold
/// it open for moments at a time.
fn open() -> anyhow::Result<clipboard_win::Clipboard> {
    for _ in 0..5 {
        if let Ok(open) = clipboard_win::Clipboard::new() {
            return Ok(open);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    clipboard_win::Clipboard::new().map_err(|error| anyhow!("the clipboard is busy: {error}"))
}

fn png(image: &ClipboardImage) -> anyhow::Result<Vec<u8>> {
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(
            &image.rgba,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        )
        .context("could not encode the image as PNG")?;
    Ok(png)
}

/// A `BITMAPV5HEADER` and the pixels as 32-bit BGRA with alpha, rows bottom to
/// top: Word and WordPad cannot paste a top-down bitmap.
fn dib_v5(image: &ClipboardImage) -> Vec<u8> {
    const HEADER_BYTES: u32 = 124;
    const BI_BITFIELDS: u32 = 3;
    const LCS_SRGB: u32 = 0x7352_4742;
    const LCS_GM_IMAGES: u32 = 4;
    let mut dib = Vec::with_capacity(HEADER_BYTES as usize + image.rgba.len());
    dib.extend(HEADER_BYTES.to_le_bytes());
    // Valid content keeps both dimensions far below i32::MAX.
    dib.extend((image.width as i32).to_le_bytes());
    dib.extend((image.height as i32).to_le_bytes());
    dib.extend(1u16.to_le_bytes()); // planes
    dib.extend(32u16.to_le_bytes()); // bits per pixel
    dib.extend(BI_BITFIELDS.to_le_bytes());
    dib.extend((image.rgba.len() as u32).to_le_bytes());
    dib.extend([0; 16]); // resolution and palette: unused
    for mask in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0xff00_0000] {
        dib.extend(mask.to_le_bytes());
    }
    dib.extend(LCS_SRGB.to_le_bytes());
    dib.extend([0; 48]); // endpoints and gamma: unused for sRGB
    dib.extend(LCS_GM_IMAGES.to_le_bytes());
    dib.extend([0; 12]); // profile and reserved
    debug_assert_eq!(dib.len(), HEADER_BYTES as usize);
    for row in image.rgba.chunks_exact(image.width as usize * 4).rev() {
        for pixel in row.chunks_exact(4) {
            dib.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    dib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dib_holds_bgra_rows_bottom_to_top_after_its_header() {
        let image = ClipboardImage {
            width: 2,
            height: 2,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        };
        let dib = dib_v5(&image);
        assert_eq!(&dib[..4], &124u32.to_le_bytes());
        assert_eq!(&dib[4..8], &2i32.to_le_bytes());
        assert_eq!(&dib[8..12], &2i32.to_le_bytes());
        assert_eq!(&dib[20..24], &16u32.to_le_bytes());
        assert_eq!(
            &dib[124..],
            &[11, 10, 9, 12, 15, 14, 13, 16, 3, 2, 1, 4, 7, 6, 5, 8]
        );
    }
}
