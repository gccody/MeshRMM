//! The self-hosted MeshRMM server: the website, the API, Agent and viewer
//! connections, and downloads, for one company.
pub mod admin;
pub mod agent_rollout;
pub mod agents;
pub mod alerts;
pub mod api;
pub mod api_tokens;
pub mod applications;
pub mod audit;
pub mod auth;
pub mod automation;
pub mod config;
pub mod custom_fields;
pub mod db;
pub mod downloads;
pub mod files;
pub mod health;
pub mod http;
pub mod logging;
pub mod mail;
pub mod maintenance;
pub mod maintenance_windows;
pub mod monitors;
pub mod notes;
pub mod notifications;
pub mod rbac;
pub mod realtime;
pub mod reports;
pub mod scim;
pub mod secrets;
pub mod serve;
pub mod settings;
pub mod storage;
pub mod system_info;
pub mod tags;
pub mod targets;
pub mod terminals;
pub mod time;
pub mod toolbox;
pub mod turn;
pub mod updates;
pub mod users;
pub mod wake;
pub mod website;

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use axum_server::Handle;

use crate::{
    auth::{AuthState, accounts::Accounts},
    config::Config,
    db::Database,
    downloads::Downloads,
    http::AppState,
    realtime::{AgentHub, Metrics, Presence, Sessions, presence::UPDATE_GRACE, sessions::Timeouts},
    secrets::InstanceKey,
    storage::Storage,
    turn::Turn,
    website::Website,
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
    let database = Database::connect_with(&config.database_url(), config.database_limits()).await?;
    database.migrate().await?;
    let downloads = Downloads::load(&config).await?;
    if let Some(version) = downloads.version()
        && agent_rollout::begin_release(&mut &database, version).await?
    {
        tracing::info!(version, "the Agent rollout starts again for this release");
    }
    tracing::info!(
        backend = ?database.backend(),
        schema_version = database.backend().expected_schema_version(),
        "database ready"
    );
    let agents = AgentHub::default();
    let idle = Duration::from_secs(config.remote.idle_timeout_seconds);
    let maintenance_windows = maintenance_windows::MaintenanceWindows::default();
    maintenance_windows
        .load(&mut &database, time::now_ms())
        .await?;
    let presence = Presence::with_maintenance(
        database.clone(),
        agents.clone(),
        UPDATE_GRACE,
        maintenance_windows.clone(),
    );
    Ok(AppState {
        config: Arc::new(config),
        accounts: Arc::new(Accounts::new(presence.access_changes())),
        presence,
        metrics: Metrics::new(database.clone()),
        alerts: alerts::Engine::default(),
        monitors: monitors::Engine::default(),
        automation: automation::Engine::default(),
        notifications: notifications::queue::Queues::default(),
        updates: updates::Engine::default(),
        wake: wake::Engine::default(),
        reports: reports::Engine::default(),
        maintenance_windows,
        sessions: Sessions::new(Timeouts::new(idle)),
        terminals: terminals::Terminals::default(),
        files: files::Files::default(),
        turn: Turn::new(&instance_key),
        database,
        settings: Arc::default(),
        instance_key,
        auth: Arc::new(AuthState::default()),
        sso_discovery: Arc::default(),
        storage,
        agents,
        agent_timeouts: crate::agents::Timeouts::default(),
        website: Website::embedded(),
        downloads,
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
    tracing::info!(
        version = meshrmm_self_update::CURRENT_VERSION,
        "starting the MeshRMM server"
    );
    serve::warn_about_open_file_limit();
    let state = prepare(config).await?;
    state.downloads.warn_about_problems();
    state.downloads.start_signing();
    if state.website.is_empty() {
        tracing::warn!(
            "this server was built without its website; run `npm run build` in dashboard/ and rebuild the server"
        );
    }
    announce_setup(&state).await?;
    let restored = state.sessions.restore(&state).await?;
    if restored > 0 {
        tracing::info!(
            restored,
            "resumed remote sessions that were live at shutdown"
        );
    }
    if let Err(error) = wake::give_up_overdue(&state, time::now_ms()).await {
        tracing::warn!(%error, "could not finish wake requests from before the restart");
    }
    state.turn.start(&state.config).await?;
    let maintenance = maintenance::spawn(state.database.clone(), state.storage.clone());
    let alerting = alerts::spawn(state.clone());
    let monitoring = monitors::spawn(state.clone());
    let automating = automation::spawn(state.clone());
    let updating = updates::spawn(state.clone());
    let applying = applications::spawn(state.clone());
    let reporting = reports::spawn(state.clone());
    let windows = maintenance_windows::spawn(state.clone());
    let storing_metrics = state.metrics.spawn_writer();
    let handle = Handle::<serve::Endpoint>::new();
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
    alerting.abort();
    monitoring.abort();
    automating.abort();
    updating.abort();
    applying.abort();
    reporting.abort();
    windows.abort();
    // Before the database closes, so the finished minutes waiting are kept.
    storing_metrics.stop().await;
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
        serve::CertificateFiles::new(cert_path.clone(), key_path.clone())?
            .load()
            .await?;
    }
    let downloads = Downloads::load(&config).await?;
    match downloads.version() {
        Some(version) => println!(
            "{} downloads of release {version}, {} of them signed",
            downloads.file_count(),
            downloads.file_count() - downloads.unsigned().len()
        ),
        None => println!(
            "no Agent or viewer downloads: {} has no {}",
            config.downloads.dir.display(),
            downloads::ARTIFACTS_FILE
        ),
    }
    if let Some(signing) = &config.downloads.macos_signing {
        let checked = signing.clone();
        let (rcodesign, team) = tokio::task::spawn_blocking(move || {
            downloads::developer_id::check_certificate(&checked)
        })
        .await??;
        println!("{rcodesign} will sign the macOS builds with Developer ID team {team}");
        if signing.notary_api_key.is_none() {
            println!(
                "the macOS builds won't be notarized; without downloads.macos_signing.notary_api_key, macOS blocks the viewer when it's first opened"
            );
        }
        if let Some(status) = downloads.macos_signing() {
            println!("macOS builds: {status}");
        }
    }
    if config.turn.enabled && config.turn.public_ip.is_none() {
        let host = config.turn_host();
        let ip = turn::resolve(&host).await?;
        println!("TURN relays will use {ip}, the address of {host}");
    }
    if config.turn.enabled {
        let turn = &config.turn;
        println!(
            "TURN relays will use UDP ports {}-{}, enough for {} remote sessions at once",
            turn.relay_port_min,
            turn.relay_port_max,
            turn.relay_sessions()
        );
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
