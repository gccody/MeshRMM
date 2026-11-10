use std::path::PathBuf;

use clap::{Parser, Subcommand};
use meshrmm_server::config::{self, Config};

#[derive(Parser)]
#[command(
    version = meshrmm_self_update::CURRENT_VERSION,
    about = "The self-hosted MeshRMM server"
)]
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
    /// Remove a user's authenticator app, passkeys and recovery codes.
    ResetTwoFactor { email: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (path, required) = match cli.config {
        Some(path) => (path, true),
        None => (PathBuf::from(config::DEFAULT_CONFIG_PATH), false),
    };
    let config = Config::load(&path, required)?;
    // Logs go to stderr, so admin commands' stdout carries only their result. Every
    // return from here on drops this, which writes the lines still queued first.
    let _logging = meshrmm_server::logging::init(&config.log)?;
    meshrmm_server::install_crypto_provider();
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async {
        match cli.command.unwrap_or(Command::Serve) {
            Command::Serve => meshrmm_server::run(config).await,
            Command::CheckConfig => meshrmm_server::check(config).await,
            Command::Admin(command) => admin(config, command).await,
        }
    });
    // Don't wait for macOS signing, which can take an hour; the next start
    // signs again.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
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
