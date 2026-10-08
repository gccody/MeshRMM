//! `/scim/v2/Users`. Deactivating a user disables the account, which ends
//! their sessions, event sockets and remote sessions; deleting one removes
//! it. Neither may leave the server without an enabled administrator.
use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use sea_query::{Expr, ExprTrait, LockType, Order, Query as Sql};
use serde_json::{Map, Value, json};

use super::{
    ListQuery, Projection, ScimBody, ScimClient, ScimError, USER_SCHEMA, base_url, boolean, filter,
    list_response, meta,
    patch::{Operation, PatchRequest},
    scim_json, text,
};
use crate::{
    api::ensure_administrator_remains,
    audit::{self, Target},
    db::{
        Executor,
        tables::{ScimGroupMembers, ScimGroups, Users},
    },
    http::AppState,
    time::now_ms,
    users::{self, MAX_DISPLAY_NAME_LENGTH, NewUser, User, new_id},
};

/// What a SCIM request says about a user.
#[derive(Debug)]
struct UserInput {
    email: String,
    display_name: String,
    /// `None` leaves the account as it is (a PUT without `active`).
    active: Option<bool>,
    external_id: Option<String>,
}

impl UserInput {
    fn from_json(resource: &Value) -> Result<Self, ScimError> {
        let user_name = text(resource, "userName")
            .ok_or_else(|| ScimError::bad_request("invalidValue", "userName is required"))?;
        let email = users::normalize_email(&user_name).map_err(|_| {
            ScimError::bad_request(
                "invalidValue",
                "userName must be the user's email address, which MeshRMM signs them in with",
            )
        })?;
        let name = filter::member(resource, "name");
        let name_part = |key: &str| name.and_then(|name| text(name, key));
        let display_name = text(resource, "displayName")
            .or_else(|| name_part("formatted"))
            .or_else(|| match (name_part("givenName"), name_part("familyName")) {
                (Some(given), Some(family)) => Some(format!("{given} {family}")),
                (given, family) => given.or(family),
            })
            .and_then(|name| users::validate_display_name(&name).ok())
            .unwrap_or_else(|| {
                let local = email.split('@').next().unwrap_or(&email);
                local.chars().take(MAX_DISPLAY_NAME_LENGTH).collect()
            });
        let external_id = text(resource, "externalId");
        if external_id.as_ref().is_some_and(|id| id.len() > 512) {
            return Err(ScimError::bad_request(
                "invalidValue",
                "externalId is too long",
            ));
        }
        Ok(Self {
            email,
            display_name,
            active: boolean(resource, "active")?,
            external_id,
        })
    }
}

/// The groups each user belongs to, as `(user ID, group ID, group name)`.
async fn memberships(
    executor: &mut impl Executor,
    user_id: Option<&str>,
) -> crate::db::Result<Vec<(String, String, String)>> {
    let mut select = Sql::select();
    select
        .column((ScimGroupMembers::Table, ScimGroupMembers::UserId))
        .column((ScimGroups::Table, ScimGroups::Id))
        .column((ScimGroups::Table, ScimGroups::DisplayName))
        .from(ScimGroupMembers::Table)
        .inner_join(
            ScimGroups::Table,
            Expr::col((ScimGroups::Table, ScimGroups::Id))
                .equals((ScimGroupMembers::Table, ScimGroupMembers::GroupId)),
        )
        .order_by((ScimGroups::Table, ScimGroups::DisplayName), Order::Asc);
    if let Some(user_id) = user_id {
        select
            .and_where(Expr::col((ScimGroupMembers::Table, ScimGroupMembers::UserId)).eq(user_id));
    }
    executor.fetch_all(&select).await
}

fn resource(state: &AppState, user: &User, groups: &[(String, String, String)]) -> Value {
    let mut resource = json!({
        "schemas": [USER_SCHEMA],
        "id": user.id,
        "userName": user.email,
        "displayName": user.display_name,
        "name": { "formatted": user.display_name },
        "emails": [{ "value": user.email, "type": "work", "primary": true }],
        "active": !user.disabled,
        "groups": groups
            .iter()
            .filter(|(member, _, _)| *member == user.id)
            .map(|(_, id, name)| json!({
                "value": id,
                "display": name,
                "$ref": format!("{}/Groups/{id}", base_url(state)),
            }))
            .collect::<Vec<_>>(),
        "meta": meta(state, "User", "Users", &user.id, user.created_at, user.updated_at),
    });
    if let Some(external_id) = &user.scim_external_id {
        resource["externalId"] = json!(external_id);
    }
    resource
}

async fn load(
    executor: &mut impl Executor,
    state: &AppState,
    id: &str,
) -> Result<Value, ScimError> {
    let user = users::by_id(executor, id)
        .await?
        .ok_or_else(|| ScimError::not_found("no such user"))?;
    let groups = memberships(executor, Some(&user.id)).await?;
    Ok(resource(state, &user, &groups))
}

fn with_location(state: &AppState, status: StatusCode, resource: Value) -> Response {
    let location = format!(
        "{}/Users/{}",
        base_url(state),
        resource["id"].as_str().unwrap_or_default()
    );
    let mut response = scim_json(status, resource);
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

/// `GET /scim/v2/Users`
pub async fn list(
    State(state): State<AppState>,
    _client: ScimClient,
    Query(query): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let mut database = &state.database;
    let all: Vec<User> = database
        .fetch_all(
            &users::select()
                .order_by(Users::CreatedAt, Order::Asc)
                .to_owned(),
        )
        .await?;
    let groups = memberships(&mut database, None).await?;
    list_response(
        all.iter()
            .map(|user| resource(&state, user, &groups))
            .collect(),
        &query,
    )
}

/// `GET /scim/v2/Users/{id}`
pub async fn get(
    State(state): State<AppState>,
    _client: ScimClient,
    Path(id): Path<String>,
    Query(projection): Query<Projection>,
) -> Result<Response, ScimError> {
    let resource = load(&mut &state.database, &state, &id).await?;
    Ok(scim_json(StatusCode::OK, projection.apply(resource)))
}

/// Fails if another account already has the email or external ID.
async fn ensure_unique(
    executor: &mut impl Executor,
    input: &UserInput,
    except: Option<&str>,
) -> Result<(), ScimError> {
    let mut clash = users::select();
    let mut same = Expr::col(Users::Email).eq(input.email.as_str());
    if let Some(external_id) = &input.external_id {
        same = same.or(Expr::col(Users::ScimExternalId).eq(external_id.as_str()));
    }
    clash.and_where(same);
    if let Some(except) = except {
        clash.and_where(Expr::col(Users::Id).ne(except));
    }
    let existing: Option<User> = executor.fetch_optional(&clash).await?;
    match existing {
        Some(user) if user.email == input.email => Err(ScimError::uniqueness(
            "a user with this userName already exists",
        )),
        Some(_) => Err(ScimError::uniqueness(
            "a user with this externalId already exists",
        )),
        None => Ok(()),
    }
}

/// `POST /scim/v2/Users`
pub async fn create(
    State(state): State<AppState>,
    client: ScimClient,
    ScimBody(body): ScimBody<Value>,
) -> Result<Response, ScimError> {
    let input = UserInput::from_json(&body)?;
    let mut transaction = state.database.begin().await?;
    ensure_unique(&mut transaction, &input, None).await?;
    let id = new_id();
    let now = now_ms();
    users::insert(
        &mut transaction,
        NewUser {
            id: &id,
            email: &input.email,
            display_name: &input.display_name,
            password_hash: None,
            role_ids: &[],
            now_ms: now,
        },
    )
    .await?;
    let disabled = input.active == Some(false);
    transaction
        .execute(
            &Sql::update()
                .table(Users::Table)
                .values([
                    (Users::ScimExternalId, input.external_id.clone().into()),
                    (Users::ScimManaged, true.into()),
                    (Users::Disabled, disabled.into()),
                ])
                .and_where(Expr::col(Users::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &client.actor,
        "scim.user_create",
        Target::user(&id),
        json!({
            "email": input.email,
            "display_name": input.display_name,
            "disabled": disabled,
            "external_id": input.external_id,
        }),
    )
    .await?;
    let resource = load(&mut transaction, &state, &id).await?;
    transaction.commit().await?;
    Ok(with_location(&state, StatusCode::CREATED, resource))
}

/// A PUT's new user, or a PATCH's operations.
enum Change {
    Replace(UserInput),
    Patch(Vec<Operation>),
}

/// Applies `change` to the user `id` and returns the new resource. A patch
/// is applied to the user as read in the same transaction, with its row
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
                .column(Users::Id)
                .from(Users::Table)
                .and_where(Expr::col(Users::Id).eq(id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let input = match change {
        Change::Replace(input) => input,
        Change::Patch(operations) => {
            let mut resource = load(&mut transaction, state, id).await?;
            super::patch::apply(&mut resource, &operations)?;
            UserInput::from_json(&resource)?
        }
    };
    let user = users::by_id(&mut transaction, id)
        .await?
        .ok_or_else(|| ScimError::not_found("no such user"))?;
    ensure_unique(&mut transaction, &input, Some(id)).await?;
    let mut changes = Map::new();
    let mut values = vec![(Users::ScimManaged, true.into())];
    if input.email != user.email {
        changes.insert("email".into(), json!(input.email));
        values.push((Users::Email, input.email.as_str().into()));
    }
    if input.display_name != user.display_name {
        changes.insert("display_name".into(), json!(input.display_name));
        values.push((Users::DisplayName, input.display_name.as_str().into()));
    }
    if input.external_id != user.scim_external_id {
        changes.insert("external_id".into(), json!(input.external_id));
        values.push((Users::ScimExternalId, input.external_id.clone().into()));
    }
    let disable = input.active.map(|active| !active);
    let disabling = disable == Some(true) && !user.disabled;
    if let Some(disabled) = disable.filter(|disabled| *disabled != user.disabled) {
        changes.insert("disabled".into(), json!(disabled));
        values.push((Users::Disabled, disabled.into()));
    }
    if !changes.is_empty() {
        values.push((Users::UpdatedAt, now_ms().into()));
    }
    transaction
        .execute(
            &Sql::update()
                .table(Users::Table)
                .values(values)
                .and_where(Expr::col(Users::Id).eq(id))
                .to_owned(),
        )
        .await?;
    if disabling {
        users::end_sessions(&mut transaction, id, None).await?;
        ensure_administrator_remains(&mut transaction).await?;
    }
    if !changes.is_empty() {
        // Names the user in the audit log, whatever changed.
        changes.entry("email").or_insert_with(|| json!(user.email));
        audit::record(
            &mut transaction,
            &client.actor,
            "scim.user_update",
            Target::user(id),
            changes.into(),
        )
        .await?;
    }
    let resource = load(&mut transaction, state, id).await?;
    transaction.commit().await?;
    if disabling {
        state.presence.recheck_access();
    }
    Ok(resource)
}

/// `PUT /scim/v2/Users/{id}`
pub async fn replace(
    State(state): State<AppState>,
    client: ScimClient,
    Path(id): Path<String>,
    ScimBody(body): ScimBody<Value>,
) -> Result<Response, ScimError> {
    let input = UserInput::from_json(&body)?;
    let resource = update(&state, &client, &id, Change::Replace(input)).await?;
    Ok(scim_json(StatusCode::OK, resource))
}

/// `PATCH /scim/v2/Users/{id}`
pub async fn patch(
    State(state): State<AppState>,
    client: ScimClient,
    Path(id): Path<String>,
    ScimBody(request): ScimBody<PatchRequest>,
) -> Result<Response, ScimError> {
    let resource = update(&state, &client, &id, Change::Patch(request.operations)).await?;
    Ok(scim_json(StatusCode::OK, resource))
}

/// `DELETE /scim/v2/Users/{id}`
pub async fn delete(
    State(state): State<AppState>,
    client: ScimClient,
    Path(id): Path<String>,
) -> Result<StatusCode, ScimError> {
    let mut transaction = state.database.begin().await?;
    let user = users::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(|| ScimError::not_found("no such user"))?;
    transaction
        .execute(
            &Sql::delete()
                .from_table(Users::Table)
                .and_where(Expr::col(Users::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    ensure_administrator_remains(&mut transaction).await?;
    audit::record(
        &mut transaction,
        &client.actor,
        "scim.user_delete",
        Target::user(&id),
        json!({ "email": user.email, "display_name": user.display_name }),
    )
    .await?;
    transaction.commit().await?;
    state.presence.recheck_access();
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_input_reads_names_and_entra_booleans() {
        let input = UserInput::from_json(&json!({
            "userName": "Ada@Example.com",
            "name": { "givenName": "Ada", "familyName": "Lovelace" },
            "active": "False",
            "externalId": "ext-1",
        }))
        .unwrap();
        assert_eq!(input.email, "ada@example.com");
        assert_eq!(input.display_name, "Ada Lovelace");
        assert_eq!(input.active, Some(false));
        assert_eq!(input.external_id.as_deref(), Some("ext-1"));

        let bare = UserInput::from_json(&json!({ "userName": "grace@example.com" })).unwrap();
        assert_eq!(bare.display_name, "grace");
        assert_eq!(bare.active, None);

        assert!(UserInput::from_json(&json!({ "userName": "not-an-email" })).is_err());
        assert!(UserInput::from_json(&json!({ "displayName": "No user name" })).is_err());
        assert!(UserInput::from_json(&json!({ "userName": "a@b.c", "active": 1 })).is_err());
    }
}
