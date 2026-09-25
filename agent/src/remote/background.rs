//! Experimental Session 0 GUI input and application launcher.
//! No SendInput, desktop switching, console hooks, or user-token launches.
pub(super) mod launch;

use crate::win32::wide;
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
            let system32 = crate::win32::windows_directory()?.join("System32");
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
                let path = wide(system32.join(icon_file));
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
        let system32 = crate::win32::windows_directory()?.join("System32");
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
            system32.join(program)
        };
        let started = launch::launch(launch::Launch {
            executable: Some(path.as_path()),
            command: &format!("\"{}\" {arguments}", path.display()),
            flags: CREATE_SUSPENDED
                | if built_in {
                    CREATE_NO_WINDOW
                } else {
                    CREATE_NEW_CONSOLE
                },
            window: Some((40, 24, 1100, (HEIGHT as i32 - TASKBAR_HEIGHT - 48) as u32)),
            ..Default::default()
        })?;
        unsafe {
            let result = AssignProcessToJobObject(self.job, started.process.0);
            if result.is_err() || ResumeThread(started.thread.0) == u32::MAX {
                let _ = TerminateProcess(started.process.0, 1);
                result?;
                anyhow::bail!("could not resume background application");
            }
        }
        let process_id = started.id;
        if *arguments == "--background-task-manager" {
            // Retain the process handle so its PID cannot be recycled before
            // cleaning its private telemetry session on forced job shutdown.
            self.task_managers
                .push((process_id, started.process.into_raw()));
        } else if *arguments == "--background-file-browser" {
            self.file_browsers
                .push((process_id, started.process.into_raw()));
        }
        if pin.console {
            self.console_inputs
                .push(super::background_console::ConsoleInput::start(
                    process_id, self.job,
                )?);
        }
        tracing::info!(
            session_id = 0,
            process_id,
            program,
            "background application started"
        );
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
mod tests;
