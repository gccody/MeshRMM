//! Windows 10-style file management on the isolated maintenance desktop.
mod clipboard;
mod commands;
pub(super) mod controls;
mod frame;
mod launch;
mod model;
mod paint;
mod window;
mod work;

use crate::win32::wide;
use anyhow::{Context, ensure};
use model::*;
use paint::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use window::*;
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Time::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, SetFocus,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};
use work::{ResultData, Work};

const LIST: usize = 101;
const LOCATION: usize = 201;
const GO: usize = 202;
const UP: usize = 203;
const REFRESH: usize = 204;
const OPEN: usize = 205;
const NEW_FOLDER: usize = 207;
const RENAME: usize = 208;
const COPY: usize = 209;
const PASTE: usize = 210;
const BACK: usize = 212;
const FORWARD: usize = 213;
const CUT: usize = 214;
const DELETE: usize = 215;
const PROPERTIES: usize = 216;
const SELECT_ALL: usize = 217;
const SELECT_NONE: usize = 218;
const INVERT: usize = 219;
const HOME: usize = 220;
const VIEW: usize = 221;
const DETAILS: usize = 222;
const SMALL: usize = 223;
const LARGE: usize = 224;
const HIDDEN: usize = 225;
const EXTENSIONS: usize = 226;
const SEARCH: usize = 227;
const NAV: usize = 228;
const CONFIRM: usize = 229;
const CANCEL: usize = 230;
const STATUS: usize = 231;
const SEARCH_GO: usize = 232;
const NEW_FILE: usize = 233;
const PREVIEW: usize = 234;
const COPY_PATH: usize = 235;
const UNDO: usize = 236;
const FILE_MENU: usize = 250;
const NEW_WINDOW: usize = 251;
const CLOSE: usize = 252;
const ADDRESS_EDIT: usize = 450;
const CRUMB: usize = 500;
const SORT_NAME: usize = 240;
const SORT_DATE: usize = 241;
const SORT_TYPE: usize = 242;
const SORT_SIZE: usize = 243;
const MAX_ENTRIES: usize = 20_000;
const MAX_PREVIEW: u64 = 1024 * 1024;
const UPDATE_SELECTION: u32 = WM_APP + 1;
const NAVIGATE: u32 = WM_APP + 2;
const DROP_FILES: u32 = WM_APP + 3;

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd).clamp(0, 32767) as usize;
        let mut text = vec![0_u16; length + 1];
        let count = GetWindowTextW(hwnd, &mut text);
        String::from_utf16_lossy(&text[..count as usize])
    }
}
fn set_text(hwnd: HWND, text: &str) {
    let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(wide(text).as_ptr())) };
}
fn modified_text(value: u64) -> String {
    if value == 0 {
        return String::new();
    }
    let file = FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    };
    let mut local = FILETIME::default();
    let mut time = SYSTEMTIME::default();
    unsafe {
        if FileTimeToLocalFileTime(&file, &mut local).is_err()
            || FileTimeToSystemTime(&local, &mut time).is_err()
        {
            return String::new();
        }
    }
    let mut date = [0u16; 80];
    let mut clock = [0u16; 80];
    unsafe {
        let date_len = GetDateFormatW(
            LOCALE_USER_DEFAULT,
            DATE_SHORTDATE.0,
            Some(&time),
            PCWSTR::null(),
            Some(&mut date),
        );
        let time_len = GetTimeFormatW(
            LOCALE_USER_DEFAULT,
            TIME_NOSECONDS.0,
            Some(&time),
            PCWSTR::null(),
            Some(&mut clock),
        );
        if date_len > 0 && time_len > 0 {
            return format!(
                "{} {}",
                String::from_utf16_lossy(&date[..date_len as usize - 1]),
                String::from_utf16_lossy(&clock[..time_len as usize - 1])
            );
        }
    }
    String::new()
}
fn size_text(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    } else {
        format!("{:.1} GB", bytes as f64 / 1073741824.0)
    }
}

struct State {
    hwnd: HWND,
    location: HWND,
    list: HWND,
    status: HWND,
    search: HWND,
    nav: HWND,
    crumbs: Vec<(HWND, PathBuf, i32)>,
    address_edit: bool,
    dragging: Vec<PathBuf>,
    control_down: bool,
    undo_stack: Vec<Vec<UndoAction>>,
    font: HFONT,
    symbols: HFONT,
    nav_icons: Vec<HICON>,
    path: PathBuf,
    rows: Vec<Entry>,
    visible: Vec<usize>,
    nav_paths: Vec<PathBuf>,
    copied: Vec<PathBuf>,
    cut: bool,
    clipboard_sequence: u64,
    history: History,
    travel: Option<usize>,
    receiver: Option<std::sync::mpsc::Receiver<anyhow::Result<ResultData>>>,
    cancelled: Arc<AtomicBool>,
    sort: usize,
    descending: bool,
    hidden: bool,
    extensions: bool,
    view_tab: bool,
    searching: bool,
    pending_delete: Vec<PathBuf>,
    pending_rename: Option<(PathBuf, String)>,
}
impl Drop for State {
    fn drop(&mut self) {
        for icon in &self.nav_icons {
            if !icon.is_invalid() {
                unsafe {
                    let _ = DestroyIcon(*icon);
                }
            }
        }
    }
}
impl State {
    fn start(&mut self, work: Work) -> anyhow::Result<()> {
        ensure!(
            self.receiver.is_none(),
            "An operation is still running. Please wait."
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        self.receiver = Some(receiver);
        self.cancelled = Arc::new(AtomicBool::new(false));
        let cancelled = self.cancelled.clone();
        self.pending_delete.clear();
        self.layout();
        set_text(self.status, "Working…");
        std::thread::spawn(move || {
            let _ = sender.send(work.execute_cancellable(&cancelled));
        });
        Ok(())
    }
    fn selection(&self) -> Vec<Entry> {
        let mut rows = Vec::new();
        let mut index = -1;
        loop {
            index = unsafe {
                SendMessageW(
                    self.list,
                    LVM_GETNEXTITEM,
                    Some(WPARAM(index as usize)),
                    Some(LPARAM(LVNI_SELECTED as isize)),
                )
                .0
            };
            if index < 0 {
                break;
            }
            if let Some(row) = self
                .visible
                .get(index as usize)
                .and_then(|i| self.rows.get(*i))
            {
                rows.push(row.clone());
            }
        }
        rows
    }
    fn selected(&self) -> anyhow::Result<Entry> {
        self.selection()
            .into_iter()
            .next()
            .context("Select a file or folder first.")
    }
    fn select(&self, all: bool, invert: bool) {
        unsafe {
            for index in 0..self.visible.len() {
                let selected = SendMessageW(
                    self.list,
                    LVM_GETITEMSTATE,
                    Some(WPARAM(index)),
                    Some(LPARAM(LVIS_SELECTED.0 as isize)),
                )
                .0 != 0;
                let item = LVITEMW {
                    stateMask: LVIS_SELECTED,
                    state: if if invert { !selected } else { all } {
                        LVIS_SELECTED
                    } else {
                        LIST_VIEW_ITEM_STATE_FLAGS(0)
                    },
                    ..Default::default()
                };
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    Some(WPARAM(index)),
                    Some(LPARAM((&item as *const LVITEMW) as isize)),
                );
            }
        }
    }
    fn selection_status(&self) {
        if self.receiver.is_some() || !self.pending_delete.is_empty() {
            return;
        }
        let selection = self.selection();
        set_text(
            self.status,
            &format!(
                "{} items{}{}",
                self.visible.len(),
                if selection.is_empty() {
                    String::new()
                } else {
                    format!(
                        "    {} items selected    {}",
                        selection.len(),
                        size_text(selection.iter().filter_map(|e| e.size).sum())
                    )
                },
                if self.searching {
                    "    Search results"
                } else {
                    ""
                }
            ),
        );
    }
    fn render(&mut self, edit: Option<&Path>, restore: Option<Vec<PathBuf>>) {
        let selected: Vec<_> = if edit.is_some() {
            Vec::new()
        } else {
            restore.unwrap_or_else(|| self.selection().into_iter().map(|e| e.path).collect())
        };
        sort_entries(&mut self.rows, self.sort, self.descending);
        unsafe {
            let header = HWND(SendMessageW(self.list, LVM_GETHEADER, None, None).0 as *mut _);
            for index in 0..4 {
                let mut item = HDITEMW {
                    mask: HDI_FORMAT,
                    fmt: HEADER_CONTROL_FORMAT_FLAGS(
                        HDF_STRING.0
                            | if index == 3 { HDF_RIGHT.0 } else { HDF_LEFT.0 }
                            | if index == self.sort {
                                if self.descending {
                                    HDF_SORTDOWN.0
                                } else {
                                    HDF_SORTUP.0
                                }
                            } else {
                                0
                            },
                    ),
                    ..Default::default()
                };
                SendMessageW(
                    header,
                    HDM_SETITEMW,
                    Some(WPARAM(index)),
                    Some(LPARAM((&mut item as *mut HDITEMW) as isize)),
                );
            }
        }
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, e)| self.hidden || !e.hidden)
            .map(|(i, _)| i)
            .collect();
        let mut edit_index = None;
        unsafe {
            let _ = SetPropW(
                self.hwnd,
                w!("MeshRMMReplacingRows"),
                Some(HANDLE(std::ptr::dangling_mut())),
            );
            SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(0)), None);
            SendMessageW(self.list, LVM_DELETEALLITEMS, None, None);
            for (index, row) in self.visible.iter().enumerate() {
                let entry = &self.rows[*row];
                let display_name = if self.extensions || entry.directory {
                    entry.name.clone()
                } else {
                    Path::new(&entry.name)
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                };
                let values = [
                    display_name,
                    modified_text(entry.modified),
                    entry.kind.clone(),
                    entry
                        .size
                        .map_or_else(String::new, |size| format!("{} KB", size.div_ceil(1024))),
                ];
                for (column, value) in values.iter().enumerate() {
                    let mut text = wide(value);
                    let item = LVITEMW {
                        mask: LVIF_TEXT
                            | if column == 0 {
                                LVIF_IMAGE | LVIF_STATE
                            } else {
                                LIST_VIEW_ITEM_FLAGS(0)
                            },
                        iItem: index as i32,
                        iSubItem: column as i32,
                        pszText: PWSTR(text.as_mut_ptr()),
                        iImage: entry.icon,
                        stateMask: LVIS_SELECTED,
                        state: if selected.contains(&entry.path)
                            || edit == Some(entry.path.as_path())
                        {
                            LVIS_SELECTED
                        } else {
                            LIST_VIEW_ITEM_STATE_FLAGS(0)
                        },
                        ..Default::default()
                    };
                    SendMessageW(
                        self.list,
                        if column == 0 {
                            LVM_INSERTITEMW
                        } else {
                            LVM_SETITEMW
                        },
                        None,
                        Some(LPARAM((&item as *const LVITEMW) as isize)),
                    );
                }
                if edit == Some(entry.path.as_path()) {
                    edit_index = Some(index);
                }
            }
            SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(1)), None);
            let _ = RemovePropW(self.hwnd, w!("MeshRMMReplacingRows"));
            let _ = InvalidateRect(Some(self.list), None, true);
            if let Some(index) = edit_index {
                let _ = SetFocus(Some(self.list));
                SendMessageW(self.list, LVM_ENSUREVISIBLE, Some(WPARAM(index)), None);
                SendMessageW(self.list, LVM_EDITLABELW, Some(WPARAM(index)), None);
            }
        }
        self.selection_status();
    }
    fn poll(&mut self) -> anyhow::Result<()> {
        let Some(receiver) = &self.receiver else {
            return Ok(());
        };
        let result = match receiver.try_recv() {
            Ok(value) => value,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(error) => Err(error.into()),
        };
        self.receiver = None;
        self.layout();
        if result.is_err() {
            self.travel = None;
            set_text(self.location, &location_label(&self.path));
        }
        match result? {
            ResultData::Opened(path) => {
                set_text(self.status, &format!("Opened {}", path.display()))
            }
            ResultData::Preview { path, text } => {
                show_preview(&path, &text)?;
                self.selection_status();
            }
            ResultData::List {
                path,
                rows,
                status,
                rename,
                search,
                clipboard,
                undo,
            } => {
                if !undo.is_empty() {
                    self.undo_stack.push(undo);
                    if self.undo_stack.len() > 20 {
                        self.undo_stack.remove(0);
                    }
                }
                if let Some(remaining) = clipboard
                    && self.clipboard_sequence == meshrmm_file_transfer::clipboard_sequence()
                {
                    clipboard::write(&remaining, true)?;
                    self.copied = remaining;
                    self.clipboard_sequence = meshrmm_file_transfer::clipboard_sequence();
                }
                if let Some(direction) = self.travel.take() {
                    if direction == BACK {
                        self.history.back.pop();
                        self.history.forward.push(self.path.clone());
                    } else {
                        self.history.forward.pop();
                        self.history.back.push(self.path.clone());
                    }
                } else {
                    self.history.visit(&self.path, &path);
                }
                if path != self.path {
                    set_text(self.search, "");
                }
                let restore = if self.path == path {
                    self.selection().into_iter().map(|e| e.path).collect()
                } else {
                    Vec::new()
                };
                self.path = path;
                self.rows = rows;
                self.visible.clear();
                self.searching = search;
                set_text(self.location, &location_label(&self.path));
                self.address_edit = false;
                self.rebuild_breadcrumbs()?;
                self.layout();
                self.render(rename.as_deref(), Some(restore));
                if !status.is_empty() {
                    set_text(self.status, &status);
                }
                unsafe {
                    for (id, enabled) in [
                        (BACK, !self.history.back.is_empty()),
                        (FORWARD, !self.history.forward.is_empty()),
                        (UP, !self.path.as_os_str().is_empty()),
                    ] {
                        if let Ok(control) = GetDlgItem(Some(self.hwnd), id as i32) {
                            let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(
                                control, enabled,
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn rebuild_breadcrumbs(&mut self) -> anyhow::Result<()> {
        for (window, _, _) in self.crumbs.drain(..) {
            unsafe {
                let _ = DestroyWindow(window);
            }
        }
        let mut paths = vec![("This PC".to_owned(), PathBuf::new())];
        if self.path == Path::new("::QuickAccess") {
            paths = vec![("Quick access".to_owned(), self.path.clone())];
        } else if !self.path.as_os_str().is_empty() {
            let ancestors: Vec<_> = self
                .path
                .ancestors()
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            for path in ancestors.into_iter().rev() {
                let label = path.file_name().map_or_else(
                    || path.to_string_lossy().into_owned(),
                    |n| n.to_string_lossy().into_owned(),
                );
                paths.push((label, path.to_path_buf()));
            }
        }
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(self.hwnd, &mut rect)?;
        }
        let budget = (rect.right - 450).max(160);
        while paths.len() > 1
            && paths
                .iter()
                .map(|(label, _)| (label.chars().count() as i32 * 7 + 28).min(180))
                .sum::<i32>()
                > budget
        {
            paths.remove(0);
        }
        for (index, (label, path)) in paths.into_iter().enumerate() {
            let width = (label.chars().count() as i32 * 7 + 28).min(180);
            let window = control(
                self.hwnd,
                w!("BUTTON"),
                &format!("{label}  ›"),
                CRUMB + index,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [116, 129, width, 26],
                self.font,
            )?;
            self.crumbs.push((window, path, width));
        }
        Ok(())
    }
    fn layout(&self) {
        unsafe {
            let mut rect = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rect);
            let width = rect.right;
            let height = rect.bottom;
            let confirmation = !self.pending_delete.is_empty();
            let busy = self.receiver.is_some();
            let footer = if confirmation || busy { 66 } else { 26 };
            let _ = MoveWindow(self.nav, 0, 162, 190, (height - 162 - footer).max(1), true);
            let _ = MoveWindow(
                self.list,
                194,
                162,
                (width - 194).max(1),
                (height - 162 - footer).max(1),
                true,
            );
            let _ = MoveWindow(self.location, 116, 130, (width - 408).max(100), 24, true);
            let mut crumb_x = 116;
            for (window, _, crumb_width) in &self.crumbs {
                let _ = MoveWindow(*window, crumb_x, 129, *crumb_width, 26, true);
                let _ = ShowWindow(*window, if self.address_edit { SW_HIDE } else { SW_SHOW });
                crumb_x += crumb_width;
            }
            let _ = ShowWindow(
                self.location,
                if self.address_edit { SW_SHOW } else { SW_HIDE },
            );
            if let Ok(end) = GetDlgItem(Some(self.hwnd), ADDRESS_EDIT as i32) {
                let _ = MoveWindow(end, crumb_x, 129, (width - 294 - crumb_x).max(1), 26, true);
                let _ = ShowWindow(end, if self.address_edit { SW_HIDE } else { SW_SHOW });
            }
            let _ = MoveWindow(self.search, (width - 248).max(230), 130, 210, 24, true);
            for (id, x) in [(GO, width - 286), (SEARCH_GO, width - 34)] {
                if let Ok(h) = GetDlgItem(Some(self.hwnd), id as i32) {
                    let _ = MoveWindow(h, x, 129, 30, 26, true);
                }
            }
            let _ = MoveWindow(
                self.status,
                12,
                height - footer + 5,
                (width - if confirmation || busy { 230 } else { 24 }).max(1),
                footer - 7,
                true,
            );
            for (id, x) in [(CONFIRM, width - 206), (CANCEL, width - 100)] {
                if let Ok(h) = GetDlgItem(Some(self.hwnd), id as i32) {
                    let _ = MoveWindow(h, x, height - footer + 8, 94, 28, true);
                    let _ = ShowWindow(
                        h,
                        if confirmation || (id == CANCEL && busy) {
                            SW_SHOW
                        } else {
                            SW_HIDE
                        },
                    );
                }
            }
            for item in RIBBON {
                if let Ok(h) = GetDlgItem(Some(self.hwnd), item.id as i32) {
                    let _ = ShowWindow(
                        h,
                        if item.view == self.view_tab {
                            SW_SHOW
                        } else {
                            SW_HIDE
                        },
                    );
                }
            }
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }
}

struct RibbonItem {
    id: usize,
    label: &'static str,
    glyph: u16,
    x: i32,
    width: i32,
    view: bool,
}
const RIBBON: &[RibbonItem] = &[
    RibbonItem {
        id: COPY,
        label: "Copy",
        glyph: 0xe8c8,
        x: 12,
        width: 54,
        view: false,
    },
    RibbonItem {
        id: PASTE,
        label: "Paste",
        glyph: 0xe77f,
        x: 68,
        width: 54,
        view: false,
    },
    RibbonItem {
        id: CUT,
        label: "Cut",
        glyph: 0xe8c6,
        x: 124,
        width: 54,
        view: false,
    },
    RibbonItem {
        id: DELETE,
        label: "Delete",
        glyph: 0xe74d,
        x: 194,
        width: 58,
        view: false,
    },
    RibbonItem {
        id: RENAME,
        label: "Rename",
        glyph: 0xe8ac,
        x: 254,
        width: 66,
        view: false,
    },
    RibbonItem {
        id: NEW_FOLDER,
        label: "New folder",
        glyph: 0xe8f4,
        x: 336,
        width: 72,
        view: false,
    },
    RibbonItem {
        id: NEW_FILE,
        label: "New item",
        glyph: 0xe8a5,
        x: 410,
        width: 68,
        view: false,
    },
    RibbonItem {
        id: PROPERTIES,
        label: "Properties",
        glyph: 0xe946,
        x: 494,
        width: 76,
        view: false,
    },
    RibbonItem {
        id: OPEN,
        label: "Open",
        glyph: 0xe8e5,
        x: 572,
        width: 54,
        view: false,
    },
    RibbonItem {
        id: SELECT_ALL,
        label: "Select all",
        glyph: 0xe8b3,
        x: 642,
        width: 76,
        view: false,
    },
    RibbonItem {
        id: SELECT_NONE,
        label: "Select none",
        glyph: 0xe8b4,
        x: 720,
        width: 80,
        view: false,
    },
    RibbonItem {
        id: INVERT,
        label: "Invert selection",
        glyph: 0xe8b5,
        x: 802,
        width: 100,
        view: false,
    },
    RibbonItem {
        id: LARGE,
        label: "Large icons",
        glyph: 0xe8a9,
        x: 12,
        width: 86,
        view: true,
    },
    RibbonItem {
        id: SMALL,
        label: "Small icons",
        glyph: 0xe80a,
        x: 100,
        width: 86,
        view: true,
    },
    RibbonItem {
        id: DETAILS,
        label: "Details",
        glyph: 0xe8a4,
        x: 188,
        width: 70,
        view: true,
    },
    RibbonItem {
        id: SORT_NAME,
        label: "Name",
        glyph: 0xe8cb,
        x: 278,
        width: 62,
        view: true,
    },
    RibbonItem {
        id: SORT_DATE,
        label: "Date modified",
        glyph: 0xe787,
        x: 342,
        width: 98,
        view: true,
    },
    RibbonItem {
        id: SORT_TYPE,
        label: "Type",
        glyph: 0xe8a5,
        x: 442,
        width: 60,
        view: true,
    },
    RibbonItem {
        id: SORT_SIZE,
        label: "Size",
        glyph: 0xe8cb,
        x: 504,
        width: 60,
        view: true,
    },
    RibbonItem {
        id: HIDDEN,
        label: "Hidden items",
        glyph: 0xe890,
        x: 584,
        width: 98,
        view: true,
    },
    RibbonItem {
        id: EXTENSIONS,
        label: "File name extensions",
        glyph: 0xe8a5,
        x: 684,
        width: 132,
        view: true,
    },
    RibbonItem {
        id: PREVIEW,
        label: "Preview",
        glyph: 0xe8a1,
        x: 832,
        width: 76,
        view: true,
    },
];
fn trace_startup(message: &str) {
    if let Some(proof) = std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(PathBuf::from(proof).join("file-browser-startup.log"))
        {
            let _ = writeln!(file, "{message}");
        }
    }
}
pub fn run() -> anyhow::Result<()> {
    let result = run_inner();
    if let Err(error) = &result {
        trace_startup(&format!("error: {error:#}"));
    }
    result
}

#[cfg(test)]
mod tests;
