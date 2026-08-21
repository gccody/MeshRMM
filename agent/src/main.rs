#[cfg(windows)]
mod installer;
mod remote;
#[cfg(windows)]
mod service;

use remote::config::{Config, ExecutionMode};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install the Rustls ring crypto provider"))?;

    #[cfg(windows)]
    {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        if arguments.len() == 1 && arguments[0] == "--install" {
            return installer::install_and_notify();
        }
        if arguments.is_empty() && installer::launch_if_embedded()? {
            return Ok(());
        }
    }

    let (mode, config) = Config::load()?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if config.json_logs {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    match mode {
        #[cfg(windows)]
        ExecutionMode::Service => service::run(config),
        ExecutionMode::Worker | ExecutionMode::Console => remote::run(config).await,
        #[cfg(not(windows))]
        ExecutionMode::Service => anyhow::bail!("the PulseRMM Agent service requires Windows"),
    }
}
