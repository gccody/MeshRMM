#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

/// Wait for application initialization, rather than merely process creation.
pub(super) fn launch_verified(mut command: std::process::Command) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let ready = std::env::current_exe()?.with_extension("ready");
    let _ = std::fs::remove_file(&ready);
    let mut child = command
        .env("MESHRMM_UPDATE_READY_FILE", &ready)
        .spawn()
        .context("could not start the updated viewer")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("updated viewer exited before readiness: {status}");
        }
        if ready.is_file() {
            let _ = std::fs::remove_file(&ready);
            return Ok(());
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

    #[test]
    fn a_spawned_process_that_exits_without_readiness_is_not_success() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        let error = launch_verified(command).unwrap_err();
        assert!(error.to_string().contains("before readiness"));
    }
}
