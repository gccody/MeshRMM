//! A shell-independent file browser on the maintenance desktop.
use anyhow::{Context, ensure};
use std::io::Read;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::{CopyFileW, MoveFileW};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

const LIST: usize = 101;
const LOCATION: usize = 201;
const GO: usize = 202;
const UP: usize = 203;
const REFRESH: usize = 204;
const OPEN: usize = 205;
const NAME: usize = 206;
const NEW_FOLDER: usize = 207;
const RENAME: usize = 208;
const COPY: usize = 209;
const PASTE: usize = 210;
const NAME_LABEL: usize = 211;
const MAX_ENTRIES: usize = 20_000;
const MAX_PREVIEW: u64 = 1024 * 1024;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn path_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn location(text: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !text.contains('\0') && text.len() <= 32767,
        "Invalid location."
    );
    let path = PathBuf::from(text);
    ensure!(
        path.is_absolute(),
        "Enter an absolute path, for example C:\\Windows."
    );
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        ensure!(
            !matches!(
                prefix.kind(),
                std::path::Prefix::DeviceNS(_) | std::path::Prefix::Verbatim(_)
            ),
            "Device paths cannot be browsed."
        );
    }
    Ok(path)
}
fn child_path(directory: &Path, name: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.encode_utf16().count() <= 255
            && name != "."
            && name != ".."
            && !name.ends_with(['.', ' '])
            && !name.chars().any(|c| c < ' ' || "\\/:*?\"<>|".contains(c)),
        "Enter a single file or folder name without path separators."
    );
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    ensure!(!reserved, "That name is reserved by Windows.");
    Ok(directory.join(name))
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    directory: bool,
    size: Option<u64>,
}
fn entries(path: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut result = Vec::new();
    for entry in
        std::fs::read_dir(path).with_context(|| format!("Cannot read {}", path.display()))?
    {
        let entry = entry?;
        ensure!(
            result.len() < MAX_ENTRIES,
            "This folder exceeds the {MAX_ENTRIES} entry limit. Enter a subfolder path."
        );
        let metadata = entry.metadata();
        let directory = metadata.as_ref().is_ok_and(|m| m.is_dir());
        let size = metadata.ok().filter(|m| m.is_file()).map(|m| m.len());
        result.push(Entry {
            path: entry.path(),
            name: entry.file_name().to_string_lossy().into_owned(),
            directory,
            size,
        });
    }
    result.sort_by_cached_key(|entry| (!entry.directory, entry.name.to_lowercase()));
    Ok(result)
}
fn preview(path: &Path) -> anyhow::Result<String> {
    ensure!(path.is_file(), "Select a regular file to preview.");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_PREVIEW + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PREVIEW,
        "Text preview is limited to 1 MiB. Switch to a physical monitor to transfer larger files with the Files tool."
    );
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        ensure!(bytes.len().is_multiple_of(2), "Invalid UTF-16 text.");
        let little = bytes[0] == 0xff;
        let units: Vec<_> = bytes[2..]
            .chunks_exact(2)
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        String::from_utf16(&units).context("This file is not valid text.")?
    } else {
        String::from_utf8(bytes)
            .context("Preview supports UTF-8 and BOM-marked UTF-16 text files.")?
            .trim_start_matches('\u{feff}')
            .to_owned()
    };
    ensure!(
        !text.contains('\0'),
        "Binary files cannot be previewed. Switch to a physical monitor to transfer them with the Files tool."
    );
    Ok(text.replace("\r\n", "\n").replace('\n', "\r\n"))
}
fn copy_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    ensure!(
        source.is_file(),
        "Copy supports files. Switch to a physical monitor for folder transfers with the Files tool."
    );
    unsafe {
        CopyFileW(
            PCWSTR(path_wide(source).as_ptr()),
            PCWSTR(path_wide(destination).as_ptr()),
            true,
        )?;
    }
    Ok(())
}
fn rename(source: &Path, destination: &Path) -> anyhow::Result<()> {
    unsafe {
        MoveFileW(
            PCWSTR(path_wide(source).as_ptr()),
            PCWSTR(path_wide(destination).as_ptr()),
        )?;
    }
    Ok(())
}

enum Work {
    List(PathBuf),
    Preview(PathBuf),
    NewFolder(PathBuf, PathBuf),
    Rename(PathBuf, PathBuf, PathBuf),
    Copy(PathBuf, PathBuf, PathBuf),
}
enum ResultData {
    List {
        path: PathBuf,
        rows: Vec<Entry>,
        status: String,
    },
    Preview {
        path: PathBuf,
        text: String,
    },
}
impl Work {
    fn execute(self) -> anyhow::Result<ResultData> {
        let (path, status) = match self {
            Self::List(path) => (path, String::new()),
            Self::Preview(path) => {
                return Ok(ResultData::Preview {
                    text: preview(&path)?,
                    path,
                });
            }
            Self::NewFolder(path, target) => {
                std::fs::create_dir(target)?;
                (path, "Folder created. ".into())
            }
            Self::Rename(path, source, target) => {
                rename(&source, &target)?;
                (path, "Renamed. ".into())
            }
            Self::Copy(path, source, target) => {
                copy_file(&source, &target)?;
                (path, "File copied. ".into())
            }
        };
        Ok(ResultData::List {
            rows: entries(&path)?,
            path,
            status,
        })
    }
}
struct State {
    location: HWND,
    list: HWND,
    status: HWND,
    name: HWND,
    path: PathBuf,
    rows: Vec<Entry>,
    copied: Option<PathBuf>,
    last_click: Option<(std::time::Instant, i32)>,
    receiver: Option<std::sync::mpsc::Receiver<anyhow::Result<ResultData>>>,
}
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
impl State {
    fn start(&mut self, work: Work) -> anyhow::Result<()> {
        ensure!(
            self.receiver.is_none(),
            "An operation is still running. Please wait."
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        self.receiver = Some(receiver);
        set_text(self.status, "Working…");
        std::thread::spawn(move || {
            let _ = sender.send(work.execute());
        });
        Ok(())
    }
    fn selected(&self) -> anyhow::Result<Entry> {
        let index = unsafe {
            SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
        }
        .0;
        self.rows
            .get(index as usize)
            .cloned()
            .context("Select a file or folder first.")
    }
    fn command(&mut self, command: usize) -> anyhow::Result<()> {
        ensure!(
            self.receiver.is_none(),
            "An operation is still running. Please wait."
        );
        match command {
            GO => self.start(Work::List(location(&window_text(self.location))?)),
            UP => self.start(Work::List(
                self.path.parent().unwrap_or(&self.path).to_path_buf(),
            )),
            REFRESH => self.start(Work::List(self.path.clone())),
            OPEN => {
                let entry = self.selected()?;
                self.start(if entry.directory {
                    Work::List(entry.path)
                } else {
                    Work::Preview(entry.path)
                })
            }
            NEW_FOLDER => self.start(Work::NewFolder(
                self.path.clone(),
                child_path(&self.path, &window_text(self.name))?,
            )),
            RENAME => self.start(Work::Rename(
                self.path.clone(),
                self.selected()?.path,
                child_path(&self.path, &window_text(self.name))?,
            )),
            COPY => {
                let entry = self.selected()?;
                ensure!(
                    !entry.directory,
                    "Copy supports files; switch to a physical monitor for folder transfers with the Files tool."
                );
                set_text(
                    self.status,
                    &format!(
                        "Copied {}. Navigate to the destination and choose Paste.",
                        entry.name
                    ),
                );
                self.copied = Some(entry.path);
                Ok(())
            }
            PASTE => {
                let source = self
                    .copied
                    .clone()
                    .context("Select a file and choose Copy first.")?;
                let name = source.file_name().context("Invalid source filename")?;
                let target = self.path.join(name);
                self.start(Work::Copy(self.path.clone(), source, target))
            }
            _ => Ok(()),
        }
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
        match result? {
            ResultData::Preview { path, text } => {
                show_preview(&path, &text)?;
                set_text(self.status, "Opened read-only text preview.");
            }
            ResultData::List { path, rows, status } => {
                unsafe {
                    SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(0)), None);
                    SendMessageW(self.list, LVM_DELETEALLITEMS, None, None);
                    for (index, entry) in rows.iter().enumerate() {
                        let values = [
                            entry.name.clone(),
                            if entry.directory {
                                "Folder".into()
                            } else {
                                "File".into()
                            },
                            entry
                                .size
                                .map_or_else(String::new, |size| format!("{size} bytes")),
                        ];
                        for (column, value) in values.iter().enumerate() {
                            let mut text = wide(value);
                            let item = LVITEMW {
                                mask: LVIF_TEXT,
                                iItem: index as i32,
                                iSubItem: column as i32,
                                pszText: PWSTR(text.as_mut_ptr()),
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
                    }
                    SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(1)), None);
                    let _ = InvalidateRect(Some(self.list), None, true);
                }
                set_text(self.location, &path.to_string_lossy());
                set_text(
                    self.status,
                    &format!(
                        "{status}{} items · SYSTEM · Open previews text files",
                        rows.len()
                    ),
                );
                self.path = path;
                self.rows = rows;
                self.last_click = None;
            }
        }
        Ok(())
    }
}

unsafe extern "system" fn preview_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_SIZE
            && let Ok(edit) = GetDlgItem(Some(hwnd), 1)
        {
            let _ = MoveWindow(
                edit,
                0,
                0,
                (lparam.0 & 0xffff) as i32,
                ((lparam.0 >> 16) & 0xffff) as i32,
                true,
            );
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
fn show_preview(path: &Path, text: &str) -> anyhow::Result<()> {
    unsafe {
        let title = wide(&format!("{} — read-only", path.display()));
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            w!("MeshRMMBackgroundPreview"),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            80,
            48,
            1000,
            640,
            None,
            None,
            None,
            None,
        )?;
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_HSCROLL
                | WINDOW_STYLE(
                    ES_MULTILINE as u32
                        | ES_READONLY as u32
                        | ES_AUTOVSCROLL as u32
                        | ES_AUTOHSCROLL as u32,
                ),
            0,
            0,
            980,
            600,
            Some(hwnd),
            Some(HMENU(std::ptr::without_provenance_mut(1))),
            None,
            None,
        )?;
        SendMessageW(
            edit,
            EM_SETLIMITTEXT,
            Some(WPARAM(MAX_PREVIEW as usize * 2)),
            None,
        );
        set_text(edit, text);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    Ok(())
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if !state.is_null() && message == WM_NOTIFY {
            let header = &*(lparam.0 as *const NMHDR);
            if header.idFrom == LIST && header.code == NM_CLICK {
                let event = &*(lparam.0 as *const NMITEMACTIVATE);
                let state = &mut *state;
                let now = std::time::Instant::now();
                let double_click = state.last_click.take().is_some_and(|(previous, item)| {
                    item == event.iItem
                        && now.duration_since(previous).as_millis()
                            <= u128::from(GetDoubleClickTime())
                });
                if event.iItem >= 0 {
                    if double_click {
                        if let Err(error) = state.command(OPEN) {
                            set_text(state.status, &format!("{error:#}"));
                        }
                    } else {
                        state.last_click = Some((now, event.iItem));
                    }
                }
                return LRESULT(0);
            }
        }
        // Do not borrow State for synchronous list-view notifications raised
        // while poll() is replacing rows.
        if !state.is_null()
            && (message == WM_TIMER
                || message == WM_SIZE
                || (message == WM_COMMAND && wparam.0 >> 16 == 0))
        {
            let state = &mut *state;
            let result = match message {
                WM_TIMER => state.poll(),
                WM_COMMAND => state.command(wparam.0 & 0xffff),
                WM_SIZE => {
                    let width = (lparam.0 & 0xffff) as i32;
                    let height = ((lparam.0 >> 16) & 0xffff) as i32;
                    let _ = MoveWindow(state.location, 10, 10, (width - 274).max(1), 26, true);
                    for (id, offset) in [(GO, 254), (UP, 172), (REFRESH, 90)] {
                        if let Ok(button) = GetDlgItem(Some(hwnd), id as i32) {
                            let _ = MoveWindow(button, width - offset, 8, 80, 30, true);
                        }
                    }
                    let _ = MoveWindow(
                        state.list,
                        10,
                        48,
                        (width - 20).max(1),
                        (height - 150).max(1),
                        true,
                    );
                    let _ = MoveWindow(state.name, 60, height - 90, 220, 26, true);
                    if let Ok(label) = GetDlgItem(Some(hwnd), NAME_LABEL as i32) {
                        let _ = MoveWindow(label, 10, height - 86, 45, 24, true);
                    }
                    for (id, x, size) in [
                        (NEW_FOLDER, 290, 104),
                        (RENAME, 402, 90),
                        (OPEN, 500, 90),
                        (COPY, 598, 90),
                        (PASTE, 696, 90),
                    ] {
                        if let Ok(button) = GetDlgItem(Some(hwnd), id as i32) {
                            let _ = MoveWindow(button, x, height - 92, size, 30, true);
                        }
                    }
                    let _ =
                        MoveWindow(state.status, 10, height - 46, (width - 20).max(1), 36, true);
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                set_text(state.status, &format!("{error:#}"));
            }
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
fn button(parent: HWND, id: usize, label: &str, x: i32, y: i32, width: i32) -> anyhow::Result<()> {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wide(label).as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            x,
            y,
            width,
            30,
            Some(parent),
            Some(HMENU(id as *mut _)),
            None,
            None,
        )?;
    }
    Ok(())
}
pub fn run() -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    let _desktop = meshrmm_remote_screen::background::Desktop::bind()?;
    let path = PathBuf::from(format!(
        "{}\\",
        std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
    ));
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        })
        .ok()?;
        for (name, proc) in [
            (
                w!("MeshRMMBackgroundFiles"),
                Some(
                    window_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                ),
            ),
            (
                w!("MeshRMMBackgroundPreview"),
                Some(
                    preview_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                ),
            ),
        ] {
            let class = WNDCLASSW {
                lpfnWndProc: proc,
                lpszClassName: name,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut _),
                ..Default::default()
            };
            ensure!(
                RegisterClassW(&class) != 0,
                "Could not register file browser window"
            );
        }
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            w!("MeshRMMBackgroundFiles"),
            w!("MeshRMM File Browser"),
            WS_OVERLAPPEDWINDOW,
            40,
            24,
            1100,
            680,
            None,
            None,
            None,
            None,
        )?;
        let location = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR(path_wide(&path).as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            10,
            10,
            800,
            26,
            Some(hwnd),
            Some(HMENU(LOCATION as *mut _)),
            None,
            None,
        )?;
        for (id, label, x) in [(GO, "Go", 820), (UP, "Up", 902), (REFRESH, "Refresh", 984)] {
            button(hwnd, id, label, x, 8, 80)?;
        }
        let list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("SysListView32"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
            10,
            48,
            1054,
            498,
            Some(hwnd),
            Some(HMENU(LIST as *mut _)),
            None,
            None,
        )?;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
            )),
        );
        for (index, (name, width)) in [("Name", 660), ("Type", 140), ("Size", 220)]
            .iter()
            .enumerate()
        {
            let mut text = wide(name);
            let column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH,
                cx: *width,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index)),
                Some(LPARAM((&column as *const LVCOLUMNW) as isize)),
            );
        }
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Name:"),
            WS_CHILD | WS_VISIBLE,
            10,
            560,
            45,
            24,
            Some(hwnd),
            Some(HMENU(NAME_LABEL as *mut _)),
            None,
            None,
        )?;
        let name = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            60,
            556,
            220,
            26,
            Some(hwnd),
            Some(HMENU(NAME as *mut _)),
            None,
            None,
        )?;
        SendMessageW(
            name,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(w!("Name for new folder or rename").as_ptr() as isize)),
        );
        SendMessageW(name, EM_SETLIMITTEXT, Some(WPARAM(255)), None);
        for (id, label, x, width) in [
            (NEW_FOLDER, "New folder", 290, 104),
            (RENAME, "Rename", 402, 90),
            (OPEN, "Open", 500, 90),
            (COPY, "Copy file", 598, 90),
            (PASTE, "Paste file", 696, 90),
        ] {
            button(hwnd, id, label, x, 554, width)?;
        }
        let status = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Loading…"),
            WS_CHILD | WS_VISIBLE,
            10,
            600,
            1054,
            36,
            Some(hwnd),
            None,
            None,
            None,
        )?;
        let mut state = Box::new(State {
            location,
            list,
            status,
            name,
            path: path.clone(),
            rows: Vec::new(),
            copied: None,
            last_click: None,
            receiver: None,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *state as *mut State) as isize);
        state.start(Work::List(path))?;
        SetTimer(Some(hwnd), 1, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut message = MSG::default();
        let mut control_down = false;
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            // Track queued modifier events instead of the input thread's newer
            // keyboard state, which may already reflect a subsequent release.
            if matches!(message.wParam.0, 0x11 | 0xa2 | 0xa3)
                && matches!(message.message, WM_KEYDOWN | WM_KEYUP)
            {
                control_down = message.message == WM_KEYDOWN;
            }
            if message.message == WM_KEYDOWN
                && message.wParam.0 == 0x41
                && control_down
                && (message.hwnd == location || message.hwnd == name)
            {
                SendMessageW(message.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            } else if message.message == WM_KEYDOWN
                && message.wParam.0 == 13
                && (message.hwnd == location || message.hwnd == list)
            {
                let command = if message.hwnd == location { GO } else { OPEN };
                if let Err(error) = state.command(command) {
                    set_text(status, &format!("{error:#}"));
                }
            } else if !IsDialogMessageW(GetAncestor(message.hwnd, GA_ROOT), &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("meshrmm-browser-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn rejects_device_paths_and_unsafe_names() {
        let root = Path::new("C:\\");
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "a\\b",
            "C:foo",
            "name:stream",
            "CON.txt",
            "COM1",
            "LPT²",
            "trailing.",
            "trailing ",
            "bad\0name",
        ] {
            assert!(child_path(root, name).is_err(), "{name:?}");
        }
        assert_eq!(
            child_path(root, "résumé 2026.txt").unwrap(),
            root.join("résumé 2026.txt")
        );
        assert!(location("relative\\path").is_err());
        assert!(location("\\\\.\\PhysicalDrive0").is_err());
        assert!(location("\\\\?\\GLOBALROOT\\Device").is_err());
        assert!(location("C:\\Windows").is_ok());
        assert!(location("\\\\server\\share\\folder").is_ok());
    }
    #[test]
    fn browse_create_rename_and_copy_preserve_existing_files() {
        let directory = Directory::new();
        let folder = child_path(&directory.0, "Subfolder").unwrap();
        Work::NewFolder(directory.0.clone(), folder.clone())
            .execute()
            .unwrap();
        let source = child_path(&directory.0, "résumé file.txt").unwrap();
        std::fs::write(&source, b"original").unwrap();
        let renamed = child_path(&directory.0, "renamed.txt").unwrap();
        Work::Rename(directory.0.clone(), source.clone(), renamed.clone())
            .execute()
            .unwrap();
        assert!(!source.exists());
        let destination = folder.join("renamed.txt");
        Work::Copy(folder.clone(), renamed.clone(), destination.clone())
            .execute()
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        std::fs::write(&destination, b"keep this").unwrap();
        assert!(copy_file(&renamed, &destination).is_err());
        assert!(rename(&renamed, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep this");
        assert!(renamed.exists());
        let rows = entries(&directory.0).unwrap();
        assert!(rows[0].directory);
        assert_eq!(rows[1].name, "renamed.txt");
        assert!(entries(&directory.0.join("missing")).is_err());
    }
    #[test]
    fn preview_is_bounded_and_supports_windows_text_encodings() {
        let directory = Directory::new();
        let path = directory.0.join("preview.txt");
        std::fs::write(&path, "Hello\n世界").unwrap();
        assert_eq!(preview(&path).unwrap(), "Hello\r\n世界");
        let bytes: Vec<_> = [0xff, 0xfe]
            .into_iter()
            .chain("Hello\r\n世界".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(preview(&path).unwrap(), "Hello\r\n世界");
        std::fs::write(&path, b"binary\0content").unwrap();
        assert!(preview(&path).is_err());
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_PREVIEW + 1).unwrap();
        drop(file);
        assert!(preview(&path).is_err());
    }
}
