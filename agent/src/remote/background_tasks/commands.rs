//! Menu and button commands of the Task Manager window.
use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

impl State {
    pub(super) fn command(&mut self, id: usize) -> anyhow::Result<()> {
        match id {
            REFRESH => {
                self.tick = 0;
                self.refresh();
            }
            END_TASK => self.activate()?,
            CANCEL => {
                self.pending = None;
                self.notice.clear();
                self.run_visible = false;
            }
            COMPACT => self.toggle_compact()?,
            RUN_TASK => {
                self.run_visible = true;
                self.pending = None;
                unsafe {
                    if let Ok(edit) = GetDlgItem(Some(self.hwnd), RUN_EDIT as i32) {
                        let _ = SetFocus(Some(edit));
                    }
                }
            }
            RUN => self.run_entered_command()?,
            EXIT => unsafe {
                PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))?;
            },
            TOPMOST => {
                self.topmost = !self.topmost;
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        Some(if self.topmost {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        }),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    )?;
                }
            }
            SPEED_HIGH..=SPEED_PAUSED => {
                self.interval = [500, 1000, 4000, 0][id - SPEED_HIGH];
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), 1);
                    if self.interval > 0 {
                        SetTimer(Some(self.hwnd), 1, self.interval, None);
                    }
                }
            }
            GROUP => {
                self.grouped = !self.grouped;
                self.rebuild();
            }
            END_TREE => self.request_end(true)?,
            GO_DETAILS => self.go_to_details()?,
            PRIORITY_NORMAL..=PRIORITY_HIGH => {
                let row = self
                    .selected()
                    .and_then(|r| r.processes.first())
                    .map(|i| self.snapshot.processes[*i].clone())
                    .context("Select a process first.")?;
                let class = [
                    NORMAL_PRIORITY_CLASS.0,
                    IDLE_PRIORITY_CLASS.0,
                    HIGH_PRIORITY_CLASS.0,
                ][id - PRIORITY_NORMAL];
                self.notice = format!("Change {} priority to {}?", row.name, priority(class));
                self.pending = Some(Pending::Priority(row, class));
            }
            RESET_HISTORY => {
                self.history.clear();
                self.rebuild();
            }
            SERVICE_START | SERVICE_STOP | SERVICE_RESTART => self.request_service(id)?,
            COPY => {
                if let Some(row) = self.selected() {
                    self.notice = row.cells.join("  |  ");
                }
            }
            _ => {}
        }
        self.check_menu_items();
        Ok(())
    }
    fn toggle_compact(&mut self) -> anyhow::Result<()> {
        if !self.compact {
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(self.hwnd, &mut rect)?;
            }
            self.expanded_size = (rect.right - rect.left, rect.bottom - rect.top);
        }
        self.compact = !self.compact;
        if self.compact {
            self.tab = Tab::Processes;
            unsafe {
                SendMessageW(self.tabs, TCM_SETCURSEL, Some(WPARAM(0)), None);
            }
        }
        self.pending = None;
        self.notice.clear();
        unsafe {
            SetMenu(self.hwnd, if self.compact { None } else { Some(self.menu) })?;
        }
        let (width, height) = if self.compact {
            (350, 320)
        } else {
            self.expanded_size
        };
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        self.configure();
        Ok(())
    }
    fn run_entered_command(&mut self) -> anyhow::Result<()> {
        let edit = unsafe { GetDlgItem(Some(self.hwnd), RUN_EDIT as i32)? };
        let mut text = [0u16; 32768];
        let count = unsafe { GetWindowTextW(edit, &mut text) };
        let command = String::from_utf16_lossy(&text[..count as usize]);
        ensure!(
            !command.trim().is_empty(),
            "Enter a program and optional arguments."
        );
        launch(&command)?;
        self.run_visible = false;
        self.notice = "Task started on the isolated background desktop as SYSTEM.".into();
        self.refresh();
        Ok(())
    }
    fn go_to_details(&mut self) -> anyhow::Result<()> {
        let row = self
            .selected()
            .context("Select a process or service first.")?
            .clone();
        let pid = row
            .processes
            .first()
            .map(|i| self.snapshot.processes[*i].pid)
            .or_else(|| self.service(&row.key).map(|s| s.pid))
            .context("No process is associated with this item.")?;
        self.change_tab(Tab::Details);
        unsafe {
            SendMessageW(
                self.tabs,
                TCM_SETCURSEL,
                Some(WPARAM(Tab::Details.index())),
                None,
            );
        }
        if let Some(index) = self
            .rows
            .iter()
            .position(|r| matches!(r.key, Key::Process(id, _) if id == pid))
        {
            self.select(index);
        }
        Ok(())
    }
    fn request_service(&mut self, id: usize) -> anyhow::Result<()> {
        let s = self
            .selected()
            .and_then(|r| self.service(&r.key))
            .context("Select a service first.")?
            .clone();
        let start = id == SERVICE_START;
        ensure!(
            !crate::service::is_agent_service(&s.name),
            "The remote connection service cannot be changed here."
        );
        self.notice = format!(
            "{} service {}?",
            if id == SERVICE_RESTART {
                "Restart"
            } else if start {
                "Start"
            } else {
                "Stop"
            },
            s.name
        );
        self.pending = Some(Pending::Service(
            s.name.clone(),
            if id == SERVICE_RESTART {
                data::ServiceAction::Restart
            } else if start {
                data::ServiceAction::Start
            } else {
                data::ServiceAction::Stop
            },
        ));
        Ok(())
    }
    fn check_menu_items(&self) {
        unsafe {
            let menu = self.menu;
            let _ = CheckMenuItem(
                menu,
                TOPMOST as u32,
                MF_BYCOMMAND.0
                    | if self.topmost {
                        MF_CHECKED.0
                    } else {
                        MF_UNCHECKED.0
                    },
            );
            let _ = CheckMenuItem(
                menu,
                GROUP as u32,
                MF_BYCOMMAND.0
                    | if self.grouped {
                        MF_CHECKED.0
                    } else {
                        MF_UNCHECKED.0
                    },
            );
            for (id, speed) in [
                (SPEED_HIGH, 500),
                (SPEED_NORMAL, 1000),
                (SPEED_LOW, 4000),
                (SPEED_PAUSED, 0),
            ] {
                let _ = CheckMenuItem(
                    menu,
                    id as u32,
                    MF_BYCOMMAND.0
                        | if self.interval == speed {
                            MF_CHECKED.0
                        } else {
                            MF_UNCHECKED.0
                        },
                );
            }
        }
    }
}

/// Runs a command like the taskbar's Run. What it starts inherits this
/// process's place in the workspace job.
fn launch(command: &str) -> anyhow::Result<()> {
    use crate::remote::background::launch;
    let target = launch::resolve(command)?;
    ensure!(
        target != launch::Target::TaskManager,
        "Task Manager is already open."
    );
    launch::start(
        &target,
        &std::env::current_exe()?,
        PROCESS_CREATION_FLAGS(0),
    )?;
    Ok(())
}
