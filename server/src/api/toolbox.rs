//! The toolbox library as the website manages it. The rules for who sees and
//! changes what are in [`crate::toolbox`].
use std::collections::HashMap;

use axum::{
    Json,
    body::Body,
    extract::{Path, Query as QueryParameters, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use meshrmm_protocol_types::{
    DEFAULT_SCRIPT_TIMEOUT_SECONDS, ScriptLanguage, normalize_folder, valid_file_name,
    valid_script_body, valid_script_description, valid_script_name, valid_script_timeout,
    valid_sha256_hex,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::parse_id,
    audit::{self, Target},
    auth::Authorized,
    db::tables::{ToolboxFiles, ToolboxScripts},
    http::{ApiError, AppState, JsonBody},
    storage::ReceiveError,
    time::now_ms,
    toolbox::{self, FileRow, Kind, ScriptRow, ToolboxUser},
    users::new_id,
};

const INVALID_FILE_NAME: &str = "file names must be at most 255 characters and ones Windows allows: no \\ / : * ? \" < > |, \
     no trailing dot or space, and not a device name such as CON";
const INVALID_FOLDER: &str =
    "folders may be at most 8 levels deep, with names of at most 64 characters";

/// The signed-in user as a toolbox user.
pub fn toolbox_user(actor: &Authorized) -> ToolboxUser {
    ToolboxUser {
        user_id: actor.user.id.clone(),
        permissions: actor.permissions.clone(),
        actor: actor.actor(),
    }
}

/// A script as the website shows it. Lists leave out the body.
#[derive(Debug, Serialize)]
pub struct ScriptView {
    id: String,
    name: String,
    folder: String,
    description: String,
    language: String,
    timeout_seconds: i64,
    shared: bool,
    /// The user added it.
    owned: bool,
    can_edit: bool,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

fn script_view(row: ScriptRow, user: &ToolboxUser, with_body: bool) -> ScriptView {
    ScriptView {
        owned: row.owner_user_id == user.user_id,
        can_edit: user.can_edit(Kind::Script, &row.owner_user_id, row.shared),
        id: row.id,
        name: row.name,
        folder: row.folder,
        description: row.description,
        language: row.language,
        timeout_seconds: row.timeout_seconds,
        shared: row.shared,
        created_at_unix_ms: row.created_at,
        updated_at_unix_ms: row.updated_at,
        body: with_body.then_some(row.body),
    }
}

#[derive(Debug, Serialize)]
pub struct FileView {
    id: String,
    name: String,
    folder: String,
    size_bytes: i64,
    sha256: String,
    shared: bool,
    owned: bool,
    can_edit: bool,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

fn file_view(row: FileRow, user: &ToolboxUser) -> FileView {
    FileView {
        owned: row.owner_user_id == user.user_id,
        can_edit: user.can_edit(Kind::File, &row.owner_user_id, row.shared),
        id: row.id,
        name: row.name,
        folder: row.folder,
        size_bytes: row.size_bytes,
        sha256: row.sha256,
        shared: row.shared,
        created_at_unix_ms: row.created_at,
        updated_at_unix_ms: row.updated_at,
    }
}

#[derive(Debug, Serialize)]
pub struct Listing {
    scripts: Vec<ScriptView>,
    files: Vec<FileView>,
    /// The largest file an upload may be.
    max_file_bytes: u64,
}

fn require_toolbox(user: &ToolboxUser) -> Result<(), ApiError> {
    if user.sees(Kind::Script) || user.sees(Kind::File) {
        Ok(())
    } else {
        user.require(Kind::Script.use_permission())
    }
}

/// `GET /v1/toolbox`: the scripts and files the user may use.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Listing>, ApiError> {
    let user = toolbox_user(&actor);
    require_toolbox(&user)?;
    let mut database = &state.database;
    let scripts = toolbox::visible_scripts(&mut database, &user)
        .await?
        .into_iter()
        .map(|row| script_view(row, &user, false))
        .collect();
    let files = toolbox::visible_files(&mut database, &user)
        .await?
        .into_iter()
        .map(|row| file_view(row, &user))
        .collect();
    Ok(Json(Listing {
        scripts,
        files,
        max_file_bytes: state.config.toolbox.max_file_bytes,
    }))
}

fn default_timeout_seconds() -> u32 {
    DEFAULT_SCRIPT_TIMEOUT_SECONDS
}

/// A script as the website saves it, whole.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptInput {
    name: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    description: String,
    language: ScriptLanguage,
    body: String,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u32,
    #[serde(default)]
    shared: bool,
}

impl ScriptInput {
    /// The input as stored, or why it cannot be.
    fn normalized(mut self) -> Result<Self, ApiError> {
        self.name = self.name.trim().to_owned();
        if !valid_script_name(&self.name) {
            return Err(ApiError::bad_request(
                "script names must be 1 to 120 characters on one line",
            ));
        }
        self.folder =
            normalize_folder(&self.folder).ok_or_else(|| ApiError::bad_request(INVALID_FOLDER))?;
        self.description = self.description.trim().to_owned();
        if !valid_script_description(&self.description) {
            return Err(ApiError::bad_request(
                "descriptions must be at most 1000 bytes",
            ));
        }
        if !valid_script_body(&self.body) {
            return Err(ApiError::bad_request(
                "scripts must not be empty and must be at most 128 KiB",
            ));
        }
        if !valid_script_timeout(self.timeout_seconds) {
            return Err(ApiError::bad_request(
                "the timeout must be between 10 and 3600 seconds",
            ));
        }
        Ok(self)
    }
}

fn script_not_found() -> ApiError {
    ApiError::not_found("Script not found")
}

/// `GET /v1/toolbox/scripts/{id}`, with its body.
pub async fn get_script(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ScriptView>, ApiError> {
    let user = toolbox_user(&actor);
    let id = parse_id(&id, "script")?;
    let row = toolbox::visible_script(&mut &state.database, &user, &id)
        .await?
        .ok_or_else(script_not_found)?;
    Ok(Json(script_view(row, &user, true)))
}

/// `POST /v1/toolbox/scripts`.
pub async fn create_script(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<ScriptInput>,
) -> Result<(StatusCode, Json<ScriptView>), ApiError> {
    let user = toolbox_user(&actor);
    let input = input.normalized()?;
    user.require_keep(Kind::Script, input.shared)?;
    let id = new_id();
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    transaction
        .execute(
            &Query::insert()
                .into_table(ToolboxScripts::Table)
                .columns([
                    ToolboxScripts::Id,
                    ToolboxScripts::OwnerUserId,
                    ToolboxScripts::Shared,
                    ToolboxScripts::Folder,
                    ToolboxScripts::Name,
                    ToolboxScripts::Description,
                    ToolboxScripts::Language,
                    ToolboxScripts::Body,
                    ToolboxScripts::TimeoutSeconds,
                    ToolboxScripts::CreatedAt,
                    ToolboxScripts::UpdatedAt,
                    ToolboxScripts::UpdatedByUserId,
                ])
                .values_panic([
                    id.as_str().into(),
                    user.user_id.as_str().into(),
                    input.shared.into(),
                    input.folder.as_str().into(),
                    input.name.as_str().into(),
                    input.description.as_str().into(),
                    input.language.as_str().into(),
                    input.body.as_str().into(),
                    i64::from(input.timeout_seconds).into(),
                    now.into(),
                    now.into(),
                    user.user_id.as_str().into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &user.actor,
        "toolbox.script_create",
        Target::toolbox_script(&id),
        json!({ "name": input.name, "shared": input.shared }),
    )
    .await?;
    let row = toolbox::visible_script(&mut transaction, &user, &id)
        .await?
        .ok_or_else(script_not_found)?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Json(script_view(row, &user, true))))
}

/// `PUT /v1/toolbox/scripts/{id}`: replaces the script. Answers 204 when the
/// change makes the script one the user no longer sees: someone else's,
/// unshared.
pub async fn update_script(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<ScriptInput>,
) -> Result<Response, ApiError> {
    let user = toolbox_user(&actor);
    let id = parse_id(&id, "script")?;
    let input = input.normalized()?;
    let mut transaction = state.database.begin().await?;
    let current = toolbox::visible_script(&mut transaction, &user, &id)
        .await?
        .ok_or_else(script_not_found)?;
    require_edit(&user, Kind::Script, current.shared)?;
    if input.shared {
        user.require(Kind::Script.manage_permission())?;
    }
    let changed = transaction
        .execute(
            &Query::update()
                .table(ToolboxScripts::Table)
                .value(ToolboxScripts::Shared, input.shared)
                .value(ToolboxScripts::Folder, input.folder.as_str())
                .value(ToolboxScripts::Name, input.name.as_str())
                .value(ToolboxScripts::Description, input.description.as_str())
                .value(ToolboxScripts::Language, input.language.as_str())
                .value(ToolboxScripts::Body, input.body.as_str())
                .value(
                    ToolboxScripts::TimeoutSeconds,
                    i64::from(input.timeout_seconds),
                )
                .value(ToolboxScripts::UpdatedAt, now_ms())
                .value(ToolboxScripts::UpdatedByUserId, user.user_id.as_str())
                .and_where(Expr::col(ToolboxScripts::Id).eq(id.as_str()))
                .and_where(Expr::col(ToolboxScripts::Shared).eq(current.shared))
                .and_where(
                    Expr::col(ToolboxScripts::OwnerUserId).eq(current.owner_user_id.as_str()),
                )
                .to_owned(),
        )
        .await?;
    if changed == 0 {
        return Err(changed_meanwhile());
    }
    audit::record(
        &mut transaction,
        &user.actor,
        "toolbox.script_update",
        Target::toolbox_script(&id),
        json!({ "name": input.name, "shared": input.shared }),
    )
    .await?;
    let row = toolbox::visible_script(&mut transaction, &user, &id).await?;
    transaction.commit().await?;
    Ok(match row {
        Some(row) => Json(script_view(row, &user, true)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// Another request shared, unshared or deleted the item after it was
/// checked. Only PostgreSQL lets that happen; SQLite serializes the two.
fn changed_meanwhile() -> ApiError {
    ApiError::conflict("someone else changed this item at the same time; reload and try again")
}

/// Checks that the user may change an item they see. A private item they
/// see is their own, so only the permission can be missing.
fn require_edit(user: &ToolboxUser, kind: Kind, shared: bool) -> Result<(), ApiError> {
    user.require(if shared {
        kind.manage_permission()
    } else {
        kind.use_permission()
    })
}

/// `DELETE /v1/toolbox/scripts/{id}`. Past runs keep the script's name.
pub async fn delete_script(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user = toolbox_user(&actor);
    let id = parse_id(&id, "script")?;
    let mut transaction = state.database.begin().await?;
    let current = toolbox::visible_script(&mut transaction, &user, &id)
        .await?
        .ok_or_else(script_not_found)?;
    require_edit(&user, Kind::Script, current.shared)?;
    let changed = transaction
        .execute(
            &Query::delete()
                .from_table(ToolboxScripts::Table)
                .and_where(Expr::col(ToolboxScripts::Id).eq(id.as_str()))
                .and_where(Expr::col(ToolboxScripts::Shared).eq(current.shared))
                .and_where(
                    Expr::col(ToolboxScripts::OwnerUserId).eq(current.owner_user_id.as_str()),
                )
                .to_owned(),
        )
        .await?;
    if changed == 0 {
        return Err(changed_meanwhile());
    }
    audit::record(
        &mut transaction,
        &user.actor,
        "toolbox.script_delete",
        Target::toolbox_script(&id),
        json!({ "name": current.name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn file_not_found() -> ApiError {
    ApiError::not_found("File not found")
}

/// `POST /v1/toolbox/files?name=&folder=&shared=&sha256=` with the file as
/// the body. The content must match the SHA-256 the browser computed.
pub async fn upload_file(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(parameters): QueryParameters<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<FileView>), ApiError> {
    let user = toolbox_user(&actor);
    let parameter = |name: &str| parameters.get(name).map(String::as_str).unwrap_or_default();
    let name = parameter("name").trim().to_owned();
    if !valid_file_name(&name) {
        return Err(ApiError::bad_request(INVALID_FILE_NAME));
    }
    let folder = normalize_folder(parameter("folder"))
        .ok_or_else(|| ApiError::bad_request(INVALID_FOLDER))?;
    let shared = matches!(parameter("shared"), "1" | "true");
    user.require_keep(Kind::File, shared)?;
    let sha256 = parameter("sha256").to_ascii_lowercase();
    if !valid_sha256_hex(&sha256) {
        return Err(ApiError::bad_request("the upload needs the file's SHA-256"));
    }
    let max_bytes = state.config.toolbox.max_file_bytes;
    let too_large = || {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("toolbox files may be at most {max_bytes} bytes"),
        )
        .with_code("file_too_large")
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|length| length > max_bytes) {
        return Err(too_large());
    }
    let id = new_id();
    let path = state.storage.toolbox_file(&id);
    let received = state
        .storage
        .receive(&path, body.into_data_stream(), max_bytes, Some(&sha256))
        .await
        .map_err(|error| match error {
            ReceiveError::TooLarge(_) => too_large(),
            ReceiveError::ChecksumMismatch => ApiError::bad_request(
                "the upload did not match its SHA-256; it may have been corrupted, so try again",
            ),
            ReceiveError::Interrupted => {
                ApiError::bad_request("the upload was interrupted; try again")
            }
            ReceiveError::Io(error) => anyhow::Error::from(error).into(),
        })?;
    let stored = store_file_row(
        &state,
        &user,
        &id,
        &name,
        &folder,
        shared,
        &received.sha256,
        received.size_bytes,
    )
    .await;
    match stored {
        Ok(row) => Ok((StatusCode::CREATED, Json(file_view(row, &user)))),
        Err(error) => {
            if let Err(remove) = state.storage.remove(&path).await {
                tracing::warn!(file_id = id, %remove, "could not remove an unrecorded upload");
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn store_file_row(
    state: &AppState,
    user: &ToolboxUser,
    id: &str,
    name: &str,
    folder: &str,
    shared: bool,
    sha256: &str,
    size_bytes: u64,
) -> Result<FileRow, ApiError> {
    let size_bytes = i64::try_from(size_bytes).map_err(|_| ApiError::internal())?;
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    transaction
        .execute(
            &Query::insert()
                .into_table(ToolboxFiles::Table)
                .columns([
                    ToolboxFiles::Id,
                    ToolboxFiles::OwnerUserId,
                    ToolboxFiles::Shared,
                    ToolboxFiles::Folder,
                    ToolboxFiles::Name,
                    ToolboxFiles::SizeBytes,
                    ToolboxFiles::Sha256,
                    ToolboxFiles::CreatedAt,
                    ToolboxFiles::UpdatedAt,
                    ToolboxFiles::UpdatedByUserId,
                ])
                .values_panic([
                    id.into(),
                    user.user_id.as_str().into(),
                    shared.into(),
                    folder.into(),
                    name.into(),
                    size_bytes.into(),
                    sha256.into(),
                    now.into(),
                    now.into(),
                    user.user_id.as_str().into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &user.actor,
        "toolbox.file_upload",
        Target::toolbox_file(id),
        json!({ "name": name, "size_bytes": size_bytes, "shared": shared }),
    )
    .await?;
    let row = toolbox::visible_file(&mut transaction, user, id)
        .await?
        .ok_or_else(file_not_found)?;
    transaction.commit().await?;
    Ok(row)
}

/// A library file's details as the website saves them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileInput {
    name: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    shared: bool,
}

/// `PUT /v1/toolbox/files/{id}`: renames, moves or shares a file. Its
/// content cannot change; upload a new file instead.
pub async fn update_file(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<FileInput>,
) -> Result<Response, ApiError> {
    let user = toolbox_user(&actor);
    let id = parse_id(&id, "file")?;
    let name = input.name.trim();
    if !valid_file_name(name) {
        return Err(ApiError::bad_request(INVALID_FILE_NAME));
    }
    let folder =
        normalize_folder(&input.folder).ok_or_else(|| ApiError::bad_request(INVALID_FOLDER))?;
    let mut transaction = state.database.begin().await?;
    let current = toolbox::visible_file(&mut transaction, &user, &id)
        .await?
        .ok_or_else(file_not_found)?;
    require_edit(&user, Kind::File, current.shared)?;
    if input.shared {
        user.require(Kind::File.manage_permission())?;
    }
    let changed = transaction
        .execute(
            &Query::update()
                .table(ToolboxFiles::Table)
                .value(ToolboxFiles::Shared, input.shared)
                .value(ToolboxFiles::Folder, folder.as_str())
                .value(ToolboxFiles::Name, name)
                .value(ToolboxFiles::UpdatedAt, now_ms())
                .value(ToolboxFiles::UpdatedByUserId, user.user_id.as_str())
                .and_where(Expr::col(ToolboxFiles::Id).eq(id.as_str()))
                .and_where(Expr::col(ToolboxFiles::Shared).eq(current.shared))
                .and_where(Expr::col(ToolboxFiles::OwnerUserId).eq(current.owner_user_id.as_str()))
                .to_owned(),
        )
        .await?;
    if changed == 0 {
        return Err(changed_meanwhile());
    }
    audit::record(
        &mut transaction,
        &user.actor,
        "toolbox.file_update",
        Target::toolbox_file(&id),
        json!({ "name": name, "shared": input.shared }),
    )
    .await?;
    let row = toolbox::visible_file(&mut transaction, &user, &id).await?;
    transaction.commit().await?;
    Ok(match row {
        Some(row) => Json(file_view(row, &user)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// `DELETE /v1/toolbox/files/{id}`.
pub async fn delete_file(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user = toolbox_user(&actor);
    let id = parse_id(&id, "file")?;
    let mut transaction = state.database.begin().await?;
    let current = toolbox::visible_file(&mut transaction, &user, &id)
        .await?
        .ok_or_else(file_not_found)?;
    require_edit(&user, Kind::File, current.shared)?;
    let changed = transaction
        .execute(
            &Query::delete()
                .from_table(ToolboxFiles::Table)
                .and_where(Expr::col(ToolboxFiles::Id).eq(id.as_str()))
                .and_where(Expr::col(ToolboxFiles::Shared).eq(current.shared))
                .and_where(Expr::col(ToolboxFiles::OwnerUserId).eq(current.owner_user_id.as_str()))
                .to_owned(),
        )
        .await?;
    if changed == 0 {
        return Err(changed_meanwhile());
    }
    audit::record(
        &mut transaction,
        &user.actor,
        "toolbox.file_delete",
        Target::toolbox_file(&id),
        json!({ "name": current.name }),
    )
    .await?;
    transaction.commit().await?;
    // The row is gone, so nothing can reach the content even if this fails.
    if let Err(error) = state.storage.remove(&state.storage.toolbox_file(&id)).await {
        tracing::warn!(file_id = id, %error, "could not remove a deleted toolbox file");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/toolbox/files/{id}/content`.
pub async fn download_file(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let user = toolbox_user(&actor);
    let id = parse_id(&id, "file")?;
    toolbox::visible_file(&mut &state.database, &user, &id)
        .await?
        .ok_or_else(file_not_found)?;
    super::agent::file_download(&state, &id)
        .await?
        .ok_or_else(file_not_found)
}
