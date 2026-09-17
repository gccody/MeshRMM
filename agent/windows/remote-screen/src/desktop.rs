//! Desktop-region capture uses GDI so rotated monitors and displays on different GPUs
//! share one physical desktop coordinate space. Encoding remains hardware based.
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::*;

use crate::{ALL_MONITORS_ID, DisplayInfo, Error, enumerate_displays};

pub(crate) struct DesktopCapture {
    background: Option<crate::background::Desktop>,
    displays: Vec<DisplayInfo>,
    bounds: DisplayInfo,
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut c_void,
    texture: ID3D11Texture2D,
    next_frame: Instant,
    next_layout_check: Instant,
    interval: Duration,
}

impl DesktopCapture {
    pub fn new(device: &ID3D11Device, fps: u32, display_id: u32) -> Result<Self, Error> {
        let background = (display_id == crate::background::DISPLAY_ID)
            .then(crate::background::Desktop::bind)
            .transpose()?;
        let displays = if background.is_some() {
            vec![crate::background::display()]
        } else {
            enumerate_displays()?
        };
        let mut bounds = displays
            .iter()
            .find(|d| d.id == display_id)
            .cloned()
            .ok_or(Error::InvalidDisplayDimensions)?;
        bounds.width = bounds
            .width
            .checked_add(1)
            .ok_or(Error::InvalidDisplayDimensions)?
            & !1;
        bounds.height = bounds
            .height
            .checked_add(1)
            .ok_or(Error::InvalidDisplayDimensions)?
            & !1;
        if bounds.width > 16384 || bounds.height > 16384 {
            return Err(Error::InvalidDisplayDimensions);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: bounds.width,
            Height: bounds.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // VideoProcessorInputView requires a video-compatible binding.
            // A shader-resource-only GDI upload texture is rejected by some GPUs.
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        }
        let texture = texture.ok_or(Error::InvalidDisplayDimensions)?;
        // Construct the owner before allocating GDI resources so every failure
        // releases the resources already acquired, on this same capture thread.
        let mut capture = Self {
            background,
            displays,
            bounds,
            screen: HDC::default(),
            memory: HDC::default(),
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            pixels: std::ptr::null_mut(),
            texture,
            next_frame: Instant::now(),
            next_layout_check: Instant::now(),
            interval: Duration::from_secs_f64(1.0 / f64::from(fps.max(1))),
        };
        unsafe {
            capture.screen = GetDC(None);
            if capture.screen.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            capture.memory = CreateCompatibleDC(Some(capture.screen));
            if capture.memory.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: capture.width() as i32,
                    biHeight: -(capture.height() as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            capture.bitmap = CreateDIBSection(
                Some(capture.screen),
                &info,
                DIB_RGB_COLORS,
                &mut capture.pixels,
                None,
                0,
            )?;
            capture.previous = SelectObject(capture.memory, HGDIOBJ(capture.bitmap.0));
            if capture.previous.is_invalid() || capture.pixels.is_null() {
                return Err(windows::core::Error::from_thread().into());
            }
        }
        Ok(capture)
    }

    pub fn width(&self) -> u32 {
        self.bounds.width
    }
    pub fn height(&self) -> u32 {
        self.bounds.height
    }

    pub fn capture(
        &mut self,
        context: &ID3D11DeviceContext,
        capture_cursor: bool,
    ) -> Result<Option<ID3D11Texture2D>, Error> {
        let now = Instant::now();
        if now < self.next_frame {
            return Ok(None);
        }
        self.next_frame = now + self.interval;
        if self.background.is_none() && now >= self.next_layout_check {
            if enumerate_displays()? != self.displays {
                return Err(Error::DesktopDuplication(
                    "desktop monitor layout changed".into(),
                ));
            }
            self.next_layout_check = now + Duration::from_secs(1);
        }
        unsafe {
            // Clear gaps and padding, then copy each physical display. GDI
            // handles rotation and adapter differences before the GPU upload.
            PatBlt(
                self.memory,
                0,
                0,
                self.width() as i32,
                self.height() as i32,
                BLACKNESS,
            )
            .ok()?;
            if self.background.is_some() {
                crate::background::paint(self.memory)?;
            }
            for display in self.displays.iter().filter(|d| {
                self.background.is_none()
                    && d.id != ALL_MONITORS_ID
                    && (self.bounds.id == ALL_MONITORS_ID || d.id == self.bounds.id)
            }) {
                BitBlt(
                    self.memory,
                    display.x - self.bounds.x,
                    display.y - self.bounds.y,
                    display.width as i32,
                    display.height as i32,
                    Some(self.screen),
                    display.x,
                    display.y,
                    SRCCOPY | CAPTUREBLT,
                )?;
            }
            if capture_cursor && self.background.is_none() {
                crate::cursor::draw_cursor(self.memory, self.bounds.x, self.bounds.y)?;
            }
            GdiFlush().ok()?;
            context.UpdateSubresource(&self.texture, 0, None, self.pixels, self.width() * 4, 0);
        }
        Ok(Some(self.texture.clone()))
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            }
            if !self.memory.is_invalid() {
                let _ = DeleteDC(self.memory);
            }
            if !self.screen.is_invalid() {
                ReleaseDC(None, self.screen);
            }
        }
    }
}
