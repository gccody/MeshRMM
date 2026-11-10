//! The instance's single row of settings.
use std::sync::{Arc, Mutex, PoisonError};

use sea_query::{Expr, ExprTrait, Query};

use crate::db::{self, Database, Executor, tables::Settings as SettingsTable};

/// Every column of `settings`. The SMTP password stays encrypted here; only
/// [`crate::mail`] decrypts it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Settings {
    pub instance_name: String,
    pub dashboard_idle_timeout_minutes: i64,
    pub blackout_message: String,
    pub display_border: bool,
    pub prevent_idle_lock: bool,
    pub allow_idle_override: bool,
    pub session_banner: bool,
    pub connection_notification: bool,
    pub background_connection_notification: bool,
    pub connection_notification_message: String,
    pub idle_disconnect_minutes: Option<i64>,
    pub allow_idle_disconnect_override: bool,
    pub clear_clipboard_on_close: bool,
    pub allow_clear_clipboard_override: bool,
    pub connection_approval: bool,
    pub connection_approval_message: String,
    pub connection_approval_timeout_seconds: i64,
    pub connection_approval_lock_idle_seconds: i64,
    pub require_two_factor: bool,
    pub password_min_length: i64,
    pub session_lifetime_hours: i64,
    /// How often online devices scan for updates; 0 leaves it to users and
    /// policies.
    pub update_scan_interval_hours: i64,
    /// Remote terminals with no input for this long are closed.
    pub terminal_idle_timeout_minutes: i64,
    /// The largest file the file browser transfers, in MiB.
    pub file_transfer_limit_mb: i64,
    /// Personal API tokens authenticate requests.
    pub api_tokens_enabled: bool,
    /// The longest a new API token may last; `None` allows tokens that never
    /// expire.
    pub api_token_max_lifetime_days: Option<i64>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<i64>,
    pub smtp_security: String,
    pub smtp_username: Option<String>,
    pub smtp_password_encrypted: Option<Vec<u8>>,
    pub smtp_from: Option<String>,
    pub updated_at: i64,
    pub updated_by_user_id: Option<String>,
}

impl Settings {
    /// Whether email can be sent: SMTP has a server and a sender.
    pub fn smtp_configured(&self) -> bool {
        self.smtp_host.is_some() && self.smtp_from.is_some()
    }
}

const COLUMNS: [SettingsTable; 34] = [
    SettingsTable::InstanceName,
    SettingsTable::DashboardIdleTimeoutMinutes,
    SettingsTable::BlackoutMessage,
    SettingsTable::DisplayBorder,
    SettingsTable::PreventIdleLock,
    SettingsTable::AllowIdleOverride,
    SettingsTable::SessionBanner,
    SettingsTable::ConnectionNotification,
    SettingsTable::BackgroundConnectionNotification,
    SettingsTable::ConnectionNotificationMessage,
    SettingsTable::IdleDisconnectMinutes,
    SettingsTable::AllowIdleDisconnectOverride,
    SettingsTable::ClearClipboardOnClose,
    SettingsTable::AllowClearClipboardOverride,
    SettingsTable::ConnectionApproval,
    SettingsTable::ConnectionApprovalMessage,
    SettingsTable::ConnectionApprovalTimeoutSeconds,
    SettingsTable::ConnectionApprovalLockIdleSeconds,
    SettingsTable::RequireTwoFactor,
    SettingsTable::PasswordMinLength,
    SettingsTable::SessionLifetimeHours,
    SettingsTable::UpdateScanIntervalHours,
    SettingsTable::TerminalIdleTimeoutMinutes,
    SettingsTable::FileTransferLimitMb,
    SettingsTable::ApiTokensEnabled,
    SettingsTable::ApiTokenMaxLifetimeDays,
    SettingsTable::SmtpHost,
    SettingsTable::SmtpPort,
    SettingsTable::SmtpSecurity,
    SettingsTable::SmtpUsername,
    SettingsTable::SmtpPasswordEncrypted,
    SettingsTable::SmtpFrom,
    SettingsTable::UpdatedAt,
    SettingsTable::UpdatedByUserId,
];

pub async fn load(executor: &mut impl Executor) -> db::Result<Settings> {
    executor
        .fetch_one(
            &Query::select()
                .columns(COLUMNS)
                .from(SettingsTable::Table)
                .and_where(Expr::col(SettingsTable::Id).eq(1))
                .to_owned(),
        )
        .await
}

/// The settings row as last read, for readers outside a transaction. The
/// row changes rarely; whatever saves it calls [`Cache::forget`] once the
/// change is committed.
#[derive(Debug, Default)]
pub struct Cache {
    inner: Mutex<Cached>,
}

#[derive(Debug, Default)]
struct Cached {
    /// Bumped by every forget, so a read that began before a save isn't kept.
    generation: u64,
    settings: Option<Arc<Settings>>,
}

impl Cache {
    /// The settings, read from `database` only when none are cached.
    pub async fn get(&self, database: &Database) -> db::Result<Arc<Settings>> {
        let generation = {
            let cached = self.lock();
            if let Some(settings) = &cached.settings {
                return Ok(Arc::clone(settings));
            }
            cached.generation
        };
        let settings = Arc::new(load(&mut &*database).await?);
        let mut cached = self.lock();
        if cached.generation == generation {
            cached.settings = Some(Arc::clone(&settings));
        }
        Ok(settings)
    }

    /// The settings row changed: the next read goes to the database.
    pub fn forget(&self) {
        let mut cached = self.lock();
        cached.generation += 1;
        cached.settings = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cached> {
        // Nothing panics while holding it, so a poisoned one is whole.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
