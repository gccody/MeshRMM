//! Viewer-selected action for the viewed Windows session once a remote session ends.
use std::sync::Mutex;

use meshrmm_protocol::{DesktopSession, SessionCloseAction};

/// Shared by every sender attempt of one remote session, so the choice
/// survives viewer resumes and runs once, when the server ends the session.
#[derive(Default)]
pub struct SessionClose {
    state: Mutex<(SessionCloseAction, Option<DesktopSession>)>,
}

impl SessionClose {
    pub fn set_action(&self, action: SessionCloseAction) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0 = action;
    }

    /// Records the Windows session currently shown to the viewer.
    pub fn set_target(&self, session: &DesktopSession) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.1.as_ref() != Some(session) {
            state.1 = Some(session.clone());
        }
    }

    /// Returns the pending action at most once. The private Session 0
    /// background desktop has no signed-in user to lock or log out.
    fn take(&self) -> Option<(SessionCloseAction, DesktopSession)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let action = std::mem::take(&mut state.0);
        let target = state.1.clone()?;
        (action != SessionCloseAction::NoAction && target != DesktopSession::Background)
            .then_some((action, target))
    }

    #[cfg(windows)]
    pub fn run(&self, session_id: &meshrmm_protocol::RemoteSessionId) {
        let Some((action, target)) = self.take() else {
            return;
        };
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || match apply(action, &target) {
            Ok(()) => {
                tracing::info!(%session_id, ?action, session = %target.label(), "ran session close action")
            }
            Err(error) => {
                tracing::warn!(%session_id, ?action, session = %target.label(), error = ?error, "session close action failed")
            }
        });
    }
}

#[cfg(windows)]
struct Handle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

#[cfg(windows)]
fn apply(action: SessionCloseAction, target: &DesktopSession) -> anyhow::Result<()> {
    use anyhow::Context;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::RemoteDesktop::{
        WTSGetActiveConsoleSessionId, WTSLogoffSession, WTSQueryUserToken,
    };

    let session = match target {
        DesktopSession::Console => unsafe { WTSGetActiveConsoleSessionId() },
        DesktopSession::Rdp { id, .. } => *id,
        DesktopSession::Background => return Ok(()),
    };
    anyhow::ensure!(
        session != u32::MAX,
        "Windows reported no active console session"
    );
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }
        .with_context(|| format!("no signed-in user in Windows session {session}"))?;
    let token = Handle(token);
    match action {
        SessionCloseAction::NoAction => Ok(()),
        SessionCloseAction::Lock => lock(&token),
        SessionCloseAction::Logout => unsafe { WTSLogoffSession(None, session, false) }
            .with_context(|| format!("could not log off Windows session {session}")),
    }
}

/// LockWorkStation only affects the caller's session and desktop, so run it
/// as the signed-in user on that session's interactive desktop.
#[cfg(windows)]
fn lock(token: &Handle) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use anyhow::Context;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessAsUserW, GetExitCodeProcess, PROCESS_INFORMATION,
        STARTUPINFOW, WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};

    let wide = |value: &std::ffi::OsStr| value.encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let executable_wide = wide(executable.as_os_str());
    let mut command = wide(std::ffi::OsStr::new(&format!(
        "\"{}\" --lock-session",
        executable.display()
    )));
    let mut desktop = wide(std::ffi::OsStr::new(r"winsta0\default"));
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            None,
            &startup,
            &mut information,
        )
    }
    .context("could not start the session lock helper as the signed-in user")?;
    let _thread = Handle(information.hThread);
    let process = Handle(information.hProcess);
    anyhow::ensure!(
        unsafe { WaitForSingleObject(process.0, 10_000) } == WAIT_OBJECT_0,
        "session lock helper did not finish"
    );
    let mut code = 0;
    unsafe { GetExitCodeProcess(process.0, &mut code) }?;
    anyhow::ensure!(code == 0, "session lock helper exited with code {code}");
    Ok(())
}

/// Entry point of the `--lock-session` helper launched by [`lock`].
#[cfg(windows)]
pub fn run_lock_helper() -> anyhow::Result<()> {
    unsafe { windows::Win32::System::Shutdown::LockWorkStation() }
        .map_err(|error| anyhow::anyhow!("LockWorkStation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_runs_once_for_the_last_viewed_session() {
        let close = SessionClose::default();
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        // No action is possible before a desktop was ever shown.
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Logout);
        close.set_target(&DesktopSession::Console);
        let rdp = DesktopSession::Rdp {
            id: 3,
            user: "user".into(),
        };
        close.set_target(&rdp);
        assert_eq!(close.take(), Some((SessionCloseAction::Logout, rdp)));
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        close.set_action(SessionCloseAction::NoAction);
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        close.set_target(&DesktopSession::Background);
        assert_eq!(close.take(), None);
    }
}
