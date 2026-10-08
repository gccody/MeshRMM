//! The toolbox: scripts and library files, each private to the user who
//! added it or shared with everyone, and their runs and deliveries on
//! devices. See docs/toolbox.md.
//!
//! Users act from the website, or from a remote session's viewer as the
//! session's technician. Either way the server hands the run or delivery to
//! the Agent over its control connection, and the Agent reports back over
//! HTTPS with its own credential.
//!
//! Who may do what:
//! - Private items belong to their owner, who needs `scripts.run` (or
//!   `files.deliver`) to keep, use and change them. Nobody else sees them.
//! - Shared items are visible to every user who may run scripts (or deliver
//!   files). Sharing an item, and changing or deleting a shared one, needs
//!   `scripts.manage_shared` (or `files.manage_shared`).
use meshrmm_protocol_types::{
    AgentCommand, FileDelivery, FileDeliveryDestination, FileDeliveryRequest, FileDeliveryStatus,
    RunAs, ScriptLanguage, ScriptRun, ScriptRunRequest, ScriptRunStatus,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::Serialize;
use serde_json::json;

use crate::{
    agents,
    audit::{self, Actor, Target},
    db::{
        self, Executor,
        tables::{FileDeliveries, ScriptRuns, ToolboxFiles, ToolboxScripts},
    },
    http::{ApiError, AppState},
    rbac::{Permission, Permissions},
    time::{MINUTE_MS, SECOND_MS, now_ms},
    users::new_id,
};

/// A run the Agent has not reported this long after its timeout is lost.
pub const RUN_REPORT_GRACE_MS: i64 = 2 * MINUTE_MS;
/// A delivery the Agent has not reported in this long is lost, and the
/// Agent may no longer download its file.
pub const DELIVERY_WINDOW_MS: i64 = 30 * MINUTE_MS;

/// Scripts or files: the two kinds of toolbox item, which follow the same
/// rules with their own permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Script,
    File,
}

impl Kind {
    /// Lets a user keep and use private items, and use shared ones.
    pub fn use_permission(self) -> Permission {
        match self {
            Self::Script => Permission::ScriptsRun,
            Self::File => Permission::FilesDeliver,
        }
    }

    pub fn manage_permission(self) -> Permission {
        match self {
            Self::Script => Permission::ScriptsManageShared,
            Self::File => Permission::FilesManageShared,
        }
    }
}

/// Who is using the toolbox: a signed-in user, or a session's technician.
#[derive(Debug, Clone)]
pub struct ToolboxUser {
    pub user_id: String,
    pub permissions: Permissions,
    /// Recorded as the actor of what the user does.
    pub actor: Actor,
}

impl ToolboxUser {
    /// Whether the user sees any items of `kind`.
    pub fn sees(&self, kind: Kind) -> bool {
        self.permissions.contains(&kind.use_permission())
            || self.permissions.contains(&kind.manage_permission())
    }

    /// Whether the user may change or delete an item.
    pub fn can_edit(&self, kind: Kind, owner_user_id: &str, shared: bool) -> bool {
        if shared {
            self.permissions.contains(&kind.manage_permission())
        } else {
            owner_user_id == self.user_id && self.permissions.contains(&kind.use_permission())
        }
    }

    /// Checks that the user may keep an item of `kind` with `shared`.
    pub fn require_keep(&self, kind: Kind, shared: bool) -> Result<(), ApiError> {
        let permission = if shared {
            kind.manage_permission()
        } else {
            kind.use_permission()
        };
        self.require(permission)
    }

    pub fn require(&self, permission: Permission) -> Result<(), ApiError> {
        if self.permissions.contains(&permission) {
            Ok(())
        } else {
            Err(
                ApiError::forbidden(format!("you don't have the {permission} permission"))
                    .with_code("permission_denied"),
            )
        }
    }

    /// Items of a kind the user sees: their own private ones, and shared ones.
    fn visible(
        &self,
        owner: impl sea_query::IntoColumnRef,
        shared: impl sea_query::IntoColumnRef,
    ) -> sea_query::Condition {
        sea_query::Cond::any()
            .add(Expr::col(shared).eq(true))
            .add(Expr::col(owner).eq(self.user_id.as_str()))
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScriptRow {
    pub id: String,
    pub owner_user_id: String,
    pub shared: bool,
    pub folder: String,
    pub name: String,
    pub description: String,
    pub language: String,
    pub body: String,
    pub timeout_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ScriptRow {
    pub fn language(&self) -> Result<ScriptLanguage, ApiError> {
        ScriptLanguage::parse(&self.language).ok_or_else(|| {
            tracing::error!(script_id = self.id, "stored script has an unknown language");
            ApiError::internal()
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileRow {
    pub id: String,
    pub owner_user_id: String,
    pub shared: bool,
    pub folder: String,
    pub name: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl FileRow {
    pub fn size_bytes(&self) -> u64 {
        u64::try_from(self.size_bytes).unwrap_or_default()
    }
}

/// Scripts, with their bodies or (for lists) with empty ones.
fn script_select(with_body: bool) -> sea_query::SelectStatement {
    let mut select = Query::select();
    select.columns([
        ToolboxScripts::Id,
        ToolboxScripts::OwnerUserId,
        ToolboxScripts::Shared,
        ToolboxScripts::Folder,
        ToolboxScripts::Name,
        ToolboxScripts::Description,
        ToolboxScripts::Language,
    ]);
    if with_body {
        select.column(ToolboxScripts::Body);
    } else {
        select.expr_as(Expr::val(""), ToolboxScripts::Body);
    }
    select
        .columns([
            ToolboxScripts::TimeoutSeconds,
            ToolboxScripts::CreatedAt,
            ToolboxScripts::UpdatedAt,
        ])
        .from(ToolboxScripts::Table)
        .to_owned()
}

fn file_select() -> sea_query::SelectStatement {
    Query::select()
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
        ])
        .from(ToolboxFiles::Table)
        .to_owned()
}

/// Orders items by folder, then name, ignoring case, the way the website
/// shows them. Done here so both backends agree.
fn sort_by_place<T>(items: &mut [T], key: impl Fn(&T) -> (&str, &str)) {
    items.sort_by_cached_key(|item| {
        let (folder, name) = key(item);
        (folder.to_lowercase(), name.to_lowercase())
    });
}

/// The scripts the user sees, without their bodies, or none if they may not
/// use scripts.
pub async fn visible_scripts(
    executor: &mut impl Executor,
    user: &ToolboxUser,
) -> db::Result<Vec<ScriptRow>> {
    if !user.sees(Kind::Script) {
        return Ok(Vec::new());
    }
    let mut rows: Vec<ScriptRow> = executor
        .fetch_all(
            &script_select(false)
                .cond_where(user.visible(ToolboxScripts::OwnerUserId, ToolboxScripts::Shared))
                .to_owned(),
        )
        .await?;
    sort_by_place(&mut rows, |row| (&row.folder, &row.name));
    Ok(rows)
}

/// A script the user sees, with its body.
pub async fn visible_script(
    executor: &mut impl Executor,
    user: &ToolboxUser,
    script_id: &str,
) -> db::Result<Option<ScriptRow>> {
    if !user.sees(Kind::Script) {
        return Ok(None);
    }
    executor
        .fetch_optional(
            &script_select(true)
                .and_where(Expr::col(ToolboxScripts::Id).eq(script_id))
                .cond_where(user.visible(ToolboxScripts::OwnerUserId, ToolboxScripts::Shared))
                .to_owned(),
        )
        .await
}

/// The files the user sees, or none if they may not deliver files.
pub async fn visible_files(
    executor: &mut impl Executor,
    user: &ToolboxUser,
) -> db::Result<Vec<FileRow>> {
    if !user.sees(Kind::File) {
        return Ok(Vec::new());
    }
    let mut rows: Vec<FileRow> = executor
        .fetch_all(
            &file_select()
                .cond_where(user.visible(ToolboxFiles::OwnerUserId, ToolboxFiles::Shared))
                .to_owned(),
        )
        .await?;
    sort_by_place(&mut rows, |row| (&row.folder, &row.name));
    Ok(rows)
}

pub async fn visible_file(
    executor: &mut impl Executor,
    user: &ToolboxUser,
    file_id: &str,
) -> db::Result<Option<FileRow>> {
    if !user.sees(Kind::File) {
        return Ok(None);
    }
    executor
        .fetch_optional(
            &file_select()
                .and_where(Expr::col(ToolboxFiles::Id).eq(file_id))
                .cond_where(user.visible(ToolboxFiles::OwnerUserId, ToolboxFiles::Shared))
                .to_owned(),
        )
        .await
}

/// A file regardless of who may see it.
pub async fn file_by_id(
    executor: &mut impl Executor,
    file_id: &str,
) -> db::Result<Option<FileRow>> {
    executor
        .fetch_optional(
            &file_select()
                .and_where(Expr::col(ToolboxFiles::Id).eq(file_id))
                .to_owned(),
        )
        .await
}

/// Where a run or delivery was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Dashboard,
    Session,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Session => "session",
        }
    }
}

const OFFLINE_RUN: &str = "The device is offline, so the script did not run.";
const OFFLINE_DELIVERY: &str = "The device is offline, so the file was not sent.";

fn device_not_found() -> ApiError {
    ApiError::not_found("Device not found")
}

fn unix_ms(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

/// Starts one of the user's scripts on `device_id` and returns the run. The
/// run is recorded, and audited, before the Agent gets it.
pub async fn start_script_run(
    state: &AppState,
    user: &ToolboxUser,
    device_id: &str,
    script_id: &str,
    run_as: RunAs,
    source: Source,
) -> Result<ScriptRun, ApiError> {
    user.require(Permission::ScriptsRun)?;
    let mut transaction = state.database.begin().await?;
    let script = visible_script(&mut transaction, user, script_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Script not found"))?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Err(device_not_found());
    }
    let language = script.language()?;
    let run_id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(ScriptRuns::Table)
                .columns([
                    ScriptRuns::Id,
                    ScriptRuns::DeviceId,
                    ScriptRuns::ScriptId,
                    ScriptRuns::ScriptName,
                    ScriptRuns::Language,
                    ScriptRuns::RequestedByUserId,
                    ScriptRuns::Source,
                    ScriptRuns::RunAs,
                    ScriptRuns::TimeoutSeconds,
                    ScriptRuns::CreatedAt,
                ])
                .values_panic([
                    run_id.as_str().into(),
                    device_id.into(),
                    script.id.as_str().into(),
                    script.name.as_str().into(),
                    language.as_str().into(),
                    user.user_id.as_str().into(),
                    source.as_str().into(),
                    run_as.as_str().into(),
                    script.timeout_seconds.into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &user.actor,
        "script.run",
        Target::device(device_id),
        json!({
            "run_id": run_id,
            "script_id": script.id,
            "script_name": script.name,
            "run_as": run_as.as_str(),
            "source": source.as_str(),
        }),
    )
    .await?;
    transaction.commit().await?;
    let timeout_seconds =
        u32::try_from(script.timeout_seconds).map_err(|_| ApiError::internal())?;
    let command = AgentCommand::RunScript {
        run: ScriptRunRequest {
            run_id: run_id.clone(),
            language,
            body: script.body,
            run_as,
            timeout_seconds,
        },
    };
    if !state.agents.send(device_id, command) {
        fail_pending(state, ScriptRuns::Table, &run_id, OFFLINE_RUN).await?;
        return Err(ApiError::conflict(OFFLINE_RUN).with_code("device_offline"));
    }
    Ok(ScriptRun {
        id: run_id,
        device_id: device_id.to_owned(),
        script_id: script.id,
        script_name: script.name,
        language,
        run_as,
        status: ScriptRunStatus::Pending,
        ran_as: None,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        output_truncated: false,
        error: None,
        created_at_unix_ms: unix_ms(now),
        completed_at_unix_ms: None,
    })
}

/// Sends one of the user's library files to `device_id` and returns the
/// delivery. The delivery is recorded, and audited, before the Agent gets it.
pub async fn start_file_delivery(
    state: &AppState,
    user: &ToolboxUser,
    device_id: &str,
    file_id: &str,
    destination: FileDeliveryDestination,
    source: Source,
) -> Result<FileDelivery, ApiError> {
    user.require(Permission::FilesDeliver)?;
    let mut transaction = state.database.begin().await?;
    let file = visible_file(&mut transaction, user, file_id)
        .await?
        .ok_or_else(|| ApiError::not_found("File not found"))?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Err(device_not_found());
    }
    let delivery_id = new_id();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(FileDeliveries::Table)
                .columns([
                    FileDeliveries::Id,
                    FileDeliveries::DeviceId,
                    FileDeliveries::FileId,
                    FileDeliveries::FileName,
                    FileDeliveries::SizeBytes,
                    FileDeliveries::RequestedByUserId,
                    FileDeliveries::Destination,
                    FileDeliveries::CreatedAt,
                ])
                .values_panic([
                    delivery_id.as_str().into(),
                    device_id.into(),
                    file.id.as_str().into(),
                    file.name.as_str().into(),
                    file.size_bytes.into(),
                    user.user_id.as_str().into(),
                    destination.as_str().into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &user.actor,
        "file.deliver",
        Target::device(device_id),
        json!({
            "delivery_id": delivery_id,
            "file_id": file.id,
            "file_name": file.name,
            "destination": destination.as_str(),
            "source": source.as_str(),
        }),
    )
    .await?;
    transaction.commit().await?;
    let command = AgentCommand::DeliverFile {
        delivery: FileDeliveryRequest {
            delivery_id: delivery_id.clone(),
            file_name: file.name.clone(),
            size_bytes: file.size_bytes(),
            sha256: file.sha256.clone(),
            destination,
        },
    };
    if !state.agents.send(device_id, command) {
        fail_pending(state, FileDeliveries::Table, &delivery_id, OFFLINE_DELIVERY).await?;
        return Err(ApiError::conflict(OFFLINE_DELIVERY).with_code("device_offline"));
    }
    Ok(FileDelivery {
        id: delivery_id,
        device_id: device_id.to_owned(),
        file_id: file.id.clone(),
        size_bytes: file.size_bytes(),
        file_name: file.name,
        status: FileDeliveryStatus::Pending,
        path: None,
        error: None,
        created_at_unix_ms: unix_ms(now),
        completed_at_unix_ms: None,
    })
}

/// Marks a run or delivery the Agent never got as failed. Both tables name
/// these columns the same.
async fn fail_pending<T: sea_query::Iden + 'static>(
    state: &AppState,
    table: T,
    id: &str,
    error: &str,
) -> db::Result<()> {
    state
        .database
        .execute(
            &Query::update()
                .table(table)
                .value(ScriptRuns::Status, "failed")
                .value(ScriptRuns::Error, error)
                .value(ScriptRuns::CompletedAt, now_ms())
                .and_where(Expr::col(ScriptRuns::Id).eq(id))
                .and_where(Expr::col(ScriptRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
pub struct RunRow {
    pub id: String,
    pub device_id: String,
    pub script_id: String,
    pub script_name: String,
    pub language: String,
    pub run_as: String,
    pub status: String,
    pub ran_as: Option<String>,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub error: Option<String>,
    pub timeout_seconds: i64,
    pub source: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub requested_by_user_id: String,
}

/// A run as the website shows it.
#[derive(Debug, Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub run: ScriptRun,
    /// `dashboard` or `session`.
    pub source: String,
    pub requested_by_you: bool,
}

/// The status to show for a run stored as `status`: one the Agent has not
/// reported long after its timeout is lost.
pub fn run_status(
    status: ScriptRunStatus,
    created_at: i64,
    timeout_seconds: i64,
    now: i64,
) -> ScriptRunStatus {
    let deadline = created_at
        .saturating_add(timeout_seconds.saturating_mul(SECOND_MS))
        .saturating_add(RUN_REPORT_GRACE_MS);
    if status == ScriptRunStatus::Pending && now > deadline {
        ScriptRunStatus::Lost
    } else {
        status
    }
}

/// The status to show for a delivery stored as `status`.
pub fn delivery_status(
    status: FileDeliveryStatus,
    created_at: i64,
    now: i64,
) -> FileDeliveryStatus {
    if status == FileDeliveryStatus::Pending && now > created_at.saturating_add(DELIVERY_WINDOW_MS)
    {
        FileDeliveryStatus::Lost
    } else {
        status
    }
}

fn corrupt(what: &str, id: &str) -> ApiError {
    tracing::error!(id, "stored {what} is invalid");
    ApiError::internal()
}

impl RunRow {
    pub fn into_run(self, now: i64) -> Result<ScriptRun, ApiError> {
        let stored =
            ScriptRunStatus::parse(&self.status).ok_or_else(|| corrupt("run", &self.id))?;
        Ok(ScriptRun {
            status: run_status(stored, self.created_at, self.timeout_seconds, now),
            language: ScriptLanguage::parse(&self.language)
                .ok_or_else(|| corrupt("run", &self.id))?,
            run_as: RunAs::parse(&self.run_as).ok_or_else(|| corrupt("run", &self.id))?,
            exit_code: self.exit_code.and_then(|code| i32::try_from(code).ok()),
            id: self.id,
            device_id: self.device_id,
            script_id: self.script_id,
            script_name: self.script_name,
            ran_as: self.ran_as,
            stdout: self.stdout,
            stderr: self.stderr,
            output_truncated: self.output_truncated,
            error: self.error,
            created_at_unix_ms: unix_ms(self.created_at),
            completed_at_unix_ms: self.completed_at.map(unix_ms),
        })
    }

    pub fn into_view(self, user_id: &str, now: i64) -> Result<RunView, ApiError> {
        let source = self.source.clone();
        let requested_by_you = self.requested_by_user_id == user_id;
        Ok(RunView {
            run: self.into_run(now)?,
            source,
            requested_by_you,
        })
    }
}

/// Runs, newest first. `with_output` includes what the scripts printed.
pub fn run_select(with_output: bool) -> sea_query::SelectStatement {
    let mut select = Query::select();
    select.columns([
        ScriptRuns::Id,
        ScriptRuns::DeviceId,
        ScriptRuns::ScriptId,
        ScriptRuns::ScriptName,
        ScriptRuns::Language,
        ScriptRuns::RunAs,
        ScriptRuns::Status,
        ScriptRuns::RanAs,
        ScriptRuns::ExitCode,
    ]);
    if with_output {
        select.columns([ScriptRuns::Stdout, ScriptRuns::Stderr]);
    } else {
        select
            .expr_as(Expr::val(""), ScriptRuns::Stdout)
            .expr_as(Expr::val(""), ScriptRuns::Stderr);
    }
    select
        .columns([
            ScriptRuns::OutputTruncated,
            ScriptRuns::Error,
            ScriptRuns::TimeoutSeconds,
            ScriptRuns::Source,
            ScriptRuns::CreatedAt,
            ScriptRuns::CompletedAt,
            ScriptRuns::RequestedByUserId,
        ])
        .from(ScriptRuns::Table)
        .order_by(ScriptRuns::CreatedAt, Order::Desc)
        .order_by(ScriptRuns::Id, Order::Desc)
        .to_owned()
}

#[derive(Debug, sqlx::FromRow)]
pub struct DeliveryRow {
    pub id: String,
    pub device_id: String,
    pub file_id: String,
    pub file_name: String,
    pub size_bytes: i64,
    pub destination: String,
    pub status: String,
    pub path: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub requested_by_user_id: String,
}

/// A delivery as the website shows it.
#[derive(Debug, Serialize)]
pub struct DeliveryView {
    #[serde(flatten)]
    pub delivery: FileDelivery,
    /// `user` or `public`.
    pub destination: String,
    pub requested_by_you: bool,
}

impl DeliveryRow {
    pub fn into_delivery(self, now: i64) -> Result<FileDelivery, ApiError> {
        let stored =
            FileDeliveryStatus::parse(&self.status).ok_or_else(|| corrupt("delivery", &self.id))?;
        Ok(FileDelivery {
            status: delivery_status(stored, self.created_at, now),
            size_bytes: unix_ms(self.size_bytes),
            id: self.id,
            device_id: self.device_id,
            file_id: self.file_id,
            file_name: self.file_name,
            path: self.path,
            error: self.error,
            created_at_unix_ms: unix_ms(self.created_at),
            completed_at_unix_ms: self.completed_at.map(unix_ms),
        })
    }

    pub fn into_view(self, user_id: &str, now: i64) -> Result<DeliveryView, ApiError> {
        let destination = self.destination.clone();
        let requested_by_you = self.requested_by_user_id == user_id;
        Ok(DeliveryView {
            delivery: self.into_delivery(now)?,
            destination,
            requested_by_you,
        })
    }
}

/// Deliveries, newest first.
pub fn delivery_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            FileDeliveries::Id,
            FileDeliveries::DeviceId,
            FileDeliveries::FileId,
            FileDeliveries::FileName,
            FileDeliveries::SizeBytes,
            FileDeliveries::Destination,
            FileDeliveries::Status,
            FileDeliveries::Path,
            FileDeliveries::Error,
            FileDeliveries::CreatedAt,
            FileDeliveries::CompletedAt,
            FileDeliveries::RequestedByUserId,
        ])
        .from(FileDeliveries::Table)
        .order_by(FileDeliveries::CreatedAt, Order::Desc)
        .order_by(FileDeliveries::Id, Order::Desc)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(permissions: &[Permission]) -> ToolboxUser {
        ToolboxUser {
            user_id: "me".to_owned(),
            permissions: permissions.iter().copied().collect(),
            actor: Actor::cli(),
        }
    }

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
        assert_eq!(
            delivery_status(
                FileDeliveryStatus::Pending,
                created,
                created + DELIVERY_WINDOW_MS
            ),
            FileDeliveryStatus::Pending
        );
        assert_eq!(
            delivery_status(
                FileDeliveryStatus::Pending,
                created,
                created + DELIVERY_WINDOW_MS + 1
            ),
            FileDeliveryStatus::Lost
        );
    }

    #[test]
    fn private_items_are_their_owners_and_shared_ones_need_the_manage_permission() {
        let runner = user(&[Permission::ScriptsRun]);
        assert!(runner.sees(Kind::Script));
        assert!(!runner.sees(Kind::File));
        assert!(runner.can_edit(Kind::Script, "me", false));
        assert!(!runner.can_edit(Kind::Script, "other", false));
        assert!(!runner.can_edit(Kind::Script, "me", true));
        assert!(runner.require_keep(Kind::Script, false).is_ok());
        assert!(runner.require_keep(Kind::Script, true).is_err());

        let manager = user(&[Permission::ScriptsManageShared]);
        assert!(manager.sees(Kind::Script));
        assert!(manager.can_edit(Kind::Script, "other", true));
        assert!(
            !manager.can_edit(Kind::Script, "other", false),
            "private items stay private"
        );
        assert!(
            !manager.can_edit(Kind::Script, "me", false),
            "keeping private scripts needs scripts.run"
        );
        assert!(!manager.can_edit(Kind::File, "other", true));
    }

    #[test]
    fn items_sort_by_folder_then_name_ignoring_case() {
        let mut items = vec![("b", "x"), ("", "Zed"), ("B", "a"), ("", "alpha")];
        sort_by_place(&mut items, |item| (item.0, item.1));
        assert_eq!(items, [("", "alpha"), ("", "Zed"), ("B", "a"), ("b", "x")]);
    }
}
