//! Launching helper processes into a desktop's session.
use std::path::Path;

use super::*;

pub(super) struct LaunchedHelper {
    pub(super) process: OwnedHandle,
    pub(super) process_id: u32,
    pub(super) session_id: u32,
    pub(super) input: File,
    pub(super) output: File,
    pub(super) stderr: File,
}

/// Sends one `command` to a new LocalSystem helper on `target`, reads its
/// reply with `read`, and ends the helper.
pub(super) fn ask_system_helper<T: Send + 'static>(
    target: DesktopTarget,
    command: ParentCommand,
    timeout: Duration,
    label: &'static str,
    read: impl FnOnce(&mut File) -> io::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let mut launched = launch_system_helper(target)?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut output = launched.output;
        let _ = sender.send(read(&mut output));
    });
    let stderr = thread::spawn(move || drain_child_stderr(launched.stderr));
    let result = (|| {
        write_command(&mut launched.input, &command)?;
        Ok(receiver
            .recv_timeout(timeout)
            .with_context(|| format!("{label} timed out"))??)
    })();
    terminate_and_wait(&launched.process);
    let _ = reader.join();
    let _ = stderr.join();
    result
}

pub(super) fn launch_system_helper(target: DesktopTarget) -> anyhow::Result<LaunchedHelper> {
    launch_helper(target, false)
}

pub(super) fn launch_helper(
    target: DesktopTarget,
    as_user: bool,
) -> anyhow::Result<LaunchedHelper> {
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    // A test binary can't be a helper, so ignored end-to-end tests name a
    // built Agent instead.
    #[cfg(test)]
    let executable = std::env::var_os("MESHRMM_TEST_HELPER").map_or(executable, PathBuf::from);
    let working_directory = executable
        .parent()
        .context("Agent executable has no parent directory")?;
    let session_id = helper_session_id(target)?;
    let session_token = if as_user {
        user_token(session_id)?
    } else {
        system_token(session_id)?
    };
    // Pipes start non-inheritable. Only this child's ends become inheritable,
    // just before the launch, and the explicit handle list keeps a concurrent
    // launch from passing them to another helper, which may run as the user.
    let (child_input, parent_input) = create_pipe()?;
    let (parent_output, child_output) = create_pipe()?;
    let (parent_stderr, child_stderr) = create_pipe()?;
    let child_handles = [child_input.0, child_output.0, child_stderr.0];
    let handle_list = HandleListAttribute::new(&child_handles)?;
    let executable_wide = wide(executable.as_os_str());
    let working_directory_wide = wide(working_directory.as_os_str());
    let mut command_line = wide(OsStr::new(&helper_command_line(&executable, target)));
    let mut desktop = wide(OsStr::new(&helper_desktop(target)));
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: child_input.0,
            hStdOutput: child_output.0,
            hStdError: child_stderr.0,
            ..Default::default()
        },
        lpAttributeList: handle_list.list(),
    };
    let mut environment = std::ptr::null_mut();
    if as_user {
        unsafe {
            windows::Win32::System::Environment::CreateEnvironmentBlock(
                &mut environment,
                Some(session_token.0),
                false,
            )
        }?;
    }
    let mut process_info = PROCESS_INFORMATION::default();
    let launched = child_handles
        .iter()
        .try_for_each(|&handle| unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
        })
        .context("failed to prepare the desktop-helper pipe handles")
        .and_then(|()| {
            unsafe {
                CreateProcessAsUserW(
                    Some(session_token.0),
                    PCWSTR(executable_wide.as_ptr()),
                    Some(PWSTR(command_line.as_mut_ptr())),
                    None,
                    None,
                    true,
                    CREATE_NO_WINDOW
                        | EXTENDED_STARTUPINFO_PRESENT
                        | windows::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT,
                    (!environment.is_null()).then_some(environment.cast_const()),
                    PCWSTR(working_directory_wide.as_ptr()),
                    &startup.StartupInfo,
                    &mut process_info,
                )
            }
            .map_err(anyhow::Error::from)
        });
    // The child holds its own copies now; close ours whether or not it started.
    drop(child_input);
    drop(child_output);
    drop(child_stderr);
    drop(handle_list);
    if !environment.is_null() {
        let _ =
            unsafe { windows::Win32::System::Environment::DestroyEnvironmentBlock(environment) };
    }
    launched.with_context(|| {
        format!(
            "failed to launch LocalSystem helper on winsta0\\{}",
            target.name()
        )
    })?;
    let _thread = OwnedHandle(process_info.hThread);
    Ok(LaunchedHelper {
        process: OwnedHandle(process_info.hProcess),
        process_id: process_info.dwProcessId,
        session_id,
        input: parent_input.into_file(),
        output: parent_output.into_file(),
        stderr: parent_stderr.into_file(),
    })
}

/// The session a helper on `target` runs in.
fn helper_session_id(target: DesktopTarget) -> anyhow::Result<u32> {
    let session_id = if target == DesktopTarget::Background {
        meshrmm_remote_screen::background::require_session_zero()?;
        0
    } else if let DesktopTarget::Rdp(id, _) = target {
        id
    } else {
        unsafe { WTSGetActiveConsoleSessionId() }
    };
    if session_id == NO_ACTIVE_SESSION {
        anyhow::bail!("Windows reported no active console session");
    }
    Ok(session_id)
}

/// The token of the user signed in to `session_id`.
fn user_token(session_id: u32) -> anyhow::Result<OwnedHandle> {
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session_id, &mut token) }
        .context("no signed-in user for file transfers")?;
    Ok(OwnedHandle(token))
}

/// A copy of this process's LocalSystem token, moved into `session_id`.
fn system_token(session_id: u32) -> anyhow::Result<OwnedHandle> {
    let mut process_token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut process_token) }
        .context("failed to open the LocalSystem coordinator token")?;
    let process_token = OwnedHandle(process_token);
    let mut session_token = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            process_token.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut session_token,
        )
    }
    .context("failed to duplicate the LocalSystem coordinator token")?;
    let session_token = OwnedHandle(session_token);
    unsafe {
        SetTokenInformation(
            session_token.0,
            TokenSessionId,
            (&session_id as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .context("failed to move the desktop helper token into the console session")?;
    Ok(session_token)
}

fn helper_command_line(executable: &Path, target: DesktopTarget) -> String {
    format!(
        "\"{}\" {}",
        executable.display(),
        if target == DesktopTarget::Background {
            "--background-helper"
        } else {
            "--capture-helper"
        }
    )
}

/// The window station and desktop a helper on `target` starts on.
fn helper_desktop(target: DesktopTarget) -> String {
    if target == DesktopTarget::Background {
        // Launch in Session 0's window station, then bind the helper to its
        // private desktop before starting any GUI or capture threads.
        "winsta0\\default".to_owned()
    } else {
        format!("winsta0\\{}", target.name())
    }
}
