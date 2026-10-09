//! Confirmed actions: ending processes, controlling services and startup entries,
//! disconnecting users and changing priority.
use super::*;
use crate::win32::OwnedHandle;
use windows::Win32::System::RemoteDesktop::WTSDisconnectSession;

// The action captures identities/handles before confirmation. Refreshing or
// selecting another row cannot redirect it to a different process or service.
pub(super) enum Pending {
    End(Vec<OwnedHandle>),
    Service(String, data::ServiceAction),
    Startup(data::startup::Startup),
    Disconnect(u32),
    Priority(Process, u32),
}
impl State {
    fn execute(&mut self, pending: Pending) {
        let (sender, receiver) = mpsc::channel();
        self.action = Some(receiver);
        self.notice = "Working…".into();
        // HANDLEs are transferable between threads in this process. Transfer
        // ownership as integer values; the worker restores RAII immediately.
        let action: Box<dyn FnOnce() -> anyhow::Result<String> + Send> = match pending {
            Pending::End(handles) => {
                let handles: Vec<usize> = handles
                    .into_iter()
                    .map(|h| {
                        let raw = h.0.0 as usize;
                        std::mem::forget(h);
                        raw
                    })
                    .collect();
                Box::new(move || {
                    let handles: Vec<_> = handles
                        .into_iter()
                        .map(|h| OwnedHandle(HANDLE(h as *mut _)))
                        .collect();
                    for handle in &handles {
                        unsafe {
                            TerminateProcess(handle.0, 1)?;
                        }
                    }
                    Ok(format!("Ended {} process(es).", handles.len()))
                })
            }
            Pending::Service(name, action) => Box::new(move || {
                data::service_action(&name, action)?;
                Ok(format!(
                    "{} requested for {name}.",
                    match action {
                        data::ServiceAction::Start => "Start",
                        data::ServiceAction::Stop => "Stop",
                        data::ServiceAction::Restart => "Restart",
                    }
                ))
            }),
            Pending::Startup(row) => Box::new(move || {
                data::startup::startup_action(&row)?;
                Ok(format!(
                    "{} {}.",
                    row.name,
                    if row.enabled { "disabled" } else { "enabled" }
                ))
            }),
            Pending::Disconnect(id) => Box::new(move || {
                ensure!(id != 0, "Session 0 cannot be disconnected.");
                unsafe {
                    WTSDisconnectSession(None, id, false)?;
                }
                Ok("User disconnected.".into())
            }),
            Pending::Priority(row, class) => Box::new(move || {
                let handle = data::identified_handle(&row, PROCESS_SET_INFORMATION)?;
                unsafe {
                    SetPriorityClass(handle.0, PROCESS_CREATION_FLAGS(class))?;
                }
                Ok("Process priority changed.".into())
            }),
        };
        std::thread::spawn(move || {
            let _ = sender.send(action());
        });
    }
    pub(super) fn request_end(&mut self, tree: bool) -> anyhow::Result<()> {
        let row = self.selected().context("Select a process first.")?.clone();
        let mut indices = row.processes.clone();
        ensure!(!indices.is_empty(), "Select a process first.");
        if tree {
            let mut identities: HashSet<u32> = indices
                .iter()
                .map(|i| self.snapshot.processes[*i].pid)
                .collect();
            loop {
                let mut added = false;
                for (i, p) in self.snapshot.processes.iter().enumerate() {
                    if identities.contains(&p.parent) && !identities.contains(&p.pid) {
                        // A child must be newer than its parent; do not follow a recycled PID.
                        let parent = self
                            .snapshot
                            .processes
                            .iter()
                            .find(|parent| parent.pid == p.parent);
                        if parent.is_some_and(|parent| {
                            parent.created.zip(p.created).is_some_and(|(a, b)| a <= b)
                        }) {
                            identities.insert(p.pid);
                            indices.push(i);
                            added = true;
                        }
                    }
                }
                if !added {
                    break;
                }
            }
        }
        let protected = data::ancestors(&self.snapshot.processes, std::process::id());
        let mut handles = Vec::new();
        for index in indices.iter().rev() {
            handles.push(data::termination_handle(
                &self.snapshot.processes[*index],
                &protected,
            )?);
        }
        self.notice = format!(
            "End {} — {} process(es)? Unsaved work will be lost.",
            row.cells[0],
            handles.len()
        );
        self.pending = Some(Pending::End(handles));
        Ok(())
    }
    pub(super) fn activate(&mut self) -> anyhow::Result<()> {
        if let Some(pending) = self.pending.take() {
            self.execute(pending);
            return Ok(());
        }
        if self.tab == Tab::History {
            self.history.clear();
            return Ok(());
        }
        let row = self.selected().context("Select an item first.")?.clone();
        match (&self.tab, &row.key) {
            (Tab::Startup, _) => {
                let s = self
                    .startup(&row.key)
                    .context("Startup entry no longer exists")?
                    .clone();
                self.notice = format!(
                    "{} {} at next sign-in?",
                    if s.enabled { "Disable" } else { "Enable" },
                    s.name
                );
                self.pending = Some(Pending::Startup(s));
            }
            (Tab::Services, _) => {
                let s = self
                    .service(&row.key)
                    .context("Service no longer exists")?
                    .clone();
                let start = s.state == 1;
                ensure!(
                    s.state == 1 || s.state == 4 || s.state == 7,
                    "Wait for the pending service operation."
                );
                ensure!(
                    !crate::service::is_agent_service(&s.name),
                    "The remote connection service cannot be changed here."
                );
                self.notice = format!(
                    "{} service {}?",
                    if start { "Start" } else { "Stop" },
                    s.name
                );
                self.pending = Some(Pending::Service(
                    s.name.clone(),
                    if start {
                        data::ServiceAction::Start
                    } else {
                        data::ServiceAction::Stop
                    },
                ));
            }
            (Tab::Users, Key::User(id)) => {
                ensure!(*id != 0, "Session 0 cannot be disconnected.");
                self.notice = format!(
                    "Disconnect {}? Applications will keep running.",
                    row.cells[0]
                );
                self.pending = Some(Pending::Disconnect(*id));
            }
            _ => self.request_end(false)?,
        }
        Ok(())
    }
}
