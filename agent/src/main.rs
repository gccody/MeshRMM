#[cfg(windows)]
mod installer;
mod remote;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod updater;

use remote::config::{Config, ExecutionMode};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--capture-helper")
    {
        return remote::capture_helper::run_child();
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--uninstall")
    {
        return installer::uninstall();
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--apply-agent-update")
    {
        return updater::apply_scheduled_update();
    }

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
    initialize_tracing(mode, &config)?;
    match mode {
        #[cfg(windows)]
        ExecutionMode::Service => service::run(config),
        ExecutionMode::Worker | ExecutionMode::Console => remote::run(config, mode).await,
        #[cfg(not(windows))]
        ExecutionMode::Service => anyhow::bail!("the PulseRMM Agent service requires Windows"),
    }
}

fn initialize_tracing(mode: ExecutionMode, config: &Config) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if mode != ExecutionMode::Console {
        let log_path = config.config_path.with_file_name("agent.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| {
                anyhow::anyhow!("failed to open Agent log {}: {error}", log_path.display())
            })?;
        let writer = std::sync::Mutex::new(log);
        if config.json_logs {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .json()
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .init();
        }
        return Ok(());
    }
    if config.json_logs {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    Ok(())
}
