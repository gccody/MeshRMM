//! Instance settings: general and remote session policy, sign-in policy, and
//! email.
use axum::{Json, extract::State, http::StatusCode};
use lettre::message::Mailbox;
use meshrmm_protocol_types as protocol;
use sea_query::{Expr, ExprTrait, Query, SimpleExpr};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{double_option, require_administrator, setup::validate_instance_name};
use crate::{
    audit::{self, Target},
    auth::Authorized,
    db::tables::Settings as SettingsTable,
    http::{ApiError, AppState, JsonBody},
    mail::{self, SMTP_PASSWORD_CONTEXT, Security, Smtp},
    rbac::Permission,
    settings::{self, Settings},
    time::now_ms,
};

const MIN_IDLE_TIMEOUT_MINUTES: u32 = 5;
const MAX_IDLE_TIMEOUT_MINUTES: u32 = 24 * 60;
const MIN_PASSWORD_LENGTH: u32 = 8;
const MAX_PASSWORD_LENGTH: u32 = 128;
const MAX_SESSION_LIFETIME_HOURS: u32 = 365 * 24;

/// Writes `values` to the settings row, recording who changed it.
async fn save(
    state: &AppState,
    actor: &Authorized,
    action: &str,
    mut values: Vec<(SettingsTable, SimpleExpr)>,
    metadata: serde_json::Value,
) -> Result<Settings, ApiError> {
    let mut transaction = state.database.begin().await?;
    if !values.is_empty() {
        values.push((SettingsTable::UpdatedAt, now_ms().into()));
        values.push((
            SettingsTable::UpdatedByUserId,
            actor.user.id.as_str().into(),
        ));
        transaction
            .execute(
                &Query::update()
                    .table(SettingsTable::Table)
                    .values(values)
                    .and_where(Expr::col(SettingsTable::Id).eq(1))
                    .to_owned(),
            )
            .await?;
        audit::record(
            &mut transaction,
            &actor.actor(),
            action,
            Target::settings(),
            metadata,
        )
        .await?;
    }
    let settings = settings::load(&mut transaction).await?;
    transaction.commit().await?;
    Ok(settings)
}

/// The instance name and the policy remote sessions follow.
#[derive(Debug, Serialize)]
pub struct GeneralSettings {
    instance_name: String,
    dashboard_idle_timeout_minutes: i64,
    blackout_message: String,
    display_border: bool,
    prevent_idle_lock: bool,
    allow_idle_override: bool,
    session_banner: bool,
    connection_notification: bool,
    background_connection_notification: bool,
    connection_notification_message: String,
    idle_disconnect_minutes: Option<i64>,
    allow_idle_disconnect_override: bool,
    clear_clipboard_on_close: bool,
    allow_clear_clipboard_override: bool,
    connection_approval: bool,
    connection_approval_message: String,
    connection_approval_timeout_seconds: i64,
    connection_approval_lock_idle_seconds: i64,
    updated_at: i64,
}

impl From<Settings> for GeneralSettings {
    fn from(settings: Settings) -> Self {
        Self {
            instance_name: settings.instance_name,
            dashboard_idle_timeout_minutes: settings.dashboard_idle_timeout_minutes,
            blackout_message: settings.blackout_message,
            display_border: settings.display_border,
            prevent_idle_lock: settings.prevent_idle_lock,
            allow_idle_override: settings.allow_idle_override,
            session_banner: settings.session_banner,
            connection_notification: settings.connection_notification,
            background_connection_notification: settings.background_connection_notification,
            connection_notification_message: settings.connection_notification_message,
            idle_disconnect_minutes: settings.idle_disconnect_minutes,
            allow_idle_disconnect_override: settings.allow_idle_disconnect_override,
            clear_clipboard_on_close: settings.clear_clipboard_on_close,
            allow_clear_clipboard_override: settings.allow_clear_clipboard_override,
            connection_approval: settings.connection_approval,
            connection_approval_message: settings.connection_approval_message,
            connection_approval_timeout_seconds: settings.connection_approval_timeout_seconds,
            connection_approval_lock_idle_seconds: settings.connection_approval_lock_idle_seconds,
            updated_at: settings.updated_at,
        }
    }
}

/// `GET /v1/settings`
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<GeneralSettings>, ApiError> {
    actor.require(Permission::SettingsManage)?;
    Ok(Json(settings::load(&mut &state.database).await?.into()))
}

/// Every field is optional; a missing one is left as it is.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dashboard_idle_timeout_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blackout_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_border: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prevent_idle_lock: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_idle_override: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_banner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_notification: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background_connection_notification: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_notification_message: Option<String>,
    /// `null` never disconnects idle sessions.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    idle_disconnect_minutes: Option<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_idle_disconnect_override: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clear_clipboard_on_close: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_clear_clipboard_override: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_approval: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_approval_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_approval_timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_approval_lock_idle_seconds: Option<u32>,
}

fn check(valid: bool, message: &'static str) -> Result<(), ApiError> {
    if valid {
        Ok(())
    } else {
        Err(ApiError::bad_request(message))
    }
}

impl GeneralUpdate {
    /// Validates the update and returns the columns it sets.
    fn columns(&mut self) -> Result<Vec<(SettingsTable, SimpleExpr)>, ApiError> {
        let mut values = Vec::new();
        if let Some(name) = &self.instance_name {
            let name = validate_instance_name(name)?;
            values.push((SettingsTable::InstanceName, name.as_str().into()));
            self.instance_name = Some(name);
        }
        if let Some(minutes) = self.dashboard_idle_timeout_minutes {
            check(
                (MIN_IDLE_TIMEOUT_MINUTES..=MAX_IDLE_TIMEOUT_MINUTES).contains(&minutes),
                "the website idle timeout must be between 5 and 1440 minutes",
            )?;
            values.push((
                SettingsTable::DashboardIdleTimeoutMinutes,
                i64::from(minutes).into(),
            ));
        }
        if let Some(message) = &self.blackout_message {
            check(
                protocol::valid_blackout_message(message),
                "the blackout message must be nonempty, at most 2048 bytes, and have no control characters except newlines",
            )?;
            values.push((SettingsTable::BlackoutMessage, message.as_str().into()));
        }
        if let Some(message) = &self.connection_notification_message {
            check(
                protocol::valid_connection_notification_message(message),
                "the connection notification must be nonempty, at most 512 bytes, and have no control characters except newlines",
            )?;
            values.push((
                SettingsTable::ConnectionNotificationMessage,
                message.as_str().into(),
            ));
        }
        if let Some(message) = &self.connection_approval_message {
            check(
                protocol::valid_connection_approval_message(message),
                "the connection approval message must be nonempty, at most 512 bytes, and have no control characters except newlines",
            )?;
            values.push((
                SettingsTable::ConnectionApprovalMessage,
                message.as_str().into(),
            ));
        }
        if let Some(minutes) = self.idle_disconnect_minutes {
            check(
                minutes.is_none_or(protocol::valid_idle_disconnect_minutes),
                "the idle disconnect time must be never or one of the offered choices",
            )?;
            values.push((
                SettingsTable::IdleDisconnectMinutes,
                minutes.map(i64::from).into(),
            ));
        }
        if let Some(seconds) = self.connection_approval_timeout_seconds {
            check(
                protocol::valid_connection_approval_timeout(seconds),
                "the connection approval timeout must be between 5 and 300 seconds",
            )?;
            values.push((
                SettingsTable::ConnectionApprovalTimeoutSeconds,
                i64::from(seconds).into(),
            ));
        }
        if let Some(seconds) = self.connection_approval_lock_idle_seconds {
            check(
                protocol::valid_connection_approval_lock_idle(seconds),
                "the lock screen idle time must be between 0 and 3600 seconds",
            )?;
            values.push((
                SettingsTable::ConnectionApprovalLockIdleSeconds,
                i64::from(seconds).into(),
            ));
        }
        for (column, value) in [
            (SettingsTable::DisplayBorder, self.display_border),
            (SettingsTable::PreventIdleLock, self.prevent_idle_lock),
            (SettingsTable::AllowIdleOverride, self.allow_idle_override),
            (SettingsTable::SessionBanner, self.session_banner),
            (
                SettingsTable::ConnectionNotification,
                self.connection_notification,
            ),
            (
                SettingsTable::BackgroundConnectionNotification,
                self.background_connection_notification,
            ),
            (
                SettingsTable::AllowIdleDisconnectOverride,
                self.allow_idle_disconnect_override,
            ),
            (
                SettingsTable::ClearClipboardOnClose,
                self.clear_clipboard_on_close,
            ),
            (
                SettingsTable::AllowClearClipboardOverride,
                self.allow_clear_clipboard_override,
            ),
            (SettingsTable::ConnectionApproval, self.connection_approval),
        ] {
            if let Some(value) = value {
                values.push((column, value.into()));
            }
        }
        Ok(values)
    }
}

/// `PATCH /v1/settings`
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(mut request): JsonBody<GeneralUpdate>,
) -> Result<Json<GeneralSettings>, ApiError> {
    actor.require(Permission::SettingsManage)?;
    let values = request.columns()?;
    let metadata = serde_json::to_value(&request).unwrap_or_default();
    Ok(Json(
        save(&state, &actor, "settings.update", values, metadata)
            .await?
            .into(),
    ))
}

#[derive(Debug, Serialize)]
pub struct AuthenticationSettings {
    require_two_factor: bool,
    password_min_length: i64,
    session_lifetime_hours: i64,
}

impl From<Settings> for AuthenticationSettings {
    fn from(settings: Settings) -> Self {
        Self {
            require_two_factor: settings.require_two_factor,
            password_min_length: settings.password_min_length,
            session_lifetime_hours: settings.session_lifetime_hours,
        }
    }
}

/// `GET /v1/settings/authentication`
pub async fn get_authentication(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<AuthenticationSettings>, ApiError> {
    actor.require(Permission::AuthenticationManage)?;
    Ok(Json(settings::load(&mut &state.database).await?.into()))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticationUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    require_two_factor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_min_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_lifetime_hours: Option<u32>,
}

/// `PATCH /v1/settings/authentication`. A new session lifetime applies to
/// sessions started afterwards.
pub async fn update_authentication(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<AuthenticationUpdate>,
) -> Result<Json<AuthenticationSettings>, ApiError> {
    actor.require(Permission::AuthenticationManage)?;
    let mut values = Vec::new();
    if let Some(required) = request.require_two_factor {
        values.push((SettingsTable::RequireTwoFactor, required.into()));
    }
    if let Some(length) = request.password_min_length {
        check(
            (MIN_PASSWORD_LENGTH..=MAX_PASSWORD_LENGTH).contains(&length),
            "the minimum password length must be between 8 and 128",
        )?;
        values.push((SettingsTable::PasswordMinLength, i64::from(length).into()));
    }
    if let Some(hours) = request.session_lifetime_hours {
        check(
            (1..=MAX_SESSION_LIFETIME_HOURS).contains(&hours),
            "the session lifetime must be between 1 and 8760 hours",
        )?;
        values.push((SettingsTable::SessionLifetimeHours, i64::from(hours).into()));
    }
    let metadata = serde_json::to_value(&request).unwrap_or_default();
    Ok(Json(
        save(
            &state,
            &actor,
            "settings.authentication_update",
            values,
            metadata,
        )
        .await?
        .into(),
    ))
}

#[derive(Debug, Serialize)]
pub struct SmtpSettings {
    configured: bool,
    host: Option<String>,
    port: Option<i64>,
    security: String,
    username: Option<String>,
    /// The password itself is never sent back.
    has_password: bool,
    from: Option<String>,
}

impl From<Settings> for SmtpSettings {
    fn from(settings: Settings) -> Self {
        Self {
            configured: settings.smtp_configured(),
            has_password: settings.smtp_password_encrypted.is_some(),
            host: settings.smtp_host,
            port: settings.smtp_port,
            security: settings.smtp_security,
            username: settings.smtp_username,
            from: settings.smtp_from,
        }
    }
}

/// `GET /v1/settings/smtp`
pub async fn get_smtp(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<SmtpSettings>, ApiError> {
    // Whoever controls email receives every password reset link, so this
    // is as powerful as being an administrator.
    require_administrator(&actor)?;
    Ok(Json(settings::load(&mut &state.database).await?.into()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpUpdate {
    host: String,
    /// Defaults to the usual port for `security`.
    #[serde(default)]
    port: Option<u16>,
    security: String,
    #[serde(default)]
    username: Option<String>,
    /// Missing keeps the stored password; `null` removes it.
    #[serde(default, deserialize_with = "double_option")]
    password: Option<Option<String>>,
    /// The sender, e.g. `MeshRMM <rmm@example.com>`.
    from: String,
}

/// `PUT /v1/settings/smtp`: sets up email.
pub async fn put_smtp(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<SmtpUpdate>,
) -> Result<Json<SmtpSettings>, ApiError> {
    // Whoever controls email receives every password reset link, so this
    // is as powerful as being an administrator.
    require_administrator(&actor)?;
    let host = request.host.trim();
    check(
        !host.is_empty()
            && host.len() <= 253
            && !host.contains(|c: char| c.is_whitespace() || c == '/'),
        "enter the SMTP server's host name",
    )?;
    let security = Security::parse(&request.security)
        .ok_or_else(|| ApiError::bad_request("security must be starttls, tls or none"))?;
    check(
        request.port != Some(0),
        "the port must be between 1 and 65535",
    )?;
    let from = request.from.trim();
    check(
        from.parse::<Mailbox>().is_ok(),
        "the sender must be an email address, optionally with a name",
    )?;
    let username = request
        .username
        .as_deref()
        .map(str::trim)
        .filter(|username| !username.is_empty());
    check(
        username.is_none_or(|username| username.len() <= 254),
        "the username is too long",
    )?;
    let mut values = vec![
        (SettingsTable::SmtpHost, host.into()),
        (SettingsTable::SmtpPort, request.port.map(i64::from).into()),
        (SettingsTable::SmtpSecurity, security.as_str().into()),
        (
            SettingsTable::SmtpUsername,
            username.map(str::to_owned).into(),
        ),
        (SettingsTable::SmtpFrom, from.into()),
    ];
    match &request.password {
        None => {}
        Some(None) => values.push((
            SettingsTable::SmtpPasswordEncrypted,
            Option::<Vec<u8>>::None.into(),
        )),
        Some(Some(password)) => values.push((
            SettingsTable::SmtpPasswordEncrypted,
            state
                .instance_key
                .encrypt(SMTP_PASSWORD_CONTEXT, password.as_bytes())
                .into(),
        )),
    }
    let metadata = json!({
        "host": host,
        "port": request.port,
        "security": security.as_str(),
        "username": username,
        "from": from,
        "password_changed": request.password.is_some(),
    });
    Ok(Json(
        save(&state, &actor, "settings.smtp_update", values, metadata)
            .await?
            .into(),
    ))
}

/// `DELETE /v1/settings/smtp`: turns email off.
pub async fn delete_smtp(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<StatusCode, ApiError> {
    // Whoever controls email receives every password reset link, so this
    // is as powerful as being an administrator.
    require_administrator(&actor)?;
    let none = || Option::<String>::None.into();
    save(
        &state,
        &actor,
        "settings.smtp_delete",
        vec![
            (SettingsTable::SmtpHost, none()),
            (SettingsTable::SmtpPort, Option::<i64>::None.into()),
            (SettingsTable::SmtpSecurity, "starttls".into()),
            (SettingsTable::SmtpUsername, none()),
            (
                SettingsTable::SmtpPasswordEncrypted,
                Option::<Vec<u8>>::None.into(),
            ),
            (SettingsTable::SmtpFrom, none()),
        ],
        json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpTest {
    /// Defaults to the signed-in user's address.
    #[serde(default)]
    to: Option<String>,
}

/// `POST /v1/settings/smtp/test`: sends a test email and reports the SMTP
/// server's error if it fails.
pub async fn test_smtp(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<SmtpTest>,
) -> Result<StatusCode, ApiError> {
    // Whoever controls email receives every password reset link, so this
    // is as powerful as being an administrator.
    require_administrator(&actor)?;
    let settings = settings::load(&mut &state.database).await?;
    let smtp = Smtp::from_settings(&settings, &state.instance_key)?
        .ok_or_else(|| ApiError::conflict("email isn't set up"))?;
    let to = request.to.as_deref().unwrap_or(&actor.user.email);
    smtp.send(
        &mail::hello_name(&state),
        to,
        &format!("{} test email", settings.instance_name),
        format!(
            "This is a test email from {}. Email is working.\n",
            settings.instance_name
        ),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("the test email failed: {error:#}"),
        )
        .with_code("email_failed")
    })?;
    Ok(StatusCode::NO_CONTENT)
}
