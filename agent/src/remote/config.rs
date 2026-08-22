use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{ArgAction, Parser};
use serde::Deserialize;

#[derive(Debug, Clone)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Config {
    pub config_path: PathBuf,
    pub server: String,
    pub device_id: String,
    pub agent_token: String,
    pub update_manifest_url: String,
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub json_logs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Service,
    Worker,
    Console,
}

#[derive(Debug, Parser)]
#[command(name = "pulsermm-agent", about = "PulseRMM Windows endpoint Agent")]
struct Arguments {
    /// Run as the Windows Service Control Manager entry point.
    #[arg(long, hide = true, conflicts_with_all = ["worker", "console"])]
    service: bool,

    /// Run the SYSTEM capture worker in the active desktop session.
    #[arg(long, hide = true, conflicts_with_all = ["service", "console"])]
    worker: bool,

    /// Run interactively for local development only.
    #[arg(long, conflicts_with_all = ["service", "worker"])]
    console: bool,

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

    /// HTTPS release manifest used for automatic Agent updates.
    #[arg(long, env = "PULSERMM_UPDATE_MANIFEST_URL")]
    update_manifest_url: Option<String>,

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
    update_manifest_url: Option<String>,
    frames_per_second: Option<u32>,
    bitrate_bits_per_second: Option<u32>,
    json_logs: Option<bool>,
}

impl Config {
    pub fn load() -> anyhow::Result<(ExecutionMode, Self)> {
        let arguments = Arguments::parse();
        let mode = if arguments.service {
            ExecutionMode::Service
        } else if arguments.worker {
            ExecutionMode::Worker
        } else if arguments.console {
            ExecutionMode::Console
        } else {
            anyhow::bail!(
                "this Agent must be installed as a Windows service from the PulseRMM dashboard; use --console only for local development"
            );
        };
        let config_path = resolve_path(arguments.config.as_deref(), "agent.json")?;
        let file = load_file(&config_path, arguments.config.is_some())?;
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
        let update_manifest_url = arguments
            .update_manifest_url
            .or(file.update_manifest_url)
            .unwrap_or_else(|| pulsermm_self_update::DEFAULT_MANIFEST_URL.to_owned());
        pulsermm_self_update::validate_manifest_url(&update_manifest_url)?;
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
        Ok((
            mode,
            Self {
                config_path,
                server,
                device_id,
                agent_token,
                update_manifest_url,
                frames_per_second,
                bitrate_bits_per_second,
                json_logs: arguments.json_logs || file.json_logs.unwrap_or(false),
            },
        ))
    }
}

fn resolve_path(explicit: Option<&Path>, default_name: &str) -> anyhow::Result<PathBuf> {
    Ok(match explicit {
        Some(path) => path.to_owned(),
        None => std::env::current_exe()
            .context("could not locate the Agent executable")?
            .parent()
            .context("Agent executable has no parent directory")?
            .join(default_name),
    })
}

fn load_file(path: &Path, explicit: bool) -> anyhow::Result<FileConfig> {
    if !explicit && !path.exists() {
        return Ok(FileConfig::default());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(contents.trim_start_matches('\u{feff}'))
        .with_context(|| format!("invalid JSON in {}", path.display()))
}
