//! Compose the real Windows cursor onto a GPU-owned copy of the captured desktop.
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGISurface1;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CAPTUREBLT, DeleteObject, GetDC, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CURSORINFO_FLAGS, DI_NORMAL, DrawIconEx, GetCursorInfo,
    GetIconInfo, HICON, ICONINFO,
};
use windows::core::Interface;

use crate::Error;

/// Whether Windows is drawing its pointer, which is the only time Desktop
/// Duplication can bake one into the image it reports without a separate
/// pointer (a window drag, pointer trails, the Magnifier). Hidden is
/// reported as flags 0: while typing, after `ShowCursor(FALSE)` or
/// `SetCursor(NULL)`, as full-screen video does.
pub(crate) fn system_pointer_drawn() -> bool {
    let mut cursor = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // Safety: cursor is a valid CURSORINFO with cbSize set.
    let flags = unsafe { GetCursorInfo(&mut cursor) }
        .ok()
        .map(|()| cursor.flags);
    pointer_drawn(flags)
}

/// Only a pointer Windows reports as hidden counts as not drawn. A pointer
/// suppressed for touch or pen input, or an unreadable state, might be.
fn pointer_drawn(flags: Option<CURSORINFO_FLAGS>) -> bool {
    flags != Some(CURSORINFO_FLAGS(0))
}

/// Whether to replace the duplicated image with a GDI copy, which never
/// contains the system pointer, so a viewer that draws its own pointer does
/// not see a second one baked in. With no separate DXGI pointer and no
/// system pointer drawn, there is nothing to remove.
pub(crate) fn needs_cursor_free_copy(
    capture_cursor: bool,
    separate_cursor_visible: bool,
    system_pointer_drawn: bool,
) -> bool {
    !capture_cursor && !separate_cursor_visible && system_pointer_drawn
}

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

    /// `system_pointer_drawn` is whether Windows drew its pointer at any
    /// point while the cached desktop could have been composed.
    pub fn compose(
        &self,
        context: &ID3D11DeviceContext,
        capture_cursor: bool,
        separate_cursor_visible: bool,
        system_pointer_drawn: bool,
    ) -> Result<&ID3D11Texture2D, Error> {
        // DXGI may bake the pointer into the desktop during window dragging.
        // Skipping DrawIconEx cannot hide that pointer. GDI desktop capture
        // excludes the system cursor, so refresh this surface from the desktop
        // when a drawn cursor could be embedded in the duplicated image. The
        // copy costs a full-screen GDI read per frame, so a merely hidden
        // pointer uses the duplicated image.
        let refresh_desktop = needs_cursor_free_copy(
            capture_cursor,
            separate_cursor_visible,
            system_pointer_drawn,
        );
        let draw_pointer = capture_cursor && separate_cursor_visible;
        if !refresh_desktop && !draw_pointer {
            return Ok(&self.desktop);
        }
        unsafe {
            // The GDI copy overwrites the whole surface.
            if !refresh_desktop {
                context.CopyResource(&self.texture, &self.desktop);
            }
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

    /// What DXGI and `GetCursorInfo` reported in each state on a real desktop
    /// (Windows 11 26200, NVIDIA RTX 3080), and whether a pointer was baked
    /// into the duplicated image there.
    #[test]
    fn only_a_drawn_pointer_without_a_separate_one_needs_the_cursor_free_copy() {
        const HIDDEN: Option<CURSORINFO_FLAGS> = Some(CURSORINFO_FLAGS(0));
        const SHOWING: Option<CURSORINFO_FLAGS> = Some(CURSOR_SHOWING);
        // (state, DXGI pointer visible, GetCursorInfo flags, baked in, copy)
        let states = [
            ("visible pointer", true, SHOWING, false, false),
            ("hidden while typing", false, HIDDEN, false, false),
            ("hidden by ShowCursor(FALSE)", false, HIDDEN, false, false),
            ("hidden by SetCursor(NULL)", false, HIDDEN, false, false),
            ("window drag", false, SHOWING, true, true),
            ("pointer trails", false, SHOWING, true, true),
            ("Magnifier", false, SHOWING, true, true),
            // DXGI reports no pointer for an output the pointer is not on.
            // Nothing is baked in, but this check cannot tell it from a drag.
            ("pointer on another output", false, SHOWING, false, true),
            // Neither observed: assumed drawn, and possibly baked in.
            (
                "suppressed for touch or pen",
                false,
                Some(CURSORINFO_FLAGS(2)),
                true,
                true,
            ),
            ("GetCursorInfo failed", false, None, true, true),
        ];
        for (state, separate, flags, baked, copy) in states {
            let drawn = pointer_drawn(flags);
            assert_eq!(
                needs_cursor_free_copy(false, separate, drawn),
                copy,
                "{state}"
            );
            assert!(
                copy || !baked,
                "{state}: a baked pointer must be copied away"
            );
            // A viewer that wants the pointer never loses it to the copy.
            assert!(!needs_cursor_free_copy(true, separate, drawn), "{state}");
        }
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop and D3D11 device"]
    fn a_drawn_pointer_is_replaced_by_gdi_pixels_and_a_hidden_one_is_not() {
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
            // return to the normal separate-pointer capture path. A hidden
            // system pointer cannot be baked in, so the DXGI pixels stay.
            for (capture, separate, drawn, expected_pixel) in [
                (true, false, true, &marker[..3]),
                (false, false, true, &expected[..]),
                (false, false, false, &marker[..3]),
                (false, true, true, &marker[..3]),
                (false, false, true, &expected[..]),
                (false, false, false, &marker[..3]),
                (true, false, true, &marker[..3]),
            ] {
                let output = compositor
                    .compose(&context, capture, separate, drawn)
                    .unwrap();
                context.CopyResource(&staging, output);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                let actual = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), 3).to_vec();
                context.Unmap(&staging, 0);
                assert_eq!(
                    actual, expected_pixel,
                    "capture={capture}, separate={separate}, drawn={drawn}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop with nothing above a new topmost window \
                at the pointer; nudges the pointer one pixel and puts it back"]
    fn a_pointer_an_application_hides_reads_as_not_drawn() {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::w;

        // Mouse input reaches the window, and with it the thread's cursor
        // state, only when the pointer moves.
        let settle = |at: POINT| unsafe {
            SetCursorPos(at.x + 1, at.y).unwrap();
            SetCursorPos(at.x, at.y).unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
            let mut message = MSG::default();
            while std::time::Instant::now() < until {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        unsafe {
            let mut at = POINT::default();
            GetCursorPos(&mut at).unwrap();
            // A STATIC window shows the arrow on this thread, which owns the
            // ShowCursor count Windows applies while the pointer is over it.
            let window = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                None,
                // SS_NOTIFY: a STATIC window is otherwise transparent to the mouse.
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(0x100),
                at.x - 50,
                at.y - 50,
                100,
                100,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            settle(at);
            let covered = GetAncestor(WindowFromPoint(at), GA_ROOT) != window;
            let mut drawn = Vec::new();
            if !covered {
                drawn.push(system_pointer_drawn());
                ShowCursor(false);
                settle(at);
                drawn.push(system_pointer_drawn());
                ShowCursor(true);
                settle(at);
                drawn.push(system_pointer_drawn());
            }
            DestroyWindow(window).unwrap();
            assert!(!covered, "another window covers the pointer");
            assert_eq!(drawn, [true, false, true]);
        }
    }
}
