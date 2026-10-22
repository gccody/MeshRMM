//! Notification channels: where the server tells people that something
//! happened, such as an alert being raised. See docs/alerts.md.
//!
//! A channel is an email to chosen users and everyone holding chosen roles,
//! sent through the server's SMTP settings, or a webhook: a signed JSON POST
//! to a URL. Whatever notifies (alert rules today) names its channels and
//! hands [`dispatch`] a [`Notification`]; delivery happens in the background,
//! a webhook is retried a few times, and each channel keeps how its last
//! delivery went. Deliveries waiting for a retry are lost if the server stops.
pub mod email;
pub mod webhook;

use std::sync::Arc;

use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    db::{self, Database, Executor, tables::NotificationChannels},
    http::AppState,
    time::now_ms,
};

/// The encryption context of a webhook's signing secret.
pub fn secret_context(channel_id: &str) -> String {
    format!("notification-channel:{channel_id}:webhook-secret")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Email,
    Webhook,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Webhook => "webhook",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "email" => Some(Self::Email),
            "webhook" => Some(Self::Webhook),
            _ => None,
        }
    }
}

/// Something to tell a channel's recipients.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// What happened, such as `alert.raised`. Webhooks get it as `event`.
    pub event: &'static str,
    /// One line: the email's subject, and the webhook's `summary` and `text`.
    pub summary: String,
    /// The email's body.
    pub body: String,
    /// What the webhook's JSON body carries besides its envelope, such as
    /// `{"alert": {...}}`.
    pub data: Map<String, Value>,
}

/// A channel as stored.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub webhook_url: Option<String>,
    pub webhook_secret_encrypted: Option<Vec<u8>>,
    pub last_delivery_at: Option<i64>,
    pub last_delivery_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Channel {
    pub fn kind(&self) -> Option<Kind> {
        Kind::parse(&self.kind)
    }
}

pub fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            NotificationChannels::Id,
            NotificationChannels::Name,
            NotificationChannels::Kind,
            NotificationChannels::Enabled,
            NotificationChannels::WebhookUrl,
            NotificationChannels::WebhookSecretEncrypted,
            NotificationChannels::LastDeliveryAt,
            NotificationChannels::LastDeliveryError,
            NotificationChannels::CreatedAt,
            NotificationChannels::UpdatedAt,
        ])
        .from(NotificationChannels::Table)
        .order_by(NotificationChannels::Name, Order::Asc)
        .to_owned()
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<Channel>> {
    executor
        .fetch_optional(
            &select()
                .and_where(Expr::col(NotificationChannels::Id).eq(id))
                .to_owned(),
        )
        .await
}

/// Sends `notification` to each of `channel_ids` that is turned on, in the
/// background.
pub fn dispatch(state: &AppState, channel_ids: Vec<String>, notification: Notification) {
    if channel_ids.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let channels: db::Result<Vec<Channel>> = state
            .database
            .fetch_all(
                &select()
                    .and_where(Expr::col(NotificationChannels::Id).is_in(channel_ids))
                    .and_where(Expr::col(NotificationChannels::Enabled).eq(true))
                    .to_owned(),
            )
            .await;
        let channels = match channels {
            Ok(channels) => channels,
            Err(error) => {
                tracing::warn!(%error, event = notification.event, "could not read notification channels");
                return;
            }
        };
        let notification = Arc::new(notification);
        for channel in channels {
            tokio::spawn(deliver_with_retries(
                state.clone(),
                channel,
                notification.clone(),
            ));
        }
    });
}

async fn deliver_with_retries(state: AppState, channel: Channel, notification: Arc<Notification>) {
    let delays = match channel.kind() {
        Some(Kind::Webhook) => webhook::RETRY_DELAYS.as_slice(),
        _ => &[],
    };
    let mut attempt = 0;
    loop {
        let outcome = attempt_delivery(&state, &channel, &notification).await;
        let retry = match &outcome {
            Ok(()) => None,
            Err(failure) if failure.retry => delays.get(attempt),
            Err(_) => None,
        };
        let Some(delay) = retry else {
            if let Err(failure) = &outcome {
                tracing::warn!(
                    channel_id = channel.id,
                    event = notification.event,
                    error = failure.message,
                    "could not deliver a notification"
                );
            }
            record(&state.database, &channel.id, outcome.map_err(|f| f.message)).await;
            return;
        };
        tracing::info!(
            channel_id = channel.id,
            error = outcome.err().map(|f| f.message),
            retry_in_seconds = delay.as_secs(),
            "a notification delivery failed; retrying"
        );
        tokio::time::sleep(*delay).await;
        attempt += 1;
    }
}

/// Delivers `notification` once, records how it went on the channel, and
/// returns the error, for a test the user is waiting on.
pub async fn deliver_now(
    state: &AppState,
    channel: &Channel,
    notification: &Notification,
) -> Result<(), String> {
    let outcome = attempt_delivery(state, channel, notification)
        .await
        .map_err(|failure| failure.message);
    record(&state.database, &channel.id, outcome.clone()).await;
    outcome
}

/// Why a delivery failed, and whether trying again might help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub retry: bool,
}

impl Failure {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: false,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: true,
        }
    }
}

async fn attempt_delivery(
    state: &AppState,
    channel: &Channel,
    notification: &Notification,
) -> Result<(), Failure> {
    match channel.kind() {
        Some(Kind::Email) => email::send(state, &channel.id, notification).await,
        Some(Kind::Webhook) => {
            let url = channel
                .webhook_url
                .as_deref()
                .ok_or_else(|| Failure::permanent("the webhook has no URL"))?;
            let secret = channel
                .webhook_secret_encrypted
                .as_deref()
                .map(|sealed| {
                    state
                        .instance_key
                        .decrypt(&secret_context(&channel.id), sealed)
                        .map_err(|_| Failure::permanent("the signing secret can't be decrypted"))
                })
                .transpose()?;
            webhook::send(
                url,
                secret.as_deref(),
                notification,
                &state.config.public_origin(),
            )
            .await
        }
        None => Err(Failure::permanent("the channel's kind is unknown")),
    }
}

/// Keeps how the channel's latest delivery went, for the website.
async fn record(database: &Database, channel_id: &str, outcome: Result<(), String>) {
    let recorded = database
        .execute(
            &Query::update()
                .table(NotificationChannels::Table)
                .value(NotificationChannels::LastDeliveryAt, now_ms())
                .value(NotificationChannels::LastDeliveryError, outcome.err())
                .and_where(Expr::col(NotificationChannels::Id).eq(channel_id))
                .to_owned(),
        )
        .await;
    if let Err(error) = recorded {
        tracing::warn!(channel_id, %error, "could not record a notification delivery");
    }
}

/// What a channel's test sends.
pub fn test_notification(instance_name: &str, channel_name: &str) -> Notification {
    Notification {
        event: "test",
        summary: format!("Test notification from {instance_name}"),
        body: format!(
            "This is a test of the notification channel \"{channel_name}\" on {instance_name}. \
             It's working.\n"
        ),
        data: Map::new(),
    }
}
