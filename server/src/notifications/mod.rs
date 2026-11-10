//! Notification channels: where the server tells people that something
//! happened, such as an alert being raised. See docs/alerts.md.
//!
//! A channel is an email to chosen users and everyone holding chosen roles,
//! sent through the server's SMTP settings, or a webhook: a signed JSON POST
//! to a URL. Whatever notifies (alert rules and automation policies) names
//! its channels and hands [`dispatch`] a [`Notification`], which only queues
//! it. Each channel has one queue and, while it has something to deliver, one
//! worker ([`queue`]), so a burst can't open more than a few connections:
//! notifications are delivered in the order they were dispatched, up to
//! [`queue::MAX_WAITING`] wait and later ones are dropped, an email channel
//! sends many waiting notifications as one digest, and a webhook channel
//! keeps a few requests open at once and retries failed ones a few times.
//! Each channel keeps how its last deliveries went. Notifications waiting,
//! or waiting for a retry, are lost if the server stops.
pub mod email;
pub mod queue;
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

/// Queues `notification` for each of `channel_ids`; each channel's worker
/// delivers it if the channel is turned on. See [`queue`].
pub fn dispatch(state: &AppState, mut channel_ids: Vec<String>, notification: Notification) {
    channel_ids.sort_unstable();
    channel_ids.dedup();
    if channel_ids.is_empty() {
        return;
    }
    let notification = Arc::new(notification);
    for channel_id in state.notifications.push(&channel_ids, &notification) {
        tokio::spawn(queue::run(state.clone(), channel_id));
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
        Some(Kind::Email) => email::send(state, &channel.id, &[notification]).await,
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

/// Keeps how the channel's latest deliveries went, for the website.
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
