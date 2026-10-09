use std::sync::OnceLock;

use super::*;
use crate::toolbar::{self, Paint, Segment};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DrawTextW, EndPaint, FillRect, HDC,
    PAINTSTRUCT, SRCCOPY, SelectObject, SetBkMode, TRANSPARENT,
};
use windows::Win32::Graphics::GdiPlus::{
    DashCapRound, FillModeWinding, GdipAddPathBezier, GdipAddPathLine, GdipClosePathFigure,
    GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush,
    GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDrawPath, GdipFillPath,
    GdipSetPenLineCap197819, GdipSetPenLineJoin, GdipSetPixelOffsetMode, GdipSetSmoothingMode,
    GdipStartPathFigure, GdiplusStartup, GdiplusStartupInput, GpGraphics, GpPath, LineCapRound,
    LineJoinRound, PixelOffsetModeHalf, SmoothingModeAntiAlias, UnitPixel,
};

impl WindowContext {
    /// Paints the toolbar into a memory bitmap, then onto the window.
    pub(super) unsafe fn paint_toolbar(&self, toolbar_window: HWND) {
        let mut paint = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(toolbar_window, &mut paint) };
        let (width, height) = unsafe { client_size(toolbar_window) }.unwrap_or_default();
        let (width, height) = (width as i32, height as i32);
        if width > 0 && height > 0 {
            unsafe {
                let memory = CreateCompatibleDC(Some(dc));
                let bitmap = CreateCompatibleBitmap(dc, width, height);
                let old_bitmap = SelectObject(memory, HGDIOBJ(bitmap.0));
                self.draw_toolbar(memory, width);
                let _ = BitBlt(dc, 0, 0, width, height, Some(memory), 0, 0, SRCCOPY);
                SelectObject(memory, old_bitmap);
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(memory);
            }
        }
        let _ = unsafe { EndPaint(toolbar_window, &paint) };
    }

    unsafe fn draw_toolbar(&self, dc: HDC, width: i32) {
        let scale = self.scale_factor();
        let height = toolbar_height(self.dpi.get());
        unsafe {
            fill_rect(
                dc,
                RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                },
                toolbar::BACKGROUND,
            );
            let border = (scale.round() as i32).max(1);
            fill_rect(
                dc,
                RECT {
                    left: 0,
                    top: height - border,
                    right: width,
                    bottom: height,
                },
                toolbar::BORDER,
            );
        }
        let model = self.toolbar.borrow();
        for x in &model.layout.separators {
            let left = ((x - 0.5) * scale).round() as i32;
            let line = RECT {
                left,
                top: (12.0 * scale).round() as i32,
                right: left + (scale.round() as i32).max(1),
                bottom: (28.0 * scale).round() as i32,
            };
            unsafe { fill_rect(dc, line, toolbar::SEPARATOR) };
        }
        let old_font = unsafe { SelectObject(dc, HGDIOBJ(self.toolbar_font.get().0)) };
        let scaled = |rect: Rect| {
            Rect::new(
                rect.x * scale,
                rect.y * scale,
                rect.width * scale,
                rect.height * scale,
            )
        };
        let mut labels = Vec::new();
        let graphics = unsafe { Graphics::new(dc) };
        for (index, (item, rect)) in model.items.iter().zip(&model.layout.rects).enumerate() {
            let style = toolbar::style(
                item,
                model.hovered == Some(index),
                model.pressed == Some(index),
            );
            let text = model.layout.labels[index]
                .as_deref()
                .map(|label| (label, self.measure(dc, label)));
            let parts = toolbar::parts(item, *rect, text.map_or(0.0, |(_, width)| width));
            if let (Some((label, _)), Some(area)) = (text, parts.label) {
                labels.push((label.to_owned(), area, style.label));
            }
            let Some(graphics) = graphics.as_ref() else {
                continue;
            };
            if let Some(background) = style.background {
                graphics.paint(
                    &toolbar::rounded_rect(scaled(*rect), style.radius * scale),
                    Paint::Fill,
                    background,
                );
            }
            for shape in toolbar::icon(item.icon) {
                let (segments, paint) = toolbar::place(&shape, scaled(parts.icon));
                graphics.paint(&segments, paint, style.foreground);
            }
            if let Some(chevron) = parts.chevron {
                for shape in toolbar::icon(toolbar::Icon::Chevron) {
                    let (segments, paint) = toolbar::place(&shape, scaled(chevron));
                    graphics.paint(&segments, paint, style.foreground);
                }
            }
            if let (Some((x, y)), Some(color)) = (parts.badge, style.badge) {
                let (x, y) = (x * scale, y * scale);
                let ring = (style.badge_radius + 1.5) * scale;
                graphics.paint(&toolbar::circle(x, y, ring), Paint::Fill, style.badge_ring);
                graphics.paint(
                    &toolbar::circle(x, y, style.badge_radius * scale),
                    Paint::Fill,
                    color,
                );
            }
        }
        // GDI draws on the bitmap once GDI+ has finished with it.
        drop(graphics);
        unsafe { SetBkMode(dc, TRANSPARENT) };
        for (label, area, color) in labels {
            let mut text: Vec<u16> = label.encode_utf16().collect();
            let mut area = self.device_rect(area);
            // Rounding must not clip the last glyph.
            area.right += 2;
            unsafe {
                SetTextColor(dc, colorref(color));
                DrawTextW(
                    dc,
                    &mut text,
                    &mut area,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
        }
        unsafe { SelectObject(dc, old_font) };
    }
}

fn colorref(color: toolbar::Color) -> COLORREF {
    let toolbar::Color(red, green, blue) = color;
    COLORREF(u32::from(red) | u32::from(green) << 8 | u32::from(blue) << 16)
}

fn argb(color: toolbar::Color) -> u32 {
    let toolbar::Color(red, green, blue) = color;
    0xff00_0000 | u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
}

unsafe fn fill_rect(dc: HDC, rect: RECT, color: toolbar::Color) {
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

/// Starts GDI+ for the process once. It is never shut down: the viewer's
/// windows use it until the process exits.
fn gdiplus_started() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token = 0;
        let status = unsafe { GdiplusStartup(&mut token, &input, ptr::null_mut()) };
        if status.0 != 0 {
            tracing::warn!(
                status = status.0,
                "GDI+ is unavailable; toolbar icons are not drawn"
            );
        }
        status.0 == 0
    })
}

/// Anti-aliased GDI+ drawing on a device context.
struct Graphics(*mut GpGraphics);

impl Graphics {
    unsafe fn new(dc: HDC) -> Option<Self> {
        if !gdiplus_started() {
            return None;
        }
        let mut graphics = ptr::null_mut();
        if unsafe { GdipCreateFromHDC(dc, &mut graphics) }.0 != 0 || graphics.is_null() {
            return None;
        }
        unsafe {
            GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
            GdipSetPixelOffsetMode(graphics, PixelOffsetModeHalf);
        }
        Some(Self(graphics))
    }

    fn paint(&self, segments: &[Segment], paint: Paint, color: toolbar::Color) {
        unsafe {
            let path = path(segments);
            if path.is_null() {
                return;
            }
            match paint {
                Paint::Fill => {
                    let mut brush = ptr::null_mut();
                    if GdipCreateSolidFill(argb(color), &mut brush).0 == 0 {
                        GdipFillPath(self.0, brush.cast(), path);
                        GdipDeleteBrush(brush.cast());
                    }
                }
                Paint::Stroke(width) => {
                    let mut pen = ptr::null_mut();
                    if GdipCreatePen1(argb(color), width as f32, UnitPixel, &mut pen).0 == 0 {
                        GdipSetPenLineCap197819(pen, LineCapRound, LineCapRound, DashCapRound);
                        GdipSetPenLineJoin(pen, LineJoinRound);
                        GdipDrawPath(self.0, pen, path);
                        GdipDeletePen(pen);
                    }
                }
            }
            GdipDeletePath(path);
        }
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.0) };
    }
}

/// A GDI+ path of `segments`, which the caller deletes.
unsafe fn path(segments: &[Segment]) -> *mut GpPath {
    let mut path = ptr::null_mut();
    if unsafe { GdipCreatePath(FillModeWinding, &mut path) }.0 != 0 {
        return ptr::null_mut();
    }
    let mut current = (0.0_f32, 0.0_f32);
    for segment in segments {
        unsafe {
            match *segment {
                Segment::Move(x, y) => {
                    GdipStartPathFigure(path);
                    current = (x as f32, y as f32);
                }
                Segment::Line(x, y) => {
                    let next = (x as f32, y as f32);
                    GdipAddPathLine(path, current.0, current.1, next.0, next.1);
                    current = next;
                }
                Segment::Cubic(ax, ay, bx, by, x, y) => {
                    let next = (x as f32, y as f32);
                    GdipAddPathBezier(
                        path, current.0, current.1, ax as f32, ay as f32, bx as f32, by as f32,
                        next.0, next.1,
                    );
                    current = next;
                }
                Segment::Close => {
                    GdipClosePathFigure(path);
                }
            }
        }
    }
    path
}
