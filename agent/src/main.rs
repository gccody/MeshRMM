#[cfg(any(windows, target_os = "macos"))]
mod enrollment;
#[cfg(windows)]
mod installer;
#[cfg(target_os = "macos")]
#[path = "macos/installer.rs"]
mod installer;
mod logging;
#[cfg(target_os = "macos")]
#[path = "macos/app.rs"]
mod macos_app;
#[cfg(any(windows, target_os = "macos", test))]
mod power;
#[cfg(windows)]
mod private_directory;
mod remote;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod updater;
#[cfg(windows)]
mod win32;

use anyhow::Context;
use remote::config::{Config, ExecutionMode};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--third-party-notices")
    {
        print!("{}", meshrmm_audio::THIRD_PARTY_NOTICES);
        return Ok(());
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--tray")
    {
        return tray::run();
    }

    #[cfg(any(windows, target_os = "macos"))]
    if meshrmm_session_transport::identity::handle_command(installer::identity_directory)? {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let arguments = std::env::args().skip(1).collect::<Vec<_>>();
        match arguments
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["--install", authorization] => return installer::install(authorization),
            ["--uninstall"] => return installer::uninstall(),
            _ => {}
        }
    }

    #[cfg(target_os = "macos")]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--session-helper")
    {
        return macos_app::run_main_loop(true, remote::run_session_helper);
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--capture-helper" || argument == "--background-helper")
    {
        return remote::capture_helper::run_child();
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--lock-session")
    {
        return remote::session_close::run_lock_helper();
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--clear-clipboard")
    {
        return remote::session_close::run_clear_clipboard_helper();
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--background-task-manager")
    {
        return remote::background_tasks::run();
    }

    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--background-file-browser")
    {
        return remote::background_files::run();
    }

    // Native helpers own their threads and optional service runtime. Dispatch
    // them before entering Tokio so a helper never nests block_on inside it.
    let agent = || {
        let result = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_agent());
        if let Err(error) = &result
            && logging::has_process_log()
        {
            tracing::error!(error = ?error, "MeshRMM Agent process stopped with an error");
        }
        logging::flush();
        result
    };
    // The root coordinator has no window server connection; everything that
    // needs one runs in the session helpers.
    #[cfg(target_os = "macos")]
    return macos_app::run_main_loop(
        !std::env::args_os().any(|argument| argument == "--service"),
        agent,
    );
    #[cfg(not(target_os = "macos"))]
    agent()
}

async fn run_agent() -> anyhow::Result<()> {
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
    let executable =
        std::env::current_exe().context("could not locate the running Agent executable")?;
    tracing::info!(
        process_id = std::process::id(),
        ?mode,
        release_version = meshrmm_self_update::CURRENT_VERSION,
        package_version = env!("CARGO_PKG_VERSION"),
        executable = %executable.display(),
        config_path = %config.config_path.display(),
        "MeshRMM Agent process started"
    );
    match mode {
        #[cfg(windows)]
        ExecutionMode::Service => service::run(config),
        // launchd supervises the macOS coordinator directly.
        #[cfg(target_os = "macos")]
        ExecutionMode::Service => remote::run(config, mode).await,
        ExecutionMode::Worker | ExecutionMode::Console => remote::run(config, mode).await,
        #[cfg(not(any(windows, target_os = "macos")))]
        ExecutionMode::Service => {
            anyhow::bail!("the MeshRMM Agent service requires Windows or macOS")
        }
    }
}

fn initialize_tracing(mode: ExecutionMode, config: &Config) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if mode != ExecutionMode::Console {
        let log_path = config.config_path.with_file_name("agent.log");
        let open_path = log_path.clone();
        let writer =
            logging::AsyncLog::new(move || meshrmm_log_file::RotatingFile::open(&open_path))
                .map_err(|error| {
                    anyhow::anyhow!("failed to open Agent log {}: {error}", log_path.display())
                })?;
        logging::set_process_log(writer.clone());
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
