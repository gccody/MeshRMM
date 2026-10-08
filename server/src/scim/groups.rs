//! `/scim/v2/Groups`. Groups come from the identity provider; an
//! administrator maps a group to a role on the Authentication page, and its
//! members hold that role while they stay in the group.
use std::collections::BTreeSet;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use sea_query::{Expr, ExprTrait, Func, LockType, Order, Query as Sql};
use serde_json::{Map, Value, json};

use super::{
    GROUP_SCHEMA, ListQuery, Projection, ScimBody, ScimClient, ScimError, base_url, filter,
    list_response, meta,
    patch::{Operation, PatchRequest},
    scim_json, text,
};
use crate::{
    audit::{self, Target},
    db::{
        Executor,
        tables::{ScimGroupMembers, ScimGroups, Users},
    },
    http::AppState,
    time::now_ms,
    users::new_id,
};

const MAX_DISPLAY_NAME_LENGTH: usize = 256;

#[derive(Debug, Clone, sqlx::FromRow)]
struct GroupRow {
    id: String,
    display_name: String,
    external_id: Option<String>,
    role_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug)]
struct GroupInput {
    display_name: String,
    external_id: Option<String>,
    members: BTreeSet<String>,
}

impl GroupInput {
    fn from_json(resource: &Value) -> Result<Self, ScimError> {
        let display_name = text(resource, "displayName")
            .filter(|name| {
                name.chars().count() <= MAX_DISPLAY_NAME_LENGTH
                    && !name.chars().any(char::is_control)
            })
            .ok_or_else(|| {
                ScimError::bad_request(
                    "invalidValue",
                    "displayName is required and must be at most 256 characters",
                )
            })?;
        let external_id = text(resource, "externalId");
        if external_id.as_ref().is_some_and(|id| id.len() > 512) {
            return Err(ScimError::bad_request(
                "invalidValue",
                "externalId is too long",
            ));
        }
        let members = match filter::member(resource, "members") {
            None | Some(Value::Null) => BTreeSet::new(),
            Some(Value::Array(members)) => members
                .iter()
                .map(|member| {
                    filter::member(member, "value")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            ScimError::bad_request("invalidValue", "each member needs a value")
                        })
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(ScimError::bad_request(
                    "invalidValue",
                    "members must be a list",
                ));
            }
        };
        Ok(Self {
            display_name,
            external_id,
            members,
        })
    }
}

fn select() -> sea_query::SelectStatement {
    Sql::select()
        .columns([
            ScimGroups::Id,
            ScimGroups::DisplayName,
            ScimGroups::ExternalId,
            ScimGroups::RoleId,
            ScimGroups::CreatedAt,
            ScimGroups::UpdatedAt,
        ])
        .from(ScimGroups::Table)
        .to_owned()
}

/// Members as `(group ID, user ID, email)`.
async fn members(
    executor: &mut impl Executor,
    group_id: Option<&str>,
) -> crate::db::Result<Vec<(String, String, String)>> {
    let mut select = Sql::select();
    select
        .column((ScimGroupMembers::Table, ScimGroupMembers::GroupId))
        .column((Users::Table, Users::Id))
        .column((Users::Table, Users::Email))
        .from(ScimGroupMembers::Table)
        .inner_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((ScimGroupMembers::Table, ScimGroupMembers::UserId)),
        )
        .order_by((Users::Table, Users::Email), Order::Asc);
    if let Some(group_id) = group_id {
        select.and_where(
            Expr::col((ScimGroupMembers::Table, ScimGroupMembers::GroupId)).eq(group_id),
        );
    }
    executor.fetch_all(&select).await
}

fn resource(state: &AppState, group: &GroupRow, members: &[(String, String, String)]) -> Value {
    let mut resource = json!({
        "schemas": [GROUP_SCHEMA],
        "id": group.id,
        "displayName": group.display_name,
        "members": members
            .iter()
            .filter(|(group_id, _, _)| *group_id == group.id)
            .map(|(_, user_id, email)| json!({
                "value": user_id,
                "display": email,
                "type": "User",
                "$ref": format!("{}/Users/{user_id}", base_url(state)),
            }))
            .collect::<Vec<_>>(),
        "meta": meta(state, "Group", "Groups", &group.id, group.created_at, group.updated_at),
    });
    if let Some(external_id) = &group.external_id {
        resource["externalId"] = json!(external_id);
    }
    resource
}

async fn row(executor: &mut impl Executor, id: &str) -> Result<GroupRow, ScimError> {
    executor
        .fetch_optional(
            &select()
                .and_where(Expr::col(ScimGroups::Id).eq(id))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ScimError::not_found("no such group"))
}

async fn load(
    executor: &mut impl Executor,
    state: &AppState,
    id: &str,
) -> Result<Value, ScimError> {
    let group = row(executor, id).await?;
    let members = members(executor, Some(id)).await?;
    Ok(resource(state, &group, &members))
}

/// `GET /scim/v2/Groups`
pub async fn list(
    State(state): State<AppState>,
    _client: ScimClient,
    Query(query): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let mut database = &state.database;
    let groups: Vec<GroupRow> = database
        .fetch_all(
            &select()
                .order_by(ScimGroups::DisplayName, Order::Asc)
                .to_owned(),
        )
        .await?;
    let members = members(&mut database, None).await?;
    list_response(
        groups
            .iter()
            .map(|group| resource(&state, group, &members))
            .collect(),
        &query,
    )
}

/// `GET /scim/v2/Groups/{id}`
pub async fn get(
    State(state): State<AppState>,
    _client: ScimClient,
    Path(id): Path<String>,
    Query(projection): Query<Projection>,
) -> Result<Response, ScimError> {
    let resource = load(&mut &state.database, &state, &id).await?;
    Ok(scim_json(StatusCode::OK, projection.apply(resource)))
}

/// Fails if another group has the name or external ID, or a member isn't a
/// user.
async fn check(
    executor: &mut impl Executor,
    input: &GroupInput,
    except: Option<&str>,
) -> Result<(), ScimError> {
    let mut clash = select();
    // Filters match display names without case, so names must differ by
    // more than case for a lookup to find one group.
    let mut same = Expr::expr(Func::lower(Expr::col(ScimGroups::DisplayName)))
        .eq(input.display_name.to_lowercase());
    if let Some(external_id) = &input.external_id {
        same = same.or(Expr::col(ScimGroups::ExternalId).eq(external_id.as_str()));
    }
    clash.and_where(same);
    if let Some(except) = except {
        clash.and_where(Expr::col(ScimGroups::Id).ne(except));
    }
    if let Some(existing) = executor.fetch_optional::<GroupRow, _>(&clash).await? {
        return Err(ScimError::uniqueness(
            if existing.display_name.to_lowercase() == input.display_name.to_lowercase() {
                "a group with this displayName already exists"
            } else {
                "a group with this externalId already exists"
            },
        ));
    }
    if input.members.is_empty() {
        return Ok(());
    }
    let found: Vec<(String,)> = executor
        .fetch_all(
            &Sql::select()
                .column(Users::Id)
                .from(Users::Table)
                .and_where(Expr::col(Users::Id).is_in(input.members.iter().cloned()))
                .to_owned(),
        )
        .await?;
    let found = found.into_iter().map(|(id,)| id).collect::<BTreeSet<_>>();
    let unknown = input
        .members
        .difference(&found)
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(ScimError::bad_request(
            "invalidValue",
            format!("these members aren't users here: {}", unknown.join(", ")),
        ));
    }
    Ok(())
}

async fn add_members(
    executor: &mut impl Executor,
    group_id: &str,
    user_ids: &[&String],
) -> crate::db::Result<()> {
    if user_ids.is_empty() {
        return Ok(());
    }
    let mut insert = Sql::insert();
    insert
        .into_table(ScimGroupMembers::Table)
        .columns([ScimGroupMembers::GroupId, ScimGroupMembers::UserId]);
    for user_id in user_ids {
        insert.values_panic([group_id.into(), user_id.as_str().into()]);
    }
    executor.execute(&insert).await?;
    Ok(())
}

fn with_location(state: &AppState, status: StatusCode, resource: Value) -> Response {
    let location = format!(
        "{}/Groups/{}",
        base_url(state),
        resource["id"].as_str().unwrap_or_default()
    );
    let mut response = scim_json(status, resource);
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

/// `POST /scim/v2/Groups`
pub async fn create(
    State(state): State<AppState>,
    client: ScimClient,
    ScimBody(body): ScimBody<Value>,
) -> Result<Response, ScimError> {
    let input = GroupInput::from_json(&body)?;
    let mut transaction = state.database.begin().await?;
    check(&mut transaction, &input, None).await?;
    let id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Sql::insert()
                .into_table(ScimGroups::Table)
                .columns([
                    ScimGroups::Id,
                    ScimGroups::DisplayName,
                    ScimGroups::ExternalId,
                    ScimGroups::CreatedAt,
                    ScimGroups::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    input.display_name.as_str().into(),
                    input.external_id.clone().into(),
                    now.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    add_members(
        &mut transaction,
        &id,
        &input.members.iter().collect::<Vec<_>>(),
    )
    .await?;
    audit::record(
        &mut transaction,
        &client.actor,
        "scim.group_create",
        Target::scim_group(&id),
        json!({
            "display_name": input.display_name,
            "external_id": input.external_id,
            "members": input.members,
        }),
    )
    .await?;
    let resource = load(&mut transaction, &state, &id).await?;
    transaction.commit().await?;
    Ok(with_location(&state, StatusCode::CREATED, resource))
}

/// A PUT's new group, or a PATCH's operations.
enum Change {
    Replace(GroupInput),
    Patch(Vec<Operation>),
}

/// Applies `change` to the group `id` and returns the new resource. A patch
/// is applied to the group as read in the same transaction, with its row
/// locked, so concurrent changes can't undo each other.
async fn update(
    state: &AppState,
    client: &ScimClient,
    id: &str,
    change: Change,
) -> Result<Value, ScimError> {
    let mut transaction = state.database.begin().await?;
    // sea-query leaves FOR UPDATE out on SQLite, whose write lock already
    // serializes the transaction.
    transaction
        .fetch_optional::<(String,), _>(
            &Sql::select()
                .column(ScimGroups::Id)
                .from(ScimGroups::Table)
                .and_where(Expr::col(ScimGroups::Id).eq(id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let input = match change {
        Change::Replace(input) => input,
        Change::Patch(operations) => {
            let mut resource = load(&mut transaction, state, id).await?;
            super::patch::apply(&mut resource, &operations)?;
            GroupInput::from_json(&resource)?
        }
    };
    let group = row(&mut transaction, id).await?;
    check(&mut transaction, &input, Some(id)).await?;
    let current: BTreeSet<String> = members(&mut transaction, Some(id))
        .await?
        .into_iter()
        .map(|(_, user_id, _)| user_id)
        .collect();
    let added = input.members.difference(&current).collect::<Vec<_>>();
    let removed = current.difference(&input.members).collect::<Vec<_>>();
    let mut changes = Map::new();
    if input.display_name != group.display_name {
        changes.insert("display_name".into(), json!(input.display_name));
    }
    if input.external_id != group.external_id {
        changes.insert("external_id".into(), json!(input.external_id));
    }
    if !added.is_empty() {
        changes.insert("members_added".into(), json!(added));
    }
    if !removed.is_empty() {
        changes.insert("members_removed".into(), json!(removed));
    }
    if changes.is_empty() {
        let resource = load(&mut transaction, state, id).await?;
        transaction.commit().await?;
        return Ok(resource);
    }
    transaction
        .execute(
            &Sql::update()
                .table(ScimGroups::Table)
                .values([
                    (ScimGroups::DisplayName, input.display_name.as_str().into()),
                    (ScimGroups::ExternalId, input.external_id.clone().into()),
                    (ScimGroups::UpdatedAt, now_ms().into()),
                ])
                .and_where(Expr::col(ScimGroups::Id).eq(id))
                .to_owned(),
        )
        .await?;
    if !removed.is_empty() {
        transaction
            .execute(
                &Sql::delete()
                    .from_table(ScimGroupMembers::Table)
                    .and_where(Expr::col(ScimGroupMembers::GroupId).eq(id))
                    .and_where(
                        Expr::col(ScimGroupMembers::UserId)
                            .is_in(removed.iter().map(|id| (*id).clone())),
                    )
                    .to_owned(),
            )
            .await?;
    }
    add_members(&mut transaction, id, &added).await?;
    audit::record(
        &mut transaction,
        &client.actor,
        "scim.group_update",
        Target::scim_group(id),
        changes.into(),
    )
    .await?;
    let resource = load(&mut transaction, state, id).await?;
    transaction.commit().await?;
    if group.role_id.is_some() && (!added.is_empty() || !removed.is_empty()) {
        state.presence.recheck_access();
    }
    Ok(resource)
}

/// `PUT /scim/v2/Groups/{id}`
pub async fn replace(
    State(state): State<AppState>,
    client: ScimClient,
    Path(id): Path<String>,
    ScimBody(body): ScimBody<Value>,
) -> Result<Response, ScimError> {
    let input = GroupInput::from_json(&body)?;
    Ok(scim_json(
        StatusCode::OK,
        update(&state, &client, &id, Change::Replace(input)).await?,
    ))
}

/// `PATCH /scim/v2/Groups/{id}`
pub async fn patch(
    State(state): State<AppState>,
    client: ScimClient,
    Path(id): Path<String>,
    ScimBody(request): ScimBody<PatchRequest>,
) -> Result<Response, ScimError> {
    Ok(scim_json(
        StatusCode::OK,
        update(&state, &client, &id, Change::Patch(request.operations)).await?,
    ))
}

/// `DELETE /scim/v2/Groups/{id}`
pub async fn delete(
    State(state): State<AppState>,
    client: ScimClient,
    Path(id): Path<String>,
) -> Result<StatusCode, ScimError> {
    let mut transaction = state.database.begin().await?;
    let group = row(&mut transaction, &id).await?;
    transaction
        .execute(
            &Sql::delete()
                .from_table(ScimGroups::Table)
                .and_where(Expr::col(ScimGroups::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &client.actor,
        "scim.group_delete",
        Target::scim_group(&id),
        json!({ "display_name": group.display_name }),
    )
    .await?;
    transaction.commit().await?;
    if group.role_id.is_some() {
        state.presence.recheck_access();
    }
    Ok(StatusCode::NO_CONTENT)
}
