//! The installed Agent's session helpers and the coordinator's link to them.
//! The root coordinator runs outside every graphical session, so a helper in
//! each session captures it and shows the session's windows. On macOS launchd
//! starts the helpers, which connect to the coordinator's socket, and each
//! helper injects input into its session; on Linux the coordinator starts one
//! in each logind session itself and, since only root may open `/dev/uinput`,
//! keeps the input descriptors and does the injection itself.
pub(crate) mod coordinator;
pub(crate) mod host;
pub(crate) mod protocol;

#[cfg(target_os = "linux")]
pub(crate) use crate::remote::linux as platform;
#[cfg(target_os = "macos")]
pub(crate) use crate::remote::macos as platform;

/// The helper socket, which tests and development builds may move.
#[cfg(target_os = "macos")]
pub(crate) fn socket_path() -> std::path::PathBuf {
    std::env::var_os("MESHRMM_HELPER_SOCKET").map_or_else(
        || crate::installer::HELPER_SOCKET.into(),
        std::path::PathBuf::from,
    )
}
