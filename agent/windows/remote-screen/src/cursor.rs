//! Compose the real Windows cursor onto a GPU-owned copy of the captured desktop.
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGISurface1;
use windows::Win32::Graphics::Gdi::{DeleteObject, HDC, HGDIOBJ};
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
        Ok(Self {
            texture,
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

    pub fn compose(
        &self,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
    ) -> Result<&ID3D11Texture2D, Error> {
        // Never paint into DXGI's borrowed texture: every frame starts with a clean
        // desktop, so moving or hiding the cursor cannot leave trails behind.
        unsafe {
            context.CopySubresourceRegion(&self.texture, 0, 0, 0, 0, source, 0, Some(&self.region));
            let dc = self.surface.GetDC(false)?;
            let drawn = draw_cursor(dc, self.origin.0, self.origin.1);
            // Release even when drawing failed, before any further D3D work.
            let released = self.surface.ReleaseDC(None);
            drawn?;
            released?;
        }
        Ok(&self.texture)
    }
}
