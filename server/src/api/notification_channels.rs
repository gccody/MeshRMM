//! Notification channels, which alert rules notify. See docs/alerts.md.
use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::double_option;
use crate::{
    agents::parse_id,
    audit::{self, Target},
    auth::Authorized,
    db::{
        Executor,
        tables::{
            NotificationChannelRoles, NotificationChannelUsers, NotificationChannels, Roles, Users,
        },
    },
    http::{ApiError, AppState, JsonBody},
    notifications::{self, Channel, Kind, webhook},
    rbac::Permission,
    settings,
    time::now_ms,
    users::new_id,
};

const MAX_NAME_LENGTH: usize = 80;

#[derive(Debug, Serialize)]
pub struct ChannelView {
    id: String,
    name: String,
    kind: Kind,
    enabled: bool,
    webhook_url: Option<String>,
    /// Whether the webhook's requests are signed. The secret is never shown.
    has_secret: bool,
    user_ids: Vec<String>,
    role_ids: Vec<String>,
    last_delivery_at: Option<i64>,
    /// Why the last delivery failed; `None` if it worked.
    last_delivery_error: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct ChannelList {
    channels: Vec<ChannelView>,
    /// Whether the server's SMTP settings let email channels deliver.
    email_available: bool,
}

async fn members(
    executor: &mut impl Executor,
) -> Result<(HashMap<String, Vec<String>>, HashMap<String, Vec<String>>), ApiError> {
    let users: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    NotificationChannelUsers::ChannelId,
                    NotificationChannelUsers::UserId,
                ])
                .from(NotificationChannelUsers::Table)
                .to_owned(),
        )
        .await?;
    let roles: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    NotificationChannelRoles::ChannelId,
                    NotificationChannelRoles::RoleId,
                ])
                .from(NotificationChannelRoles::Table)
                .to_owned(),
        )
        .await?;
    let group = |pairs: Vec<(String, String)>| {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (channel, member) in pairs {
            map.entry(channel).or_default().push(member);
        }
        map.values_mut().for_each(|members| members.sort());
        map
    };
    Ok((group(users), group(roles)))
}

fn view(
    channel: Channel,
    users: &HashMap<String, Vec<String>>,
    roles: &HashMap<String, Vec<String>>,
) -> Result<ChannelView, ApiError> {
    let kind = channel.kind().ok_or_else(|| {
        tracing::error!(
            channel_id = channel.id,
            "stored notification channel has an unknown kind"
        );
        ApiError::internal()
    })?;
    Ok(ChannelView {
        user_ids: users.get(&channel.id).cloned().unwrap_or_default(),
        role_ids: roles.get(&channel.id).cloned().unwrap_or_default(),
        has_secret: channel.webhook_secret_encrypted.is_some(),
        id: channel.id,
        name: channel.name,
        kind,
        enabled: channel.enabled,
        webhook_url: channel.webhook_url,
        last_delivery_at: channel.last_delivery_at,
        last_delivery_error: channel.last_delivery_error,
        created_at: channel.created_at,
        updated_at: channel.updated_at,
    })
}

async fn load_view(executor: &mut impl Executor, id: &str) -> Result<ChannelView, ApiError> {
    let channel = notifications::by_id(executor, id)
        .await?
        .ok_or_else(|| ApiError::not_found("Channel not found"))?;
    let (users, roles) = members(executor).await?;
    view(channel, &users, &roles)
}

async fn email_available(state: &AppState) -> Result<bool, ApiError> {
    Ok(settings::load(&mut &state.database)
        .await?
        .smtp_configured())
}

/// `GET /v1/notification-channels`
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<ChannelList>, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let mut database = &state.database;
    let channels: Vec<Channel> = database.fetch_all(&notifications::select()).await?;
    let (users, roles) = members(&mut database).await?;
    Ok(Json(ChannelList {
        channels: channels
            .into_iter()
            .map(|channel| view(channel, &users, &roles))
            .collect::<Result<_, _>>()?,
        email_available: email_available(&state).await?,
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RecipientUser {
    id: String,
    display_name: String,
    email: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RecipientRole {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
pub struct Recipients {
    users: Vec<RecipientUser>,
    roles: Vec<RecipientRole>,
}

/// `GET /v1/notification-channels/recipients`: who an email channel can
/// reach, for its editor.
pub async fn recipients(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Recipients>, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let database = &state.database;
    let users = database
        .fetch_all(
            &Query::select()
                .columns([Users::Id, Users::DisplayName, Users::Email])
                .from(Users::Table)
                .and_where(Expr::col(Users::Disabled).eq(false))
                .order_by(Users::DisplayName, Order::Asc)
                .to_owned(),
        )
        .await?;
    let roles = database
        .fetch_all(
            &Query::select()
                .columns([Roles::Id, Roles::Name])
                .from(Roles::Table)
                .order_by(Roles::Name, Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(Json(Recipients { users, roles }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelInput {
    name: String,
    kind: Kind,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    webhook_url: Option<String>,
    /// A new signing secret; `null` removes it and a missing field keeps it.
    #[serde(default, deserialize_with = "double_option")]
    webhook_secret: Option<Option<String>>,
    #[serde(default)]
    user_ids: Vec<String>,
    #[serde(default)]
    role_ids: Vec<String>,
}

fn enabled_by_default() -> bool {
    true
}

/// The input, checked, with only the fields its kind uses.
struct Checked {
    name: String,
    kind: Kind,
    enabled: bool,
    webhook_url: Option<String>,
    webhook_secret: Option<Option<String>>,
    user_ids: Vec<String>,
    role_ids: Vec<String>,
}

async fn check(executor: &mut impl Executor, input: ChannelInput) -> Result<Checked, ApiError> {
    let invalid =
        |message: &str| ApiError::bad_request(message.to_owned()).with_code("invalid_channel");
    let name = input.name.trim().to_owned();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(invalid(
            "the channel name must be 1 to 80 characters with no control characters",
        ));
    }
    let mut checked = Checked {
        name,
        kind: input.kind,
        enabled: input.enabled,
        webhook_url: None,
        webhook_secret: None,
        user_ids: Vec::new(),
        role_ids: Vec::new(),
    };
    match input.kind {
        Kind::Webhook => {
            let url = input
                .webhook_url
                .as_deref()
                .ok_or_else(|| invalid("a webhook needs a URL"))?;
            checked.webhook_url = Some(webhook::validate_url(url).map_err(invalid)?);
            checked.webhook_secret = match input.webhook_secret {
                Some(Some(secret)) if secret.trim().is_empty() => Some(None),
                Some(Some(secret)) if secret.chars().count() > webhook::MAX_SECRET_LENGTH => {
                    return Err(invalid("the signing secret must be at most 256 characters"));
                }
                other => other,
            };
        }
        Kind::Email => {
            let mut user_ids = input.user_ids;
            let mut role_ids = input.role_ids;
            for id in &user_ids {
                parse_id(id, "user")?;
            }
            user_ids.sort();
            user_ids.dedup();
            role_ids.sort();
            role_ids.dedup();
            if user_ids.is_empty() && role_ids.is_empty() {
                return Err(invalid("choose at least one user or role to email"));
            }
            let found: Vec<(String,)> = if user_ids.is_empty() {
                Vec::new()
            } else {
                executor
                    .fetch_all(
                        &Query::select()
                            .column(Users::Id)
                            .from(Users::Table)
                            .and_where(Expr::col(Users::Id).is_in(user_ids.clone()))
                            .to_owned(),
                    )
                    .await?
            };
            if found.len() != user_ids.len() {
                return Err(ApiError::not_found("a chosen user no longer exists"));
            }
            let found: Vec<(String,)> = if role_ids.is_empty() {
                Vec::new()
            } else {
                executor
                    .fetch_all(
                        &Query::select()
                            .column(Roles::Id)
                            .from(Roles::Table)
                            .and_where(Expr::col(Roles::Id).is_in(role_ids.clone()))
                            .to_owned(),
                    )
                    .await?
            };
            if found.len() != role_ids.len() {
                return Err(ApiError::not_found("a chosen role no longer exists"));
            }
            checked.user_ids = user_ids;
            checked.role_ids = role_ids;
        }
    }
    Ok(checked)
}

async fn ensure_name_free(
    executor: &mut impl Executor,
    name: &str,
    except_id: Option<&str>,
) -> Result<(), ApiError> {
    let names: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([NotificationChannels::Id, NotificationChannels::Name])
                .from(NotificationChannels::Table)
                .to_owned(),
        )
        .await?;
    let wanted = name.to_lowercase();
    if names
        .iter()
        .any(|(id, existing)| Some(id.as_str()) != except_id && existing.to_lowercase() == wanted)
    {
        return Err(ApiError::conflict(
            "a channel with this name already exists",
        ));
    }
    Ok(())
}

async fn write_members(
    executor: &mut impl Executor,
    id: &str,
    checked: &Checked,
) -> Result<(), ApiError> {
    executor
        .execute(
            &Query::delete()
                .from_table(NotificationChannelUsers::Table)
                .and_where(Expr::col(NotificationChannelUsers::ChannelId).eq(id))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(NotificationChannelRoles::Table)
                .and_where(Expr::col(NotificationChannelRoles::ChannelId).eq(id))
                .to_owned(),
        )
        .await?;
    if !checked.user_ids.is_empty() {
        let mut insert = Query::insert();
        insert.into_table(NotificationChannelUsers::Table).columns([
            NotificationChannelUsers::ChannelId,
            NotificationChannelUsers::UserId,
        ]);
        for user_id in &checked.user_ids {
            insert.values_panic([id.into(), user_id.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    if !checked.role_ids.is_empty() {
        let mut insert = Query::insert();
        insert.into_table(NotificationChannelRoles::Table).columns([
            NotificationChannelRoles::ChannelId,
            NotificationChannelRoles::RoleId,
        ]);
        for role_id in &checked.role_ids {
            insert.values_panic([id.into(), role_id.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

/// What the audit log keeps: a webhook's host, not its URL, which may carry
/// a secret token.
fn metadata(checked: &Checked) -> serde_json::Value {
    json!({
        "name": checked.name,
        "kind": checked.kind.as_str(),
        "enabled": checked.enabled,
        "webhook_host": checked.webhook_url.as_deref().map(webhook::url_host),
        "user_ids": checked.user_ids,
        "role_ids": checked.role_ids,
    })
}

/// `POST /v1/notification-channels`
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<ChannelInput>,
) -> Result<(StatusCode, Json<ChannelView>), ApiError> {
    actor.require(Permission::AlertsManage)?;
    if input.kind == Kind::Email && !email_available(&state).await? {
        return Err(ApiError::conflict(
            "email isn't set up on this server; an administrator can set it up under Authentication",
        )
        .with_code("email_unavailable"));
    }
    let mut transaction = state.database.begin().await?;
    let checked = check(&mut transaction, input).await?;
    ensure_name_free(&mut transaction, &checked.name, None).await?;
    let id = new_id();
    let now = now_ms();
    let secret = checked.webhook_secret.clone().flatten().map(|secret| {
        state
            .instance_key
            .encrypt(&notifications::secret_context(&id), secret.as_bytes())
    });
    transaction
        .execute(
            &Query::insert()
                .into_table(NotificationChannels::Table)
                .columns([
                    NotificationChannels::Id,
                    NotificationChannels::Name,
                    NotificationChannels::Kind,
                    NotificationChannels::Enabled,
                    NotificationChannels::WebhookUrl,
                    NotificationChannels::WebhookSecretEncrypted,
                    NotificationChannels::CreatedAt,
                    NotificationChannels::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    checked.name.as_str().into(),
                    checked.kind.as_str().into(),
                    checked.enabled.into(),
                    checked.webhook_url.clone().into(),
                    secret.into(),
                    now.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    write_members(&mut transaction, &id, &checked).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "notification_channel.create",
        Target::notification_channel(&id),
        metadata(&checked),
    )
    .await?;
    let created = load_view(&mut transaction, &id).await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `PUT /v1/notification-channels/{id}`: replaces the channel. Its kind
/// can't change.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<ChannelInput>,
) -> Result<Json<ChannelView>, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let id = parse_id(&id, "channel")?;
    let mut transaction = state.database.begin().await?;
    let existing = notifications::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Channel not found"))?;
    if existing.kind() != Some(input.kind) {
        return Err(
            ApiError::bad_request("a channel's kind can't change").with_code("invalid_channel")
        );
    }
    let checked = check(&mut transaction, input).await?;
    ensure_name_free(&mut transaction, &checked.name, Some(&id)).await?;
    let mut update = Query::update();
    update
        .table(NotificationChannels::Table)
        .value(NotificationChannels::Name, checked.name.as_str())
        .value(NotificationChannels::Enabled, checked.enabled)
        .value(
            NotificationChannels::WebhookUrl,
            checked.webhook_url.clone(),
        )
        .value(NotificationChannels::UpdatedAt, now_ms())
        .and_where(Expr::col(NotificationChannels::Id).eq(id.as_str()));
    if let Some(secret) = &checked.webhook_secret {
        update.value(
            NotificationChannels::WebhookSecretEncrypted,
            secret.as_ref().map(|secret| {
                state
                    .instance_key
                    .encrypt(&notifications::secret_context(&id), secret.as_bytes())
            }),
        );
    }
    transaction.execute(&update).await?;
    write_members(&mut transaction, &id, &checked).await?;
    let mut details = metadata(&checked);
    details["previous_name"] = json!(existing.name);
    if let Some(secret) = &checked.webhook_secret {
        details["secret"] = json!(if secret.is_some() {
            "replaced"
        } else {
            "removed"
        });
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "notification_channel.update",
        Target::notification_channel(&id),
        details,
    )
    .await?;
    let updated = load_view(&mut transaction, &id).await?;
    transaction.commit().await?;
    Ok(Json(updated))
}

/// `DELETE /v1/notification-channels/{id}`: rules stop notifying it.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let id = parse_id(&id, "channel")?;
    let mut transaction = state.database.begin().await?;
    let channel = notifications::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Channel not found"))?;
    transaction
        .execute(
            &Query::delete()
                .from_table(NotificationChannels::Table)
                .and_where(Expr::col(NotificationChannels::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "notification_channel.delete",
        Target::notification_channel(&id),
        json!({ "name": channel.name, "kind": channel.kind }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    delivered: bool,
    error: Option<String>,
}

/// `POST /v1/notification-channels/{id}/test`: sends a test notification
/// once, even to a turned-off channel, and says how it went.
pub async fn test(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<TestResult>, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let id = parse_id(&id, "channel")?;
    let mut database = &state.database;
    let channel = notifications::by_id(&mut database, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Channel not found"))?;
    let instance_name = settings::load(&mut database).await?.instance_name;
    let outcome = notifications::deliver_now(
        &state,
        &channel,
        &notifications::test_notification(&instance_name, &channel.name),
    )
    .await;
    audit::record(
        &mut database,
        &actor.actor(),
        "notification_channel.test",
        Target::notification_channel(&id),
        json!({ "name": channel.name, "delivered": outcome.is_ok() }),
    )
    .await?;
    Ok(Json(TestResult {
        delivered: outcome.is_ok(),
        error: outcome.err(),
    }))
}
