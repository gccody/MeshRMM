//! Jobs: one script run, file delivery, update scan, update install or
//! wake on many devices, and how far it has got on each.
//!
//! A job is created with every device it covers. A device that can't run the
//! script's interpreter, or has no update manager MeshRMM supports, is
//! skipped; an online one is queued for the dispatcher; an offline one waits
//! for its Agent to connect, or is missed at once if the job doesn't wait.
//! Creating one is two steps: [`plan`] decides all that through the
//! database's readers, and [`create`] only inserts it, so SQLite's one
//! writer is held for the inserts alone. A job on more devices than
//! [`DEVICES_PER_TRANSACTION`] is written in several short transactions
//! rather than one long one: its row, marked as being created; its devices,
//! that many to a transaction; and the one that clears the mark and records
//! its start. Until then nothing starts, queues, counts or shows it, and one
//! whose creation stopped part-way is removed ([`discard_abandoned`]).
//! Each device's run, delivery or update run is an ordinary one carrying the
//! job's ID, so its result, output and alerts work as for one started by
//! hand. A wake job is the other way round: it skips online devices, and
//! wakes each offline one through the devices on its network.
//!
//! What happened on each device is stored with it in `job_devices`: once
//! it starts, how its run ended (`result`, NULL while it awaits a report)
//! and when its report is due (`report_by`). Every change of a job run's
//! status writes them in the same transaction ([`runs_changed`]), so a
//! job's counts are an index read of its own devices ([`counts`]), however
//! many devices it has, and a report that never comes turns into no result
//! by the clock alone, with nothing to sweep.
use std::collections::{HashMap, HashSet};

use meshrmm_protocol_types::{
    DevicePlatform, FileDeliveryDestination, RebootBehavior, RunAs, ScriptLanguage, UpdateCategory,
};
use sea_query::{Expr, ExprTrait, InsertStatement, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Creating, Engine, dispatch::QUEUE_CHUNK};
use crate::{
    applications::{
        jobs::RunAction,
        tables::{ApplicationRunItems, ApplicationRuns},
    },
    audit::{self, Actor, Target as AuditTarget},
    db::{
        self, Database, Executor, Transaction,
        tables::{
            Agents, DeviceUpdateStatus, FileDeliveries, JobDevices, Jobs, ScriptRuns, UpdateRuns,
            Users, WakeRequests,
        },
    },
    http::{ApiError, AppState},
    rbac::{Permission, Permissions},
    realtime::AgentHub,
    time::{HOUR_MS, SECOND_MS},
    toolbox::{DELIVERY_WINDOW_MS, RUN_REPORT_GRACE_MS},
    updates::runs::{INSTALL_REPORT_WINDOW_MS, Install, SCAN_REPORT_WINDOW_MS},
    users::new_id,
};

/// The longest a job waits for offline devices: a week.
pub const MAX_WAIT_HOURS: i64 = 7 * 24;
pub const DEFAULT_WAIT_HOURS: i64 = 24;
/// Rows written per statement.
const INSERT_CHUNK: usize = 400;
/// The most devices of a job one transaction writes. A job on more is
/// created in several transactions, this many devices to each, so SQLite's
/// one writer, which Agents' check-ins, results and sign-ins wait for, is
/// held for one of them at a time rather than for the whole job. Measured on
/// a debug build, a job on 100,000 devices in one transaction held it for
/// about 5 seconds (PostgreSQL's transaction, 2.5); 1,000 devices took about
/// 10 milliseconds on either, 2,000 about 40 and 5,000 a quarter to half a
/// second, while a 100,000-device job took about as long in all in
/// transactions of 1,000 as of 2,000. A job on at most this many devices is
/// written in one transaction.
pub const DEVICES_PER_TRANSACTION: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Script,
    File,
    UpdateScan,
    UpdateInstall,
    Wake,
    AppInstall,
    AppUpdate,
    AppUninstall,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::File => "file",
            Self::UpdateScan => "update_scan",
            Self::UpdateInstall => "update_install",
            Self::Wake => "wake",
            Self::AppInstall => "app_install",
            Self::AppUpdate => "app_update",
            Self::AppUninstall => "app_uninstall",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "script" => Some(Self::Script),
            "file" => Some(Self::File),
            "update_scan" => Some(Self::UpdateScan),
            "update_install" => Some(Self::UpdateInstall),
            "wake" => Some(Self::Wake),
            "app_install" => Some(Self::AppInstall),
            "app_update" => Some(Self::AppUpdate),
            "app_uninstall" => Some(Self::AppUninstall),
            _ => None,
        }
    }

    /// Whether it installs, updates or uninstalls applications.
    pub fn is_application(self) -> bool {
        matches!(
            self,
            Self::AppInstall | Self::AppUpdate | Self::AppUninstall
        )
    }
}

pub use crate::targets::{MAX_CHOSEN_DEVICES, Target, TargetKind};

pub fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_job")
}

/// Whether a job waits for offline devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Offline {
    /// Offline devices are missed.
    Skip,
    /// Offline devices run when they connect, until the job expires.
    Wait,
}

impl Offline {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Wait => "wait",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "skip" => Some(Self::Skip),
            "wait" => Some(Self::Wait),
            _ => None,
        }
    }
}

/// What a job sends to each device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Script {
        script_id: String,
        name: String,
        language: ScriptLanguage,
        body: String,
        timeout_seconds: i64,
        run_as: RunAs,
    },
    File {
        file_id: String,
        name: String,
        destination: FileDeliveryDestination,
    },
    UpdateScan,
    /// Installs the pending updates in `categories`, or every one.
    UpdateInstall {
        categories: Vec<UpdateCategory>,
        reboot: RebootBehavior,
        reboot_delay_minutes: u32,
    },
    /// Wakes the devices that are offline.
    Wake,
    /// Installs, updates or uninstalls applications.
    Applications(Box<crate::applications::jobs::Spec>),
}

impl Payload {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Script { .. } => Kind::Script,
            Self::File { .. } => Kind::File,
            Self::UpdateScan => Kind::UpdateScan,
            Self::UpdateInstall { .. } => Kind::UpdateInstall,
            Self::Wake => Kind::Wake,
            Self::Applications(spec) => match spec.action {
                RunAction::Install => Kind::AppInstall,
                RunAction::Update => Kind::AppUpdate,
                RunAction::Uninstall => Kind::AppUninstall,
            },
        }
    }

    /// What it sends, by ID and name. Update jobs have no item; their name
    /// says what they do.
    fn item(&self) -> (&str, String) {
        match self {
            Self::Script {
                script_id, name, ..
            } => (script_id, name.clone()),
            Self::File { file_id, name, .. } => (file_id, name.clone()),
            Self::UpdateScan => ("", "Scan for updates".to_owned()),
            Self::Wake => ("", "Wake devices".to_owned()),
            Self::UpdateInstall { categories, .. } => (
                "",
                format!("Install {}", Install::describe(&[], categories)),
            ),
            Self::Applications(spec) => ("", spec.summary.clone()),
        }
    }

    /// Why a device can't take it, or `None` if it can: its platform, and
    /// whether its last update scan found an update manager MeshRMM supports.
    fn unsupported_on(
        &self,
        platform: Option<DevicePlatform>,
        updates_supported: bool,
    ) -> Option<String> {
        match (self, platform) {
            (Self::Script { language, .. }, Some(platform)) if !language.runs_on(platform) => {
                Some(format!(
                    "{} has no {} interpreter",
                    platform.label(),
                    language.label()
                ))
            }
            (Self::UpdateScan | Self::UpdateInstall { .. }, _) if !updates_supported => Some(
                "The device has no update service or package manager MeshRMM supports".to_owned(),
            ),
            (Self::Applications(spec), platform) => spec.unsupported_on(platform),
            _ => None,
        }
    }
}

/// Where a device stands in a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Queued,
    Waiting,
    Started,
    Skipped,
    Missed,
    Cancelled,
}

impl DeviceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Waiting => "waiting",
            Self::Started => "started",
            Self::Skipped => "skipped",
            Self::Missed => "missed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "queued" => Some(Self::Queued),
            "waiting" => Some(Self::Waiting),
            "started" => Some(Self::Started),
            "skipped" => Some(Self::Skipped),
            "missed" => Some(Self::Missed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// How a job went on one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Online; its run starts soon.
    Queued,
    /// Offline; it runs when the device connects.
    Waiting,
    /// Started and not reported yet.
    Running,
    Succeeded,
    /// Couldn't run, exited with a code other than 0, or the file wasn't saved.
    Failed,
    TimedOut,
    /// The device never reported back.
    NoResult,
    /// The device has no interpreter for the script.
    Skipped,
    /// Offline, and the job didn't wait or it didn't connect in time.
    Missed,
    Cancelled,
}

impl Outcome {
    /// Whether the device may still do something.
    pub fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Waiting | Self::Running)
    }

    /// Whether the outcome counts as a failure for notifications.
    pub fn failure(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut | Self::NoResult)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::NoResult => "no_result",
            Self::Skipped => "skipped",
            Self::Missed => "missed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Waiting => "Waiting for the device",
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::TimedOut => "Timed out",
            Self::NoResult => "No result",
            Self::Skipped => "Skipped",
            Self::Missed => "Missed",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// A started device's outcome as `job_devices.result` stores it: how its
/// run ended, or `None` while it awaits a report.
fn parse_result(result: &str) -> Outcome {
    match result {
        "succeeded" => Outcome::Succeeded,
        "failed" => Outcome::Failed,
        "timed_out" => Outcome::TimedOut,
        // None of the applications was for the device.
        "skipped" => Outcome::Skipped,
        // Lost: the device never reported.
        _ => Outcome::NoResult,
    }
}

/// A device's outcome from its state and, once started, its stored result:
/// how its run ended, or `None` while it awaits a report. `overdue` is a
/// report that was due before now, or a device with no run to report.
pub fn outcome(state: DeviceState, result: Option<&str>, overdue: bool) -> Outcome {
    match state {
        DeviceState::Queued => Outcome::Queued,
        DeviceState::Waiting => Outcome::Waiting,
        DeviceState::Skipped => Outcome::Skipped,
        DeviceState::Missed => Outcome::Missed,
        DeviceState::Cancelled => Outcome::Cancelled,
        DeviceState::Started => match result {
            None if overdue => Outcome::NoResult,
            None => Outcome::Running,
            Some(result) => parse_result(result),
        },
    }
}

/// The table a job's device has its part in: the run, delivery, update run,
/// application run or wake request carrying the job's ID. Each change of
/// such a row's status calls [`runs_changed`] in the same transaction, which
/// writes into `job_devices` the outcome that used to be worked out on every
/// read by joining the five tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTable {
    Script,
    Delivery,
    Update,
    Application,
    Wake,
}

impl RunTable {
    /// Where a job of `kind` keeps its devices' runs.
    pub fn of(kind: Kind) -> Self {
        match kind {
            Kind::Script => Self::Script,
            Kind::File => Self::Delivery,
            Kind::UpdateScan | Kind::UpdateInstall => Self::Update,
            Kind::Wake => Self::Wake,
            Kind::AppInstall | Kind::AppUpdate | Kind::AppUninstall => Self::Application,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Script => "script_runs",
            Self::Delivery => "file_deliveries",
            Self::Update => "update_runs",
            Self::Application => "application_runs",
            Self::Wake => "wake_requests",
        }
    }

    /// How a row ended, as `job_devices.result` stores it: NULL while it
    /// awaits a report. A run that completed with exit code 0, a delivered
    /// file, an update or application run that succeeded and a woken device
    /// that came online succeeded; a wake whose relays all failed, or that
    /// had none, failed; a run, update run or application run the device
    /// never reported has no result.
    fn result_sql(self) -> &'static str {
        match self {
            Self::Script => {
                "CASE script_runs.status WHEN 'pending' THEN NULL \
                 WHEN 'completed' THEN CASE WHEN script_runs.exit_code = 0 THEN 'succeeded' ELSE 'failed' END \
                 WHEN 'failed' THEN 'failed' WHEN 'timed_out' THEN 'timed_out' ELSE 'no_result' END"
            }
            Self::Delivery => {
                "CASE file_deliveries.status WHEN 'pending' THEN NULL \
                 WHEN 'delivered' THEN 'succeeded' WHEN 'failed' THEN 'failed' ELSE 'no_result' END"
            }
            Self::Update => {
                "CASE update_runs.status WHEN 'pending' THEN NULL \
                 WHEN 'succeeded' THEN 'succeeded' WHEN 'partial' THEN 'failed' \
                 WHEN 'failed' THEN 'failed' ELSE 'no_result' END"
            }
            Self::Application => {
                "CASE application_runs.status WHEN 'pending' THEN NULL \
                 WHEN 'succeeded' THEN 'succeeded' WHEN 'partial' THEN 'failed' \
                 WHEN 'failed' THEN 'failed' WHEN 'skipped' THEN 'skipped' ELSE 'no_result' END"
            }
            Self::Wake => {
                "CASE WHEN wake_requests.online_at IS NOT NULL THEN 'succeeded' \
                 WHEN wake_requests.status IN ('sending', 'sent') THEN NULL ELSE 'failed' END"
            }
        }
    }

    /// When a row's report is due: past it, one still awaiting a report has
    /// no result.
    fn deadline_sql(self) -> String {
        match self {
            Self::Script => format!(
                "script_runs.created_at + script_runs.timeout_seconds * {SECOND_MS} + {RUN_REPORT_GRACE_MS}"
            ),
            Self::Delivery => format!("file_deliveries.created_at + {DELIVERY_WINDOW_MS}"),
            Self::Update => format!(
                "update_runs.created_at + CASE update_runs.kind WHEN 'scan' THEN {SCAN_REPORT_WINDOW_MS} \
                 ELSE {INSTALL_REPORT_WINDOW_MS} END"
            ),
            Self::Application => format!(
                "application_runs.created_at + CASE application_runs.action WHEN 'scan' THEN {} \
                 ELSE {} END",
                crate::applications::runs::SCAN_REPORT_WINDOW_MS,
                crate::applications::runs::RUN_REPORT_WINDOW_MS
            ),
            Self::Wake => format!(
                "wake_requests.created_at + {}",
                crate::wake::ONLINE_WINDOW_MS
            ),
        }
    }

    /// `expression` of the row of this table that `which` picks.
    fn read(self, expression: String, which: sea_query::Condition) -> sea_query::SelectStatement {
        Query::select()
            .expr(Expr::cust(expression))
            .from(self.name())
            .cond_where(which)
            .to_owned()
    }

    fn column(self, column: &'static str) -> Expr {
        Expr::col((self.name(), column))
    }

    /// The row with ID `id`.
    fn by_id(self, id: &str) -> sea_query::Condition {
        sea_query::Cond::all().add(self.column("id").eq(id))
    }

    /// The row of the `job_devices` row being written, by its job and device.
    fn of_part(self) -> sea_query::Condition {
        sea_query::Cond::all()
            .add(
                self.column("job_id")
                    .equals((JobDevices::Table, JobDevices::JobId)),
            )
            .add(
                self.column("device_id")
                    .equals((JobDevices::Table, JobDevices::DeviceId)),
            )
    }

    /// Sets a started part's result and report deadline from the row that
    /// `which` picks.
    fn set_from(self, update: &mut sea_query::UpdateStatement, which: sea_query::Condition) {
        // A device has one row of a job at a time (one that never reached
        // its Agent is removed), but a value must be one row whatever.
        let latest = |expression: String| {
            self.read(expression, which.clone())
                .order_by_expr(
                    Expr::cust(format!("{}.created_at", self.name())),
                    Order::Desc,
                )
                .limit(1)
                .to_owned()
        };
        update
            .value(JobDevices::Result, latest(self.result_sql().to_owned()))
            .value(JobDevices::ReportBy, latest(self.deadline_sql()));
    }
}

/// Which rows of a [`RunTable`] changed.
#[derive(Debug, Clone, Copy)]
pub enum Changed<'a> {
    /// The row with this ID.
    Id(&'a str),
    /// The device's rows, such as its wake requests when it came online.
    Device(&'a str),
}

/// Writes into each started job device whose row of `table` changed, as
/// `changed` says, how that row ended and when its report is due. Callers run
/// it in the transaction that changed the row's status, so a job's counts
/// never see one without the other. A row that belongs to no job changes
/// nothing. Returns how many devices it wrote.
pub async fn runs_changed(
    executor: &mut impl Executor,
    table: RunTable,
    changed: Changed<'_>,
    now: i64,
) -> db::Result<u64> {
    executor
        .execute(&runs_changed_statement(table, changed, now))
        .await
}

/// The statement [`runs_changed`] runs: a lookup of the changed rows' job
/// devices by their primary key, or by their device.
pub fn runs_changed_statement(
    table: RunTable,
    changed: Changed<'_>,
    now: i64,
) -> sea_query::UpdateStatement {
    let mut update = Query::update();
    update
        .table(JobDevices::Table)
        .value(JobDevices::UpdatedAt, now)
        .and_where(
            Expr::col((JobDevices::Table, JobDevices::State)).eq(DeviceState::Started.as_str()),
        );
    match changed {
        Changed::Id(id) => {
            table.set_from(&mut update, table.by_id(id));
            // Looked up by the job's primary key; a row without a job matches
            // nothing.
            update
                .and_where(
                    Expr::col((JobDevices::Table, JobDevices::JobId))
                        .eq(table.read(format!("{}.job_id", table.name()), table.by_id(id))),
                )
                .and_where(
                    Expr::col((JobDevices::Table, JobDevices::DeviceId))
                        .eq(table.read(format!("{}.device_id", table.name()), table.by_id(id))),
                );
        }
        Changed::Device(device_id) => {
            table.set_from(&mut update, table.of_part());
            update
                .and_where(Expr::col((JobDevices::Table, JobDevices::DeviceId)).eq(device_id))
                .and_where(
                    Expr::col((JobDevices::Table, JobDevices::JobId)).in_subquery(
                        table.read(
                            format!("{}.job_id", table.name()),
                            sea_query::Cond::all()
                                .add(table.column("device_id").eq(device_id))
                                .add(table.column("job_id").is_not_null()),
                        ),
                    ),
                );
        }
    }
    update
}

/// Claims a queued device for the run `run_id` of `table`, recorded in the
/// same transaction: the device is started, with the run's result (it may
/// have failed already, such as a file deleted from the library) and its
/// report deadline. Returns whether it was still queued.
pub(crate) async fn claim(
    executor: &mut impl Executor,
    job_id: &str,
    device_id: &str,
    table: RunTable,
    run_id: &str,
    now: i64,
) -> db::Result<bool> {
    let mut update = Query::update();
    update
        .table(JobDevices::Table)
        .value(JobDevices::State, DeviceState::Started.as_str())
        .value(JobDevices::UpdatedAt, now)
        .and_where(Expr::col(JobDevices::JobId).eq(job_id))
        .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
        .and_where(Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()));
    table.set_from(&mut update, table.by_id(run_id));
    Ok(executor.execute(&update).await? == 1)
}

/// How many devices of a job have each outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub total: i64,
    pub queued: i64,
    pub waiting: i64,
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub timed_out: i64,
    pub no_result: i64,
    pub skipped: i64,
    pub missed: i64,
    pub cancelled: i64,
}

impl Counts {
    pub fn add(&mut self, outcome: Outcome, count: i64) {
        self.total += count;
        *self.slot(outcome) += count;
    }

    /// How many devices have `outcome`, or how many there are.
    pub fn of(mut self, outcome: Option<Outcome>) -> i64 {
        match outcome {
            Some(outcome) => *self.slot(outcome),
            None => self.total,
        }
    }

    fn slot(&mut self, outcome: Outcome) -> &mut i64 {
        match outcome {
            Outcome::Queued => &mut self.queued,
            Outcome::Waiting => &mut self.waiting,
            Outcome::Running => &mut self.running,
            Outcome::Succeeded => &mut self.succeeded,
            Outcome::Failed => &mut self.failed,
            Outcome::TimedOut => &mut self.timed_out,
            Outcome::NoResult => &mut self.no_result,
            Outcome::Skipped => &mut self.skipped,
            Outcome::Missed => &mut self.missed,
            Outcome::Cancelled => &mut self.cancelled,
        }
    }

    /// Devices that may still do something.
    pub fn active(&self) -> i64 {
        self.queued + self.waiting + self.running
    }

    pub fn failures(&self) -> i64 {
        self.failed + self.timed_out + self.no_result
    }
}

/// A job as lists show it: what it sends, to whom, and how far it got.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Job {
    pub id: String,
    pub kind: Kind,
    pub item_id: String,
    pub item_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<ScriptLanguage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_as: Option<RunAs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<FileDeliveryDestination>,
    /// An install's categories; empty for every pending update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<UpdateCategory>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reboot: Option<RebootBehavior>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reboot_delay_minutes: Option<i64>,
    pub target: TargetKind,
    pub target_label: String,
    pub offline: Offline,
    pub expires_at_unix_ms: i64,
    #[serde(skip)]
    pub requested_by_user_id: String,
    /// The user who started it, or whose access the policy ran with.
    pub requested_by: String,
    pub policy_id: Option<String>,
    pub policy_name: Option<String>,
    pub created_at_unix_ms: i64,
    pub finished_at_unix_ms: Option<i64>,
    pub counts: Counts,
}

impl Job {
    pub fn finished(&self) -> bool {
        self.finished_at_unix_ms.is_some() || self.counts.active() == 0
    }
}

/// What a user may see and do with jobs.
#[derive(Debug, Clone)]
pub struct Viewer {
    pub user_id: String,
    /// Everyone's jobs, as the audit log would show them.
    pub everyone: bool,
    /// Policies' jobs.
    pub policies: bool,
}

impl Viewer {
    pub fn new(user_id: &str, permissions: &Permissions) -> Self {
        Self {
            user_id: user_id.to_owned(),
            everyone: permissions.contains(&Permission::AuditView),
            policies: permissions.contains(&Permission::AutomationManage),
        }
    }

    /// Whether the user may see any jobs at all.
    pub fn may_list(permissions: &Permissions) -> bool {
        [
            Permission::ScriptsRun,
            Permission::FilesDeliver,
            Permission::UpdatesView,
            Permission::UpdatesInstall,
            Permission::ApplicationsView,
            Permission::ApplicationsDeploy,
            Permission::AutomationManage,
            Permission::AuditView,
        ]
        .iter()
        .any(|permission| permissions.contains(permission))
    }

    pub fn sees(&self, job: &Job) -> bool {
        self.everyone
            || job.requested_by_user_id == self.user_id
            || (self.policies && job.policy_id.is_some())
    }

    /// Whether the user may cancel what the job hasn't started: the user who
    /// started it, or for a policy's job, those who manage policies.
    pub fn may_cancel(&self, job: &Job) -> bool {
        job.requested_by_user_id == self.user_id && job.policy_id.is_none()
            || (self.policies && job.policy_id.is_some())
    }

    fn condition(&self) -> Option<sea_query::Condition> {
        if self.everyone {
            return None;
        }
        let mut condition = sea_query::Cond::any()
            .add(Expr::col((Jobs::Table, Jobs::RequestedByUserId)).eq(self.user_id.as_str()));
        if self.policies {
            condition = condition.add(Expr::col((Jobs::Table, Jobs::PolicyId)).is_not_null());
        }
        Some(condition)
    }

    pub fn view<'a>(&self, job: &'a Job) -> JobView<'a> {
        JobView {
            job,
            requested_by_you: job.requested_by_user_id == self.user_id,
            can_cancel: !job.finished() && self.may_cancel(job),
        }
    }
}

/// A job as one user sees it.
#[derive(Debug, Serialize)]
pub struct JobView<'a> {
    #[serde(flatten)]
    pub job: &'a Job,
    pub requested_by_you: bool,
    pub can_cancel: bool,
}

/// A device in a job.
#[derive(Debug, Clone, Copy)]
struct Device<'a> {
    id: &'a str,
    platform: Option<DevicePlatform>,
    updates_supported: bool,
}

/// A job to create.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub payload: Payload,
    pub target: Target,
    pub offline: Offline,
    /// How long it waits for offline devices.
    pub wait_ms: i64,
    pub requested_by_user_id: String,
    /// The policy that starts it: its ID and name.
    pub policy: Option<(String, String)>,
    /// Recorded in the audit log as starting it.
    pub actor: Actor,
}

/// A created job, before the dispatcher starts anything: its creation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: String,
    /// The devices written: those planned, less any deleted since.
    pub devices: i64,
}

#[derive(sqlx::FromRow)]
struct DeviceRow {
    id: String,
    name: String,
    platform: String,
    /// `None` before the device's first update scan.
    supported: Option<bool>,
}

/// The devices `target` covers that are enrolled and not deleted, in no
/// particular order: the dispatcher starts a job's devices by ID, and the
/// job's page sorts them by name.
async fn resolve(executor: &mut impl Executor, target: &Target) -> db::Result<Vec<DeviceRow>> {
    let select = || {
        Query::select()
            .columns([
                (Agents::Table, Agents::Id),
                (Agents::Table, Agents::Name),
                (Agents::Table, Agents::Platform),
            ])
            .column((DeviceUpdateStatus::Table, DeviceUpdateStatus::Supported))
            .from(Agents::Table)
            .left_join(
                DeviceUpdateStatus::Table,
                Expr::col((DeviceUpdateStatus::Table, DeviceUpdateStatus::DeviceId))
                    .equals((Agents::Table, Agents::Id)),
            )
            .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
            .to_owned()
    };
    let mut rows: Vec<DeviceRow> = Vec::new();
    match target {
        Target::Devices(ids) => {
            for chunk in ids.chunks(500) {
                rows.extend(
                    executor
                        .fetch_all::<DeviceRow, _>(
                            &select()
                                .and_where(
                                    Expr::col((Agents::Table, Agents::Id))
                                        .is_in(chunk.iter().cloned()),
                                )
                                .to_owned(),
                        )
                        .await?,
                );
            }
        }
        _ => {
            let mut select = select();
            if let Some(condition) = target.condition() {
                select.and_where(condition);
            }
            rows = executor.fetch_all(&select).await?;
        }
    }
    Ok(rows)
}

/// The target in words: the device's name, the companies' or tags' names,
/// or how many devices.
async fn target_label(
    executor: &mut impl Executor,
    target: &Target,
    devices: &[DeviceRow],
) -> db::Result<String> {
    if let Some(names) = target.names(executor).await? {
        return Ok(names);
    }
    Ok(match target {
        Target::Devices(_) if devices.len() == 1 => devices[0].name.clone(),
        Target::Devices(_) => format!("{} devices", devices.len()),
        _ => "All devices".to_owned(),
    })
}

/// From this many devices, planning a job copies the set of connected
/// devices once, provided no more are connected than the job covers. A
/// smaller job asks the hub about each of its devices instead, so a job on a
/// few devices of a large fleet doesn't copy every connected device's ID
/// under the hub's lock.
const COPY_CONNECTED_FROM: usize = 1_000;

/// How planning a job tells whether a device's Agent is connected.
#[derive(Debug)]
enum Connected<'a> {
    /// The hub, asked about each device.
    Ask(&'a AgentHub),
    /// The connected devices, copied once.
    Copied(HashSet<String>),
}

impl<'a> Connected<'a> {
    /// For a job on `devices` devices: a copy when it covers at least
    /// [`COPY_CONNECTED_FROM`] and at least as many as are connected, where
    /// copying is no more work than the lookups it replaces.
    fn new(hub: &'a AgentHub, devices: usize) -> Self {
        if devices >= COPY_CONNECTED_FROM && devices >= hub.count() {
            Self::Copied(hub.connected())
        } else {
            Self::Ask(hub)
        }
    }

    fn contains(&self, device_id: &str) -> bool {
        match self {
            Self::Ask(hub) => hub.is_connected(device_id),
            Self::Copied(connected) => connected.contains(device_id),
        }
    }
}

/// The devices of a planned job that start in the same state with the same
/// detail.
#[derive(Debug, Clone)]
struct Group {
    state: DeviceState,
    detail: Option<String>,
    device_ids: Vec<String>,
}

/// A job decided without writing anything: the devices its target covers,
/// how each starts, and its label. [`plan`] makes it through the readers,
/// which don't wait for the writer; [`write`] stores it.
#[derive(Debug, Clone)]
pub struct Plan {
    job: NewJob,
    /// What happens to offline devices, and how long they are waited for.
    offline: Offline,
    wait_ms: i64,
    label: String,
    groups: Vec<Group>,
    devices: usize,
}

impl Plan {
    /// How many devices it covers.
    pub fn devices(&self) -> usize {
        self.devices
    }

    /// Whether it is written in several transactions: it covers more devices
    /// than one writes.
    pub fn batched(&self) -> bool {
        self.devices > DEVICES_PER_TRANSACTION
    }
}

fn no_devices() -> ApiError {
    ApiError::bad_request("there are no devices to run on").with_code("no_devices")
}

/// Plans `job`: the devices its target covers that are enrolled and not
/// deleted, and for each whether it is skipped (it can't take the job),
/// queued (its Agent is connected), or waits or is missed. It only reads, so
/// callers pass the database itself rather than a transaction, and the
/// writer stays free however many devices there are. Fails with
/// `no_devices` if the target covers none.
pub async fn plan(
    executor: &mut impl Executor,
    agents: &AgentHub,
    job: NewJob,
) -> Result<Plan, ApiError> {
    let devices = resolve(executor, &job.target).await?;
    if devices.is_empty() {
        return Err(no_devices());
    }
    let label = target_label(executor, &job.target, &devices).await?;
    // A wake job is for offline devices; it never waits for them.
    let offline = if job.payload == Payload::Wake {
        Offline::Skip
    } else {
        job.offline
    };
    let wait_ms = if offline == Offline::Wait {
        job.wait_ms.clamp(HOUR_MS, MAX_WAIT_HOURS * HOUR_MS)
    } else {
        0
    };
    let count = devices.len();
    let connected = Connected::new(agents, count);
    // A few groups: one a state, and one for each reason devices are skipped.
    let mut groups: Vec<Group> = Vec::new();
    for row in devices {
        let device = Device {
            id: &row.id,
            platform: DevicePlatform::parse(&row.platform),
            updates_supported: row.supported.unwrap_or(true),
        };
        let (state, detail) = initial_state(&job, device, &connected);
        let index = match groups
            .iter()
            .position(|group| group.state == state && group.detail == detail)
        {
            Some(index) => index,
            None => {
                groups.push(Group {
                    state,
                    detail,
                    device_ids: Vec::new(),
                });
                groups.len() - 1
            }
        };
        groups[index].device_ids.push(row.id);
    }
    // In ID order, each transaction of a job's devices adds to one stretch
    // of each index on `job_devices`, rather than to pages all over them:
    // SQLite writes back each page a transaction changed when it checkpoints,
    // so random IDs had every batch rewrite most of the indexes.
    for group in &mut groups {
        group.device_ids.sort_unstable();
    }
    Ok(Plan {
        job,
        offline,
        wait_ms,
        label,
        groups,
        devices: count,
    })
}

/// A payload's own columns in its job's row.
#[derive(Default)]
struct Columns<'a> {
    language: Option<&'a str>,
    body: Option<&'a str>,
    timeout_seconds: Option<i64>,
    run_as: Option<&'a str>,
    destination: Option<&'a str>,
    categories: Option<&'a [UpdateCategory]>,
    reboot: Option<&'a str>,
    reboot_delay_minutes: Option<i64>,
}

impl<'a> Columns<'a> {
    fn of(payload: &'a Payload) -> Self {
        match payload {
            Payload::Script {
                language,
                body,
                timeout_seconds,
                run_as,
                ..
            } => Self {
                language: Some(language.as_str()),
                body: Some(body.as_str()),
                timeout_seconds: Some(*timeout_seconds),
                run_as: Some(run_as.as_str()),
                ..Self::default()
            },
            Payload::File { destination, .. } => Self {
                destination: Some(destination.as_str()),
                ..Self::default()
            },
            Payload::UpdateInstall {
                categories,
                reboot,
                reboot_delay_minutes,
            } => Self {
                categories: Some(categories.as_slice()),
                reboot: Some(reboot.as_str()),
                reboot_delay_minutes: Some(i64::from(*reboot_delay_minutes)),
                ..Self::default()
            },
            Payload::UpdateScan | Payload::Wake | Payload::Applications(_) => Self::default(),
        }
    }
}

/// Inserts those of `device_ids` that are still enrolled and not deleted
/// into the job `job_id`, as `group` starts. The database leaves out a
/// device deleted after planning rather than failing the job.
fn insert_devices(job_id: &str, group: &Group, device_ids: &[String], now: i64) -> InsertStatement {
    Query::insert()
        .into_table(JobDevices::Table)
        .columns([
            JobDevices::JobId,
            JobDevices::DeviceId,
            JobDevices::State,
            JobDevices::Detail,
            JobDevices::UpdatedAt,
        ])
        .select_from(
            Query::select()
                .expr(Expr::val(job_id))
                .column((Agents::Table, Agents::Id))
                .expr(Expr::val(group.state.as_str()))
                .expr(Expr::val(group.detail.clone()))
                .expr(Expr::val(now))
                .from(Agents::Table)
                .and_where(Expr::col((Agents::Table, Agents::Id)).is_in(device_ids.iter().cloned()))
                .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
                .to_owned(),
        )
        .expect("as many values as columns")
        .to_owned()
}

/// A job being created: its row is written, marked as being created
/// (`jobs.creating`), by [`draft`]; its devices next, by
/// [`Draft::write_devices`]; and [`Draft::finish`] clears the mark. While it
/// is marked, nothing starts, queues, counts or shows the job; while the
/// draft lives, the engine knows this server is creating it, and once it is
/// gone a job still marked is removed ([`discard_abandoned`]).
#[derive(Debug)]
pub struct Draft<'a> {
    plan: &'a Plan,
    id: String,
    now: i64,
    /// The devices written so far, by how they start.
    counts: HashMap<DeviceState, i64>,
    _creating: Creating,
}

/// Writes the row of a planned job as part of the caller's transaction,
/// marked as being created, with its applications if it has them.
pub async fn draft<'a>(
    executor: &mut impl Executor,
    engine: &Engine,
    plan: &'a Plan,
    now: i64,
) -> db::Result<Draft<'a>> {
    let job = &plan.job;
    let id = new_id();
    // Held before the row exists, so the engine never takes the job for one
    // whose creation stopped.
    let creating = engine.hold_creation(&id);
    let columns = Columns::of(&job.payload);
    let (item_id, item_name) = job.payload.item();
    executor
        .execute(
            &Query::insert()
                .into_table(Jobs::Table)
                .columns([
                    Jobs::Id,
                    Jobs::Kind,
                    Jobs::ItemId,
                    Jobs::ItemName,
                    Jobs::Language,
                    Jobs::Body,
                    Jobs::TimeoutSeconds,
                    Jobs::RunAs,
                    Jobs::Destination,
                    Jobs::UpdateCategories,
                    Jobs::Reboot,
                    Jobs::RebootDelayMinutes,
                    Jobs::Target,
                    Jobs::TargetLabel,
                    Jobs::Offline,
                    Jobs::ExpiresAt,
                    Jobs::RequestedByUserId,
                    Jobs::PolicyId,
                    Jobs::PolicyName,
                    Jobs::CreatedAt,
                    Jobs::Creating,
                ])
                .values_panic([
                    id.as_str().into(),
                    job.payload.kind().as_str().into(),
                    item_id.into(),
                    item_name.as_str().into(),
                    columns.language.into(),
                    columns.body.into(),
                    columns.timeout_seconds.into(),
                    columns.run_as.into(),
                    columns.destination.into(),
                    columns.categories.and_then(Install::categories_text).into(),
                    columns.reboot.into(),
                    columns.reboot_delay_minutes.into(),
                    job.target.kind().as_str().into(),
                    plan.label.as_str().into(),
                    plan.offline.as_str().into(),
                    (now + plan.wait_ms).into(),
                    job.requested_by_user_id.as_str().into(),
                    job.policy.as_ref().map(|(id, _)| id.as_str()).into(),
                    job.policy.as_ref().map(|(_, name)| name.as_str()).into(),
                    now.into(),
                    true.into(),
                ])
                .to_owned(),
        )
        .await?;
    if let Payload::Applications(spec) = &job.payload {
        crate::applications::jobs::save(executor, &id, spec).await?;
    }
    Ok(Draft {
        plan,
        id,
        now,
        counts: HashMap::new(),
        _creating: creating,
    })
}

impl Draft<'_> {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Writes the job's devices, grouped by how they start, a statement for
    /// each [`INSERT_CHUNK`]. A job that isn't [`batched`](Plan::batched)
    /// has them written in `transaction`, the one that wrote its row, which
    /// is returned to finish it in. Otherwise `transaction` is committed, so
    /// the job is stored marked as being created, its devices are written
    /// [`DEVICES_PER_TRANSACTION`] to a transaction of their own, and a new
    /// transaction is returned to finish it in. Each batch's insert leaves
    /// out the devices deleted since planning, as the planned job's would.
    pub async fn write_devices(
        &mut self,
        state: &AppState,
        transaction: Transaction,
    ) -> db::Result<Transaction> {
        let plan = self.plan;
        let batched = plan.batched();
        let mut batch = if batched {
            transaction.commit().await?;
            state.database.begin().await?
        } else {
            transaction
        };
        // Devices in this transaction so far. A statement takes what is left
        // of its group, up to a chunk and what fits in the transaction.
        let mut devices = 0;
        for group in &plan.groups {
            let mut rest = group.device_ids.as_slice();
            while !rest.is_empty() {
                if devices == DEVICES_PER_TRANSACTION {
                    batch.commit().await?;
                    state.automation.devices_written(&self.id, devices).await;
                    batch = state.database.begin().await?;
                    devices = 0;
                }
                let take = rest
                    .len()
                    .min(INSERT_CHUNK)
                    .min(DEVICES_PER_TRANSACTION - devices);
                let (chunk, after) = rest.split_at(take);
                self.insert(&mut batch, group, chunk).await?;
                devices += take;
                rest = after;
            }
        }
        if !batched {
            return Ok(batch);
        }
        batch.commit().await?;
        state.automation.devices_written(&self.id, devices).await;
        state.database.begin().await
    }

    async fn insert(
        &mut self,
        executor: &mut impl Executor,
        group: &Group,
        device_ids: &[String],
    ) -> db::Result<()> {
        let written = executor
            .execute(&insert_devices(&self.id, group, device_ids, self.now))
            .await?;
        *self.counts.entry(group.state).or_default() += i64::try_from(written).unwrap_or(i64::MAX);
        Ok(())
    }

    /// Ends the job's creation as part of the caller's transaction: clears
    /// its mark and records who started it, with its counts, in the audit
    /// log. Nothing else is read or written unless devices were deleted
    /// since planning: they were left out, the counts are of the devices
    /// written, and a job on chosen devices is labelled by those left. If
    /// every device was deleted, the job is removed and this fails with
    /// `no_devices`.
    pub async fn finish(self, executor: &mut impl Executor) -> Result<Created, ApiError> {
        let Self {
            plan, id, counts, ..
        } = self;
        let job = &plan.job;
        let devices = counts.values().sum::<i64>();
        if devices == 0 {
            // Every device was deleted since planning. Its applications go too.
            executor
                .execute(
                    &Query::delete()
                        .from_table(Jobs::Table)
                        .and_where(Expr::col(Jobs::Id).eq(id.as_str()))
                        .to_owned(),
                )
                .await?;
            return Err(no_devices());
        }
        let label = if usize::try_from(devices).is_ok_and(|devices| devices == plan.devices) {
            plan.label.clone()
        } else {
            relabel(executor, plan, &id, devices).await?
        };
        let cleared = executor
            .execute(
                &Query::update()
                    .table(Jobs::Table)
                    .value(Jobs::Creating, false)
                    .value(Jobs::TargetLabel, label.as_str())
                    .and_where(Expr::col(Jobs::Id).eq(id.as_str()))
                    .and_where(Expr::col(Jobs::Creating))
                    .to_owned(),
            )
            .await?;
        if cleared != 1 {
            // Only a job this server isn't creating is removed, so another
            // server on the same database took it for one whose creation
            // stopped.
            tracing::error!(job_id = id, "a job was removed while it was being created");
            return Err(ApiError::internal());
        }
        let columns = Columns::of(&job.payload);
        let (item_id, item_name) = job.payload.item();
        let mut metadata = json!({
            "kind": job.payload.kind().as_str(),
            "item_id": item_id,
            "item_name": item_name,
            "target": job.target.kind().as_str(),
            "target_label": label,
            "devices": devices,
            "offline": plan.offline.as_str(),
            "queued": counts.get(&DeviceState::Queued).copied().unwrap_or(0),
            "waiting": counts.get(&DeviceState::Waiting).copied().unwrap_or(0),
            "skipped": counts.get(&DeviceState::Skipped).copied().unwrap_or(0),
        });
        if let Some(run_as) = columns.run_as {
            metadata["run_as"] = run_as.into();
        }
        if let Some(destination) = columns.destination {
            metadata["destination"] = destination.into();
        }
        if let Some(categories) = columns.categories {
            metadata["categories"] = json!(categories);
        }
        if let Some(reboot) = columns.reboot {
            metadata["reboot"] = reboot.into();
            metadata["reboot_delay_minutes"] = columns.reboot_delay_minutes.into();
        }
        if let Payload::Applications(spec) = &job.payload {
            metadata["applications"] = spec.audit();
        }
        if let Some((policy_id, policy_name)) = &job.policy {
            metadata["policy_id"] = policy_id.as_str().into();
            metadata["policy_name"] = policy_name.as_str().into();
            metadata["requested_by_user_id"] = job.requested_by_user_id.as_str().into();
        }
        audit::record(
            executor,
            &job.actor,
            "job.start",
            AuditTarget::job(&id),
            metadata,
        )
        .await?;
        Ok(Created { id, devices })
    }
}

/// Creates a planned job: [`draft`], [`Draft::write_devices`] and
/// [`Draft::finish`], in one transaction unless the job is
/// [`batched`](Plan::batched). If a later transaction of a batched job
/// fails, the job is removed again and the error returned.
pub async fn create(state: &AppState, plan: &Plan, now: i64) -> Result<Created, ApiError> {
    let mut transaction = state.database.begin().await?;
    let mut draft = draft(&mut transaction, &state.automation, plan, now).await?;
    let id = draft.id.clone();
    let created = async {
        let mut transaction = draft.write_devices(state, transaction).await?;
        let created = draft.finish(&mut transaction).await;
        // A job whose every device was deleted is removed with that.
        if created.is_ok() || created.as_ref().err().and_then(ApiError::code) == Some("no_devices")
        {
            transaction.commit().await?;
        }
        created
    }
    .await;
    if created.is_err() && plan.batched() {
        abandon(state, &id).await;
    }
    created
}

/// Removes a job whose creation failed, now, or on the engine's next full
/// pass if that fails too.
pub async fn abandon(state: &AppState, job_id: &str) {
    if let Err(error) = discard(&state.database, job_id).await {
        tracing::warn!(
            job_id,
            %error,
            "could not remove a job whose creation failed; the engine will"
        );
    }
}

/// Removes the job `job_id` if it is still marked as being created: its
/// devices [`DEVICES_PER_TRANSACTION`] to a transaction, then the job with
/// its applications. Each transaction takes the job's row first, so one
/// whose creation ends meanwhile is waited for and then kept. Returns
/// whether the job was removed.
pub async fn discard(database: &Database, job_id: &str) -> db::Result<bool> {
    loop {
        let mut transaction = database.begin().await?;
        let marked = transaction
            .execute(
                &Query::update()
                    .table(Jobs::Table)
                    .value(Jobs::Creating, true)
                    .and_where(Expr::col(Jobs::Id).eq(job_id))
                    .and_where(Expr::col(Jobs::Creating))
                    .to_owned(),
            )
            .await?;
        if marked == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        let deleted = transaction
            .execute(
                &Query::delete()
                    .from_table(JobDevices::Table)
                    .and_where(Expr::col(JobDevices::JobId).eq(job_id))
                    .and_where(
                        Expr::col(JobDevices::DeviceId).in_subquery(
                            Query::select()
                                .column(JobDevices::DeviceId)
                                .from(JobDevices::Table)
                                .and_where(Expr::col(JobDevices::JobId).eq(job_id))
                                .limit(DEVICES_PER_TRANSACTION as u64)
                                .to_owned(),
                        ),
                    )
                    .to_owned(),
            )
            .await?;
        if deleted == 0 {
            transaction
                .execute(
                    &Query::delete()
                        .from_table(Jobs::Table)
                        .and_where(Expr::col(Jobs::Id).eq(job_id))
                        .to_owned(),
                )
                .await?;
            transaction.commit().await?;
            return Ok(true);
        }
        transaction.commit().await?;
    }
}

/// The jobs marked as being created, from their own index: none, or the
/// few being created now.
pub fn creating_query() -> sea_query::SelectStatement {
    Query::select()
        .column((Jobs::Table, Jobs::Id))
        .from(Jobs::Table)
        .and_where(Expr::col((Jobs::Table, Jobs::Creating)))
        .to_owned()
}

/// Removes the jobs whose creation stopped part-way: those marked as being
/// created that this server isn't creating, because the server stopped, or
/// the request or policy firing creating it ended, between its
/// transactions. After a restart that is every job still marked. Returns
/// how many it removed.
pub async fn discard_abandoned(state: &AppState) -> db::Result<usize> {
    let marked: Vec<(String,)> = state.database.fetch_all(&creating_query()).await?;
    let mut discarded = 0;
    for (id,) in marked {
        if state.automation.is_creating(&id) {
            continue;
        }
        if discard(&state.database, &id).await? {
            tracing::warn!(job_id = id, "removed a job whose creation stopped part-way");
            discarded += 1;
        }
    }
    Ok(discarded)
}

/// The label of a job written with fewer devices than planned. One on chosen
/// devices is labelled by those left, as if the others had been deleted
/// before planning; other targets name their companies or tags, or every
/// device, whatever the count.
async fn relabel(
    executor: &mut impl Executor,
    plan: &Plan,
    job_id: &str,
    devices: i64,
) -> db::Result<String> {
    if !matches!(plan.job.target, Target::Devices(_)) {
        return Ok(plan.label.clone());
    }
    Ok(if devices == 1 {
        let (name,): (String,) = executor
            .fetch_one(
                &Query::select()
                    .column((Agents::Table, Agents::Name))
                    .from(JobDevices::Table)
                    .inner_join(
                        Agents::Table,
                        Expr::col((Agents::Table, Agents::Id))
                            .equals((JobDevices::Table, JobDevices::DeviceId)),
                    )
                    .and_where(Expr::col((JobDevices::Table, JobDevices::JobId)).eq(job_id))
                    .to_owned(),
            )
            .await?;
        name
    } else {
        format!("{devices} devices")
    })
}

fn initial_state(
    job: &NewJob,
    device: Device<'_>,
    connected: &Connected<'_>,
) -> (DeviceState, Option<String>) {
    if job.payload == Payload::Wake {
        return if connected.contains(device.id) {
            (
                DeviceState::Skipped,
                Some("The device is online already".to_owned()),
            )
        } else {
            (DeviceState::Queued, None)
        };
    }
    if let Some(reason) = job
        .payload
        .unsupported_on(device.platform, device.updates_supported)
    {
        (DeviceState::Skipped, Some(reason))
    } else if connected.contains(device.id) {
        (DeviceState::Queued, None)
    } else if job.offline == Offline::Wait {
        (DeviceState::Waiting, None)
    } else {
        (
            DeviceState::Missed,
            Some("The device was offline".to_owned()),
        )
    }
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: String,
    kind: String,
    item_id: String,
    item_name: String,
    language: Option<String>,
    run_as: Option<String>,
    destination: Option<String>,
    update_categories: Option<String>,
    reboot: Option<String>,
    reboot_delay_minutes: Option<i64>,
    target: String,
    target_label: String,
    offline: String,
    expires_at: i64,
    requested_by_user_id: String,
    requested_by: Option<String>,
    policy_id: Option<String>,
    policy_name: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

/// The SQL that is true of a job whose creation ended: every job but those
/// whose devices are still being written. Whatever starts, queues, counts,
/// finishes or shows jobs reads only these.
pub fn created() -> Expr {
    Expr::col((Jobs::Table, Jobs::Creating)).not()
}

fn job_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            (Jobs::Table, Jobs::Id),
            (Jobs::Table, Jobs::Kind),
            (Jobs::Table, Jobs::ItemId),
            (Jobs::Table, Jobs::ItemName),
            (Jobs::Table, Jobs::Language),
            (Jobs::Table, Jobs::RunAs),
            (Jobs::Table, Jobs::Destination),
            (Jobs::Table, Jobs::UpdateCategories),
            (Jobs::Table, Jobs::Reboot),
            (Jobs::Table, Jobs::RebootDelayMinutes),
            (Jobs::Table, Jobs::Target),
            (Jobs::Table, Jobs::TargetLabel),
            (Jobs::Table, Jobs::Offline),
            (Jobs::Table, Jobs::ExpiresAt),
            (Jobs::Table, Jobs::RequestedByUserId),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "requested_by",
        )
        .columns([
            (Jobs::Table, Jobs::PolicyId),
            (Jobs::Table, Jobs::PolicyName),
            (Jobs::Table, Jobs::CreatedAt),
            (Jobs::Table, Jobs::FinishedAt),
        ])
        .from(Jobs::Table)
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id)).equals((Jobs::Table, Jobs::RequestedByUserId)),
        )
        // Shown, counted and finished only once created.
        .and_where(created())
        .to_owned()
}

impl JobRow {
    /// The job, or `None` (logged) if the row is invalid.
    fn into_job(self, counts: Counts) -> Option<Job> {
        let (Some(kind), Some(target), Some(offline)) = (
            Kind::parse(&self.kind),
            TargetKind::parse(&self.target),
            Offline::parse(&self.offline),
        ) else {
            tracing::error!(job_id = self.id, "stored job is invalid");
            return None;
        };
        let id = self.id;
        Some(Job {
            kind,
            language: self.language.as_deref().and_then(ScriptLanguage::parse),
            run_as: self.run_as.as_deref().and_then(RunAs::parse),
            destination: self
                .destination
                .as_deref()
                .and_then(FileDeliveryDestination::parse),
            categories: (kind == Kind::UpdateInstall)
                .then(|| Install::parse_categories(self.update_categories.as_deref())),
            reboot: self.reboot.as_deref().and_then(RebootBehavior::parse),
            reboot_delay_minutes: self.reboot_delay_minutes,
            target,
            offline,
            item_id: self.item_id,
            item_name: self.item_name,
            target_label: self.target_label,
            expires_at_unix_ms: self.expires_at,
            requested_by: self
                .requested_by
                .unwrap_or_else(|| "A removed user".to_owned()),
            requested_by_user_id: self.requested_by_user_id,
            policy_id: self.policy_id,
            policy_name: self.policy_name,
            created_at_unix_ms: self.created_at,
            finished_at_unix_ms: self.finished_at,
            counts,
            id,
        })
    }
}

/// Jobs the viewer sees, newest first, optionally only a policy's, with
/// their counts.
pub async fn list(
    executor: &mut impl Executor,
    viewer: &Viewer,
    policy_id: Option<&str>,
    limit: u64,
    now: i64,
) -> db::Result<Vec<Job>> {
    let mut select = job_select();
    if let Some(condition) = viewer.condition() {
        select.cond_where(condition);
    }
    if let Some(policy_id) = policy_id {
        select.and_where(Expr::col((Jobs::Table, Jobs::PolicyId)).eq(policy_id));
    }
    select
        .order_by((Jobs::Table, Jobs::CreatedAt), Order::Desc)
        .order_by((Jobs::Table, Jobs::Id), Order::Desc)
        .limit(limit);
    let rows: Vec<JobRow> = executor.fetch_all(&select).await?;
    with_counts(executor, rows, now).await
}

/// Jobs by ID, with their counts, whoever started them: [`QUEUE_CHUNK`]
/// to a statement, however many, such as every unfinished job.
pub async fn by_ids(
    executor: &mut impl Executor,
    ids: &[String],
    now: i64,
) -> db::Result<Vec<Job>> {
    let mut rows: Vec<JobRow> = Vec::new();
    for chunk in ids.chunks(QUEUE_CHUNK) {
        rows.extend(
            executor
                .fetch_all::<JobRow, _>(
                    &job_select()
                        .and_where(Expr::col((Jobs::Table, Jobs::Id)).is_in(chunk.iter().cloned()))
                        .to_owned(),
                )
                .await?,
        );
    }
    with_counts(executor, rows, now).await
}

pub async fn get(executor: &mut impl Executor, id: &str, now: i64) -> db::Result<Option<Job>> {
    Ok(by_ids(executor, &[id.to_owned()], now)
        .await?
        .into_iter()
        .next())
}

/// The IDs of jobs that haven't finished.
pub async fn unfinished(executor: &mut impl Executor) -> db::Result<Vec<String>> {
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(Jobs::Id)
                .from(Jobs::Table)
                .and_where(Expr::col(Jobs::FinishedAt).is_null())
                .and_where(created())
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The unfinished jobs these devices are waiting, queued or started in.
pub async fn unfinished_of_devices(
    executor: &mut impl Executor,
    device_ids: &[String],
) -> db::Result<Vec<String>> {
    let mut ids = Vec::new();
    for chunk in device_ids.chunks(500) {
        let rows: Vec<(String,)> = executor
            .fetch_all(
                &Query::select()
                    .distinct()
                    .column((JobDevices::Table, JobDevices::JobId))
                    .from(JobDevices::Table)
                    .inner_join(
                        Jobs::Table,
                        Expr::col((Jobs::Table, Jobs::Id))
                            .equals((JobDevices::Table, JobDevices::JobId)),
                    )
                    // Naming the states lets the lookup use the index on
                    // (state, device_id).
                    .and_where(Expr::col((JobDevices::Table, JobDevices::State)).is_in([
                        DeviceState::Waiting.as_str(),
                        DeviceState::Queued.as_str(),
                        DeviceState::Started.as_str(),
                    ]))
                    .and_where(
                        Expr::col((JobDevices::Table, JobDevices::DeviceId))
                            .is_in(chunk.iter().cloned()),
                    )
                    .and_where(Expr::col((Jobs::Table, Jobs::FinishedAt)).is_null())
                    .and_where(created())
                    .to_owned(),
            )
            .await?;
        ids.extend(rows.into_iter().map(|(id,)| id));
    }
    Ok(ids)
}

async fn with_counts(
    executor: &mut impl Executor,
    rows: Vec<JobRow>,
    now: i64,
) -> db::Result<Vec<Job>> {
    let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
    let mut counts = counts(executor, &ids, now).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let counts = counts.remove(&row.id).unwrap_or_default();
            row.into_job(counts)
        })
        .collect())
}

/// Joins a job's device to its run, delivery, update run, application run or
/// wake request, for what the job's page shows of it. The device's outcome
/// doesn't need them: it is stored with the device.
fn join_runs(select: &mut sea_query::SelectStatement) {
    let part = |job_id: Expr, device_id: Expr| {
        job_id
            .equals((JobDevices::Table, JobDevices::JobId))
            .and(device_id.equals((JobDevices::Table, JobDevices::DeviceId)))
    };
    select
        .left_join(
            ScriptRuns::Table,
            part(
                Expr::col((ScriptRuns::Table, ScriptRuns::JobId)),
                Expr::col((ScriptRuns::Table, ScriptRuns::DeviceId)),
            ),
        )
        .left_join(
            FileDeliveries::Table,
            part(
                Expr::col((FileDeliveries::Table, FileDeliveries::JobId)),
                Expr::col((FileDeliveries::Table, FileDeliveries::DeviceId)),
            ),
        )
        .left_join(
            UpdateRuns::Table,
            part(
                Expr::col((UpdateRuns::Table, UpdateRuns::JobId)),
                Expr::col((UpdateRuns::Table, UpdateRuns::DeviceId)),
            ),
        )
        .left_join(
            WakeRequests::Table,
            part(
                Expr::col((WakeRequests::Table, WakeRequests::JobId)),
                Expr::col((WakeRequests::Table, WakeRequests::DeviceId)),
            ),
        )
        .left_join(
            ApplicationRuns::Table,
            part(
                Expr::col((ApplicationRuns::Table, ApplicationRuns::JobId)),
                Expr::col((ApplicationRuns::Table, ApplicationRuns::DeviceId)),
            ),
        );
}

/// SQL that is 1 for a started device's report that was due before `now`,
/// or that it has no run to report, and 0 otherwise.
fn overdue_sql(now: i64) -> String {
    format!(
        "CASE WHEN job_devices.report_by IS NULL OR job_devices.report_by < {now} THEN 1 ELSE 0 END"
    )
}

/// The statement [`counts`] reads the counts of up to 200 jobs with: a
/// row per job and state, from `idx_job_devices_job` alone, with the started
/// devices' results summed in columns, so the rows come grouped in the
/// index's order without sorting a job's devices.
pub fn counts_query(ids: &[String], now: i64) -> sea_query::SelectStatement {
    let sum = |condition: String| {
        Expr::cust(format!(
            "CAST(SUM(CASE WHEN {condition} THEN 1 ELSE 0 END) AS BIGINT)"
        ))
    };
    let result = |outcome: Outcome| sum(format!("job_devices.result = '{}'", outcome.as_str()));
    Query::select()
        .columns([
            (JobDevices::Table, JobDevices::JobId),
            (JobDevices::Table, JobDevices::State),
        ])
        .expr_as(Expr::cust("CAST(COUNT(*) AS BIGINT)"), "devices")
        .expr_as(result(Outcome::Succeeded), "succeeded")
        .expr_as(result(Outcome::Failed), "failed")
        .expr_as(result(Outcome::TimedOut), "timed_out")
        .expr_as(result(Outcome::Skipped), "skipped")
        .expr_as(
            sum(format!(
                "job_devices.result IS NULL AND {} = 0",
                overdue_sql(now)
            )),
            "running",
        )
        .from(JobDevices::Table)
        .and_where(Expr::col((JobDevices::Table, JobDevices::JobId)).is_in(ids.iter().cloned()))
        .group_by_col((JobDevices::Table, JobDevices::JobId))
        .group_by_col((JobDevices::Table, JobDevices::State))
        .to_owned()
}

#[derive(sqlx::FromRow)]
struct CountRow {
    job_id: String,
    state: String,
    devices: i64,
    succeeded: Option<i64>,
    failed: Option<i64>,
    timed_out: Option<i64>,
    skipped: Option<i64>,
    running: Option<i64>,
}

impl CountRow {
    /// Adds the row's devices to `counts`: those of a state that isn't
    /// started by their state, and started ones by their result; the rest of
    /// those are lost or overdue, with no result.
    fn add_to(&self, counts: &mut Counts) {
        let Some(state) = DeviceState::parse(&self.state) else {
            return;
        };
        if state != DeviceState::Started {
            counts.add(outcome(state, None, false), self.devices);
            return;
        }
        let mut rest = self.devices;
        for (outcome, devices) in [
            (Outcome::Succeeded, self.succeeded),
            (Outcome::Failed, self.failed),
            (Outcome::TimedOut, self.timed_out),
            (Outcome::Skipped, self.skipped),
            (Outcome::Running, self.running),
        ] {
            let devices = devices.unwrap_or(0);
            counts.add(outcome, devices);
            rest -= devices;
        }
        counts.add(Outcome::NoResult, rest);
    }
}

/// Each job's counts: an index read of each job's devices, joined to
/// nothing.
pub async fn counts(
    executor: &mut impl Executor,
    ids: &[String],
    now: i64,
) -> db::Result<HashMap<String, Counts>> {
    let mut counts: HashMap<String, Counts> = HashMap::new();
    for chunk in ids.chunks(200) {
        let rows: Vec<CountRow> = executor.fetch_all(&counts_query(chunk, now)).await?;
        for row in rows {
            row.add_to(counts.entry(row.job_id.clone()).or_default());
        }
    }
    Ok(counts)
}

/// A device's part in a job, as the job's page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobDevice {
    pub device_id: String,
    /// `None` once the device is gone for good.
    pub device_name: Option<String>,
    pub platform: Option<DevicePlatform>,
    pub outcome: Outcome,
    /// Why it was skipped or missed, or the run's or delivery's error.
    pub detail: Option<String>,
    /// The device's run, for its output at `/v1/script-runs/{id}`.
    pub run_id: Option<String>,
    pub delivery_id: Option<String>,
    /// The device's update scan or install, at `/v1/update-runs/{id}`.
    pub update_run_id: Option<String>,
    /// The device's wake request, at `/v1/wakes?ids={id}`.
    pub wake_id: Option<String>,
    /// The device's application run, at `/v1/application-runs/{id}`.
    pub application_run_id: Option<String>,
    /// The applications that failed or were skipped on its application
    /// run, in order, with why.
    pub applications: Vec<ApplicationProblem>,
    pub exit_code: Option<i64>,
    /// Where a delivered file was saved.
    pub path: Option<String>,
    pub started_at_unix_ms: Option<i64>,
    pub completed_at_unix_ms: Option<i64>,
}

/// An application that failed or was skipped on a device's run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct ApplicationProblem {
    #[serde(skip)]
    pub run_id: String,
    pub name: String,
    /// `failed` or `skipped`.
    pub outcome: String,
    pub error: Option<String>,
}

#[derive(sqlx::FromRow)]
struct JobDeviceRow {
    device_id: String,
    device_name: Option<String>,
    platform: Option<String>,
    state: String,
    detail: Option<String>,
    run_id: Option<String>,
    delivery_id: Option<String>,
    update_run_id: Option<String>,
    wake_id: Option<String>,
    application_run_id: Option<String>,
    updates_installed: Option<i64>,
    updates_failed: Option<i64>,
    applications_failed: Option<i64>,
    exit_code: Option<i64>,
    error: Option<String>,
    path: Option<String>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    result: Option<String>,
    overdue: i64,
}

/// Which of a job's devices a page holds: those with `outcome`, or every
/// one, in device ID order, from after the device `after` (if any), less the
/// first `offset`, at most `limit`.
#[derive(Debug, Clone, Default)]
pub struct DevicePage {
    pub outcome: Option<Outcome>,
    pub after: Option<String>,
    pub offset: u64,
    pub limit: u64,
}

/// Whether a job's device has `outcome` at `now`, from its row of
/// `job_devices` alone: [`outcome`] as SQL, bucketing the rows as
/// [`counts_query`] does.
pub fn has_outcome(outcome: Outcome, now: i64) -> sea_query::Condition {
    use sea_query::Cond;
    let column = |column: JobDevices| Expr::col((JobDevices::Table, column));
    let state = |state: DeviceState| Cond::all().add(column(JobDevices::State).eq(state.as_str()));
    let ended = |result: Outcome| {
        state(DeviceState::Started).add(column(JobDevices::Result).eq(result.as_str()))
    };
    match outcome {
        Outcome::Queued => state(DeviceState::Queued),
        Outcome::Waiting => state(DeviceState::Waiting),
        Outcome::Missed => state(DeviceState::Missed),
        Outcome::Cancelled => state(DeviceState::Cancelled),
        // Not run for the device's platform, or none of the applications
        // was for it.
        Outcome::Skipped => Cond::any()
            .add(state(DeviceState::Skipped))
            .add(ended(Outcome::Skipped)),
        Outcome::Succeeded | Outcome::Failed | Outcome::TimedOut => ended(outcome),
        // Awaiting a report that isn't due yet.
        Outcome::Running => state(DeviceState::Started)
            .add(column(JobDevices::Result).is_null())
            .add(column(JobDevices::ReportBy).gte(now)),
        // Lost, or awaiting a report that was due, or with no run to report.
        Outcome::NoResult => state(DeviceState::Started).add(
            Cond::any()
                .add(column(JobDevices::Result).eq(Outcome::NoResult.as_str()))
                .add(
                    Cond::all().add(column(JobDevices::Result).is_null()).add(
                        Cond::any()
                            .add(column(JobDevices::ReportBy).is_null())
                            .add(column(JobDevices::ReportBy).lt(now)),
                    ),
                ),
        ),
    }
}

/// The page of a job's rows of `job_devices` that [`devices_query`] joins:
/// cut in device ID order from the job's key, `idx_job_devices_job` or
/// `idx_job_devices_result`, so it reads about as many index entries as the
/// page holds, past `offset`. Those that need more say so in
/// `docs/automation.md`.
pub fn page_query(job_id: &str, page: &DevicePage, now: i64) -> sea_query::SelectStatement {
    let mut rows = Query::select();
    rows.columns([
        (JobDevices::Table, JobDevices::JobId),
        (JobDevices::Table, JobDevices::DeviceId),
        (JobDevices::Table, JobDevices::State),
        (JobDevices::Table, JobDevices::Detail),
        (JobDevices::Table, JobDevices::Result),
        (JobDevices::Table, JobDevices::ReportBy),
    ])
    .from(JobDevices::Table)
    .and_where(Expr::col((JobDevices::Table, JobDevices::JobId)).eq(job_id));
    if let Some(outcome) = page.outcome {
        rows.cond_where(has_outcome(outcome, now));
    }
    if let Some(after) = &page.after {
        rows.and_where(Expr::col((JobDevices::Table, JobDevices::DeviceId)).gt(after.as_str()));
    }
    rows.order_by((JobDevices::Table, JobDevices::DeviceId), Order::Asc)
        .limit(page.limit)
        .offset(page.offset)
        .to_owned()
}

/// The statement [`devices`] reads a page of a job's devices with: the
/// page is cut first ([`page_query`]), then only its rows are joined to
/// their devices and runs, and put back in order. The page is named as the
/// table it is cut from, so the joins and columns read as on the table.
pub fn devices_query(job_id: &str, page: &DevicePage, now: i64) -> sea_query::SelectStatement {
    let mut select = Query::select();
    select
        .column((JobDevices::Table, JobDevices::DeviceId))
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .column((Agents::Table, Agents::Platform))
        .columns([
            (JobDevices::Table, JobDevices::State),
            (JobDevices::Table, JobDevices::Detail),
        ])
        .expr_as(Expr::col((ScriptRuns::Table, ScriptRuns::Id)), "run_id")
        .expr_as(
            Expr::col((FileDeliveries::Table, FileDeliveries::Id)),
            "delivery_id",
        )
        .expr_as(Expr::col((UpdateRuns::Table, UpdateRuns::Id)), "update_run_id")
        .expr_as(Expr::col((WakeRequests::Table, WakeRequests::Id)), "wake_id")
        .expr_as(
            Expr::col((ApplicationRuns::Table, ApplicationRuns::Id)),
            "application_run_id",
        )
        .expr_as(
            Expr::col((ApplicationRuns::Table, ApplicationRuns::Failed)),
            "applications_failed",
        )
        .expr_as(
            Expr::col((UpdateRuns::Table, UpdateRuns::Installed)),
            "updates_installed",
        )
        .expr_as(
            Expr::col((UpdateRuns::Table, UpdateRuns::Failed)),
            "updates_failed",
        )
        .column((ScriptRuns::Table, ScriptRuns::ExitCode))
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.error, file_deliveries.error, update_runs.error, \
                 application_runs.error, wake_requests.detail)",
            ),
            "error",
        )
        .column((FileDeliveries::Table, FileDeliveries::Path))
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.created_at, file_deliveries.created_at, update_runs.created_at, \
                 application_runs.created_at, wake_requests.created_at)",
            ),
            "started_at",
        )
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.completed_at, file_deliveries.completed_at, update_runs.completed_at, \
                 application_runs.completed_at, wake_requests.online_at)",
            ),
            "completed_at",
        )
        .column((JobDevices::Table, JobDevices::Result))
        .expr_as(
            Expr::cust(format!("CAST({} AS BIGINT)", overdue_sql(now))),
            "overdue",
        )
        .from_subquery(page_query(job_id, page, now), JobDevices::Table)
        .left_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((JobDevices::Table, JobDevices::DeviceId)),
        );
    join_runs(&mut select);
    // The database's order, which `after` follows: its collation may not
    // order IDs as Rust would.
    select.order_by((JobDevices::Table, JobDevices::DeviceId), Order::Asc);
    select
}

/// A page of the job's devices, in device ID order. It reads as many rows
/// as the page holds, and runs the same statements for a job of any size:
/// one for the page, and one for the problems of its application runs.
pub async fn devices(
    executor: &mut impl Executor,
    job_id: &str,
    page: &DevicePage,
    now: i64,
) -> db::Result<Vec<JobDevice>> {
    if page.limit == 0 {
        return Ok(Vec::new());
    }
    let rows: Vec<JobDeviceRow> = executor
        .fetch_all(&devices_query(job_id, page, now))
        .await?;
    let mut devices = rows
        .into_iter()
        .filter_map(|row| {
            let state = DeviceState::parse(&row.state)?;
            let outcome = outcome(state, row.result.as_deref(), row.overdue != 0);
            // An install that went partly wrong says how much.
            let installs = match (row.updates_installed, row.updates_failed) {
                (Some(installed), Some(failed)) if failed > 0 => Some(format!(
                    "{failed} of {} updates failed to install",
                    installed + failed
                )),
                _ => None,
            };
            let applications = match row.applications_failed {
                Some(1) => Some("1 application failed".to_owned()),
                Some(failed) if failed > 1 => Some(format!("{failed} applications failed")),
                _ => None,
            };
            // A woken device that never came online has no error of its own.
            let unwoken = (row.wake_id.is_some() && outcome == Outcome::NoResult).then(|| {
                format!(
                    "The packets were sent, but the device didn't come online within {} minutes",
                    crate::wake::ONLINE_WINDOW_MS / crate::time::MINUTE_MS
                )
            });
            Some(JobDevice {
                device_id: row.device_id,
                device_name: row.device_name,
                platform: row.platform.as_deref().and_then(DevicePlatform::parse),
                outcome,
                detail: row
                    .error
                    .or(installs)
                    .or(applications)
                    .or(unwoken)
                    .or(row.detail),
                run_id: row.run_id,
                delivery_id: row.delivery_id,
                update_run_id: row.update_run_id,
                wake_id: row.wake_id,
                application_run_id: row.application_run_id,
                applications: Vec::new(),
                exit_code: row.exit_code,
                path: row.path,
                started_at_unix_ms: row.started_at,
                completed_at_unix_ms: row.completed_at,
            })
        })
        .collect::<Vec<_>>();
    let runs = devices
        .iter()
        .filter_map(|device| device.application_run_id.clone())
        .collect::<Vec<_>>();
    if runs.is_empty() {
        return Ok(devices);
    }
    let problems: Vec<ApplicationProblem> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    ApplicationRunItems::RunId,
                    ApplicationRunItems::Name,
                    ApplicationRunItems::Outcome,
                    ApplicationRunItems::Error,
                ])
                .from(ApplicationRunItems::Table)
                .and_where(Expr::col(ApplicationRunItems::RunId).is_in(runs))
                .and_where(Expr::col(ApplicationRunItems::Outcome).is_in(["failed", "skipped"]))
                .order_by(ApplicationRunItems::RunId, Order::Asc)
                .order_by(ApplicationRunItems::Position, Order::Asc)
                .to_owned(),
        )
        .await?;
    if !problems.is_empty() {
        let mut by_run = HashMap::<String, Vec<ApplicationProblem>>::new();
        for problem in problems {
            by_run
                .entry(problem.run_id.clone())
                .or_default()
                .push(problem);
        }
        for device in &mut devices {
            if let Some(problems) = device
                .application_run_id
                .as_deref()
                .and_then(|id| by_run.remove(id))
            {
                device.applications = problems;
            }
        }
    }
    Ok(devices)
}

/// Whether the viewer may see the job a run or delivery belongs to.
pub async fn sees_job(
    executor: &mut impl Executor,
    viewer: &Viewer,
    job_id: &str,
) -> db::Result<bool> {
    let row: Option<(String, Option<String>)> = executor
        .fetch_optional(
            &Query::select()
                .columns([Jobs::RequestedByUserId, Jobs::PolicyId])
                .from(Jobs::Table)
                .and_where(Expr::col(Jobs::Id).eq(job_id))
                .to_owned(),
        )
        .await?;
    Ok(row.is_some_and(|(requested_by, policy_id)| {
        viewer.everyone
            || requested_by == viewer.user_id
            || (viewer.policies && policy_id.is_some())
    }))
}

/// Cancels what the job hasn't started. Returns how many devices it cancelled.
pub async fn cancel(executor: &mut impl Executor, job_id: &str, now: i64) -> db::Result<u64> {
    executor
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Cancelled.as_str())
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::JobId).eq(job_id))
                .and_where(
                    Expr::col(JobDevices::State)
                        .is_in([DeviceState::Queued.as_str(), DeviceState::Waiting.as_str()]),
                )
                .to_owned(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_follow_the_device_and_its_run() {
        use DeviceState::*;
        assert_eq!(outcome(Queued, None, false), Outcome::Queued);
        assert_eq!(outcome(Waiting, None, false), Outcome::Waiting);
        assert_eq!(outcome(Skipped, None, false), Outcome::Skipped);
        assert_eq!(outcome(Missed, None, false), Outcome::Missed);
        assert_eq!(outcome(Cancelled, None, false), Outcome::Cancelled);
        assert_eq!(outcome(Started, None, false), Outcome::Running);
        assert_eq!(
            outcome(Started, None, true),
            Outcome::NoResult,
            "an overdue report"
        );
        for result in [
            Outcome::Succeeded,
            Outcome::Failed,
            Outcome::TimedOut,
            Outcome::NoResult,
            Outcome::Skipped,
        ] {
            assert_eq!(outcome(Started, Some(result.as_str()), false), result);
            assert_eq!(outcome(Started, Some(result.as_str()), true), result);
        }
    }

    #[test]
    fn counts_add_up() {
        let mut counts = Counts::default();
        counts.add(Outcome::Succeeded, 3);
        counts.add(Outcome::Waiting, 2);
        counts.add(Outcome::TimedOut, 1);
        counts.add(Outcome::Skipped, 4);
        assert_eq!(counts.total, 10);
        assert_eq!(counts.active(), 2);
        assert_eq!(counts.failures(), 1);
    }

    #[test]
    fn only_jobs_on_many_devices_copy_the_connected_devices() {
        let hub = AgentHub::default();
        let _connections = (0..3)
            .map(|index| hub.connect(&format!("device-{index}")))
            .collect::<Vec<_>>();
        assert!(
            matches!(Connected::new(&hub, 1), Connected::Ask(_)),
            "a job on one device asks about it"
        );
        assert!(matches!(
            Connected::new(&hub, COPY_CONNECTED_FROM - 1),
            Connected::Ask(_)
        ));
        let Connected::Copied(copied) = Connected::new(&hub, COPY_CONNECTED_FROM) else {
            panic!("a job on many devices copies the connected ones once");
        };
        assert_eq!(copied.len(), 3);
        // With more connected than the job covers, asking is less work.
        let _fleet = (0..=COPY_CONNECTED_FROM)
            .map(|index| hub.connect(&format!("fleet-{index}")))
            .collect::<Vec<_>>();
        assert!(matches!(
            Connected::new(&hub, COPY_CONNECTED_FROM),
            Connected::Ask(_)
        ));
        for devices in [1, 2 * COPY_CONNECTED_FROM] {
            let connected = Connected::new(&hub, devices);
            assert!(connected.contains("device-0"), "{devices}");
            assert!(!connected.contains("elsewhere"), "{devices}");
        }
    }
}
