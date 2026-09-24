//! Experimental Session 0 GUI input and application launcher.
//! No SendInput, desktop switching, console hooks, or user-token launches.
use anyhow::Context;
use meshrmm_protocol::{PointerButton, RemoteInput};
use meshrmm_remote_screen::background::{self, HEIGHT, WIDTH};
use std::time::{Duration, Instant};
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
const TASKS_LEFT: i32 = 8 + PINS.len() as i32 * PIN_WIDTH + 12;
const TASK_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
/// An application on the background taskbar.
struct Pin {
    label: &'static str,
    program: &'static str,
    arguments: &'static str,
    /// Runs in its own console, which takes keyboard input through a console input helper.
    console: bool,
}

const PINS: &[Pin] = &[
    Pin {
        label: "Command Prompt",
        program: "cmd.exe",
        arguments: "/k title MeshRMM Background Command Prompt",
        console: true,
    },
    Pin {
        label: "PowerShell",
        program: "WindowsPowerShell\\v1.0\\powershell.exe",
        arguments: "-NoLogo -NoProfile -NoExit",
        console: true,
    },
    Pin {
        label: "Registry Editor",
        program: "..\\regedit.exe",
        arguments: "/m",
        console: false,
    },
    Pin {
        label: "Services",
        program: "mmc.exe",
        arguments: "services.msc",
        console: false,
    },
    Pin {
        label: "Event Viewer",
        program: "mmc.exe",
        arguments: "eventvwr.msc",
        console: false,
    },
    Pin {
        label: "Resource Monitor",
        program: "resmon.exe",
        arguments: "",
        console: false,
    },
    Pin {
        label: "Task Manager",
        program: "taskmgr.exe",
        arguments: "--background-task-manager",
        console: false,
    },
    Pin {
        label: "Computer Mgmt",
        program: "mmc.exe",
        arguments: "compmgmt.msc",
        console: false,
    },
    Pin {
        label: "Device Manager",
        program: "mmc.exe",
        arguments: "devmgmt.msc",
        console: false,
    },
    Pin {
        label: "Firewall",
        program: "mmc.exe",
        arguments: "wf.msc",
        console: false,
    },
    Pin {
        label: "File Explorer",
        program: "..\\explorer.exe",
        arguments: "--background-file-browser",
        console: false,
    },
];

pub struct Workspace {
    icons: Vec<HICON>,
    task_managers: Vec<(u32, HANDLE)>,
    file_browsers: Vec<(u32, HANDLE)>,
    shell: HWND,
    tooltip: HWND,
    hovered: Option<usize>,
    tasks: Vec<TaskButton>,
    last_task_refresh: Instant,
    job: HANDLE,
    focus: HWND,
    pointer: POINT,
    pressed: Option<HWND>,
    drag: Option<(HWND, POINT, RECT)>,
    keys: [u8; 256],
    attached_thread: Option<u32>,
    console_inputs: Vec<super::background_console::ConsoleInput>,
}

struct TaskButton {
    window: HWND,
    process: u32,
    button: HWND,
    icon: HICON,
    title: String,
}

struct TaskWindow {
    window: HWND,
    process: u32,
    title: String,
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
                file_browsers: Vec::new(),
                shell: HWND::default(),
                tooltip: HWND::default(),
                hovered: None,
                tasks: Vec::new(),
                last_task_refresh: Instant::now(),
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
            for (index, pin) in PINS.iter().enumerate() {
                let label = wide(pin.label);
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
                let (icon_file, icon_index) = match pin.arguments {
                    "services.msc" => ("filemgmt.dll", 0),
                    "eventvwr.msc" => ("miguiresource.dll", 0),
                    "compmgmt.msc" => ("mycomput.dll", 2),
                    "devmgmt.msc" => ("devmgr.dll", 4),
                    "wf.msc" => ("authfwgp.dll", 0),
                    _ => (pin.program, 0),
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

    fn owns_window(&self, window: HWND) -> bool {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(window, Some(&mut pid));
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut owned = windows::core::BOOL(0);
            let result = IsProcessInJob(process, Some(self.job), &mut owned);
            let _ = CloseHandle(process);
            result.is_ok() && owned.as_bool()
        }
    }

    fn refresh_tasks(&mut self) -> anyhow::Result<()> {
        // A console's input helper leaves once its programs exit and the console closes.
        self.console_inputs
            .retain_mut(|console| !console.finished());
        let visible = task_windows(self.shell, self.tooltip)?;
        unsafe {
            self.tasks.retain(|task| {
                let exists = visible
                    .iter()
                    .any(|window| window.window == task.window && window.process == task.process);
                if !exists {
                    let _ = DestroyWindow(task.button);
                    if !task.icon.is_invalid() {
                        let _ = DestroyIcon(task.icon);
                    }
                }
                exists
            });
            for window in visible {
                if let Some(task) = self
                    .tasks
                    .iter_mut()
                    .find(|task| task.window == window.window && task.process == window.process)
                {
                    if task.title != window.title {
                        task.title = window.title;
                        SetWindowTextW(task.button, PCWSTR(wide(&task.title).as_ptr()))?;
                    }
                    continue;
                }
                let button = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    PCWSTR(wide(&window.title).as_ptr()),
                    WS_CHILD | WINDOW_STYLE(BS_OWNERDRAW as u32),
                    0,
                    4,
                    1,
                    TASKBAR_HEIGHT - 8,
                    Some(self.shell),
                    None,
                    None,
                    None,
                )?;
                let icon = task_icon(window.window, window.process, self.shell);
                SetWindowLongPtrW(button, GWLP_USERDATA, icon.0 as isize);
                self.tasks.push(TaskButton {
                    window: window.window,
                    process: window.process,
                    button,
                    icon,
                    title: window.title,
                });
            }
            let width =
                ((WIDTH as i32 - TASKS_LEFT - 8) / self.tasks.len().max(1) as i32).min(PIN_WIDTH);
            for (index, task) in self.tasks.iter().enumerate() {
                let id = (PINS.len() + index + 1) as i32;
                if GetDlgCtrlID(task.button) != id {
                    SetWindowLongPtrW(task.button, GWLP_ID, id as isize);
                }
                let x = TASKS_LEFT + index as i32 * width;
                let mut rect = RECT::default();
                if GetWindowRect(task.button, &mut rect).is_err()
                    || rect.left != x
                    || rect.top != HEIGHT as i32 - TASKBAR_HEIGHT + 4
                    || rect.right - rect.left != width
                    || rect.bottom - rect.top != TASKBAR_HEIGHT - 8
                    || !IsWindowVisible(task.button).as_bool()
                {
                    let _ = SetWindowPos(
                        task.button,
                        None,
                        x,
                        4,
                        width,
                        TASKBAR_HEIGHT - 8,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
            }
        }
        Ok(())
    }

    fn restore_task(&mut self, index: usize) {
        let Some(task) = self.tasks.get(index) else {
            return;
        };
        unsafe {
            let mut process = 0;
            GetWindowThreadProcessId(task.window, Some(&mut process));
            if process != task.process || !IsWindow(Some(task.window)).as_bool() {
                return;
            }
            let _ = ShowWindowAsync(
                task.window,
                if IsIconic(task.window).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            let _ = SetWindowPos(
                task.window,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            self.focus = task.window;
        }
    }

    fn launch(&mut self, index: usize) -> anyhow::Result<()> {
        let pin = PINS
            .get(index.wrapping_sub(1))
            .context("unknown background application")?;
        let (program, arguments) = (&pin.program, &pin.arguments);
        if matches!(
            *arguments,
            "--background-task-manager" | "--background-file-browser"
        ) {
            unsafe {
                let (class, owned) = if *arguments == "--background-task-manager" {
                    (w!("MeshRMMBackgroundTasks"), &self.task_managers)
                } else {
                    (w!("MeshRMMBackgroundFiles"), &self.file_browsers)
                };
                if let Ok(window) = FindWindowW(class, None) {
                    let mut pid = 0;
                    GetWindowThreadProcessId(window, Some(&mut pid));
                    if owned.iter().any(|(owned, _)| *owned == pid)
                        || (*arguments == "--background-file-browser" && self.owns_window(window))
                    {
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
            } else if *arguments == "--background-file-browser" {
                self.file_browsers.push((info.dwProcessId, info.hProcess));
            } else {
                let _ = CloseHandle(info.hProcess);
            }
            if pin.console {
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

    pub fn pump(&mut self) {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if self.last_task_refresh.elapsed() >= TASK_REFRESH_INTERVAL {
            if let Err(error) = self.refresh_tasks() {
                tracing::warn!(%error, "could not refresh background taskbar");
            }
            self.last_task_refresh = Instant::now();
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
        let hovered = hovered.or_else(|| {
            self.tasks
                .iter()
                .position(|task| unsafe {
                    let mut rect = RECT::default();
                    GetWindowRect(task.button, &mut rect).is_ok()
                        && self.pointer.x >= rect.left
                        && self.pointer.x < rect.right
                        && self.pointer.y >= rect.top
                        && self.pointer.y < rect.bottom
                })
                .map(|index| PINS.len() + index)
        });
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
                    let label = wide(if index < PINS.len() {
                        PINS[index].label
                    } else {
                        &self.tasks[index - PINS.len()].title
                    });
                    let _ = SetWindowTextW(self.tooltip, PCWSTR(label.as_ptr()));
                    let x = if index < PINS.len() {
                        8 + index as i32 * PIN_WIDTH
                    } else {
                        let mut rect = RECT::default();
                        let _ = GetWindowRect(self.tasks[index - PINS.len()].button, &mut rect);
                        rect.left
                    };
                    let _ = SetWindowPos(
                        self.tooltip,
                        Some(HWND_TOPMOST),
                        x.min(WIDTH as i32 - 200),
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
                    let id = GetDlgCtrlID(hwnd) as usize;
                    if id <= PINS.len() {
                        self.launch(id)?;
                    } else {
                        self.restore_task(id - PINS.len() - 1);
                    }
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
                            let mut class = [0u16; 64];
                            let length = GetClassNameW(top, &mut class);
                            let browser = String::from_utf16_lossy(&class[..length as usize])
                                == "MeshRMMBackgroundFiles";
                            if (browser && self.owns_window(top))
                                || self
                                    .task_managers
                                    .iter()
                                    .chain(&self.file_browsers)
                                    .any(|(owned, _)| *owned == pid)
                            {
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
                        HTMINBUTTON => {
                            return self.post(top, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
                        }
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
            for (_, process) in self.file_browsers.drain(..) {
                WaitForSingleObject(process, 5000);
                let _ = CloseHandle(process);
            }
            if !self.tooltip.is_invalid() {
                let _ = DestroyWindow(self.tooltip);
            }
            if !self.shell.is_invalid() {
                let _ = DestroyWindow(self.shell);
            }
            for task in self.tasks.drain(..) {
                if !task.icon.is_invalid() {
                    let _ = DestroyIcon(task.icon);
                }
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

fn task_windows(shell: HWND, tooltip: HWND) -> windows::core::Result<Vec<TaskWindow>> {
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
            if window == shell || window == tooltip {
                continue;
            }
            let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
            let ex_style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
            if (style & WS_VISIBLE.0 == 0 && !IsIconic(window).as_bool())
                || ex_style & WS_EX_TOOLWINDOW.0 != 0
                || GetWindow(window, GW_OWNER).is_ok()
            {
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

fn task_icon(window: HWND, process_id: u32, shell: HWND) -> HICON {
    unsafe {
        let mut class = [0_u16; 64];
        let length = GetClassNameW(window, &mut class) as usize;
        let pinned = match String::from_utf16_lossy(&class[..length]).as_str() {
            "MeshRMMBackgroundTasks" => Some(7),
            "MeshRMMBackgroundFiles" => Some(11),
            _ => None,
        };
        if let Some(index) = pinned
            && let Ok(button) = GetDlgItem(Some(shell), index)
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
                let x = if (item.CtlID as usize) <= PINS.len() {
                    (PIN_WIDTH - ICON_SIZE) / 2
                } else {
                    (item.rcItem.right - item.rcItem.left - ICON_SIZE) / 2
                };
                let _ = DrawIconEx(
                    item.hDC,
                    x,
                    (TASKBAR_HEIGHT - 8 - ICON_SIZE) / 2,
                    icon,
                    ICON_SIZE,
                    ICON_SIZE,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            if (item.CtlID as usize) > PINS.len() || item.itemState.0 & ODS_SELECTED.0 != 0 {
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

    #[test]
    #[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
    fn running_window_taskbar_restores_minimized_window() -> anyhow::Result<()> {
        unsafe extern "system" fn test_window_proc(
            window: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
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
            let window = unsafe {
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(test_window_proc),
                    lpszClassName: w!("MeshRMMTaskbarTest"),
                    hIcon: LoadIconW(None, IDI_WARNING)?,
                    ..Default::default()
                });
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("MeshRMMTaskbarTest"),
                    w!("Taskbar restore test"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    40,
                    50,
                    400,
                    250,
                    None,
                    None,
                    None,
                    None,
                )?
            };
            workspace.refresh_tasks()?;
            let task = workspace
                .tasks
                .iter()
                .find(|task| task.window == window)
                .context("running window is missing from taskbar")?;
            anyhow::ensure!(
                !task.icon.is_invalid(),
                "running window has no taskbar icon"
            );
            let button = task.button;
            workspace.pump();
            let mut rect = RECT::default();
            unsafe { GetWindowRect(button, &mut rect)? };
            let frame = background::snapshot_bmp()?;
            let pixel = |x: i32, y: i32| -> &[u8] {
                let start = 54 + (y as usize * WIDTH as usize + x as usize) * 4;
                &frame[start..start + 3]
            };
            let background = pixel(rect.left + 1, rect.top + 1);
            anyhow::ensure!(
                (rect.top + 5..rect.bottom - 5).any(|y| {
                    (rect.left + 8..rect.right - 8).any(|x| pixel(x, y) != background)
                }),
                "running-window icon did not render"
            );
            anyhow::ensure!(
                pixel((rect.left + rect.right) / 2, rect.bottom - 2) != background,
                "running-window indicator did not render"
            );
            let mut window_rect = RECT::default();
            unsafe { GetWindowRect(window, &mut window_rect)? };
            let minimize = (window_rect.left..window_rect.right)
                .rev()
                .find(|x| unsafe {
                    SendMessageW(
                        window,
                        WM_NCHITTEST,
                        Some(WPARAM(0)),
                        Some(LPARAM(pack(POINT {
                            x: *x,
                            y: window_rect.top + 15,
                        }))),
                    )
                    .0 as u32
                        == HTMINBUTTON
                })
                .context("standard minimize button was not found")?;
            workspace.move_pointer(
                (minimize as u32 * 65535 / (WIDTH - 1)) as u16,
                ((window_rect.top + 15) as u32 * 65535 / (HEIGHT - 1)) as u16,
            );
            workspace.button(PointerButton::Left, true)?;
            workspace.button(PointerButton::Left, false)?;
            workspace.pump();
            workspace.refresh_tasks()?;
            anyhow::ensure!(
                unsafe { IsIconic(window) }.as_bool(),
                "window did not minimize"
            );
            anyhow::ensure!(
                workspace.tasks.iter().any(|task| task.window == window),
                "minimized window disappeared from taskbar"
            );
            unsafe { GetWindowRect(button, &mut rect)? };
            let x = (rect.left + rect.right) / 2;
            let y = (rect.top + rect.bottom) / 2;
            workspace.move_pointer(
                (x as u32 * 65535 / (WIDTH - 1)) as u16,
                (y as u32 * 65535 / (HEIGHT - 1)) as u16,
            );
            workspace.button(PointerButton::Left, true)?;
            workspace.button(PointerButton::Left, false)?;
            workspace.pump();
            anyhow::ensure!(
                !unsafe { IsIconic(window) }.as_bool(),
                "taskbar button did not restore the minimized window"
            );
            unsafe { DestroyWindow(window)? };
            workspace.refresh_tasks()?;
            anyhow::ensure!(
                !workspace.tasks.iter().any(|task| task.window == window),
                "closed window remained on taskbar"
            );
            Ok(())
        })
        .join()
        .expect("background taskbar test panicked")
    }

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
        tool_opens_in_background(11, "File Explorer", "background-file-browser.bmp")
    }

    fn file_browser_interactions(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
        use windows::Win32::UI::Controls::*;
        fn settle(workspace: &mut Workspace, millis: u64) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
            while std::time::Instant::now() < deadline {
                workspace.pump();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        fn text(window: HWND) -> String {
            let mut value = [0u16; 8192];
            let length = unsafe {
                SendMessageW(
                    window,
                    WM_GETTEXT,
                    Some(WPARAM(value.len())),
                    Some(LPARAM(value.as_mut_ptr() as isize)),
                )
                .0
            };
            String::from_utf16_lossy(&value[..length as usize])
        }
        fn command(window: HWND, id: usize) -> anyhow::Result<()> {
            unsafe {
                PostMessageW(Some(window), WM_COMMAND, WPARAM(id), LPARAM(0))?;
            }
            Ok(())
        }
        fn wait(workspace: &mut Workspace, status: HWND) -> anyhow::Result<()> {
            settle(workspace, 300);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "Browser operation timed out: {}",
                    text(status)
                );
                let before = text(status);
                settle(workspace, 200);
                let after = text(status);
                if before == after
                    && !after.starts_with("Working")
                    && !after.starts_with("Cancelling")
                {
                    println!("Browser status: {after}");
                    return Ok(());
                }
            }
        }
        fn select(list: HWND, name: &str) {
            unsafe {
                SendMessageW(list, WM_KEYDOWN, Some(WPARAM(0x24)), None);
            }
            for c in name.encode_utf16() {
                unsafe {
                    SendMessageW(list, WM_CHAR, Some(WPARAM(c as usize)), None);
                }
            }
        }
        let root = std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let fixture = root.join(format!("explorer-fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(fixture.join("Folder"))?;
        std::fs::write(fixture.join("alpha.txt"), b"Explorer fixture\r\n")?;
        std::fs::write(fixture.join("beta.txt"), b"Second fixture\r\n")?;
        std::fs::write(
            fixture.join("Folder").join("nested.txt"),
            b"Recursive copy fixture",
        )?;
        let location = unsafe { GetDlgItem(Some(window), 201)? };
        let list = unsafe { GetDlgItem(Some(window), 101)? };
        let status = unsafe { GetDlgItem(Some(window), 231)? };
        let search = unsafe { GetDlgItem(Some(window), 227)? };
        let navigate = |path: &std::path::Path| -> anyhow::Result<()> {
            unsafe {
                SendMessageW(
                    location,
                    WM_SETTEXT,
                    None,
                    Some(LPARAM(wide(&path.to_string_lossy()).as_ptr() as isize)),
                );
            }
            command(window, 202)
        };
        navigate(&fixture)?;
        wait(workspace, status)?;
        std::fs::write(
            root.join("explorer-initial.bmp"),
            background::snapshot_bmp()?,
        )?;
        assert_eq!(
            unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 },
            3
        );
        // Exercise toolbar commands and verify their effects only inside this fixture.
        select(list, "alpha");
        command(window, 209)?;
        settle(workspace, 100);
        command(window, 210)?;
        wait(workspace, status)?;
        assert_eq!(
            std::fs::read(fixture.join("alpha - Copy.txt"))?,
            b"Explorer fixture\r\n"
        );
        command(window, 207)?;
        wait(workspace, status)?;
        let edit = HWND(unsafe { SendMessageW(list, LVM_GETEDITCONTROL, None, None).0 } as *mut _);
        anyhow::ensure!(!edit.is_invalid(), "New folder did not begin inline rename");
        unsafe {
            SendMessageW(
                edit,
                WM_SETTEXT,
                None,
                Some(LPARAM(w!("Renamed folder").as_ptr() as isize)),
            );
            PostMessageW(Some(edit), WM_KEYDOWN, WPARAM(13), LPARAM(0))?;
        }
        wait(workspace, status)?;
        anyhow::ensure!(
            fixture.join("Renamed folder").is_dir(),
            "Inline rename failed"
        );
        select(list, "Folder");
        command(window, 209)?;
        settle(workspace, 100);
        navigate(&fixture.join("Renamed folder"))?;
        wait(workspace, status)?;
        command(window, 210)?;
        wait(workspace, status)?;
        assert_eq!(
            std::fs::read(fixture.join("Renamed folder/Folder/nested.txt"))?,
            b"Recursive copy fixture"
        );
        command(window, 212)?;
        wait(workspace, status)?;
        assert_eq!(text(location), fixture.to_string_lossy());
        command(window, 213)?;
        wait(workspace, status)?;
        assert_eq!(
            text(location),
            fixture.join("Renamed folder").to_string_lossy()
        );
        command(window, 203)?;
        wait(workspace, status)?;
        select(list, "beta");
        command(window, 214)?;
        settle(workspace, 100);
        navigate(&fixture.join("Renamed folder"))?;
        wait(workspace, status)?;
        command(window, 210)?;
        wait(workspace, status)?;
        anyhow::ensure!(
            !fixture.join("beta.txt").exists() && fixture.join("Renamed folder/beta.txt").exists(),
            "Cut/paste failed"
        );
        // Cancel and confirm must act on the captured selection, not another row.
        select(list, "beta");
        command(window, 215)?;
        settle(workspace, 150);
        anyhow::ensure!(
            fixture.join("Renamed folder/beta.txt").exists(),
            "Delete skipped confirmation"
        );
        std::fs::write(
            root.join("explorer-delete.bmp"),
            background::snapshot_bmp()?,
        )?;
        command(window, 230)?;
        settle(workspace, 100);
        anyhow::ensure!(
            fixture.join("Renamed folder/beta.txt").exists(),
            "Cancel deleted the fixture"
        );
        command(window, 215)?;
        settle(workspace, 100);
        command(window, 229)?;
        wait(workspace, status)?;
        anyhow::ensure!(
            !fixture.join("Renamed folder/beta.txt").exists(),
            "Confirmed delete failed"
        );
        navigate(&fixture)?;
        wait(workspace, status)?;
        unsafe {
            SendMessageW(
                search,
                WM_SETTEXT,
                None,
                Some(LPARAM(w!("nested").as_ptr() as isize)),
            );
        }
        command(window, 232)?;
        wait(workspace, status)?;
        assert_eq!(
            unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 },
            2
        );
        unsafe {
            SendMessageW(
                search,
                WM_SETTEXT,
                None,
                Some(LPARAM(w!("").as_ptr() as isize)),
            );
        }
        command(window, 232)?;
        wait(workspace, status)?;
        command(window, 217)?;
        wait(workspace, status)?;
        assert_eq!(
            unsafe { SendMessageW(list, LVM_GETSELECTEDCOUNT, None, None).0 },
            4
        );
        command(window, 219)?;
        wait(workspace, status)?;
        assert_eq!(
            unsafe { SendMessageW(list, LVM_GETSELECTEDCOUNT, None, None).0 },
            0
        );
        for (command_id, mode) in [
            (224, LV_VIEW_ICON),
            (223, LV_VIEW_SMALLICON),
            (222, LV_VIEW_DETAILS),
        ] {
            command(window, command_id)?;
            wait(workspace, status)?;
            assert_eq!(
                unsafe { SendMessageW(list, LVM_GETVIEW, None, None).0 },
                mode as isize
            );
        }
        // Exercise real list-view drag notifications through remote pointer input.
        let drag_folder = fixture.join("Drag test");
        std::fs::create_dir_all(drag_folder.join("Target"))?;
        std::fs::write(drag_folder.join("drag.txt"), b"drag fixture")?;
        navigate(&drag_folder)?;
        wait(workspace, status)?;
        let drag_item = |workspace: &mut Workspace| -> anyhow::Result<()> {
            let header = HWND(unsafe { SendMessageW(list, LVM_GETHEADER, None, None).0 } as *mut _);
            let mut bounds = RECT::default();
            let mut heading = RECT::default();
            unsafe {
                GetWindowRect(list, &mut bounds)?;
                GetWindowRect(header, &mut heading)?;
            }
            let x = bounds.left + 80;
            let from_y = heading.bottom + 27;
            let to_y = heading.bottom + 9;
            workspace.move_pointer(
                (x as u32 * 65535 / (WIDTH - 1)) as u16,
                (from_y as u32 * 65535 / (HEIGHT - 1)) as u16,
            );
            workspace.button(PointerButton::Left, true)?;
            settle(workspace, 100);
            for (px, py) in [(x + 18, from_y), (x + 18, to_y)] {
                workspace.apply(RemoteInput::PointerMove {
                    display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                    x: (px as u32 * 65535 / (WIDTH - 1)) as u16,
                    y: (py as u32 * 65535 / (HEIGHT - 1)) as u16,
                })?;
                settle(workspace, 100);
            }
            workspace.button(PointerButton::Left, false)?;
            wait(workspace, status)
        };
        drag_item(workspace)?;
        anyhow::ensure!(
            !drag_folder.join("drag.txt").exists() && drag_folder.join("Target/drag.txt").exists(),
            "Dragging into a folder did not move the file"
        );
        assert_eq!(
            text(location),
            drag_folder.to_string_lossy(),
            "Drop unexpectedly navigated away from the source"
        );
        std::fs::write(drag_folder.join("copy.txt"), b"ctrl drag fixture")?;
        command(window, 204)?;
        wait(workspace, status)?;
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code: 0x1d,
            extended: false,
            pressed: true,
        })?;
        settle(workspace, 100);
        drag_item(workspace)?;
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code: 0x1d,
            extended: false,
            pressed: false,
        })?;
        anyhow::ensure!(
            drag_folder.join("copy.txt").exists() && drag_folder.join("Target/copy.txt").exists(),
            "Ctrl-drag did not preserve and copy the source"
        );
        command(window, 236)?;
        wait(workspace, status)?;
        anyhow::ensure!(
            drag_folder.join("copy.txt").exists() && !drag_folder.join("Target/copy.txt").exists(),
            "Undo copy did not preserve the original and remove its unchanged copy"
        );
        command(window, 236)?;
        wait(workspace, status)?;
        anyhow::ensure!(
            drag_folder.join("drag.txt").exists() && !drag_folder.join("Target/drag.txt").exists(),
            "Undo move did not restore the original location"
        );
        navigate(&fixture)?;
        wait(workspace, status)?;
        // Posted background input must resize and minimize the custom frame.
        let mut original = RECT::default();
        unsafe {
            GetWindowRect(window, &mut original)?;
        }
        let x = original.right - 2;
        let y = original.top + 220;
        workspace.move_pointer(
            (x as u32 * 65535 / (WIDTH - 1)) as u16,
            (y as u32 * 65535 / (HEIGHT - 1)) as u16,
        );
        workspace.button(PointerButton::Left, true)?;
        settle(workspace, 100);
        workspace.apply(RemoteInput::PointerMove {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            x: ((x - 60) as u32 * 65535 / (WIDTH - 1)) as u16,
            y: (y as u32 * 65535 / (HEIGHT - 1)) as u16,
        })?;
        settle(workspace, 100);
        workspace.button(PointerButton::Left, false)?;
        settle(workspace, 100);
        let mut resized = RECT::default();
        unsafe {
            GetWindowRect(window, &mut resized)?;
        }
        anyhow::ensure!(
            resized.right - resized.left < original.right - original.left,
            "Explorer border did not resize with background pointer input"
        );
        unsafe {
            PostMessageW(
                Some(window),
                WM_SYSCOMMAND,
                WPARAM(SC_MAXIMIZE as usize),
                LPARAM(0),
            )?;
        }
        settle(workspace, 200);
        let mut maximized = RECT::default();
        unsafe {
            GetWindowRect(window, &mut maximized)?;
        }
        assert_eq!(maximized.right - maximized.left, WIDTH as i32);
        assert_eq!(
            maximized.bottom - maximized.top,
            HEIGHT as i32 - TASKBAR_HEIGHT
        );
        unsafe {
            PostMessageW(
                Some(window),
                WM_SYSCOMMAND,
                WPARAM(SC_RESTORE as usize),
                LPARAM(0),
            )?;
        }
        settle(workspace, 200);
        let helpers = workspace.file_browsers.len();
        unsafe {
            GetWindowRect(window, &mut resized)?;
        }
        let x = resized.right - 115;
        let y = resized.top + 16;
        workspace.move_pointer(
            (x as u32 * 65535 / (WIDTH - 1)) as u16,
            (y as u32 * 65535 / (HEIGHT - 1)) as u16,
        );
        workspace.button(PointerButton::Left, true)?;
        workspace.button(PointerButton::Left, false)?;
        settle(workspace, 200);
        anyhow::ensure!(
            unsafe { IsIconic(window) }.as_bool(),
            "Explorer minimize button did not respond"
        );
        workspace.launch(11)?;
        settle(workspace, 200);
        anyhow::ensure!(
            !unsafe { IsIconic(window) }.as_bool(),
            "Explorer taskbar pin did not restore the window"
        );
        assert_eq!(
            workspace.file_browsers.len(),
            helpers,
            "Restoring Explorer launched a duplicate"
        );
        command(window, 221)?;
        settle(workspace, 150);
        std::fs::write(root.join("explorer-view.bmp"), background::snapshot_bmp()?)?;
        command(window, 220)?;
        settle(workspace, 150);
        std::fs::write(root.join("explorer-home.bmp"), background::snapshot_bmp()?)?;
        navigate(&root)?;
        wait(workspace, status)?;
        std::fs::remove_dir_all(fixture)?;
        println!(
            "Explorer navigation, inline rename, file/folder copy, move, deletion confirmation, search, selection and view modes passed"
        );
        Ok(())
    }

    fn task_manager_interactions(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
        use windows::Win32::UI::Controls::*;
        fn settle(workspace: &mut Workspace, millis: u64) {
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
                        if index == 11 {
                            file_browser_interactions(&mut workspace, window)?;
                        }
                        std::fs::write(
                            std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(std::env::temp_dir)
                                .join(screenshot),
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
                    for (pid, process) in workspace
                        .task_managers
                        .iter()
                        .chain(&workspace.file_browsers)
                    {
                        let mut code = 0;
                        let _ = unsafe { GetExitCodeProcess(*process, &mut code) };
                        eprintln!("Background tool {pid} exit={code}");
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
                if frame[taskbar_start..] != baseline[taskbar_start..] {
                    std::fs::write(
                        std::env::temp_dir().join("management-taskbar-baseline.bmp"),
                        &baseline,
                    )?;
                    std::fs::write(
                        std::env::temp_dir().join("management-taskbar-worker.bmp"),
                        &frame,
                    )?;
                    anyhow::bail!("taskbar changed with management open");
                }
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
    fn console_exit_closes_window_task_and_input_helper() -> anyhow::Result<()> {
        std::thread::spawn(|| -> anyhow::Result<()> {
            unsafe {
                use windows::Win32::System::StationsAndDesktops::*;
                let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
                SetProcessWindowStation(station)?;
            }
            let _owner = background::Desktop::create()?;
            let _binding = background::Desktop::bind()?;
            let mut workspace = Workspace::new()?;
            let pin = PINS
                .iter()
                .position(|pin| pin.program == "cmd.exe")
                .unwrap()
                + 1;
            assert!(PINS[pin - 1].console);
            workspace.launch(pin)?;
            assert_eq!(workspace.console_inputs.len(), 1);
            let helper = workspace.console_inputs[0].helper_process_id();
            let console = workspace.console_inputs[0].window;
            let helper = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, helper)? };
            let wait_for =
                |workspace: &mut Workspace, what: &str, done: &dyn Fn(&Workspace) -> bool| {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !done(workspace) {
                        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
                        workspace.pump();
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(())
                };
            wait_for(&mut workspace, "the console task", &|workspace| {
                workspace.tasks.iter().any(|task| task.window == console)
            })?;
            // Let cmd reach its prompt before typing.
            let ready = Instant::now() + Duration::from_secs(2);
            while Instant::now() < ready {
                workspace.pump();
                std::thread::sleep(Duration::from_millis(20));
            }
            workspace.focus = console;
            workspace.apply(RemoteInput::TypeText {
                display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                text: "exit".into(),
            })?;
            send_key(&mut workspace, 0x1c, true)?;
            send_key(&mut workspace, 0x1c, false)?;
            wait_for(&mut workspace, "the console window to close", &|_| {
                !unsafe { IsWindow(Some(console)) }.as_bool()
            })?;
            wait_for(
                &mut workspace,
                "the console task and input to go",
                &|workspace| {
                    workspace.console_inputs.is_empty()
                        && !workspace.tasks.iter().any(|task| task.window == console)
                },
            )?;
            let exited = unsafe { WaitForSingleObject(helper, 5000) };
            unsafe { CloseHandle(helper)? };
            assert_eq!(exited, WAIT_OBJECT_0, "console input helper kept running");
            println!("Session 0 console exit closed its window, task and input helper");
            Ok(())
        })
        .join()
        .expect("console exit test thread panicked")
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
