//! Windows 10-style file management on the isolated maintenance desktop.
mod clipboard;
mod controls;
mod frame;
mod launch;
mod model;
use anyhow::{Context, ensure};
use model::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Time::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, SetFocus};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

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

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn path_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
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

enum Work {
    List(PathBuf),
    Preview(PathBuf),
    Open(PathBuf),
    NewFolder(PathBuf, PathBuf),
    NewFile(PathBuf, PathBuf),
    Rename(PathBuf, PathBuf, PathBuf),
    #[cfg(test)]
    Copy(PathBuf, PathBuf, PathBuf),
    Transfer(PathBuf, PathBuf, Vec<PathBuf>, bool, bool),
    Delete(PathBuf, Vec<PathBuf>),
    Search(PathBuf, String),
    Undo(PathBuf, Vec<UndoAction>),
}
enum ResultData {
    Opened(PathBuf),
    List {
        path: PathBuf,
        rows: Vec<Entry>,
        status: String,
        rename: Option<PathBuf>,
        search: bool,
        clipboard: Option<Vec<PathBuf>>,
        undo: Vec<UndoAction>,
    },
    Preview {
        path: PathBuf,
        text: String,
    },
}
impl Work {
    #[cfg(test)]
    fn execute(self) -> anyhow::Result<ResultData> {
        self.execute_cancellable(&AtomicBool::new(false))
    }
    fn execute_cancellable(self, cancelled: &AtomicBool) -> anyhow::Result<ResultData> {
        ensure!(!cancelled.load(Ordering::Relaxed), "Operation cancelled.");
        let mut edit = None;
        let mut clipboard = None;
        let mut undo_actions = Vec::new();
        let (path, status) = match self {
            Self::List(path) => (path, String::new()),
            Self::Undo(mut path, actions) => {
                let status = match undo(&actions) {
                    Ok(()) => "Operation undone. ".into(),
                    Err(error) => format!("Undo stopped: {error:#}"),
                };
                while !virtual_location(&path) && !path.is_dir() {
                    path = path.parent().unwrap_or(Path::new("")).to_path_buf();
                }
                (path, status)
            }
            Self::Open(path) => {
                launch::open(&path)?;
                return Ok(ResultData::Opened(path));
            }
            Self::Preview(path) => {
                return Ok(ResultData::Preview {
                    text: preview(&path)?,
                    path,
                });
            }
            Self::Search(path, query) => {
                let mut rows = Vec::new();
                let mut pending = vec![path.clone()];
                let query = query.to_lowercase();
                let mut skipped = 0;
                while let Some(folder) = pending.pop() {
                    ensure!(!cancelled.load(Ordering::Relaxed), "Search cancelled.");
                    match entries(&folder) {
                        Ok(children) => {
                            for entry in children {
                                use std::os::windows::fs::MetadataExt;
                                if entry.directory
                                    && std::fs::symlink_metadata(&entry.path).is_ok_and(|m| {
                                        m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
                                    })
                                {
                                    pending.push(entry.path.clone());
                                }
                                if search_matches(&entry.name, &query) {
                                    rows.push(entry);
                                }
                                ensure!(
                                    rows.len() < MAX_ENTRIES,
                                    "Search exceeds {MAX_ENTRIES} results. Use a more specific search."
                                );
                            }
                        }
                        Err(_) => skipped += 1,
                    }
                }
                return Ok(ResultData::List {
                    path,
                    rows,
                    status: if skipped > 0 {
                        format!("{skipped} inaccessible folders skipped. ")
                    } else {
                        String::new()
                    },
                    rename: None,
                    search: true,
                    clipboard: None,
                    undo: Vec::new(),
                });
            }
            Self::NewFolder(path, target) => {
                std::fs::create_dir(&target)?;
                if let Ok(action) = UndoAction::created(&target) {
                    undo_actions.push(action);
                }
                edit = Some(target);
                (path, String::new())
            }
            Self::NewFile(path, target) => {
                std::fs::File::create_new(&target)?;
                if let Ok(action) = UndoAction::created(&target) {
                    undo_actions.push(action);
                }
                edit = Some(target);
                (path, String::new())
            }
            Self::Rename(path, source, target) => {
                rename(&source, &target)?;
                if let Ok(action) = UndoAction::moved(&source, &target) {
                    undo_actions.push(action);
                }
                (path, "Renamed. ".into())
            }
            #[cfg(test)]
            Self::Copy(path, source, target) => {
                copy_file(&source, &target)?;
                (path, "Copied. ".into())
            }
            Self::Transfer(path, destination, sources, cut, update_clipboard) => {
                let mut completed = 0;
                let mut remaining = sources.clone();
                let mut errors = Vec::new();
                for source in sources {
                    if cancelled.load(Ordering::Relaxed) {
                        errors.push("Operation cancelled; completed items were kept.".into());
                        break;
                    }
                    let target = if source.parent() == Some(destination.as_path()) && !cut {
                        unique_target(&destination, &source)
                    } else {
                        destination.join(source.file_name().context("Invalid source")?)
                    };
                    let result = if cut {
                        rename(&source, &target)
                    } else {
                        copy_tree_cancellable(&source, &target, cancelled)
                    };
                    match result {
                        Ok(()) => {
                            completed += 1;
                            match if cut {
                                UndoAction::moved(&source, &target)
                            } else {
                                UndoAction::created(&target)
                            } {
                                Ok(action) => undo_actions.push(action),
                                Err(error) => errors.push(format!(
                                    "Item completed, but Undo is unavailable: {error:#}"
                                )),
                            }
                            remaining.retain(|path| path != &source);
                        }
                        Err(e) => errors.push(format!("{}: {e:#}", source.display())),
                    }
                }
                if cut && update_clipboard {
                    clipboard = Some(remaining);
                }
                (
                    path,
                    format!(
                        "{completed} item(s) {}. {}",
                        if cut { "moved" } else { "copied" },
                        errors.join("; ")
                    ),
                )
            }
            Self::Delete(path, sources) => {
                let mut errors = Vec::new();
                for source in sources {
                    if cancelled.load(Ordering::Relaxed) {
                        errors.push("Operation cancelled; completed items were kept.".into());
                        break;
                    }
                    if let Err(e) = delete_tree(&source) {
                        errors.push(format!("{}: {e:#}", source.display()));
                    }
                }
                (path, errors.join("; "))
            }
        };
        Ok(ResultData::List {
            rows: entries(&path)?,
            path,
            status,
            rename: edit,
            search: false,
            clipboard,
            undo: undo_actions,
        })
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
    resizing: Option<(u32, POINT, RECT)>,
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
    last_click: Option<(std::time::Instant, i32)>,
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
    fn command(&mut self, command: usize) -> anyhow::Result<()> {
        if command == CANCEL && self.receiver.is_some() {
            self.cancelled.store(true, Ordering::Relaxed);
            set_text(self.status, "Cancelling…");
            return Ok(());
        }
        if command == CANCEL {
            self.address_edit = false;
            self.dragging.clear();
            self.pending_delete.clear();
            self.layout();
            self.selection_status();
            return Ok(());
        }
        ensure!(
            self.receiver.is_none(),
            "An operation is still running. Please wait."
        );
        if virtual_location(&self.path)
            && matches!(
                command,
                NEW_FOLDER | NEW_FILE | PASTE | SEARCH_GO | RENAME | DELETE | CUT
            )
        {
            anyhow::bail!("Open a drive or folder first.");
        }
        match command {
            UNDO => {
                let actions = self
                    .undo_stack
                    .pop()
                    .context("There is no file operation to undo.")?;
                self.start(Work::Undo(self.path.clone(), actions))
            }
            ADDRESS_EDIT => {
                self.address_edit = true;
                self.layout();
                unsafe {
                    let _ = SetFocus(Some(self.location));
                    SendMessageW(self.location, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                }
                Ok(())
            }
            CRUMB..=599 => {
                let path = self
                    .crumbs
                    .get(command - CRUMB)
                    .context("Unknown path segment")?
                    .1
                    .clone();
                self.start(Work::List(path))
            }
            CLOSE => {
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                Ok(())
            }
            NEW_WINDOW => {
                use std::os::windows::process::CommandExt;
                std::process::Command::new(std::env::current_exe()?)
                    .arg("--background-file-browser")
                    .creation_flags(0x08000000)
                    .spawn()?;
                Ok(())
            }
            GO => self.start(Work::List(location(&window_text(self.location))?)),
            UP => self.start(Work::List(
                self.path.parent().unwrap_or(Path::new("")).to_path_buf(),
            )),
            BACK | FORWARD => {
                let stack = if command == BACK {
                    &self.history.back
                } else {
                    &self.history.forward
                };
                let path = stack.last().context("No more history.")?.clone();
                self.travel = Some(command);
                self.start(Work::List(path))
            }
            REFRESH => {
                if self.searching {
                    self.command(SEARCH_GO)
                } else {
                    self.start(Work::List(self.path.clone()))
                }
            }
            SEARCH_GO => {
                let query = window_text(self.search);
                if query.trim().is_empty() {
                    self.start(Work::List(self.path.clone()))
                } else {
                    self.start(Work::Search(self.path.clone(), query))
                }
            }
            OPEN | PREVIEW => {
                let entry = self.selected()?;
                self.start(if entry.directory {
                    Work::List(entry.path)
                } else if command == PREVIEW {
                    Work::Preview(entry.path)
                } else {
                    Work::Open(entry.path)
                })
            }
            NEW_FOLDER | NEW_FILE => {
                let name = if command == NEW_FOLDER {
                    "New folder"
                } else {
                    "New Text Document.txt"
                };
                let mut target = self.path.join(name);
                for number in 2.. {
                    if !target.exists() {
                        break;
                    }
                    target = self.path.join(if command == NEW_FOLDER {
                        format!("New folder ({number})")
                    } else {
                        format!("New Text Document ({number}).txt")
                    });
                }
                self.start(if command == NEW_FOLDER {
                    Work::NewFolder(self.path.clone(), target)
                } else {
                    Work::NewFile(self.path.clone(), target)
                })
            }
            RENAME => {
                ensure!(self.selection().len() == 1, "Select one item to rename.");
                unsafe {
                    let index = SendMessageW(
                        self.list,
                        LVM_GETNEXTITEM,
                        Some(WPARAM(usize::MAX)),
                        Some(LPARAM(LVNI_SELECTED as isize)),
                    );
                    let _ = SetFocus(Some(self.list));
                    SendMessageW(
                        self.list,
                        LVM_EDITLABELW,
                        Some(WPARAM(index.0 as usize)),
                        None,
                    );
                }
                Ok(())
            }
            COPY | CUT => {
                let rows = self.selection();
                ensure!(!rows.is_empty(), "Select files or folders first.");
                self.copied = rows.into_iter().map(|e| e.path).collect();
                self.cut = command == CUT;
                clipboard::write(&self.copied, self.cut)?;
                self.clipboard_sequence = meshrmm_file_transfer::clipboard_sequence();
                self.render(None, None);
                set_text(
                    self.status,
                    &format!(
                        "{} item(s) ready to {}. Choose a destination and Paste.",
                        self.copied.len(),
                        if self.cut { "move" } else { "copy" }
                    ),
                );
                Ok(())
            }
            PASTE => {
                (self.copied, self.cut) = clipboard::read()?;
                self.clipboard_sequence = meshrmm_file_transfer::clipboard_sequence();
                ensure!(
                    !self.copied.is_empty(),
                    "Copy or cut files or folders first."
                );
                self.start(Work::Transfer(
                    self.path.clone(),
                    self.path.clone(),
                    self.copied.clone(),
                    self.cut,
                    true,
                ))
            }
            DELETE => {
                self.pending_delete = self.selection().into_iter().map(|e| e.path).collect();
                ensure!(
                    !self.pending_delete.is_empty(),
                    "Select files or folders first."
                );
                self.layout();
                set_text(
                    self.status,
                    &format!(
                        "Permanently delete {} item(s)? They will not go to the Recycle Bin. {}",
                        self.pending_delete.len(),
                        self.pending_delete
                            .iter()
                            .filter_map(|p| p.file_name())
                            .map(|p| p.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                Ok(())
            }
            CONFIRM => {
                ensure!(!self.pending_delete.is_empty(), "No deletion is pending.");
                let paths = std::mem::take(&mut self.pending_delete);
                self.start(Work::Delete(self.path.clone(), paths))
            }
            PROPERTIES => {
                let rows = self.selection();
                ensure!(!rows.is_empty(), "Select an item first.");
                let text = rows.iter().map(|e| format!("Name: {}\r\nType: {}\r\nLocation: {}\r\nSize: {}\r\nDate modified: {}\r\nHidden: {}\r\n", e.name, e.kind, e.path.display(), e.size.map_or_else(|| "Folder".into(), size_text), modified_text(e.modified), e.hidden)).collect::<Vec<_>>().join("\r\n");
                show_preview(Path::new("Properties"), &text)
            }
            COPY_PATH => {
                let text = self
                    .selection()
                    .iter()
                    .map(|e| format!("\"{}\"", e.path.display()))
                    .collect::<Vec<_>>()
                    .join("\r\n");
                ensure!(!text.is_empty(), "Select an item first.");
                meshrmm_clipboard::ClipboardSync::new(false)?
                    .apply(meshrmm_protocol::ClipboardContent::Text(text))?;
                set_text(self.status, "Paths copied to clipboard.");
                Ok(())
            }
            SELECT_ALL | SELECT_NONE | INVERT => {
                self.select(command == SELECT_ALL, command == INVERT);
                Ok(())
            }
            HOME | VIEW => {
                self.view_tab = command == VIEW;
                self.layout();
                Ok(())
            }
            DETAILS | SMALL | LARGE => {
                unsafe {
                    SendMessageW(
                        self.list,
                        LVM_SETVIEW,
                        Some(WPARAM(match command {
                            LARGE => LV_VIEW_ICON,
                            SMALL => LV_VIEW_SMALLICON,
                            _ => LV_VIEW_DETAILS,
                        } as usize)),
                        None,
                    );
                }
                Ok(())
            }
            HIDDEN | EXTENSIONS => {
                if command == HIDDEN {
                    self.hidden = !self.hidden;
                } else {
                    self.extensions = !self.extensions;
                }
                self.render(None, None);
                Ok(())
            }
            SORT_NAME..=SORT_SIZE => {
                let column = command - SORT_NAME;
                self.descending = self.sort == column && !self.descending;
                self.sort = column;
                self.render(None, None);
                Ok(())
            }
            _ => Ok(()),
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
                self.last_click = None;
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
fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    id: usize,
    style: WINDOW_STYLE,
    rect: [i32; 4],
    font: HFONT,
) -> anyhow::Result<HWND> {
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(wide(text).as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            rect[0],
            rect[1],
            rect[2],
            rect[3],
            Some(parent),
            Some(HMENU(id as *mut _)),
            None,
            None,
        )?;
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        Ok(h)
    }
}
fn fill(dc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(dc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}
fn draw_text(
    dc: HDC,
    text: &str,
    mut rect: RECT,
    flags: DRAW_TEXT_FORMAT,
    color: u32,
    font: HFONT,
) {
    if text.is_empty() {
        return;
    }
    unsafe {
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(color));
        DrawTextW(
            dc,
            &mut text.encode_utf16().collect::<Vec<_>>(),
            &mut rect,
            flags,
        );
        SelectObject(dc, old);
    }
}
fn paint(hwnd: HWND, state: &State) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        fill(dc, &r, 0xffffff);
        fill(
            dc,
            &RECT {
                top: 27,
                bottom: 122,
                ..r
            },
            0xf5f6f7,
        );
        fill(
            dc,
            &RECT {
                top: 121,
                bottom: 122,
                ..r
            },
            0xd9d9d9,
        );
        fill(
            dc,
            &RECT {
                top: 161,
                bottom: 162,
                ..r
            },
            0xe5e5e5,
        );
        fill(
            dc,
            &RECT {
                left: 190,
                right: 191,
                top: 162,
                bottom: r.bottom - 26,
            },
            0xe5e5e5,
        );
        fill(
            dc,
            &RECT {
                top: r.bottom - 27,
                bottom: r.bottom - 26,
                ..r
            },
            0xe5e5e5,
        );
        for (label, left, right) in if state.view_tab {
            vec![
                ("Layout", 10, 262),
                ("Sort by", 276, 568),
                ("Show/hide", 582, 820),
                ("Panes", 830, 912),
            ]
        } else {
            vec![
                ("Clipboard", 10, 182),
                ("Organize", 192, 324),
                ("New", 334, 482),
                ("Open", 492, 630),
                ("Select", 640, 908),
            ]
        } {
            fill(
                dc,
                &RECT {
                    left: right,
                    right: right + 1,
                    top: 36,
                    bottom: 111,
                },
                0xd9d9d9,
            );
            draw_text(
                dc,
                label,
                RECT {
                    left,
                    right,
                    top: 101,
                    bottom: 119,
                },
                DT_CENTER | DT_SINGLELINE,
                0x646464,
                state.font,
            );
        }
        for h in [state.location, state.search] {
            let mut box_rect = RECT::default();
            let _ = GetWindowRect(h, &mut box_rect);
            let mut point = POINT {
                x: box_rect.left,
                y: box_rect.top,
            };
            let _ = ScreenToClient(hwnd, &mut point);
            let edge = RECT {
                left: point.x - 3,
                top: point.y - 2,
                right: point.x + box_rect.right - box_rect.left + 2,
                bottom: point.y + box_rect.bottom - box_rect.top + 2,
            };
            let brush = CreateSolidBrush(COLORREF(0xc5c5c5));
            FrameRect(dc, &edge, brush);
            let _ = DeleteObject(brush.into());
        }
        let _ = EndPaint(hwnd, &ps);
    }
}
fn draw_navigation(draw: &DRAWITEMSTRUCT, state: &State) {
    if draw.itemID == u32::MAX {
        return;
    }
    let index = draw.itemID as usize;
    let selected = draw.itemState.0 & ODS_SELECTED.0 != 0;
    fill(
        draw.hDC,
        &draw.rcItem,
        if selected { 0xf7e8d5 } else { 0xffffff },
    );
    let mut text = vec![0u16; 512];
    unsafe {
        SendMessageW(
            state.nav,
            LB_GETTEXT,
            Some(WPARAM(index)),
            Some(LPARAM(text.as_mut_ptr() as isize)),
        );
    }
    let text =
        String::from_utf16_lossy(&text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())]);
    let heading = index == 0 || index == 5;
    let left = if heading { 20 } else { 36 };
    if index == 0 {
        draw_text(
            draw.hDC,
            "★",
            RECT {
                left,
                right: left + 20,
                ..draw.rcItem
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            0xdb8f22,
            state.font,
        );
    } else {
        let icon = state.nav_icons[if index == 5 {
            1
        } else if index > 5 {
            2
        } else {
            0
        }];
        if !icon.is_invalid() {
            unsafe {
                let _ = DrawIconEx(
                    draw.hDC,
                    left + 2,
                    draw.rcItem.top + 6,
                    icon,
                    16,
                    16,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
        }
    }
    draw_text(
        draw.hDC,
        &text,
        RECT {
            left: left + 26,
            ..draw.rcItem
        },
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        0x242424,
        state.font,
    );
}
fn draw_button(draw: &DRAWITEMSTRUCT, state: &State) {
    let id = draw.CtlID as usize;
    let pressed = draw.itemState.0 & ODS_SELECTED.0 != 0;
    let checked = (id == HOME && !state.view_tab)
        || (id == VIEW && state.view_tab)
        || (id == HIDDEN && state.hidden)
        || (id == EXTENSIONS && state.extensions);
    let disabled = draw.itemState.0 & ODS_DISABLED.0 != 0;
    fill(
        draw.hDC,
        &draw.rcItem,
        if pressed || checked {
            0xf7e8d5
        } else if RIBBON.iter().any(|i| i.id == id) {
            0xf5f6f7
        } else {
            0xffffff
        },
    );
    let color = if id == FILE_MENU {
        0xffffff
    } else if disabled {
        0xaaaaaa
    } else {
        0x303030
    };
    if id == FILE_MENU {
        fill(draw.hDC, &draw.rcItem, 0xc06700);
    }
    if let Some(item) = RIBBON.iter().find(|i| i.id == id) {
        if id == NEW_FOLDER {
            let x = (draw.rcItem.right - 32) / 2;
            fill(
                draw.hDC,
                &RECT {
                    left: x,
                    top: 13,
                    right: x + 13,
                    bottom: 20,
                },
                0x24c6f5,
            );
            fill(
                draw.hDC,
                &RECT {
                    left: x,
                    top: 18,
                    right: x + 32,
                    bottom: 39,
                },
                0x24c6f5,
            );
        } else {
            draw_text(
                draw.hDC,
                &String::from_utf16_lossy(&[item.glyph]),
                RECT {
                    top: 9,
                    bottom: 41,
                    ..draw.rcItem
                },
                DT_CENTER | DT_SINGLELINE,
                if id == NEW_FOLDER {
                    0x00b6ed
                } else if id == DELETE {
                    0x4444cb
                } else {
                    0x92652d
                },
                state.symbols,
            );
        }
        draw_text(
            draw.hDC,
            item.label,
            RECT {
                top: 48,
                bottom: 68,
                ..draw.rcItem
            },
            DT_CENTER | DT_SINGLELINE,
            color,
            state.font,
        );
    } else {
        let label = window_text(draw.hwndItem);
        draw_text(
            draw.hDC,
            &label,
            draw.rcItem,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            color,
            state.font,
        );
    }
    if draw.itemState.0 & ODS_FOCUS.0 != 0 {
        unsafe {
            let _ = DrawFocusRect(draw.hDC, &draw.rcItem);
        }
    }
}
fn context_menu(hwnd: HWND, state: &State, point: POINT) {
    unsafe {
        if let Ok(menu) = CreatePopupMenu() {
            let selected = !state.selection().is_empty();
            for (id, label, enabled) in [
                (OPEN, "Open\tEnter", selected),
                (CUT, "Cut\tCtrl+X", selected),
                (COPY, "Copy\tCtrl+C", selected),
                (
                    PASTE,
                    "Paste\tCtrl+V",
                    meshrmm_file_transfer::clipboard_has_files(),
                ),
                (RENAME, "Rename\tF2", selected),
                (DELETE, "Delete\tDel", selected),
                (NEW_FOLDER, "New folder\tCtrl+Shift+N", true),
                (REFRESH, "Refresh\tF5", true),
                (PROPERTIES, "Properties\tAlt+Enter", selected),
            ] {
                let _ = AppendMenuW(
                    menu,
                    if enabled {
                        MF_STRING
                    } else {
                        MF_STRING | MF_GRAYED
                    },
                    id,
                    PCWSTR(wide(label).as_ptr()),
                );
            }
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            if command.0 != 0 {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_COMMAND,
                    WPARAM(command.0 as usize),
                    LPARAM(0),
                );
            }
        }
    }
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        // List notifications are synchronous, including during render(). Never
        // borrow State on those paths; defer selection/sort/navigation to the queue.
        if message == WM_NOTIFY && !pointer.is_null() {
            let header = &*(lparam.0 as *const NMHDR);
            if header.idFrom == LIST {
                if header.code == LVN_ITEMCHANGED {
                    if !GetPropW(hwnd, w!("MeshRMMReplacingRows")).is_invalid() {
                        return LRESULT(0);
                    }
                    let _ = PostMessageW(Some(hwnd), UPDATE_SELECTION, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                if header.code == LVN_COLUMNCLICK {
                    let event = &*(lparam.0 as *const NMLISTVIEW);
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_COMMAND,
                        WPARAM(SORT_NAME + event.iSubItem as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                if header.code == LVN_BEGINLABELEDITW {
                    return LRESULT(
                        (*pointer)
                            .try_borrow()
                            .is_ok_and(|state| state.receiver.is_some() || state.searching)
                            as isize,
                    );
                }
                if header.code == LVN_ENDLABELEDITW {
                    let event = &*(lparam.0 as *const NMLVDISPINFOW);
                    if !event.item.pszText.is_null()
                        && let Ok(mut state) = (*pointer).try_borrow_mut()
                        && let Some(entry) = state
                            .visible
                            .get(event.item.iItem as usize)
                            .and_then(|i| state.rows.get(*i))
                    {
                        state.pending_rename = Some((
                            entry.path.clone(),
                            event.item.pszText.to_string().unwrap_or_default(),
                        ));
                        let _ = PostMessageW(Some(hwnd), NAVIGATE, WPARAM(1), LPARAM(0));
                    }
                    return LRESULT(0);
                }
                if header.code == LVN_BEGINDRAG {
                    if let Ok(mut state) = (*pointer).try_borrow_mut()
                        && state.receiver.is_none()
                    {
                        state.dragging = state
                            .selection()
                            .into_iter()
                            .map(|entry| entry.path)
                            .collect();
                        set_text(
                            state.status,
                            "Drag to a folder to move. Hold Ctrl to copy; press Esc to cancel.",
                        );
                    }
                    return LRESULT(0);
                }
                if header.code == NM_CLICK {
                    let event = &*(lparam.0 as *const NMITEMACTIVATE);
                    let Ok(mut state) = (*pointer).try_borrow_mut() else {
                        return LRESULT(0);
                    };
                    let now = std::time::Instant::now();
                    let double = state.last_click.take().is_some_and(|(then, item)| {
                        item == event.iItem
                            && now.duration_since(then).as_millis()
                                <= u128::from(GetDoubleClickTime())
                    });
                    if event.iItem >= 0 {
                        if double {
                            let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(OPEN), LPARAM(0));
                        } else {
                            state.last_click = Some((now, event.iItem));
                        }
                    }
                    return LRESULT(0);
                }
            }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_NCCALCSIZE {
            let rect = if wparam.0 != 0 {
                &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
            } else {
                &mut *(lparam.0 as *mut RECT)
            };
            rect.left += 1;
            rect.right -= 1;
            rect.top += 31;
            rect.bottom -= 1;
            return LRESULT(0);
        }
        if message == WM_NCHITTEST {
            return LRESULT(frame::hit(
                hwnd,
                POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                },
            ) as isize);
        }
        if !pointer.is_null() {
            if matches!(message, WM_NCPAINT | WM_NCACTIVATE | WM_PRINT) {
                let result = DefWindowProcW(hwnd, message, wparam, lparam);
                let dc = if message == WM_PRINT {
                    HDC(wparam.0 as *mut _)
                } else {
                    GetWindowDC(Some(hwnd))
                };
                if !dc.is_invalid() {
                    if let Ok(state) = (*pointer).try_borrow() {
                        frame::paint(hwnd, dc, state.font);
                    }
                    if message != WM_PRINT {
                        ReleaseDC(Some(hwnd), dc);
                    }
                }
                return result;
            }
            if message == WM_LBUTTONDOWN {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut point);
                let edge = frame::hit(hwnd, point);
                if edge == HTMINBUTTON {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(SC_MINIMIZE as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                if matches!(
                    edge,
                    HTLEFT
                        | HTRIGHT
                        | HTTOP
                        | HTBOTTOM
                        | HTTOPLEFT
                        | HTTOPRIGHT
                        | HTBOTTOMLEFT
                        | HTBOTTOMRIGHT
                ) {
                    let mut bounds = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut bounds);
                    if let Ok(mut state) = (*pointer).try_borrow_mut() {
                        state.resizing = Some((edge, point, bounds));
                    }
                    return LRESULT(0);
                }
            }
            if message == WM_MOUSEMOVE
                && let Some((edge, start, bounds)) = (*pointer)
                    .try_borrow()
                    .ok()
                    .and_then(|state| state.resizing)
            {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut point);
                let bounds = frame::resize(edge, start, bounds, point);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    bounds.left,
                    bounds.top,
                    bounds.right - bounds.left,
                    bounds.bottom - bounds.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                return LRESULT(0);
            }
            if matches!(message, WM_LBUTTONUP | WM_CANCELMODE)
                && let Ok(mut state) = (*pointer).try_borrow_mut()
            {
                state.resizing = None;
            }

            if message == WM_PAINT {
                if let Ok(state) = (*pointer).try_borrow() {
                    paint(hwnd, &state);
                    return LRESULT(0);
                }
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            if message == WM_DRAWITEM {
                let Ok(state) = (*pointer).try_borrow() else {
                    return LRESULT(0);
                };
                let draw = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if draw.CtlID as usize == NAV {
                    draw_navigation(draw, &state);
                    return LRESULT(1);
                }
                draw_button(&*(lparam.0 as *const DRAWITEMSTRUCT), &state);
                return LRESULT(1);
            }
            if message == WM_CONTEXTMENU {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                if point.x == -1 {
                    let _ = GetCursorPos(&mut point);
                }
                if let Ok(state) = (*pointer).try_borrow() {
                    context_menu(hwnd, &state, point);
                }
                return LRESULT(0);
            }
            if message == WM_COMMAND && wparam.0 & 0xffff == FILE_MENU {
                if let Ok(menu) = CreatePopupMenu() {
                    for (id, label) in [
                        (NEW_WINDOW, "Open new window"),
                        (COPY_PATH, "Copy path"),
                        (UNDO, "Undo\tCtrl+Z"),
                        (PROPERTIES, "Properties"),
                        (CLOSE, "Close"),
                    ] {
                        let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide(label).as_ptr()));
                    }
                    let mut point = POINT { x: 0, y: 27 };
                    let _ = ClientToScreen(hwnd, &mut point);
                    let command =
                        TrackPopupMenu(menu, TPM_RETURNCMD, point.x, point.y, None, hwnd, None);
                    let _ = DestroyMenu(menu);
                    if command.0 != 0 {
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_COMMAND,
                            WPARAM(command.0 as usize),
                            LPARAM(0),
                        );
                    }
                }
                return LRESULT(0);
            }
            let Ok(mut state) = (*pointer).try_borrow_mut() else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let result = match message {
                WM_TIMER => state.poll(),
                WM_SIZE => {
                    state.layout();
                    Ok(())
                }
                UPDATE_SELECTION => {
                    state.selection_status();
                    Ok(())
                }
                DROP_FILES => {
                    let sources = std::mem::take(&mut state.dragging);
                    if sources.is_empty() {
                        Ok(())
                    } else {
                        let point = POINT {
                            x: lparam.0 as i16 as i32,
                            y: (lparam.0 >> 16) as i16 as i32,
                        };
                        let mut list_bounds = RECT::default();
                        let mut nav_bounds = RECT::default();
                        let _ = GetWindowRect(state.list, &mut list_bounds);
                        let _ = GetWindowRect(state.nav, &mut nav_bounds);
                        let contains = |r: RECT| {
                            point.x >= r.left
                                && point.x < r.right
                                && point.y >= r.top
                                && point.y < r.bottom
                        };
                        let target = if contains(list_bounds) {
                            let mut hit = LVHITTESTINFO {
                                pt: POINT {
                                    x: point.x - list_bounds.left,
                                    y: point.y - list_bounds.top,
                                },
                                ..Default::default()
                            };
                            let row = SendMessageW(
                                state.list,
                                LVM_HITTEST,
                                None,
                                Some(LPARAM((&mut hit as *mut LVHITTESTINFO) as isize)),
                            )
                            .0;
                            state
                                .visible
                                .get(row as usize)
                                .and_then(|index| state.rows.get(*index))
                                .filter(|entry| entry.directory)
                                .map(|entry| entry.path.clone())
                                .or_else(|| Some(state.path.clone()))
                        } else if contains(nav_bounds) {
                            let index = SendMessageW(
                                state.nav,
                                LB_ITEMFROMPOINT,
                                None,
                                Some(LPARAM(
                                    ((point.x - nav_bounds.left) as u16 as u32
                                        | ((point.y - nav_bounds.top) as u16 as u32) << 16)
                                        as isize,
                                )),
                            )
                            .0;
                            state.nav_paths.get((index & 0xffff) as usize).cloned()
                        } else {
                            None
                        };
                        if let Some(target) = target.filter(|path| !virtual_location(path)) {
                            let cut = !state.control_down
                                && sources.iter().all(|source| {
                                    source.components().next() == target.components().next()
                                });
                            if sources
                                .iter()
                                .all(|source| source.parent() == Some(target.as_path()))
                                && cut
                            {
                                state.selection_status();
                                Ok(())
                            } else {
                                {
                                    let path = state.path.clone();
                                    state.start(Work::Transfer(path, target, sources, cut, false))
                                }
                            }
                        } else {
                            state.selection_status();
                            Ok(())
                        }
                    }
                }
                NAVIGATE => {
                    if wparam.0 == 1 {
                        if let Some((source, mut name)) = state.pending_rename.take() {
                            if !state.extensions
                                && source.is_file()
                                && let Some(extension) = source.extension()
                            {
                                name.push('.');
                                name.push_str(&extension.to_string_lossy());
                            }
                            child_path(source.parent().unwrap_or(&state.path), &name).and_then(
                                |target| {
                                    if target == source {
                                        Ok(())
                                    } else {
                                        let directory = state.path.clone();
                                        state.start(Work::Rename(directory, source, target))
                                    }
                                },
                            )
                        } else {
                            Ok(())
                        }
                    } else {
                        let index = SendMessageW(state.nav, LB_GETCURSEL, None, None).0;
                        if let Some(path) = state.nav_paths.get(index as usize).cloned() {
                            state.start(Work::List(path))
                        } else {
                            Ok(())
                        }
                    }
                }
                WM_COMMAND
                    if wparam.0 & 0xffff == NAV && wparam.0 >> 16 == LBN_SELCHANGE as usize =>
                {
                    let _ = PostMessageW(Some(hwnd), NAVIGATE, WPARAM(0), LPARAM(0));
                    Ok(())
                }
                WM_COMMAND if wparam.0 >> 16 == 0 => state.command(wparam.0 & 0xffff),
                _ => Ok(()),
            };
            if let Err(error) = result {
                set_text(state.status, &format!("{error:#}"));
            }
        }
        if message == WM_CTLCOLORSTATIC {
            SetBkMode(HDC(wparam.0 as *mut _), TRANSPARENT);
            return LRESULT(GetStockObject(WHITE_BRUSH).0 as isize);
        }
        if message == WM_MEASUREITEM {
            let item = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
            if item.CtlID as usize == NAV {
                item.itemHeight = 28;
                return LRESULT(1);
            }
        }
        if message == WM_GETMINMAXINFO {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: 940, y: 400 };
            info.ptMaxPosition = POINT { x: 0, y: 0 };
            info.ptMaxSize = POINT {
                x: meshrmm_remote_screen::background::WIDTH as i32,
                y: meshrmm_remote_screen::background::HEIGHT as i32
                    - super::background::TASKBAR_HEIGHT,
            };
            info.ptMaxTrackSize = info.ptMaxSize;
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
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
fn run_inner() -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    let _desktop = meshrmm_remote_screen::background::Desktop::bind()?;
    let _styles = controls::VisualStyles::activate()?;
    let path = PathBuf::new();
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
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
                ..Default::default()
            };
            ensure!(
                RegisterClassW(&class) != 0,
                "Could not register File Explorer window"
            );
        }
        let font = CreateFontW(
            -12,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let symbols = CreateFontW(
            -28,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe MDL2 Assets"),
        );
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            w!("MeshRMMBackgroundFiles"),
            w!("File Explorer"),
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
        for (id, text, x, width) in [(HOME, "Home", 60, 60), (VIEW, "View", 120, 60)] {
            control(
                hwnd,
                w!("BUTTON"),
                text,
                id,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [x, 0, width, 27],
                font,
            )?;
        }
        control(
            hwnd,
            w!("BUTTON"),
            "File",
            FILE_MENU,
            WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            [0, 0, 56, 27],
            font,
        )?;
        for item in RIBBON {
            control(
                hwnd,
                w!("BUTTON"),
                item.label,
                item.id,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [item.x, 28, item.width, 72],
                font,
            )?;
        }
        for (id, label, x, width) in [
            (BACK, "←", 6, 30),
            (FORWARD, "→", 40, 30),
            (UP, "↑", 76, 30),
            (GO, "→", 814, 30),
            (SEARCH_GO, "⌕", 1030, 30),
        ] {
            control(
                hwnd,
                w!("BUTTON"),
                label,
                id,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [x, 129, width, 26],
                font,
            )?;
        }
        let location = control(
            hwnd,
            w!("EDIT"),
            &location_label(&path),
            LOCATION,
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            [116, 130, 682, 24],
            font,
        )?;
        control(
            hwnd,
            w!("BUTTON"),
            "",
            ADDRESS_EDIT,
            WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            [116, 129, 600, 26],
            font,
        )?;
        let search = control(
            hwnd,
            w!("EDIT"),
            "",
            SEARCH,
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            [850, 130, 180, 24],
            font,
        )?;
        SendMessageW(
            search,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(w!("Search this folder").as_ptr() as isize)),
        );
        let nav = control(
            hwnd,
            w!("LISTBOX"),
            "",
            NAV,
            WS_TABSTOP
                | WS_VSCROLL
                | WINDOW_STYLE(
                    LBS_NOTIFY as u32
                        | LBS_NOINTEGRALHEIGHT as u32
                        | LBS_OWNERDRAWFIXED as u32
                        | LBS_HASSTRINGS as u32,
                ),
            [0, 162, 190, 450],
            font,
        )?;
        SendMessageW(nav, LB_SETITEMHEIGHT, None, Some(LPARAM(28)));
        let mut nav_paths = Vec::new();
        let public =
            PathBuf::from(std::env::var("PUBLIC").unwrap_or_else(|_| "C:\\Users\\Public".into()));
        let mut places = vec![
            ("Quick access".to_owned(), PathBuf::from("::QuickAccess")),
            ("Desktop".to_owned(), public.join("Desktop")),
            ("Downloads".to_owned(), public.join("Downloads")),
            ("Documents".to_owned(), public.join("Documents")),
            ("Pictures".to_owned(), public.join("Pictures")),
            ("This PC".to_owned(), PathBuf::new()),
        ];
        let drives = GetLogicalDrives();
        for bit in 0..26 {
            if drives & (1 << bit) != 0 {
                let drive = format!("{}:\\", (b'A' + bit) as char);
                places.push((
                    format!("Local Disk ({}:)", (b'A' + bit) as char),
                    PathBuf::from(drive),
                ));
            }
        }
        for (label, path) in places {
            SendMessageW(
                nav,
                LB_ADDSTRING,
                None,
                Some(LPARAM(wide(&label).as_ptr() as isize)),
            );
            nav_paths.push(path);
        }
        let list = control(
            hwnd,
            w!("SysListView32"),
            "",
            LIST,
            WS_TABSTOP
                | WINDOW_STYLE(
                    LVS_REPORT | LVS_SHOWSELALWAYS | LVS_EDITLABELS | LVS_SHAREIMAGELISTS,
                ),
            [194, 162, 880, 450],
            font,
        )?;
        let _ = SetWindowTheme(list, w!("Explorer"), PCWSTR::null());
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
            )),
        );
        for (flags, kind) in [
            (SHGFI_SMALLICON, LVSIL_SMALL),
            (SHGFI_LARGEICON, LVSIL_NORMAL),
        ] {
            let mut info = SHFILEINFOW::default();
            let images = SHGetFileInfoW(
                w!("C:\\"),
                FILE_ATTRIBUTE_DIRECTORY,
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES | flags,
            );
            SendMessageW(
                list,
                LVM_SETIMAGELIST,
                Some(WPARAM(kind as usize)),
                Some(LPARAM(images as isize)),
            );
        }
        for (index, (name, width)) in [
            ("Name", 340),
            ("Date modified", 160),
            ("Type", 170),
            ("Size", 100),
        ]
        .iter()
        .enumerate()
        {
            let mut text = wide(name);
            let column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                cx: *width,
                fmt: if index == 3 {
                    LVCFMT_RIGHT
                } else {
                    LVCFMT_LEFT
                },
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
        controls::install(list, font)?;
        let status = control(
            hwnd,
            w!("STATIC"),
            "Loading…",
            STATUS,
            WINDOW_STYLE(0),
            [12, 618, 1054, 22],
            font,
        )?;
        for (id, label) in [(CONFIRM, "Delete"), (CANCEL, "Cancel")] {
            control(
                hwnd,
                w!("BUTTON"),
                label,
                id,
                WS_TABSTOP,
                [0, 0, 94, 28],
                font,
            )?;
        }
        let state = Box::new(RefCell::new(State {
            hwnd,
            location,
            list,
            status,
            search,
            nav,
            crumbs: Vec::new(),
            address_edit: false,
            resizing: None,
            dragging: Vec::new(),
            control_down: false,
            undo_stack: Vec::new(),
            font,
            symbols,
            nav_icons: [SIID_FOLDER, SIID_DESKTOPPC, SIID_DRIVEFIXED]
                .into_iter()
                .map(|id| {
                    let mut info = SHSTOCKICONINFO {
                        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
                        ..Default::default()
                    };
                    let _ = SHGetStockIconInfo(id, SHGSI_ICON | SHGSI_SMALLICON, &mut info);
                    info.hIcon
                })
                .collect(),
            path: path.clone(),
            rows: Vec::new(),
            visible: Vec::new(),
            nav_paths,
            copied: Vec::new(),
            cut: false,
            clipboard_sequence: 0,
            history: History::default(),
            travel: None,
            last_click: None,
            receiver: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            sort: 0,
            descending: false,
            hidden: false,
            extensions: true,
            view_tab: false,
            searching: false,
            pending_delete: Vec::new(),
            pending_rename: None,
        }));
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*state as *const RefCell<State>) as isize,
        );
        state.borrow_mut().start(Work::List(path))?;
        state.borrow().layout();
        SetTimer(Some(hwnd), 1, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut message = MSG::default();
        let mut control_down = false;
        let mut shift_down = false;
        let mut alt_down = false;
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let key = message.wParam.0;
            if matches!(
                message.message,
                WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
            ) {
                let down = matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN);
                match key {
                    0x11 | 0xa2 | 0xa3 => {
                        control_down = down;
                        state.borrow_mut().control_down = down;
                    }
                    0x10 | 0xa0 | 0xa1 => shift_down = down,
                    0x12 | 0xa4 | 0xa5 => alt_down = down,
                    _ => {}
                }
            }
            let mut handled = false;
            if matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN)
                && GetAncestor(message.hwnd, GA_ROOT) == hwnd
            {
                let editing = message.hwnd == location
                    || message.hwnd == search
                    || SendMessageW(list, LVM_GETEDITCONTROL, None, None).0
                        == message.hwnd.0 as isize;
                let command = match key {
                    0x5a if control_down && !editing => Some(UNDO),
                    0x41 if control_down && !editing => Some(SELECT_ALL),
                    0x43 if control_down && !editing => Some(COPY),
                    0x58 if control_down && !editing => Some(CUT),
                    0x56 if control_down && !editing => Some(PASTE),
                    0x4e if control_down && shift_down && !editing => Some(NEW_FOLDER),
                    0x71 if !editing => Some(RENAME),
                    0x74 => Some(REFRESH),
                    0x2e if !editing => Some(DELETE),
                    0x25 if alt_down => Some(BACK),
                    0x27 if alt_down => Some(FORWARD),
                    0x26 if alt_down => Some(UP),
                    0x08 if !editing => Some(UP),
                    13 if alt_down => Some(PROPERTIES),
                    13 if message.hwnd == location => Some(GO),
                    13 if message.hwnd == search => Some(SEARCH_GO),
                    13 if message.hwnd == list => Some(OPEN),
                    27 if message.hwnd == search => {
                        set_text(search, "");
                        Some(SEARCH_GO)
                    }
                    27 if !editing => Some(CANCEL),
                    _ => None,
                };
                if key == 0x41 && control_down && editing {
                    SendMessageW(message.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    handled = true;
                } else if (control_down && key == 0x4c) || (alt_down && key == 0x44) || key == 0x75
                {
                    state.borrow_mut().address_edit = true;
                    state.borrow().layout();
                    let _ = SetFocus(Some(location));
                    SendMessageW(location, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    handled = true;
                } else if (control_down && matches!(key, 0x46 | 0x45)) || key == 0x72 {
                    let _ = SetFocus(Some(search));
                    SendMessageW(search, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    handled = true;
                } else if let Some(command) = command {
                    if let Err(e) = state.borrow_mut().command(command) {
                        set_text(status, &format!("{e:#}"));
                    }
                    handled = true;
                }
            }
            if !handled && !IsDialogMessageW(GetAncestor(message.hwnd, GA_ROOT), &message).as_bool()
            {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(symbols.into());
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
