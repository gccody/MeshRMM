//! The Agent's processes log through [`AsyncLog`], so a stalled disk must not stall endpoint
//! services.
use std::sync::OnceLock;
use std::time::Duration;

pub use meshrmm_log_file::AsyncLog;

/// Records queued for the log before new ones are dropped.
pub const QUEUE_RECORDS: usize = 1024;
/// How long exit paths wait for queued records to reach the log.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

static PROCESS_LOG: OnceLock<AsyncLog> = OnceLock::new();

/// Makes `log` the log that [`flush`] waits for.
pub fn set_process_log(log: AsyncLog) {
    let _ = PROCESS_LOG.set(log);
}

/// Whether this process writes its log through [`AsyncLog`].
/// Logs a macOS session helper to `~/Library/Logs/MeshRMM/<file_name>`, or
/// to `/Library/Logs/MeshRMM` for the login window's helper, which runs as root.
#[cfg(target_os = "macos")]
pub fn initialize_helper(file_name: &str) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let directory = if unsafe { libc::geteuid() } == 0 {
        std::path::PathBuf::from("/Library/Logs/MeshRMM")
    } else {
        std::path::Path::new(
            &std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?,
        )
        .join("Library/Logs/MeshRMM")
    };
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(file_name);
    let writer = AsyncLog::new(QUEUE_RECORDS, move || {
        meshrmm_log_file::RotatingFile::open(&path)
    })?;
    set_process_log(writer.clone());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(())
}

/// Logs a Linux session helper to `$XDG_STATE_HOME/meshrmm/<file_name>`
/// (`~/.local/state/meshrmm`), or beside the Agent's configuration for the
/// processes that run as root.
#[cfg(target_os = "linux")]
pub fn initialize_helper(file_name: &str) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let directory = if unsafe { libc::geteuid() } == 0 {
        crate::installer::config_directory()?
    } else {
        match std::env::var_os("XDG_STATE_HOME").filter(|state| !state.is_empty()) {
            Some(state) => std::path::PathBuf::from(state),
            None => std::path::Path::new(
                &std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?,
            )
            .join(".local/state"),
        }
        .join("meshrmm")
    };
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(file_name);
    let writer = AsyncLog::new(QUEUE_RECORDS, move || {
        meshrmm_log_file::RotatingFile::open(&path)
    })?;
    set_process_log(writer.clone());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(())
}

pub fn has_process_log() -> bool {
    PROCESS_LOG.get().is_some()
}

/// Waits briefly for the process log to write what is queued. Call it before the process exits
/// or reports itself stopped, or the last records, often the reason it stopped, are lost.
pub fn flush() {
    if let Some(log) = PROCESS_LOG.get() {
        log.flush(FLUSH_TIMEOUT);
    }
}
