//! Experimental Session 0 GUI input and application launcher.
//! No console hooks or user-token launches. The workspace makes its desktop
//! Session 0's input desktop and sends it real input, but never switches the
//! console's. The workspace thread sends that input, so no window it owns may
//! start a modal loop, which would wait for input the thread can't send: the
//! taskbar handles its own presses, and Run has a thread of its own.
mod inject;
mod keyboard;
pub(super) mod launch;
mod paint;
mod profile;
mod run;
mod screen;
mod task_windows;

use crate::win32::wide;
use anyhow::Context;
use meshrmm_protocol::{PointerButton, RemoteInput};
use meshrmm_remote_screen::background::{self, HEIGHT, WIDTH};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

pub(super) const TASKBAR_HEIGHT: i32 = 48;
const PIN_WIDTH: i32 = 48;
const ICON_SIZE: i32 = 32;
const TASKS_LEFT: i32 = 8 + PINS.len() as i32 * PIN_WIDTH + 12;
const TASK_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
/// How long a started process's first window still comes to the front.
const START_FOREGROUND: Duration = Duration::from_secs(15);
/// An application on the background taskbar.
struct Pin {
    label: &'static str,
    kind: Kind,
    /// Relative to System32. Built-in tools run the Agent itself.
    program: &'static str,
    arguments: &'static str,
    /// The file, relative to System32, and index `ExtractIconExW` takes the icon from.
    icon: (&'static str, i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Program,
    /// Built-in tools, one window each, that replace Windows' shell-bound ones.
    TaskManager,
    FileExplorer,
    Run,
}

const PINS: &[Pin] = &[
    Pin {
        label: "Command Prompt",
        kind: Kind::Program,
        program: "cmd.exe",
        arguments: "/k title MeshRMM Background Command Prompt",
        icon: ("cmd.exe", 0),
    },
    Pin {
        label: "PowerShell",
        kind: Kind::Program,
        program: "WindowsPowerShell\\v1.0\\powershell.exe",
        arguments: "-NoLogo -NoProfile -NoExit",
        icon: ("WindowsPowerShell\\v1.0\\powershell.exe", 0),
    },
    Pin {
        label: "Registry Editor",
        kind: Kind::Program,
        program: "..\\regedit.exe",
        arguments: "/m",
        icon: ("..\\regedit.exe", 0),
    },
    Pin {
        label: "Services",
        kind: Kind::Program,
        program: "mmc.exe",
        arguments: "services.msc",
        icon: ("filemgmt.dll", 0),
    },
    Pin {
        label: "Event Viewer",
        kind: Kind::Program,
        program: "mmc.exe",
        arguments: "eventvwr.msc",
        icon: ("miguiresource.dll", 0),
    },
    Pin {
        label: "Resource Monitor",
        kind: Kind::Program,
        program: "resmon.exe",
        arguments: "",
        icon: ("resmon.exe", 0),
    },
    Pin {
        label: "Task Manager",
        kind: Kind::TaskManager,
        program: "",
        arguments: "--background-task-manager",
        icon: ("taskmgr.exe", 0),
    },
    Pin {
        label: "Computer Mgmt",
        kind: Kind::Program,
        program: "mmc.exe",
        arguments: "compmgmt.msc",
        icon: ("mycomput.dll", 2),
    },
    Pin {
        label: "Device Manager",
        kind: Kind::Program,
        program: "mmc.exe",
        arguments: "devmgmt.msc",
        icon: ("devmgr.dll", 5),
    },
    Pin {
        label: "Firewall",
        kind: Kind::Program,
        program: "mmc.exe",
        arguments: "wf.msc",
        icon: ("authfwgp.dll", 0),
    },
    Pin {
        label: "File Explorer",
        kind: Kind::FileExplorer,
        program: "",
        arguments: "--background-file-browser",
        icon: ("..\\explorer.exe", 0),
    },
    Pin {
        label: "Disk Management",
        kind: Kind::Program,
        program: "mmc.exe",
        arguments: "diskmgmt.msc",
        icon: ("dmdskres.dll", 0),
    },
    // sysdm.cpl starts SystemPropertiesComputerName.exe; this opens the Advanced tab.
    Pin {
        label: "System Properties",
        kind: Kind::Program,
        program: "SystemPropertiesAdvanced.exe",
        arguments: "",
        icon: ("SystemPropertiesAdvanced.exe", 0),
    },
    Pin {
        label: "Notepad",
        kind: Kind::Program,
        program: "notepad.exe",
        arguments: "",
        icon: ("notepad.exe", 0),
    },
    Pin {
        label: "Run",
        kind: Kind::Run,
        program: "",
        arguments: "",
        icon: ("shell32.dll", 24),
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
    /// Processes started recently whose first window hasn't appeared yet.
    starting: Vec<(u32, Instant)>,
    job: HANDLE,
    pointer: POINT,
    input: inject::Injector,
    /// Buttons pressed on the taskbar, whose releases aren't sent either.
    taskbar_presses: HashSet<PointerButton>,
    shell_keys: keyboard::ShellKeys,
    /// Created the first time it opens.
    run: Option<run::Run>,
    /// Dropped last, after the applications and taskbar are gone.
    _screen: screen::Screen,
}

struct TaskButton {
    window: HWND,
    process: u32,
    button: HWND,
    icon: HICON,
    title: String,
}

impl Workspace {
    pub fn new() -> anyhow::Result<Self> {
        background::require_session_zero()?;
        profile::create_desktop_folders();
        let screen = screen::Screen::claim(TASKBAR_HEIGHT);
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
                starting: Vec::new(),
                job,
                pointer: POINT::default(),
                input: inject::Injector::default(),
                taskbar_presses: HashSet::new(),
                shell_keys: keyboard::ShellKeys::default(),
                run: None,
                _screen: screen,
            };
            let class = WNDCLASSW {
                lpfnWndProc: Some(paint::launcher_proc),
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
                lpfnWndProc: Some(paint::tooltip_proc),
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
                let (icon_file, icon_index) = pin.icon;
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
        self.starting
            .retain(|(_, started)| started.elapsed() < START_FOREGROUND);
        let visible = task_windows::task_windows(self.shell, self.tooltip)?;
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
                let icon = task_windows::task_icon(window.window, window.process, self.shell);
                SetWindowLongPtrW(button, GWLP_USERDATA, icon.0 as isize);
                // Some programs hand over to another: resmon.exe starts
                // perfmon.exe, and control.exe starts rundll32.exe.
                if let Some(index) = self.starting.iter().position(|(process, _)| {
                    *process == window.process
                        || Some(*process) == task_windows::parent_process(window.process)
                }) {
                    self.starting.swap_remove(index);
                    self.bring_forward(window.window);
                }
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
        let Some(&TaskButton {
            window, process, ..
        }) = self.tasks.get(index)
        else {
            return;
        };
        let mut current = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&mut current)) };
        if current == process && unsafe { IsWindow(Some(window)) }.as_bool() {
            self.bring_forward(window);
        }
    }

    /// Restores `window` and makes it the foreground window, which takes
    /// keyboard input, as the Windows taskbar does. `HWND_TOP` alone can't raise
    /// it above another process's foreground window.
    fn bring_forward(&self, window: HWND) {
        unsafe {
            let _ = ShowWindowAsync(
                window,
                if IsIconic(window).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            let _ = SetForegroundWindow(window);
        }
    }

    fn launch(&mut self, index: usize) -> anyhow::Result<()> {
        let pin = PINS
            .get(index.wrapping_sub(1))
            .context("unknown background application")?;
        if pin.kind == Kind::Run {
            self.open_run();
            return Ok(());
        }
        if let Some(window) = self.tool_window(pin.kind) {
            self.bring_forward(window);
            return Ok(());
        }
        let executable = match pin.kind {
            Kind::TaskManager | Kind::FileExplorer => agent_executable()?,
            _ => crate::win32::windows_directory()?
                .join("System32")
                .join(pin.program),
        };
        let started = launch::launch(launch::Launch {
            executable: Some(&executable),
            command: &format!("\"{}\" {}", executable.display(), pin.arguments),
            flags: CREATE_SUSPENDED
                | match pin.kind {
                    Kind::TaskManager | Kind::FileExplorer => CREATE_NO_WINDOW,
                    _ => CREATE_NEW_CONSOLE,
                },
            window: Some((40, 24, 1100, (HEIGHT as i32 - TASKBAR_HEIGHT - 48) as u32)),
            ..Default::default()
        })?;
        let process_id = started.id;
        self.adopt(started, pin.kind)?;
        tracing::info!(
            session_id = 0,
            process_id,
            program = pin.label,
            "background application started"
        );
        Ok(())
    }

    /// The open window of a built-in tool that allows only one.
    fn tool_window(&self, kind: Kind) -> Option<HWND> {
        let (class, owned) = match kind {
            Kind::TaskManager => (w!("MeshRMMBackgroundTasks"), &self.task_managers),
            Kind::FileExplorer => (w!("MeshRMMBackgroundFiles"), &self.file_browsers),
            _ => return None,
        };
        let window = unsafe { FindWindowW(class, None) }.ok()?;
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        (owned.iter().any(|(owned, _)| *owned == pid)
            || (kind == Kind::FileExplorer && self.owns_window(window)))
        .then_some(window)
    }

    /// Moves a suspended process into the workspace's job, then starts it. Like
    /// a program started from Windows' taskbar, its first window comes to the
    /// front: a new process doesn't get to take the foreground from the window
    /// the input went to.
    fn adopt(&mut self, started: launch::DesktopProcess, kind: Kind) -> anyhow::Result<()> {
        unsafe {
            let result = AssignProcessToJobObject(self.job, started.process.0);
            if result.is_err() || ResumeThread(started.thread.0) == u32::MAX {
                let _ = TerminateProcess(started.process.0, 1);
                result?;
                anyhow::bail!("could not resume background application");
            }
        }
        let process_id = started.id;
        self.starting.push((process_id, Instant::now()));
        match kind {
            // Retain the process handle so its PID cannot be recycled before
            // cleaning its private telemetry session on forced job shutdown.
            Kind::TaskManager => self
                .task_managers
                .push((process_id, started.process.into_raw())),
            Kind::FileExplorer => self
                .file_browsers
                .push((process_id, started.process.into_raw())),
            Kind::Program | Kind::Run => {}
        }
        Ok(())
    }

    /// Opens Run and makes it the foreground window, as Win+R does.
    fn open_run(&mut self) {
        if self.run.is_none() {
            let icon = self.pin_icon(Kind::Run);
            match run::Run::new(icon) {
                Ok(run) => self.run = Some(run),
                Err(error) => {
                    tracing::warn!(%error, "could not create the background Run dialog");
                    return;
                }
            }
        }
        let Some(run) = &mut self.run else {
            return;
        };
        if !run.visible() {
            run.previous = unsafe { GetForegroundWindow() };
        }
        run.show(work_area());
    }

    /// Runs what Run's OK asked for, or closes it on Cancel.
    fn run_action(&mut self) {
        let Some(run) = &self.run else {
            return;
        };
        match run.take_action() {
            run::Action::None => return,
            run::Action::Close => {}
            run::Action::Run => {
                let command = run.command();
                if let Err(error) = self.run_command(&command) {
                    if let Some(run) = &self.run {
                        run.set_error(&format!("{error:#}"));
                    }
                    return;
                }
            }
        }
        let Some(run) = &self.run else {
            return;
        };
        run.hide();
        // Typing goes back where it went before Run opened.
        let previous = run.previous;
        if unsafe { IsWindow(Some(previous)) }.as_bool() {
            unsafe {
                let _ = SetForegroundWindow(previous);
            }
        }
    }

    fn run_command(&mut self, command: &str) -> anyhow::Result<()> {
        let target = launch::resolve(command)?;
        let kind = match target {
            launch::Target::TaskManager => {
                let index = PINS
                    .iter()
                    .position(|pin| pin.kind == Kind::TaskManager)
                    .context("Task Manager is not pinned")?;
                return self.launch(index + 1);
            }
            launch::Target::Folder(_) => Kind::FileExplorer,
            launch::Target::Program { .. } => Kind::Program,
        };
        let started = launch::start(&target, &agent_executable()?, CREATE_SUSPENDED)?;
        let process_id = started.id;
        self.adopt(started, kind)?;
        tracing::info!(
            session_id = 0,
            process_id,
            "background Run started a program"
        );
        Ok(())
    }

    fn pin_icon(&self, kind: Kind) -> HICON {
        PINS.iter()
            .position(|pin| pin.kind == kind)
            .and_then(|index| unsafe { GetDlgItem(Some(self.shell), index as i32 + 1) }.ok())
            .map(|button| HICON(unsafe { GetWindowLongPtrW(button, GWLP_USERDATA) } as *mut _))
            .unwrap_or_default()
    }

    pub fn pump(&mut self) {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        self.run_action();
        if self.last_task_refresh.elapsed() >= TASK_REFRESH_INTERVAL {
            if let Err(error) = self.refresh_tasks() {
                tracing::warn!(%error, "could not refresh background taskbar");
            }
            self.last_task_refresh = Instant::now();
        }
    }

    /// Moves the pointer to canvas coordinates normalized to 0..=65535, and
    /// updates the taskbar's hover state.
    fn move_pointer(&mut self, x: u16, y: u16) -> anyhow::Result<()> {
        // The nearest pixel, so a pixel's own normalized coordinate maps back to it.
        self.pointer = POINT {
            x: (i32::from(x) * (WIDTH as i32 - 1) + 32767) / 65535,
            y: (i32::from(y) * (HEIGHT as i32 - 1) + 32767) / 65535,
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
                    // Capture copies the window's retained surface, so a label
                    // change must repaint it.
                    let _ = InvalidateRect(Some(self.tooltip), None, false);
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
        self.input.move_to(self.pointer)
    }

    /// Presses on the taskbar launch and restore applications here, like the
    /// Windows shell. Everything else is real input.
    fn button(&mut self, button: PointerButton, down: bool) -> anyhow::Result<()> {
        if !down && self.taskbar_presses.remove(&button) {
            return Ok(());
        }
        let window = unsafe { WindowFromPoint(self.pointer) };
        let taskbar_button = unsafe { GetParent(window) }
            .ok()
            .filter(|parent| *parent == self.shell)
            .map(|_| unsafe { GetDlgCtrlID(window) } as usize);
        if !down || (window != self.shell && taskbar_button.is_none()) {
            return self.input.button(button, down);
        }
        self.taskbar_presses.insert(button);
        match taskbar_button {
            Some(id) if button == PointerButton::Left && id <= PINS.len() => self.launch(id)?,
            Some(id) if button == PointerButton::Left => self.restore_task(id - PINS.len() - 1),
            _ => {}
        }
        Ok(())
    }

    pub fn release(&mut self) {
        if let Err(error) = self.input.release() {
            tracing::warn!(%error, "could not release background input");
        }
        self.taskbar_presses.clear();
        self.shell_keys.release();
    }

    pub fn apply(&mut self, event: RemoteInput) -> anyhow::Result<()> {
        if event.display_id().0 != background::DISPLAY_ID {
            return Ok(());
        }
        match self.shell_keys.route(&event) {
            keyboard::Route::Application => {}
            keyboard::Route::Dropped => return Ok(()),
            keyboard::Route::Run => {
                self.open_run();
                return Ok(());
            }
        }
        match event {
            RemoteInput::PointerMove { x, y, .. } => self.move_pointer(x, y),
            RemoteInput::PointerButtonAt {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                self.move_pointer(x, y)?;
                self.button(button, pressed)
            }
            RemoteInput::PointerButton {
                button, pressed, ..
            } => self.button(button, pressed),
            RemoteInput::WheelAt {
                x,
                y,
                horizontal,
                vertical,
                ..
            } => {
                self.move_pointer(x, y)?;
                self.input.wheel(horizontal, vertical)
            }
            RemoteInput::Wheel {
                horizontal,
                vertical,
                ..
            } => self.input.wheel(horizontal, vertical),
            RemoteInput::TypeText { text, .. } => self.input.text(&text),
            RemoteInput::Key {
                scan_code,
                extended,
                pressed,
                ..
            } => self.input.key(scan_code, extended, pressed),
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.release();
        self.run = None;
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

/// The Agent, which runs the built-in tools.
fn agent_executable() -> anyhow::Result<std::path::PathBuf> {
    #[cfg(test)]
    return Ok(std::env::var_os("MESHRMM_BACKGROUND_TEST_AGENT")
        .context("set MESHRMM_BACKGROUND_TEST_AGENT to the built Agent for GUI tests")?
        .into());
    #[cfg(not(test))]
    Ok(std::env::current_exe()?)
}

/// Where Session 0 maximizes windows. The workspace sets it to end above the
/// taskbar.
pub(super) fn work_area() -> RECT {
    let mut area = RECT::default();
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut area as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if result.is_err() || area.right <= area.left || area.bottom <= area.top {
        return RECT {
            left: 0,
            top: 0,
            right: WIDTH as i32,
            bottom: HEIGHT as i32 - TASKBAR_HEIGHT,
        };
    }
    area
}

/// How far a built-in tool's resize border reaches into its window. Real input
/// goes to the child window under the pointer before its frame, so the border
/// is nonclient area rather than a strip of the client.
pub(super) const RESIZE_BORDER: i32 = 4;

/// A built-in tool's nonclient border beside and below its client area: the
/// resize border, or a line while maximized.
pub(super) fn frame_border(hwnd: HWND) -> i32 {
    if unsafe { IsZoomed(hwnd) }.as_bool() {
        1
    } else {
        RESIZE_BORDER
    }
}

/// The part of a built-in tool's window its borderless frame occupies. Windows
/// maximizes a sizable window to the work area plus a standard frame overhang
/// on every side, and ignores `WM_GETMINMAXINFO` and later moves that fit the
/// work area exactly, so while maximized the frame draws inside the work area.
pub(super) fn frame_rect(hwnd: HWND, window: RECT) -> RECT {
    if !unsafe { IsZoomed(hwnd) }.as_bool() {
        return window;
    }
    let area = work_area();
    RECT {
        left: window.left.max(area.left),
        top: window.top.max(area.top),
        right: window.right.min(area.right),
        bottom: window.bottom.min(area.bottom),
    }
}

#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod tests;
