//! Update runs: the scans and installs the server asked devices for, by a
//! user or as a device's part of a job, and how they went. Installs, with
//! each update's outcome, are a device's update history.
use meshrmm_protocol_types::{
    AgentCommand, RebootBehavior, UpdateCategory, UpdateInstallRequest, UpdateInstallResult,
    UpdateInstallStatus, UpdateItemOutcome, UpdateSelection,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        self, Executor,
        tables::{Agents, UpdateRunItems, UpdateRuns, Users},
    },
    time::{MINUTE_MS, SECOND_MS},
};

/// A scan the Agent hasn't reported this long after it was asked for is
/// lost: its own limit, and a margin for reconnecting.
pub const SCAN_REPORT_WINDOW_MS: i64 =
    meshrmm_protocol_types::UPDATE_SCAN_TIMEOUT_SECONDS as i64 * SECOND_MS + 15 * MINUTE_MS;
/// Likewise for an install, which scans before and after installing.
pub const INSTALL_REPORT_WINDOW_MS: i64 =
    meshrmm_protocol_types::UPDATE_INSTALL_TIMEOUT_SECONDS as i64 * SECOND_MS + 60 * MINUTE_MS;
/// Rows per insert.
const INSERT_CHUNK: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Scan,
    Install,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Install => "install",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "scan" => Some(Self::Scan),
            "install" => Some(Self::Install),
            _ => None,
        }
    }

    /// How long the server waits for the Agent's report.
    pub fn report_window_ms(self) -> i64 {
        match self {
            Self::Scan => SCAN_REPORT_WINDOW_MS,
            Self::Install => INSTALL_REPORT_WINDOW_MS,
        }
    }
}

/// What an install installs and how the computer restarts afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// Chosen updates; empty for `categories`.
    pub update_ids: Vec<String>,
    /// Chosen categories; both empty for every pending update.
    pub categories: Vec<UpdateCategory>,
    pub reboot: RebootBehavior,
    pub reboot_delay_minutes: u32,
}

impl Install {
    /// Categories as stored: comma-separated, `None` for every one.
    pub fn categories_text(categories: &[UpdateCategory]) -> Option<String> {
        (!categories.is_empty()).then(|| {
            categories
                .iter()
                .map(|category| category.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
    }

    pub fn parse_categories(text: Option<&str>) -> Vec<UpdateCategory> {
        let mut categories = text
            .unwrap_or_default()
            .split(',')
            .filter_map(UpdateCategory::parse)
            .collect::<Vec<_>>();
        categories.sort();
        categories.dedup();
        categories
    }

    /// What it installs, in words: "all updates", "security and critical
    /// updates" or "3 chosen updates".
    pub fn describe(update_ids: &[String], categories: &[UpdateCategory]) -> String {
        if !update_ids.is_empty() {
            return match update_ids.len() {
                1 => "1 chosen update".to_owned(),
                count => format!("{count} chosen updates"),
            };
        }
        if categories.is_empty() {
            return "all updates".to_owned();
        }
        let names = categories
            .iter()
            .map(|category| category.label().to_lowercase())
            .collect::<Vec<_>>();
        let list = match names.as_slice() {
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            [] => unreachable!(),
        };
        format!("{list} updates")
    }
}

/// A run to record.
#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    pub id: &'a str,
    pub device_id: &'a str,
    pub kind: Kind,
    pub requested_by_user_id: &'a str,
    pub policy_name: Option<&'a str>,
    pub job_id: Option<&'a str>,
    /// For an install.
    pub install: Option<&'a Install>,
    pub created_at: i64,
}

pub async fn insert(executor: &mut impl Executor, run: &NewRun<'_>) -> db::Result<()> {
    let install = run.install;
    executor
        .execute(
            &Query::insert()
                .into_table(UpdateRuns::Table)
                .columns([
                    UpdateRuns::Id,
                    UpdateRuns::DeviceId,
                    UpdateRuns::Kind,
                    UpdateRuns::RequestedByUserId,
                    UpdateRuns::PolicyName,
                    UpdateRuns::JobId,
                    UpdateRuns::UpdateIds,
                    UpdateRuns::UpdateCategories,
                    UpdateRuns::Reboot,
                    UpdateRuns::RebootDelayMinutes,
                    UpdateRuns::CreatedAt,
                ])
                .values_panic([
                    run.id.into(),
                    run.device_id.into(),
                    run.kind.as_str().into(),
                    run.requested_by_user_id.into(),
                    run.policy_name.into(),
                    run.job_id.into(),
                    install
                        .filter(|install| !install.update_ids.is_empty())
                        .map(|install| {
                            serde_json::to_string(&install.update_ids).expect("IDs serialize")
                        })
                        .into(),
                    install
                        .and_then(|install| Install::categories_text(&install.categories))
                        .into(),
                    install.map(|install| install.reboot.as_str()).into(),
                    install
                        .map(|install| i64::from(install.reboot_delay_minutes))
                        .into(),
                    run.created_at.into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// The command that asks the Agent for the run.
pub fn command(id: &str, kind: Kind, install: Option<&Install>) -> AgentCommand {
    match (kind, install) {
        (Kind::Install, Some(install)) => AgentCommand::InstallUpdates {
            install: UpdateInstallRequest {
                run_id: id.to_owned(),
                selection: UpdateSelection {
                    ids: install.update_ids.clone(),
                    categories: install.categories.clone(),
                },
                reboot: install.reboot,
                reboot_delay_minutes: install.reboot_delay_minutes,
            },
        },
        _ => AgentCommand::ScanUpdates {
            run_id: Some(id.to_owned()),
        },
    }
}

/// Removes a pending run the Agent never got.
pub async fn discard(executor: &mut impl Executor, id: &str) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::Id).eq(id))
                .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// How one update of an install went, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Item {
    pub update_id: String,
    pub title: String,
    pub kb: Option<String>,
    pub category: String,
    pub outcome: String,
    pub error: Option<String>,
}

/// A run as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub id: String,
    pub device_id: String,
    /// `None` once the device is gone.
    pub device_name: Option<String>,
    pub kind: Kind,
    pub status: String,
    pub progress: Option<String>,
    pub progress_percent: Option<i64>,
    pub update_ids: Vec<String>,
    pub categories: Vec<UpdateCategory>,
    pub reboot: Option<RebootBehavior>,
    pub reboot_delay_minutes: Option<i64>,
    /// Who asked for it, or whose access the policy ran with.
    pub requested_by: String,
    #[serde(skip)]
    pub requested_by_user_id: String,
    pub policy_name: Option<String>,
    pub job_id: Option<String>,
    pub error: Option<String>,
    pub installed: i64,
    pub failed: i64,
    pub reboot_required: bool,
    pub reboot_scheduled_at: Option<i64>,
    pub reboot_error: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    /// Each update's outcome, for an install read on its own or in a history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<Item>>,
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: String,
    device_id: String,
    device_name: Option<String>,
    kind: String,
    status: String,
    progress: Option<String>,
    progress_percent: Option<i64>,
    update_ids: Option<String>,
    update_categories: Option<String>,
    reboot: Option<String>,
    reboot_delay_minutes: Option<i64>,
    requested_by: Option<String>,
    requested_by_user_id: String,
    policy_name: Option<String>,
    job_id: Option<String>,
    error: Option<String>,
    installed: i64,
    failed: i64,
    reboot_required: bool,
    reboot_scheduled_at: Option<i64>,
    reboot_error: Option<String>,
    created_at: i64,
    completed_at: Option<i64>,
}

impl RunRow {
    fn into_run(self) -> Option<Run> {
        let Some(kind) = Kind::parse(&self.kind) else {
            tracing::error!(run_id = self.id, "stored update run is invalid");
            return None;
        };
        Some(Run {
            kind,
            update_ids: self
                .update_ids
                .as_deref()
                .and_then(|ids| serde_json::from_str(ids).ok())
                .unwrap_or_default(),
            categories: Install::parse_categories(self.update_categories.as_deref()),
            reboot: self.reboot.as_deref().and_then(RebootBehavior::parse),
            requested_by: self
                .requested_by
                .unwrap_or_else(|| "A removed user".to_owned()),
            id: self.id,
            device_id: self.device_id,
            device_name: self.device_name,
            status: self.status,
            progress: self.progress,
            progress_percent: self.progress_percent,
            reboot_delay_minutes: self.reboot_delay_minutes,
            requested_by_user_id: self.requested_by_user_id,
            policy_name: self.policy_name,
            job_id: self.job_id,
            error: self.error,
            installed: self.installed,
            failed: self.failed,
            reboot_required: self.reboot_required,
            reboot_scheduled_at: self.reboot_scheduled_at,
            reboot_error: self.reboot_error,
            created_at: self.created_at,
            completed_at: self.completed_at,
            items: None,
        })
    }
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            (UpdateRuns::Table, UpdateRuns::Id),
            (UpdateRuns::Table, UpdateRuns::DeviceId),
        ])
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .columns([
            (UpdateRuns::Table, UpdateRuns::Kind),
            (UpdateRuns::Table, UpdateRuns::Status),
            (UpdateRuns::Table, UpdateRuns::Progress),
            (UpdateRuns::Table, UpdateRuns::ProgressPercent),
            (UpdateRuns::Table, UpdateRuns::UpdateIds),
            (UpdateRuns::Table, UpdateRuns::UpdateCategories),
            (UpdateRuns::Table, UpdateRuns::Reboot),
            (UpdateRuns::Table, UpdateRuns::RebootDelayMinutes),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "requested_by",
        )
        .columns([
            (UpdateRuns::Table, UpdateRuns::RequestedByUserId),
            (UpdateRuns::Table, UpdateRuns::PolicyName),
            (UpdateRuns::Table, UpdateRuns::JobId),
            (UpdateRuns::Table, UpdateRuns::Error),
            (UpdateRuns::Table, UpdateRuns::Installed),
            (UpdateRuns::Table, UpdateRuns::Failed),
            (UpdateRuns::Table, UpdateRuns::RebootRequired),
            (UpdateRuns::Table, UpdateRuns::RebootScheduledAt),
            (UpdateRuns::Table, UpdateRuns::RebootError),
            (UpdateRuns::Table, UpdateRuns::CreatedAt),
            (UpdateRuns::Table, UpdateRuns::CompletedAt),
        ])
        .from(UpdateRuns::Table)
        .left_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((UpdateRuns::Table, UpdateRuns::DeviceId)),
        )
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id))
                .equals((UpdateRuns::Table, UpdateRuns::RequestedByUserId)),
        )
        .to_owned()
}

/// A run, with its items if `items`.
pub async fn get(executor: &mut impl Executor, id: &str, items: bool) -> db::Result<Option<Run>> {
    let row: Option<RunRow> = executor
        .fetch_optional(
            &select()
                .and_where(Expr::col((UpdateRuns::Table, UpdateRuns::Id)).eq(id))
                .to_owned(),
        )
        .await?;
    let Some(mut run) = row.and_then(RunRow::into_run) else {
        return Ok(None);
    };
    if items && run.kind == Kind::Install {
        run.items = Some(items_of(executor, &run.id).await?);
    }
    Ok(Some(run))
}

async fn items_of(executor: &mut impl Executor, run_id: &str) -> db::Result<Vec<Item>> {
    let mut items: Vec<Item> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    UpdateRunItems::UpdateId,
                    UpdateRunItems::Title,
                    UpdateRunItems::Kb,
                    UpdateRunItems::Category,
                    UpdateRunItems::Outcome,
                    UpdateRunItems::Error,
                ])
                .from(UpdateRunItems::Table)
                .and_where(Expr::col(UpdateRunItems::RunId).eq(run_id))
                .to_owned(),
        )
        .await?;
    // Failures first, then by title.
    items.sort_by_cached_key(|item| {
        (
            item.outcome != UpdateItemOutcome::Failed.as_str(),
            item.title.to_lowercase(),
        )
    });
    Ok(items)
}

/// Which runs to list.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub device_id: Option<String>,
    pub kind: Option<Kind>,
}

/// Runs matching `filter`, newest first, installs with their items.
pub async fn list(
    executor: &mut impl Executor,
    filter: &Filter,
    limit: u64,
) -> db::Result<Vec<Run>> {
    let mut select = select();
    if let Some(device_id) = &filter.device_id {
        select
            .and_where(Expr::col((UpdateRuns::Table, UpdateRuns::DeviceId)).eq(device_id.as_str()));
    }
    if let Some(kind) = filter.kind {
        select.and_where(Expr::col((UpdateRuns::Table, UpdateRuns::Kind)).eq(kind.as_str()));
    }
    select
        .order_by((UpdateRuns::Table, UpdateRuns::CreatedAt), Order::Desc)
        .order_by((UpdateRuns::Table, UpdateRuns::Id), Order::Desc)
        .limit(limit);
    let rows: Vec<RunRow> = executor.fetch_all(&select).await?;
    let mut runs = rows
        .into_iter()
        .filter_map(RunRow::into_run)
        .collect::<Vec<_>>();
    for run in &mut runs {
        if run.kind == Kind::Install {
            run.items = Some(items_of(executor, &run.id).await?);
        }
    }
    Ok(runs)
}

/// The device's pending run of `kind`, if any.
pub async fn pending(
    executor: &mut impl Executor,
    device_id: &str,
    kind: Kind,
) -> db::Result<Option<String>> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(UpdateRuns::Id)
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(UpdateRuns::Kind).eq(kind.as_str()))
                .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                .order_by(UpdateRuns::CreatedAt, Order::Desc)
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(id,)| id))
}

/// Notes how far the device's pending install has got. Returns whether it
/// was pending.
pub async fn progress(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
    message: &str,
    percent: Option<u8>,
) -> db::Result<bool> {
    let message = message.replace('\0', "");
    let updated = executor
        .execute(
            &Query::update()
                .table(UpdateRuns::Table)
                .value(
                    UpdateRuns::Progress,
                    message.chars().take(256).collect::<String>(),
                )
                .value(UpdateRuns::ProgressPercent, percent.map(i64::from))
                .and_where(Expr::col(UpdateRuns::Id).eq(run_id))
                .and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

/// Finishes the device's pending scans among `run_ids`: they worked, or
/// failed with `error`. Returns the ones it finished.
pub async fn finish_scans(
    executor: &mut impl Executor,
    device_id: &str,
    run_ids: &[String],
    error: Option<&str>,
    now: i64,
) -> db::Result<Vec<String>> {
    let mut finished = Vec::new();
    for run_id in run_ids {
        let updated = executor
            .execute(
                &Query::update()
                    .table(UpdateRuns::Table)
                    .value(
                        UpdateRuns::Status,
                        if error.is_some() {
                            "failed"
                        } else {
                            "succeeded"
                        },
                    )
                    .value(
                        UpdateRuns::Error,
                        error.map(|error| error.replace('\0', "")),
                    )
                    .value(UpdateRuns::CompletedAt, now)
                    .and_where(Expr::col(UpdateRuns::Id).eq(run_id.as_str()))
                    .and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id))
                    .and_where(Expr::col(UpdateRuns::Kind).eq(Kind::Scan.as_str()))
                    .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                    .to_owned(),
            )
            .await?;
        if updated == 1 {
            finished.push(run_id.clone());
        }
    }
    Ok(finished)
}

/// Records how the device's pending install went. Returns whether it was
/// pending.
pub async fn finish_install(
    executor: &mut impl Executor,
    device_id: &str,
    run_id: &str,
    result: UpdateInstallResult,
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
    let status = match result.status {
        UpdateInstallStatus::Succeeded => "succeeded",
        UpdateInstallStatus::Partial => "partial",
        UpdateInstallStatus::Failed => "failed",
    };
    let updated = executor
        .execute(
            &Query::update()
                .table(UpdateRuns::Table)
                .value(UpdateRuns::Status, status)
                .value(UpdateRuns::Progress, Option::<String>::None)
                .value(UpdateRuns::ProgressPercent, Option::<i64>::None)
                .value(UpdateRuns::Error, result.error.clone())
                .value(UpdateRuns::Installed, count(UpdateItemOutcome::Installed))
                .value(UpdateRuns::Failed, count(UpdateItemOutcome::Failed))
                .value(UpdateRuns::RebootRequired, result.reboot_required)
                .value(
                    UpdateRuns::RebootScheduledAt,
                    result.reboot_scheduled_at_unix_ms,
                )
                .value(UpdateRuns::RebootError, result.reboot_error.clone())
                .value(UpdateRuns::CompletedAt, now)
                .and_where(Expr::col(UpdateRuns::Id).eq(run_id))
                .and_where(Expr::col(UpdateRuns::DeviceId).eq(device_id))
                .and_where(Expr::col(UpdateRuns::Kind).eq(Kind::Install.as_str()))
                .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    if updated != 1 {
        return Ok(false);
    }
    let mut seen = std::collections::HashSet::new();
    let items = result
        .items
        .iter()
        .filter(|item| seen.insert(item.id.clone()))
        .collect::<Vec<_>>();
    for chunk in items.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(UpdateRunItems::Table).columns([
            UpdateRunItems::RunId,
            UpdateRunItems::UpdateId,
            UpdateRunItems::Title,
            UpdateRunItems::Kb,
            UpdateRunItems::Category,
            UpdateRunItems::Outcome,
            UpdateRunItems::Error,
        ]);
        for item in chunk {
            insert.values_panic([
                run_id.into(),
                item.id.replace('\0', "").into(),
                item.title.replace('\0', "").into(),
                item.kb.clone().into(),
                item.category.as_str().into(),
                item.outcome.as_str().into(),
                item.error.clone().into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    Ok(true)
}

/// Marks runs the Agent never reported as lost. Returns them, with their
/// devices and kinds.
pub async fn sweep_lost(
    executor: &mut impl Executor,
    now: i64,
) -> db::Result<Vec<(String, String, Kind)>> {
    let pending: Vec<(String, String, String, i64)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    UpdateRuns::Id,
                    UpdateRuns::DeviceId,
                    UpdateRuns::Kind,
                    UpdateRuns::CreatedAt,
                ])
                .from(UpdateRuns::Table)
                .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                .to_owned(),
        )
        .await?;
    let mut lost = Vec::new();
    for (id, device_id, kind, created_at) in pending {
        let Some(kind) = Kind::parse(&kind) else {
            continue;
        };
        if created_at + kind.report_window_ms() >= now {
            continue;
        }
        let updated = executor
            .execute(
                &Query::update()
                    .table(UpdateRuns::Table)
                    .value(UpdateRuns::Status, "lost")
                    .value(UpdateRuns::Progress, Option::<String>::None)
                    .value(UpdateRuns::ProgressPercent, Option::<i64>::None)
                    .value(UpdateRuns::Error, "The device never reported how it went")
                    .value(UpdateRuns::CompletedAt, now)
                    .and_where(Expr::col(UpdateRuns::Id).eq(id.as_str()))
                    .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                    .to_owned(),
            )
            .await?;
        if updated == 1 {
            lost.push((id, device_id, kind));
        }
    }
    Ok(lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_describe_what_they_install() {
        assert_eq!(Install::describe(&[], &[]), "all updates");
        assert_eq!(
            Install::describe(&[], &[UpdateCategory::Security]),
            "security updates"
        );
        assert_eq!(
            Install::describe(
                &[],
                &[
                    UpdateCategory::Security,
                    UpdateCategory::Critical,
                    UpdateCategory::Definition
                ]
            ),
            "security, critical and definition updates"
        );
        assert_eq!(Install::describe(&["a".into()], &[]), "1 chosen update");
        assert_eq!(
            Install::parse_categories(Some("other,security,bogus,security")),
            vec![UpdateCategory::Security, UpdateCategory::Other]
        );
        assert_eq!(Install::parse_categories(None), vec![]);
        assert_eq!(
            Install::categories_text(&[UpdateCategory::Security, UpdateCategory::Driver]),
            Some("security,driver".into())
        );
        assert_eq!(Install::categories_text(&[]), None);
    }
}
