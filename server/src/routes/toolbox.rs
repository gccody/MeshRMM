//! The toolbox: scripts and library files a company keeps, each private to
//! the user who added it or shared with the whole company, and their runs
//! and deliveries on devices. See docs/toolbox.md.
//!
//! Dashboard users manage the toolbox and run scripts with their WorkOS
//! token. A remote session's viewer uses its technician's toolbox with the
//! session's client token. Either way the server hands the run or delivery
//! to the Agent over its coordinator connection, and the Agent reports back
//! with its own credential.
use std::collections::HashMap;

use meshrmm_protocol_types::{
    AgentCommand, FileDelivery, FileDeliveryDestination, FileDeliveryReport, FileDeliveryRequest,
    FileDeliveryStatus, MAX_SCRIPT_OUTPUT_BYTES, MAX_TOOLBOX_FILE_BYTES, RunAs, ScriptLanguage,
    ScriptRun, ScriptRunReport, ScriptRunRequest, ScriptRunStatus, StartFileDelivery,
    StartScriptRun, ToolboxFile, ToolboxListing, ToolboxScript, normalize_folder, truncate_output,
    valid_file_name, valid_script_body, valid_script_description, valid_script_name,
    valid_script_timeout, valid_sha256_hex,
};

use crate::remote_session::SessionIdentity;
use crate::*;

/// The R2 bucket binding that stores library files.
const TOOLBOX_BUCKET: &str = "TOOLBOX";
/// A run the Agent has not reported this long after its timeout is lost.
const RUN_REPORT_GRACE_MS: i64 = 2 * 60 * 1000;
/// A delivery the Agent has not reported in this long is lost, and the
/// Agent may no longer download its file.
const DELIVERY_WINDOW_MS: i64 = 30 * 60 * 1000;
/// An Agent's report: two output streams at their limit, escaped as JSON.
const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
/// Recent runs the dashboard lists.
const MAX_LISTED_RUNS: u32 = 50;
const MAX_RAN_AS_CHARS: usize = 256;
const MAX_REPORTED_ERROR_CHARS: usize = 2000;
const MAX_PATH_CHARS: usize = 1024;

/// Keys start with the company, so a company's files share a prefix.
pub(crate) fn toolbox_key(company_id: &str, file_id: &str) -> String {
    format!("toolbox/{company_id}/{file_id}")
}

/// Who is using the toolbox. Only dashboard users may change it, and an
/// administrator may change anything shared with the company.
struct ToolboxUser {
    company_id: String,
    user_id: String,
    admin: bool,
}

impl ToolboxUser {
    fn dashboard(identity: &Identity) -> Self {
        Self {
            company_id: identity.company_id.clone(),
            user_id: identity.user_id.clone(),
            admin: identity.is_company_admin(),
        }
    }

    fn session(identity: &SessionIdentity) -> Self {
        Self {
            company_id: identity.company_id.clone(),
            user_id: identity.user_id.clone(),
            admin: false,
        }
    }

    fn can_edit(&self, owner_user_id: &str, shared: bool) -> bool {
        owner_user_id == self.user_id || (shared && self.admin)
    }
}

#[derive(Debug, Deserialize)]
struct ScriptRow {
    id: String,
    owner_user_id: String,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    shared: bool,
    folder: String,
    name: String,
    description: String,
    language: String,
    timeout_seconds: u32,
    created_at: i64,
    updated_at: i64,
    #[serde(default)]
    body: Option<String>,
}

/// A script as the dashboard shows it. The body is left out of lists.
#[derive(Debug, Serialize)]
struct ScriptView {
    id: String,
    name: String,
    folder: String,
    description: String,
    language: String,
    timeout_seconds: u32,
    shared: bool,
    /// The user added it.
    owned: bool,
    can_edit: bool,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

impl ScriptRow {
    fn view(self, user: &ToolboxUser) -> ScriptView {
        ScriptView {
            owned: self.owner_user_id == user.user_id,
            can_edit: user.can_edit(&self.owner_user_id, self.shared),
            id: self.id,
            name: self.name,
            folder: self.folder,
            description: self.description,
            language: self.language,
            timeout_seconds: self.timeout_seconds,
            shared: self.shared,
            created_at_unix_ms: self.created_at,
            updated_at_unix_ms: self.updated_at,
            body: self.body,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FileRow {
    id: String,
    owner_user_id: String,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    shared: bool,
    folder: String,
    name: String,
    size_bytes: u64,
    sha256: String,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Serialize)]
struct FileView {
    id: String,
    name: String,
    folder: String,
    size_bytes: u64,
    sha256: String,
    shared: bool,
    owned: bool,
    can_edit: bool,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

impl FileRow {
    fn view(self, user: &ToolboxUser) -> FileView {
        FileView {
            owned: self.owner_user_id == user.user_id,
            can_edit: user.can_edit(&self.owner_user_id, self.shared),
            id: self.id,
            name: self.name,
            folder: self.folder,
            size_bytes: self.size_bytes,
            sha256: self.sha256,
            shared: self.shared,
            created_at_unix_ms: self.created_at,
            updated_at_unix_ms: self.updated_at,
        }
    }
}

fn default_timeout_seconds() -> u32 {
    meshrmm_protocol_types::DEFAULT_SCRIPT_TIMEOUT_SECONDS
}

/// A script as the dashboard saves it, whole.
#[derive(Debug, Deserialize)]
struct ScriptInput {
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
    fn normalized(mut self) -> std::result::Result<Self, &'static str> {
        self.name = self.name.trim().to_owned();
        if !valid_script_name(&self.name) {
            return Err("script names must be 1 to 120 characters on one line");
        }
        self.folder = normalize_folder(&self.folder)
            .ok_or("folders may be at most 8 levels deep, with names of at most 64 characters")?;
        self.description = self.description.trim().to_owned();
        if !valid_script_description(&self.description) {
            return Err("descriptions must be at most 1000 bytes");
        }
        if !valid_script_body(&self.body) {
            return Err("scripts must not be empty and must be at most 128 KiB");
        }
        if !valid_script_timeout(self.timeout_seconds) {
            return Err("the timeout must be between 10 and 3600 seconds");
        }
        Ok(self)
    }
}

/// A library file's details as the dashboard saves them.
#[derive(Debug, Deserialize)]
struct FileInput {
    name: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    shared: bool,
}

const INVALID_FILE_NAME: &str = "file names must be at most 255 characters and ones Windows allows: no \\ / : * ? \" < > |, \
     no trailing dot or space, and not a device name such as CON";
const INVALID_FOLDER: &str =
    "folders may be at most 8 levels deep, with names of at most 64 characters";

async fn dashboard_user(
    request: &Request,
    environment: &Env,
) -> Result<std::result::Result<ToolboxUser, Response>> {
    match authorize_workos_user(request, environment).await {
        Ok(identity) => Ok(Ok(ToolboxUser::dashboard(&identity))),
        Err(error) => workos_auth_error(error).map(Err),
    }
}

/// The technician of a live session, from the viewer's client token.
async fn session_user(
    request: &Request,
    environment: &Env,
    session_id: &str,
) -> Result<std::result::Result<(ToolboxUser, String), Response>> {
    if Uuid::parse_str(session_id).is_err() {
        return api_error(400, "invalid session ID").map(Err);
    }
    let headers = Headers::new();
    if let Some(authorization) = request.headers().get("Authorization")? {
        headers.set("Authorization", &authorization)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let lookup = Request::new_with_init("https://session.internal/identity", &init)?;
    let mut response = object_stub(environment, "REMOTE_SESSION", session_id)?
        .fetch_with_request(lookup)
        .await?;
    match response.status_code() {
        200 => {}
        401 => return api_error(401, "remote session authentication failed").map(Err),
        404 | 410 => return api_error(410, "the remote session has ended").map(Err),
        status => {
            console_error!("event=session_identity_failed status={}", status);
            return api_error(503, "the remote session could not be verified; try again").map(Err);
        }
    }
    let identity: SessionIdentity = response.json().await?;
    if !device_is_active(environment, &identity.device_id).await? {
        return api_error(410, "the remote session has ended").map(Err);
    }
    crate::usage::attribute_company(&identity.company_id);
    Ok(Ok((ToolboxUser::session(&identity), identity.device_id)))
}

macro_rules! authorized {
    ($lookup:expr) => {
        match $lookup.await? {
            Ok(user) => user,
            Err(response) => return Ok(response),
        }
    };
}

async fn json_body<T: serde::de::DeserializeOwned>(
    request: &mut Request,
    message: &str,
) -> Result<std::result::Result<T, Response>> {
    match request.json().await {
        Ok(body) => Ok(Ok(body)),
        Err(_) => api_error(400, message).map(Err),
    }
}

macro_rules! body {
    ($request:expr, $message:expr) => {
        match json_body($request, $message).await? {
            Ok(body) => body,
            Err(response) => return Ok(response),
        }
    };
}

fn changes(result: &D1Result) -> usize {
    result
        .meta()
        .ok()
        .flatten()
        .and_then(|meta| meta.changes)
        .unwrap_or_default()
}

/// Audits a run or delivery in the batch that creates it. `sql` inserts the
/// audit event only if `?9` names a row that now exists.
#[allow(clippy::too_many_arguments)]
fn audit_if_created(
    db: &D1Database,
    sql: &str,
    user: &ToolboxUser,
    action: &str,
    device_id: &str,
    metadata: serde_json::Value,
    created_id: &str,
) -> Result<D1PreparedStatement> {
    query!(
        db,
        sql,
        Uuid::new_v4().to_string(),
        user.company_id,
        user.user_id,
        action,
        "agent",
        device_id,
        metadata.to_string(),
        now_ms_i64()?,
        created_id
    )
}

fn toolbox_audit(
    db: &D1Database,
    user: &ToolboxUser,
    action: &str,
    target_type: &str,
    target_id: &str,
    metadata: serde_json::Value,
) -> Result<D1PreparedStatement> {
    query!(
        db,
        "INSERT INTO audit_events (id, company_id, actor_user_id, action, target_type, target_id, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        Uuid::new_v4().to_string(),
        user.company_id,
        user.user_id,
        action,
        target_type,
        target_id,
        metadata.to_string(),
        now_ms_i64()?
    )
}

async fn visible_scripts(db: &D1Database, user: &ToolboxUser) -> Result<Vec<ScriptRow>> {
    query!(
        db,
        "SELECT id, owner_user_id, shared, folder, name, description, language, timeout_seconds, created_at, updated_at FROM toolbox_scripts WHERE company_id = ?1 AND (shared = 1 OR owner_user_id = ?2) ORDER BY folder COLLATE NOCASE, name COLLATE NOCASE",
        user.company_id,
        user.user_id
    )?
    .metered_all()
    .await?
    .results::<ScriptRow>()
}

async fn visible_script(
    db: &D1Database,
    user: &ToolboxUser,
    script_id: &str,
) -> Result<Option<ScriptRow>> {
    query!(
        db,
        "SELECT id, owner_user_id, shared, folder, name, description, language, timeout_seconds, created_at, updated_at, body FROM toolbox_scripts WHERE id = ?1 AND company_id = ?2 AND (shared = 1 OR owner_user_id = ?3)",
        script_id,
        user.company_id,
        user.user_id
    )?
    .metered_first::<ScriptRow>(None)
    .await
}

async fn visible_files(db: &D1Database, user: &ToolboxUser) -> Result<Vec<FileRow>> {
    query!(
        db,
        "SELECT id, owner_user_id, shared, folder, name, size_bytes, sha256, created_at, updated_at FROM toolbox_files WHERE company_id = ?1 AND (shared = 1 OR owner_user_id = ?2) ORDER BY folder COLLATE NOCASE, name COLLATE NOCASE",
        user.company_id,
        user.user_id
    )?
    .metered_all()
    .await?
    .results::<FileRow>()
}

async fn visible_file(
    db: &D1Database,
    user: &ToolboxUser,
    file_id: &str,
) -> Result<Option<FileRow>> {
    query!(
        db,
        "SELECT id, owner_user_id, shared, folder, name, size_bytes, sha256, created_at, updated_at FROM toolbox_files WHERE id = ?1 AND company_id = ?2 AND (shared = 1 OR owner_user_id = ?3)",
        file_id,
        user.company_id,
        user.user_id
    )?
    .metered_first::<FileRow>(None)
    .await
}

/// `GET /v1/toolbox`: the scripts and files the user may use.
pub(crate) async fn list_toolbox(request: &Request, environment: &Env) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    let db = environment.d1("DB")?;
    let scripts: Vec<_> = visible_scripts(&db, &user)
        .await?
        .into_iter()
        .map(|row| row.view(&user))
        .collect();
    let files: Vec<_> = visible_files(&db, &user)
        .await?
        .into_iter()
        .map(|row| row.view(&user))
        .collect();
    Response::from_json(&serde_json::json!({ "scripts": scripts, "files": files }))
}

/// `GET /v1/toolbox/scripts/{id}`, with its body.
pub(crate) async fn get_toolbox_script(
    request: &Request,
    environment: &Env,
    script_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(script_id, "script ID").is_err() {
        return api_error(400, "invalid script ID");
    }
    let db = environment.d1("DB")?;
    match visible_script(&db, &user, script_id).await? {
        Some(row) => Response::from_json(&row.view(&user)),
        None => api_error(404, "Script not found"),
    }
}

/// `POST /v1/toolbox/scripts`.
pub(crate) async fn create_toolbox_script(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    let input: ScriptInput = body!(request, "invalid script");
    let input = match input.normalized() {
        Ok(input) => input,
        Err(message) => return api_error(400, message),
    };
    let db = environment.d1("DB")?;
    let id = Uuid::new_v4().to_string();
    let now = now_ms_i64()?;
    let insert = query!(
        &db,
        "INSERT INTO toolbox_scripts (id, company_id, owner_user_id, shared, folder, name, description, language, body, timeout_seconds, created_at, updated_at, updated_by_user_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?3)",
        id,
        user.company_id,
        user.user_id,
        input.shared,
        input.folder,
        input.name,
        input.description,
        input.language.as_str(),
        input.body,
        input.timeout_seconds,
        now
    )?;
    let audit = toolbox_audit(
        &db,
        &user,
        "toolbox.script_create",
        "toolbox_script",
        &id,
        serde_json::json!({ "name": input.name, "shared": input.shared }),
    )?;
    metered_batch(&db, vec![insert, audit]).await?;
    let row = visible_script(&db, &user, &id)
        .await?
        .ok_or_else(|| Error::RustError("the new script was not stored".into()))?;
    Ok(Response::from_json(&row.view(&user))?.with_status(201))
}

/// `PUT /v1/toolbox/scripts/{id}`: replaces the script. Its owner may change
/// it, and so may an administrator while it is shared.
pub(crate) async fn update_toolbox_script(
    request: &mut Request,
    environment: &Env,
    script_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(script_id, "script ID").is_err() {
        return api_error(400, "invalid script ID");
    }
    let input: ScriptInput = body!(request, "invalid script");
    let input = match input.normalized() {
        Ok(input) => input,
        Err(message) => return api_error(400, message),
    };
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "UPDATE toolbox_scripts SET shared = ?1, folder = ?2, name = ?3, description = ?4, language = ?5, body = ?6, timeout_seconds = ?7, updated_at = ?8, updated_by_user_id = ?9 WHERE id = ?10 AND company_id = ?11 AND (owner_user_id = ?9 OR (shared = 1 AND ?12 = 1))",
        input.shared,
        input.folder,
        input.name,
        input.description,
        input.language.as_str(),
        input.body,
        input.timeout_seconds,
        now_ms_i64()?,
        user.user_id,
        script_id,
        user.company_id,
        user.admin
    )?
    .metered_run()
    .await?;
    if changes(&result) == 0 {
        return api_error(404, "Script not found, or you may not change it");
    }
    toolbox_audit(
        &db,
        &user,
        "toolbox.script_update",
        "toolbox_script",
        script_id,
        serde_json::json!({ "name": input.name, "shared": input.shared }),
    )?
    .metered_run()
    .await?;
    // An administrator who unshares someone else's script can no longer see it.
    match visible_script(&db, &user, script_id).await? {
        Some(row) => Response::from_json(&row.view(&user)),
        None => Response::empty().map(|response| response.with_status(204)),
    }
}

/// `DELETE /v1/toolbox/scripts/{id}`. Past runs keep the script's name.
pub(crate) async fn delete_toolbox_script(
    request: &Request,
    environment: &Env,
    script_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(script_id, "script ID").is_err() {
        return api_error(400, "invalid script ID");
    }
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "DELETE FROM toolbox_scripts WHERE id = ?1 AND company_id = ?2 AND (owner_user_id = ?3 OR (shared = 1 AND ?4 = 1))",
        script_id,
        user.company_id,
        user.user_id,
        user.admin
    )?
    .metered_run()
    .await?;
    if changes(&result) == 0 {
        return api_error(404, "Script not found, or you may not delete it");
    }
    toolbox_audit(
        &db,
        &user,
        "toolbox.script_delete",
        "toolbox_script",
        script_id,
        serde_json::json!({}),
    )?
    .metered_run()
    .await?;
    Response::empty().map(|response| response.with_status(204))
}

fn decode_sha256(hex: &str) -> Option<Vec<u8>> {
    if !valid_sha256_hex(hex) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
        .collect()
}

/// `POST /v1/toolbox/files?name=&folder=&shared=&sha256=` with the file as
/// the body. R2 checks the content against the SHA-256 the browser computed.
pub(crate) async fn upload_toolbox_file(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    let parameters: HashMap<String, String> = request.url()?.query_pairs().into_owned().collect();
    let parameter = |name: &str| parameters.get(name).map(String::as_str).unwrap_or_default();
    let name = parameter("name").trim();
    if !valid_file_name(name) {
        return api_error(400, INVALID_FILE_NAME);
    }
    let Some(folder) = normalize_folder(parameter("folder")) else {
        return api_error(400, INVALID_FOLDER);
    };
    let shared = matches!(parameter("shared"), "1" | "true");
    let sha256 = parameter("sha256").to_ascii_lowercase();
    let Some(digest) = decode_sha256(&sha256) else {
        return api_error(400, "the upload needs the file's SHA-256");
    };
    let Some(size) = request
        .headers()
        .get("Content-Length")?
        .and_then(|length| length.parse::<u64>().ok())
    else {
        return api_error(411, "the upload needs a Content-Length");
    };
    if size > MAX_TOOLBOX_FILE_BYTES {
        return api_error(413, "toolbox files may be at most 95 MiB");
    }
    // R2 needs the length up front. A body forwarded through the dashboard's
    // service binding may not carry it, so the declared length is enforced.
    let data = if size == 0 {
        Data::Empty
    } else {
        match request.stream() {
            Ok(stream) => Data::Stream(FixedLengthStream::wrap(stream, size)),
            Err(_) => return api_error(400, "the upload has no content"),
        }
    };
    let id = Uuid::new_v4().to_string();
    let key = toolbox_key(&user.company_id, &id);
    let bucket = environment.bucket(TOOLBOX_BUCKET)?;
    let stored = bucket
        .put(&key, data)
        .sha256(digest)
        .http_metadata(HttpMetadata {
            content_type: Some("application/octet-stream".into()),
            ..Default::default()
        })
        .execute()
        .await;
    match stored {
        Ok(Some(object)) if object.size() == size => {}
        Ok(_) => {
            let _ = bucket.delete(&key).await;
            return api_error(400, "the upload was incomplete; try again");
        }
        Err(error) => {
            console_error!("event=toolbox_upload_failed error={}", error);
            return api_error(
                400,
                "the upload did not match its checksum or was interrupted; try again",
            );
        }
    }
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let insert = query!(
        &db,
        "INSERT INTO toolbox_files (id, company_id, owner_user_id, shared, folder, name, size_bytes, sha256, created_at, updated_at, updated_by_user_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?3)",
        id,
        user.company_id,
        user.user_id,
        shared,
        folder,
        name,
        size as f64,
        sha256,
        now
    )?;
    let audit = toolbox_audit(
        &db,
        &user,
        "toolbox.file_upload",
        "toolbox_file",
        &id,
        serde_json::json!({ "name": name, "size_bytes": size, "shared": shared }),
    )?;
    if let Err(error) = metered_batch(&db, vec![insert, audit]).await {
        let _ = bucket.delete(&key).await;
        return Err(error);
    }
    let row = visible_file(&db, &user, &id)
        .await?
        .ok_or_else(|| Error::RustError("the new file was not stored".into()))?;
    Ok(Response::from_json(&row.view(&user))?.with_status(201))
}

/// `PUT /v1/toolbox/files/{id}`: renames, moves or shares a file. Its
/// content cannot change; upload a new file instead.
pub(crate) async fn update_toolbox_file(
    request: &mut Request,
    environment: &Env,
    file_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(file_id, "file ID").is_err() {
        return api_error(400, "invalid file ID");
    }
    let input: FileInput = body!(request, "invalid file details");
    let name = input.name.trim();
    if !valid_file_name(name) {
        return api_error(400, INVALID_FILE_NAME);
    }
    let Some(folder) = normalize_folder(&input.folder) else {
        return api_error(400, INVALID_FOLDER);
    };
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "UPDATE toolbox_files SET shared = ?1, folder = ?2, name = ?3, updated_at = ?4, updated_by_user_id = ?5 WHERE id = ?6 AND company_id = ?7 AND (owner_user_id = ?5 OR (shared = 1 AND ?8 = 1))",
        input.shared,
        folder,
        name,
        now_ms_i64()?,
        user.user_id,
        file_id,
        user.company_id,
        user.admin
    )?
    .metered_run()
    .await?;
    if changes(&result) == 0 {
        return api_error(404, "File not found, or you may not change it");
    }
    toolbox_audit(
        &db,
        &user,
        "toolbox.file_update",
        "toolbox_file",
        file_id,
        serde_json::json!({ "name": name, "shared": input.shared }),
    )?
    .metered_run()
    .await?;
    match visible_file(&db, &user, file_id).await? {
        Some(row) => Response::from_json(&row.view(&user)),
        None => Response::empty().map(|response| response.with_status(204)),
    }
}

/// `DELETE /v1/toolbox/files/{id}`.
pub(crate) async fn delete_toolbox_file(
    request: &Request,
    environment: &Env,
    file_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(file_id, "file ID").is_err() {
        return api_error(400, "invalid file ID");
    }
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "DELETE FROM toolbox_files WHERE id = ?1 AND company_id = ?2 AND (owner_user_id = ?3 OR (shared = 1 AND ?4 = 1))",
        file_id,
        user.company_id,
        user.user_id,
        user.admin
    )?
    .metered_run()
    .await?;
    if changes(&result) == 0 {
        return api_error(404, "File not found, or you may not delete it");
    }
    toolbox_audit(
        &db,
        &user,
        "toolbox.file_delete",
        "toolbox_file",
        file_id,
        serde_json::json!({}),
    )?
    .metered_run()
    .await?;
    // The row is gone, so nothing can reach the content even if this fails.
    if let Err(error) = environment
        .bucket(TOOLBOX_BUCKET)?
        .delete(toolbox_key(&user.company_id, file_id))
        .await
    {
        console_error!("event=toolbox_file_delete_failed error={}", error);
    }
    Response::empty().map(|response| response.with_status(204))
}

/// A stored file's content as a download.
async fn file_content(
    environment: &Env,
    company_id: &str,
    file_id: &str,
) -> Result<Option<Response>> {
    let Some(object) = environment
        .bucket(TOOLBOX_BUCKET)?
        .get(toolbox_key(company_id, file_id))
        .execute()
        .await?
    else {
        return Ok(None);
    };
    let size = object.size();
    let Some(body) = object.body() else {
        return Ok(None);
    };
    let headers = Headers::new();
    headers.set("Content-Type", "application/octet-stream")?;
    headers.set("Content-Length", &size.to_string())?;
    headers.set("Content-Disposition", "attachment")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set("Cache-Control", "private, no-store")?;
    Ok(Some(
        Response::from_body(body.response_body()?)?.with_headers(headers),
    ))
}

/// `GET /v1/toolbox/files/{id}/content`, for the dashboard.
pub(crate) async fn download_toolbox_file(
    request: &Request,
    environment: &Env,
    file_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(file_id, "file ID").is_err() {
        return api_error(400, "invalid file ID");
    }
    let db = environment.d1("DB")?;
    if visible_file(&db, &user, file_id).await?.is_none() {
        return api_error(404, "File not found");
    }
    match file_content(environment, &user.company_id, file_id).await? {
        Some(response) => Ok(response),
        None => api_error(404, "File not found"),
    }
}

#[derive(Debug, Deserialize)]
struct RunRow {
    id: String,
    device_id: String,
    script_id: String,
    script_name: String,
    language: String,
    run_as: String,
    status: String,
    ran_as: Option<String>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    #[serde(deserialize_with = "deserialize_sql_bool")]
    output_truncated: bool,
    error: Option<String>,
    timeout_seconds: u32,
    source: String,
    created_at: i64,
    completed_at: Option<i64>,
    requested_by_user_id: String,
}

/// A run as the dashboard shows it.
#[derive(Debug, Serialize)]
struct RunView {
    #[serde(flatten)]
    run: ScriptRun,
    /// `dashboard` or `session`.
    source: String,
    requested_by_you: bool,
}

/// The status to show for a run stored as `status`: one the Agent has not
/// reported long after its timeout is lost.
fn run_status(
    status: ScriptRunStatus,
    created_at: i64,
    timeout_seconds: u32,
    now: i64,
) -> ScriptRunStatus {
    let deadline = created_at
        .saturating_add(i64::from(timeout_seconds) * 1000)
        .saturating_add(RUN_REPORT_GRACE_MS);
    if status == ScriptRunStatus::Pending && now > deadline {
        ScriptRunStatus::Lost
    } else {
        status
    }
}

fn unix_ms(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

impl RunRow {
    fn into_run(self, now: i64) -> Result<(ScriptRun, String, String)> {
        let stored = ScriptRunStatus::parse(&self.status)
            .ok_or_else(|| Error::RustError("invalid stored run status".into()))?;
        let run = ScriptRun {
            status: run_status(stored, self.created_at, self.timeout_seconds, now),
            language: ScriptLanguage::parse(&self.language)
                .ok_or_else(|| Error::RustError("invalid stored script language".into()))?,
            run_as: RunAs::parse(&self.run_as)
                .ok_or_else(|| Error::RustError("invalid stored run account".into()))?,
            id: self.id,
            device_id: self.device_id,
            script_id: self.script_id,
            script_name: self.script_name,
            ran_as: self.ran_as,
            exit_code: self.exit_code,
            stdout: self.stdout,
            stderr: self.stderr,
            output_truncated: self.output_truncated,
            error: self.error,
            created_at_unix_ms: unix_ms(self.created_at),
            completed_at_unix_ms: self.completed_at.map(unix_ms),
        };
        Ok((run, self.source, self.requested_by_user_id))
    }
}

async fn stored_run(db: &D1Database, company_id: &str, run_id: &str) -> Result<Option<RunRow>> {
    query!(
        db,
        "SELECT id, device_id, script_id, script_name, language, run_as, status, ran_as, exit_code, stdout, stderr, output_truncated, error, timeout_seconds, source, created_at, completed_at, requested_by_user_id FROM script_runs WHERE id = ?1 AND company_id = ?2",
        run_id,
        company_id
    )?
    .metered_first::<RunRow>(None)
    .await
}

/// Starts `start`'s script on `device_id` for `user`, and returns the run.
async fn start_script_run(
    environment: &Env,
    user: &ToolboxUser,
    device_id: &str,
    start: StartScriptRun,
    source: &str,
) -> Result<std::result::Result<ScriptRun, Response>> {
    if validate_identifier(&start.script_id, "script ID").is_err() {
        return api_error(400, "invalid script ID").map(Err);
    }
    let db = environment.d1("DB")?;
    let Some(script) = visible_script(&db, user, &start.script_id).await? else {
        return api_error(404, "Script not found").map(Err);
    };
    let Some(body) = script.body else {
        return Err(Error::RustError("the script body was not loaded".into()));
    };
    let language = ScriptLanguage::parse(&script.language)
        .ok_or_else(|| Error::RustError("invalid stored script language".into()))?;
    let run_id = Uuid::new_v4().to_string();
    let now = now_ms_i64()?;
    let insert = query!(
        &db,
        "INSERT INTO script_runs (id, company_id, device_id, script_id, script_name, language, requested_by_user_id, source, run_as, timeout_seconds, created_at) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11 WHERE EXISTS (SELECT 1 FROM agents WHERE id = ?3 AND company_id = ?2 AND deletion_requested_at IS NULL)",
        run_id,
        user.company_id,
        device_id,
        script.id,
        script.name,
        language.as_str(),
        user.user_id,
        source,
        start.run_as.as_str(),
        script.timeout_seconds,
        now
    )?;
    let audit = audit_if_created(
        &db,
        "INSERT INTO audit_events (id, company_id, actor_user_id, action, target_type, target_id, metadata_json, created_at) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 WHERE EXISTS (SELECT 1 FROM script_runs WHERE id = ?9)",
        user,
        "script.run",
        device_id,
        serde_json::json!({
            "run_id": run_id,
            "script_id": script.id,
            "script_name": script.name,
            "run_as": start.run_as.as_str(),
            "source": source,
        }),
        &run_id,
    )?;
    let results = metered_batch(&db, vec![insert, audit]).await?;
    if results.first().map(changes) != Some(1) {
        return api_error(404, "Agent not found").map(Err);
    }
    let command = AgentCommand::RunScript {
        run: ScriptRunRequest {
            run_id: run_id.clone(),
            language,
            body,
            run_as: start.run_as,
            timeout_seconds: script.timeout_seconds,
        },
    };
    if !crate::agent_coordinator::send_command(environment, device_id, &command).await? {
        query!(
            &db,
            "UPDATE script_runs SET status = 'failed', error = ?1, completed_at = ?2 WHERE id = ?3 AND status = 'pending'",
            "The device is offline, so the script did not run.",
            now_ms_i64()?,
            run_id
        )?
        .metered_run()
        .await?;
        return api_error(409, "The device is offline, so the script did not run.").map(Err);
    }
    Ok(Ok(ScriptRun {
        id: run_id,
        device_id: device_id.to_owned(),
        script_id: script.id,
        script_name: script.name,
        language,
        run_as: start.run_as,
        status: ScriptRunStatus::Pending,
        ran_as: None,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        output_truncated: false,
        error: None,
        created_at_unix_ms: unix_ms(now),
        completed_at_unix_ms: None,
    }))
}

/// `POST /v1/agents/{device_id}/script-runs`: runs a script from the dashboard.
pub(crate) async fn run_script_on_agent(
    request: &mut Request,
    environment: &Env,
    device_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(device_id, "device ID").is_err() {
        return api_error(400, "invalid device ID");
    }
    let start: StartScriptRun = body!(request, "invalid script run");
    match start_script_run(environment, &user, device_id, start, "dashboard").await? {
        Ok(run) => Ok(Response::from_json(&RunView {
            run,
            source: "dashboard".into(),
            requested_by_you: true,
        })?
        .with_status(201)),
        Err(response) => Ok(response),
    }
}

/// `GET /v1/script-runs?device_id=`: recent runs, newest first. Users see
/// their own runs; administrators see everyone's.
pub(crate) async fn list_script_runs(request: &Request, environment: &Env) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    let device_id = request
        .url()?
        .query_pairs()
        .find(|(name, _)| name == "device_id")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    if !device_id.is_empty() && validate_identifier(&device_id, "device ID").is_err() {
        return api_error(400, "invalid device ID");
    }
    let db = environment.d1("DB")?;
    let rows = query!(
        &db,
        "SELECT id, device_id, script_id, script_name, language, run_as, status, ran_as, exit_code, '' AS stdout, '' AS stderr, output_truncated, error, timeout_seconds, source, created_at, completed_at, requested_by_user_id FROM script_runs WHERE company_id = ?1 AND (?2 = '' OR device_id = ?2) AND (?3 = 1 OR requested_by_user_id = ?4) ORDER BY created_at DESC LIMIT ?5",
        user.company_id,
        device_id,
        user.admin,
        user.user_id,
        MAX_LISTED_RUNS
    )?
    .metered_all()
    .await?
    .results::<RunRow>()?;
    let now = now_ms_i64()?;
    let runs = rows
        .into_iter()
        .map(|row| {
            let (run, source, requester) = row.into_run(now)?;
            Ok(RunView {
                run,
                source,
                requested_by_you: requester == user.user_id,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Response::from_json(&serde_json::json!({ "runs": runs }))
}

/// `GET /v1/script-runs/{id}`, with the run's output.
pub(crate) async fn get_script_run(
    request: &Request,
    environment: &Env,
    run_id: &str,
) -> Result<Response> {
    let user = authorized!(dashboard_user(request, environment));
    if validate_identifier(run_id, "run ID").is_err() {
        return api_error(400, "invalid run ID");
    }
    let db = environment.d1("DB")?;
    let Some(row) = stored_run(&db, &user.company_id, run_id).await? else {
        return api_error(404, "Run not found");
    };
    if row.requested_by_user_id != user.user_id && !user.admin {
        return api_error(404, "Run not found");
    }
    let (run, source, requester) = row.into_run(now_ms_i64()?)?;
    Response::from_json(&RunView {
        run,
        source,
        requested_by_you: requester == user.user_id,
    })
}

/// Reads an Agent's JSON report, refusing one too large to be real.
async fn agent_report<T: serde::de::DeserializeOwned>(
    request: &mut Request,
) -> Result<std::result::Result<T, Response>> {
    let declared = request
        .headers()
        .get("Content-Length")?
        .and_then(|length| length.parse::<usize>().ok());
    if declared.is_some_and(|length| length > MAX_REPORT_BYTES) {
        return api_error(413, "the report is too large").map(Err);
    }
    let body = request.bytes().await?;
    if body.len() > MAX_REPORT_BYTES {
        return api_error(413, "the report is too large").map(Err);
    }
    match serde_json::from_slice(&body) {
        Ok(report) => Ok(Ok(report)),
        Err(_) => api_error(400, "invalid report").map(Err),
    }
}

fn bounded(text: Option<String>, max_chars: usize) -> Option<String> {
    text.map(|text| text.chars().take(max_chars).collect())
}

/// `POST /v1/agents/{device_id}/script-runs/{id}/result`, from the Agent.
pub(crate) async fn report_script_run(
    request: &mut Request,
    environment: &Env,
    device_id: &str,
    run_id: &str,
) -> Result<Response> {
    let authorization = match authorize_agent(request, environment, device_id).await {
        Ok(authorization) => authorization,
        Err(_) => return api_error(401, "Agent authentication failed"),
    };
    if validate_identifier(run_id, "run ID").is_err() {
        return api_error(400, "invalid run ID");
    }
    let report: ScriptRunReport = match agent_report(request).await? {
        Ok(report) => report,
        Err(response) => return Ok(response),
    };
    if !matches!(
        report.status,
        ScriptRunStatus::Completed | ScriptRunStatus::Failed | ScriptRunStatus::TimedOut
    ) {
        return api_error(400, "a run reports how it finished");
    }
    let (stdout, stdout_cut) = truncate_output(&report.stdout, MAX_SCRIPT_OUTPUT_BYTES);
    let (stderr, stderr_cut) = truncate_output(&report.stderr, MAX_SCRIPT_OUTPUT_BYTES);
    let ran_as: String = report.ran_as.chars().take(MAX_RAN_AS_CHARS).collect();
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "UPDATE script_runs SET status = ?1, ran_as = ?2, exit_code = ?3, stdout = ?4, stderr = ?5, output_truncated = ?6, error = ?7, completed_at = ?8 WHERE id = ?9 AND device_id = ?10 AND company_id = ?11 AND status = 'pending'",
        report.status.as_str(),
        ran_as,
        report.exit_code,
        stdout,
        stderr,
        report.output_truncated || stdout_cut || stderr_cut,
        bounded(report.error, MAX_REPORTED_ERROR_CHARS),
        now_ms_i64()?,
        run_id,
        device_id,
        authorization.company_id
    )?
    .metered_run()
    .await?;
    if changes(&result) == 0 {
        return api_error(404, "no pending run has this ID");
    }
    Response::empty().map(|response| response.with_status(204))
}

#[derive(Debug, Deserialize)]
struct DeliveryRow {
    id: String,
    device_id: String,
    file_id: String,
    file_name: String,
    size_bytes: u64,
    status: String,
    path: Option<String>,
    error: Option<String>,
    created_at: i64,
    completed_at: Option<i64>,
    requested_by_user_id: String,
}

impl DeliveryRow {
    fn into_delivery(self, now: i64) -> Result<FileDelivery> {
        let stored = FileDeliveryStatus::parse(&self.status)
            .ok_or_else(|| Error::RustError("invalid stored delivery status".into()))?;
        let status = if stored == FileDeliveryStatus::Pending
            && now > self.created_at.saturating_add(DELIVERY_WINDOW_MS)
        {
            FileDeliveryStatus::Lost
        } else {
            stored
        };
        Ok(FileDelivery {
            id: self.id,
            device_id: self.device_id,
            file_id: self.file_id,
            file_name: self.file_name,
            size_bytes: self.size_bytes,
            status,
            path: self.path,
            error: self.error,
            created_at_unix_ms: unix_ms(self.created_at),
            completed_at_unix_ms: self.completed_at.map(unix_ms),
        })
    }
}

/// Sends a library file to `device_id` for `user`, and returns the delivery.
async fn start_file_delivery(
    environment: &Env,
    user: &ToolboxUser,
    device_id: &str,
    file_id: &str,
    destination: FileDeliveryDestination,
) -> Result<std::result::Result<FileDelivery, Response>> {
    if validate_identifier(file_id, "file ID").is_err() {
        return api_error(400, "invalid file ID").map(Err);
    }
    let db = environment.d1("DB")?;
    let Some(file) = visible_file(&db, user, file_id).await? else {
        return api_error(404, "File not found").map(Err);
    };
    let delivery_id = Uuid::new_v4().to_string();
    let now = now_ms_i64()?;
    let insert = query!(
        &db,
        "INSERT INTO file_deliveries (id, company_id, device_id, file_id, file_name, size_bytes, requested_by_user_id, destination, created_at) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9 WHERE EXISTS (SELECT 1 FROM agents WHERE id = ?3 AND company_id = ?2 AND deletion_requested_at IS NULL)",
        delivery_id,
        user.company_id,
        device_id,
        file.id,
        file.name,
        file.size_bytes as f64,
        user.user_id,
        destination.as_str(),
        now
    )?;
    let audit = audit_if_created(
        &db,
        "INSERT INTO audit_events (id, company_id, actor_user_id, action, target_type, target_id, metadata_json, created_at) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 WHERE EXISTS (SELECT 1 FROM file_deliveries WHERE id = ?9)",
        user,
        "file.deliver",
        device_id,
        serde_json::json!({
            "delivery_id": delivery_id,
            "file_id": file.id,
            "file_name": file.name,
            "destination": destination.as_str(),
        }),
        &delivery_id,
    )?;
    let results = metered_batch(&db, vec![insert, audit]).await?;
    if results.first().map(changes) != Some(1) {
        return api_error(404, "Agent not found").map(Err);
    }
    let command = AgentCommand::DeliverFile {
        delivery: FileDeliveryRequest {
            delivery_id: delivery_id.clone(),
            file_name: file.name.clone(),
            size_bytes: file.size_bytes,
            sha256: file.sha256,
            destination,
        },
    };
    if !crate::agent_coordinator::send_command(environment, device_id, &command).await? {
        query!(
            &db,
            "UPDATE file_deliveries SET status = 'failed', error = ?1, completed_at = ?2 WHERE id = ?3 AND status = 'pending'",
            "The device is offline, so the file was not sent.",
            now_ms_i64()?,
            delivery_id
        )?
        .metered_run()
        .await?;
        return api_error(409, "The device is offline, so the file was not sent.").map(Err);
    }
    Ok(Ok(FileDelivery {
        id: delivery_id,
        device_id: device_id.to_owned(),
        file_id: file.id,
        file_name: file.name,
        size_bytes: file.size_bytes,
        status: FileDeliveryStatus::Pending,
        path: None,
        error: None,
        created_at_unix_ms: unix_ms(now),
        completed_at_unix_ms: None,
    }))
}

/// `GET /v1/agents/{device_id}/file-deliveries/{id}/content`: the Agent
/// downloads a file it was asked to save, while the delivery is pending.
pub(crate) async fn agent_delivery_content(
    request: &Request,
    environment: &Env,
    device_id: &str,
    delivery_id: &str,
) -> Result<Response> {
    let authorization = match authorize_agent(request, environment, device_id).await {
        Ok(authorization) => authorization,
        Err(_) => return api_error(401, "Agent authentication failed"),
    };
    if validate_identifier(delivery_id, "delivery ID").is_err() {
        return api_error(400, "invalid delivery ID");
    }
    let db = environment.d1("DB")?;
    let file_id = query!(
        &db,
        "SELECT d.file_id AS file_id FROM file_deliveries d JOIN toolbox_files f ON f.id = d.file_id AND f.company_id = d.company_id WHERE d.id = ?1 AND d.device_id = ?2 AND d.company_id = ?3 AND d.status = 'pending' AND d.created_at > ?4",
        delivery_id,
        device_id,
        authorization.company_id,
        now_ms_i64()? - DELIVERY_WINDOW_MS
    )?
    .metered_first::<String>(Some("file_id"))
    .await?;
    let Some(file_id) = file_id else {
        return api_error(404, "no pending delivery has this ID");
    };
    match file_content(environment, &authorization.company_id, &file_id).await? {
        Some(response) => Ok(response),
        None => api_error(404, "the file is no longer in the toolbox"),
    }
}

/// `POST /v1/agents/{device_id}/file-deliveries/{id}/result`, from the Agent.
pub(crate) async fn report_file_delivery(
    request: &mut Request,
    environment: &Env,
    device_id: &str,
    delivery_id: &str,
) -> Result<Response> {
    let authorization = match authorize_agent(request, environment, device_id).await {
        Ok(authorization) => authorization,
        Err(_) => return api_error(401, "Agent authentication failed"),
    };
    if validate_identifier(delivery_id, "delivery ID").is_err() {
        return api_error(400, "invalid delivery ID");
    }
    let report: FileDeliveryReport = match agent_report(request).await? {
        Ok(report) => report,
        Err(response) => return Ok(response),
    };
    if !matches!(
        report.status,
        FileDeliveryStatus::Delivered | FileDeliveryStatus::Failed
    ) {
        return api_error(400, "a delivery reports how it finished");
    }
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "UPDATE file_deliveries SET status = ?1, path = ?2, error = ?3, completed_at = ?4 WHERE id = ?5 AND device_id = ?6 AND company_id = ?7 AND status = 'pending'",
        report.status.as_str(),
        bounded(report.path, MAX_PATH_CHARS),
        bounded(report.error, MAX_REPORTED_ERROR_CHARS),
        now_ms_i64()?,
        delivery_id,
        device_id,
        authorization.company_id
    )?
    .metered_run()
    .await?;
    if changes(&result) == 0 {
        return api_error(404, "no pending delivery has this ID");
    }
    Response::empty().map(|response| response.with_status(204))
}

/// `GET /v1/remote/sessions/{id}/toolbox`: what the session's technician
/// may run or send.
pub(crate) async fn session_toolbox(
    request: &Request,
    environment: &Env,
    session_id: &str,
) -> Result<Response> {
    let (user, _) = authorized!(session_user(request, environment, session_id));
    let db = environment.d1("DB")?;
    let listing = ToolboxListing {
        scripts: visible_scripts(&db, &user)
            .await?
            .into_iter()
            .filter_map(|row| {
                Some(ToolboxScript {
                    language: ScriptLanguage::parse(&row.language)?,
                    id: row.id,
                    name: row.name,
                    folder: row.folder,
                    description: row.description,
                    shared: row.shared,
                })
            })
            .collect(),
        files: visible_files(&db, &user)
            .await?
            .into_iter()
            .map(|row| ToolboxFile {
                id: row.id,
                name: row.name,
                folder: row.folder,
                size_bytes: row.size_bytes,
                shared: row.shared,
            })
            .collect(),
    };
    Response::from_json(&listing)
}

/// `POST /v1/remote/sessions/{id}/script-runs`: runs a script on the
/// session's device.
pub(crate) async fn session_run_script(
    request: &mut Request,
    environment: &Env,
    session_id: &str,
) -> Result<Response> {
    let (user, device_id) = authorized!(session_user(request, environment, session_id));
    let start: StartScriptRun = body!(request, "invalid script run");
    match start_script_run(environment, &user, &device_id, start, "session").await? {
        Ok(run) => Ok(Response::from_json(&run)?.with_status(201)),
        Err(response) => Ok(response),
    }
}

/// `GET /v1/remote/sessions/{id}/script-runs/{run_id}`: a run the session's
/// technician started on its device.
pub(crate) async fn session_script_run(
    request: &Request,
    environment: &Env,
    session_id: &str,
    run_id: &str,
) -> Result<Response> {
    let (user, device_id) = authorized!(session_user(request, environment, session_id));
    if validate_identifier(run_id, "run ID").is_err() {
        return api_error(400, "invalid run ID");
    }
    let db = environment.d1("DB")?;
    match stored_run(&db, &user.company_id, run_id).await? {
        Some(row) if row.device_id == device_id && row.requested_by_user_id == user.user_id => {
            Response::from_json(&row.into_run(now_ms_i64()?)?.0)
        }
        _ => api_error(404, "Run not found"),
    }
}

/// `POST /v1/remote/sessions/{id}/file-deliveries`: sends a library file to
/// the session's device.
pub(crate) async fn session_deliver_file(
    request: &mut Request,
    environment: &Env,
    session_id: &str,
) -> Result<Response> {
    let (user, device_id) = authorized!(session_user(request, environment, session_id));
    let start: StartFileDelivery = body!(request, "invalid file delivery");
    let destination = if start.background {
        FileDeliveryDestination::Public
    } else {
        FileDeliveryDestination::User
    };
    match start_file_delivery(environment, &user, &device_id, &start.file_id, destination).await? {
        Ok(delivery) => Ok(Response::from_json(&delivery)?.with_status(201)),
        Err(response) => Ok(response),
    }
}

/// `GET /v1/remote/sessions/{id}/file-deliveries/{delivery_id}`.
pub(crate) async fn session_file_delivery(
    request: &Request,
    environment: &Env,
    session_id: &str,
    delivery_id: &str,
) -> Result<Response> {
    let (user, device_id) = authorized!(session_user(request, environment, session_id));
    if validate_identifier(delivery_id, "delivery ID").is_err() {
        return api_error(400, "invalid delivery ID");
    }
    let db = environment.d1("DB")?;
    let row = query!(
        &db,
        "SELECT id, device_id, file_id, file_name, size_bytes, status, path, error, created_at, completed_at, requested_by_user_id FROM file_deliveries WHERE id = ?1 AND company_id = ?2",
        delivery_id,
        user.company_id
    )?
    .metered_first::<DeliveryRow>(None)
    .await?;
    match row {
        Some(row) if row.device_id == device_id && row.requested_by_user_id == user.user_id => {
            Response::from_json(&row.into_delivery(now_ms_i64()?)?)
        }
        _ => api_error(404, "Delivery not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreported_runs_become_lost_after_their_timeout_and_grace() {
        let created = 1_000_000;
        let deadline = created + 60_000 + RUN_REPORT_GRACE_MS;
        assert_eq!(
            run_status(ScriptRunStatus::Pending, created, 60, deadline),
            ScriptRunStatus::Pending
        );
        assert_eq!(
            run_status(ScriptRunStatus::Pending, created, 60, deadline + 1),
            ScriptRunStatus::Lost
        );
        assert_eq!(
            run_status(ScriptRunStatus::Completed, created, 60, deadline + 1),
            ScriptRunStatus::Completed,
            "a finished run keeps its status"
        );
    }

    #[test]
    fn digests_decode_to_bytes() {
        let digest = decode_sha256(&"0f".repeat(32)).unwrap();
        assert_eq!(digest, vec![0x0f; 32]);
        assert_eq!(decode_sha256("0f"), None);
        assert_eq!(decode_sha256(&"0F".repeat(32)), None);
    }

    #[test]
    fn only_owners_and_administrators_of_shared_items_edit() {
        let user = ToolboxUser {
            company_id: "c".into(),
            user_id: "me".into(),
            admin: false,
        };
        assert!(user.can_edit("me", false));
        assert!(user.can_edit("me", true));
        assert!(!user.can_edit("other", true));
        let admin = ToolboxUser {
            admin: true,
            ..user
        };
        assert!(admin.can_edit("other", true));
        assert!(
            !admin.can_edit("other", false),
            "private items stay private"
        );
    }

    #[test]
    fn script_input_is_normalized_and_validated() {
        let input = |name: &str, folder: &str, body: &str, timeout: u32| ScriptInput {
            name: name.into(),
            folder: folder.into(),
            description: " Cleans up \n".into(),
            language: ScriptLanguage::Powershell,
            body: body.into(),
            timeout_seconds: timeout,
            shared: false,
        };
        let normalized = input(" Clean ", " Disk / Temp ", "Remove-Item", 300)
            .normalized()
            .unwrap();
        assert_eq!(normalized.name, "Clean");
        assert_eq!(normalized.folder, "Disk/Temp");
        assert_eq!(normalized.description, "Cleans up");
        assert!(input("", "", "x", 300).normalized().is_err());
        assert!(input("x", "", "  ", 300).normalized().is_err());
        assert!(input("x", "", "x", 5).normalized().is_err());
        assert!(input("x", &"f/".repeat(9), "x", 300).normalized().is_err());
    }
}
