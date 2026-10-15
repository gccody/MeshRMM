//! Companies: named groups devices belong to, such as an MSP's customers.
//! A device belongs to at most one company.
use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Func, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::double_option;
use crate::{
    agents::{self, parse_id},
    audit::{self, Target},
    auth::Authorized,
    db::{
        Executor,
        tables::{Agents, Companies},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    users::new_id,
};

const MAX_NAME_LENGTH: usize = 120;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Company {
    id: String,
    name: String,
    created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct CompanyView {
    #[serde(flatten)]
    company: Company,
    device_count: i64,
}

/// How many active devices each company has.
async fn device_counts(executor: &mut impl Executor) -> Result<HashMap<String, i64>, ApiError> {
    let counts: Vec<(String, i64)> = executor
        .fetch_all(
            &Query::select()
                .column(Agents::CompanyId)
                .expr(Func::count(Expr::col(Agents::Id)))
                .from(Agents::Table)
                .and_where(Expr::col(Agents::CompanyId).is_not_null())
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .group_by_col(Agents::CompanyId)
                .to_owned(),
        )
        .await?;
    Ok(counts.into_iter().collect())
}

fn company_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([Companies::Id, Companies::Name, Companies::CreatedAt])
        .from(Companies::Table)
        .to_owned()
}

async fn find(executor: &mut impl Executor, id: &str) -> Result<Company, ApiError> {
    executor
        .fetch_optional(
            &company_select()
                .and_where(Expr::col(Companies::Id).eq(id))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("no such company"))
}

/// `GET /v1/companies`: every company, by name, with its device count.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Vec<CompanyView>>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let mut database = &state.database;
    let counts = device_counts(&mut database).await?;
    let mut companies: Vec<Company> = database.fetch_all(&company_select()).await?;
    companies.sort_by_cached_key(|company| (company.name.to_lowercase(), company.id.clone()));
    Ok(Json(
        companies
            .into_iter()
            .map(|company| CompanyView {
                device_count: counts.get(&company.id).copied().unwrap_or_default(),
                company,
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
            "the company name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
        )));
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
    let names: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([Companies::Id, Companies::Name])
                .from(Companies::Table)
                .to_owned(),
        )
        .await?;
    let wanted = name.to_lowercase();
    if names
        .iter()
        .any(|(id, existing)| Some(id.as_str()) != except_id && existing.to_lowercase() == wanted)
    {
        return Err(ApiError::conflict(
            "a company with this name already exists",
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanyRequest {
    name: String,
}

/// `POST /v1/companies`
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<CompanyRequest>,
) -> Result<(StatusCode, Json<CompanyView>), ApiError> {
    actor.require(Permission::CompaniesManage)?;
    let name = validate_name(&request.name)?;
    let mut transaction = state.database.begin().await?;
    ensure_name_free(&mut transaction, &name, None).await?;
    let id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(Companies::Table)
                .columns([
                    Companies::Id,
                    Companies::Name,
                    Companies::CreatedAt,
                    Companies::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    name.as_str().into(),
                    now.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "company.create",
        Target::company(&id),
        json!({ "name": name }),
    )
    .await?;
    transaction.commit().await?;
    state.presence.refresh_company(&id).await;
    Ok((
        StatusCode::CREATED,
        Json(CompanyView {
            company: Company {
                id,
                name,
                created_at: now,
            },
            device_count: 0,
        }),
    ))
}

/// `PATCH /v1/companies/{id}`: renames the company.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<CompanyRequest>,
) -> Result<Json<CompanyView>, ApiError> {
    actor.require(Permission::CompaniesManage)?;
    let id = parse_id(&id, "company")?;
    let name = validate_name(&request.name)?;
    let mut transaction = state.database.begin().await?;
    let mut company = find(&mut transaction, &id).await?;
    ensure_name_free(&mut transaction, &name, Some(&id)).await?;
    transaction
        .execute(
            &Query::update()
                .table(Companies::Table)
                .value(Companies::Name, name.as_str())
                .value(Companies::UpdatedAt, now_ms())
                .and_where(Expr::col(Companies::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "company.rename",
        Target::company(&id),
        json!({ "name": name, "previous_name": company.name }),
    )
    .await?;
    let device_count = device_counts(&mut transaction)
        .await?
        .get(&id)
        .copied()
        .unwrap_or_default();
    transaction.commit().await?;
    state.presence.refresh_company(&id).await;
    company.name = name;
    Ok(Json(CompanyView {
        company,
        device_count,
    }))
}

/// `DELETE /v1/companies/{id}`: deletes the company. Its devices stay
/// enrolled and belong to no company.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::CompaniesManage)?;
    let id = parse_id(&id, "company")?;
    let mut transaction = state.database.begin().await?;
    let company = find(&mut transaction, &id).await?;
    let devices: Vec<(String,)> = transaction
        .fetch_all(
            &agents::active()
                .and_where(Expr::col(Agents::CompanyId).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    // The foreign key would clear these too; doing it here also marks the
    // devices changed.
    transaction
        .execute(
            &Query::update()
                .table(Agents::Table)
                .value(Agents::CompanyId, Option::<String>::None)
                .value(Agents::UpdatedAt, now_ms())
                .and_where(Expr::col(Agents::CompanyId).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(Companies::Table)
                .and_where(Expr::col(Companies::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "company.delete",
        Target::company(&id),
        json!({ "name": company.name, "device_count": devices.len() }),
    )
    .await?;
    transaction.commit().await?;
    for (device_id,) in &devices {
        state.presence.refresh(device_id).await;
    }
    state.presence.refresh_company(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCompanyRequest {
    /// The company to move the device to; `null` for none.
    #[serde(deserialize_with = "double_option")]
    company_id: Option<Option<String>>,
}

/// `PUT /v1/agents/{id}/company`: moves the device to a company, or out of
/// its company with `null`.
pub async fn assign(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<DeviceCompanyRequest>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::CompaniesManage)?;
    let id = parse_id(&id, "device")?;
    let company_id = request
        .company_id
        .flatten()
        .map(|company_id| parse_id(&company_id, "company"))
        .transpose()?;
    let mut transaction = state.database.begin().await?;
    let (name, previous): (String, Option<String>) = transaction
        .fetch_optional(
            &Query::select()
                .columns([Agents::Name, Agents::CompanyId])
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(id.as_str()))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    let company = match &company_id {
        Some(company_id) => Some(find(&mut transaction, company_id).await?),
        None => None,
    };
    if previous == company_id {
        return Ok(StatusCode::NO_CONTENT);
    }
    transaction
        .execute(
            &Query::update()
                .table(Agents::Table)
                .value(Agents::CompanyId, company_id.clone())
                .value(Agents::UpdatedAt, now_ms())
                .and_where(Expr::col(Agents::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "agent.move",
        Target::device(&id),
        json!({
            "name": name,
            "from_company_id": previous,
            "company_id": company_id,
            "company_name": company.map(|company| company.name),
        }),
    )
    .await?;
    transaction.commit().await?;
    state.presence.refresh(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Checks that an installer's company exists.
pub async fn ensure_exists(executor: &mut impl Executor, id: &str) -> Result<String, ApiError> {
    let id = parse_id(id, "company")?;
    find(executor, &id).await?;
    Ok(id)
}
