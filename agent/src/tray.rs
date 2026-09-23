//! Passive notification-area UI. Runs as the signed-in user, without agent config.
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

const ICON: &[u8] = include_bytes!("../assets/tray.ico");

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
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MeshRMMAgentTray");
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

fn notification(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP,
        hIcon: icon,
        ..Default::default()
    };
    for (slot, character) in data
        .szTip
        .iter_mut()
        .zip("MeshRMM Agent is running".encode_utf16())
    {
        *slot = character;
    }
    data
}

unsafe fn add_icon(hwnd: HWND, icon: HICON) {
    if unsafe { Shell_NotifyIconW(NIM_ADD, &notification(hwnd, icon)) }.as_bool() {
        let _ = unsafe { KillTimer(Some(hwnd), 1) };
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
