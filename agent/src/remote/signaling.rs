pub use pulsermm_signaling_client::authenticated_websocket;
use pulsermm_signaling_client::endpoint_url;
use url::Url;

pub fn agent_connection_url(server: &str, device_id: &str) -> anyhow::Result<Url> {
    endpoint_url(server, &["v1", "agents", device_id, "connect"], &[], true)
}

pub fn session_signal_url(server: &str, session_id: &str, role: &str) -> anyhow::Result<Url> {
    endpoint_url(
        server,
        &["v1", "remote", "sessions", session_id, "signal"],
        &[("role", role)],
        true,
    )
}
