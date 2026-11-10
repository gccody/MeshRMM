//! `/scim/v2/Groups`. Groups come from the identity provider; an
//! administrator maps a group to a role on the Authentication page, and its
//! members hold that role while they stay in the group.
use std::collections::{BTreeSet, HashMap};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use sea_query::{Expr, ExprTrait, Func, LockType, Order, Query as Sql, SelectStatement};
use serde_json::{Map, Value, json};

use super::{
    GROUP_SCHEMA, ListQuery, Projection, ScimBody, ScimClient, ScimError, Whose, base_url,
    ensure_administrator_remains,
    filter::{self, Filter},
    list_page, meta,
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
    rbac::ADMINISTRATOR_ROLE_ID,
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

impl GroupRow {
    /// Members leaving the group, or its deletion, can take the role from
    /// the last administrator.
    fn grants_administrator(&self) -> bool {
        self.role_id.as_deref() == Some(ADMINISTRATOR_ROLE_ID)
    }
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

/// A condition on groups that SQL tests exactly as [`filter::matches`]
/// would on their resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKey {
    Id(String),
    /// `displayName`, lowercased as `display_name_key` is.
    Name(String),
    ExternalId(String),
    /// The group has this user as a member.
    Member(String),
}

impl GroupKey {
    /// `filter` as a condition SQL tests, if it is one: the ID, the
    /// external ID, `displayName` or a member's ID equals a string.
    fn from_filter(filter: &Filter) -> Option<Self> {
        if let Some(id) = filter::equals(filter, &["id"]) {
            return Some(Self::Id(id.to_owned()));
        }
        if let Some(external_id) = filter::equals(filter, &["externalid"]) {
            return Some(Self::ExternalId(external_id.to_owned()));
        }
        if let Some(user_id) = filter::equals(filter, &["members", "value"]) {
            return Some(Self::Member(user_id.to_owned()));
        }
        filter::equals(filter, &["displayname"]).map(|name| Self::Name(name_key(name)))
    }

    fn condition(&self) -> Expr {
        match self {
            Self::Id(id) => Expr::col((ScimGroups::Table, ScimGroups::Id)).eq(id.as_str()),
            Self::Name(key) => {
                Expr::col((ScimGroups::Table, ScimGroups::DisplayNameKey)).eq(key.as_str())
            }
            Self::ExternalId(external_id) => {
                Expr::col((ScimGroups::Table, ScimGroups::ExternalId)).eq(external_id.as_str())
            }
            Self::Member(user_id) => Expr::col((ScimGroups::Table, ScimGroups::Id)).in_subquery(
                Sql::select()
                    .column((ScimGroupMembers::Table, ScimGroupMembers::GroupId))
                    .from(ScimGroupMembers::Table)
                    .and_where(
                        Expr::col((ScimGroupMembers::Table, ScimGroupMembers::UserId))
                            .eq(user_id.as_str()),
                    )
                    .to_owned(),
            ),
        }
    }
}

/// A display name as filters compare it, kept in `display_name_key`.
fn name_key(display_name: &str) -> String {
    display_name.to_lowercase()
}

/// Splits a list's filter into the conditions SQL tests and the rest, which
/// is evaluated on the groups SQL found.
pub fn plan(filter: Option<Filter>) -> (Vec<GroupKey>, Option<Filter>) {
    filter.map_or_else(Default::default, |filter| {
        filter::split(filter, GroupKey::from_filter)
    })
}

/// Every group `keys` select, in the order lists return them.
pub fn matching_query(keys: &[GroupKey]) -> SelectStatement {
    let mut matching = select();
    for key in keys {
        matching.and_where(key.condition());
    }
    matching
        .order_by((ScimGroups::Table, ScimGroups::DisplayName), Order::Asc)
        .to_owned()
}

/// A page of the groups `keys` select.
pub fn page_query(keys: &[GroupKey], offset: u64, limit: u64) -> SelectStatement {
    matching_query(keys).limit(limit).offset(offset).to_owned()
}

/// How many groups `keys` select.
pub fn count_query(keys: &[GroupKey]) -> SelectStatement {
    let mut select = Sql::select();
    select
        .expr(Func::count(Expr::col((ScimGroups::Table, ScimGroups::Id))))
        .from(ScimGroups::Table);
    for key in keys {
        select.and_where(key.condition());
    }
    select
}

/// The members of the groups `whose` names, as `(group ID, user ID,
/// email)` in email order.
pub fn members_query(whose: Whose<'_, GroupKey>) -> SelectStatement {
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
    let group = Expr::col((ScimGroupMembers::Table, ScimGroupMembers::GroupId));
    match whose {
        Whose::Ids(ids) => {
            select.and_where(group.is_in(ids.iter().map(String::as_str)));
        }
        Whose::Matching([]) => {}
        Whose::Matching(keys) => {
            let mut groups = Sql::select();
            groups
                .column((ScimGroups::Table, ScimGroups::Id))
                .from(ScimGroups::Table);
            for key in keys {
                groups.and_where(key.condition());
            }
            select.and_where(group.in_subquery(groups));
        }
    }
    select
}

/// Each group's members, as `(user ID, email)` in email order.
type Members = HashMap<String, Vec<(String, String)>>;

async fn members_of(
    executor: &mut impl Executor,
    whose: Whose<'_, GroupKey>,
) -> crate::db::Result<Members> {
    if matches!(whose, Whose::Ids([])) {
        return Ok(Members::new());
    }
    let rows: Vec<(String, String, String)> = executor.fetch_all(&members_query(whose)).await?;
    let mut members = Members::new();
    for (group_id, user_id, email) in rows {
        members.entry(group_id).or_default().push((user_id, email));
    }
    Ok(members)
}

fn resource(state: &AppState, group: &GroupRow, members: &Members) -> Value {
    let mut resource = json!({
        "schemas": [GROUP_SCHEMA],
        "id": group.id,
        "displayName": group.display_name,
        "members": members
            .get(&group.id)
            .into_iter()
            .flatten()
            .map(|(user_id, email)| json!({
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

fn ids<'a>(groups: impl IntoIterator<Item = &'a GroupRow>) -> Vec<String> {
    groups.into_iter().map(|group| group.id.clone()).collect()
}

/// The group `id`, with its members if `with_members`.
async fn find(
    executor: &mut impl Executor,
    state: &AppState,
    id: &str,
    with_members: bool,
) -> Result<Value, ScimError> {
    let group = row(executor, id).await?;
    let members = if with_members {
        members_of(executor, Whose::Ids(&ids([&group]))).await?
    } else {
        Members::new()
    };
    Ok(resource(state, &group, &members))
}

async fn load(
    executor: &mut impl Executor,
    state: &AppState,
    id: &str,
) -> Result<Value, ScimError> {
    find(executor, state, id, true).await
}

/// `GET /scim/v2/Groups`. The conditions identity providers look groups up
/// by run in SQL, which pages them too; the rest of a filter is evaluated on
/// the groups those conditions select. Members are read for the page only,
/// and not at all when the response leaves them out.
pub async fn list(
    State(state): State<AppState>,
    _client: ScimClient,
    Query(query): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let (keys, rest) = plan(query.filter()?);
    let page = query.page();
    let with_members = query.projection().returns("members");
    let mut database = &state.database;
    let Some(rest) = rest else {
        let groups: Vec<GroupRow> = if page.count == 0 {
            Vec::new()
        } else {
            database
                .fetch_all(&page_query(&keys, page.offset(), page.limit()))
                .await?
        };
        let total = match page.total(groups.len()) {
            Some(total) => total,
            None => {
                let (count,): (i64,) = database.fetch_one(&count_query(&keys)).await?;
                usize::try_from(count).unwrap_or_default()
            }
        };
        let members = if with_members {
            members_of(&mut database, Whose::Ids(&ids(&groups))).await?
        } else {
            Members::new()
        };
        let resources = groups
            .iter()
            .map(|group| resource(&state, group, &members))
            .collect();
        return Ok(list_page(total, page, resources, &query));
    };
    let groups: Vec<GroupRow> = database.fetch_all(&matching_query(&keys)).await?;
    let filtered_by_members = filter::mentions(&rest, "members");
    let mut members = if filtered_by_members {
        members_of(&mut database, Whose::Matching(&keys)).await?
    } else {
        Members::new()
    };
    let matching = groups
        .iter()
        .filter(|group| filter::matches(&resource(&state, group, &members), &rest))
        .collect::<Vec<_>>();
    let total = matching.len();
    let groups = page.cut(matching);
    if with_members && !filtered_by_members {
        members = members_of(&mut database, Whose::Ids(&ids(groups.iter().copied()))).await?;
    }
    let resources = groups
        .iter()
        .map(|group| resource(&state, group, &members))
        .collect();
    Ok(list_page(total, page, resources, &query))
}

/// `GET /scim/v2/Groups/{id}`
pub async fn get(
    State(state): State<AppState>,
    _client: ScimClient,
    Path(id): Path<String>,
    Query(projection): Query<Projection>,
) -> Result<Response, ScimError> {
    let resource = find(
        &mut &state.database,
        &state,
        &id,
        projection.returns("members"),
    )
    .await?;
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
                    ScimGroups::DisplayNameKey,
                    ScimGroups::ExternalId,
                    ScimGroups::CreatedAt,
                    ScimGroups::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    input.display_name.as_str().into(),
                    name_key(&input.display_name).into(),
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
    let current: BTreeSet<String> = members_of(&mut transaction, Whose::Ids(&[id.to_owned()]))
        .await?
        .remove(id)
        .into_iter()
        .flatten()
        .map(|(user_id, _)| user_id)
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
                    (
                        ScimGroups::DisplayNameKey,
                        name_key(&input.display_name).into(),
                    ),
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
    if !removed.is_empty() && group.grants_administrator() {
        ensure_administrator_remains(&mut transaction).await?;
    }
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
    if group.grants_administrator() {
        ensure_administrator_remains(&mut transaction).await?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> (Vec<GroupKey>, Option<Filter>) {
        plan(Some(filter::parse(text).unwrap()))
    }

    #[test]
    fn lookups_by_name_ids_and_member_run_in_sql() {
        assert_eq!(
            keys(r#"displayName eq "Équipe IT""#),
            (vec![GroupKey::Name("équipe it".into())], None)
        );
        // Entra checks a membership with the group's ID and the user's.
        assert_eq!(
            keys(r#"id eq "G1" and members[value eq "U1"]"#),
            (
                vec![GroupKey::Id("G1".into()), GroupKey::Member("U1".into())],
                None
            )
        );
        assert_eq!(
            keys(r#"members.value eq "U1" and externalId eq "Ext""#),
            (
                vec![
                    GroupKey::Member("U1".into()),
                    GroupKey::ExternalId("Ext".into())
                ],
                None
            )
        );
        assert_eq!(
            keys(r#"displayName eq "IT" and displayName co "i""#),
            (
                vec![GroupKey::Name("it".into())],
                Some(filter::parse(r#"displayName co "i""#).unwrap())
            )
        );
    }

    #[test]
    fn other_filters_are_evaluated_on_the_resources() {
        for text in [
            r#"displayName sw "IT""#,
            r#"displayName eq "a" or displayName eq "b""#,
            r#"not (members[value eq "U1"])"#,
            r#"members[value eq "U1" and display eq "a@example.com"]"#,
            r#"members[display eq "a@example.com"]"#,
            "members pr",
        ] {
            let (keys, rest) = keys(text);
            assert!(keys.is_empty(), "{text}");
            assert_eq!(rest, Some(filter::parse(text).unwrap()), "{text}");
        }
    }
}
