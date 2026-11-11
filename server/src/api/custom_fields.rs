//! Custom fields: their definitions, devices' and companies' values, the
//! device filter, and the values toolbox scripts write through their
//! Agent. See docs/custom-fields.md.
//!
//! Every route about one device or company finds it through
//! [`authorized_device`] or [`authorized_company`], which check the
//! caller's permission first; that is the one place to narrow which
//! devices and companies a caller reaches. Definitions are the same for
//! everyone.
use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    body::Body,
    extract::{Path, Query as QueryString, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
};
use meshrmm_protocol_types::{MAX_SCRIPT_FIELD_VALUE_BYTES, ScriptFieldWrite, ScriptRunStatus};
use sea_query::{Expr, ExprTrait, Func, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agents::{self, parse_id},
    audit::{self, Actor, Target},
    auth::Authorized,
    custom_fields::{
        self, AppliesTo, Author, ChangedBy, Definition, DeviceRef, DeviceValues, FieldType,
        MAX_DEVICES_PER_CHANGE, MAX_FIELDS, Operator, Owner, ScriptAccess, Source, Stored,
    },
    db::{
        Executor,
        tables::{Agents, CustomFields, FieldValues, ScriptRuns},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    users::new_id,
};

fn field_target(id: &str) -> Target<'_> {
    Target {
        kind: "custom_field",
        id,
    }
}

fn field_not_found() -> ApiError {
    ApiError::not_found("no such custom field")
}

/// The device a request is about, once the caller holds `permission`.
async fn authorized_device(
    executor: &mut impl Executor,
    actor: &Authorized,
    device_id: &str,
    permission: Permission,
) -> Result<DeviceRef, ApiError> {
    actor.require(permission)?;
    let device_id = parse_id(device_id, "device")?;
    custom_fields::device(executor, &device_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))
}

/// The company a request is about, with its name, once the caller holds
/// `permission`.
async fn authorized_company(
    executor: &mut impl Executor,
    actor: &Authorized,
    company_id: &str,
    permission: Permission,
) -> Result<(String, String), ApiError> {
    actor.require(permission)?;
    let company_id = parse_id(company_id, "company")?;
    let name = custom_fields::company_name(executor, &company_id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such company"))?;
    Ok((company_id, name))
}

#[derive(Debug, Serialize)]
pub struct Definitions {
    fields: Vec<Definition>,
}

/// `GET /v1/custom-fields`: every field, in display order.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Definitions>, ApiError> {
    actor.require(Permission::DevicesView)?;
    Ok(Json(Definitions {
        fields: custom_fields::definitions(&mut &state.database).await?,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewField {
    key: String,
    label: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "type")]
    field_type: FieldType,
    #[serde(default)]
    options: Vec<String>,
    applies_to: AppliesTo,
    #[serde(default = "no_script_access")]
    script_access: ScriptAccess,
}

fn no_script_access() -> ScriptAccess {
    ScriptAccess::None
}

/// `POST /v1/custom-fields`: a new field, shown after the others.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<NewField>,
) -> Result<(StatusCode, Json<Definition>), ApiError> {
    actor.require(Permission::CustomFieldsManage)?;
    let key = request.key.trim().to_owned();
    custom_fields::check_key(&key)?;
    let input = custom_fields::check_definition(
        request.field_type,
        &request.label,
        &request.description,
        request.options,
        request.applies_to,
        request.script_access,
    )?;
    let mut transaction = state.database.begin().await?;
    let existing = custom_fields::definitions(&mut transaction).await?;
    if existing.len() >= MAX_FIELDS {
        return Err(
            ApiError::conflict(format!("there are already {MAX_FIELDS} custom fields"))
                .with_code("too_many_fields"),
        );
    }
    if existing.iter().any(|field| field.key == key) {
        return Err(
            ApiError::conflict(format!("another field has the key “{key}”")).with_code("key_taken"),
        );
    }
    let position = existing
        .iter()
        .map(|field| field.position)
        .max()
        .map_or(0, |last| last + 1);
    let id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(CustomFields::Table)
                .columns([
                    CustomFields::Id,
                    CustomFields::FieldKey,
                    CustomFields::Label,
                    CustomFields::Description,
                    CustomFields::FieldType,
                    CustomFields::Options,
                    CustomFields::AppliesToDevices,
                    CustomFields::AppliesToCompanies,
                    CustomFields::Position,
                    CustomFields::ScriptAccess,
                    CustomFields::CreatedAt,
                    CustomFields::UpdatedAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    key.as_str().into(),
                    input.label.as_str().into(),
                    input.description.as_str().into(),
                    request.field_type.as_str().into(),
                    serde_json::to_string(&input.options)
                        .map_err(|_| ApiError::internal())?
                        .into(),
                    input.applies_to.devices().into(),
                    input.applies_to.companies().into(),
                    position.into(),
                    input.script_access.as_str().into(),
                    now.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.create",
        field_target(&id),
        json!({
            "key": key,
            "label": input.label,
            "type": request.field_type.as_str(),
            "applies_to": input.applies_to,
            "script_access": input.script_access.as_str(),
        }),
    )
    .await?;
    let created = custom_fields::definition(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldChange {
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    options: Vec<String>,
    applies_to: AppliesTo,
    script_access: ScriptAccess,
}

/// `PUT /v1/custom-fields/{id}`: changes a field's label, description,
/// options, where it applies and what scripts may do with it. Its key and
/// type stay. Values a change leaves without a place go: those of devices
/// or companies the field no longer applies to, and options no longer
/// offered.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<FieldChange>,
) -> Result<Json<Definition>, ApiError> {
    actor.require(Permission::CustomFieldsManage)?;
    let id = parse_id(&id, "custom field")?;
    let mut transaction = state.database.begin().await?;
    let field = custom_fields::definition(&mut transaction, &id)
        .await?
        .filter(|field| field.id == id)
        .ok_or_else(field_not_found)?;
    let input = custom_fields::check_definition(
        field.field_type,
        &request.label,
        &request.description,
        request.options,
        request.applies_to,
        request.script_access,
    )?;
    let now = now_ms();
    transaction
        .execute(
            &Query::update()
                .table(CustomFields::Table)
                .value(CustomFields::Label, input.label.as_str())
                .value(CustomFields::Description, input.description.as_str())
                .value(
                    CustomFields::Options,
                    serde_json::to_string(&input.options).map_err(|_| ApiError::internal())?,
                )
                .value(CustomFields::AppliesToDevices, input.applies_to.devices())
                .value(
                    CustomFields::AppliesToCompanies,
                    input.applies_to.companies(),
                )
                .value(CustomFields::ScriptAccess, input.script_access.as_str())
                .value(CustomFields::UpdatedAt, now)
                .and_where(Expr::col(CustomFields::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    let mut cleared = 0;
    for (table, kept) in [
        (FieldValues::DeviceTable, input.applies_to.devices()),
        (FieldValues::CompanyTable, input.applies_to.companies()),
    ] {
        if !kept {
            cleared += transaction
                .execute(
                    &Query::delete()
                        .from_table(table)
                        .and_where(Expr::col(FieldValues::FieldId).eq(id.as_str()))
                        .to_owned(),
                )
                .await?;
        }
    }
    let changed = custom_fields::definition(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    if field.field_type.has_options() && changed.options != field.options {
        cleared += drop_removed_options(&mut transaction, &changed).await?;
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.update",
        field_target(&id),
        json!({
            "key": field.key,
            "label": changed.label,
            "previous_label": field.label,
            "applies_to": changed.applies_to,
            "script_access": changed.script_access.as_str(),
            "previous_script_access": field.script_access.as_str(),
            "options": changed.options,
            "values_cleared": cleared,
        }),
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(changed))
}

/// Takes options the field no longer offers out of its values, removing a
/// value that held nothing else. Returns how many values were removed.
async fn drop_removed_options(
    executor: &mut impl Executor,
    field: &Definition,
) -> Result<u64, ApiError> {
    let mut removed = 0;
    for (table, column, companies) in [
        (FieldValues::DeviceTable, FieldValues::DeviceId, false),
        (FieldValues::CompanyTable, FieldValues::CompanyId, true),
    ] {
        for stored in custom_fields::stored_of_field(executor, &field.id, companies).await? {
            let Some(value) = stored.value.as_deref() else {
                continue;
            };
            let kept = value
                .split('\n')
                .filter(|option| field.options.iter().any(|offered| offered == option))
                .collect::<Vec<_>>();
            if kept.len() == value.split('\n').count() {
                continue;
            }
            let row = Expr::col(column)
                .eq(stored.owner_id.as_str())
                .and(Expr::col(FieldValues::FieldId).eq(field.id.as_str()));
            if kept.is_empty() {
                removed += executor
                    .execute(&Query::delete().from_table(table).cond_where(row).to_owned())
                    .await?;
            } else {
                executor
                    .execute(
                        &Query::update()
                            .table(table)
                            .value(FieldValues::Value, kept.join("\n"))
                            .cond_where(row)
                            .to_owned(),
                    )
                    .await?;
            }
        }
    }
    Ok(removed)
}

/// `DELETE /v1/custom-fields/{id}`: removes a field and every value of it.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::CustomFieldsManage)?;
    let id = parse_id(&id, "custom field")?;
    let mut transaction = state.database.begin().await?;
    let field = custom_fields::definition(&mut transaction, &id)
        .await?
        .filter(|field| field.id == id)
        .ok_or_else(field_not_found)?;
    let mut values = 0_i64;
    for table in [FieldValues::DeviceTable, FieldValues::CompanyTable] {
        let (count,): (i64,) = transaction
            .fetch_one(
                &Query::select()
                    .expr(Func::count(Expr::col(FieldValues::FieldId)))
                    .from(table)
                    .and_where(Expr::col(FieldValues::FieldId).eq(id.as_str()))
                    .to_owned(),
            )
            .await?;
        values += count;
    }
    transaction
        .execute(
            &Query::delete()
                .from_table(CustomFields::Table)
                .and_where(Expr::col(CustomFields::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.delete",
        field_target(&id),
        json!({ "key": field.key, "label": field.label, "values": values }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Order {
    ids: Vec<String>,
}

/// `PUT /v1/custom-fields/order`: shows the fields in the order given,
/// which names every field once.
pub async fn reorder(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<Order>,
) -> Result<Json<Definitions>, ApiError> {
    actor.require(Permission::CustomFieldsManage)?;
    let mut transaction = state.database.begin().await?;
    let fields = custom_fields::definitions(&mut transaction).await?;
    let known = fields
        .iter()
        .map(|field| field.id.as_str())
        .collect::<HashSet<_>>();
    let given = request
        .ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if given.len() != request.ids.len() || given != known {
        return Err(
            ApiError::bad_request("name every custom field once, in order")
                .with_code("invalid_order"),
        );
    }
    let now = now_ms();
    for (position, id) in request.ids.iter().enumerate() {
        transaction
            .execute(
                &Query::update()
                    .table(CustomFields::Table)
                    .value(CustomFields::Position, position as i64)
                    .value(CustomFields::UpdatedAt, now)
                    .and_where(Expr::col(CustomFields::Id).eq(id.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.reorder",
        Target {
            kind: "custom_field",
            id: "",
        },
        json!({ "keys": request.ids.iter().filter_map(|id| fields.iter().find(|field| &field.id == id)).map(|field| field.key.as_str()).collect::<Vec<_>>() }),
    )
    .await?;
    let reordered = custom_fields::definitions(&mut transaction).await?;
    transaction.commit().await?;
    Ok(Json(Definitions { fields: reordered }))
}

#[derive(Debug, Clone, Serialize)]
struct CompanyRef {
    id: String,
    name: String,
}

/// One field's value as a device's or company's page shows it.
#[derive(Debug, Serialize)]
pub struct FieldValue {
    field: Definition,
    /// The value as JSON of the field's type; `null` when there is none,
    /// and always for a secret field, which is revealed on its own.
    value: Value,
    /// Whether there is a value, a secret one included.
    set: bool,
    /// `device` for the device's own value, `company` for one it takes from
    /// its company; `null` without a value. A company's own values say
    /// `company`.
    source: Option<Source>,
    /// The company an inherited value comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    inherited_from: Option<CompanyRef>,
    updated_at: Option<i64>,
    updated_by: Option<Author>,
}

impl FieldValue {
    fn of(
        field: Definition,
        stored: Option<&Stored>,
        source: Option<Source>,
        inherited_from: Option<CompanyRef>,
    ) -> Self {
        let value = match stored.and_then(|stored| stored.value.as_deref()) {
            Some(value) if field.field_type != FieldType::Secret => {
                custom_fields::to_json(&field, value)
            }
            _ => Value::Null,
        };
        Self {
            value,
            set: stored.is_some(),
            source: stored.and(source),
            inherited_from: stored.and(inherited_from),
            updated_at: stored.map(|stored| stored.updated_at),
            updated_by: stored.map(Stored::author),
            field,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ValueList {
    fields: Vec<FieldValue>,
}

async fn device_view(
    executor: &mut impl Executor,
    device: &DeviceRef,
    only: Option<&str>,
) -> Result<Vec<FieldValue>, ApiError> {
    let fields = custom_fields::definitions(executor).await?;
    let values = DeviceValues::load(executor, device).await?;
    let company = match &device.company_id {
        Some(id) => custom_fields::company_name(executor, id)
            .await?
            .map(|name| CompanyRef {
                id: id.clone(),
                name,
            }),
        None => None,
    };
    Ok(fields
        .into_iter()
        .filter(|field| field.applies_to.devices())
        .filter(|field| only.is_none_or(|id| field.id == id))
        .map(|field| {
            let effective = values.effective(&field);
            let (stored, source) = (effective.stored.cloned(), effective.source);
            let inherited = (source == Some(Source::Company))
                .then(|| company.clone())
                .flatten();
            FieldValue::of(field, stored.as_ref(), source, inherited)
        })
        .collect())
}

async fn company_view(
    executor: &mut impl Executor,
    company_id: &str,
    only: Option<&str>,
) -> Result<Vec<FieldValue>, ApiError> {
    let fields = custom_fields::definitions(executor).await?;
    let values = custom_fields::stored(executor, Owner::Company(company_id)).await?;
    Ok(fields
        .into_iter()
        .filter(|field| field.applies_to.companies())
        .filter(|field| only.is_none_or(|id| field.id == id))
        .map(|field| {
            let stored = values.get(&field.id);
            FieldValue::of(field, stored, Some(Source::Company), None)
        })
        .collect())
}

/// `GET /v1/agents/{id}/custom-fields`: the device's value of every field
/// that applies to devices, its own or its company's.
pub async fn device_values(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ValueList>, ApiError> {
    let mut database = &state.database;
    let device = authorized_device(&mut database, &actor, &id, Permission::DevicesView).await?;
    Ok(Json(ValueList {
        fields: device_view(&mut database, &device, None).await?,
    }))
}

/// `GET /v1/companies/{id}/custom-fields`: the company's value of every
/// field that applies to companies.
pub async fn company_values(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ValueList>, ApiError> {
    let mut database = &state.database;
    let (company_id, _) =
        authorized_company(&mut database, &actor, &id, Permission::DevicesView).await?;
    Ok(Json(ValueList {
        fields: company_view(&mut database, &company_id, None).await?,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueChange {
    /// JSON of the field's type, or `null` to clear it.
    value: Value,
}

/// The field a value route names by ID or key, if it applies to `owner`s.
async fn field_for(
    executor: &mut impl Executor,
    field: &str,
    companies: bool,
) -> Result<Definition, ApiError> {
    custom_fields::definition(executor, field)
        .await?
        .filter(|field| {
            if companies {
                field.applies_to.companies()
            } else {
                field.applies_to.devices()
            }
        })
        .ok_or_else(field_not_found)
}

fn audit_change(
    field: &Definition,
    name: &str,
    value: Option<&str>,
    source: &str,
) -> serde_json::Value {
    json!({
        "name": name,
        "field_id": field.id,
        "key": field.key,
        "label": field.label,
        "value": custom_fields::audited(field, value),
        "cleared": value.is_none(),
        "source": source,
    })
}

/// `PUT /v1/agents/{id}/custom-fields/{field}`: sets or clears the device's
/// own value of a field, named by ID or key.
pub async fn set_device_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path((id, field)): Path<(String, String)>,
    JsonBody(request): JsonBody<ValueChange>,
) -> Result<Json<FieldValue>, ApiError> {
    let mut transaction = state.database.begin().await?;
    let device = authorized_device(&mut transaction, &actor, &id, Permission::DevicesEdit).await?;
    let field = field_for(&mut transaction, &field, false).await?;
    let value = custom_fields::from_json(&field, &request.value)?;
    custom_fields::store(
        &mut transaction,
        &state.instance_key,
        &field,
        Owner::Device(&device.id),
        value.as_deref(),
        ChangedBy::User(&actor.user.id),
        now_ms(),
    )
    .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.set",
        Target::device(&device.id),
        audit_change(&field, &device.name, value.as_deref(), "website"),
    )
    .await?;
    let view = device_view(&mut transaction, &device, Some(&field.id))
        .await?
        .pop()
        .ok_or_else(ApiError::internal)?;
    transaction.commit().await?;
    Ok(Json(view))
}

/// `PUT /v1/companies/{id}/custom-fields/{field}`: sets or clears the
/// company's value of a field, which its devices without their own take.
pub async fn set_company_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path((id, field)): Path<(String, String)>,
    JsonBody(request): JsonBody<ValueChange>,
) -> Result<Json<FieldValue>, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (company_id, name) =
        authorized_company(&mut transaction, &actor, &id, Permission::CompaniesManage).await?;
    let field = field_for(&mut transaction, &field, true).await?;
    let value = custom_fields::from_json(&field, &request.value)?;
    custom_fields::store(
        &mut transaction,
        &state.instance_key,
        &field,
        Owner::Company(&company_id),
        value.as_deref(),
        ChangedBy::User(&actor.user.id),
        now_ms(),
    )
    .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.set",
        Target::company(&company_id),
        audit_change(&field, &name, value.as_deref(), "website"),
    )
    .await?;
    let view = company_view(&mut transaction, &company_id, Some(&field.id))
        .await?
        .pop()
        .ok_or_else(ApiError::internal)?;
    transaction.commit().await?;
    Ok(Json(view))
}

#[derive(Debug, Serialize)]
pub struct Revealed {
    value: String,
    source: Source,
}

fn not_secret() -> ApiError {
    ApiError::bad_request("only secret fields are revealed; their value is in the list")
        .with_code("not_secret")
}

fn no_value() -> ApiError {
    ApiError::not_found("the field has no value here").with_code("no_value")
}

/// `POST /v1/agents/{id}/custom-fields/{field}/reveal`: a secret field's
/// value on the device, its own or its company's. Recorded in the audit
/// log, without the value.
pub async fn reveal_device_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path((id, field)): Path<(String, String)>,
) -> Result<Json<Revealed>, ApiError> {
    actor.require(Permission::CustomFieldsReveal)?;
    let mut transaction = state.database.begin().await?;
    let device = authorized_device(&mut transaction, &actor, &id, Permission::DevicesView).await?;
    let field = field_for(&mut transaction, &field, false).await?;
    if field.field_type != FieldType::Secret {
        return Err(not_secret());
    }
    let values = DeviceValues::load(&mut transaction, &device).await?;
    let effective = values.effective(&field);
    let (Some(stored), Some(source)) = (effective.stored, effective.source) else {
        return Err(no_value());
    };
    let owner = match source {
        Source::Device => Owner::Device(&device.id),
        Source::Company => Owner::Company(device.company_id.as_deref().unwrap_or_default()),
    };
    let value = stored.open(&state.instance_key, &field, owner)?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.reveal",
        Target::device(&device.id),
        json!({
            "name": device.name,
            "field_id": field.id,
            "key": field.key,
            "label": field.label,
            "source": source,
        }),
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(Revealed { value, source }))
}

/// `POST /v1/companies/{id}/custom-fields/{field}/reveal`: a secret
/// field's value on the company, recorded in the audit log.
pub async fn reveal_company_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path((id, field)): Path<(String, String)>,
) -> Result<Json<Revealed>, ApiError> {
    actor.require(Permission::CustomFieldsReveal)?;
    let mut transaction = state.database.begin().await?;
    let (company_id, name) =
        authorized_company(&mut transaction, &actor, &id, Permission::DevicesView).await?;
    let field = field_for(&mut transaction, &field, true).await?;
    if field.field_type != FieldType::Secret {
        return Err(not_secret());
    }
    let values = custom_fields::stored(&mut transaction, Owner::Company(&company_id)).await?;
    let stored = values.get(&field.id).ok_or_else(no_value)?;
    let value = stored.open(&state.instance_key, &field, Owner::Company(&company_id))?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "custom_field.reveal",
        Target::company(&company_id),
        json!({
            "name": name,
            "field_id": field.id,
            "key": field.key,
            "label": field.label,
            "source": Source::Company,
        }),
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(Revealed {
        value,
        source: Source::Company,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkChange {
    /// The field's ID or key.
    field: String,
    device_ids: Vec<String>,
    value: Value,
}

#[derive(Debug, Serialize)]
pub struct BulkResult {
    changed: usize,
}

/// `POST /v1/device-custom-fields`: sets or clears one field on many
/// devices. Each device is recorded in the audit log.
pub async fn bulk_set(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<BulkChange>,
) -> Result<Json<BulkResult>, ApiError> {
    actor.require(Permission::DevicesEdit)?;
    let mut device_ids = request
        .device_ids
        .iter()
        .map(|id| parse_id(id, "device"))
        .collect::<Result<Vec<_>, _>>()?;
    device_ids.sort();
    device_ids.dedup();
    if device_ids.is_empty() || device_ids.len() > MAX_DEVICES_PER_CHANGE {
        return Err(
            ApiError::bad_request(format!("choose 1 to {MAX_DEVICES_PER_CHANGE} devices"))
                .with_code("invalid_devices"),
        );
    }
    let mut transaction = state.database.begin().await?;
    let field = field_for(&mut transaction, &request.field, false).await?;
    let value = custom_fields::from_json(&field, &request.value)?;
    let mut devices: Vec<(String, String)> = Vec::with_capacity(device_ids.len());
    for chunk in device_ids.chunks(500) {
        devices.extend(
            transaction
                .fetch_all::<(String, String), _>(
                    &agents::active()
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
    let now = now_ms();
    let actor_label = actor.actor();
    for (device_id, name) in &devices {
        custom_fields::store(
            &mut transaction,
            &state.instance_key,
            &field,
            Owner::Device(device_id),
            value.as_deref(),
            ChangedBy::User(&actor.user.id),
            now,
        )
        .await?;
        audit::record(
            &mut transaction,
            &actor_label,
            "custom_field.set",
            Target::device(device_id),
            audit_change(&field, name, value.as_deref(), "bulk"),
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(Json(BulkResult {
        changed: devices.len(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    op: Operator,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Serialize)]
pub struct Matching {
    field_id: String,
    /// The devices whose value, their own or their company's, matches.
    device_ids: Vec<String>,
    matching: usize,
}

/// `GET /v1/custom-fields/{field}/devices?op=&value=`: the devices whose
/// value of a field (their own, or else their company's) matches.
pub async fn matching_devices(
    State(state): State<AppState>,
    actor: Authorized,
    Path(field): Path<String>,
    filter: Result<QueryString<Filter>, QueryRejection>,
) -> Result<Json<Matching>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let QueryString(filter) = filter.map_err(|rejection| {
        ApiError::bad_request(rejection.body_text()).with_code("invalid_filter")
    })?;
    let mut database = &state.database;
    let field = field_for(&mut database, &field, false).await?;
    if !field.field_type.takes(filter.op) {
        return Err(ApiError::bad_request(format!(
            "{} fields can't be filtered that way",
            field.field_type.as_str()
        ))
        .with_code("invalid_filter"));
    }
    if matches!(filter.op, Operator::Equals | Operator::Contains) && filter.value.trim().is_empty()
    {
        return Err(
            ApiError::bad_request("give a value to compare with").with_code("invalid_filter")
        );
    }
    let devices: Vec<(String, Option<String>)> = database
        .fetch_all(&agents::active().column(Agents::CompanyId).to_owned())
        .await?;
    let own =
        values_by_owner(custom_fields::stored_of_field(&mut database, &field.id, false).await?);
    let companies = if field.applies_to.companies() {
        values_by_owner(custom_fields::stored_of_field(&mut database, &field.id, true).await?)
    } else {
        HashMap::new()
    };
    let mut device_ids = devices
        .into_iter()
        .filter(|(device_id, company_id)| {
            let value = own.get(device_id).or_else(|| {
                company_id
                    .as_ref()
                    .and_then(|company_id| companies.get(company_id))
            });
            custom_fields::matches(
                &field,
                value.map(|value| value.as_deref().unwrap_or_default()),
                filter.op,
                &filter.value,
            )
        })
        .map(|(device_id, _)| device_id)
        .collect::<Vec<_>>();
    device_ids.sort();
    Ok(Json(Matching {
        field_id: field.id,
        matching: device_ids.len(),
        device_ids,
    }))
}

/// Values by whose they are; a secret's is `None`, which only matches
/// "is empty" and "isn't empty".
fn values_by_owner(stored: Vec<Stored>) -> HashMap<String, Option<String>> {
    stored
        .into_iter()
        .map(|stored| (stored.owner_id, stored.value))
        .collect()
}

#[derive(Debug, sqlx::FromRow)]
struct RunRow {
    status: String,
    script_name: String,
    requested_by_user_id: String,
}

/// `POST /v1/agents/{id}/script-runs/{run_id}/custom-fields`: a running
/// script sets one of its device's fields, through its Agent. The run must
/// still be running, and the field one scripts may write. Recorded as the
/// script's change.
pub async fn script_write(
    State(state): State<AppState>,
    Path((device_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let agent = agents::authenticate(&state, &headers, &device_id).await?;
    let run_id = parse_id(&run_id, "run")?;
    let bytes = super::agent::read_body(
        &headers,
        body,
        MAX_SCRIPT_FIELD_VALUE_BYTES + 1024,
        "the value is too large",
        state.agent_timeouts.upload_stall,
    )
    .await?;
    let write: ScriptFieldWrite = serde_json::from_slice(&bytes)
        .map_err(|error| ApiError::bad_request(format!("invalid request: {error}")))?;
    let mut transaction = state.database.begin().await?;
    let run: RunRow = transaction
        .fetch_optional(
            &Query::select()
                .columns([
                    ScriptRuns::Status,
                    ScriptRuns::ScriptName,
                    ScriptRuns::RequestedByUserId,
                ])
                .from(ScriptRuns::Table)
                .and_where(Expr::col(ScriptRuns::Id).eq(run_id.as_str()))
                .and_where(Expr::col(ScriptRuns::DeviceId).eq(agent.device_id.as_str()))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("no run has this ID"))?;
    if run.status != ScriptRunStatus::Pending.as_str() {
        return Err(ApiError::conflict("the script's run has finished").with_code("run_finished"));
    }
    let device = custom_fields::device(&mut transaction, &agent.device_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    let field = custom_fields::definition(&mut transaction, &write.key)
        .await?
        .filter(|field| field.key == write.key && field.applies_to.devices())
        .ok_or_else(|| {
            ApiError::not_found(format!("devices have no custom field “{}”", write.key))
                .with_code("field_not_found")
        })?;
    if field.script_access != ScriptAccess::ReadWrite {
        return Err(
            ApiError::forbidden(format!("scripts may not change “{}”", field.label))
                .with_code("field_not_writable"),
        );
    }
    let value = custom_fields::from_text(&field, &write.value)?;
    custom_fields::store(
        &mut transaction,
        &state.instance_key,
        &field,
        Owner::Device(&device.id),
        value.as_deref(),
        ChangedBy::Run {
            run_id: &run_id,
            script_name: &run.script_name,
        },
        now_ms(),
    )
    .await?;
    let mut change = audit_change(&field, &device.name, value.as_deref(), "script");
    change["run_id"] = run_id.clone().into();
    change["requested_by_user_id"] = run.requested_by_user_id.into();
    audit::record(
        &mut transaction,
        &Actor {
            user_id: None,
            label: format!("script “{}”", run.script_name),
            ip: None,
            api_token: None,
        },
        "custom_field.set",
        Target::device(&device.id),
        change,
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
