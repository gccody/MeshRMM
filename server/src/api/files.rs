//! The file browser's API: a device's folders, changes to them, and the
//! downloads and uploads that stream through the server. Browsing and
//! downloading need `files.browse`, changes and uploads `files.manage`, and
//! working as SYSTEM or root (the default) `files.system` as well. API
//! tokens may use every route, within their permissions. See
//! [`crate::files`].
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State, WebSocketUpgrade, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use meshrmm_protocol_types::{
    FileBatch, FileDownloadInfo, FileEntry, FileListing, FileOperation, FileReply, FileRoots,
    FileSort, MAX_FILE_BATCH, MAX_FILE_LIST_PAGE, MAX_FILE_STREAM_MESSAGE_BYTES, RunAs,
    valid_sha256_hex,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::{self, parse_id},
    auth::Authorized,
    files::{self, Download, Progress, Upload, relay},
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
};

fn parameters<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(parameters)| parameters)
        .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
}

fn default_account() -> RunAs {
    RunAs::System
}

/// The enrolled device `id`, checked.
async fn device(state: &AppState, id: &str) -> Result<String, ApiError> {
    let device_id = parse_id(id, "device")?;
    agents::active_platform(&mut &state.database, &device_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    Ok(device_id)
}

fn unexpected() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "the device answered with something unexpected",
    )
}

fn check_paths(paths: &[String]) -> Result<(), ApiError> {
    if paths.is_empty() || paths.len() > MAX_FILE_BATCH {
        return Err(ApiError::bad_request(format!(
            "choose 1 to {MAX_FILE_BATCH} entries"
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct AccountQuery {
    #[serde(default = "default_account")]
    run_as: RunAs,
}

/// `GET /v1/agents/{id}/files/roots?run_as=`: the drives or `/` and
/// volumes, and the users' folders, to start browsing from.
pub async fn roots(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<AccountQuery>, QueryRejection>,
) -> Result<Json<FileRoots>, ApiError> {
    let AccountQuery { run_as } = parameters(query)?;
    files::require(&actor, Permission::FilesBrowse, run_as)?;
    let device_id = device(&state, &id).await?;
    match files::ask(&state, &actor, &device_id, run_as, FileOperation::Roots).await? {
        FileReply::Roots(roots) => Ok(Json(roots)),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Order {
    Asc,
    Desc,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    path: String,
    #[serde(default = "default_account")]
    run_as: RunAs,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
    sort: Option<String>,
    order: Option<Order>,
    #[serde(default)]
    hidden: bool,
    filter: Option<String>,
    #[serde(default)]
    refresh: bool,
}

/// `GET /v1/agents/{id}/files?path=&run_as=&offset=&limit=&sort=&order=&hidden=&filter=&refresh=`:
/// a page of a folder's entries, folders first, in `sort` order (`name`,
/// `size`, `modified` or `type`).
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<FileListing>, ApiError> {
    let query = parameters(query)?;
    files::require(&actor, Permission::FilesBrowse, query.run_as)?;
    let device_id = device(&state, &id).await?;
    let sort = match query.sort.as_deref() {
        None => FileSort::Name,
        Some(sort) => FileSort::parse(sort)
            .ok_or_else(|| ApiError::bad_request("sort by name, size, modified or type"))?,
    };
    let limit = query.limit.unwrap_or(500);
    if limit == 0 || limit > MAX_FILE_LIST_PAGE {
        return Err(ApiError::bad_request(format!(
            "a page has 1 to {MAX_FILE_LIST_PAGE} entries"
        )));
    }
    let operation = FileOperation::List {
        path: query.path,
        offset: query.offset,
        limit,
        sort,
        descending: matches!(query.order, Some(Order::Desc)),
        hidden: query.hidden,
        filter: query.filter.filter(|filter| !filter.trim().is_empty()),
        refresh: query.refresh,
    };
    match files::ask(&state, &actor, &device_id, query.run_as, operation).await? {
        FileReply::Listing(listing) => Ok(Json(listing)),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Serialize)]
pub struct EntryView {
    path: String,
    entry: FileEntry,
}

fn entry_view(reply: FileReply) -> Result<EntryView, ApiError> {
    match reply {
        FileReply::Entry { path, entry } => Ok(EntryView { path, entry }),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateFolder {
    path: String,
    #[serde(default = "default_account")]
    run_as: RunAs,
}

/// `POST /v1/agents/{id}/files/folders {path, run_as}`: a new folder.
pub async fn create_folder(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<CreateFolder>,
) -> Result<(StatusCode, Json<EntryView>), ApiError> {
    files::require(&actor, Permission::FilesManage, request.run_as)?;
    let device_id = device(&state, &id).await?;
    let operation = FileOperation::CreateFolder { path: request.path };
    let view =
        entry_view(files::ask(&state, &actor, &device_id, request.run_as, operation).await?)?;
    relay::record(
        &state,
        &actor.actor(),
        &device_id,
        "files.create_folder",
        json!({ "path": view.path, "run_as": request.run_as.as_str() }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    path: String,
    name: String,
    #[serde(default = "default_account")]
    run_as: RunAs,
}

/// `POST /v1/agents/{id}/files/rename {path, name, run_as}`: renames an
/// entry in its folder.
pub async fn rename(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<Rename>,
) -> Result<Json<EntryView>, ApiError> {
    files::require(&actor, Permission::FilesManage, request.run_as)?;
    let device_id = device(&state, &id).await?;
    let operation = FileOperation::Rename {
        path: request.path.clone(),
        name: request.name,
    };
    let view =
        entry_view(files::ask(&state, &actor, &device_id, request.run_as, operation).await?)?;
    relay::record(
        &state,
        &actor.actor(),
        &device_id,
        "files.rename",
        json!({ "path": request.path, "new_path": view.path, "run_as": request.run_as.as_str() }),
    )
    .await;
    Ok(Json(view))
}

/// The audit log's view of a batch: what was done, and why the rest wasn't.
fn batch_metadata(batch: &FileBatch) -> serde_json::Value {
    json!({
        "paths": batch.done.iter().take(100).collect::<Vec<_>>(),
        "count": batch.done.len(),
        "failed": batch.failed.iter().take(100).map(|failure| json!({
            "path": failure.path,
            "code": failure.error.code.as_str(),
        })).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Move {
    paths: Vec<String>,
    destination: String,
    #[serde(default = "default_account")]
    run_as: RunAs,
}

/// `POST /v1/agents/{id}/files/move {paths, destination, run_as}`: moves
/// entries into a folder; says which moved and why others didn't.
pub async fn move_entries(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<Move>,
) -> Result<Json<FileBatch>, ApiError> {
    files::require(&actor, Permission::FilesManage, request.run_as)?;
    check_paths(&request.paths)?;
    let device_id = device(&state, &id).await?;
    let operation = FileOperation::Move {
        paths: request.paths,
        destination: request.destination.clone(),
    };
    let FileReply::Batch(batch) =
        files::ask(&state, &actor, &device_id, request.run_as, operation).await?
    else {
        return Err(unexpected());
    };
    if !batch.done.is_empty() {
        let mut metadata = batch_metadata(&batch);
        metadata["destination"] = request.destination.into();
        metadata["run_as"] = request.run_as.as_str().into();
        relay::record(&state, &actor.actor(), &device_id, "files.move", metadata).await;
    }
    Ok(Json(batch))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    paths: Vec<String>,
    #[serde(default)]
    trash: bool,
    #[serde(default = "default_account")]
    run_as: RunAs,
}

/// `POST /v1/agents/{id}/files/delete {paths, trash, run_as}`: deletes
/// entries, into the signed-in user's Recycle Bin or Trash when `trash`.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<Delete>,
) -> Result<Json<FileBatch>, ApiError> {
    files::require(&actor, Permission::FilesManage, request.run_as)?;
    check_paths(&request.paths)?;
    let device_id = device(&state, &id).await?;
    let operation = FileOperation::Delete {
        paths: request.paths,
        trash: request.trash,
    };
    let FileReply::Batch(batch) =
        files::ask(&state, &actor, &device_id, request.run_as, operation).await?
    else {
        return Err(unexpected());
    };
    if !batch.done.is_empty() {
        let mut metadata = batch_metadata(&batch);
        metadata["trash"] = request.trash.into();
        metadata["run_as"] = request.run_as.as_str().into();
        relay::record(&state, &actor.actor(), &device_id, "files.delete", metadata).await;
    }
    Ok(Json(batch))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDownload {
    paths: Vec<String>,
    #[serde(default = "default_account")]
    run_as: RunAs,
}

#[derive(Debug, Serialize)]
pub struct DownloadView {
    id: String,
    device_id: String,
    #[serde(flatten)]
    info: FileDownloadInfo,
    /// Where the browser gets it.
    url: String,
    /// How its latest attempt is going.
    progress: Progress,
}

fn download_view(download: &Download, progress: Progress) -> DownloadView {
    DownloadView {
        url: format!("/v1/file-downloads/{}", download.id),
        id: download.id.clone(),
        device_id: download.device_id.clone(),
        info: download.info.clone(),
        progress,
    }
}

/// `POST /v1/agents/{id}/files/downloads {paths, run_as}`: checks that the
/// entries can be downloaded and returns a download to fetch at `url`:
/// one file as itself, or a folder or several entries as a ZIP archive.
pub async fn create_download(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<NewDownload>,
) -> Result<(StatusCode, Json<DownloadView>), ApiError> {
    files::require(&actor, Permission::FilesBrowse, request.run_as)?;
    check_paths(&request.paths)?;
    let device_id = device(&state, &id).await?;
    let max_bytes = files::transfer_limit(&state).await?;
    let operation = FileOperation::PrepareDownload {
        paths: request.paths.clone(),
        max_bytes,
    };
    let FileReply::Download(info) =
        files::ask(&state, &actor, &device_id, request.run_as, operation).await?
    else {
        return Err(unexpected());
    };
    let download = Download {
        id: uuid::Uuid::new_v4().to_string(),
        device_id,
        user_id: actor.user.id.clone(),
        actor: actor.actor(),
        run_as: request.run_as,
        paths: request.paths,
        info,
    };
    state.files.add_download(download.clone())?;
    Ok((
        StatusCode::CREATED,
        Json(download_view(&download, Progress::default())),
    ))
}

/// The download `id` of `actor`, with access to it checked again.
fn own_download(
    state: &AppState,
    actor: &Authorized,
    id: &str,
) -> Result<(Download, Progress), ApiError> {
    let id = parse_id(id, "download")?;
    let (download, progress) = state.files.download(&id, &actor.user.id).ok_or_else(|| {
        ApiError::not_found("the download has expired; start it again")
            .with_code("download_not_found")
    })?;
    files::require(actor, Permission::FilesBrowse, download.run_as)?;
    Ok((download, progress))
}

/// `GET /v1/file-downloads/{id}`: the download's bytes. A file can be
/// resumed with `Range`; the download can be fetched again while it lasts.
pub async fn fetch_download(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (download, _) = own_download(&state, &actor, &id)?;
    if !state.agents.is_connected(&download.device_id) {
        return Err(files::device_offline());
    }
    relay::download(&state, download, &headers).await
}

/// `GET /v1/file-downloads/{id}/progress`: how its latest attempt is
/// going, with the SHA-256 of what was sent once it's done.
pub async fn download_progress(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<DownloadView>, ApiError> {
    let (download, progress) = own_download(&state, &actor, &id)?;
    Ok(Json(download_view(&download, progress)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUpload {
    path: String,
    size: u64,
    #[serde(default)]
    overwrite: bool,
    #[serde(default = "default_account")]
    run_as: RunAs,
}

#[derive(Debug, Serialize)]
pub struct UploadView {
    id: String,
    device_id: String,
    path: String,
    size: u64,
    /// The bytes the device has.
    received: u64,
}

fn upload_view(upload: &Upload, received: u64) -> UploadView {
    UploadView {
        id: upload.id.clone(),
        device_id: upload.device_id.clone(),
        path: upload.path.clone(),
        size: upload.size,
        received,
    }
}

/// `POST /v1/agents/{id}/files/uploads {path, size, overwrite, run_as}`:
/// starts an upload to `path`, making missing folders. The bytes follow
/// with `PUT /v1/file-uploads/{id}`, and `…/finish` puts the file in place.
pub async fn create_upload(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<NewUpload>,
) -> Result<(StatusCode, Json<UploadView>), ApiError> {
    files::require(&actor, Permission::FilesManage, request.run_as)?;
    let device_id = device(&state, &id).await?;
    let limit = files::transfer_limit(&state).await?;
    if request.size > limit {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "the file is larger than this server's transfer limit of {} MB",
                limit / (1024 * 1024)
            ),
        )
        .with_code("too_large"));
    }
    let upload = Upload {
        id: uuid::Uuid::new_v4().to_string(),
        device_id,
        user_id: actor.user.id.clone(),
        actor: actor.actor(),
        run_as: request.run_as,
        path: request.path,
        size: request.size,
        overwrite: request.overwrite,
    };
    let operation = FileOperation::PrepareUpload {
        upload_id: upload.id.clone(),
        path: upload.path.clone(),
        size: upload.size,
        overwrite: upload.overwrite,
    };
    let FileReply::Upload { received } =
        files::ask(&state, &actor, &upload.device_id, upload.run_as, operation).await?
    else {
        return Err(unexpected());
    };
    state.files.add_upload(upload.clone())?;
    Ok((StatusCode::CREATED, Json(upload_view(&upload, received))))
}

/// The upload `id` of `actor`, with access to it checked again.
fn own_upload(state: &AppState, actor: &Authorized, id: &str) -> Result<Upload, ApiError> {
    let id = parse_id(id, "upload")?;
    let upload = state.files.upload(&id, &actor.user.id).ok_or_else(|| {
        ApiError::not_found("the upload has expired; start it again").with_code("upload_not_found")
    })?;
    files::require(actor, Permission::FilesManage, upload.run_as)?;
    Ok(upload)
}

/// `GET /v1/file-uploads/{id}`: how much of the upload the device has, to
/// resume it.
pub async fn upload_status(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<UploadView>, ApiError> {
    let upload = own_upload(&state, &actor, &id)?;
    let operation = FileOperation::UploadStatus {
        upload_id: upload.id.clone(),
        path: upload.path.clone(),
    };
    match files::ask(&state, &actor, &upload.device_id, upload.run_as, operation).await? {
        FileReply::Upload { received } => Ok(Json(upload_view(&upload, received))),
        _ => Err(unexpected()),
    }
}

#[derive(Debug, Deserialize)]
pub struct OffsetQuery {
    #[serde(default)]
    offset: u64,
}

/// `PUT /v1/file-uploads/{id}?offset=` with bytes of the file from
/// `offset`, which must be what the device has: the whole rest of it, or a
/// part. Returns how much the device has after.
pub async fn send_upload(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<OffsetQuery>, QueryRejection>,
    body: Body,
) -> Result<Json<UploadView>, ApiError> {
    let OffsetQuery { offset } = parameters(query)?;
    let upload = own_upload(&state, &actor, &id)?;
    if !state.agents.is_connected(&upload.device_id) {
        return Err(files::device_offline());
    }
    let received = relay::upload(&state, &upload, offset, body).await?;
    Ok(Json(upload_view(&upload, received)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finish {
    sha256: String,
}

/// `POST /v1/file-uploads/{id}/finish {sha256}`: once the device has the
/// whole file, checks it against the browser's SHA-256 and puts it in
/// place. A mismatch discards it.
pub async fn finish_upload(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<Finish>,
) -> Result<Json<EntryView>, ApiError> {
    let upload = own_upload(&state, &actor, &id)?;
    let sha256 = request.sha256.to_ascii_lowercase();
    if !valid_sha256_hex(&sha256) {
        return Err(ApiError::bad_request("the SHA-256 must be 64 hex digits"));
    }
    let operation = FileOperation::FinishUpload {
        upload_id: upload.id.clone(),
        path: upload.path.clone(),
        size: upload.size,
        sha256: sha256.clone(),
        overwrite: upload.overwrite,
    };
    let finished = files::ask(&state, &actor, &upload.device_id, upload.run_as, operation).await;
    if let Err(error) = &finished
        && error.code() == Some("checksum_mismatch")
    {
        // The device discarded the bytes; the upload starts over.
        state.files.remove_upload(&upload.id);
    }
    let view = entry_view(finished?)?;
    state.files.remove_upload(&upload.id);
    relay::record(
        &state,
        &actor.actor(),
        &upload.device_id,
        "files.upload",
        json!({
            "path": view.path,
            "size": upload.size,
            "sha256": sha256,
            "overwrite": upload.overwrite,
            "run_as": upload.run_as.as_str(),
        }),
    )
    .await;
    Ok(Json(view))
}

/// `DELETE /v1/file-uploads/{id}`: stops an upload and removes what the
/// device has of it.
pub async fn cancel_upload(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let upload = own_upload(&state, &actor, &id)?;
    state.files.remove_upload(&upload.id);
    if state.agents.is_connected(&upload.device_id) {
        let operation = FileOperation::CancelUpload {
            upload_id: upload.id.clone(),
            path: upload.path.clone(),
        };
        files::ask(&state, &actor, &upload.device_id, upload.run_as, operation).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/agents/{id}/file-transfers/{transfer_id}`: the Agent's socket
/// for a transfer the server asked it for.
pub async fn agent_connect(
    State(state): State<AppState>,
    Path((device_id, transfer_id)): Path<(String, String)>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let agent = agents::authenticate(&state, &headers, &device_id).await?;
    let transfer_id = parse_id(&transfer_id, "transfer")?;
    let waiting = (!agent.deletion_requested)
        .then(|| state.files.claim_agent(&transfer_id, &agent.device_id))
        .flatten()
        .ok_or_else(|| {
            ApiError::not_found("the transfer has ended").with_code("transfer_not_found")
        })?;
    Ok(upgrade
        .max_message_size(MAX_FILE_STREAM_MESSAGE_BYTES + 1024)
        .on_upgrade(move |socket| async move {
            // The request may have given up waiting; dropping the socket
            // tells the Agent.
            let _ = waiting.send(socket);
        }))
}
