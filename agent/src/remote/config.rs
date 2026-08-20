use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{ArgAction, Parser};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub server: String,
    pub device_id: String,
    pub agent_token: String,
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub json_logs: bool,
}

#[derive(Debug, Parser)]
#[command(name = "pulsermm-agent", about = "PulseRMM Windows endpoint Agent")]
struct Arguments {
    /// JSON configuration file. Defaults to agent.json beside the executable.
    #[arg(long)]
    config: Option<PathBuf>,

    /// HTTPS base URL of the deployed PulseRMM Cloudflare Worker.
    #[arg(long, env = "PULSERMM_SERVER")]
    server: Option<String>,

    /// Stable device identity provisioned for this Agent.
    #[arg(long, env = "PULSERMM_DEVICE_ID")]
    device_id: Option<String>,

    /// Per-device secret issued once by the company-scoped PulseRMM dashboard.
    #[arg(long, env = "PULSERMM_AGENT_TOKEN", hide_env_values = true)]
    agent_token: Option<String>,

    #[arg(long, env = "PULSERMM_REMOTE_FPS")]
    frames_per_second: Option<u32>,

    #[arg(long, env = "PULSERMM_REMOTE_BITRATE")]
    bitrate_bits_per_second: Option<u32>,

    #[arg(long, env = "PULSERMM_JSON_LOGS", action = ArgAction::SetTrue)]
    json_logs: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    server: Option<String>,
    device_id: Option<String>,
    agent_token: Option<String>,
    frames_per_second: Option<u32>,
    bitrate_bits_per_second: Option<u32>,
    json_logs: Option<bool>,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let arguments = Arguments::parse();
        let file = load_file(arguments.config.as_deref(), "agent.json")?;
        let server = arguments
            .server
            .or(file.server)
            .context("missing server URL in agent.json or --server")?;
        let device_id = arguments
            .device_id
            .or(file.device_id)
            .context("missing device ID in agent.json or --device-id")?;
        let agent_token = arguments
            .agent_token
            .or(file.agent_token)
            .context("missing Agent token in agent.json or --agent-token")?;
        let frames_per_second = arguments
            .frames_per_second
            .or(file.frames_per_second)
            .unwrap_or(60);
        let bitrate_bits_per_second = arguments
            .bitrate_bits_per_second
            .or(file.bitrate_bits_per_second)
            .unwrap_or(12_000_000);
        if frames_per_second == 0 || bitrate_bits_per_second == 0 {
            anyhow::bail!("frame rate and bitrate must be greater than zero");
        }
        Ok(Self {
            server,
            device_id,
            agent_token,
            frames_per_second,
            bitrate_bits_per_second,
            json_logs: arguments.json_logs || file.json_logs.unwrap_or(false),
        })
    }
}

fn load_file(explicit: Option<&Path>, default_name: &str) -> anyhow::Result<FileConfig> {
    let path = match explicit {
        Some(path) => path.to_owned(),
        None => std::env::current_exe()
            .context("could not locate the Agent executable")?
            .parent()
            .context("Agent executable has no parent directory")?
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
