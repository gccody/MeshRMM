//! The registry tool's API: a Windows device's registry, browsed with
//! `tools.registry.view` and changed with `tools.registry.manage`. Every
//! change is audited on the device with its key, value name and result,
//! never the data written; browsing isn't. A device of another platform
//! has no registry, which the server says without asking it. See
//! docs/remote-tools.md#registry.
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use meshrmm_protocol_types::{
    DevicePlatform, RegistryData, RegistryKey, RegistryListing, RegistryRoot, RegistryValue,
    RegistryView, ToolOperation, ToolReply, valid_registry_value_name,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::tools::{authorized_device, failure, record, unexpected};
use crate::{
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    tools,
};

/// The Windows device `id` that `actor` may use with `permission`. Every
/// route here authorizes its device through this, and only this.
async fn windows_device(
    state: &AppState,
    actor: &Authorized,
    id: &str,
    permission: Permission,
) -> Result<String, ApiError> {
    let (device_id, platform) = authorized_device(state, actor, id, permission).await?;
    if platform != DevicePlatform::Windows {
        return Err(ApiError::bad_request(format!(
            "a {} device has no registry; only Windows does",
            platform.label()
        ))
        .with_code("unsupported"));
    }
    Ok(device_id)
}

fn parameters<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(parameters)| parameters)
        .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid")
}

/// `key`, checked, or why it isn't a key Windows could have.
fn checked_key(
    root: RegistryRoot,
    path: String,
    view: RegistryView,
) -> Result<RegistryKey, ApiError> {
    let key = RegistryKey { root, path, view };
    if key.valid() {
        Ok(key)
    } else {
        Err(invalid("that isn't a registry key's path"))
    }
}

fn checked_value_name(name: &str) -> Result<(), ApiError> {
    if valid_registry_value_name(name) {
        Ok(())
    } else {
        Err(invalid("that isn't a registry value's name"))
    }
}

/// The audit log's view of `key`: its root, path and view, under names
/// the audit log doesn't read as the target's.
fn key_metadata(key: &RegistryKey) -> Value {
    json!({ "key": key.display(), "view": key.view.as_str() })
}

/// Asks the device for a change and records it as `action` with `asked`
/// and its result.
async fn change(
    state: &AppState,
    actor: &Authorized,
    device_id: &str,
    operation: ToolOperation,
    action: &str,
    asked: Value,
) -> Result<ToolReply, ApiError> {
    match tools::ask(state, device_id, operation).await {
        Ok(reply @ (ToolReply::RegistryChanged | ToolReply::RegistryValue(_))) => {
            let mut metadata = asked;
            if let ToolReply::RegistryValue(value) = &reply {
                metadata["type"] = value.data.type_name().into();
                metadata["size"] = value.size.into();
            }
            metadata["result"] = "completed".into();
            record(state, actor, device_id, action, metadata).await;
            Ok(reply)
        }
        Ok(_) => {
            let error = unexpected();
            record(state, actor, device_id, action, failure(asked, &error)).await;
            Err(error)
        }
        Err(error) => {
            if !tools::never_sent(&error) {
                record(state, actor, device_id, action, failure(asked, &error)).await;
            }
            Err(error)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyQuery {
    root: RegistryRoot,
    #[serde(default)]
    path: String,
    #[serde(default)]
    view: RegistryView,
}

/// `GET /v1/agents/{id}/registry?root=&path=&view=`: a key's subkeys and
/// values, each value's data cut short.
pub async fn list_key(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<KeyQuery>, QueryRejection>,
) -> Result<Json<RegistryListing>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryView).await?;
    let query = parameters(query)?;
    let key = checked_key(query.root, query.path, query.view)?;
    match tools::ask(&state, &device_id, ToolOperation::ListRegistryKey { key }).await? {
        ToolReply::RegistryKey(listing) => Ok(Json(listing)),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueQuery {
    root: RegistryRoot,
    #[serde(default)]
    path: String,
    #[serde(default)]
    view: RegistryView,
    /// Empty for the key's default value.
    #[serde(default)]
    name: String,
}

/// `GET /v1/agents/{id}/registry/value?root=&path=&view=&name=`: one
/// value, whole, to edit it.
pub async fn get_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<ValueQuery>, QueryRejection>,
) -> Result<Json<RegistryValue>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryView).await?;
    let query = parameters(query)?;
    let key = checked_key(query.root, query.path, query.view)?;
    checked_value_name(&query.name)?;
    let operation = ToolOperation::GetRegistryValue {
        key,
        name: query.name,
    };
    match tools::ask(&state, &device_id, operation).await? {
        ToolReply::RegistryValue(value) => Ok(Json(value)),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyChange {
    root: RegistryRoot,
    path: String,
    #[serde(default)]
    view: RegistryView,
    /// For a delete: with its subkeys.
    #[serde(default)]
    recursive: bool,
}

/// `POST /v1/agents/{id}/registry/keys {root, path, view}`: creates a key
/// under one that exists. Audited as `registry.key.create`.
pub async fn create_key(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<KeyChange>,
) -> Result<Json<Value>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryManage).await?;
    let key = checked_key(request.root, request.path, request.view)?;
    if key.depth() == 0 {
        return Err(invalid("a root key can't be created"));
    }
    let asked = key_metadata(&key);
    let operation = ToolOperation::CreateRegistryKey { key };
    change(
        &state,
        &actor,
        &device_id,
        operation,
        "registry.key.create",
        asked,
    )
    .await?;
    Ok(Json(json!({})))
}

/// `POST /v1/agents/{id}/registry/keys/delete {root, path, view, recursive}`:
/// deletes a key; one with subkeys only when `recursive`. Audited as
/// `registry.key.delete`.
pub async fn delete_key(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<KeyChange>,
) -> Result<Json<Value>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryManage).await?;
    let key = checked_key(request.root, request.path, request.view)?;
    if key.depth() == 0 {
        return Err(invalid("a root key can't be deleted"));
    }
    let mut asked = key_metadata(&key);
    asked["recursive"] = request.recursive.into();
    let operation = ToolOperation::DeleteRegistryKey {
        key,
        recursive: request.recursive,
    };
    change(
        &state,
        &actor,
        &device_id,
        operation,
        "registry.key.delete",
        asked,
    )
    .await?;
    Ok(Json(json!({})))
}

// Not `deny_unknown_fields`, which serde doesn't support with `flatten`.
#[derive(Debug, Deserialize)]
pub struct SetValue {
    root: RegistryRoot,
    path: String,
    #[serde(default)]
    view: RegistryView,
    #[serde(default)]
    name: String,
    #[serde(flatten)]
    data: RegistryData,
    /// A new value: fails if one of that name exists.
    #[serde(default)]
    create: bool,
}

/// `POST /v1/agents/{id}/registry/values {root, path, view, name, type,
/// data, create}`: sets a value; returns it as written. Audited as
/// `registry.value.set` with its type and size, never its data.
pub async fn set_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<SetValue>,
) -> Result<Json<RegistryValue>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryManage).await?;
    let key = checked_key(request.root, request.path, request.view)?;
    checked_value_name(&request.name)?;
    if !request.data.writable() {
        return Err(invalid(format!(
            "{} data can't be written here",
            request.data.type_name()
        )));
    }
    if request.data.stored_bytes() > meshrmm_protocol_types::MAX_REGISTRY_VALUE_BYTES {
        return Err(invalid(format!(
            "a value may hold at most {} bytes here",
            meshrmm_protocol_types::MAX_REGISTRY_VALUE_BYTES
        )));
    }
    let mut asked = key_metadata(&key);
    asked["value"] = request.name.clone().into();
    asked["type"] = request.data.type_name().into();
    asked["created"] = request.create.into();
    let operation = ToolOperation::SetRegistryValue {
        key,
        name: request.name,
        data: request.data,
        create: request.create,
    };
    match change(
        &state,
        &actor,
        &device_id,
        operation,
        "registry.value.set",
        asked,
    )
    .await?
    {
        ToolReply::RegistryValue(value) => Ok(Json(value)),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameValue {
    root: RegistryRoot,
    path: String,
    #[serde(default)]
    view: RegistryView,
    from: String,
    to: String,
}

/// `POST /v1/agents/{id}/registry/values/rename {root, path, view, from,
/// to}`: renames a value. Audited as `registry.value.rename`.
pub async fn rename_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<RenameValue>,
) -> Result<Json<Value>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryManage).await?;
    let key = checked_key(request.root, request.path, request.view)?;
    checked_value_name(&request.from)?;
    checked_value_name(&request.to)?;
    if request.from == request.to {
        return Err(invalid("the new name is the same as the old"));
    }
    let mut asked = key_metadata(&key);
    asked["value"] = request.from.clone().into();
    asked["renamed_to"] = request.to.clone().into();
    let operation = ToolOperation::RenameRegistryValue {
        key,
        from: request.from,
        to: request.to,
    };
    change(
        &state,
        &actor,
        &device_id,
        operation,
        "registry.value.rename",
        asked,
    )
    .await?;
    Ok(Json(json!({})))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteValue {
    root: RegistryRoot,
    path: String,
    #[serde(default)]
    view: RegistryView,
    #[serde(default)]
    name: String,
}

/// `POST /v1/agents/{id}/registry/values/delete {root, path, view, name}`:
/// deletes a value. Audited as `registry.value.delete`.
pub async fn delete_value(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<DeleteValue>,
) -> Result<Json<Value>, ApiError> {
    let device_id = windows_device(&state, &actor, &id, Permission::ToolsRegistryManage).await?;
    let key = checked_key(request.root, request.path, request.view)?;
    checked_value_name(&request.name)?;
    let mut asked = key_metadata(&key);
    asked["value"] = request.name.clone().into();
    let operation = ToolOperation::DeleteRegistryValue {
        key,
        name: request.name,
    };
    change(
        &state,
        &actor,
        &device_id,
        operation,
        "registry.value.delete",
        asked,
    )
    .await?;
    Ok(Json(json!({})))
}
