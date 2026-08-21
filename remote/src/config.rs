use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{ArgAction, Parser};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub server: String,
    pub handoff_token: String,
    pub json_logs: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "pulsermm-remote",
    about = "PulseRMM low-latency remote-control client"
)]
struct Arguments {
    /// Single-use PulseRMM dashboard link.
    #[arg(value_name = "DEEP_LINK")]
    deep_link: Option<String>,

    /// JSON configuration file. Defaults to remote.json beside the executable.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Base URL of the PulseRMM Cloudflare Worker.
    #[arg(long, env = "PULSERMM_SERVER")]
    server: Option<String>,

    /// Short-lived, single-use browser handoff token.
    #[arg(long, env = "PULSERMM_HANDOFF_TOKEN", hide_env_values = true)]
    handoff_token: Option<String>,

    #[arg(long, env = "PULSERMM_JSON_LOGS", action = ArgAction::SetTrue)]
    json_logs: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    server: Option<String>,
    handoff_token: Option<String>,
    json_logs: Option<bool>,
}

#[derive(Debug)]
struct LinkedSession {
    server: String,
    handoff_token: String,
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

        let server = linked
            .as_ref()
            .map(|session| session.server.clone())
            .or(arguments.server)
            .or(file.server)
            .context("missing server URL in the PulseRMM handoff link or --server")?;
        let handoff_token = linked
            .map(|session| session.handoff_token)
            .or(arguments.handoff_token)
            .or(file.handoff_token)
            .context("missing single-use PulseRMM handoff token")?;
        validate_server(&server)?;
        validate_handoff_token(&handoff_token)?;

        Ok(Self {
            server,
            handoff_token,
            json_logs: arguments.json_logs || file.json_logs.unwrap_or(false),
        })
    }
}

fn session_from_deep_link(value: &str) -> anyhow::Result<LinkedSession> {
    let link = url::Url::parse(value).context("invalid PulseRMM deep link")?;
    if link.scheme() != "pulsermm" || link.host_str() != Some("connect") {
        anyhow::bail!("deep link must use pulsermm://connect");
    }
    let mut server = None;
    let mut handoff_token = None;
    for (key, value) in link.query_pairs() {
        match key.as_ref() {
            "server" => server = Some(value.into_owned()),
            "handoff" => handoff_token = Some(value.into_owned()),
            _ => {}
        }
    }
    let server = server.context("PulseRMM deep link is missing the server parameter")?;
    let handoff_token =
        handoff_token.context("PulseRMM deep link is missing the handoff parameter")?;
    validate_server(&server)?;
    validate_handoff_token(&handoff_token)?;
    Ok(LinkedSession {
        server,
        handoff_token,
    })
}

fn validate_server(value: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(value).context("PulseRMM server is not a valid URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        anyhow::bail!("PulseRMM server must use HTTPS");
    }
    Ok(())
}

fn validate_handoff_token(value: &str) -> anyhow::Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        anyhow::bail!("PulseRMM handoff token is invalid")
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
            "pulsermm://connect?handoff={token}&server=https%3A%2F%2Fapi.example.com"
        ))
        .unwrap();
        assert_eq!(linked.server, "https://api.example.com");
        assert_eq!(linked.handoff_token, token);
    }

    #[test]
    fn rejects_permanent_or_insecure_links() {
        assert!(session_from_deep_link("pulsermm://connect?device=office-pc").is_err());
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(
            session_from_deep_link(&format!(
                "pulsermm://connect?handoff={token}&server=http%3A%2F%2Fapi.example.com"
            ))
            .is_err()
        );
    }
}
