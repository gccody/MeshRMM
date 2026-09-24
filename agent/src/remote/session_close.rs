//! Viewer-selected cleanup for the viewed Windows session once a remote session ends.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use meshrmm_protocol::{DesktopSession, SessionCloseAction};

#[cfg(windows)]
use crate::win32::OwnedHandle;

/// How long a resolved logon is reused while the viewer keeps showing the same session.
const LOGON_REFRESH: Duration = Duration::from_secs(1);

/// The Windows sign-in shown to the viewer. Close actions may run minutes after the
/// viewer left, so they only reach this logon, never a user who signed in later.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Logon {
    session: u32,
    /// Logon session LUID of the user's token, unique for each sign-in until reboot.
    logon_id: u64,
    user: String,
}

struct Target {
    session: DesktopSession,
    logon: Option<Logon>,
    resolved_at: Instant,
}

#[derive(Default)]
struct State {
    action: SessionCloseAction,
    clear_clipboard: bool,
    target: Option<Target>,
    /// A blocking task is resolving the logon of the recorded session again.
    refreshing: bool,
}

/// How to bring the recorded target up to date with the viewed session.
#[derive(Debug, PartialEq, Eq)]
enum Refresh {
    /// The viewer switched sessions, so resolve before a close could use the old one.
    Now,
    /// Look for a user switch in the same session without holding up the caller.
    Background,
}

#[derive(Debug, PartialEq, Eq)]
struct Pending {
    action: SessionCloseAction,
    clear_clipboard: bool,
    target: DesktopSession,
    logon: Option<Logon>,
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

    /// Records the Windows session currently shown to the viewer and the user
    /// signed in to it. Resolving the user queries Terminal Services, which can
    /// stall during a sign-in, so the periodic check of an unchanged session runs
    /// on a blocking thread instead of the caller's runtime worker.
    #[cfg(windows)]
    pub fn set_target(self: &std::sync::Arc<Self>, session: &DesktopSession) {
        let now = Instant::now();
        match self.refresh(session, now) {
            None => {}
            Some(Refresh::Now) => {
                let logon = resolve(session, resolve_logon);
                self.record(session, logon, now, false);
            }
            Some(Refresh::Background) => {
                let close = std::sync::Arc::clone(self);
                let session = session.clone();
                tokio::task::spawn_blocking(move || {
                    let logon = resolve(&session, resolve_logon);
                    close.record(&session, logon, Instant::now(), true);
                });
            }
        }
    }

    /// Resolves the target on the calling thread, as [`Self::set_target`] does after a switch.
    #[cfg(test)]
    fn set_target_with(
        &self,
        session: &DesktopSession,
        now: Instant,
        resolver: impl FnOnce(&DesktopSession) -> anyhow::Result<Logon>,
    ) {
        if let Some(refresh) = self.refresh(session, now) {
            let refreshed = refresh == Refresh::Background;
            self.record(session, resolve(session, resolver), now, refreshed);
        }
    }

    fn refresh(&self, session: &DesktopSession, now: Instant) -> Option<Refresh> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(target) = state
            .target
            .as_ref()
            .filter(|target| target.session == *session)
        else {
            return Some(Refresh::Now);
        };
        // The background desktop never has a user to look for.
        if state.refreshing
            || *session == DesktopSession::Background
            || now.saturating_duration_since(target.resolved_at) < LOGON_REFRESH
        {
            return None;
        }
        state.refreshing = true;
        Some(Refresh::Background)
    }

    /// `refreshed` marks the result of a background refresh, which a switch to
    /// another session since it started makes obsolete.
    fn record(
        &self,
        session: &DesktopSession,
        logon: Option<Logon>,
        now: Instant,
        refreshed: bool,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if refreshed {
            state.refreshing = false;
            if state
                .target
                .as_ref()
                .is_none_or(|target| target.session != *session)
            {
                return;
            }
        }
        if state.target.as_ref().map(|target| &target.logon) != Some(&logon) {
            match &logon {
                Some(logon) => {
                    tracing::info!(session = %session.label(), windows_session = logon.session, user = %logon.user, "viewed Windows logon recorded for session close")
                }
                None => {
                    tracing::info!(session = %session.label(), "viewed session has no signed-in user for session close")
                }
            }
        }
        state.target = Some(Target {
            session: session.clone(),
            logon,
            resolved_at: now,
        });
    }

    /// Returns the pending work at most once. The private Session 0
    /// background desktop has no signed-in user and no user clipboard.
    fn take(&self) -> Option<Pending> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let action = std::mem::take(&mut state.action);
        // Logging off discards the session's clipboard with everything else.
        let clear_clipboard =
            std::mem::take(&mut state.clear_clipboard) && action != SessionCloseAction::Logout;
        let target = state.target.as_ref()?;
        ((action != SessionCloseAction::NoAction || clear_clipboard)
            && target.session != DesktopSession::Background)
            .then(|| Pending {
                action,
                clear_clipboard,
                target: target.session.clone(),
                logon: target.logon.clone(),
            })
    }

    #[cfg(windows)]
    pub fn run(&self, session_id: &meshrmm_protocol::RemoteSessionId) {
        self.spawn(session_id);
    }

    /// Runs the pending work like [`Self::run`] and waits for it, for a coordinator that is
    /// about to exit.
    #[cfg(windows)]
    pub async fn finish(&self, session_id: &meshrmm_protocol::RemoteSessionId) {
        if let Some(task) = self.spawn(session_id) {
            let _ = task.await;
        }
    }

    #[cfg(windows)]
    fn spawn(
        &self,
        session_id: &meshrmm_protocol::RemoteSessionId,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let pending = self.take()?;
        let session_id = session_id.clone();
        Some(tokio::task::spawn_blocking(move || {
            let Pending {
                action,
                clear_clipboard,
                target,
                logon,
            } = pending;
            let user = match user_session(&target).and_then(|(current, token)| {
                same_logon(logon.as_ref(), &current).map(|()| (current, token))
            }) {
                Ok(user) => user,
                Err(error) => {
                    tracing::warn!(%session_id, ?action, clear_clipboard, session = %target.label(), windows_session = logon.as_ref().map(|logon| logon.session), user = logon.as_ref().map(|logon| logon.user.as_str()), error = ?error, "session close cleanup skipped");
                    return;
                }
            };
            let (windows_session, name) = (user.0.session, user.0.user.clone());
            // Clear first so a locked session does not keep the copied data.
            if clear_clipboard {
                match run_helper(&user.1, "--clear-clipboard") {
                    Ok(()) => {
                        tracing::info!(%session_id, session = %target.label(), windows_session, user = %name, "cleared clipboard on session close")
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, session = %target.label(), windows_session, user = %name, error = ?error, "could not clear clipboard on session close")
                    }
                }
            }
            if action == SessionCloseAction::NoAction {
                return;
            }
            match apply(action, user) {
                Ok(()) => {
                    tracing::info!(%session_id, ?action, session = %target.label(), windows_session, user = %name, "ran session close action")
                }
                Err(error) => {
                    tracing::warn!(%session_id, ?action, session = %target.label(), windows_session, user = %name, error = ?error, "session close action failed")
                }
            }
        }))
    }
}

/// Succeeds when `current` is the same sign-in the viewer was shown.
fn same_logon(recorded: Option<&Logon>, current: &Logon) -> anyhow::Result<()> {
    let recorded = recorded
        .ok_or_else(|| anyhow::anyhow!("no signed-in user was seen in the viewed session"))?;
    anyhow::ensure!(
        recorded.session == current.session && recorded.logon_id == current.logon_id,
        "the viewed sign-in of {} (Windows session {}) is no longer current; {} is now signed in to Windows session {}",
        recorded.user,
        recorded.session,
        current.user,
        current.session
    );
    Ok(())
}

#[cfg(windows)]
fn resolve_logon(target: &DesktopSession) -> anyhow::Result<Logon> {
    user_session(target).map(|(logon, _)| logon)
}

/// Current sign-in and user token of the viewed session.
#[cfg(windows)]
fn user_session(target: &DesktopSession) -> anyhow::Result<(Logon, OwnedHandle)> {
    use anyhow::Context;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_STATISTICS, TokenStatistics};
    use windows::Win32::System::RemoteDesktop::{
        WTSDomainName, WTSGetActiveConsoleSessionId, WTSQueryUserToken, WTSUserName,
    };

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
    let token = OwnedHandle(token);
    let mut statistics = TOKEN_STATISTICS::default();
    let mut length = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenStatistics,
            Some((&raw mut statistics).cast()),
            std::mem::size_of::<TOKEN_STATISTICS>() as u32,
            &mut length,
        )
    }
    .with_context(|| format!("could not read the sign-in of Windows session {session}"))?;
    let id = statistics.AuthenticationId;
    let user = match (
        session_text(session, WTSDomainName),
        session_text(session, WTSUserName),
    ) {
        (Some(domain), Some(name)) if !domain.is_empty() => format!(r"{domain}\{name}"),
        (_, Some(name)) => name,
        _ => "unknown user".into(),
    };
    Ok((
        Logon {
            session,
            logon_id: (u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart),
            user,
        },
        token,
    ))
}

#[cfg(windows)]
fn session_text(
    session: u32,
    class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
) -> Option<String> {
    use windows::Win32::System::RemoteDesktop::{WTSFreeMemory, WTSQuerySessionInformationW};
    use windows::core::PWSTR;

    let mut value = PWSTR::null();
    let mut bytes = 0;
    unsafe { WTSQuerySessionInformationW(None, session, class, &mut value, &mut bytes) }.ok()?;
    if value.is_null() {
        return None;
    }
    let text = unsafe { value.to_string() }.ok();
    unsafe { WTSFreeMemory(value.0.cast()) };
    text
}

#[cfg(windows)]
fn apply(action: SessionCloseAction, (logon, token): (Logon, OwnedHandle)) -> anyhow::Result<()> {
    use anyhow::Context;
    use windows::Win32::System::RemoteDesktop::WTSLogoffSession;

    match action {
        SessionCloseAction::NoAction => Ok(()),
        SessionCloseAction::Lock => run_helper(&token, "--lock-session"),
        SessionCloseAction::Logout => unsafe { WTSLogoffSession(None, logon.session, false) }
            .with_context(|| format!("could not log off Windows session {}", logon.session)),
    }
}

/// LockWorkStation and the clipboard only affect the caller's session, so
/// helpers run as the signed-in user on that session's interactive desktop.
#[cfg(windows)]
fn run_helper(token: &OwnedHandle, argument: &str) -> anyhow::Result<()> {
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
    let _thread = OwnedHandle(information.hThread);
    let process = OwnedHandle(information.hProcess);
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

/// The user signed in to `session`. The console may be at the sign-in screen
/// with no user, and the background desktop never has one, which leaves nothing
/// to act on.
fn resolve(
    session: &DesktopSession,
    resolver: impl FnOnce(&DesktopSession) -> anyhow::Result<Logon>,
) -> Option<Logon> {
    (*session != DesktopSession::Background)
        .then(|| resolver(session).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logon(session: u32, logon_id: u64) -> Logon {
        Logon {
            session,
            logon_id,
            user: format!("user{logon_id}"),
        }
    }

    fn view(close: &SessionClose, session: &DesktopSession, now: Instant, logon: Option<Logon>) {
        close.set_target_with(session, now, |_| {
            logon.ok_or_else(|| anyhow::anyhow!("no signed-in user"))
        });
    }

    fn pending(
        action: SessionCloseAction,
        clear_clipboard: bool,
        target: DesktopSession,
        logon: Option<Logon>,
    ) -> Option<Pending> {
        Some(Pending {
            action,
            clear_clipboard,
            target,
            logon,
        })
    }

    #[test]
    fn action_runs_once_for_the_last_viewed_session() {
        let now = Instant::now();
        let close = SessionClose::default();
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        // No action is possible before a desktop was ever shown.
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Logout);
        view(&close, &DesktopSession::Console, now, Some(logon(1, 10)));
        let rdp = DesktopSession::Rdp {
            id: 3,
            user: "user".into(),
        };
        view(&close, &rdp, now, Some(logon(3, 30)));
        assert_eq!(
            close.take(),
            pending(SessionCloseAction::Logout, false, rdp, Some(logon(3, 30)))
        );
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        close.set_action(SessionCloseAction::NoAction);
        assert_eq!(close.take(), None);
        close.set_action(SessionCloseAction::Lock);
        view(&close, &DesktopSession::Background, now, Some(logon(0, 1)));
        assert_eq!(close.take(), None);
    }

    #[test]
    fn clipboard_clear_runs_once_and_is_skipped_by_logout() {
        let now = Instant::now();
        let console = Some(logon(1, 10));
        let close = SessionClose::default();
        close.set_clear_clipboard(true);
        // Nothing to clear before a desktop was ever shown.
        assert_eq!(close.take(), None);
        close.set_clear_clipboard(true);
        view(&close, &DesktopSession::Console, now, console.clone());
        assert_eq!(
            close.take(),
            pending(
                SessionCloseAction::NoAction,
                true,
                DesktopSession::Console,
                console.clone()
            )
        );
        assert_eq!(close.take(), None);
        close.set_clear_clipboard(true);
        close.set_action(SessionCloseAction::Lock);
        assert_eq!(
            close.take(),
            pending(
                SessionCloseAction::Lock,
                true,
                DesktopSession::Console,
                console.clone()
            )
        );
        close.set_clear_clipboard(true);
        close.set_action(SessionCloseAction::Logout);
        assert_eq!(
            close.take(),
            pending(
                SessionCloseAction::Logout,
                false,
                DesktopSession::Console,
                console
            )
        );
        close.set_clear_clipboard(true);
        close.set_clear_clipboard(false);
        assert_eq!(close.take(), None);
        close.set_clear_clipboard(true);
        view(&close, &DesktopSession::Background, now, None);
        assert_eq!(close.take(), None);
    }

    #[test]
    fn viewed_logon_follows_the_session_while_it_is_shown() {
        let now = Instant::now();
        let close = SessionClose::default();
        close.set_action(SessionCloseAction::Lock);
        view(&close, &DesktopSession::Console, now, Some(logon(1, 10)));
        // The same session is not queried again within the refresh interval.
        close.set_target_with(&DesktopSession::Console, now + LOGON_REFRESH / 2, |_| {
            panic!("resolved again too soon")
        });
        // A console user switch while the viewer watches retargets the close action.
        view(
            &close,
            &DesktopSession::Console,
            now + LOGON_REFRESH,
            Some(logon(2, 20)),
        );
        assert_eq!(
            close.take(),
            pending(
                SessionCloseAction::Lock,
                false,
                DesktopSession::Console,
                Some(logon(2, 20))
            )
        );
        // A sign-in screen leaves no user to act on.
        close.set_action(SessionCloseAction::Lock);
        view(
            &close,
            &DesktopSession::Console,
            now + LOGON_REFRESH * 2,
            None,
        );
        assert_eq!(
            close.take(),
            pending(
                SessionCloseAction::Lock,
                false,
                DesktopSession::Console,
                None
            )
        );
        // The background desktop never has a user to resolve.
        view(&close, &DesktopSession::Console, now, Some(logon(1, 10)));
        close.set_target_with(&DesktopSession::Background, now, |_| {
            panic!("resolved the background desktop")
        });
    }

    #[test]
    fn close_actions_only_reach_the_viewed_sign_in() {
        let viewed = logon(1, 10);
        assert!(same_logon(Some(&viewed), &logon(1, 10)).is_ok());
        // Another user signed in at the console after the viewer left.
        let error = same_logon(Some(&viewed), &logon(2, 20)).unwrap_err();
        assert!(error.to_string().contains("user20 is now signed in"));
        // The viewed user signed out and someone signed in to the reused session.
        assert!(same_logon(Some(&viewed), &logon(1, 11)).is_err());
        // The same logon ID in another session is a different sign-in.
        assert!(same_logon(Some(&viewed), &logon(2, 10)).is_err());
        // Nobody was signed in while the viewer watched.
        assert!(same_logon(None, &logon(1, 10)).is_err());
    }
}
