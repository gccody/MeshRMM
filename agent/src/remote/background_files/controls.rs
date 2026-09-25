use super::*;
struct Header {
    font: HFONT,
    resizing: Option<(usize, i32, i32)>,
}
unsafe extern "system" fn header_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        let state = data as *mut Header;
        if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(header_proc), id);
            let result = DefSubclassProc(hwnd, message, wparam, lparam);
            drop(Box::from_raw(data as *mut Header));
            return result;
        }
        if message == WM_LBUTTONDOWN {
            let x = lparam.0 as i16 as i32;
            let mut hit = HDHITTESTINFO {
                pt: POINT {
                    x,
                    y: (lparam.0 >> 16) as i16 as i32,
                },
                ..Default::default()
            };
            let index = SendMessageW(
                hwnd,
                HDM_HITTEST,
                None,
                Some(LPARAM((&mut hit as *mut HDHITTESTINFO) as isize)),
            )
            .0;
            if index >= 0 && (hit.flags & (HHT_ONDIVIDER | HHT_ONDIVOPEN)).0 != 0 {
                let list = GetParent(hwnd).unwrap_or_default();
                let width =
                    SendMessageW(list, LVM_GETCOLUMNWIDTH, Some(WPARAM(index as usize)), None).0
                        as i32;
                (*state).resizing = Some((index as usize, x, width));
                return LRESULT(0);
            }
        }
        if message == WM_MOUSEMOVE
            && let Some((index, start, width)) = (*state).resizing
        {
            let width = (width + lparam.0 as i16 as i32 - start).clamp(50, 2000);
            SendMessageW(
                GetParent(hwnd).unwrap_or_default(),
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(index)),
                Some(LPARAM(width as isize)),
            );
            return LRESULT(0);
        }
        if matches!(message, WM_LBUTTONUP | WM_CANCELMODE) && (*state).resizing.take().is_some() {
            return LRESULT(0);
        }
        if matches!(message, WM_PAINT | WM_PRINTCLIENT) {
            let mut ps = PAINTSTRUCT::default();
            let dc = if message == WM_PAINT {
                BeginPaint(hwnd, &mut ps)
            } else {
                HDC(wparam.0 as *mut _)
            };
            let mut bounds = RECT::default();
            let _ = GetClientRect(hwnd, &mut bounds);
            fill(dc, &bounds, 0xffffff);
            let count = SendMessageW(hwnd, HDM_GETITEMCOUNT, None, None).0;
            for index in 0..count {
                let mut rect = RECT::default();
                SendMessageW(
                    hwnd,
                    HDM_GETITEMRECT,
                    Some(WPARAM(index as usize)),
                    Some(LPARAM((&mut rect as *mut RECT) as isize)),
                );
                let mut text = [0u16; 256];
                let mut item = HDITEMW {
                    mask: HDI_TEXT | HDI_FORMAT,
                    pszText: PWSTR(text.as_mut_ptr()),
                    cchTextMax: text.len() as i32,
                    ..Default::default()
                };
                SendMessageW(
                    hwnd,
                    HDM_GETITEMW,
                    Some(WPARAM(index as usize)),
                    Some(LPARAM((&mut item as *mut HDITEMW) as isize)),
                );
                let text = String::from_utf16_lossy(
                    &text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())],
                );
                fill(
                    dc,
                    &RECT {
                        left: rect.right - 1,
                        ..rect
                    },
                    0xe5e5e5,
                );
                draw_text(
                    dc,
                    &text,
                    RECT {
                        left: rect.left + 8,
                        right: rect.right - 8,
                        ..rect
                    },
                    DT_VCENTER
                        | DT_SINGLELINE
                        | DT_END_ELLIPSIS
                        | if item.fmt.0 & HDF_RIGHT.0 != 0 {
                            DT_RIGHT
                        } else {
                            DT_LEFT
                        },
                    0x505050,
                    (*state).font,
                );
                if item.fmt.0 & (HDF_SORTUP.0 | HDF_SORTDOWN.0) != 0 {
                    draw_text(
                        dc,
                        if item.fmt.0 & HDF_SORTUP.0 != 0 {
                            "⌃"
                        } else {
                            "⌄"
                        },
                        RECT {
                            left: (rect.left + rect.right) / 2 - 5,
                            right: (rect.left + rect.right) / 2 + 5,
                            top: rect.top,
                            bottom: rect.top + 10,
                        },
                        DT_CENTER | DT_SINGLELINE,
                        0x808080,
                        (*state).font,
                    );
                }
            }
            fill(
                dc,
                &RECT {
                    top: bounds.bottom - 1,
                    ..bounds
                },
                0xe5e5e5,
            );
            if message == WM_PAINT {
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}
unsafe extern "system" fn drag_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(drag_proc), id);
        }
        if message == WM_LBUTTONUP {
            let mut point = POINT {
                x: lparam.0 as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            let _ = ClientToScreen(hwnd, &mut point);
            let _ = PostMessageW(
                Some(GetAncestor(hwnd, GA_ROOT)),
                DROP_FILES,
                WPARAM(0),
                LPARAM((point.x as u16 as u32 | (point.y as u16 as u32) << 16) as isize),
            );
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}
pub(super) fn install(list: HWND, font: HFONT) -> anyhow::Result<()> {
    super::super::background_tasks::install_list_scrollbars(list)?;
    unsafe {
        ensure!(
            SetWindowSubclass(list, Some(drag_proc), 2, 0).as_bool(),
            "Could not initialize file drag input"
        );
        let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut _);
        let data = Box::into_raw(Box::new(Header {
            font,
            resizing: None,
        }));
        if !SetWindowSubclass(header, Some(header_proc), 1, data as usize).as_bool() {
            drop(Box::from_raw(data));
            anyhow::bail!("Could not initialize Explorer columns");
        }
    }
    Ok(())
}

// Activate common-controls v6 for this helper only. Other Agent windows retain
// their existing activation context. v5 silently ignores LVM_SETVIEW.
pub(super) struct VisualStyles {
    handle: HANDLE,
    cookie: usize,
    manifest: PathBuf,
}
impl VisualStyles {
    pub fn activate() -> anyhow::Result<Self> {
        use std::io::Write;
        use windows::Win32::System::ApplicationInstallationAndServicing::*;
        let manifest = std::env::temp_dir().join(format!(
            "meshrmm-explorer-{}.manifest",
            uuid::Uuid::new_v4()
        ));
        let mut file = std::fs::File::create_new(&manifest)?;
        file.write_all(br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="MeshRMM.BackgroundExplorer" type="win32"/>
<dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
</assembly>"#)?;
        drop(file);
        let path = wide(&manifest);
        let context = ACTCTXW {
            cbSize: std::mem::size_of::<ACTCTXW>() as u32,
            lpSource: PCWSTR(path.as_ptr()),
            ..Default::default()
        };
        let handle = unsafe { CreateActCtxW(&context) };
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                let _ = std::fs::remove_file(manifest);
                return Err(error.into());
            }
        };
        let mut result = Self {
            handle,
            cookie: 0,
            manifest,
        };
        unsafe {
            ActivateActCtx(Some(result.handle), &mut result.cookie)?;
        }
        // The parsed activation context no longer needs the source file. Remove
        // it now because kill-on-close workspace cleanup may terminate the helper.
        let _ = std::fs::remove_file(&result.manifest);
        Ok(result)
    }
}
impl Drop for VisualStyles {
    fn drop(&mut self) {
        use windows::Win32::System::ApplicationInstallationAndServicing::*;
        unsafe {
            if self.cookie != 0 {
                let _ = DeactivateActCtx(0, self.cookie);
            }
            ReleaseActCtx(self.handle);
        }
        let _ = std::fs::remove_file(&self.manifest);
    }
}
