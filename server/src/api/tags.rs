//! Tags: free-form labels for grouping devices besides their company, and
//! adding and removing them on devices.
use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Alias, Expr, ExprTrait, Func, OnConflict, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::parse_id,
    audit::{self, Target},
    auth::Authorized,
    db::{
        Executor,
        tables::{Agents, DeviceTags, Tags},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    tags::{self, Color, MAX_NAME_LENGTH, MAX_TAGS_PER_DEVICE, Tag},
    time::now_ms,
    users::new_id,
};

/// The most devices one request may tag or untag.
pub const MAX_DEVICES_PER_CHANGE: usize = 1_000;

#[derive(Debug, Serialize)]
pub struct TagView {
    #[serde(flatten)]
    tag: Tag,
    /// Enrolled devices with the tag.
    device_count: i64,
    /// Automation policies and alert rules that target it.
    policy_count: i64,
    rule_count: i64,
}

/// How many rows of `table` name each tag; for device tags, only those of
/// devices that aren't deleted.
async fn counts(
    executor: &mut impl Executor,
    table: &'static str,
) -> Result<HashMap<String, i64>, ApiError> {
    let column = || Expr::col((Alias::new(table), Alias::new("tag_id")));
    let mut select = Query::select();
    select
        .expr(column())
        .expr(Func::count(column()))
        .from(Alias::new(table))
        .add_group_by([column()]);
    if table == "device_tags" {
        select.and_where(
            Expr::col((DeviceTags::Table, DeviceTags::DeviceId))
                .in_subquery(crate::agents::active()),
        );
    }
    let rows: Vec<(String, i64)> = executor.fetch_all(&select).await?;
    Ok(rows.into_iter().collect())
}

async fn views(executor: &mut impl Executor, tags: Vec<Tag>) -> Result<Vec<TagView>, ApiError> {
    let devices = counts(executor, "device_tags").await?;
    let policies = counts(executor, "automation_policy_tags").await?;
    let rules = counts(executor, "alert_rule_tags").await?;
    Ok(tags
        .into_iter()
        .map(|tag| TagView {
            device_count: devices.get(&tag.id).copied().unwrap_or_default(),
            policy_count: policies.get(&tag.id).copied().unwrap_or_default(),
            rule_count: rules.get(&tag.id).copied().unwrap_or_default(),
            tag,
        })
        .collect())
}

async fn find(executor: &mut impl Executor, id: &str) -> Result<Tag, ApiError> {
    tags::load(executor, Some(id))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("no such tag"))
}

/// `GET /v1/tags`: every tag, by name, with how many devices have it and
/// how many policies and rules target it.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Vec<TagView>>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let mut database = &state.database;
    let tags = tags::load(&mut database, None).await?;
    Ok(Json(views(&mut database, tags).await?))
}

fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the tag name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
        ))
        .with_code("invalid_tag"));
    }
    Ok(name.to_owned())
}

async fn ensure_name_free(
    executor: &mut impl Executor,
    name: &str,
    except_id: Option<&str>,
) -> Result<(), ApiError> {
    // Compared here rather than with SQL lower(), which SQLite applies to
    // ASCII letters only.
    let wanted = name.to_lowercase();
    if tags::load(executor, None)
        .await?
        .iter()
        .any(|tag| Some(tag.id.as_str()) != except_id && tag.name.to_lowercase() == wanted)
    {
        return Err(
            ApiError::conflict("a tag with this name already exists").with_code("tag_exists")
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTag {
    name: String,
    color: Color,
}

/// `POST /v1/tags`
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<NewTag>,
) -> Result<(StatusCode, Json<TagView>), ApiError> {
    actor.require(Permission::TagsManage)?;
    let name = validate_name(&request.name)?;
    let mut transaction = state.database.begin().await?;
    ensure_name_free(&mut transaction, &name, None).await?;
    let id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(Tags::Table)
                .columns([
                    Tags::Id,
                    Tags::Name,
                    Tags::Color,
                    Tags::CreatedAt,
                    Tags::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    name.as_str().into(),
                    request.color.as_str().into(),
                    now.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "tag.create",
        Target::tag(&id),
        json!({ "name": name, "color": request.color.as_str() }),
    )
    .await?;
    transaction.commit().await?;
    state.presence.refresh_tag(&id).await;
    Ok((
        StatusCode::CREATED,
        Json(TagView {
            tag: Tag {
                id,
                name,
                color: request.color.as_str().to_owned(),
            },
            device_count: 0,
            policy_count: 0,
            rule_count: 0,
        }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagChange {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    color: Option<Color>,
}

/// `PATCH /v1/tags/{id}`: renames or recolors the tag.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<TagChange>,
) -> Result<Json<TagView>, ApiError> {
    actor.require(Permission::TagsManage)?;
    let id = parse_id(&id, "tag")?;
    let name = request.name.as_deref().map(validate_name).transpose()?;
    let mut transaction = state.database.begin().await?;
    let previous = find(&mut transaction, &id).await?;
    let mut tag = previous.clone();
    if let Some(name) = name {
        ensure_name_free(&mut transaction, &name, Some(&id)).await?;
        tag.name = name;
    }
    if let Some(color) = request.color {
        color.as_str().clone_into(&mut tag.color);
    }
    if tag != previous {
        transaction
            .execute(
                &Query::update()
                    .table(Tags::Table)
                    .value(Tags::Name, tag.name.as_str())
                    .value(Tags::Color, tag.color.as_str())
                    .value(Tags::UpdatedAt, now_ms())
                    .and_where(Expr::col(Tags::Id).eq(id.as_str()))
                    .to_owned(),
            )
            .await?;
        audit::record(
            &mut transaction,
            &actor.actor(),
            "tag.update",
            Target::tag(&id),
            json!({
                "name": tag.name,
                "previous_name": previous.name,
                "color": tag.color,
                "previous_color": previous.color,
            }),
        )
        .await?;
    }
    let view = views(&mut transaction, vec![tag])
        .await?
        .pop()
        .expect("one tag in, one out");
    transaction.commit().await?;
    state.presence.refresh_tag(&id).await;
    Ok(Json(view))
}

/// `DELETE /v1/tags/{id}`: deletes the tag and takes it off its devices.
/// Policies and rules that target it no longer cover its devices.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::TagsManage)?;
    let id = parse_id(&id, "tag")?;
    let mut transaction = state.database.begin().await?;
    let tag = find(&mut transaction, &id).await?;
    let devices: Vec<(String,)> = transaction
        .fetch_all(
            &Query::select()
                .column(DeviceTags::DeviceId)
                .from(DeviceTags::Table)
                .and_where(Expr::col(DeviceTags::TagId).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    // The foreign keys remove the tag from devices, policies and rules.
    transaction
        .execute(
            &Query::delete()
                .from_table(Tags::Table)
                .and_where(Expr::col(Tags::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "tag.delete",
        Target::tag(&id),
        json!({ "name": tag.name, "device_count": devices.len() }),
    )
    .await?;
    transaction.commit().await?;
    for (device_id,) in &devices {
        state.presence.refresh(device_id).await;
    }
    state.presence.refresh_tag(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceTagChange {
    device_ids: Vec<String>,
    /// Tags to add to every device.
    #[serde(default)]
    add: Vec<String>,
    /// Tags to take off every device.
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DeviceTagResult {
    /// How many devices' tags changed.
    changed: usize,
}

/// Sorted, without duplicates, each checked to be an ID.
fn ids(mut ids: Vec<String>, what: &str) -> Result<Vec<String>, ApiError> {
    for id in &ids {
        parse_id(id, what)?;
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// `POST /v1/device-tags`: adds tags to devices and takes others off them,
/// one device or many. Each device whose tags changed is recorded in the
/// audit log.
pub async fn change(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<DeviceTagChange>,
) -> Result<Json<DeviceTagResult>, ApiError> {
    actor.require(Permission::DevicesEdit)?;
    let invalid = |message: String| ApiError::bad_request(message).with_code("invalid_tags");
    let device_ids = ids(request.device_ids, "device")?;
    let add = ids(request.add, "tag")?;
    let remove = ids(request.remove, "tag")?;
    if device_ids.is_empty() || device_ids.len() > MAX_DEVICES_PER_CHANGE {
        return Err(invalid(format!(
            "choose 1 to {MAX_DEVICES_PER_CHANGE} devices"
        )));
    }
    if add.is_empty() && remove.is_empty() {
        return Err(invalid("choose tags to add or remove".to_owned()));
    }
    if add.iter().any(|id| remove.contains(id)) {
        return Err(invalid("a tag can't be both added and removed".to_owned()));
    }
    let mut transaction = state.database.begin().await?;
    let names: HashMap<String, String> = tags::load(&mut transaction, None)
        .await?
        .into_iter()
        .map(|tag| (tag.id, tag.name))
        .collect();
    if add.iter().chain(&remove).any(|id| !names.contains_key(id)) {
        return Err(ApiError::not_found("a chosen tag no longer exists"));
    }
    let mut devices: Vec<(String, String)> = Vec::new();
    for chunk in device_ids.chunks(500) {
        devices.extend(
            transaction
                .fetch_all::<(String, String), _>(
                    &crate::agents::active()
                        .column(Agents::Name)
                        .and_where(Expr::col(Agents::Id).is_in(chunk.iter().cloned()))
                        .to_owned(),
                )
                .await?,
        );
    }
    if devices.len() != device_ids.len() {
        return Err(ApiError::not_found("a chosen device no longer exists"));
    }
    let mut current = HashMap::<String, HashSet<String>>::new();
    for chunk in device_ids.chunks(500) {
        let rows: Vec<(String, String)> = transaction
            .fetch_all(
                &Query::select()
                    .columns([DeviceTags::DeviceId, DeviceTags::TagId])
                    .from(DeviceTags::Table)
                    .and_where(Expr::col(DeviceTags::DeviceId).is_in(chunk.iter().cloned()))
                    .to_owned(),
            )
            .await?;
        for (device_id, tag_id) in rows {
            current.entry(device_id).or_default().insert(tag_id);
        }
    }
    let now = now_ms();
    let actor_label = actor.actor();
    let mut changed = Vec::new();
    for (device_id, device_name) in &devices {
        let has = current.remove(device_id).unwrap_or_default();
        let added = add
            .iter()
            .filter(|id| !has.contains(*id))
            .collect::<Vec<_>>();
        let removed = remove
            .iter()
            .filter(|id| has.contains(*id))
            .collect::<Vec<_>>();
        if added.is_empty() && removed.is_empty() {
            continue;
        }
        if has.len() + added.len() - removed.len() > MAX_TAGS_PER_DEVICE {
            return Err(invalid(format!(
                "{device_name} would have more than {MAX_TAGS_PER_DEVICE} tags"
            )));
        }
        if !added.is_empty() {
            let mut insert = Query::insert();
            insert
                .into_table(DeviceTags::Table)
                .columns([
                    DeviceTags::DeviceId,
                    DeviceTags::TagId,
                    DeviceTags::CreatedAt,
                ])
                .on_conflict(
                    OnConflict::columns([DeviceTags::DeviceId, DeviceTags::TagId])
                        .do_nothing()
                        .to_owned(),
                );
            for tag_id in &added {
                insert.values_panic([
                    device_id.as_str().into(),
                    tag_id.as_str().into(),
                    now.into(),
                ]);
            }
            transaction.execute(&insert).await?;
        }
        if !removed.is_empty() {
            transaction
                .execute(
                    &Query::delete()
                        .from_table(DeviceTags::Table)
                        .and_where(Expr::col(DeviceTags::DeviceId).eq(device_id.as_str()))
                        .and_where(
                            Expr::col(DeviceTags::TagId).is_in(removed.iter().copied().cloned()),
                        )
                        .to_owned(),
                )
                .await?;
        }
        let named = |ids: &[&String]| ids.iter().map(|id| names[*id].clone()).collect::<Vec<_>>();
        audit::record(
            &mut transaction,
            &actor_label,
            "agent.tag",
            Target::device(device_id),
            json!({
                "name": device_name,
                "added": named(&added),
                "removed": named(&removed),
            }),
        )
        .await?;
        changed.push(device_id.clone());
    }
    transaction.commit().await?;
    for device_id in &changed {
        state.presence.refresh(device_id).await;
    }
    Ok(Json(DeviceTagResult {
        changed: changed.len(),
    }))
}
