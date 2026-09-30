//! Association launches stay on the current maintenance desktop and inherit its job.
use crate::remote::background::launch;
use anyhow::{Context, ensure};
use std::path::Path;
use windows::Win32::System::Threading::CREATE_NEW_CONSOLE;

pub(super) fn open(path: &Path) -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    ensure!(path.is_file(), "The file no longer exists.");
    let (executable, command) =
        launch::command_for(path, "").context("Use Preview for text files")?;
    // It inherits the launcher's kill-on-close job.
    launch::launch(launch::Launch {
        executable: Some(&executable),
        command: &command,
        directory: Some(path.parent().context("File has no parent folder")?),
        flags: CREATE_NEW_CONSOLE,
        ..Default::default()
    })
    .context("The associated application could not start on the background desktop")?;
    Ok(())
}
