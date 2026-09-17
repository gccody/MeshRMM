//! GDI process manager for the isolated desktop; no interactive-user shell is needed.
use anyhow::{Context, ensure};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

const LIST: usize = 101;
const REFRESH: usize = 102;
const END_TASK: usize = 103;

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Clone)]
struct Process {
    pid: u32,
    name: String,
    session: Option<u32>,
    memory: Option<usize>,
    created: Option<u64>,
}

fn creation(process: HANDLE) -> anyhow::Result<u64> {
    let (mut created, mut exit, mut kernel, mut user) = Default::default();
    unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user)? };
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn processes() -> anyhow::Result<Vec<Process>> {
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut rows = Vec::new();
    unsafe { Process32FirstW(snapshot.0, &mut entry)? };
    loop {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let mut row = Process {
            pid: entry.th32ProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..end]),
            session: None,
            memory: None,
            created: None,
        };
        let mut session = 0;
        if unsafe { ProcessIdToSessionId(row.pid, &mut session) }.is_ok() {
            row.session = Some(session);
        }
        if let Ok(handle) =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, row.pid) }
        {
            let handle = Handle(handle);
            row.created = creation(handle.0).ok();
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            if unsafe {
                GetProcessMemoryInfo(
                    handle.0,
                    &mut counters,
                    std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                )
            }
            .is_ok()
            {
                row.memory = Some(counters.WorkingSetSize);
            }
        }
        rows.push(row);
        if let Err(error) = unsafe { Process32NextW(snapshot.0, &mut entry) } {
            if error.code() != ERROR_NO_MORE_FILES.to_hresult() {
                return Err(error.into());
            }
            break;
        }
    }
    rows.sort_by_cached_key(|row| (row.name.to_lowercase(), row.pid));
    Ok(rows)
}

// Hold the validated handle across the confirmation dialog: a recycled PID can
// never redirect End Task to a different process.
fn termination_handle(row: &Process) -> anyhow::Result<Handle> {
    ensure!(
        row.pid != std::process::id(),
        "The process manager cannot end itself."
    );
    let expected = row
        .created
        .context("This process cannot be safely identified. Refresh the list.")?;
    let handle = Handle(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            false,
            row.pid,
        )?
    });
    ensure!(
        creation(handle.0)? == expected,
        "The process has exited. Refresh the list."
    );
    let mut critical = windows::core::BOOL(0);
    unsafe { IsProcessCritical(handle.0, &mut critical)? };
    ensure!(
        !critical.as_bool(),
        "Windows marks this process as critical; it cannot be ended here."
    );
    Ok(handle)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct State {
    list: HWND,
    status: HWND,
    rows: Vec<Process>,
    pending: Option<Handle>,
    notice: Option<String>,
    receiver: Option<std::sync::mpsc::Receiver<anyhow::Result<Vec<Process>>>>,
}

impl State {
    fn refresh(&mut self) {
        if self.receiver.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        self.receiver = Some(receiver);
        // Process queries must not stall painting or input on the UI thread.
        std::thread::spawn(move || {
            let _ = sender.send(processes());
        });
    }

    fn poll(&mut self) -> anyhow::Result<()> {
        let Some(receiver) = &self.receiver else {
            return Ok(());
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(error) => Err(error.into()),
        };
        self.receiver = None;
        let rows = result?;
        unsafe {
            let selected = SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
            .0;
            let pid = self.rows.get(selected as usize).map(|row| row.pid);
            let top = SendMessageW(self.list, LVM_GETTOPINDEX, None, None).0;
            let mut bounds = RECT::default();
            SendMessageW(
                self.list,
                LVM_GETITEMRECT,
                Some(WPARAM(top as usize)),
                Some(LPARAM((&mut bounds as *mut RECT) as isize)),
            );
            let anchor = self.rows.get(top as usize).map(|row| row.pid);
            let new_top = anchor
                .and_then(|pid| rows.iter().position(|row| row.pid == pid))
                .unwrap_or(top as usize);
            let scroll = (new_top as isize - top)
                .saturating_mul((bounds.bottom - bounds.top).max(0) as isize);
            SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(0)), None);
            // Update rows in place. Deleting the whole list resets its scroll
            // range and can move a different process underneath the pointer.
            for index in (rows.len()..self.rows.len()).rev() {
                SendMessageW(self.list, LVM_DELETEITEM, Some(WPARAM(index)), None);
            }
            for (index, row) in rows.iter().enumerate() {
                let values = [
                    row.name.clone(),
                    row.pid.to_string(),
                    row.session.map_or_else(|| "—".into(), |s| s.to_string()),
                    row.memory
                        .map_or_else(|| "—".into(), |m| format!("{:.1} MB", m as f64 / 1048576.0)),
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
                        if column == 0 && index >= self.rows.len() {
                            LVM_INSERTITEMW
                        } else {
                            LVM_SETITEMW
                        },
                        None,
                        Some(LPARAM((&item as *const LVITEMW) as isize)),
                    );
                }
            }
            let mut selection = LVITEMW {
                stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                ..Default::default()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM((&selection as *const LVITEMW) as isize)),
            );
            if let Some(index) = pid.and_then(|pid| rows.iter().position(|row| row.pid == pid)) {
                selection.state = selection.stateMask;
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    Some(WPARAM(index)),
                    Some(LPARAM((&selection as *const LVITEMW) as isize)),
                );
            }
            SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(1)), None);
            SendMessageW(self.list, LVM_SCROLL, None, Some(LPARAM(scroll)));
            let _ = InvalidateRect(Some(self.list), None, true);
            let status = wide(&self.notice.clone().unwrap_or_else(|| {
                format!(
                    "{} processes · SYSTEM · refreshes every 2 seconds",
                    rows.len()
                )
            }));
            SetWindowTextW(self.status, PCWSTR(status.as_ptr()))?;
            if let Ok(parent) = GetParent(self.list) {
                let _ = RedrawWindow(
                    Some(parent),
                    None,
                    None,
                    RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW | RDW_FRAME,
                );
            }
        }
        self.rows = rows;
        Ok(())
    }

    fn selected(&self) -> anyhow::Result<Process> {
        let selected = unsafe {
            SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
        }
        .0;
        let row = self
            .rows
            .get(selected as usize)
            .context("Select a process first.")?;
        Ok(row.clone())
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if !state.is_null()
            && (message == WM_TIMER
                || message == WM_SIZE
                || (message == WM_COMMAND && matches!(wparam.0 & 0xffff, REFRESH | END_TASK)))
        {
            // List-view messages synchronously notify the parent. Ignore those
            // notifications without borrowing State during an active update.
            let state = &mut *state;
            let result = match message {
                WM_TIMER => {
                    if wparam.0 == 1 {
                        state.refresh();
                    }
                    state.poll()
                }
                WM_COMMAND if wparam.0 & 0xffff == REFRESH => {
                    state.pending = None;
                    state.notice = None;
                    state.refresh();
                    Ok(())
                }
                WM_COMMAND if wparam.0 & 0xffff == END_TASK => {
                    if let Some(handle) = state.pending.take() {
                        let result = TerminateProcess(handle.0, 1).map_err(anyhow::Error::from);
                        state.notice = Some("Process ended.".into());
                        state.refresh();
                        result
                    } else {
                        state.selected().and_then(|row| {
                            state.pending = Some(termination_handle(&row)?);
                            state.notice = Some(format!(
                                "End {} (PID {})? Unsaved work will be lost.",
                                row.name, row.pid
                            ));
                            Ok(())
                        })
                    }
                }
                WM_SIZE => {
                    let width = (lparam.0 & 0xffff) as i32;
                    let height = ((lparam.0 >> 16) & 0xffff) as i32;
                    let _ = MoveWindow(
                        state.list,
                        10,
                        10,
                        (width - 20).max(1),
                        (height - 64).max(1),
                        true,
                    );
                    let _ = MoveWindow(
                        state.status,
                        10,
                        (height - 36).max(0),
                        (width - 260).max(1),
                        24,
                        true,
                    );
                    if let Ok(button) = GetDlgItem(Some(hwnd), REFRESH as i32) {
                        let _ = MoveWindow(button, width - 234, height - 40, 104, 30, true);
                    }
                    if let Ok(button) = GetDlgItem(Some(hwnd), END_TASK as i32) {
                        let _ = MoveWindow(button, width - 120, height - 40, 110, 30, true);
                    }
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                state.notice = Some(format!("{error:#}"));
            }
            if message == WM_COMMAND {
                let text = wide(state.notice.as_deref().unwrap_or("Refreshing…"));
                let _ = SetWindowTextW(state.status, PCWSTR(text.as_ptr()));
                if let Ok(button) = GetDlgItem(Some(hwnd), REFRESH as i32) {
                    let _ = SetWindowTextW(
                        button,
                        if state.pending.is_some() {
                            w!("Cancel")
                        } else {
                            w!("Refresh")
                        },
                    );
                }
                if let Ok(button) = GetDlgItem(Some(hwnd), END_TASK as i32) {
                    let _ = SetWindowTextW(
                        button,
                        if state.pending.is_some() {
                            w!("Confirm end")
                        } else {
                            w!("End task")
                        },
                    );
                }
            }
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub fn run() -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    let _desktop = meshrmm_remote_screen::background::Desktop::bind()?;
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        })
        .ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: w!("MeshRMMBackgroundTasks"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut _),
            ..Default::default()
        };
        ensure!(
            RegisterClassW(&class) != 0,
            "Could not register the process manager window"
        );
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            class.lpszClassName,
            w!("MeshRMM Task Manager"),
            WS_OVERLAPPEDWINDOW,
            40,
            24,
            1000,
            650,
            None,
            None,
            None,
            None,
        )?;
        let list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("SysListView32"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
            10,
            10,
            950,
            535,
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
        for (index, (label, width)) in [
            ("Process", 460),
            ("PID", 110),
            ("Session", 110),
            ("Memory", 180),
        ]
        .iter()
        .enumerate()
        {
            let mut text = wide(label);
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
        let status = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Loading processes…"),
            WS_CHILD | WS_VISIBLE,
            10,
            570,
            650,
            24,
            Some(hwnd),
            None,
            None,
            None,
        )?;
        for (id, label, x) in [(REFRESH, "Refresh", 750), (END_TASK, "End task", 864)] {
            let text = wide(label);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                x,
                565,
                104,
                30,
                Some(hwnd),
                Some(HMENU(id as *mut _)),
                None,
                None,
            )?;
        }
        let mut state = Box::new(State {
            list,
            status,
            rows: Vec::new(),
            pending: None,
            notice: None,
            receiver: None,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *state as *mut State) as isize);
        state.refresh();
        SetTimer(Some(hwnd), 1, 2000, None);
        SetTimer(Some(hwnd), 2, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            if !IsDialogMessageW(hwnd, &message).as_bool() {
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
    #[test]
    fn refresh_preserves_selected_process_and_scroll_anchor() {
        unsafe {
            InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_LISTVIEW_CLASSES,
            })
            .ok()
            .unwrap();
            let parent = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                800,
                300,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let list = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("SysListView32"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL),
                0,
                0,
                800,
                200,
                Some(parent),
                None,
                None,
                None,
            )
            .unwrap();
            let status = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WS_CHILD,
                0,
                210,
                800,
                30,
                Some(parent),
                None,
                None,
                None,
            )
            .unwrap();
            let mut state = State {
                list,
                status,
                rows: Vec::new(),
                pending: None,
                notice: None,
                receiver: None,
            };
            let rows: Vec<_> = (0..100)
                .map(|pid| Process {
                    pid,
                    name: format!("Process {pid:03}"),
                    session: Some(0),
                    memory: Some(1024),
                    created: Some(1),
                })
                .collect();
            let (sender, receiver) = std::sync::mpsc::channel();
            sender.send(Ok(rows.clone())).unwrap();
            state.receiver = Some(receiver);
            state.poll().unwrap();
            let item = LVITEMW {
                state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_SETITEMSTATE,
                Some(WPARAM(70)),
                Some(LPARAM((&item as *const LVITEMW) as isize)),
            );
            SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(70)), Some(LPARAM(0)));
            let top = SendMessageW(list, LVM_GETTOPINDEX, None, None).0;
            assert!(top > 0);
            let mut changed = rows;
            changed.insert(
                0,
                Process {
                    pid: 999,
                    name: "New process".into(),
                    session: Some(0),
                    memory: None,
                    created: None,
                },
            );
            let (sender, receiver) = std::sync::mpsc::channel();
            sender.send(Ok(changed)).unwrap();
            state.receiver = Some(receiver);
            state.poll().unwrap();
            let selected = state.selected().unwrap().pid;
            let refreshed_top = SendMessageW(list, LVM_GETTOPINDEX, None, None).0;
            DestroyWindow(parent).unwrap();
            assert_eq!(selected, 70);
            assert_eq!(refreshed_top, top + 1);
        }
    }

    #[test]
    fn process_snapshot_includes_self_and_rejects_self_termination() {
        let rows = processes().unwrap();
        let row = rows
            .iter()
            .find(|row| row.pid == std::process::id())
            .unwrap();
        assert!(row.created.is_some());
        assert!(termination_handle(row).is_err());
    }
    #[test]
    fn end_task_rejects_recycled_process_identity() {
        let child = std::process::Command::new("cmd.exe")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = child;
        let result = || {
            let mut rows = processes().unwrap();
            let row = rows.iter_mut().find(|row| row.pid == child.id()).unwrap();
            row.created = Some(row.created.unwrap().wrapping_add(1));
            assert!(termination_handle(row).is_err());
            row.created = Some(row.created.unwrap().wrapping_sub(1));
            let handle = termination_handle(row).unwrap();
            unsafe {
                TerminateProcess(handle.0, 1).unwrap();
            }
        };
        result();
        let _ = child.kill();
        child.wait().unwrap();
    }
}
