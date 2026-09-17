//! Experimental Session 0 GUI input and application launcher.
//! No SendInput, desktop switching, console hooks, or user-token launches.
use anyhow::Context;
use meshrmm_protocol::{PointerButton, RemoteInput};
use meshrmm_remote_screen::background::{self, HEIGHT, WIDTH};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

const TASKBAR_HEIGHT: i32 = 72;
const PIN_WIDTH: i32 = 116;
const PINS: &[(&str, &str, &str)] = &[
    (
        "Command Prompt",
        "cmd.exe",
        "/k title MeshRMM Background Command Prompt",
    ),
    (
        "PowerShell",
        "WindowsPowerShell\\v1.0\\powershell.exe",
        "-NoLogo -NoProfile -NoExit",
    ),
    ("Registry Editor", "..\\regedit.exe", "/m"),
    ("Services", "mmc.exe", "services.msc"),
    ("Event Viewer", "mmc.exe", "eventvwr.msc"),
    ("Resource Monitor", "resmon.exe", ""),
    ("Task Manager", "taskmgr.exe", ""),
    ("Computer Mgmt", "mmc.exe", "compmgmt.msc"),
    ("Device Manager", "mmc.exe", "devmgmt.msc"),
    ("Firewall", "mmc.exe", "wf.msc"),
];

pub struct Workspace {
    icons: Vec<HICON>,
    shell: HWND,
    job: HANDLE,
    focus: HWND,
    pointer: POINT,
    pressed: Option<HWND>,
    drag: Option<(HWND, POINT, RECT)>,
    keys: [u8; 256],
    attached_thread: Option<u32>,
    console_inputs: Vec<super::background_console::ConsoleInput>,
}

impl Workspace {
    pub fn new() -> anyhow::Result<Self> {
        background::require_session_zero()?;
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null())?;
            let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };
            if let Err(error) = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) {
                let _ = CloseHandle(job);
                return Err(error.into());
            }
            let mut workspace = Self {
                icons: Vec::new(),
                shell: HWND::default(),
                job,
                focus: HWND::default(),
                pointer: POINT::default(),
                pressed: None,
                drag: None,
                keys: [0; 256],
                attached_thread: None,
                console_inputs: Vec::new(),
            };
            let class = WNDCLASSW {
                lpfnWndProc: Some(launcher_proc),
                lpszClassName: w!("MeshRMMBackgroundLauncher"),
                hbrBackground: HBRUSH(GetStockObject(DKGRAY_BRUSH).0),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
            workspace.shell = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("MeshRMMBackgroundLauncher"),
                w!("MeshRMM Background — Session 0 (SYSTEM)"),
                WS_POPUP | WS_VISIBLE,
                0,
                HEIGHT as i32 - TASKBAR_HEIGHT,
                WIDTH as i32,
                TASKBAR_HEIGHT,
                None,
                None,
                None,
                None,
            )?;
            let root = std::env::var("SystemRoot").context("SystemRoot is unavailable")?;
            for (index, (label, program, _)) in PINS.iter().enumerate() {
                let label = wide(label);
                let button = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    PCWSTR(label.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_OWNERDRAW as u32),
                    8 + index as i32 * PIN_WIDTH,
                    4,
                    PIN_WIDTH - 4,
                    TASKBAR_HEIGHT - 8,
                    Some(workspace.shell),
                    Some(HMENU((index + 1) as *mut _)),
                    None,
                    None,
                )?;
                let path = wide(&format!("{root}\\System32\\{program}"));
                let mut icon = HICON::default();
                ExtractIconExW(PCWSTR(path.as_ptr()), 0, Some(&mut icon), None, 1);
                if !icon.is_invalid() {
                    SetWindowLongPtrW(button, GWLP_USERDATA, icon.0 as isize);
                    workspace.icons.push(icon);
                }
            }
            Ok(workspace)
        }
    }

    fn launch(&mut self, index: usize) -> anyhow::Result<()> {
        let (_, program, arguments) = PINS
            .get(index.wrapping_sub(1))
            .context("unknown background application")?;
        let root = std::env::var("SystemRoot").context("SystemRoot is unavailable")?;
        let executable = wide(&format!("{root}\\System32\\{program}"));
        let mut command = wide(&format!("\"{root}\\System32\\{program}\" {arguments}"));
        let mut desktop = wide(&background::desktop_path()?);
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USEPOSITION | STARTF_USESIZE,
            dwX: 40,
            dwY: 24,
            dwXSize: 1100,
            dwYSize: (HEIGHT as i32 - TASKBAR_HEIGHT - 48) as u32,
            ..Default::default()
        };
        unsafe {
            let mut info = PROCESS_INFORMATION::default();
            CreateProcessW(
                PCWSTR(executable.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_SUSPENDED | CREATE_NEW_CONSOLE,
                None,
                PCWSTR::null(),
                &startup,
                &mut info,
            )?;
            let result = AssignProcessToJobObject(self.job, info.hProcess);
            if result.is_err() || ResumeThread(info.hThread) == u32::MAX {
                let _ = TerminateProcess(info.hProcess, 1);
                let _ = CloseHandle(info.hThread);
                let _ = CloseHandle(info.hProcess);
                result?;
                anyhow::bail!("could not resume background application");
            }
            let _ = CloseHandle(info.hThread);
            let _ = CloseHandle(info.hProcess);
            if index == 1 || index == 2 {
                self.console_inputs
                    .push(super::background_console::ConsoleInput::start(
                        info.dwProcessId,
                        self.job,
                    )?);
            }
            tracing::info!(
                session_id = 0,
                process_id = info.dwProcessId,
                program,
                "background application started"
            );
        }
        Ok(())
    }

    pub fn pump(&self) {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn move_pointer(&mut self, x: u16, y: u16) {
        self.pointer = POINT {
            x: i32::from(x) * (WIDTH as i32 - 1) / 65535,
            y: i32::from(y) * (HEIGHT as i32 - 1) / 65535,
        };
        if let Some((hwnd, origin, rect)) = self.drag {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left + self.pointer.x - origin.x,
                    rect.top + self.pointer.y - origin.y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
    }

    fn hit(&self) -> Option<HWND> {
        unsafe {
            for top in background::windows().ok()? {
                let mut rect = RECT::default();
                if GetWindowRect(top, &mut rect).is_err()
                    || self.pointer.x < rect.left
                    || self.pointer.x >= rect.right
                    || self.pointer.y < rect.top
                    || self.pointer.y >= rect.bottom
                {
                    continue;
                }
                let mut child = top;
                for _ in 0..32 {
                    let mut point = self.pointer;
                    let _ = ScreenToClient(child, &mut point);
                    let next = ChildWindowFromPointEx(
                        child,
                        point,
                        CWP_SKIPDISABLED | CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT,
                    );
                    if next.is_invalid() || next == child {
                        break;
                    }
                    child = next;
                }
                return Some(child);
            }
        }
        None
    }

    fn post(&self, hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> anyhow::Result<()> {
        if hwnd.is_invalid() {
            return Ok(());
        }
        // All targets originate from enumeration of this helper's desktop.
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Ok(());
            }
            let result = PostMessageW(Some(hwnd), message, WPARAM(wparam), LPARAM(lparam));
            // A close-button down event can destroy the target before button up.
            if result.is_err() && !IsWindow(Some(hwnd)).as_bool() {
                return Ok(());
            }
            result?;
        }
        Ok(())
    }

    fn client_point(&self, hwnd: HWND) -> isize {
        let mut point = self.pointer;
        unsafe {
            let _ = ScreenToClient(hwnd, &mut point);
        }
        pack(point)
    }

    fn button(&mut self, button: PointerButton, down: bool) -> anyhow::Result<()> {
        if !down && self.drag.take().is_some() {
            return Ok(());
        }
        let Some(hwnd) = (if down {
            self.hit()
        } else {
            self.pressed.take().or_else(|| self.hit())
        }) else {
            return Ok(());
        };
        unsafe {
            if GetParent(hwnd).ok() == Some(self.shell) {
                if down && button == PointerButton::Left {
                    self.launch(GetDlgCtrlID(hwnd) as usize)?;
                }
                return Ok(());
            }
            if down {
                self.focus = hwnd;
                self.pressed = Some(hwnd);
                let top = GetAncestor(hwnd, GA_ROOT);
                let _ = SetWindowPos(
                    top,
                    Some(HWND_TOP),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let mut hit = 0;
                SendMessageTimeoutW(
                    top,
                    WM_NCHITTEST,
                    WPARAM(0),
                    LPARAM(pack(self.pointer)),
                    SMTO_ABORTIFHUNG,
                    20,
                    Some(&mut hit),
                );
                if button == PointerButton::Left {
                    match hit as u32 {
                        HTCAPTION => {
                            let mut rect = RECT::default();
                            GetWindowRect(top, &mut rect)?;
                            self.drag = Some((top, self.pointer, rect));
                            return Ok(());
                        }
                        HTCLOSE => return self.post(top, WM_CLOSE, 0, 0),
                        HTMAXBUTTON => {
                            return self.post(
                                top,
                                WM_SYSCOMMAND,
                                if IsZoomed(top).as_bool() {
                                    SC_RESTORE
                                } else {
                                    SC_MAXIMIZE
                                } as usize,
                                0,
                            );
                        }
                        _ => {}
                    }
                }
                let thread = GetWindowThreadProcessId(hwnd, None);
                let ours = GetCurrentThreadId();
                if self.attached_thread != Some(thread) {
                    if let Some(previous) = self.attached_thread.take() {
                        let _ = AttachThreadInput(ours, previous, false);
                    }
                    if thread != ours && AttachThreadInput(ours, thread, true).as_bool() {
                        self.attached_thread = Some(thread);
                    }
                }
                let _ = SetFocus(Some(hwnd));
            }
        }
        let (message, mask) = match (button, down) {
            (PointerButton::Left, true) => (WM_LBUTTONDOWN, 1),
            (PointerButton::Left, false) => (WM_LBUTTONUP, 0),
            (PointerButton::Right, true) => (WM_RBUTTONDOWN, 2),
            (PointerButton::Right, false) => (WM_RBUTTONUP, 0),
            (PointerButton::Middle, true) => (WM_MBUTTONDOWN, 16),
            (PointerButton::Middle, false) => (WM_MBUTTONUP, 0),
            _ => return Ok(()),
        };
        self.post(hwnd, message, mask, self.client_point(hwnd))
    }

    pub fn release(&mut self) {
        for console in &self.console_inputs {
            console.release();
        }
        if let Some(hwnd) = self.pressed.take() {
            for message in [WM_LBUTTONUP, WM_RBUTTONUP, WM_MBUTTONUP] {
                let _ = self.post(hwnd, message, 0, self.client_point(hwnd));
            }
        }
        self.keys = [0; 256];
        unsafe {
            let _ = SetKeyboardState(&self.keys);
            if let Some(thread) = self.attached_thread.take() {
                let _ = AttachThreadInput(GetCurrentThreadId(), thread, false);
            }
        }
        self.drag = None;
    }

    pub fn apply(&mut self, event: RemoteInput) -> anyhow::Result<()> {
        if event.display_id().0 != background::DISPLAY_ID {
            return Ok(());
        }
        if matches!(
            event,
            RemoteInput::Key { .. } | RemoteInput::TypeText { .. }
        ) {
            let root = unsafe { GetAncestor(self.keyboard_target(), GA_ROOT) };
            if let Some(console) = self
                .console_inputs
                .iter()
                .find(|console| console.window == root)
            {
                return console.apply(event);
            }
        }
        match event {
            RemoteInput::PointerMove { x, y, .. } => {
                self.move_pointer(x, y);
                if let Some(hwnd) = self.pressed.or_else(|| self.hit()) {
                    self.post(
                        hwnd,
                        WM_MOUSEMOVE,
                        usize::from(self.pressed.is_some()),
                        self.client_point(hwnd),
                    )?;
                }
            }
            RemoteInput::PointerButtonAt {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                self.move_pointer(x, y);
                self.button(button, pressed)?;
            }
            RemoteInput::PointerButton {
                button, pressed, ..
            } => self.button(button, pressed)?,
            RemoteInput::WheelAt {
                x,
                y,
                horizontal,
                vertical,
                ..
            } => {
                self.move_pointer(x, y);
                self.wheel(horizontal, vertical)?;
            }
            RemoteInput::Wheel {
                horizontal,
                vertical,
                ..
            } => self.wheel(horizontal, vertical)?,
            RemoteInput::TypeText { text, .. } => {
                for character in text.encode_utf16() {
                    self.post(self.keyboard_target(), WM_CHAR, character as usize, 1)?;
                }
            }
            RemoteInput::Key {
                scan_code,
                extended,
                pressed,
                ..
            } => {
                let scan = u32::from(scan_code) | if extended { 0xe000 } else { 0 };
                let key = unsafe { MapVirtualKeyW(scan, MAPVK_VSC_TO_VK_EX) } as usize;
                if key == 0 || key >= 256 {
                    return Ok(());
                }
                self.keys[key] = if pressed { 128 } else { 0 };
                self.keys[VK_SHIFT.0 as usize] =
                    self.keys[VK_LSHIFT.0 as usize] | self.keys[VK_RSHIFT.0 as usize];
                self.keys[VK_CONTROL.0 as usize] =
                    self.keys[VK_LCONTROL.0 as usize] | self.keys[VK_RCONTROL.0 as usize];
                self.keys[VK_MENU.0 as usize] =
                    self.keys[VK_LMENU.0 as usize] | self.keys[VK_RMENU.0 as usize];
                unsafe {
                    SetKeyboardState(&self.keys)?;
                }
                let target = self.keyboard_target();
                let bits = 1
                    | ((scan_code as isize) << 16)
                    | (isize::from(extended) << 24)
                    | if pressed { 0 } else { 3 << 30 };
                unsafe {
                    let alt = self.keys[VK_MENU.0 as usize] != 0;
                    let mut characters = [0_u16; 8];
                    let count = if pressed {
                        ToUnicodeEx(
                            key as u32,
                            u32::from(scan_code),
                            &self.keys,
                            &mut characters,
                            0,
                            Some(GetKeyboardLayout(GetWindowThreadProcessId(target, None))),
                        )
                    } else {
                        0
                    };
                    let literal = !alt
                        && self.keys[VK_CONTROL.0 as usize] == 0
                        && (count < 0
                            || (count > 0 && characters[0] >= 32 && characters[0] != 127));
                    if literal {
                        // Explicit characters preserve the remote modifier state without
                        // depending on the time an application's message loop runs.
                        for character in characters.iter().take(count.max(0) as usize) {
                            SendMessageTimeoutW(
                                target,
                                WM_CHAR,
                                WPARAM(*character as usize),
                                LPARAM(bits),
                                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                                20,
                                None,
                            );
                        }
                    } else {
                        // Accelerators and dialog navigation are interpreted by the
                        // application's message loop, before DispatchMessage.
                        let message = match (alt, pressed) {
                            (true, true) => WM_SYSKEYDOWN,
                            (true, false) => WM_SYSKEYUP,
                            (false, true) => WM_KEYDOWN,
                            (false, false) => WM_KEYUP,
                        };
                        let message_key = match key as u16 {
                            value if value == VK_LSHIFT.0 || value == VK_RSHIFT.0 => VK_SHIFT.0,
                            value if value == VK_LCONTROL.0 || value == VK_RCONTROL.0 => {
                                VK_CONTROL.0
                            }
                            value if value == VK_LMENU.0 || value == VK_RMENU.0 => VK_MENU.0,
                            value => value,
                        };
                        self.post(
                            target,
                            message,
                            message_key as usize,
                            bits | (isize::from(alt) << 29),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn keyboard_target(&self) -> HWND {
        unsafe {
            if self.focus.is_invalid() || !IsWindow(Some(self.focus)).as_bool() {
                return HWND::default();
            }
            // Conhost does not expose its text input through GUI thread focus.
            // Attached input queues may still report the previous GUI control.
            let root = GetAncestor(self.focus, GA_ROOT);
            if self
                .console_inputs
                .iter()
                .any(|console| console.window == root)
            {
                return self.focus;
            }
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if GetGUIThreadInfo(GetWindowThreadProcessId(self.focus, None), &mut info).is_ok()
                && !info.hwndFocus.is_invalid()
            {
                info.hwndFocus
            } else {
                self.focus
            }
        }
    }

    fn wheel(&self, horizontal: i16, vertical: i16) -> anyhow::Result<()> {
        if let Some(hwnd) = self.hit() {
            for (message, delta) in [(WM_MOUSEWHEEL, vertical), (WM_MOUSEHWHEEL, horizontal)] {
                if delta != 0 {
                    self.post(
                        hwnd,
                        message,
                        (delta as u16 as usize) << 16,
                        pack(self.pointer),
                    )?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.release();
        self.console_inputs.clear();
        unsafe {
            let _ = CloseHandle(self.job);
            if !self.shell.is_invalid() {
                let _ = DestroyWindow(self.shell);
            }
            for icon in self.icons.drain(..) {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

fn pack(point: POINT) -> isize {
    (point.x as u16 as u32 | ((point.y as u16 as u32) << 16)) as isize
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

unsafe extern "system" fn launcher_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_DRAWITEM && lparam.0 != 0 {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let brush = CreateSolidBrush(COLORREF(if item.itemState.0 & ODS_SELECTED.0 != 0 {
                0x665544
            } else {
                0x383838
            }));
            FillRect(item.hDC, &item.rcItem, brush);
            let _ = DeleteObject(brush.into());
            let icon = HICON(GetWindowLongPtrW(item.hwndItem, GWLP_USERDATA) as *mut _);
            if !icon.is_invalid() {
                let _ = DrawIconEx(
                    item.hDC,
                    (PIN_WIDTH - 36) / 2,
                    3,
                    icon,
                    32,
                    32,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            SetBkMode(item.hDC, TRANSPARENT);
            SetTextColor(item.hDC, COLORREF(0xffffff));
            let font = SelectObject(item.hDC, GetStockObject(DEFAULT_GUI_FONT));
            let mut label = [0_u16; 64];
            let length = GetWindowTextW(item.hwndItem, &mut label) as usize;
            let mut rect = item.rcItem;
            rect.top += 39;
            DrawTextW(
                item.hDC,
                &mut label[..length],
                &mut rect,
                DT_CENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(item.hDC, font);
            return LRESULT(1);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send_key(workspace: &mut Workspace, scan_code: u16, pressed: bool) -> anyhow::Result<()> {
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code,
            extended: false,
            pressed,
        })
    }

    #[test]
    #[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
    fn session_zero_gui() -> anyhow::Result<()> {
        // A dedicated thread can bind before any HWNDs or hooks are created.
        std::thread::spawn(|| -> anyhow::Result<()> {
            unsafe {
                use windows::Win32::System::StationsAndDesktops::*;
                let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
                SetProcessWindowStation(station)?;
            }
            let _owner = background::Desktop::create().context("create background desktop")?;
            let _binding = background::Desktop::bind().context("bind background desktop")?;
            background::windows().context("enumerate empty background desktop")?;
            background::snapshot_bmp().context("render empty background desktop")?;
            let mut workspace = Workspace::new().context("create background workspace")?;
            let edit = unsafe {
                CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    w!("EDIT"),
                    w!(""),
                    WS_OVERLAPPED | WS_CAPTION | WS_VISIBLE | WS_TABSTOP,
                    50,
                    200,
                    600,
                    180,
                    None,
                    None,
                    None,
                    None,
                )?
            };
            workspace.focus = edit;
            workspace.apply(RemoteInput::TypeText {
                display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                text: "Session 0 GUI input verified".into(),
            })?;
            workspace.pump();
            let mut text = [0_u16; 128];
            let count = unsafe { GetWindowTextW(edit, &mut text) };
            assert_eq!(
                String::from_utf16_lossy(&text[..count as usize]),
                "Session 0 GUI input verified"
            );
            let before = background::snapshot_bmp()?;
            assert!(
                before[54..]
                    .chunks_exact(4)
                    .any(|p| p[..3] != [0x30, 0x20, 0x18])
            );
            unsafe {
                DestroyWindow(edit)?;
            }
            let baseline = background::snapshot_bmp()?;
            workspace.apply(RemoteInput::PointerButtonAt {
                display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                x: ((8 + 2 * PIN_WIDTH as u32 + 30) * 65535 / (WIDTH - 1)) as u16,
                y: ((HEIGHT - TASKBAR_HEIGHT as u32 + 30) * 65535 / (HEIGHT - 1)) as u16,
                button: PointerButton::Left,
                pressed: true,
            })?;
            workspace.apply(RemoteInput::PointerButton {
                display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                button: PointerButton::Left,
                pressed: false,
            })?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut applications = Vec::new();
            while std::time::Instant::now() < deadline {
                workspace.pump();
                applications = background::windows()?
                    .into_iter()
                    .filter(|hwnd| *hwnd != workspace.shell)
                    .collect();
                if !applications.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(
                !applications.is_empty(),
                "Registry Editor did not create a background window"
            );
            let mut processes = Vec::new();
            for hwnd in applications {
                let mut process = 0;
                unsafe {
                    GetWindowThreadProcessId(hwnd, Some(&mut process));
                }
                let mut session = u32::MAX;
                unsafe {
                    windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
                        process,
                        &mut session,
                    )?;
                }
                assert_eq!(session, 0);
                processes.push(process);
            }
            let mut after = baseline.clone();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while after == baseline && std::time::Instant::now() < deadline {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(100));
                after = background::snapshot_bmp()?;
            }
            for hwnd in background::windows()? {
                let mut title = [0_u16; 256];
                let mut rect = RECT::default();
                unsafe {
                    GetWindowRect(hwnd, &mut rect)?;
                }
                let count = unsafe { GetWindowTextW(hwnd, &mut title) };
                println!(
                    "application window: {} {rect:?}",
                    String::from_utf16_lossy(&title[..count as usize])
                );
            }
            let evidence = std::env::temp_dir().join("meshrmm-background-gui.bmp");
            std::fs::write(&evidence, &after)?;
            assert!(
                baseline != after,
                "Registry Editor did not render into the desktop"
            );
            println!("Session 0 rendering evidence: {}", evidence.display());
            let (frames, received) = std::sync::mpsc::sync_channel(8);
            let mut streamer = meshrmm_remote_screen::WindowsDesktopDuplicationStreamer::new();
            let format = streamer.start(
                meshrmm_remote_screen::StreamConfig {
                    frames_per_second: 10,
                    ..Default::default()
                },
                background::DISPLAY_ID,
                std::sync::Arc::new(move |frame| {
                    let _ = frames.try_send(frame);
                }),
            )?;
            assert_eq!((format.width, format.height), (WIDTH, HEIGHT));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut keyframe = false;
            while !keyframe && std::time::Instant::now() < deadline {
                workspace.pump();
                keyframe = received
                    .try_iter()
                    .any(|frame| frame.keyframe && !frame.data.is_empty());
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let stop = std::thread::spawn(move || streamer.stop());
            while !stop.is_finished() {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            stop.join().expect("capture stop panicked")?;
            assert!(
                keyframe,
                "Session 0 renderer did not produce an encoded keyframe"
            );
            println!("Session 0 H.264 keyframe verified");
            workspace.launch(2)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let console = loop {
                workspace.pump();
                let console = background::windows()?.into_iter().find(|hwnd| {
                    let mut class = [0_u16; 64];
                    let count = unsafe { GetClassNameW(*hwnd, &mut class) };
                    String::from_utf16_lossy(&class[..count as usize]) == "ConsoleWindowClass"
                });
                if let Some(console) = console {
                    break console;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "PowerShell console did not open"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            };
            // The console HWND precedes PowerShell/PSReadLine initialization;
            // let startup finish before exercising its interactive input mode.
            let ready = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < ready {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(console, &mut rect)?;
            }
            workspace.move_pointer(
                ((rect.left + 100) as u32 * 65535 / (WIDTH - 1)) as u16,
                ((rect.top + 80) as u32 * 65535 / (HEIGHT - 1)) as u16,
            );
            workspace.button(PointerButton::Left, true)?;
            workspace.button(PointerButton::Left, false)?;
            let evidence = std::env::temp_dir().join(format!(
                "meshrmm-console-keyboard-{}.txt",
                std::process::id()
            ));
            assert_eq!(
                workspace.console_inputs.last().unwrap().window,
                console,
                "console attachment returned the wrong window"
            );
            assert_eq!(
                unsafe { GetAncestor(workspace.keyboard_target(), GA_ROOT) },
                console,
                "keyboard focus did not reach the console"
            );
            let _ = std::fs::remove_file(&evidence);
            let command = format!(
                "Set-Content -LiteralPath '{}' -Value 'AbC_123'",
                evidence.display()
            );
            for character in command.encode_utf16() {
                let key = unsafe { VkKeyScanW(character) };
                assert!(key >= 0, "test character has no keyboard mapping");
                let shift = key & 0x100 != 0;
                if shift {
                    send_key(&mut workspace, 0x2a, true)?;
                }
                let scan = unsafe { MapVirtualKeyW((key & 0xff) as u32, MAPVK_VK_TO_VSC) } as u16;
                send_key(&mut workspace, scan, true)?;
                send_key(&mut workspace, scan, false)?;
                if shift {
                    send_key(&mut workspace, 0x2a, false)?;
                }
                workspace.pump();
            }
            send_key(&mut workspace, 0x1c, true)?;
            send_key(&mut workspace, 0x1c, false)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !evidence.exists() && std::time::Instant::now() < deadline {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            std::fs::write(
                std::env::temp_dir().join("meshrmm-background-console.bmp"),
                background::snapshot_bmp()?,
            )?;
            let output = std::fs::read_to_string(&evidence)
                .context("PowerShell did not execute the typed command")?;
            assert_eq!(
                output.trim(),
                "AbC_123",
                "Console duplicated or changed keyboard characters"
            );
            std::fs::remove_file(evidence)?;
            println!("Session 0 PowerShell mixed-case keyboard input verified");
            drop(workspace);
            for process in processes {
                if let Ok(handle) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process) } {
                    let status = unsafe { WaitForSingleObject(handle, 5000) };
                    unsafe {
                        CloseHandle(handle)?;
                    }
                    assert_eq!(
                        status, WAIT_OBJECT_0,
                        "Background application survived workspace cleanup"
                    );
                }
            }
            Ok(())
        })
        .join()
        .expect("background test thread panicked")
    }
}
