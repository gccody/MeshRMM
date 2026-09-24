//! Starts programs on the Session 0 background desktop.
use std::path::Path;

use anyhow::Context;
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTF_USEPOSITION,
    STARTF_USESIZE, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::win32::{OwnedHandle, wide};

/// What to start and how.
#[derive(Default)]
pub(crate) struct Launch<'a> {
    /// The program, or `None` to take it from the start of `command`.
    pub(crate) executable: Option<&'a Path>,
    pub(crate) command: &'a str,
    /// The working directory, or `None` for the Agent's.
    pub(crate) directory: Option<&'a Path>,
    pub(crate) flags: PROCESS_CREATION_FLAGS,
    /// The first window's position and size.
    pub(crate) window: Option<(i32, i32, u32, u32)>,
}

pub(crate) struct DesktopProcess {
    pub(crate) id: u32,
    pub(crate) process: OwnedHandle,
    pub(crate) thread: OwnedHandle,
}

/// Starts a process on the background desktop without ShellExecute or DDE,
/// which could activate it in another session. It inherits the caller's
/// kill-on-close job unless the flags say otherwise.
pub(crate) fn launch(launch: Launch) -> anyhow::Result<DesktopProcess> {
    let executable = launch.executable.map(wide);
    let mut command = wide(launch.command);
    let directory = launch.directory.map(wide);
    let mut desktop = wide(meshrmm_remote_screen::background::desktop_path()?);
    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    if let Some((x, y, width, height)) = launch.window {
        startup.dwFlags |= STARTF_USEPOSITION | STARTF_USESIZE;
        startup.dwX = x as u32;
        startup.dwY = y as u32;
        startup.dwXSize = width;
        startup.dwYSize = height;
    }
    let pointer = |value: &Option<Vec<u16>>| {
        value
            .as_ref()
            .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr()))
    };
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            pointer(&executable),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            launch.flags,
            None,
            pointer(&directory),
            &startup,
            &mut information,
        )
    }
    .context("could not start a program on the background desktop")?;
    Ok(DesktopProcess {
        id: information.dwProcessId,
        process: OwnedHandle(information.hProcess),
        thread: OwnedHandle(information.hThread),
    })
}
