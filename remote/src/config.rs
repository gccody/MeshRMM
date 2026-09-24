use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{ArgAction, Parser};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub bootstrap: Option<meshrmm_protocol::SessionBootstrap>,
    pub server: String,
    pub handoff_token: String,
    pub update_manifest_url: String,
    pub auto_update: bool,
    pub json_logs: bool,
    /// The dashboard's device ID from the link. It only selects which viewer
    /// a new link replaces; the handoff token alone authorizes the session.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub device_id: Option<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "meshrmm-remote",
    about = "MeshRMM low-latency remote-control client"
)]
struct Arguments {
    /// Single-use MeshRMM dashboard link.
    #[arg(value_name = "DEEP_LINK")]
    deep_link: Option<String>,

    /// JSON configuration file. Defaults to remote.json beside the executable.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Base URL of the MeshRMM Cloudflare Worker.
    #[arg(long, env = "MESHRMM_SERVER")]
    server: Option<String>,

    /// Short-lived, single-use browser handoff token.
    #[arg(long, env = "MESHRMM_HANDOFF_TOKEN", hide_env_values = true)]
    handoff_token: Option<String>,

    /// HTTPS release manifest checked before starting a remote session.
    #[arg(long, env = "MESHRMM_UPDATE_MANIFEST_URL")]
    update_manifest_url: Option<String>,

    #[arg(long, env = "MESHRMM_JSON_LOGS", action = ArgAction::SetTrue)]
    json_logs: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    server: Option<String>,
    handoff_token: Option<String>,
    update_manifest_url: Option<String>,
    auto_update: Option<bool>,
    json_logs: Option<bool>,
}

#[derive(Debug)]
struct LinkedSession {
    server: String,
    handoff_token: String,
    device_id: Option<String>,
}

impl Config {
    #[cfg(not(target_os = "macos"))]
    pub fn load() -> anyhow::Result<Self> {
        Self::load_inner(None)
    }

    #[cfg(target_os = "macos")]
    pub fn load_with_deep_link(deep_link: Option<&str>) -> anyhow::Result<Self> {
        Self::load_inner(deep_link)
    }

    fn load_inner(launch_deep_link: Option<&str>) -> anyhow::Result<Self> {
        let arguments = Arguments::parse();
        let file = load_file(arguments.config.as_deref(), "remote.json")?;
        let linked = launch_deep_link
            .or(arguments.deep_link.as_deref())
            .map(session_from_deep_link)
            .transpose()?;

        let device_id = linked
            .as_ref()
            .and_then(|session| session.device_id.clone());
        let server = linked
            .as_ref()
            .map(|session| session.server.clone())
            .or(arguments.server)
            .or(file.server)
            .context("missing server URL in the MeshRMM handoff link or --server")?;
        let handoff_token = linked
            .map(|session| session.handoff_token)
            .or(arguments.handoff_token)
            .or(file.handoff_token)
            .context("missing single-use MeshRMM handoff token")?;
        validate_server(&server)?;
        validate_handoff_token(&handoff_token)?;
        let update_manifest_url = arguments
            .update_manifest_url
            .or(file.update_manifest_url)
            .unwrap_or_else(|| meshrmm_self_update::DEFAULT_MANIFEST_URL.to_owned());
        meshrmm_self_update::validate_manifest_url(&update_manifest_url)?;

        let bootstrap = std::env::var("MESHRMM_SESSION_BOOTSTRAP")
            .ok()
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .context("invalid resumed launch session")?;
        Ok(Self {
            bootstrap,
            server,
            handoff_token,
            update_manifest_url,
            auto_update: file.auto_update.unwrap_or(true),
            json_logs: arguments.json_logs || file.json_logs.unwrap_or(false),
            device_id,
        })
    }
}

fn session_from_deep_link(value: &str) -> anyhow::Result<LinkedSession> {
    let link = url::Url::parse(value).context("invalid MeshRMM deep link")?;
    if link.scheme() != "meshrmm" || link.host_str() != Some("connect") {
        anyhow::bail!("deep link must use meshrmm://connect");
    }
    let mut server = None;
    let mut handoff_token = None;
    let mut device_id = None;
    for (key, value) in link.query_pairs() {
        match key.as_ref() {
            "server" => server = Some(value.into_owned()),
            "handoff" => handoff_token = Some(value.into_owned()),
            // Older dashboards omit it; a malformed one is ignored rather than
            // failing a link whose token is valid.
            "device" if is_device_id(&value) => device_id = Some(value.into_owned()),
            _ => {}
        }
    }
    let server = server.context("MeshRMM deep link is missing the server parameter")?;
    let handoff_token =
        handoff_token.context("MeshRMM deep link is missing the handoff parameter")?;
    validate_server(&server)?;
    validate_handoff_token(&handoff_token)?;
    Ok(LinkedSession {
        server,
        handoff_token,
        device_id,
    })
}

/// The server's identifier rule, which also keeps it safe in a kernel object name.
fn is_device_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_server(value: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(value).context("MeshRMM server is not a valid URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        anyhow::bail!("MeshRMM server must use HTTPS");
    }
    Ok(())
}

fn validate_handoff_token(value: &str) -> anyhow::Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        anyhow::bail!("MeshRMM handoff token is invalid")
    }
}

fn load_file(explicit: Option<&Path>, default_name: &str) -> anyhow::Result<FileConfig> {
    let path = match explicit {
        Some(path) => path.to_owned(),
        None => std::env::current_exe()
            .context("could not locate the viewer executable")?
            .parent()
            .context("viewer executable has no parent directory")?
            .join(default_name),
    };
    if explicit.is_none() && !path.exists() {
        return Ok(FileConfig::default());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(contents.trim_start_matches('\u{feff}'))
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_use_dashboard_handoff() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let linked = session_from_deep_link(&format!(
            "meshrmm://connect?handoff={token}&server=https%3A%2F%2Fapi.example.com"
        ))
        .unwrap();
        assert_eq!(linked.server, "https://api.example.com");
        assert_eq!(linked.handoff_token, token);
        assert_eq!(linked.device_id, None);
    }

    #[test]
    fn reads_an_optional_device_id_and_ignores_a_malformed_one() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let link = |device: &str| {
            session_from_deep_link(&format!(
                "meshrmm://connect?handoff={token}&server=https%3A%2F%2Fapi.example.com&device={device}"
            ))
            .unwrap()
            .device_id
        };
        assert_eq!(
            link("3f2a9c1e-0b7d-4e21-9a55-6c0d8e1f2a3b").as_deref(),
            Some("3f2a9c1e-0b7d-4e21-9a55-6c0d8e1f2a3b")
        );
        assert_eq!(link("..%5CGlobal%5Cx"), None);
        assert_eq!(link(""), None);
        assert_eq!(link(&"a".repeat(129)), None);
    }

    #[test]
    fn rejects_permanent_or_insecure_links() {
        assert!(session_from_deep_link("meshrmm://connect?device=office-pc").is_err());
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(
            session_from_deep_link(&format!(
                "meshrmm://connect?handoff={token}&server=http%3A%2F%2Fapi.example.com"
            ))
            .is_err()
        );
    }
}
