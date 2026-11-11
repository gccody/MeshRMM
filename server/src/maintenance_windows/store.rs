//! Maintenance windows in the database.
use sea_query::{Cond, Expr, ExprTrait, Order, Query, SelectStatement};
use serde::Serialize;

use super::{Automation, Held, RESTART_GRACE_MS};
use crate::{
    db::{
        self, Database, Executor,
        tables::{Agents, Companies, MaintenanceWindows, UpdateRuns},
    },
    time::MINUTE_MS,
};

/// Rows per insert.
const INSERT_CHUNK: usize = 400;
/// What `ended_by` says of a window an update install's report ended.
pub const INSTALL_FINISHED: &str = "The update install finished";

/// A window to write.
#[derive(Debug, Clone)]
pub struct NewWindow<'a> {
    pub id: String,
    pub device_id: &'a str,
    pub starts_at: i64,
    pub ends_at: Option<i64>,
    pub reason: &'a str,
    pub automation: Automation,
    pub created_by_user_id: Option<&'a str>,
    pub created_by: &'a str,
    /// The policy that set it: its ID and name.
    pub policy: Option<(&'a str, &'a str)>,
    pub update_run_id: Option<&'a str>,
}

/// Writes `windows`, set at `now`.
pub async fn insert(
    executor: &mut impl Executor,
    windows: &[NewWindow<'_>],
    now: i64,
) -> db::Result<()> {
    for chunk in windows.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(MaintenanceWindows::Table).columns([
            MaintenanceWindows::Id,
            MaintenanceWindows::DeviceId,
            MaintenanceWindows::StartsAt,
            MaintenanceWindows::EndsAt,
            MaintenanceWindows::Reason,
            MaintenanceWindows::Automation,
            MaintenanceWindows::CreatedByUserId,
            MaintenanceWindows::CreatedBy,
            MaintenanceWindows::PolicyId,
            MaintenanceWindows::PolicyName,
            MaintenanceWindows::UpdateRunId,
            MaintenanceWindows::CreatedAt,
            MaintenanceWindows::UpdatedAt,
        ]);
        for window in chunk {
            insert.values_panic([
                window.id.as_str().into(),
                window.device_id.into(),
                window.starts_at.into(),
                window.ends_at.into(),
                window.reason.into(),
                window.automation.as_str().into(),
                window.created_by_user_id.into(),
                window.created_by.into(),
                window.policy.map(|(id, _)| id).into(),
                window.policy.map(|(_, name)| name).into(),
                window.update_run_id.into(),
                now.into(),
                now.into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct HeldRow {
    id: String,
    device_id: String,
    starts_at: i64,
    ends_at: Option<i64>,
    reason: String,
    automation: String,
    created_by: String,
    update_run_id: Option<String>,
}

/// Windows that may still be in force: those ending after `now` or not
/// ending until someone ends them. Of every device, or only of `devices`.
pub async fn not_over(
    executor: &mut impl Executor,
    devices: Option<&[String]>,
    now: i64,
) -> db::Result<Vec<Held>> {
    let mut select = Query::select();
    select
        .columns([
            MaintenanceWindows::Id,
            MaintenanceWindows::DeviceId,
            MaintenanceWindows::StartsAt,
            MaintenanceWindows::EndsAt,
            MaintenanceWindows::Reason,
            MaintenanceWindows::Automation,
            MaintenanceWindows::CreatedBy,
            MaintenanceWindows::UpdateRunId,
        ])
        .from(MaintenanceWindows::Table)
        .cond_where(
            Cond::any()
                .add(Expr::col(MaintenanceWindows::EndsAt).is_null())
                .add(Expr::col(MaintenanceWindows::EndsAt).gt(now)),
        );
    if let Some(devices) = devices {
        select.and_where(Expr::col(MaintenanceWindows::DeviceId).is_in(devices.iter().cloned()));
    }
    let rows: Vec<HeldRow> = executor.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let held = Held {
                automation: Automation::parse(&row.automation)?,
                id: row.id,
                device_id: row.device_id,
                starts_at: row.starts_at,
                ends_at: row.ends_at,
                reason: row.reason,
                created_by: row.created_by,
                update_run_id: row.update_run_id,
            };
            (!held.over(now)).then_some(held)
        })
        .collect())
}

/// Where a window stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    /// It starts later.
    Upcoming,
    /// It is in force.
    Current,
    /// It was in force, and is over.
    Ended,
    /// It was ended before it started.
    Cancelled,
}

/// A window as the website and the API show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Window {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    pub company_id: Option<String>,
    pub company_name: Option<String>,
    pub starts_at: i64,
    /// `None` until someone ends it.
    pub ends_at: Option<i64>,
    pub reason: String,
    pub automation: Automation,
    /// Who set it: a user's name, or `policy “…”`.
    pub created_by: String,
    pub created_by_user_id: Option<String>,
    pub policy_id: Option<String>,
    pub policy_name: Option<String>,
    /// The update install it covers.
    pub update_run_id: Option<String>,
    /// Who ended it before its end, if someone did.
    pub ended_by: Option<String>,
    pub state: WindowState,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct WindowRow {
    id: String,
    device_id: String,
    device_name: Option<String>,
    company_id: Option<String>,
    company_name: Option<String>,
    starts_at: i64,
    ends_at: Option<i64>,
    reason: String,
    automation: String,
    created_by: String,
    created_by_user_id: Option<String>,
    policy_id: Option<String>,
    policy_name: Option<String>,
    update_run_id: Option<String>,
    ended_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl WindowRow {
    fn into_window(self, now: i64) -> Option<Window> {
        let state = match self.ends_at {
            Some(end) if end <= self.starts_at => WindowState::Cancelled,
            Some(end) if end <= now => WindowState::Ended,
            _ if self.starts_at > now => WindowState::Upcoming,
            _ => WindowState::Current,
        };
        Some(Window {
            automation: Automation::parse(&self.automation)?,
            id: self.id,
            device_id: self.device_id,
            device_name: self.device_name.unwrap_or_default(),
            company_id: self.company_id,
            company_name: self.company_name,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            reason: self.reason,
            created_by: self.created_by,
            created_by_user_id: self.created_by_user_id,
            policy_id: self.policy_id,
            policy_name: self.policy_name,
            update_run_id: self.update_run_id,
            ended_by: self.ended_by,
            state,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn window_select() -> SelectStatement {
    let window = |column| (MaintenanceWindows::Table, column);
    Query::select()
        .columns([
            window(MaintenanceWindows::Id),
            window(MaintenanceWindows::DeviceId),
        ])
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .expr_as(Expr::col((Agents::Table, Agents::CompanyId)), "company_id")
        .expr_as(
            Expr::col((Companies::Table, Companies::Name)),
            "company_name",
        )
        .columns([
            window(MaintenanceWindows::StartsAt),
            window(MaintenanceWindows::EndsAt),
            window(MaintenanceWindows::Reason),
            window(MaintenanceWindows::Automation),
            window(MaintenanceWindows::CreatedBy),
            window(MaintenanceWindows::CreatedByUserId),
            window(MaintenanceWindows::PolicyId),
            window(MaintenanceWindows::PolicyName),
            window(MaintenanceWindows::UpdateRunId),
            window(MaintenanceWindows::EndedBy),
            window(MaintenanceWindows::CreatedAt),
            window(MaintenanceWindows::UpdatedAt),
        ])
        .from(MaintenanceWindows::Table)
        .left_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id)).equals(window(MaintenanceWindows::DeviceId)),
        )
        .left_join(
            Companies::Table,
            Expr::col((Companies::Table, Companies::Id)).equals((Agents::Table, Agents::CompanyId)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .to_owned()
}

/// Which windows a list shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Showing {
    /// In force now, or starting later: the default.
    #[default]
    Open,
    Current,
    Upcoming,
    /// Over: ended or cancelled.
    Past,
    All,
}

/// What a list of windows is limited to.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub showing: Showing,
    pub device_id: Option<String>,
    /// A company's ID, or `Some(None)` for devices in none.
    pub company: Option<Option<String>>,
    pub policy_id: Option<String>,
}

/// Windows that `filter` shows at `now`, up to `limit`: open ones by when
/// they start, soonest first; past ones by when they ended, latest first.
pub async fn list(
    executor: &mut impl Executor,
    filter: &Filter,
    now: i64,
    limit: u64,
) -> db::Result<Vec<Window>> {
    let column = |column| Expr::col((MaintenanceWindows::Table, column));
    let ends = || column(MaintenanceWindows::EndsAt);
    let starts = || column(MaintenanceWindows::StartsAt);
    // Not over: no end yet, or one after now and after its start.
    let not_over = || {
        Cond::any()
            .add(ends().is_null())
            .add(Cond::all().add(ends().gt(now)).add(ends().gt(starts())))
    };
    let mut select = window_select();
    match filter.showing {
        Showing::Open => {
            select.cond_where(not_over());
        }
        Showing::Current => {
            select.cond_where(not_over()).and_where(starts().lte(now));
        }
        Showing::Upcoming => {
            select.cond_where(not_over()).and_where(starts().gt(now));
        }
        Showing::Past => {
            select.cond_where(not_over().not());
        }
        Showing::All => {}
    }
    if let Some(device_id) = &filter.device_id {
        select.and_where(column(MaintenanceWindows::DeviceId).eq(device_id.as_str()));
    }
    match &filter.company {
        Some(Some(company_id)) => {
            select.and_where(Expr::col((Agents::Table, Agents::CompanyId)).eq(company_id.as_str()));
        }
        Some(None) => {
            select.and_where(Expr::col((Agents::Table, Agents::CompanyId)).is_null());
        }
        None => {}
    }
    if let Some(policy_id) = &filter.policy_id {
        select.and_where(column(MaintenanceWindows::PolicyId).eq(policy_id.as_str()));
    }
    match filter.showing {
        Showing::Past | Showing::All => {
            select
                .order_by(
                    (MaintenanceWindows::Table, MaintenanceWindows::EndsAt),
                    Order::Desc,
                )
                .order_by(
                    (MaintenanceWindows::Table, MaintenanceWindows::StartsAt),
                    Order::Desc,
                );
        }
        _ => {
            select.order_by(
                (MaintenanceWindows::Table, MaintenanceWindows::StartsAt),
                Order::Asc,
            );
        }
    }
    select
        .order_by(
            (MaintenanceWindows::Table, MaintenanceWindows::Id),
            Order::Asc,
        )
        .limit(limit);
    let rows: Vec<WindowRow> = executor.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.into_window(now))
        .collect())
}

/// One window, as at `now`, if its device isn't deleted.
pub async fn get(executor: &mut impl Executor, id: &str, now: i64) -> db::Result<Option<Window>> {
    let row: Option<WindowRow> = executor
        .fetch_optional(
            &window_select()
                .and_where(Expr::col((MaintenanceWindows::Table, MaintenanceWindows::Id)).eq(id))
                .to_owned(),
        )
        .await?;
    Ok(row.and_then(|row| row.into_window(now)))
}

/// The windows of `devices` in force at `now`, and also those that start
/// later if `upcoming`.
pub async fn open_on(
    executor: &mut impl Executor,
    devices: &[String],
    now: i64,
    upcoming: bool,
) -> db::Result<Vec<Window>> {
    let mut windows = Vec::new();
    for chunk in devices.chunks(500) {
        let mut select = window_select();
        select
            .and_where(
                Expr::col((MaintenanceWindows::Table, MaintenanceWindows::DeviceId))
                    .is_in(chunk.iter().cloned()),
            )
            .cond_where(
                Cond::any()
                    .add(
                        Expr::col((MaintenanceWindows::Table, MaintenanceWindows::EndsAt))
                            .is_null(),
                    )
                    .add(
                        Expr::col((MaintenanceWindows::Table, MaintenanceWindows::EndsAt))
                            .gt(now)
                            .and(
                                Expr::col((MaintenanceWindows::Table, MaintenanceWindows::EndsAt))
                                    .gt(Expr::col((
                                        MaintenanceWindows::Table,
                                        MaintenanceWindows::StartsAt,
                                    ))),
                            ),
                    ),
            );
        if !upcoming {
            select.and_where(
                Expr::col((MaintenanceWindows::Table, MaintenanceWindows::StartsAt)).lte(now),
            );
        }
        let rows: Vec<WindowRow> = executor.fetch_all(&select).await?;
        windows.extend(rows.into_iter().filter_map(|row| row.into_window(now)));
    }
    Ok(windows)
}

/// Who ends a window early.
#[derive(Debug, Clone, Copy)]
pub struct Ender<'a> {
    pub user_id: Option<&'a str>,
    pub label: &'a str,
}

/// Ends the window at `now`, as `ender`, if it isn't over: one in force
/// ends, and one that hasn't started is cancelled. Returns whether it was
/// open.
pub async fn end(
    executor: &mut impl Executor,
    id: &str,
    ender: Ender<'_>,
    now: i64,
) -> db::Result<bool> {
    let ended = executor
        .execute(
            &Query::update()
                .table(MaintenanceWindows::Table)
                .value(MaintenanceWindows::EndsAt, now)
                .value(MaintenanceWindows::EndedByUserId, ender.user_id)
                .value(MaintenanceWindows::EndedBy, ender.label)
                .value(MaintenanceWindows::UpdatedAt, now)
                .and_where(Expr::col(MaintenanceWindows::Id).eq(id))
                .cond_where(
                    Cond::any()
                        .add(Expr::col(MaintenanceWindows::EndsAt).is_null())
                        .add(
                            Expr::col(MaintenanceWindows::EndsAt).gt(now).and(
                                Expr::col(MaintenanceWindows::EndsAt)
                                    .gt(Expr::col(MaintenanceWindows::StartsAt)),
                            ),
                        ),
                )
                .to_owned(),
        )
        .await?;
    Ok(ended == 1)
}

/// What a change to an open window sets; `None` keeps the field.
#[derive(Debug, Clone, Default)]
pub struct Change {
    /// `Some(None)` lasts until someone ends it.
    pub ends_at: Option<Option<i64>>,
    pub reason: Option<String>,
    pub automation: Option<Automation>,
}

/// Changes the open window `id`. Returns whether it was open.
pub async fn update(
    executor: &mut impl Executor,
    id: &str,
    change: &Change,
    now: i64,
) -> db::Result<bool> {
    let mut update = Query::update();
    update
        .table(MaintenanceWindows::Table)
        .value(MaintenanceWindows::UpdatedAt, now)
        .and_where(Expr::col(MaintenanceWindows::Id).eq(id))
        .cond_where(
            Cond::any()
                .add(Expr::col(MaintenanceWindows::EndsAt).is_null())
                .add(Expr::col(MaintenanceWindows::EndsAt).gt(now)),
        );
    if let Some(ends_at) = change.ends_at {
        update.value(MaintenanceWindows::EndsAt, ends_at);
    }
    if let Some(reason) = &change.reason {
        update.value(MaintenanceWindows::Reason, reason.as_str());
    }
    if let Some(automation) = change.automation {
        update.value(MaintenanceWindows::Automation, automation.as_str());
    }
    Ok(executor.execute(&update).await? == 1)
}

#[derive(sqlx::FromRow)]
struct FinishedInstall {
    id: String,
    device_id: String,
    ends_at: Option<i64>,
    completed_at: Option<i64>,
    reboot_scheduled_at: Option<i64>,
}

/// Ends the windows of update installs that reported or were lost: at once,
/// or when the restart the install set is due and [`RESTART_GRACE_MS`]
/// more. Returns their devices.
pub async fn end_finished_installs(database: &Database, now: i64) -> db::Result<Vec<String>> {
    let window = |column| (MaintenanceWindows::Table, column);
    let finished: Vec<FinishedInstall> = database
        .fetch_all(
            &Query::select()
                .columns([
                    window(MaintenanceWindows::Id),
                    window(MaintenanceWindows::DeviceId),
                    window(MaintenanceWindows::EndsAt),
                ])
                .columns([
                    (UpdateRuns::Table, UpdateRuns::CompletedAt),
                    (UpdateRuns::Table, UpdateRuns::RebootScheduledAt),
                ])
                .from(MaintenanceWindows::Table)
                .inner_join(
                    UpdateRuns::Table,
                    Expr::col((UpdateRuns::Table, UpdateRuns::Id))
                        .equals(window(MaintenanceWindows::UpdateRunId)),
                )
                .and_where(Expr::col(window(MaintenanceWindows::UpdateRunId)).is_not_null())
                .and_where(Expr::col(window(MaintenanceWindows::EndedBy)).is_null())
                .and_where(Expr::col((UpdateRuns::Table, UpdateRuns::Status)).ne("pending"))
                .to_owned(),
        )
        .await?;
    let mut devices = Vec::new();
    for install in finished {
        let completed = Ord::min(install.completed_at.unwrap_or(now), now);
        let restart = install
            .reboot_scheduled_at
            // By the device's clock: a restart more than a day off is a
            // clock that is wrong.
            .filter(|at| (*at - completed).abs() < 24 * 60 * MINUTE_MS)
            .map(|at| Ord::max(at, completed) + RESTART_GRACE_MS);
        let mut end = restart.unwrap_or(completed);
        if let Some(planned) = install.ends_at {
            end = Ord::min(end, planned);
        }
        database
            .execute(
                &Query::update()
                    .table(MaintenanceWindows::Table)
                    .value(MaintenanceWindows::EndsAt, end)
                    .value(MaintenanceWindows::EndedBy, INSTALL_FINISHED)
                    .value(MaintenanceWindows::UpdatedAt, now)
                    .and_where(Expr::col(MaintenanceWindows::Id).eq(install.id.as_str()))
                    .and_where(Expr::col(MaintenanceWindows::EndedBy).is_null())
                    .to_owned(),
            )
            .await?;
        devices.push(install.device_id);
    }
    devices.sort_unstable();
    devices.dedup();
    Ok(devices)
}

/// Deletes a device's windows.
pub async fn delete_for_device(executor: &mut impl Executor, device_id: &str) -> db::Result<u64> {
    executor
        .execute(
            &Query::delete()
                .from_table(MaintenanceWindows::Table)
                .and_where(Expr::col(MaintenanceWindows::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await
}

/// Deletes the window of an update install that never reached its Agent.
pub async fn delete_for_install(executor: &mut impl Executor, run_id: &str) -> db::Result<u64> {
    executor
        .execute(
            &Query::delete()
                .from_table(MaintenanceWindows::Table)
                .and_where(Expr::col(MaintenanceWindows::UpdateRunId).eq(run_id))
                .to_owned(),
        )
        .await
}

/// A policy's latest run of windows: how many it set, and when the last of
/// them ends (`None` if one lasts until ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PolicyRun {
    pub devices: i64,
    pub ends_at: Option<i64>,
}

/// The windows the policy set at `at`, its last run.
pub async fn policy_run(
    executor: &mut impl Executor,
    policy_id: &str,
    at: i64,
) -> db::Result<PolicyRun> {
    let (devices, ends_at): (i64, Option<i64>) = executor
        .fetch_one(
            &Query::select()
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .expr(Expr::cust("MAX(ends_at)"))
                .from(MaintenanceWindows::Table)
                .and_where(Expr::col(MaintenanceWindows::PolicyId).eq(policy_id))
                .and_where(Expr::col(MaintenanceWindows::CreatedAt).eq(at))
                .to_owned(),
        )
        .await?;
    Ok(PolicyRun { devices, ends_at })
}
