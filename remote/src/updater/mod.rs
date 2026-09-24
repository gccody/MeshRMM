#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

/// Wait for application initialization, rather than merely process creation.
pub(super) fn launch_verified(command: std::process::Command) -> anyhow::Result<()> {
    launch_verified_with(command, &std::env::current_exe()?.with_extension("ready"))
}

fn launch_verified_with(
    mut command: std::process::Command,
    ready: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let _ = std::fs::remove_file(ready);
    let mut child = command
        .env("MESHRMM_UPDATE_READY_FILE", ready)
        .spawn()
        .context("could not start the updated viewer")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let exited = child.try_wait()?;
        // Check readiness after the exit status: a viewer can acknowledge and
        // exit between two polls, for example when its session request fails.
        if ready.is_file() {
            let _ = std::fs::remove_file(ready);
            return Ok(());
        }
        if let Some(status) = exited {
            bail!("updated viewer exited before readiness: {status}");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("updated viewer did not acknowledge readiness");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ready_file(test: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("meshrmm-{test}-{}.ready", std::process::id()))
    }

    #[test]
    fn a_viewer_that_acknowledges_and_exits_at_once_is_success() {
        let ready = ready_file("acknowledged");
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "printf ready > \"$MESHRMM_UPDATE_READY_FILE\"; exit 3",
        ]);
        launch_verified_with(command, &ready).unwrap();
        assert!(!ready.exists());
    }

    #[test]
    fn a_spawned_process_that_exits_without_readiness_is_not_success() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        let error = launch_verified_with(command, &ready_file("silent")).unwrap_err();
        assert!(error.to_string().contains("before readiness"));
    }
}
