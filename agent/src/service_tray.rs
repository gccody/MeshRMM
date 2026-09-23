//! Service-owned, unprivileged tray processes, one per active user session.
use super::*;
use std::collections::HashMap;
use windows::Win32::System::RemoteDesktop::*;
use windows::Win32::System::Threading::CreateProcessAsUserW;

#[derive(Default)]
pub(super) struct Trays(HashMap<u32, TrayProcess>);

impl Trays {
    pub(super) fn refresh(&mut self) -> anyhow::Result<()> {
        let mut buffer = std::ptr::null_mut();
        let mut count = 0;
        unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }?;
        let sessions = if buffer.is_null() {
            Vec::new()
        } else {
            let sessions = unsafe { std::slice::from_raw_parts(buffer, count as usize) }
                .iter()
                .filter(|session| session.State == WTSActive && session.SessionId != 0)
                .map(|session| session.SessionId)
                .collect::<Vec<_>>();
            unsafe { WTSFreeMemory(buffer.cast()) };
            sessions
        };
        self.0
            .retain(|id, process| sessions.contains(id) && process.worker.is_running());
        for id in sessions {
            if self.0.contains_key(&id) {
                continue;
            }
            // No token is available on the sign-in screen; wait for a signed-in user.
            let mut token = HANDLE::default();
            if unsafe { WTSQueryUserToken(id, &mut token) }.is_err() {
                continue;
            }
            let token = OwnedHandle(token);
            match TrayProcess::launch(token.0) {
                Ok(process) => {
                    tracing::info!(session_id = id, "started Agent tray helper");
                    self.0.insert(id, process);
                }
                Err(error) => {
                    tracing::warn!(session_id = id, ?error, "could not start Agent tray helper")
                }
            }
        }
        Ok(())
    }
}

struct TrayProcess {
    worker: WorkerProcess,
    thread_id: u32,
}

impl TrayProcess {
    fn launch(token: HANDLE) -> anyhow::Result<Self> {
        let executable = std::env::current_exe()?;
        let executable_wide = wide(executable.as_os_str());
        let directory = wide(
            executable
                .parent()
                .context("Agent has no directory")?
                .as_os_str(),
        );
        let mut command = wide(OsStr::new(&format!(
            "\"{}\" --tray {} {}",
            executable.display(),
            active_service_name(),
            std::process::id()
        )));
        let mut desktop = wide(OsStr::new(r"winsta0\default"));
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut information = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(token),
                PCWSTR(executable_wide.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR(directory.as_ptr()),
                &startup,
                &mut information,
            )
        }
        .context("failed to launch tray as signed-in user")?;
        let _thread = OwnedHandle(information.hThread);
        Ok(Self {
            thread_id: information.dwThreadId,
            worker: WorkerProcess {
                process: OwnedHandle(information.hProcess),
            },
        })
    }
}

impl Drop for TrayProcess {
    fn drop(&mut self) {
        // Ask the UI thread to remove its icon before the process fallback.
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        let _ = unsafe { WaitForSingleObject(self.worker.process.0, 5_000) };
        // WorkerProcess provides a termination fallback for an unresponsive helper.
    }
}
