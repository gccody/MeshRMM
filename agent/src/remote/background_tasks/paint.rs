//! Drawing of the Task Manager window: header, process list, performance graphs and caption.
use super::*;

pub(super) unsafe fn padded_icon(images: HIMAGELIST, icon: HICON) -> i32 {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return -1;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 16,
                biHeight: -28,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)
        else {
            let _ = DeleteDC(dc);
            return -1;
        };
        let old = SelectObject(dc, bitmap.into());
        let mask = COLORREF(0xff00ff);
        fill(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: 16,
                bottom: 28,
            },
            mask,
        );
        let result = if DrawIconEx(dc, 0, 6, icon, 16, 16, 0, None, DI_NORMAL).is_ok() {
            // Classic Session 0 controls need an explicit color-key mask;
            // their PrintWindow path does not composite image-list alpha.
            let _ = GdiFlush();
            SelectObject(dc, old);
            ImageList_AddMasked(images, bitmap, mask)
        } else {
            -1
        };
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        result
    }
}

pub(super) unsafe fn draw_text(
    dc: HDC,
    text: &str,
    rect: RECT,
    color: COLORREF,
    flags: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let text = wide(text);
        SetTextColor(dc, color);
        SetBkMode(dc, TRANSPARENT);
        let mut rect = rect;
        DrawTextW(
            dc,
            &mut text[..text.len() - 1].to_vec(),
            &mut rect,
            flags | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}

pub(super) unsafe fn fill(dc: HDC, rect: &RECT, color: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

pub(super) unsafe fn header_paint(state: &State, hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        fill(dc, &rect, WHITE);
        SelectObject(dc, state.font.into());
        let mut x = -GetScrollPos(state.list, SB_HORZ);
        for (index, (name, _)) in Tab::Processes.columns().iter().enumerate() {
            let width =
                SendMessageW(state.list, LVM_GETCOLUMNWIDTH, Some(WPARAM(index)), None).0 as i32;
            let cell = RECT {
                left: x,
                top: 0,
                right: x + width,
                bottom: rect.bottom - 1,
            };
            if index >= 2 {
                let total = match index {
                    2 => state
                        .snapshot
                        .cpu
                        .map_or_else(|| "—".into(), |v| format!("{v:.0}%")),
                    3 => {
                        if state.snapshot.memory_total > 0 {
                            format!(
                                "{:.0}%",
                                100.0
                                    * (state.snapshot.memory_total
                                        - state.snapshot.memory_available)
                                        as f64
                                    / state.snapshot.memory_total as f64
                            )
                        } else {
                            "—".into()
                        }
                    }
                    4 => state
                        .snapshot
                        .disk
                        .map_or_else(|| "—".into(), |v| format!("{v:.0}%")),
                    5 => state
                        .snapshot
                        .network_rate
                        .filter(|_| state.snapshot.network_capacity > 0)
                        .map_or_else(
                            || "—".into(),
                            |v| {
                                format!(
                                    "{:.0}%",
                                    (v * 800.0 / state.snapshot.network_capacity as f64).min(100.0)
                                )
                            },
                        ),
                    _ => "—".into(),
                };
                SelectObject(dc, state.heading_font.into());
                draw_text(
                    dc,
                    &total,
                    RECT {
                        left: x + 5,
                        top: 5,
                        right: x + width - 7,
                        bottom: 29,
                    },
                    COLORREF(0x222222),
                    DT_RIGHT,
                );
            }
            SelectObject(dc, state.font.into());
            draw_text(
                dc,
                name,
                RECT {
                    left: x + 8,
                    top: 27,
                    right: x + width - 7,
                    bottom: rect.bottom - 3,
                },
                COLORREF(0x66594e),
                if index >= 2 { DT_RIGHT } else { DT_LEFT },
            );
            fill(
                dc,
                &RECT {
                    left: cell.right - 1,
                    top: 12,
                    right: cell.right,
                    bottom: cell.bottom,
                },
                COLORREF(0xe5e5e5),
            );
            if state.sort == index {
                draw_text(
                    dc,
                    if state.descending { "⌄" } else { "⌃" },
                    RECT {
                        left: x,
                        top: 0,
                        right: x + width,
                        bottom: 12,
                    },
                    COLORREF(0x777777),
                    DT_CENTER,
                );
            }
            x += width;
        }
        fill(
            dc,
            &RECT {
                top: rect.bottom - 1,
                ..rect
            },
            COLORREF(0xaaaaaa),
        );
    }
}
// Session 0 common controls can leave nonclient scrollbar pixels unpainted.
// Paint their actual native geometry after native list painting. Native scroll
// commands retain the list control's range, selection, keyboard and wheel behavior.

pub(super) unsafe extern "system" fn list_paint(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        let input = &*(data as *const Cell<Option<ScrollInput>>);
        if message == WM_NCDESTROY {
            let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
            let _ = RemoveWindowSubclass(hwnd, Some(list_paint), id);
            let result = DefSubclassProc(hwnd, message, wparam, lparam);
            drop(Box::from_raw(data as *mut Cell<Option<ScrollInput>>));
            return result;
        }
        if message == WM_TIMER && wparam.0 == SCROLL_REPEAT {
            if let Some(ScrollInput::Repeat { vertical, command }) = input.get() {
                scroll_command(hwnd, vertical, command, 0);
                SetTimer(Some(hwnd), SCROLL_REPEAT, 60, None);
            }
            return LRESULT(0);
        }
        if message == WM_LBUTTONUP && input.take().is_some() {
            let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
            return LRESULT(0);
        }
        if message == WM_CANCELMODE {
            input.set(None);
            let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
        }
        // The isolated desktop routes client mouse messages even for nonclient
        // scrollbar coordinates. Adapt only this list, without changing routing
        // or invoking the native modal scrollbar loop on other applications.
        if matches!(message, WM_LBUTTONDOWN | WM_MOUSEMOVE) {
            let mut point = POINT {
                x: lparam.0 as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            let _ = ClientToScreen(hwnd, &mut point);
            if message == WM_MOUSEMOVE {
                if let Some(ScrollInput::Drag {
                    vertical,
                    start,
                    position,
                    travel,
                    minimum,
                    maximum,
                }) = input.get()
                {
                    let current = if vertical { point.y } else { point.x };
                    let delta = i64::from(current - start) * i64::from(maximum - minimum)
                        / i64::from(travel.max(1));
                    let position = (i64::from(position) + delta)
                        .clamp(i64::from(minimum), i64::from(maximum))
                        as i32;
                    scroll_command(hwnd, vertical, SB_THUMBPOSITION, position);
                    return LRESULT(0);
                }
            } else {
                for (object, bar, vertical) in [
                    (OBJID_VSCROLL, SB_VERT, true),
                    (OBJID_HSCROLL, SB_HORZ, false),
                ] {
                    let mut info = SCROLLBARINFO {
                        cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
                        ..Default::default()
                    };
                    if GetScrollBarInfo(hwnd, object, &mut info).is_err()
                        || info.rgstate[0] & 0x8001 != 0
                        || !PtInRect(&info.rcScrollBar, point).as_bool()
                    {
                        continue;
                    }
                    let current = if vertical { point.y } else { point.x };
                    let start = if vertical {
                        info.rcScrollBar.top
                    } else {
                        info.rcScrollBar.left
                    };
                    let end = if vertical {
                        info.rcScrollBar.bottom
                    } else {
                        info.rcScrollBar.right
                    };
                    let command = if current < start + info.dxyLineButton {
                        Some(SB_LINEUP)
                    } else if current >= end - info.dxyLineButton {
                        Some(SB_LINEDOWN)
                    } else if current < start + info.xyThumbTop {
                        Some(SB_PAGEUP)
                    } else if current >= start + info.xyThumbBottom {
                        Some(SB_PAGEDOWN)
                    } else {
                        None
                    };
                    if let Some(command) = command {
                        input.set(Some(ScrollInput::Repeat { vertical, command }));
                        scroll_command(hwnd, vertical, command, 0);
                        SetTimer(Some(hwnd), SCROLL_REPEAT, 400, None);
                    } else {
                        let mut range = SCROLLINFO {
                            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                            fMask: SIF_ALL,
                            ..Default::default()
                        };
                        if GetScrollInfo(hwnd, bar, &mut range).is_ok() {
                            input.set(Some(ScrollInput::Drag {
                                vertical,
                                start: current,
                                position: range.nPos,
                                travel: end
                                    - start
                                    - 2 * info.dxyLineButton
                                    - (info.xyThumbBottom - info.xyThumbTop),
                                minimum: range.nMin,
                                maximum: (range.nMax - range.nPage.saturating_sub(1) as i32)
                                    .max(range.nMin),
                            }));
                        }
                    }
                    return LRESULT(0);
                }
            }
        }
        let result = DefSubclassProc(hwnd, message, wparam, lparam);
        if matches!(message, WM_PAINT | WM_NCPAINT | WM_HSCROLL | WM_VSCROLL) {
            let dc = GetWindowDC(Some(hwnd));
            if !dc.is_invalid() {
                captured_scrollbars(hwnd, dc);
                ReleaseDC(Some(hwnd), dc);
            }
        }
        result
    }
}

pub(super) unsafe fn performance_paint(state: &State, hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        fill(dc, &rect, WHITE);
        SelectObject(dc, state.font.into());
        let sidebar = 180;
        let selected = state.performance;
        let used = state
            .snapshot
            .memory_total
            .saturating_sub(state.snapshot.memory_available);
        let values = [
            percent(state.snapshot.cpu),
            format!(
                "{:.1} / {:.1} GB",
                used as f64 / 1073741824.0,
                state.snapshot.memory_total as f64 / 1073741824.0
            ),
            percent(state.snapshot.disk),
            state
                .snapshot
                .network_rate
                .map_or_else(|| "—".into(), |n| format!("{:.2} Mbps", n * 8.0 / 1e6)),
        ];
        let titles = ["CPU", "Memory", "Disk", "Ethernet / Wi-Fi"];
        for i in 0..4 {
            let top = 10 + i as i32 * 66;
            if i == selected {
                fill(
                    dc,
                    &RECT {
                        left: 6,
                        top,
                        right: sidebar - 6,
                        bottom: top + 60,
                    },
                    COLORREF(0xf2e7d9),
                );
            }
            draw_text(
                dc,
                titles[i],
                RECT {
                    left: 18,
                    top: top + 4,
                    right: sidebar - 10,
                    bottom: top + 29,
                },
                COLORREF(0x222222),
                DT_LEFT,
            );
            draw_text(
                dc,
                &values[i],
                RECT {
                    left: 18,
                    top: top + 29,
                    right: sidebar - 10,
                    bottom: top + 51,
                },
                COLORREF(0x555555),
                DT_LEFT,
            );
        }
        let left = sidebar + 20;
        let right = rect.right - 24;
        let top = 56;
        draw_text(
            dc,
            titles[selected],
            RECT {
                left,
                top: 8,
                right,
                bottom: 35,
            },
            BLUE,
            DT_LEFT,
        );
        let subtitle = [
            "% Utilization",
            "% Physical memory in use",
            "% Active time • all physical disks",
            "Throughput • hardware adapters",
        ][selected];
        draw_text(
            dc,
            subtitle,
            RECT {
                left,
                top: 33,
                right,
                bottom: 54,
            },
            COLORREF(0x777777),
            DT_LEFT,
        );
        let graph = RECT {
            left,
            top,
            right,
            bottom: (rect.bottom - 126).max(top + 60),
        };
        fill(dc, &graph, COLORREF(0xfffcf8));
        for col in 0..=12 {
            let x = left + (right - left) * col / 12;
            fill(
                dc,
                &RECT {
                    left: x,
                    top,
                    right: x + 1,
                    bottom: graph.bottom,
                },
                COLORREF(0xe8d8ca),
            );
        }
        for row in 0..=10 {
            let y = top + (graph.bottom - top) * row / 10;
            fill(
                dc,
                &RECT {
                    left,
                    top: y,
                    right,
                    bottom: y + 1,
                },
                COLORREF(0xe8d8ca),
            );
        }
        let scale = if selected == 3 {
            state.samples.iter().map(|s| s[3]).fold(125000.0, f64::max)
        } else {
            100.0
        };
        let pen = CreatePen(PS_SOLID, 2, BLUE);
        let old = SelectObject(dc, pen.into());
        let offset = 60 - state.samples.len();
        for (i, sample) in state.samples.iter().enumerate() {
            let x = left + (offset + i) as i32 * (right - left) / 59;
            let y = graph.bottom
                - 1
                - ((sample[selected] / scale).clamp(0.0, 1.0) * (graph.bottom - top - 2) as f64)
                    as i32;
            if i == 0 {
                let _ = MoveToEx(dc, x, y, None);
            } else {
                let _ = LineTo(dc, x, y);
            }
        }
        SelectObject(dc, old);
        let _ = DeleteObject(pen.into());
        draw_text(
            dc,
            &format!(
                "60 samples • {}",
                if state.interval == 0 {
                    "Paused".into()
                } else {
                    format!("{:.1} second interval", state.interval as f64 / 1000.0)
                }
            ),
            RECT {
                left,
                top: graph.bottom,
                right,
                bottom: graph.bottom + 23,
            },
            COLORREF(0x777777),
            DT_LEFT,
        );
        let summary = match selected {
            0 => vec![
                format!("Utilization    {}", values[0]),
                format!(
                    "Processes    {}     Threads    {}     Handles    {}",
                    state.snapshot.processes.len(),
                    state.snapshot.threads,
                    state.snapshot.handles
                ),
                format!(
                    "Up time    {}:{:02}:{:02}:{:02}",
                    state.snapshot.uptime / 86400,
                    state.snapshot.uptime / 3600 % 24,
                    state.snapshot.uptime / 60 % 60,
                    state.snapshot.uptime % 60
                ),
            ],
            1 => vec![
                format!(
                    "In use    {:.1} GB       Available    {:.1} GB",
                    used as f64 / 1073741824.0,
                    state.snapshot.memory_available as f64 / 1073741824.0
                ),
                format!(
                    "Committed    {:.1} / {:.1} GB",
                    state.snapshot.commit as f64 / 1073741824.0,
                    state.snapshot.commit_limit as f64 / 1073741824.0
                ),
                format!(
                    "Total physical memory    {:.1} GB",
                    state.snapshot.memory_total as f64 / 1073741824.0
                ),
            ],
            2 => vec![
                format!("Active time    {}", values[2]),
                state.snapshot.disk_rate.map_or_else(
                    || "Transfer rate    —".into(),
                    |v| format!("Transfer rate    {:.2} MB/s", v / 1048576.0),
                ),
                "All physical disks combined".into(),
            ],
            _ => vec![
                format!("Send + receive    {}", values[3]),
                format!(
                    "Combined link capacity    {:.0} Mbps",
                    state.snapshot.network_capacity as f64 / 1e6
                ),
                "Loopback and virtual interfaces excluded".into(),
            ],
        };
        for (i, line) in summary.iter().enumerate() {
            draw_text(
                dc,
                line,
                RECT {
                    left,
                    top: graph.bottom + 30 + i as i32 * 25,
                    right,
                    bottom: graph.bottom + 55 + i as i32 * 25,
                },
                COLORREF(0x333333),
                DT_LEFT,
            );
        }
    }
}

pub(super) unsafe fn caption(state: &State, dc: HDC) {
    unsafe {
        let mut window = RECT::default();
        if GetWindowRect(state.hwnd, &mut window).is_err() {
            return;
        }
        let width = window.right - window.left;
        let mut menu_rect = RECT::default();
        let menu = GetMenu(state.hwnd);
        let bottom = if GetMenuItemRect(Some(state.hwnd), menu, 0, &mut menu_rect).is_ok() {
            menu_rect.top - window.top
        } else {
            31
        };
        fill(
            dc,
            &RECT {
                left: 1,
                top: 1,
                right: width - 1,
                bottom,
            },
            COLORREF(0xe8a32c),
        );
        SelectObject(dc, state.font.into());
        if let Ok(icon) = LoadIconW(None, IDI_APPLICATION) {
            let _ = DrawIconEx(dc, 8, (bottom - 16) / 2, icon, 16, 16, 0, None, DI_NORMAL);
        }
        draw_text(
            dc,
            "Task Manager",
            RECT {
                left: 30,
                top: 1,
                right: width - 140,
                bottom,
            },
            COLORREF(0x111111),
            DT_LEFT,
        );
        for (i, label) in ["—", "□", "×"].iter().enumerate() {
            draw_text(
                dc,
                label,
                RECT {
                    left: width - 139 + i as i32 * 46,
                    top: 1,
                    right: width - 1 - (2 - i as i32) * 46,
                    bottom,
                },
                COLORREF(0x111111),
                DT_CENTER,
            );
        }
        for (i, label) in ["File", "Options", "View"].iter().enumerate() {
            let mut rect = RECT::default();
            if GetMenuItemRect(Some(state.hwnd), menu, i as u32, &mut rect).is_ok() {
                rect.left -= window.left;
                rect.right -= window.left;
                rect.top -= window.top;
                rect.bottom -= window.top;
                fill(dc, &rect, WHITE);
                draw_text(dc, label, rect, COLORREF(0x111111), DT_CENTER);
            }
        }
    }
}
