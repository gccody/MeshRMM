//! Viewer-selected cleanup for the viewed Windows session once a remote session ends.
use std::sync::Mutex;

use meshrmm_protocol::{DesktopSession, SessionCloseAction};

#[derive(Default)]
struct State {
    action: SessionCloseAction,
    clear_clipboard: bool,
    target: Option<DesktopSession>,
}

#[derive(Debug, PartialEq, Eq)]
struct Pending {
    action: SessionCloseAction,
    clear_clipboard: bool,
    target: DesktopSession,
}

/// Shared by every sender attempt of one remote session, so the choices
/// survive viewer resumes and run once, when the server ends the session.
#[derive(Default)]
pub struct SessionClose {
    state: Mutex<State>,
}

impl SessionClose {
    pub fn set_action(&self, action: SessionCloseAction) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).action = action;
    }

    pub fn set_clear_clipboard(&self, enabled: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear_clipboard = enabled;
    }

    /// Records the Windows session currently shown to the viewer.
    pub fn set_target(&self, session: &DesktopSession) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.target.as_ref() != Some(session) {
            state.target = Some(session.clone());
        }
    }

    /// Returns the pending work at most once. The private Session 0
    /// background desktop has no signed-in user and no user clipboard.
    fn take(&self) -> Option<Pending> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let action = std::mem::take(&mut state.action);
        // Logging off discards the session's clipboard with everything else.
        let clear_clipboard =
            std::mem::take(&mut state.clear_clipboard) && action != SessionCloseAction::Logout;
        let target = state.target.clone()?;
        ((action != SessionCloseAction::NoAction || clear_clipboard)
            && target != DesktopSession::Background)
            .then_some(Pending {
                action,
                clear_clipboard,
                target,
            })
    }

    #[cfg(windows)]
    pub fn run(&self, session_id: &meshrmm_protocol::RemoteSessionId) {
        let Some(pending) = self.take() else {
            return;
        };
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            let Pending {
                action,
                clear_clipboard,
                target,
            } = pending;
            let user = match user_session(&target) {
                Ok(user) => user,
                Err(error) => {
                    tracing::warn!(%session_id, ?action, clear_clipboard, session = %target.label(), error = ?error, "session close cleanup skipped");
                    return;
                }
            };
            // Clear first so a locked session does not keep the copied data.
            if clear_clipboard {
                match run_helper(&user.1, "--clear-clipboard") {
                    Ok(()) => {
                        tracing::info!(%session_id, session = %target.label(), "cleared clipboard on session close")
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, session = %target.label(), error = ?error, "could not clear clipboard on session close")
                    }
                }
            }
            if action == SessionCloseAction::NoAction {
                return;
            }
            match apply(action, user) {
                Ok(()) => {
                    tracing::info!(%session_id, ?action, session = %target.label(), "ran session close action")
                }
                Err(error) => {
                    tracing::warn!(%session_id, ?action, session = %target.label(), error = ?error, "session close action failed")
                }
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

/// Windows session ID and signed-in user token of the viewed session.
#[cfg(windows)]
fn user_session(target: &DesktopSession) -> anyhow::Result<(u32, Handle)> {
    use anyhow::Context;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};

    let session = match target {
        DesktopSession::Console => unsafe { WTSGetActiveConsoleSessionId() },
        DesktopSession::Rdp { id, .. } => *id,
        DesktopSession::Background => anyhow::bail!("the background desktop has no user"),
    };
    anyhow::ensure!(
        session != u32::MAX,
        "Windows reported no active console session"
    );
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }
        .with_context(|| format!("no signed-in user in Windows session {session}"))?;
    Ok((session, Handle(token)))
}

#[cfg(windows)]
fn apply(action: SessionCloseAction, (session, token): (u32, Handle)) -> anyhow::Result<()> {
    use anyhow::Context;
    use windows::Win32::System::RemoteDesktop::WTSLogoffSession;

    match action {
        SessionCloseAction::NoAction => Ok(()),
        SessionCloseAction::Lock => run_helper(&token, "--lock-session"),
        SessionCloseAction::Logout => unsafe { WTSLogoffSession(None, session, false) }
            .with_context(|| format!("could not log off Windows session {session}")),
    }
}

/// LockWorkStation and the clipboard only affect the caller's session, so
/// helpers run as the signed-in user on that session's interactive desktop.
#[cfg(windows)]
fn run_helper(token: &Handle, argument: &str) -> anyhow::Result<()> {
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
        "\"{}\" {argument}",
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
    .with_context(|| format!("could not start the {argument} helper as the signed-in user"))?;
    let _thread = Handle(information.hThread);
    let process = Handle(information.hProcess);
    anyhow::ensure!(
        unsafe { WaitForSingleObject(process.0, 10_000) } == WAIT_OBJECT_0,
        "{argument} helper did not finish"
    );
    let mut code = 0;
    unsafe { GetExitCodeProcess(process.0, &mut code) }?;
    anyhow::ensure!(code == 0, "{argument} helper exited with code {code}");
    Ok(())
}

/// Entry point of the `--lock-session` helper launched by [`run_helper`].
#[cfg(windows)]
pub fn run_lock_helper() -> anyhow::Result<()> {
    unsafe { windows::Win32::System::Shutdown::LockWorkStation() }
        .map_err(|error| anyhow::anyhow!("LockWorkStation failed: {error}"))
}

/// Entry point of the `--clear-clipboard` helper launched by [`run_helper`].
#[cfg(windows)]
pub fn run_clear_clipboard_helper() -> anyhow::Result<()> {
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};

    // Another application may briefly hold the clipboard open.
    for _ in 0..20 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            let result = unsafe { EmptyClipboard() };
            let _ = unsafe { CloseClipboard() };
            return result.map_err(|error| anyhow::anyhow!("EmptyClipboard failed: {error}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("the clipboard stayed open by another application")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(
        action: SessionCloseAction,
        clear_clipboard: bool,
        target: DesktopSession,
    ) -> Option<Pending> {
        Some(Pending {
            action,
            clear_clipboard,
            target,
        })
    }

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
        assert_eq!(
            close.take(),
            pending(SessionCloseAction::Logout, false, rdp)
        );
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        close.set_action(SessionCloseAction::NoAction);
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        close.set_target(&DesktopSession::Background);
        assert_eq!(close.take(), None);
    }

    #[test]
    fn clipboard_clear_runs_once_and_is_skipped_by_logout() {
        let close = SessionClose::default();
        close.set_clear_clipboard(true);
        // Nothing to clear before a desktop was ever shown.
        assert_eq!(close.take(), None);
        close.set_clear_clipboard(true);
        close.set_target(&DesktopSession::Console);
        assert_eq!(
            close.take(),
            pending(SessionCloseAction::NoAction, true, DesktopSession::Console)
        );
        assert_eq!(close.take(), None);
        close.set_clear_clipboard(true);
        close.set_action(SessionCloseAction::Lock);
        assert_eq!(
            close.take(),
            pending(SessionCloseAction::Lock, true, DesktopSession::Console)
        );
        close.set_clear_clipboard(true);
        close.set_action(SessionCloseAction::Logout);
        assert_eq!(
            close.take(),
            pending(SessionCloseAction::Logout, false, DesktopSession::Console)
        );
        close.set_clear_clipboard(true);
        close.set_clear_clipboard(false);
        assert_eq!(close.take(), None);
        close.set_clear_clipboard(true);
        close.set_target(&DesktopSession::Background);
        assert_eq!(close.take(), None);
    }
}
