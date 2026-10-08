use std::path::PathBuf;

use clap::{Parser, Subcommand};
use meshrmm_server::config::{self, Config, LogFormat};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "The self-hosted MeshRMM server")]
struct Cli {
    /// The configuration file. Settings can also come from MESHRMM_*
    /// environment variables, e.g. MESHRMM_TLS__MODE=proxy.
    #[arg(long, short, env = "MESHRMM_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (the default).
    Serve,
    /// Check the configuration and database connection, then exit.
    CheckConfig,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (path, required) = match cli.config {
        Some(path) => (path, true),
        None => (PathBuf::from(config::DEFAULT_CONFIG_PATH), false),
    };
    let config = Config::load(&path, required)?;
    init_logging(&config);
    meshrmm_server::install_crypto_provider();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        match cli.command.unwrap_or(Command::Serve) {
            Command::Serve => meshrmm_server::run(config).await,
            Command::CheckConfig => meshrmm_server::check(config).await,
        }
    })
}

fn init_logging(config: &Config) {
    let filter = EnvFilter::try_new(&config.log.level).unwrap_or_else(|error| {
        eprintln!(
            "log.level {:?} is invalid ({error}); using info",
            config.log.level
        );
        EnvFilter::new("info")
    });
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log.format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}
