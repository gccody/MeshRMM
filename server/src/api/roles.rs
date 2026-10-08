//! Custom roles: named sets of permissions an administrator defines.
use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Func, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ensure_within, require_any};
use crate::{
    audit::{self, Target},
    auth::Authorized,
    db::{
        self, Executor,
        tables::{RolePermissions, Roles, UserRoles},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::{self, Permission, Permissions, Role},
    time::now_ms,
    users::new_id,
};

const MAX_NAME_LENGTH: usize = 80;
const MAX_DESCRIPTION_LENGTH: usize = 500;

#[derive(Debug, Serialize)]
pub struct PermissionView {
    name: Permission,
    description: &'static str,
}

/// `GET /v1/permissions`: every permission a role can grant.
pub async fn permissions(_actor: Authorized) -> Json<Vec<PermissionView>> {
    Json(
        Permission::ALL
            .iter()
            .map(|permission| PermissionView {
                name: *permission,
                description: permission.description(),
            })
            .collect(),
    )
}

#[derive(Debug, Serialize)]
pub struct RoleView {
    #[serde(flatten)]
    role: Role,
    member_count: i64,
}

async fn member_counts(executor: &mut impl Executor) -> db::Result<HashMap<String, i64>> {
    let counts: Vec<(String, i64)> = executor
        .fetch_all(
            &Query::select()
                .column(UserRoles::RoleId)
                .expr(Func::count(Expr::col(UserRoles::UserId)))
                .from(UserRoles::Table)
                .group_by_col(UserRoles::RoleId)
                .to_owned(),
        )
        .await?;
    Ok(counts.into_iter().collect())
}

/// `GET /v1/roles`: every role, for managing roles or assigning them.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Vec<RoleView>>, ApiError> {
    require_any(&actor, &[Permission::RolesManage, Permission::UsersManage])?;
    let mut database = &state.database;
    let counts = member_counts(&mut database).await?;
    Ok(Json(
        rbac::load_roles(&mut database, None)
            .await?
            .into_iter()
            .map(|role| RoleView {
                member_count: counts.get(&role.id).copied().unwrap_or_default(),
                role,
            })
            .collect(),
    ))
}

fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the role name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
        )));
    }
    Ok(name.to_owned())
}

fn validate_description(raw: &str) -> Result<String, ApiError> {
    let description = raw.trim();
    if description.chars().count() > MAX_DESCRIPTION_LENGTH
        || description.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(ApiError::bad_request(format!(
            "the description must be at most {MAX_DESCRIPTION_LENGTH} characters"
        )));
    }
    Ok(description.to_owned())
}

async fn ensure_name_free(
    executor: &mut impl Executor,
    name: &str,
    except_id: Option<&str>,
) -> Result<(), ApiError> {
    // Compared here rather than with SQL lower(), which SQLite applies to
    // ASCII letters only.
    let names: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([Roles::Id, Roles::Name])
                .from(Roles::Table)
                .to_owned(),
        )
        .await?;
    let wanted = name.to_lowercase();
    if names
        .iter()
        .any(|(id, existing)| Some(id.as_str()) != except_id && existing.to_lowercase() == wanted)
    {
        return Err(ApiError::conflict("a role with this name already exists"));
    }
    Ok(())
}

async fn set_permissions(
    executor: &mut impl Executor,
    role_id: &str,
    permissions: &Permissions,
) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(RolePermissions::Table)
                .and_where(Expr::col(RolePermissions::RoleId).eq(role_id))
                .to_owned(),
        )
        .await?;
    if permissions.is_empty() {
        return Ok(());
    }
    let mut insert = Query::insert();
    insert
        .into_table(RolePermissions::Table)
        .columns([RolePermissions::RoleId, RolePermissions::Permission]);
    for permission in permissions {
        insert.values_panic([role_id.into(), permission.as_str().into()]);
    }
    executor.execute(&insert).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRole {
    name: String,
    #[serde(default)]
    description: String,
    permissions: Permissions,
}

/// `POST /v1/roles`
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<NewRole>,
) -> Result<(StatusCode, Json<RoleView>), ApiError> {
    actor.require(Permission::RolesManage)?;
    let name = validate_name(&request.name)?;
    let description = validate_description(&request.description)?;
    ensure_within(&actor, &request.permissions, "create this role")?;
    let mut transaction = state.database.begin().await?;
    ensure_name_free(&mut transaction, &name, None).await?;
    let id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(Roles::Table)
                .columns([
                    Roles::Id,
                    Roles::Name,
                    Roles::Description,
                    Roles::CreatedAt,
                    Roles::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    name.as_str().into(),
                    description.as_str().into(),
                    now.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    set_permissions(&mut transaction, &id, &request.permissions).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "role.create",
        Target::role(&id),
        json!({ "name": name, "permissions": request.permissions }),
    )
    .await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(RoleView {
            role: Role {
                id,
                name,
                description,
                builtin: None,
                permissions: request.permissions,
            },
            member_count: 0,
        }),
    ))
}

/// Loads a role the actor may change: not the Administrator role, and none
/// granting permissions the actor lacks.
async fn editable(
    executor: &mut impl Executor,
    actor: &Authorized,
    id: &str,
) -> Result<Role, ApiError> {
    actor.require(Permission::RolesManage)?;
    let role = rbac::load_roles(executor, Some(&[id.to_owned()]))
        .await?
        .pop()
        .ok_or_else(|| ApiError::not_found("no such role"))?;
    if role.is_administrator() {
        return Err(ApiError::forbidden(
            "the Administrator role always has every permission and can't be changed",
        ));
    }
    ensure_within(actor, &role.permissions, "change this role")?;
    Ok(role)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleUpdate {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    permissions: Option<Permissions>,
}

/// `PATCH /v1/roles/{id}`
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<RoleUpdate>,
) -> Result<Json<RoleView>, ApiError> {
    let mut transaction = state.database.begin().await?;
    let mut role = editable(&mut transaction, &actor, &id).await?;
    let mut changes = serde_json::Map::new();
    if let Some(name) = &request.name {
        role.name = validate_name(name)?;
        ensure_name_free(&mut transaction, &role.name, Some(&role.id)).await?;
        changes.insert("name".into(), json!(role.name));
    }
    if let Some(description) = &request.description {
        role.description = validate_description(description)?;
        changes.insert("description".into(), json!(role.description));
    }
    transaction
        .execute(
            &Query::update()
                .table(Roles::Table)
                .values([
                    (Roles::Name, role.name.as_str().into()),
                    (Roles::Description, role.description.as_str().into()),
                    (Roles::UpdatedAt, now_ms().into()),
                ])
                .and_where(Expr::col(Roles::Id).eq(role.id.as_str()))
                .to_owned(),
        )
        .await?;
    if let Some(permissions) = request.permissions {
        ensure_within(&actor, &permissions, "grant these permissions")?;
        set_permissions(&mut transaction, &role.id, &permissions).await?;
        changes.insert("permissions".into(), json!(permissions));
        role.permissions = permissions;
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "role.update",
        Target::role(&role.id),
        changes.into(),
    )
    .await?;
    let member_count = member_counts(&mut transaction)
        .await?
        .get(&role.id)
        .copied()
        .unwrap_or_default();
    transaction.commit().await?;
    Ok(Json(RoleView { role, member_count }))
}

/// `DELETE /v1/roles/{id}`: deletes a custom role; its members lose it.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let role = editable(&mut transaction, &actor, &id).await?;
    if role.builtin.is_some() {
        return Err(ApiError::forbidden("built-in roles can't be deleted"));
    }
    transaction
        .execute(
            &Query::delete()
                .from_table(Roles::Table)
                .and_where(Expr::col(Roles::Id).eq(role.id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "role.delete",
        Target::role(&role.id),
        json!({ "name": role.name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
