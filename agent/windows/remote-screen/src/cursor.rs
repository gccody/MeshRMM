//! Compose the real Windows cursor onto a GPU-owned copy of the captured desktop.
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGISurface1;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CAPTUREBLT, DeleteObject, GetDC, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};
use windows::core::Interface;

use crate::Error;

/// Draw at the physical desktop position, accounting for the cursor's hotspot.
/// Windows handles color, alpha and monochrome AND/XOR cursor masks for us.
pub(crate) fn draw_cursor(dc: HDC, origin_x: i32, origin_y: i32) -> Result<(), Error> {
    let mut cursor = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if GetCursorInfo(&mut cursor).is_err() || cursor.flags != CURSOR_SHOWING {
            return Ok(());
        }
        let mut icon = ICONINFO::default();
        if GetIconInfo(HICON(cursor.hCursor.0), &mut icon).is_err() {
            return Ok(());
        }
        let result = DrawIconEx(
            dc,
            cursor.ptScreenPos.x - origin_x - icon.xHotspot as i32,
            cursor.ptScreenPos.y - origin_y - icon.yHotspot as i32,
            HICON(cursor.hCursor.0),
            0,
            0,
            0,
            None,
            DI_NORMAL,
        );
        if !icon.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
        }
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
        }
        result?;
    }
    Ok(())
}

pub(crate) struct CursorCompositor {
    texture: ID3D11Texture2D,
    desktop: ID3D11Texture2D,
    surface: IDXGISurface1,
    region: D3D11_BOX,
    origin: (i32, i32),
}

impl CursorCompositor {
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    ) -> Result<Self, Error> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        }
        let texture = texture.ok_or(Error::InvalidDisplayDimensions)?;
        let surface = texture.cast()?;
        let mut desktop = None;
        let desktop_desc = D3D11_TEXTURE2D_DESC {
            MiscFlags: 0,
            ..desc
        };
        unsafe {
            device.CreateTexture2D(&desktop_desc, None, Some(&mut desktop))?;
        }
        let desktop = desktop.ok_or(Error::InvalidDisplayDimensions)?;
        Ok(Self {
            texture,
            desktop,
            surface,
            region: D3D11_BOX {
                right: width,
                bottom: height,
                back: 1,
                ..Default::default()
            },
            origin: (x, y),
        })
    }

    pub fn update(&self, context: &ID3D11DeviceContext, source: &ID3D11Texture2D) {
        // Keep an owned frame after DXGI releases its borrowed surface.
        // It may contain an embedded pointer when DXGI reports no separate one.
        // Ownership may change on a static desktop without any new damage.
        unsafe {
            context.CopySubresourceRegion(&self.desktop, 0, 0, 0, 0, source, 0, Some(&self.region));
        }
    }

    unsafe fn copy_cursor_free_desktop(&self, destination: HDC) -> Result<(), Error> {
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            let copied = BitBlt(
                destination,
                0,
                0,
                self.region.right as i32,
                self.region.bottom as i32,
                Some(screen),
                self.origin.0,
                self.origin.1,
                SRCCOPY | CAPTUREBLT,
            );
            ReleaseDC(None, screen);
            copied?;
        }
        Ok(())
    }

    pub fn compose(
        &self,
        context: &ID3D11DeviceContext,
        capture_cursor: bool,
        separate_cursor_visible: bool,
    ) -> Result<&ID3D11Texture2D, Error> {
        // DXGI may bake the pointer into the desktop during window dragging.
        // Skipping DrawIconEx cannot hide that pointer. GDI desktop capture
        // excludes the system cursor, so refresh this surface from the desktop
        // when a hidden cursor could be embedded in the duplicated image.
        let refresh_desktop = !capture_cursor && !separate_cursor_visible;
        let draw_pointer = capture_cursor && separate_cursor_visible;
        if !refresh_desktop && !draw_pointer {
            return Ok(&self.desktop);
        }
        unsafe {
            context.CopyResource(&self.texture, &self.desktop);
            let dc = self.surface.GetDC(false)?;
            let drawn = if refresh_desktop {
                self.copy_cursor_free_desktop(dc)
            } else {
                draw_cursor(dc, self.origin.0, self.origin.1)
            };
            // Release even when drawing failed, before any further D3D work.
            let released = self.surface.ReleaseDC(None);
            drawn?;
            released?;
        }
        Ok(&self.texture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive Windows desktop and D3D11 device"]
    fn hidden_embedded_cursor_refreshes_pixels_instead_of_reusing_dxgi_frame() {
        use windows::Win32::Graphics::Gdi::GetPixel;

        let (device, context) = windows_capture::d3d11::create_d3d_device().unwrap();
        let compositor = CursorCompositor::new(&device, 2, 2, 0, 0).unwrap();
        unsafe {
            let screen = GetDC(None);
            assert!(!screen.is_invalid());
            let color = GetPixel(screen, 0, 0).0;
            ReleaseDC(None, screen);
            assert_ne!(color, u32::MAX);
            let expected = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
            // Simulate a cursor baked into DXGI's frame with pixels that differ
            // from the real desktop. Merely suppressing DrawIconEx retains them.
            let marker = [!expected[0], !expected[1], !expected[2], 255];
            let pixels = marker.repeat(4);
            context.UpdateSubresource(&compositor.desktop, 0, None, pixels.as_ptr().cast(), 8, 0);
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            compositor.desktop.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            // Include a visibility change with no new DXGI frame, then a
            // return to the normal separate-pointer capture path.
            for (capture, separate, expected_pixel) in [
                (true, false, &marker[..3]),
                (false, false, &expected[..]),
                (false, true, &marker[..3]),
                (false, false, &expected[..]),
                (true, false, &marker[..3]),
            ] {
                let output = compositor.compose(&context, capture, separate).unwrap();
                context.CopyResource(&staging, output);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                let actual = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), 3).to_vec();
                context.Unmap(&staging, 0);
                assert_eq!(
                    actual, expected_pixel,
                    "capture={capture}, separate={separate}"
                );
            }
        }
    }
}
