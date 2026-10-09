use super::{Kind, PINS};
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR};

pub(super) struct TaskWindow {
    pub(super) window: HWND,
    pub(super) process: u32,
    pub(super) title: String,
}

/// The process that started `process`, while it's listed.
pub(super) fn parent_process(process: u32) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let snapshot =
        crate::win32::OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while next.is_ok() {
        if entry.th32ProcessID == process {
            return Some(entry.th32ParentProcessID);
        }
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    None
}

pub(super) fn task_windows(shell: HWND, tooltip: HWND) -> windows::core::Result<Vec<TaskWindow>> {
    unsafe extern "system" fn collect(hwnd: HWND, parameter: LPARAM) -> windows::core::BOOL {
        unsafe {
            let windows = &mut *(parameter.0 as *mut Vec<HWND>);
            if windows.len() < 128 {
                windows.push(hwnd);
            }
        }
        windows::core::BOOL(1)
    }
    unsafe {
        let desktop =
            windows::Win32::System::StationsAndDesktops::GetThreadDesktop(GetCurrentThreadId())?;
        let mut handles = Vec::new();
        windows::Win32::System::StationsAndDesktops::EnumDesktopWindows(
            Some(desktop),
            Some(collect),
            LPARAM((&mut handles as *mut Vec<HWND>) as isize),
        )?;
        let mut windows = Vec::new();
        for window in handles {
            if window == shell || window == tooltip || !has_taskbar_button(window) {
                continue;
            }
            let mut title = [0_u16; 256];
            let count = GetWindowTextW(window, &mut title);
            if count == 0 {
                continue;
            }
            let mut process = 0;
            GetWindowThreadProcessId(window, Some(&mut process));
            windows.push(TaskWindow {
                window,
                process,
                title: String::from_utf16_lossy(&title[..count as usize]),
            });
        }
        Ok(windows)
    }
}

/// Follows Windows' taskbar rules: a visible window that isn't a tool window gets
/// a button if it has no owner, its owner is hidden, or it has `WS_EX_APPWINDOW`.
/// Dialogs such as Run and System Properties are owned by hidden windows, and
/// could only be recovered by moving whatever covered them.
fn has_taskbar_button(window: HWND) -> bool {
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
        let ex_style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        if (style & WS_VISIBLE.0 == 0 && !IsIconic(window).as_bool())
            || ex_style & WS_EX_TOOLWINDOW.0 != 0
        {
            return false;
        }
        if ex_style & WS_EX_APPWINDOW.0 == 0
            && let Ok(owner) = GetWindow(window, GW_OWNER)
            && GetWindowLongPtrW(owner, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0
        {
            return false;
        }
        let mut class = [0_u16; 64];
        let length = GetClassNameW(window, &mut class) as usize;
        !matches!(
            String::from_utf16_lossy(&class[..length]).as_str(),
            "#32768" | "tooltips_class32"
        )
    }
}

pub(super) fn task_icon(window: HWND, process_id: u32, shell: HWND) -> HICON {
    unsafe {
        let mut class = [0_u16; 64];
        let length = GetClassNameW(window, &mut class) as usize;
        let pinned = match String::from_utf16_lossy(&class[..length]).as_str() {
            "MeshRMMBackgroundTasks" => Some(Kind::TaskManager),
            "MeshRMMBackgroundFiles" => Some(Kind::FileExplorer),
            "MeshRMMBackgroundRun" => Some(Kind::Run),
            _ => None,
        };
        if let Some(index) = pinned.and_then(|kind| PINS.iter().position(|pin| pin.kind == kind))
            && let Ok(button) = GetDlgItem(Some(shell), index as i32 + 1)
        {
            let source = HICON(GetWindowLongPtrW(button, GWLP_USERDATA) as *mut _);
            if !source.is_invalid()
                && let Ok(icon) = CopyIcon(source)
            {
                return icon;
            }
        }
        for size in [ICON_SMALL2, ICON_SMALL, ICON_BIG] {
            let mut result = 0;
            SendMessageTimeoutW(
                window,
                WM_GETICON,
                WPARAM(size as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                20,
                Some(&mut result),
            );
            if result != 0
                && let Ok(icon) = CopyIcon(HICON(result as *mut _))
            {
                return icon;
            }
        }
        for index in [GCLP_HICONSM, GCLP_HICON] {
            let source = GetClassLongPtrW(window, index);
            if source != 0
                && let Ok(icon) = CopyIcon(HICON(source as *mut _))
            {
                return icon;
            }
        }
        if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
            let mut path = vec![0_u16; 32768];
            let mut length = path.len() as u32;
            let found = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
            .is_ok();
            let _ = CloseHandle(process);
            if found {
                let mut icon = HICON::default();
                ExtractIconExW(PCWSTR(path.as_ptr()), 0, Some(&mut icon), None, 1);
                if !icon.is_invalid() {
                    return icon;
                }
            }
        }
        LoadIconW(None, IDI_APPLICATION)
            .and_then(|icon| CopyIcon(icon))
            .unwrap_or_default()
    }
}
