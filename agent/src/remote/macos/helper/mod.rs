//! The installed Agent's session helpers and the coordinator's link to them.
pub(crate) mod coordinator;
pub(crate) mod host;
pub(crate) mod protocol;

use crate::installer::HELPER_SOCKET;

/// The helper socket, which tests and development builds may move.
pub(crate) fn socket_path() -> std::path::PathBuf {
    std::env::var_os("MESHRMM_HELPER_SOCKET")
        .map_or_else(|| HELPER_SOCKET.into(), std::path::PathBuf::from)
}
