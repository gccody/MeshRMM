//! Application runs: the scans, installs, updates and uninstalls the server
//! asked devices for, by a user or as a device's part of a job, and how
//! each application went. They are a device's application history.
use meshrmm_protocol_types::{
    APPLICATION_RUN_TIMEOUT_SECONDS, APPLICATION_SCAN_TOTAL_SECONDS, AgentCommand,
    ApplicationOperation, ApplicationRunRequest, ApplicationRunResult, ItemOutcome,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use super::tables::{ApplicationRunItems, ApplicationRuns};
use crate::{
    db::{
        self, Executor,
        tables::{Agents, Users},
    },
    time::{MINUTE_MS, SECOND_MS},
};

/// A scan the Agent hasn't reported this long after it was asked for is
/// lost: the Agent's limit for a whole scan, and a margin for reconnecting.
pub const SCAN_REPORT_WINDOW_MS: i64 =
    APPLICATION_SCAN_TOTAL_SECONDS as i64 * SECOND_MS + 15 * MINUTE_MS;
/// Likewise for a run: its applications' limit, and a whole scan before
/// and after.
pub const RUN_REPORT_WINDOW_MS: i64 = APPLICATION_RUN_TIMEOUT_SECONDS as i64 * SECOND_MS
    + 2 * APPLICATION_SCAN_TOTAL_SECONDS as i64 * SECOND_MS
    + 15 * MINUTE_MS;
/// The most text of a progress message kept.
const MAX_PROGRESS_CHARS: usize = 256;
const MAX_ERROR_CHARS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Scan,
    Install,
    Update,
    Uninstall,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Install => "install",
            Self::Update => "update",
            Self::Uninstall => "uninstall",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "scan" => Some(Self::Scan),
            "install" => Some(Self::Install),
            "update" => Some(Self::Update),
            "uninstall" => Some(Self::Uninstall),
            _ => None,
        }
    }

    pub fn of(operation: &ApplicationOperation) -> Self {
        match operation {
            ApplicationOperation::Install { .. } => Self::Install,
            ApplicationOperation::Update { .. } => Self::Update,
            ApplicationOperation::Uninstall { .. } => Self::Uninstall,
        }
    }

    pub fn report_window_ms(self) -> i64 {
        match self {
            Self::Scan => SCAN_REPORT_WINDOW_MS,
            _ => RUN_REPORT_WINDOW_MS,
        }
    }

    /// The audit log's name for asking a device to do it.
    pub fn audit_kind(self) -> &'static str {
        match self {
            Self::Scan => "application.scan",
            Self::Install => "application.install",
            Self::Update => "application.update",
            Self::Uninstall => "application.uninstall",
        }
    }
}

/// A run to record.
#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    pub id: &'a str,
    pub device_id: &'a str,
    pub summary: &'a str,
    /// What the Agent is asked; `None` for a scan.
    pub operation: Option<&'a ApplicationOperation>,
    pub requested_by_user_id: &'a str,
    pub policy_name: Option<&'a str>,
    pub job_id: Option<&'a str>,
    pub created_at: i64,
}

impl NewRun<'_> {
    pub fn action(&self) -> Action {
        self.operation.map_or(Action::Scan, Action::of)
    }
}

/// Whether `error` is the database refusing a second pending scan, or a
/// second pending install, update or uninstall, for one device
/// (`idx_application_runs_one_pending`): another request started one first.
pub fn is_already_pending(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(error) if error.is_unique_violation())
}

/// Records a pending run. A device has at most one pending scan and one
/// pending other run, which the database enforces: see
/// [`is_already_pending`].
pub async fn insert(executor: &mut impl Executor, run: &NewRun<'_>) -> db::Result<()> {
    executor
        .execute(
            &Query::insert()
                .into_table(ApplicationRuns::Table)
                .columns([
                    ApplicationRuns::Id,
                    ApplicationRuns::DeviceId,
                    ApplicationRuns::Action,
                    ApplicationRuns::Summary,
                    ApplicationRuns::Request,
                    ApplicationRuns::RequestedByUserId,
                    ApplicationRuns::PolicyName,
                    ApplicationRuns::JobId,
                    ApplicationRuns::CreatedAt,
                ])
                .values_panic([
                    run.id.into(),
                    run.device_id.into(),
                    run.action().as_str().into(),
                    run.summary.into(),
                    run.operation
                        .map(|operation| {
                            serde_json::to_string(operation).expect("operations serialize")
                        })
                        .into(),
                    run.requested_by_user_id.into(),
                    run.policy_name.into(),
                    run.job_id.into(),
                    run.created_at.into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// The command that asks the Agent for the run.
pub fn command(id: &str, operation: Option<&ApplicationOperation>) -> AgentCommand {
    match operation {
        Some(operation) => AgentCommand::RunApplications {
            run: ApplicationRunRequest {
                run_id: id.to_owned(),
                operation: operation.clone(),
            },
        },
        None => AgentCommand::ScanApplications {
            run_id: Some(id.to_owned()),
        },
    }
}

/// Removes a pending run the Agent never got.
pub async fn discard(executor: &mut impl Executor, id: &str) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::Id).eq(id))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Finishes a run that couldn't be sent, such as a job's on a device none of
/// its applications installs on.
pub async fn fail(executor: &mut impl Executor, id: &str, error: &str, now: i64) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(ApplicationRuns::Table)
                .value(ApplicationRuns::Status, "failed")
                .value(ApplicationRuns::Error, error)
                .value(ApplicationRuns::CompletedAt, now)
                .and_where(Expr::col(ApplicationRuns::Id).eq(id))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// How one application of a run went, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Item {
    pub position: i64,
    pub name: String,
    pub source: Option<String>,
    pub package_id: Option<String>,
    pub outcome: String,
    pub version: Option<String>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    /// The end of the installer's or package manager's output; left out of
    /// lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub reboot_required: bool,
}

/// A run as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub id: String,
    pub device_id: String,
    /// `None` once the device is gone.
    pub device_name: Option<String>,
    pub action: Action,
    pub summary: String,
    pub status: String,
    pub progress: Option<String>,
    /// Who asked for it, or whose access the policy ran with.
    pub requested_by: String,
    #[serde(skip)]
    pub requested_by_user_id: String,
    pub policy_name: Option<String>,
    pub job_id: Option<String>,
    pub error: Option<String>,
    /// How many of its applications were done, had nothing to do, were
    /// skipped as not for the device, and failed.
    pub succeeded: i64,
    pub unchanged: i64,
    pub skipped: i64,
    pub failed: i64,
    pub reboot_required: bool,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<Item>>,
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: String,
    device_id: String,
    device_name: Option<String>,
    action: String,
    summary: String,
    status: String,
    progress: Option<String>,
    requested_by: Option<String>,
    requested_by_user_id: String,
    policy_name: Option<String>,
    job_id: Option<String>,
    error: Option<String>,
    succeeded: i64,
    unchanged: i64,
    skipped: i64,
    failed: i64,
    reboot_required: bool,
    created_at: i64,
    completed_at: Option<i64>,
}

impl RunRow {
    fn into_run(self) -> Option<Run> {
        let Some(action) = Action::parse(&self.action) else {
            tracing::error!(run_id = self.id, "stored application run is invalid");
            return None;
        };
        Some(Run {
            action,
            requested_by: self
                .requested_by
                .unwrap_or_else(|| "A removed user".to_owned()),
            id: self.id,
            device_id: self.device_id,
            device_name: self.device_name,
            summary: self.summary,
            status: self.status,
            progress: self.progress,
            requested_by_user_id: self.requested_by_user_id,
            policy_name: self.policy_name,
            job_id: self.job_id,
            error: self.error,
            succeeded: self.succeeded,
            unchanged: self.unchanged,
            skipped: self.skipped,
            failed: self.failed,
            reboot_required: self.reboot_required,
            created_at: self.created_at,
            completed_at: self.completed_at,
            items: None,
        })
    }
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            (ApplicationRuns::Table, ApplicationRuns::Id),
            (ApplicationRuns::Table, ApplicationRuns::DeviceId),
        ])
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .columns([
            (ApplicationRuns::Table, ApplicationRuns::Action),
            (ApplicationRuns::Table, ApplicationRuns::Summary),
            (ApplicationRuns::Table, ApplicationRuns::Status),
            (ApplicationRuns::Table, ApplicationRuns::Progress),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "requested_by",
        )
        .columns([
            (ApplicationRuns::Table, ApplicationRuns::RequestedByUserId),
            (ApplicationRuns::Table, ApplicationRuns::PolicyName),
            (ApplicationRuns::Table, ApplicationRuns::JobId),
            (ApplicationRuns::Table, ApplicationRuns::Error),
            (ApplicationRuns::Table, ApplicationRuns::Succeeded),
            (ApplicationRuns::Table, ApplicationRuns::Unchanged),
            (ApplicationRuns::Table, ApplicationRuns::Skipped),
            (ApplicationRuns::Table, ApplicationRuns::Failed),
            (ApplicationRuns::Table, ApplicationRuns::RebootRequired),
            (ApplicationRuns::Table, ApplicationRuns::CreatedAt),
            (ApplicationRuns::Table, ApplicationRuns::CompletedAt),
        ])
        .from(ApplicationRuns::Table)
        .left_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((ApplicationRuns::Table, ApplicationRuns::DeviceId)),
        )
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((ApplicationRuns::Table, ApplicationRuns::RequestedByUserId)),
        )
        .to_owned()
}

/// A run, with its items and their output if `items`.
pub async fn get(executor: &mut impl Executor, id: &str, items: bool) -> db::Result<Option<Run>> {
    let row: Option<RunRow> = executor
        .fetch_optional(
            &select()
                .and_where(Expr::col((ApplicationRuns::Table, ApplicationRuns::Id)).eq(id))
                .to_owned(),
        )
        .await?;
    let Some(mut run) = row.and_then(RunRow::into_run) else {
        return Ok(None);
    };
    if items && run.action != Action::Scan {
        run.items = Some(items_of(executor, &run.id, true).await?);
    }
    Ok(Some(run))
}

async fn items_of(
    executor: &mut impl Executor,
    run_id: &str,
    output: bool,
) -> db::Result<Vec<Item>> {
    let mut select = Query::select();
    select
        .columns([
            ApplicationRunItems::Position,
            ApplicationRunItems::Name,
            ApplicationRunItems::Source,
            ApplicationRunItems::PackageId,
            ApplicationRunItems::Outcome,
            ApplicationRunItems::Version,
            ApplicationRunItems::ExitCode,
            ApplicationRunItems::Error,
        ])
        .from(ApplicationRunItems::Table)
        .and_where(Expr::col(ApplicationRunItems::RunId).eq(run_id))
        .order_by(ApplicationRunItems::Position, Order::Asc);
    if output {
        select.column(ApplicationRunItems::Output);
    } else {
        select.expr_as(Expr::cust("NULL"), "output");
    }
    select.column(ApplicationRunItems::RebootRequired);
    executor.fetch_all(&select).await
}

/// Which runs to list.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub device_id: Option<String>,
    pub action: Option<Action>,
}

/// Runs matching `filter`, newest first, with their items but not their
/// output.
pub async fn list(
    executor: &mut impl Executor,
    filter: &Filter,
    limit: u64,
) -> db::Result<Vec<Run>> {
    let mut select = select();
    if let Some(device_id) = &filter.device_id {
        select.and_where(
            Expr::col((ApplicationRuns::Table, ApplicationRuns::DeviceId)).eq(device_id.as_str()),
        );
    }
    if let Some(action) = filter.action {
        select.and_where(
            Expr::col((ApplicationRuns::Table, ApplicationRuns::Action)).eq(action.as_str()),
        );
    }
    select
        .order_by(
            (ApplicationRuns::Table, ApplicationRuns::CreatedAt),
            Order::Desc,
        )
        .order_by((ApplicationRuns::Table, ApplicationRuns::Id), Order::Desc)
        .limit(limit);
    let rows: Vec<RunRow> = executor.fetch_all(&select).await?;
    let mut runs = rows
        .into_iter()
        .filter_map(RunRow::into_run)
        .collect::<Vec<_>>();
    for run in &mut runs {
        if run.action != Action::Scan {
            run.items = Some(items_of(executor, &run.id, false).await?);
        }
    }
    Ok(runs)
}

/// The device's pending scan, or (with `scan` false) its pending install,
/// update or uninstall, if any.
pub async fn pending(
    executor: &mut impl Executor,
    device_id: &str,
    scan: bool,
) -> db::Result<Option<String>> {
    let action = Expr::col(ApplicationRuns::Action);
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(ApplicationRuns::Id)
                .from(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::DeviceId).eq(device_id))
                .and_where(if scan {
                    action.eq(Action::Scan.as_str())
                } else {
                    action.ne(Action::Scan.as_str())
                })
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .order_by(ApplicationRuns::CreatedAt, Order::Desc)
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(id,)| id))
}

/// What the device's pending run asked the Agent for; `None` if the run
/// isn't pending on the device.
pub async fn pending_operation(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
) -> db::Result<Option<ApplicationOperation>> {
    let row: Option<(Option<String>,)> = executor
        .fetch_optional(
            &Query::select()
                .column(ApplicationRuns::Request)
                .from(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::Id).eq(run_id))
                .and_where(Expr::col(ApplicationRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(row
        .and_then(|(request,)| request)
        .and_then(|request| serde_json::from_str(&request).ok()))
}

/// Notes how far the device's pending run has got. Returns whether it was
/// pending.
pub async fn progress(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
    message: &str,
) -> db::Result<bool> {
    let message = meshrmm_protocol_types::clean_text(message);
    let updated = executor
        .execute(
            &Query::update()
                .table(ApplicationRuns::Table)
                .value(
                    ApplicationRuns::Progress,
                    message.chars().take(MAX_PROGRESS_CHARS).collect::<String>(),
                )
                .and_where(Expr::col(ApplicationRuns::Id).eq(run_id))
                .and_where(Expr::col(ApplicationRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(ApplicationRuns::Action).ne(Action::Scan.as_str()))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

/// Finishes the device's pending scans among `run_ids`. Returns the ones it
/// finished.
pub async fn finish_scans(
    executor: &mut impl Executor,
    device_id: &str,
    run_ids: &[String],
    now: i64,
) -> db::Result<Vec<String>> {
    let mut finished = Vec::new();
    for run_id in run_ids {
        let updated = executor
            .execute(
                &Query::update()
                    .table(ApplicationRuns::Table)
                    .value(ApplicationRuns::Status, "succeeded")
                    .value(ApplicationRuns::CompletedAt, now)
                    .and_where(Expr::col(ApplicationRuns::Id).eq(run_id.as_str()))
                    .and_where(Expr::col(ApplicationRuns::DeviceId).eq(device_id))
                    .and_where(Expr::col(ApplicationRuns::Action).eq(Action::Scan.as_str()))
                    .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                    .to_owned(),
            )
            .await?;
        if updated == 1 {
            finished.push(run_id.clone());
        }
    }
    Ok(finished)
}

/// Text as PostgreSQL stores it.
fn storable(text: &str) -> String {
    text.replace('\0', "")
}

/// Records how the device's pending run went. Returns whether it was
/// pending.
pub async fn finish(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
    result: ApplicationRunResult,
    now: i64,
) -> db::Result<bool> {
    let result = result.sanitized();
    let count = |outcome| {
        result
            .items
            .iter()
            .filter(|item| item.outcome == outcome)
            .count() as i64
    };
    let updated = executor
        .execute(
            &Query::update()
                .table(ApplicationRuns::Table)
                .value(ApplicationRuns::Status, result.status.as_str())
                .value(ApplicationRuns::Progress, Option::<String>::None)
                .value(
                    ApplicationRuns::Error,
                    result.error.as_deref().map(|error| {
                        storable(error)
                            .chars()
                            .take(MAX_ERROR_CHARS)
                            .collect::<String>()
                    }),
                )
                .value(ApplicationRuns::Succeeded, count(ItemOutcome::Succeeded))
                .value(ApplicationRuns::Unchanged, count(ItemOutcome::Unchanged))
                .value(ApplicationRuns::Skipped, count(ItemOutcome::Skipped))
                .value(ApplicationRuns::Failed, count(ItemOutcome::Failed))
                .value(
                    ApplicationRuns::RebootRequired,
                    result.items.iter().any(|item| item.reboot_required),
                )
                .value(ApplicationRuns::CompletedAt, now)
                .and_where(Expr::col(ApplicationRuns::Id).eq(run_id))
                .and_where(Expr::col(ApplicationRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(ApplicationRuns::Action).ne(Action::Scan.as_str()))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    if updated != 1 {
        return Ok(false);
    }
    for (position, item) in result.items.iter().enumerate() {
        executor
            .execute(
                &Query::insert()
                    .into_table(ApplicationRunItems::Table)
                    .columns([
                        ApplicationRunItems::RunId,
                        ApplicationRunItems::Position,
                        ApplicationRunItems::Name,
                        ApplicationRunItems::Source,
                        ApplicationRunItems::PackageId,
                        ApplicationRunItems::Outcome,
                        ApplicationRunItems::Version,
                        ApplicationRunItems::ExitCode,
                        ApplicationRunItems::Error,
                        ApplicationRunItems::Output,
                        ApplicationRunItems::RebootRequired,
                    ])
                    .values_panic([
                        run_id.into(),
                        (position as i64).into(),
                        storable(&item.name).into(),
                        item.source.map(|source| source.as_str()).into(),
                        item.package_id.as_deref().map(storable).into(),
                        item.outcome.as_str().into(),
                        item.version.as_deref().map(storable).into(),
                        item.exit_code.into(),
                        item.error
                            .as_deref()
                            .map(|error| {
                                storable(error)
                                    .chars()
                                    .take(MAX_ERROR_CHARS)
                                    .collect::<String>()
                            })
                            .into(),
                        storable(&item.output).into(),
                        item.reboot_required.into(),
                    ])
                    .to_owned(),
            )
            .await?;
    }
    Ok(true)
}

/// Marks runs the Agent never reported as lost. Returns them, with their
/// devices and actions.
pub async fn sweep_lost(
    executor: &mut impl Executor,
    now: i64,
) -> db::Result<Vec<(String, String, Action)>> {
    let pending: Vec<(String, String, String, i64)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    ApplicationRuns::Id,
                    ApplicationRuns::DeviceId,
                    ApplicationRuns::Action,
                    ApplicationRuns::CreatedAt,
                ])
                .from(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    let mut lost = Vec::new();
    for (id, device_id, action, created_at) in pending {
        let Some(action) = Action::parse(&action) else {
            continue;
        };
        if created_at + action.report_window_ms() >= now {
            continue;
        }
        if mark_lost(executor, &id, "The device never reported how it went", now).await? {
            lost.push((id, device_id, action));
        }
    }
    Ok(lost)
}

/// Marks a pending run lost, with why; false if it isn't pending.
async fn mark_lost(
    executor: &mut impl Executor,
    id: &str,
    error: &str,
    now: i64,
) -> db::Result<bool> {
    let updated = executor
        .execute(
            &Query::update()
                .table(ApplicationRuns::Table)
                .value(ApplicationRuns::Status, "lost")
                .value(ApplicationRuns::Progress, Option::<String>::None)
                .value(ApplicationRuns::Error, error)
                .value(ApplicationRuns::CompletedAt, now)
                .and_where(Expr::col(ApplicationRuns::Id).eq(id))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

pub const INTERRUPTED: &str = "The device's Agent restarted, or the device did, before it finished; it may have done part of it";

/// A run is handed to the Agent just after it's recorded. One recorded this
/// close before a connection may have been handed to that connection, after
/// the Agent said what it has; it's left to its report window.
pub const HANDED_OVER_MS: i64 = 30 * SECOND_MS;

/// The device's Agent reconnected and still has `known`, the scans and runs
/// it is doing or has yet to report. Marks the device's other pending runs,
/// asked for well before it connected at `connected_at`, as lost: a
/// restart or a connection that dropped before it got them ended them. Runs
/// asked for since are on the new connection. Returns them, with their
/// actions.
pub async fn interrupted(
    executor: &mut impl Executor,
    device_id: &str,
    known: &[String],
    connected_at: i64,
    now: i64,
) -> db::Result<Vec<(String, Action)>> {
    let pending: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([ApplicationRuns::Id, ApplicationRuns::Action])
                .from(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .and_where(Expr::col(ApplicationRuns::CreatedAt).lt(connected_at - HANDED_OVER_MS))
                .to_owned(),
        )
        .await?;
    let mut lost = Vec::new();
    for (id, action) in pending {
        let Some(action) = Action::parse(&action) else {
            continue;
        };
        if known.contains(&id) {
            continue;
        }
        if mark_lost(executor, &id, INTERRUPTED, now).await? {
            lost.push((id, action));
        }
    }
    Ok(lost)
}

/// How many of each device's job runs await a report at `now`.
pub async fn awaiting(executor: &mut impl Executor, now: i64) -> db::Result<Vec<(String, i64)>> {
    executor
        .fetch_all(
            &Query::select()
                .column(ApplicationRuns::DeviceId)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::JobId).is_not_null())
                .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                .and_where(Expr::cust(format!(
                    "created_at + CASE action WHEN 'scan' THEN {SCAN_REPORT_WINDOW_MS} \
                     ELSE {RUN_REPORT_WINDOW_MS} END >= {now}"
                )))
                .group_by_col(ApplicationRuns::DeviceId)
                .to_owned(),
        )
        .await
}

/// Removes finished runs that completed before `before`, with their items.
pub async fn purge(executor: &mut impl Executor, before: i64) -> db::Result<u64> {
    let old = Query::select()
        .column(ApplicationRuns::Id)
        .from(ApplicationRuns::Table)
        .and_where(Expr::col(ApplicationRuns::CompletedAt).lt(before))
        .to_owned();
    executor
        .execute(
            &Query::delete()
                .from_table(ApplicationRunItems::Table)
                .and_where(Expr::col(ApplicationRunItems::RunId).in_subquery(old))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(ApplicationRuns::Table)
                .and_where(Expr::col(ApplicationRuns::CompletedAt).lt(before))
                .to_owned(),
        )
        .await
}
