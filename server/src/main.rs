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
    /// Repair accounts when nobody can sign in to fix them.
    #[command(subcommand)]
    Admin(Admin),
}

#[derive(Subcommand)]
enum Admin {
    /// Create a user and print a link to choose their password.
    CreateUser {
        email: String,
        /// The display name; defaults to the email address.
        #[arg(long)]
        name: Option<String>,
        /// A role ID or name to grant; repeatable. Defaults to Administrator.
        #[arg(long = "role")]
        roles: Vec<String>,
    },
    /// Print a link that sets a new password, and enable the account.
    ResetPassword { email: String },
    /// Remove a user's authenticator app and recovery codes.
    ResetTwoFactor { email: String },
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
            Command::Admin(command) => admin(config, command).await,
        }
    })
}

async fn admin(config: Config, command: Admin) -> anyhow::Result<()> {
    let state = meshrmm_server::prepare(config).await?;
    let result = match command {
        Admin::CreateUser { email, name, roles } => {
            let name = name.unwrap_or_else(|| email.clone());
            meshrmm_server::admin::create_user(&state, &email, &name, &roles).await
        }
        Admin::ResetPassword { email } => {
            meshrmm_server::admin::reset_password(&state, &email).await
        }
        Admin::ResetTwoFactor { email } => {
            meshrmm_server::admin::reset_two_factor(&state, &email).await
        }
    };
    state.database.close().await;
    println!("{}", result?);
    Ok(())
}

fn init_logging(config: &Config) {
    let filter = EnvFilter::try_new(&config.log.level).unwrap_or_else(|error| {
        eprintln!(
            "log.level {:?} is invalid ({error}); using info",
            config.log.level
        );
        EnvFilter::new("info")
    });
    // Logs go to stderr, so admin commands' stdout carries only their result.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match config.log.format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}
