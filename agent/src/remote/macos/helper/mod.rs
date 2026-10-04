//! The installed Agent's session helpers and the coordinator's link to them.
pub(crate) mod coordinator;
pub(crate) mod host;
pub(crate) mod protocol;

/// Where the root coordinator accepts session helpers. Only root can create
/// files in `/var/run`, so no other process can take the name first.
pub(crate) const SOCKET: &str = "/var/run/com.meshrmm.agent.sock";

/// The helper socket, which tests and development builds may move.
pub(crate) fn socket_path() -> std::path::PathBuf {
    std::env::var_os("MESHRMM_HELPER_SOCKET")
        .map_or_else(|| SOCKET.into(), std::path::PathBuf::from)
}
