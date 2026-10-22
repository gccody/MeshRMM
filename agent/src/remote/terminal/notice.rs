//! The company's connection notification, shown to the user signed in on the
//! console when a terminal opens, so a shell on their computer isn't a
//! secret from them. The server decides whether there is one; the Agent shows
//! it when somebody is signed in, without waiting for it, and a failure only
//! logs. It uses the remote session's notification window and closes by
//! itself.
use crate::remote::config::ExecutionMode;

/// How long the notification stays when it doesn't close itself.
#[cfg(unix)]
const VISIBLE_FOR: std::time::Duration = std::time::Duration::from_secs(15);

/// Shows `text` to the console's user, if anybody is signed in.
pub(super) fn show(mode: ExecutionMode, text: String) {
    let shown = std::thread::Builder::new()
        .name("meshrmm-terminal-notice".into())
        .spawn(move || {
            if let Err(error) = show_now(mode, text) {
                tracing::warn!(error = %format!("{error:#}"), "could not tell the computer's user about a terminal");
            }
        });
    if let Err(error) = shown {
        tracing::warn!(%error, "could not tell the computer's user about a terminal");
    }
}

/// Through the console user's session helper for the installed coordinator;
/// a development Agent already runs in that session and shows it itself.
#[cfg(unix)]
fn show_now(_mode: ExecutionMode, text: String) -> anyhow::Result<()> {
    use anyhow::Context;

    match crate::remote::helper::coordinator::registry() {
        Ok(registry) => {
            #[cfg(target_os = "macos")]
            let uid = crate::remote::macos::session_close::console_user();
            #[cfg(target_os = "linux")]
            let uid = crate::remote::linux::session_close::console_user();
            let Some(uid) = uid else {
                tracing::debug!("nobody is signed in to see the terminal notification");
                return Ok(());
            };
            registry
                .user_helper(uid)?
                .run(crate::remote::helper::protocol::Request::ShowTerminalNotice(text))
                .context("the session helper could not show it")
        }
        Err(_) => show_here(text),
    }
}

/// Shows `text` in this process, which runs in the user's session, for
/// [`VISIBLE_FOR`].
#[cfg(unix)]
pub(crate) fn show_here(text: String) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let notice = crate::remote::macos::ui::ConnectionNotification::show(&text)?;
    #[cfg(target_os = "linux")]
    let notice = crate::remote::linux::ui::connection_notification(&text)?;
    std::thread::Builder::new()
        .name("meshrmm-terminal-notice".into())
        .spawn(move || {
            std::thread::sleep(VISIBLE_FOR);
            drop(notice);
        })?;
    Ok(())
}

/// Starts the `--terminal-notice` helper as the signed-in user, on their
/// desktop, and leaves it to close by itself.
#[cfg(windows)]
fn show_now(mode: ExecutionMode, text: String) -> anyhow::Result<()> {
    use anyhow::Context;
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};

    use crate::win32::{OwnedHandle, wide};

    let Some(user) = crate::remote::toolbox::windows::signed_in_user(mode) else {
        tracing::debug!("nobody is signed in to see the terminal notification");
        return Ok(());
    };
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let executable_wide = wide(executable.as_os_str());
    let mut command = wide(format!(
        "\"{}\" --terminal-notice {}",
        executable.display(),
        hex(text.as_bytes())
    ));
    let mut desktop = wide(r"winsta0\default");
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(user.token.0),
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
    .context("could not start the notification as the signed-in user")?;
    drop(OwnedHandle(information.hThread));
    drop(OwnedHandle(information.hProcess));
    Ok(())
}

#[cfg_attr(not(windows), allow(dead_code))]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// Entry point of `--terminal-notice <hex UTF-8 text>`: shows the
/// notification and exits once it has closed itself.
#[cfg(windows)]
pub(crate) fn run_notice_helper(encoded: &str) -> anyhow::Result<()> {
    use anyhow::Context;

    let text = unhex(encoded)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .context("the notification text is not hex-encoded UTF-8")?;
    crate::remote::connection_notification::NotificationWindow::show(&text)?.wait_closed();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_text_survives_the_command_line() {
        let text = "Ada Lovelace has connected to this computer.\n“quoted” & <tags>";
        assert_eq!(unhex(&hex(text.as_bytes())).unwrap(), text.as_bytes());
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(""), Some(Vec::new()));
    }
}
