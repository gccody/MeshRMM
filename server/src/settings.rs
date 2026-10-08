//! The instance's single row of settings.
use sea_query::{Expr, ExprTrait, Query};

use crate::db::{self, Executor, tables::Settings as SettingsTable};

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

const COLUMNS: [SettingsTable; 29] = [
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
