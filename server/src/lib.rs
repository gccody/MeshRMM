//! The self-hosted MeshRMM server: the website, the API, Agent and viewer
//! connections, and downloads, for one company.
pub mod admin;
pub mod agents;
pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod health;
pub mod http;
pub mod mail;
pub mod maintenance;
pub mod rbac;
pub mod realtime;
pub mod secrets;
pub mod serve;
pub mod settings;
pub mod storage;
pub mod time;
pub mod toolbox;
pub mod turn;
pub mod users;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use axum_server::Handle;

use crate::{
    auth::AuthState,
    config::Config,
    db::Database,
    http::AppState,
    realtime::{AgentHub, Presence, Sessions, presence::UPDATE_GRACE, sessions::Timeouts},
    secrets::InstanceKey,
    storage::Storage,
    turn::Turn,
};

/// How long in-flight requests get to finish after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Uses `ring` for every TLS connection. Call once at startup.
pub fn install_crypto_provider() {
    // Fails only if a provider is already installed, which is the same one.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Opens the data directory and database, then builds the application state.
pub async fn prepare(config: Config) -> anyhow::Result<AppState> {
    secrets::ensure_data_dir(&config.data_dir)?;
    let instance_key = InstanceKey::load_or_create(&config.data_dir)?;
    let storage = Storage::open(&config.data_dir)?;
    let database =
        Database::connect(&config.database_url(), config.database.max_connections).await?;
    database.migrate().await?;
    tracing::info!(
        backend = ?database.backend(),
        schema_version = database.backend().expected_schema_version(),
        "database ready"
    );
    let agents = AgentHub::default();
    let idle = Duration::from_secs(config.remote.idle_timeout_seconds);
    Ok(AppState {
        config: Arc::new(config),
        presence: Presence::new(database.clone(), agents.clone(), UPDATE_GRACE),
        sessions: Sessions::new(Timeouts::new(idle)),
        turn: Turn::new(&instance_key),
        database,
        instance_key,
        auth: Arc::new(AuthState::default()),
        storage,
        agents,
    })
}

/// While no account exists, issues a setup token and logs the link that
/// creates the first administrator. Returns the link.
pub async fn announce_setup(state: &AppState) -> anyhow::Result<Option<String>> {
    if users::count(&mut &state.database).await? > 0 {
        return Ok(None);
    }
    let token = state.auth.setup.issue();
    let link = format!("{}/setup#token={token}", state.config.public_origin());
    tracing::warn!(
        %link,
        "no accounts exist yet; open this link to create the first administrator (it changes on every restart)"
    );
    Ok(Some(link))
}

/// Runs the server until SIGINT or SIGTERM, then drains connections.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let state = prepare(config).await?;
    announce_setup(&state).await?;
    let restored = state.sessions.restore(&state).await?;
    if restored > 0 {
        tracing::info!(
            restored,
            "resumed remote sessions that were live at shutdown"
        );
    }
    state.turn.start(&state.config).await?;
    let maintenance = maintenance::spawn(state.database.clone(), state.storage.clone());
    let handle = Handle::<SocketAddr>::new();
    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
        }
    });
    let served = serve::serve(&state.config, http::router(state.clone()), handle).await;
    maintenance.abort();
    state.turn.stop().await;
    state.database.close().await;
    served
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "could not listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

/// Confirms the configuration, certificate files and database are usable
/// without serving or creating anything.
pub async fn check(config: Config) -> anyhow::Result<()> {
    if let config::TlsConfig::Files {
        cert_path,
        key_path,
    } = &config.tls
    {
        axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path)
            .await
            .with_context(|| {
                format!(
                    "could not load the certificate {} and key {}",
                    cert_path.display(),
                    key_path.display()
                )
            })?;
    }
    if config.turn.enabled && config.turn.public_ip.is_none() {
        let host = config.turn_host();
        let ip = turn::resolve(&host).await?;
        println!("TURN relays will use {ip}, the address of {host}");
    }
    let url = config.database_url();
    if let Some(path) = db::sqlite_path(&url)?
        && !path.exists()
    {
        println!(
            "configuration OK; the database {} will be created on start",
            path.display()
        );
        return Ok(());
    }
    let database = Database::connect(&url, 1).await?;
    let applied = database
        .schema_version()
        .await
        .context("could not read the schema version")?;
    let expected = database.backend().expected_schema_version();
    database.close().await;
    match applied {
        Some(version) if version == expected => {
            println!("configuration OK; database schema is current ({version})")
        }
        Some(version) if version > expected => anyhow::bail!(
            "the database schema ({version}) is newer than this server ({expected}); upgrade the server"
        ),
        Some(version) => {
            println!("configuration OK; database schema {version} will be migrated to {expected}")
        }
        None => println!("configuration OK; the empty database will be set up on start"),
    }
    Ok(())
}
