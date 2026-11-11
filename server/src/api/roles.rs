//! Custom roles: named sets of permissions an administrator defines, and
//! the companies they reach.
use std::collections::{BTreeSet, HashMap};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Func, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ensure_grants_within, require_any};
use crate::{
    access::{CompanySet, instance_permissions_on_limited_role},
    agents::parse_id,
    audit::{self, Target},
    auth::Authorized,
    db::{
        self, Executor,
        tables::{Companies, EffectiveUserRoles, RoleCompanies, RolePermissions, Roles},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::{self, Permission, Permissions, Reach, Role},
    time::now_ms,
    users::new_id,
};

const MAX_NAME_LENGTH: usize = 80;
const MAX_DESCRIPTION_LENGTH: usize = 500;

#[derive(Debug, Serialize)]
pub struct PermissionView {
    name: Permission,
    description: &'static str,
    /// `company` when a role limited to some companies can grant it on
    /// their devices; `instance` when only roles for every company can.
    reach: Reach,
}

/// `GET /v1/permissions`: every permission a role can grant.
pub async fn permissions(_actor: Authorized) -> Json<Vec<PermissionView>> {
    Json(
        Permission::ALL
            .iter()
            .map(|permission| PermissionView {
                name: *permission,
                description: permission.description(),
                reach: permission.reach(),
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
                .column(EffectiveUserRoles::RoleId)
                .expr(Func::count(Expr::col(EffectiveUserRoles::UserId)))
                .from(EffectiveUserRoles::Table)
                .group_by_col(EffectiveUserRoles::RoleId)
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

/// The companies a request chose: every company, or the companies named,
/// which must exist and of which there must be at least one.
async fn chosen_companies(
    executor: &mut impl Executor,
    all_companies: bool,
    company_ids: Vec<String>,
) -> Result<CompanySet, ApiError> {
    if all_companies {
        if !company_ids.is_empty() {
            return Err(ApiError::bad_request(
                "a role for every company can't also name companies",
            )
            .with_code("invalid_role_companies"));
        }
        return Ok(CompanySet::All);
    }
    let ids = company_ids
        .iter()
        .map(|id| parse_id(id, "company"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if ids.is_empty() {
        return Err(
            ApiError::bad_request("choose at least one company, or every company")
                .with_code("invalid_role_companies"),
        );
    }
    let mut found = 0;
    let ids_list = ids.iter().cloned().collect::<Vec<_>>();
    for chunk in ids_list.chunks(500) {
        let rows: Vec<(String,)> = executor
            .fetch_all(
                &Query::select()
                    .column(Companies::Id)
                    .from(Companies::Table)
                    .and_where(Expr::col(Companies::Id).is_in(chunk.iter().cloned()))
                    .to_owned(),
            )
            .await?;
        found += rows.len();
    }
    if found != ids.len() {
        return Err(ApiError::not_found("a chosen company no longer exists"));
    }
    Ok(CompanySet::Only(ids))
}

/// Refuses permissions a role can't hold on `companies`, and permissions
/// the actor doesn't hold on all of them.
fn ensure_grantable(
    actor: &Authorized,
    permissions: &Permissions,
    companies: &CompanySet,
    what: &str,
) -> Result<(), ApiError> {
    let instance = instance_permissions_on_limited_role(permissions, companies);
    if !instance.is_empty() {
        let names = instance
            .iter()
            .map(|permission| permission.as_str())
            .collect::<Vec<_>>();
        return Err(ApiError::bad_request(format!(
            "a role limited to some companies can't grant permissions that act on the whole instance: {}",
            names.join(", ")
        ))
        .with_code("instance_permission_on_limited_role"));
    }
    ensure_grants_within(actor, permissions, companies, what)
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

async fn set_companies(
    executor: &mut impl Executor,
    role_id: &str,
    companies: &CompanySet,
) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(Roles::Table)
                .value(Roles::AllCompanies, companies.is_all())
                .and_where(Expr::col(Roles::Id).eq(role_id))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(RoleCompanies::Table)
                .and_where(Expr::col(RoleCompanies::RoleId).eq(role_id))
                .to_owned(),
        )
        .await?;
    let CompanySet::Only(ids) = companies else {
        return Ok(());
    };
    let ids = ids.iter().collect::<Vec<_>>();
    for chunk in ids.chunks(400) {
        let mut insert = Query::insert();
        insert
            .into_table(RoleCompanies::Table)
            .columns([RoleCompanies::RoleId, RoleCompanies::CompanyId]);
        for id in chunk {
            insert.values_panic([role_id.into(), id.as_str().into()]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

/// The companies as the API and the audit log show them.
fn companies_json(companies: &CompanySet) -> serde_json::Value {
    match companies {
        CompanySet::All => json!({ "all_companies": true, "company_ids": [] }),
        CompanySet::Only(ids) => json!({ "all_companies": false, "company_ids": ids }),
    }
}

fn apply_companies(role: &mut Role, companies: CompanySet) {
    match companies {
        CompanySet::All => {
            role.all_companies = true;
            role.company_ids = BTreeSet::new();
        }
        CompanySet::Only(ids) => {
            role.all_companies = false;
            role.company_ids = ids;
        }
    }
}

fn every_company() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRole {
    name: String,
    #[serde(default)]
    description: String,
    permissions: Permissions,
    /// Whether the role reaches every company; otherwise only
    /// `company_ids`.
    #[serde(default = "every_company")]
    all_companies: bool,
    #[serde(default)]
    company_ids: Vec<String>,
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
    let mut transaction = state.database.begin().await?;
    let companies =
        chosen_companies(&mut transaction, request.all_companies, request.company_ids).await?;
    ensure_grantable(&actor, &request.permissions, &companies, "create this role")?;
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
    set_companies(&mut transaction, &id, &companies).await?;
    let mut details = json!({ "name": name, "permissions": request.permissions });
    if let (Some(details), serde_json::Value::Object(scope)) =
        (details.as_object_mut(), companies_json(&companies))
    {
        details.extend(scope);
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "role.create",
        Target::role(&id),
        details,
    )
    .await?;
    transaction.commit().await?;
    let mut role = Role {
        id,
        name,
        description,
        builtin: None,
        permissions: request.permissions,
        all_companies: true,
        company_ids: BTreeSet::new(),
    };
    apply_companies(&mut role, companies);
    Ok((
        StatusCode::CREATED,
        Json(RoleView {
            role,
            member_count: 0,
        }),
    ))
}

/// Loads a role the actor may change: not the Administrator role, and none
/// granting permissions the actor lacks on its companies.
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
            "the Administrator role always has every permission in every company and can't be changed",
        ));
    }
    ensure_grants_within(
        actor,
        &role.permissions,
        &role.companies(),
        "change this role",
    )?;
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
    /// With `company_ids`, the companies the role reaches; both are
    /// replaced together.
    #[serde(default)]
    all_companies: Option<bool>,
    #[serde(default)]
    company_ids: Option<Vec<String>>,
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
    let previous_companies = role.companies();
    let companies = match (request.all_companies, request.company_ids) {
        (None, None) => previous_companies.clone(),
        (all_companies, company_ids) => {
            let company_ids = company_ids.unwrap_or_default();
            // Naming companies alone limits the role to them.
            let all_companies = all_companies.unwrap_or(company_ids.is_empty());
            chosen_companies(&mut transaction, all_companies, company_ids).await?
        }
    };
    let permissions = request
        .permissions
        .clone()
        .unwrap_or_else(|| role.permissions.clone());
    if request.permissions.is_some() || companies != previous_companies {
        ensure_grantable(&actor, &permissions, &companies, "grant these permissions")?;
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
    if request.permissions.is_some() {
        set_permissions(&mut transaction, &role.id, &permissions).await?;
        changes.insert("permissions".into(), json!(permissions));
        role.permissions = permissions;
    }
    if companies != previous_companies {
        set_companies(&mut transaction, &role.id, &companies).await?;
        if let serde_json::Value::Object(scope) = companies_json(&companies) {
            changes.extend(scope);
        }
        changes.insert(
            "previous_companies".into(),
            companies_json(&previous_companies),
        );
        apply_companies(&mut role, companies);
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
