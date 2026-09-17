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

pub(super) const TASKBAR_HEIGHT: i32 = 48;
const PIN_WIDTH: i32 = 48;
const ICON_SIZE: i32 = 32;
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
    ("Task Manager", "taskmgr.exe", "--background-task-manager"),
    ("Computer Mgmt", "mmc.exe", "compmgmt.msc"),
    ("Device Manager", "mmc.exe", "devmgmt.msc"),
    ("Firewall", "mmc.exe", "wf.msc"),
    (
        "File Explorer",
        "..\\explorer.exe",
        "--background-file-browser",
    ),
];

pub struct Workspace {
    icons: Vec<HICON>,
    task_managers: Vec<(u32, HANDLE)>,
    shell: HWND,
    tooltip: HWND,
    hovered: Option<usize>,
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
                task_managers: Vec::new(),
                shell: HWND::default(),
                tooltip: HWND::default(),
                hovered: None,
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
                hbrBackground: HBRUSH::default(),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
            // PrintWindow can capture ordinary child repaints partway through.
            // Composite the launcher and its buttons before exposing their pixels.
            workspace.shell = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_COMPOSITED,
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
            let tooltip_class = WNDCLASSW {
                lpfnWndProc: Some(tooltip_proc),
                lpszClassName: w!("MeshRMMBackgroundTooltip"),
                ..Default::default()
            };
            if RegisterClassW(&tooltip_class) == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
            workspace.tooltip = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                tooltip_class.lpszClassName,
                w!(""),
                WS_POPUP | WS_BORDER,
                0,
                0,
                240,
                24,
                Some(workspace.shell),
                None,
                None,
                None,
            )?;
            let root = std::env::var("SystemRoot").context("SystemRoot is unavailable")?;
            for (index, (label, program, arguments)) in PINS.iter().enumerate() {
                let label = wide(label);
                let button = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    PCWSTR(label.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_OWNERDRAW as u32),
                    8 + index as i32 * PIN_WIDTH,
                    4,
                    PIN_WIDTH,
                    TASKBAR_HEIGHT - 8,
                    Some(workspace.shell),
                    Some(HMENU((index + 1) as *mut _)),
                    None,
                    None,
                )?;
                let (icon_file, icon_index) = match *arguments {
                    "services.msc" => ("filemgmt.dll", 0),
                    "eventvwr.msc" => ("miguiresource.dll", 0),
                    "compmgmt.msc" => ("mycomput.dll", 2),
                    "devmgmt.msc" => ("devmgr.dll", 4),
                    "wf.msc" => ("authfwgp.dll", 0),
                    _ => (*program, 0),
                };
                let path = wide(&format!("{root}\\System32\\{icon_file}"));
                let mut icon = HICON::default();
                ExtractIconExW(PCWSTR(path.as_ptr()), icon_index, Some(&mut icon), None, 1);
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
        if *arguments == "--background-task-manager" {
            unsafe {
                if let Ok(window) = FindWindowW(w!("MeshRMMBackgroundTasks"), None) {
                    let mut pid = 0;
                    GetWindowThreadProcessId(window, Some(&mut pid));
                    if self.task_managers.iter().any(|(owned, _)| *owned == pid) {
                        let _ = ShowWindowAsync(
                            window,
                            if IsIconic(window).as_bool() {
                                SW_RESTORE
                            } else {
                                SW_SHOW
                            },
                        );
                        let _ = SetWindowPos(
                            window,
                            Some(HWND_TOP),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                        self.focus = window;
                        return Ok(());
                    }
                }
            }
        }
        let root = std::env::var("SystemRoot").context("SystemRoot is unavailable")?;
        let built_in = matches!(
            *arguments,
            "--background-task-manager" | "--background-file-browser"
        );
        let path = if built_in {
            #[cfg(test)]
            let executable = std::path::PathBuf::from(
                std::env::var_os("MESHRMM_BACKGROUND_TEST_AGENT").context(
                    "set MESHRMM_BACKGROUND_TEST_AGENT to the built Agent for GUI tests",
                )?,
            );
            #[cfg(not(test))]
            let executable = std::env::current_exe()?;
            executable
        } else {
            std::path::PathBuf::from(format!("{root}\\System32\\{program}"))
        };
        let executable = wide(&path.to_string_lossy());
        let mut command = wide(&format!("\"{}\" {arguments}", path.display()));
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
                CREATE_SUSPENDED
                    | if built_in {
                        CREATE_NO_WINDOW
                    } else {
                        CREATE_NEW_CONSOLE
                    },
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
            if *arguments == "--background-task-manager" {
                // Retain the process handle so its PID cannot be recycled before
                // cleaning its private telemetry session on forced job shutdown.
                self.task_managers.push((info.dwProcessId, info.hProcess));
            } else {
                let _ = CloseHandle(info.hProcess);
            }
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
        let hovered = (self.pointer.y >= HEIGHT as i32 - TASKBAR_HEIGHT + 4
            && self.pointer.y < HEIGHT as i32 - 4
            && self.pointer.x >= 8)
            .then(|| ((self.pointer.x - 8) / PIN_WIDTH) as usize)
            .filter(|index| *index < PINS.len());
        if self.hovered != hovered {
            unsafe {
                for index in [self.hovered, hovered].into_iter().flatten() {
                    if let Ok(button) = GetDlgItem(Some(self.shell), index as i32 + 1) {
                        SendMessageW(
                            button,
                            BM_SETSTATE,
                            Some(WPARAM(usize::from(hovered == Some(index)))),
                            None,
                        );
                    }
                }
                if let Some(index) = hovered {
                    let label = wide(PINS[index].0);
                    let _ = SetWindowTextW(self.tooltip, PCWSTR(label.as_ptr()));
                    let _ = SetWindowPos(
                        self.tooltip,
                        Some(HWND_TOPMOST),
                        8 + index as i32 * PIN_WIDTH,
                        HEIGHT as i32 - TASKBAR_HEIGHT - 26,
                        200,
                        24,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                } else {
                    let _ = ShowWindow(self.tooltip, SW_HIDE);
                }
            }
            self.hovered = hovered;
        }
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
                        HTLEFT | HTRIGHT | HTTOP | HTTOPLEFT | HTTOPRIGHT | HTBOTTOM
                        | HTBOTTOMLEFT | HTBOTTOMRIGHT => {
                            let mut pid = 0;
                            GetWindowThreadProcessId(top, Some(&mut pid));
                            if self.task_managers.iter().any(|(owned, _)| *owned == pid) {
                                // Task Manager owns its resize adapter. Send its
                                // border clicks to the frame rather than an
                                // overlapping list child; leave other apps alone.
                                self.pressed = Some(top);
                                self.focus = top;
                                return self.post(top, WM_LBUTTONDOWN, 1, self.client_point(top));
                            }
                        }
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
                            // Keep text in FIFO order with queued Home/Delete,
                            // shortcuts, and key releases. A synchronous send can
                            // overtake them or time out while the app is painting.
                            self.post(target, WM_CHAR, *character as usize, bits)?;
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
            for (pid, process) in self.task_managers.drain(..) {
                WaitForSingleObject(process, 5000);
                super::background_tasks::stop_telemetry(pid);
                let _ = CloseHandle(process);
            }
            if !self.tooltip.is_invalid() {
                let _ = DestroyWindow(self.tooltip);
            }
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

unsafe extern "system" fn tooltip_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if matches!(message, WM_PAINT | WM_PRINT | WM_PRINTCLIENT) {
            let mut paint = PAINTSTRUCT::default();
            let dc = if message == WM_PAINT {
                BeginPaint(hwnd, &mut paint)
            } else {
                HDC(wparam.0 as *mut _)
            };
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            FillRect(dc, &rect, HBRUSH(GetStockObject(WHITE_BRUSH).0));
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0));
            let font = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
            let mut label = [0_u16; 64];
            let length = GetWindowTextW(hwnd, &mut label) as usize;
            rect.left += 5;
            DrawTextW(
                dc,
                &mut label[..length],
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(dc, font);
            if message == WM_PAINT {
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

unsafe extern "system" fn launcher_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_ERASEBKGND {
            let dc = HDC(wparam.0 as *mut _);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let brush = CreateSolidBrush(COLORREF(0x3f3933));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush.into());
            return LRESULT(1);
        }
        if matches!(message, WM_PAINT | WM_PRINTCLIENT) {
            let mut paint = PAINTSTRUCT::default();
            let dc = if message == WM_PAINT {
                BeginPaint(hwnd, &mut paint)
            } else {
                HDC(wparam.0 as *mut _)
            };
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let brush = CreateSolidBrush(COLORREF(0x3f3933));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush.into());
            if message == WM_PAINT {
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(0);
        }
        if message == WM_DRAWITEM && lparam.0 != 0 {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let brush = CreateSolidBrush(COLORREF(if item.itemState.0 & ODS_SELECTED.0 != 0 {
                0x655c53
            } else {
                0x3f3933
            }));
            FillRect(item.hDC, &item.rcItem, brush);
            let _ = DeleteObject(brush.into());
            let icon = HICON(GetWindowLongPtrW(item.hwndItem, GWLP_USERDATA) as *mut _);
            if !icon.is_invalid() {
                let _ = DrawIconEx(
                    item.hDC,
                    (PIN_WIDTH - ICON_SIZE) / 2,
                    (TASKBAR_HEIGHT - 8 - ICON_SIZE) / 2,
                    icon,
                    ICON_SIZE,
                    ICON_SIZE,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            if item.itemState.0 & ODS_SELECTED.0 != 0 {
                let brush = CreateSolidBrush(COLORREF(0xcbb54c));
                let mut line = item.rcItem;
                line.top = line.bottom - 3;
                FillRect(item.hDC, &line, brush);
                let _ = DeleteObject(brush.into());
            }
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
    fn task_manager_opens_in_background() -> anyhow::Result<()> {
        tool_opens_in_background(7, "Task Manager", "background-task-manager.bmp")
    }

    #[test]
    #[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
    fn file_browser_opens_in_background() -> anyhow::Result<()> {
        tool_opens_in_background(11, "MeshRMM File Browser", "background-file-browser.bmp")
    }

    fn task_manager_interactions(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
        use windows::Win32::UI::Controls::*;
        fn settle(workspace: &Workspace, millis: u64) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
            while std::time::Instant::now() < deadline {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        fn click(workspace: &mut Workspace, x: i32, y: i32) -> anyhow::Result<()> {
            workspace.move_pointer(
                (x as u32 * 65535 / (WIDTH - 1)) as u16,
                (y as u32 * 65535 / (HEIGHT - 1)) as u16,
            );
            workspace.button(PointerButton::Left, true)?;
            workspace.button(PointerButton::Left, false)?;
            settle(workspace, 60);
            Ok(())
        }
        fn command(window: HWND, id: usize) -> anyhow::Result<()> {
            unsafe {
                PostMessageW(Some(window), WM_COMMAND, WPARAM(id), LPARAM(0))?;
            }
            Ok(())
        }
        fn select_tab(workspace: &mut Workspace, tabs: HWND, index: usize) -> anyhow::Result<()> {
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(tabs, &mut rect)?;
            }
            // Exercise actual routed mouse input. The native tab widths depend on
            // the Windows font/theme, so scan instead of guessing fixed widths.
            for x in (rect.left + 5..rect.right).step_by(6) {
                click(workspace, x, rect.top + 10)?;
                if unsafe { SendMessageW(tabs, TCM_GETCURSEL, None, None).0 } == index as isize {
                    return Ok(());
                }
            }
            anyhow::bail!("Tab {index} did not respond to background mouse input")
        }
        let proof = std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&proof)?;
        let tabs = unsafe { GetDlgItem(Some(window), 104)? };
        assert_eq!(
            unsafe { SendMessageW(tabs, TCM_GETITEMCOUNT, None, None).0 },
            7
        );
        settle(workspace, 2400);
        std::fs::write(
            proof.join("task-manager-processes.bmp"),
            background::snapshot_bmp()?,
        )?;
        for (index, name) in [
            "processes",
            "performance",
            "history",
            "startup",
            "users",
            "details",
            "services",
        ]
        .iter()
        .enumerate()
        {
            select_tab(workspace, tabs, index)?;
            settle(workspace, 250);
            if index == 6 {
                let list = unsafe { GetDlgItem(Some(window), 101)? };
                for (id, bar) in [(OBJID_VSCROLL, SB_VERT), (OBJID_HSCROLL, SB_HORZ)] {
                    let mut info = SCROLLBARINFO {
                        cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
                        ..Default::default()
                    };
                    unsafe { GetScrollBarInfo(list, id, &mut info)? };
                    assert_eq!(info.rgstate[0] & 0x8000, 0, "Scrollbar must be visible");
                    let before = unsafe { GetScrollPos(list, bar) };
                    let x = if bar == SB_VERT {
                        (info.rcScrollBar.left + info.rcScrollBar.right) / 2
                    } else {
                        info.rcScrollBar.right - info.dxyLineButton / 2
                    };
                    let y = if bar == SB_VERT {
                        info.rcScrollBar.bottom - info.dxyLineButton / 2
                    } else {
                        (info.rcScrollBar.top + info.rcScrollBar.bottom) / 2
                    };
                    click(workspace, x, y)?;
                    assert!(
                        unsafe { GetScrollPos(list, bar) } > before,
                        "Scrollbar arrow did not scroll"
                    );
                    unsafe { GetScrollBarInfo(list, id, &mut info)? };
                    let before_drag = unsafe { GetScrollPos(list, bar) };
                    let thumb = (info.xyThumbTop + info.xyThumbBottom) / 2;
                    let (x, y) = if bar == SB_VERT {
                        (x, info.rcScrollBar.top + thumb)
                    } else {
                        (info.rcScrollBar.left + thumb, y)
                    };
                    workspace.move_pointer(
                        (x as u32 * 65535 / (WIDTH - 1)) as u16,
                        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
                    );
                    workspace.button(PointerButton::Left, true)?;
                    settle(workspace, 60);
                    let (x, y) = if bar == SB_VERT {
                        (x, y + 30)
                    } else {
                        (x + 30, y)
                    };
                    workspace.apply(RemoteInput::PointerMove {
                        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                        x: (x as u32 * 65535 / (WIDTH - 1)) as u16,
                        y: (y as u32 * 65535 / (HEIGHT - 1)) as u16,
                    })?;
                    settle(workspace, 60);
                    workspace.button(PointerButton::Left, false)?;
                    settle(workspace, 60);
                    assert!(
                        unsafe { GetScrollPos(list, bar) } > before_drag,
                        "Scrollbar {bar:?} thumb did not drag from {before_drag} (now {})",
                        unsafe { GetScrollPos(list, bar) }
                    );
                    unsafe {
                        SendMessageW(
                            list,
                            if bar == SB_VERT {
                                WM_VSCROLL
                            } else {
                                WM_HSCROLL
                            },
                            Some(WPARAM(SB_TOP.0 as usize)),
                            None,
                        );
                    }
                }
                settle(workspace, 100);
            }
            std::fs::write(
                proof.join(format!("task-manager-{name}.bmp")),
                background::snapshot_bmp()?,
            )?;
        }
        let mut bounds = RECT::default();
        unsafe { GetWindowRect(window, &mut bounds)? };
        click(workspace, bounds.right - 116, bounds.top + 15)?;
        settle(workspace, 200);
        assert!(
            unsafe { IsIconic(window) }.as_bool(),
            "Minimize button did not minimize"
        );
        let helpers = workspace.task_managers.len();
        workspace.launch(7)?;
        settle(workspace, 200);
        assert!(
            !unsafe { IsIconic(window) }.as_bool(),
            "Taskbar pin did not restore Task Manager"
        );
        assert_eq!(
            workspace.task_managers.len(),
            helpers,
            "Restoring created a duplicate helper"
        );
        unsafe { GetWindowRect(window, &mut bounds)? };
        click(workspace, bounds.right - 70, bounds.top + 15)?;
        settle(workspace, 200);
        assert!(
            unsafe { IsZoomed(window) }.as_bool(),
            "Maximize button did not maximize"
        );
        unsafe { GetWindowRect(window, &mut bounds)? };
        assert!(
            bounds.left == 0
                && bounds.top == 0
                && bounds.right == WIDTH as i32
                && bounds.bottom == HEIGHT as i32 - TASKBAR_HEIGHT,
            "Maximized Task Manager must fill the background work area: {bounds:?}"
        );
        click(workspace, bounds.right - 70, bounds.top + 15)?;
        settle(workspace, 200);
        assert!(!unsafe { IsZoomed(window) }.as_bool());
        unsafe { GetWindowRect(window, &mut bounds)? };
        let original = bounds;
        workspace.move_pointer(
            ((bounds.right - 1) as u32 * 65535 / (WIDTH - 1)) as u16,
            ((bounds.bottom - 1) as u32 * 65535 / (HEIGHT - 1)) as u16,
        );
        workspace.button(PointerButton::Left, true)?;
        settle(workspace, 60);
        workspace.apply(RemoteInput::PointerMove {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            x: ((bounds.right + 89) as u32 * 65535 / (WIDTH - 1)) as u16,
            y: ((bounds.bottom + 59) as u32 * 65535 / (HEIGHT - 1)) as u16,
        })?;
        settle(workspace, 60);
        workspace.button(PointerButton::Left, false)?;
        settle(workspace, 100);
        unsafe { GetWindowRect(window, &mut bounds)? };
        assert!(
            bounds.right > original.right + 50 && bounds.bottom > original.bottom + 30,
            "Window border did not resize: {bounds:?}"
        );
        unsafe {
            SetWindowPos(
                window,
                None,
                original.left,
                original.top,
                original.right - original.left,
                original.bottom - original.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        settle(workspace, 100);
        select_tab(workspace, tabs, 0)?;
        command(window, 213)?; // pause; manual refresh must still work
        command(window, 102)?;
        settle(workspace, 200);
        command(window, 105)?;
        settle(workspace, 100);
        assert!(!unsafe { IsWindowVisible(tabs) }.as_bool());
        std::fs::write(
            proof.join("task-manager-compact.bmp"),
            background::snapshot_bmp()?,
        )?;
        command(window, 105)?;
        command(window, 211)?;
        settle(workspace, 100);
        assert!(unsafe { IsWindowVisible(tabs) }.as_bool());
        // Run a benign command through the actual new-task form on this desktop.
        let output = proof.join("task-manager-run.txt");
        let _ = std::fs::remove_file(&output);
        command(window, 201)?;
        settle(workspace, 100);
        let edit = unsafe { GetDlgItem(Some(window), 107)? };
        let text = wide(&format!(
            "cmd.exe /c echo MeshRMMTaskManagerFixture>\"{}\"",
            output.display()
        ));
        unsafe {
            SendMessageW(edit, WM_SETTEXT, None, Some(LPARAM(text.as_ptr() as isize)));
        }
        command(window, 108)?;
        settle(workspace, 600);
        anyhow::ensure!(
            std::fs::read_to_string(&output)?.contains("MeshRMMTaskManagerFixture"),
            "Run new task did not execute"
        );
        // Termination is tested only against an executable copied into this test's
        // directory. Verify the selected PID before issuing any End Task command.
        let exe = proof.join("MeshRMMTaskFixture.exe");
        std::fs::copy(
            std::path::PathBuf::from(std::env::var("SystemRoot")?).join("System32\\cmd.exe"),
            &exe,
        )?;
        let mut child = std::process::Command::new(&exe)
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let result = (|| -> anyhow::Result<()> {
            select_tab(workspace, tabs, 5)?;
            command(window, 102)?;
            settle(workspace, 1800);
            let list = unsafe { GetDlgItem(Some(window), 101)? };
            unsafe {
                SendMessageW(list, WM_KEYDOWN, Some(WPARAM(0x24)), None);
            }
            for c in "MeshRMMTaskFixture.exe".encode_utf16() {
                unsafe {
                    SendMessageW(list, WM_CHAR, Some(WPARAM(c as usize)), None);
                }
            }
            command(window, 222)?;
            settle(workspace, 100);
            let status = unsafe { GetDlgItem(Some(window), 109)? };
            let mut text = [0u16; 4096];
            let len = unsafe { GetWindowTextW(status, &mut text) };
            let text = String::from_utf16_lossy(&text[..len as usize]);
            anyhow::ensure!(
                text.split("  |  ").nth(1) == Some(child.id().to_string().as_str()),
                "Refusing to end unexpected selection: {text}"
            );
            command(window, 103)?;
            settle(workspace, 100);
            anyhow::ensure!(child.try_wait()?.is_none(), "End task skipped confirmation");
            std::fs::write(
                proof.join("task-manager-confirmation.bmp"),
                background::snapshot_bmp()?,
            )?;
            command(window, 106)?;
            settle(workspace, 100);
            anyhow::ensure!(child.try_wait()?.is_none(), "Cancel terminated the process");
            command(window, 103)?;
            settle(workspace, 100);
            command(window, 103)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while child.try_wait()?.is_none() {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "Confirmed End Task did not terminate fixture"
                );
                settle(workspace, 50);
            }
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(exe);
        result?;
        select_tab(workspace, tabs, 0)?;
        command(window, 106)?;
        settle(workspace, 300);
        Ok(())
    }

    fn tool_opens_in_background(
        index: usize,
        expected_title: &'static str,
        screenshot: &'static str,
    ) -> anyhow::Result<()> {
        std::thread::spawn(move || -> anyhow::Result<()> {
            unsafe {
                use windows::Win32::System::StationsAndDesktops::*;
                let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
                SetProcessWindowStation(station)?;
            }
            let _owner = background::Desktop::create()?;
            let _binding = background::Desktop::bind()?;
            let mut workspace = Workspace::new()?;
            workspace.launch(index)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                workspace.pump();
                for window in background::windows()? {
                    let mut title = [0_u16; 256];
                    let count = unsafe { GetWindowTextW(window, &mut title) };
                    if String::from_utf16_lossy(&title[..count as usize]) == expected_title {
                        let list = unsafe { GetDlgItem(Some(window), 101)? };
                        let mut rows = 0;
                        unsafe {
                            SendMessageTimeoutW(
                                list,
                                windows::Win32::UI::Controls::LVM_GETITEMCOUNT,
                                WPARAM(0),
                                LPARAM(0),
                                SMTO_ABORTIFHUNG,
                                100,
                                Some(&mut rows),
                            );
                        }
                        if rows == 0 {
                            continue;
                        }
                        let mut pid = 0;
                        unsafe {
                            GetWindowThreadProcessId(window, Some(&mut pid));
                        }
                        let process = unsafe {
                            OpenProcess(
                                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                                false,
                                pid,
                            )?
                        };
                        let mut in_job = windows::core::BOOL(0);
                        unsafe {
                            IsProcessInJob(process, Some(workspace.job), &mut in_job)?;
                        }
                        assert!(in_job.as_bool(), "process manager escaped session cleanup");
                        if index == 7 {
                            task_manager_interactions(&mut workspace, window)?;
                        }
                        std::fs::write(
                            std::env::temp_dir().join(screenshot),
                            background::snapshot_bmp()?,
                        )?;
                        drop(workspace);
                        let exited = unsafe { WaitForSingleObject(process, 5000) };
                        let _ = unsafe { CloseHandle(process) };
                        assert_eq!(
                            exited, WAIT_OBJECT_0,
                            "background tool survived workspace close"
                        );
                        return Ok(());
                    }
                }
                if std::time::Instant::now() >= deadline {
                    for window in background::windows()? {
                        let mut title = [0u16; 512];
                        let count = unsafe { GetWindowTextW(window, &mut title) };
                        eprintln!(
                            "Window: {}",
                            String::from_utf16_lossy(&title[..count as usize])
                        );
                    }
                    for (pid, process) in &workspace.task_managers {
                        let mut code = 0;
                        let _ = unsafe { GetExitCodeProcess(*process, &mut code) };
                        eprintln!("Task manager {pid} exit={code}");
                    }
                    std::fs::write(
                        std::env::temp_dir().join("task-manager-failure.bmp"),
                        background::snapshot_bmp()?,
                    )?;
                    anyhow::bail!("{expected_title} did not open on the background desktop");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .join()
        .expect("background test panicked")
    }

    #[test]
    #[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
    fn stable_taskbar_and_management_caption() -> anyhow::Result<()> {
        fn capture(workspace: &mut Workspace) -> anyhow::Result<Vec<u8>> {
            let worker = std::thread::spawn(|| -> anyhow::Result<Vec<u8>> {
                let _binding = background::Desktop::bind()?;
                Ok(background::snapshot_bmp()?)
            });
            while !worker.is_finished() {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            worker.join().expect("capture worker panicked")
        }
        std::thread::spawn(|| -> anyhow::Result<()> {
            unsafe {
                use windows::Win32::System::StationsAndDesktops::*;
                let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
                SetProcessWindowStation(station)?;
            }
            let _owner = background::Desktop::create()?;
            let _binding = background::Desktop::bind()?;
            let mut workspace = Workspace::new()?;
            workspace.pump();
            let baseline = background::snapshot_bmp()?;
            let taskbar_start = 54 + (WIDTH * (HEIGHT - TASKBAR_HEIGHT as u32) * 4) as usize;
            for _ in 0..60 {
                workspace.pump();
                let frame = capture(&mut workspace)?;
                if frame[taskbar_start..] != baseline[taskbar_start..] {
                    std::fs::write(std::env::temp_dir().join("taskbar-baseline.bmp"), &baseline)?;
                    std::fs::write(std::env::temp_dir().join("taskbar-worker.bmp"), &frame)?;
                    anyhow::bail!("idle taskbar changed");
                }
            }
            workspace.launch(8)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let management = loop {
                workspace.pump();
                let window = background::windows()?.into_iter().find(|hwnd| {
                    let mut title = [0_u16; 256];
                    let count = unsafe { GetWindowTextW(*hwnd, &mut title) };
                    String::from_utf16_lossy(&title[..count as usize]) == "Computer Management"
                });
                if let Some(window) = window {
                    break window;
                }
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "Computer Management did not open"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            let ready = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < ready {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(management, &mut rect)?;
            }
            let caption = |frame: &[u8]| -> Vec<u8> {
                let mut pixels = Vec::new();
                for y in rect.top.max(0)..(rect.top + 28).min(HEIGHT as i32) {
                    let start = 54 + (y as usize * WIDTH as usize + rect.left.max(0) as usize) * 4;
                    let end = 54
                        + (y as usize * WIDTH as usize + rect.right.min(WIDTH as i32) as usize) * 4;
                    pixels.extend_from_slice(&frame[start..end]);
                }
                pixels
            };
            let baseline = background::snapshot_bmp()?;
            std::fs::write(
                std::env::temp_dir().join("meshrmm-compact-taskbar.bmp"),
                &baseline,
            )?;
            for _ in 0..60 {
                workspace.pump();
                let frame = capture(&mut workspace)?;
                anyhow::ensure!(
                    caption(&frame) == caption(&baseline),
                    "idle management caption changed"
                );
                anyhow::ensure!(
                    frame[taskbar_start..] == baseline[taskbar_start..],
                    "taskbar changed with management open"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // MMC's File/Action/View/Help toolbar can print only its background
            // after a move on this off-screen desktop. Compare the actual menu
            // pixels after dragging, without hovering or activating the menu.
            let menu_pixels = |frame: &[u8], rect: RECT| -> Vec<u8> {
                let mut pixels = Vec::new();
                for y in rect.top + 30..rect.top + 50 {
                    let start = 54 + (y as usize * WIDTH as usize + rect.left as usize + 8) * 4;
                    pixels.extend_from_slice(&frame[start..start + 170 * 4]);
                }
                pixels
            };
            let expected_menu = menu_pixels(&baseline, rect);
            anyhow::ensure!(
                expected_menu
                    .chunks_exact(4)
                    .filter(|pixel| pixel[..3].iter().all(|value| *value < 100))
                    .count()
                    > 40,
                "baseline MMC menu must contain visible text"
            );
            for (dx, dy) in [(150, 80), (60, 20), (200, 60), (0, 0)] {
                workspace.pointer = POINT {
                    x: rect.left + 300,
                    y: rect.top + 12,
                };
                workspace.drag = Some((management, workspace.pointer, rect));
                workspace.move_pointer(
                    ((rect.left + 300 + dx) as u32 * 65535 / (WIDTH - 1)) as u16,
                    ((rect.top + 12 + dy) as u32 * 65535 / (HEIGHT - 1)) as u16,
                );
                workspace.drag = None;
                let mut moved = RECT::default();
                unsafe { GetWindowRect(management, &mut moved)? };
                for _ in 0..10 {
                    let frame = capture(&mut workspace)?;
                    if menu_pixels(&frame, moved) != expected_menu {
                        std::fs::write(
                            std::env::temp_dir().join("mmc-menu-before.bmp"),
                            &baseline,
                        )?;
                        std::fs::write(std::env::temp_dir().join("mmc-menu-after.bmp"), &frame)?;
                    }
                    anyhow::ensure!(
                        menu_pixels(&frame, moved) == expected_menu,
                        "MMC menu labels changed after moving from {rect:?} to {moved:?}"
                    );
                }
            }
            Ok(())
        })
        .join()
        .expect("capture stability test thread panicked")
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
            let empty = background::snapshot_bmp().context("render empty background desktop")?;
            assert!(
                empty[54..]
                    .chunks_exact(4)
                    .all(|pixel| pixel[..3] == [0, 0, 0]),
                "background must be black"
            );
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
            // This EDIT belongs to this thread, so it deliberately cannot pump
            // queued navigation until after every printable key was submitted.
            // Synchronous WM_CHAR used to overtake Home/Delete and corrupt text.
            unsafe {
                SetWindowTextW(edit, w!("C:\\"))?;
            }
            for scan in [0x47, 0x53, 0x53, 0x53] {
                for pressed in [true, false] {
                    workspace.apply(RemoteInput::Key {
                        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                        scan_code: scan,
                        extended: true,
                        pressed,
                    })?;
                }
            }
            let expected = "C:\\Windows\\System32";
            for character in expected.encode_utf16() {
                let key = unsafe { VkKeyScanW(character) };
                assert!(key >= 0);
                if key & 0x100 != 0 {
                    send_key(&mut workspace, 0x2a, true)?;
                }
                let scan = unsafe { MapVirtualKeyW((key & 0xff) as u32, MAPVK_VK_TO_VSC) } as u16;
                send_key(&mut workspace, scan, true)?;
                send_key(&mut workspace, scan, false)?;
                if key & 0x100 != 0 {
                    send_key(&mut workspace, 0x2a, false)?;
                }
            }
            workspace.pump();
            let count = unsafe { GetWindowTextW(edit, &mut text) };
            assert_eq!(
                String::from_utf16_lossy(&text[..count as usize]),
                expected,
                "queued navigation and literal text must stay ordered"
            );
            let before = background::snapshot_bmp()?;
            assert!(before[54..].chunks_exact(4).any(|p| p[..3] != [0, 0, 0]));
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
                    .filter(|hwnd| *hwnd != workspace.shell && *hwnd != workspace.tooltip)
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
