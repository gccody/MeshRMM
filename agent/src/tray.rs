//! Notification-area UI. Runs as the signed-in user, without agent config.
//! During a remote session its icon opens the session chat, which the session
//! notice window owns, possibly in a LocalSystem helper on the same desktop.
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const ICON: &[u8] = include_bytes!("../assets/tray.ico");
const ICON_ID: u32 = 1;
const ICON_CALLBACK: u32 = WM_APP + 1;
pub(crate) const TRAY_CLASS: PCWSTR = w!("MeshRMMAgentTray");
pub(crate) const SESSION_CLASS: PCWSTR = w!("MeshRMMSessionIndicator");
/// Tray to session command: toggle the chat popup. `LPARAM` carries the
/// click point as two signed 16-bit screen coordinates.
pub(crate) const CHAT_TOGGLE: usize = 1;
/// Tray to session command: report chat availability again.
pub(crate) const CHAT_STATUS_REQUEST: usize = 2;
static CHAT_AVAILABLE: AtomicBool = AtomicBool::new(false);
static LAST_KEY_SELECT: AtomicI32 = AtomicI32::new(0);

/// Sent by the tray to session windows; `WPARAM` is `CHAT_TOGGLE` or
/// `CHAT_STATUS_REQUEST`.
pub(crate) fn chat_command_message() -> u32 {
    unsafe { RegisterWindowMessageW(w!("MeshRMMSessionChatCommand")) }
}

/// Sent by a session window to the tray; `WPARAM` is 1 while chat is available.
pub(crate) fn chat_status_message() -> u32 {
    unsafe { RegisterWindowMessageW(w!("MeshRMMSessionChatStatus")) }
}

/// Top-level windows of `class` on the calling thread's desktop.
pub(crate) fn windows_of_class(class: PCWSTR) -> Vec<HWND> {
    let mut windows = Vec::new();
    let mut after = None;
    while let Ok(hwnd) = unsafe { FindWindowExW(None, after, class, PCWSTR::null()) } {
        windows.push(hwnd);
        after = Some(hwnd);
    }
    windows
}

/// Where the Agent icon is, for popups opened without a click.
pub(crate) fn icon_rect() -> Option<RECT> {
    windows_of_class(TRAY_CLASS).into_iter().find_map(|hwnd| {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: hwnd,
            uID: ICON_ID,
            ..Default::default()
        };
        unsafe { Shell_NotifyIconGetRect(&identifier) }.ok()
    })
}

pub fn run() -> anyhow::Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let name = std::env::args()
        .nth(2)
        .ok_or_else(|| anyhow::anyhow!("missing service name"))?;
    anyhow::ensure!(
        name == crate::service::SERVICE_NAME || name == crate::service::LEGACY_SERVICE_NAME,
        "invalid service name"
    );
    let owner: u32 = std::env::args()
        .nth(3)
        .ok_or_else(|| anyhow::anyhow!("missing service PID"))?
        .parse()?;
    // A remote session ended by sign-out or restart cannot restore the wallpaper
    // it hid. Retry while Explorer's shell is still starting after sign-in.
    std::thread::spawn(|| {
        for _ in 0..60 {
            if crate::remote::wallpaper::restore_interrupted().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = TRAY_CLASS;
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        anyhow::ensure!(
            RegisterClassW(&definition) != 0,
            "tray window registration failed"
        );
        // A hidden top-level window receives Explorer's TaskbarCreated broadcast.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("MeshRMM Agent"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let icon = load_icon()?;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, icon.0 as isize);
        // Retry while Explorer is starting, and recover after Explorer restarts.
        SetTimer(Some(hwnd), 1, 2_000, None);
        add_icon(hwnd, icon);
        // Recover the chat state of a session that outlived a previous tray.
        post_to_sessions(CHAT_STATUS_REQUEST, LPARAM(0));
        let window = hwnd.0 as usize;
        std::thread::spawn(move || {
            // SCM handles are thread-bound; open and query on the monitor thread.
            if let Ok(service) =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                    .and_then(|manager| manager.open_service(name, ServiceAccess::QUERY_STATUS))
            {
                while service.query_status().is_ok_and(|status| {
                    status.current_state == ServiceState::Running
                        && status.process_id == Some(owner)
                }) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            let _ = PostMessageW(Some(HWND(window as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        });
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = DestroyWindow(hwnd);
        let _ = DestroyIcon(icon);
    }
    Ok(())
}

fn load_icon() -> anyhow::Result<HICON> {
    // The first ICO image is the tray asset; no installer/resource compiler needed.
    anyhow::ensure!(
        ICON.len() >= 22 && ICON[..6] == [0, 0, 1, 0, 1, 0],
        "tray.ico must contain one image"
    );
    let length = u32::from_le_bytes(ICON[14..18].try_into()?) as usize;
    let offset = u32::from_le_bytes(ICON[18..22].try_into()?) as usize;
    let data = offset
        .checked_add(length)
        .and_then(|end| ICON.get(offset..end))
        .ok_or_else(|| anyhow::anyhow!("invalid tray.ico image range"))?;
    Ok(unsafe { CreateIconFromResourceEx(data, true, 0x0003_0000, 0, 0, LR_DEFAULTCOLOR) }?)
}

fn tooltip(chat_available: bool) -> &'static str {
    if chat_available {
        "MeshRMM Agent — click to chat with the remote viewer"
    } else {
        "MeshRMM Agent is running"
    }
}

fn notification(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        // Version 4 hides the standard tooltip unless NIF_SHOWTIP asks for it.
        uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE | NIF_SHOWTIP,
        uCallbackMessage: ICON_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    for (slot, character) in data
        .szTip
        .iter_mut()
        .take(127)
        .zip(tooltip(CHAT_AVAILABLE.load(Ordering::Relaxed)).encode_utf16())
    {
        *slot = character;
    }
    data
}

unsafe fn add_icon(hwnd: HWND, icon: HICON) {
    let data = notification(hwnd, icon);
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        // Version 4 reports keyboard selection and the icon's anchor point.
        let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
        let _ = unsafe { KillTimer(Some(hwnd), 1) };
    }
}

fn post_to_sessions(command: usize, lp: LPARAM) -> bool {
    let message = chat_command_message();
    let sessions = windows_of_class(SESSION_CLASS);
    for &session in &sessions {
        unsafe {
            let mut process = 0;
            if command == CHAT_TOGGLE && GetWindowThreadProcessId(session, Some(&mut process)) != 0
            {
                // The shell lets the icon owner take the foreground after a
                // click; pass that on so the user can type immediately.
                let _ = AllowSetForegroundWindow(process);
            }
            let _ = PostMessageW(Some(session), message, WPARAM(command), lp);
        }
    }
    !sessions.is_empty()
}

unsafe fn set_chat_available(hwnd: HWND, icon: HICON, available: bool) {
    if CHAT_AVAILABLE.swap(available, Ordering::Relaxed) != available && !icon.is_invalid() {
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &notification(hwnd, icon)) };
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let icon = HICON(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut _);
        if message == WM_TIMER || (taskbar_created != 0 && message == taskbar_created) {
            if !icon.is_invalid() {
                SetTimer(Some(hwnd), 1, 2_000, None);
                add_icon(hwnd, icon);
            }
            return LRESULT(0);
        }
        let chat_status = chat_status_message();
        if chat_status != 0 && message == chat_status {
            set_chat_available(hwnd, icon, wp.0 == 1);
            return LRESULT(0);
        }
        if message == ICON_CALLBACK {
            // Version 4: the event is in the low word, the anchor point in WPARAM.
            let event = lp.0 as u32 & 0xffff;
            let key = event == NIN_SELECT | NINF_KEY;
            // Enter reports the keyboard selection twice; one press is one toggle.
            let repeated = key && {
                let time = GetMessageTime();
                time.wrapping_sub(LAST_KEY_SELECT.swap(time, Ordering::Relaxed)) < 200
            };
            if (event == NIN_SELECT || key)
                && !repeated
                && !post_to_sessions(CHAT_TOGGLE, LPARAM(wp.0 as u32 as isize))
            {
                // A session helper that exited abruptly never reported its end.
                set_chat_available(hwnd, icon, false);
            }
            return LRESULT(0);
        }
        match message {
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &notification(hwnd, icon));
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wp, lp),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_icon_loads() {
        let icon = super::load_icon().expect("embedded tray icon must be a valid Windows icon");
        unsafe { super::DestroyIcon(icon).unwrap() };
    }
}
