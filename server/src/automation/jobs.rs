//! Jobs: one script run, file delivery, update scan, update install or
//! wake on many devices, and how far it has got on each.
//!
//! A job is created with every device it covers. A device that can't run the
//! script's interpreter, or has no update manager MeshRMM supports, is
//! skipped; an online one is queued for the dispatcher; an offline one waits
//! for its Agent to connect, or is missed at once if the job doesn't wait.
//! Each device's run, delivery or update run is an ordinary one carrying the
//! job's ID, so its result, output and alerts work as for one started by
//! hand. A wake job is the other way round: it skips online devices, and
//! wakes each offline one through the devices on its network.
use std::collections::{HashMap, HashSet};

use meshrmm_protocol_types::{
    DevicePlatform, FileDeliveryDestination, RebootBehavior, RunAs, ScriptLanguage, UpdateCategory,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    audit::{self, Actor, Target as AuditTarget},
    db::{
        self, Executor,
        tables::{
            Agents, Companies, DeviceUpdateStatus, FileDeliveries, JobDevices, Jobs, ScriptRuns,
            UpdateRuns, Users, WakeRequests,
        },
    },
    http::ApiError,
    rbac::{Permission, Permissions},
    time::{HOUR_MS, SECOND_MS},
    toolbox::{DELIVERY_WINDOW_MS, RUN_REPORT_GRACE_MS},
    updates::runs::{INSTALL_REPORT_WINDOW_MS, Install, SCAN_REPORT_WINDOW_MS},
    users::new_id,
};

/// The longest a job waits for offline devices: a week.
pub const MAX_WAIT_HOURS: i64 = 7 * 24;
pub const DEFAULT_WAIT_HOURS: i64 = 24;
/// The most devices a request may choose one by one.
pub const MAX_CHOSEN_DEVICES: usize = 10_000;
/// Rows written per statement.
const INSERT_CHUNK: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Script,
    File,
    UpdateScan,
    UpdateInstall,
    Wake,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::File => "file",
            Self::UpdateScan => "update_scan",
            Self::UpdateInstall => "update_install",
            Self::Wake => "wake",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "script" => Some(Self::Script),
            "file" => Some(Self::File),
            "update_scan" => Some(Self::UpdateScan),
            "update_install" => Some(Self::UpdateInstall),
            "wake" => Some(Self::Wake),
            _ => None,
        }
    }
}

/// Which devices a job or policy covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// Chosen devices.
    Devices,
    /// Every device in chosen companies.
    Companies,
    /// Every device.
    All,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Devices => "devices",
            Self::Companies => "companies",
            Self::All => "all",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "devices" => Some(Self::Devices),
            "companies" => Some(Self::Companies),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Whole companies and every device need [`Permission::JobsRunWide`].
    pub fn is_wide(self) -> bool {
        self != Self::Devices
    }
}

/// The devices a job covers, as chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Devices(Vec<String>),
    Companies(Vec<String>),
    All,
}

impl Target {
    pub fn kind(&self) -> TargetKind {
        match self {
            Self::Devices(_) => TargetKind::Devices,
            Self::Companies(_) => TargetKind::Companies,
            Self::All => TargetKind::All,
        }
    }

    /// From a request's fields: the kind, and the IDs it uses. Checks the
    /// IDs' form and that a choice isn't empty.
    pub fn from_request(
        kind: TargetKind,
        device_ids: Vec<String>,
        company_ids: Vec<String>,
    ) -> Result<Self, ApiError> {
        let ids = |mut ids: Vec<String>, what: &str| {
            for id in &ids {
                crate::agents::parse_id(id, what)?;
            }
            ids.sort();
            ids.dedup();
            Ok::<_, ApiError>(ids)
        };
        match kind {
            TargetKind::Devices => {
                if device_ids.is_empty() {
                    return Err(invalid("choose at least one device"));
                }
                if device_ids.len() > MAX_CHOSEN_DEVICES {
                    return Err(invalid(format!(
                        "choose at most {MAX_CHOSEN_DEVICES} devices, or whole companies"
                    )));
                }
                Ok(Self::Devices(ids(device_ids, "device")?))
            }
            TargetKind::Companies => {
                if company_ids.is_empty() {
                    return Err(invalid("choose at least one company"));
                }
                Ok(Self::Companies(ids(company_ids, "company")?))
            }
            TargetKind::All => Ok(Self::All),
        }
    }

    pub fn device_ids(&self) -> &[String] {
        match self {
            Self::Devices(ids) => ids,
            _ => &[],
        }
    }

    pub fn company_ids(&self) -> &[String] {
        match self {
            Self::Companies(ids) => ids,
            _ => &[],
        }
    }
}

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
}

impl Payload {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Script { .. } => Kind::Script,
            Self::File { .. } => Kind::File,
            Self::UpdateScan => Kind::UpdateScan,
            Self::UpdateInstall { .. } => Kind::UpdateInstall,
            Self::Wake => Kind::Wake,
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

/// A device's outcome from its state and its run's or delivery's status.
/// `succeeded` is a run that exited with 0, or a delivered file; `overdue`
/// a pending one the device should have reported long ago.
pub fn outcome(
    state: DeviceState,
    status: Option<&str>,
    succeeded: bool,
    overdue: bool,
) -> Outcome {
    match state {
        DeviceState::Queued => Outcome::Queued,
        DeviceState::Waiting => Outcome::Waiting,
        DeviceState::Skipped => Outcome::Skipped,
        DeviceState::Missed => Outcome::Missed,
        DeviceState::Cancelled => Outcome::Cancelled,
        DeviceState::Started => match status {
            Some("pending") if overdue => Outcome::NoResult,
            Some("pending") => Outcome::Running,
            Some("completed" | "delivered" | "succeeded") if succeeded => Outcome::Succeeded,
            Some("completed" | "failed" | "partial") => Outcome::Failed,
            Some("timed_out") => Outcome::TimedOut,
            // Lost, or purged with the toolbox history.
            _ => Outcome::NoResult,
        },
    }
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
        let slot = match outcome {
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
        };
        *slot += count;
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

/// A created job, before the dispatcher starts anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: String,
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

/// The devices `target` covers that are enrolled and not deleted, by name.
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
        Target::All => rows = executor.fetch_all(&select()).await?,
        Target::Companies(ids) => {
            rows = executor
                .fetch_all(
                    &select()
                        .and_where(
                            Expr::col((Agents::Table, Agents::CompanyId))
                                .is_in(ids.iter().cloned()),
                        )
                        .to_owned(),
                )
                .await?;
        }
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
    }
    rows.sort_by(|left, right| {
        (left.name.to_lowercase(), &left.id).cmp(&(right.name.to_lowercase(), &right.id))
    });
    Ok(rows)
}

/// The target in words: the device's name, the companies' names, or how
/// many devices.
async fn target_label(
    executor: &mut impl Executor,
    target: &Target,
    devices: &[DeviceRow],
) -> db::Result<String> {
    Ok(match target {
        Target::All => "All devices".to_owned(),
        Target::Devices(_) if devices.len() == 1 => devices[0].name.clone(),
        Target::Devices(_) => format!("{} devices", devices.len()),
        Target::Companies(ids) => {
            let mut names: Vec<(String,)> = executor
                .fetch_all(
                    &Query::select()
                        .column(Companies::Name)
                        .from(Companies::Table)
                        .and_where(Expr::col(Companies::Id).is_in(ids.iter().cloned()))
                        .order_by(Companies::Name, Order::Asc)
                        .to_owned(),
                )
                .await?;
            names.truncate(4);
            let mut label = names
                .iter()
                .map(|(name,)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if ids.len() > names.len() {
                label.push_str(&format!(" and {} more", ids.len() - names.len()));
            }
            label
        }
    })
}

/// Creates a job and its devices, and records who started it. Devices
/// without the script's interpreter are skipped; connected ones are queued;
/// the others wait or are missed. Fails with `no_devices` if the target
/// covers none.
pub async fn create(
    executor: &mut impl Executor,
    job: &NewJob,
    connected: &HashSet<String>,
    now: i64,
) -> Result<Created, ApiError> {
    let devices = resolve(executor, &job.target).await?;
    if devices.is_empty() {
        return Err(ApiError::bad_request("there are no devices to run on").with_code("no_devices"));
    }
    let label = target_label(executor, &job.target, &devices).await?;
    let id = new_id();
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
    let (language, body, timeout_seconds, run_as, destination) = match &job.payload {
        Payload::UpdateScan | Payload::UpdateInstall { .. } | Payload::Wake => {
            (None, None, None, None, None)
        }
        Payload::Script {
            language,
            body,
            timeout_seconds,
            run_as,
            ..
        } => (
            Some(language.as_str()),
            Some(body.as_str()),
            Some(*timeout_seconds),
            Some(run_as.as_str()),
            None,
        ),
        Payload::File { destination, .. } => (None, None, None, None, Some(destination.as_str())),
    };
    let (item_id, item_name) = job.payload.item();
    let (categories, reboot, reboot_delay_minutes) = match &job.payload {
        Payload::UpdateInstall {
            categories,
            reboot,
            reboot_delay_minutes,
        } => (
            Some(categories.as_slice()),
            Some(reboot.as_str()),
            Some(i64::from(*reboot_delay_minutes)),
        ),
        _ => (None, None, None),
    };
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
                ])
                .values_panic([
                    id.as_str().into(),
                    job.payload.kind().as_str().into(),
                    item_id.into(),
                    item_name.as_str().into(),
                    language.into(),
                    body.into(),
                    timeout_seconds.into(),
                    run_as.into(),
                    destination.into(),
                    categories.and_then(Install::categories_text).into(),
                    reboot.into(),
                    reboot_delay_minutes.into(),
                    job.target.kind().as_str().into(),
                    label.as_str().into(),
                    offline.as_str().into(),
                    (now + wait_ms).into(),
                    job.requested_by_user_id.as_str().into(),
                    job.policy.as_ref().map(|(id, _)| id.as_str()).into(),
                    job.policy.as_ref().map(|(_, name)| name.as_str()).into(),
                    now.into(),
                ])
                .to_owned(),
        )
        .await?;
    let mut counts: HashMap<DeviceState, i64> = HashMap::new();
    for chunk in devices.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(JobDevices::Table).columns([
            JobDevices::JobId,
            JobDevices::DeviceId,
            JobDevices::State,
            JobDevices::Detail,
            JobDevices::UpdatedAt,
        ]);
        for row in chunk {
            let device = Device {
                id: &row.id,
                platform: DevicePlatform::parse(&row.platform),
                updates_supported: row.supported.unwrap_or(true),
            };
            let (state, detail) = initial_state(job, device, connected);
            *counts.entry(state).or_default() += 1;
            insert.values_panic([
                id.as_str().into(),
                device.id.into(),
                state.as_str().into(),
                detail.into(),
                now.into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    let mut metadata = json!({
        "kind": job.payload.kind().as_str(),
        "item_id": item_id,
        "item_name": item_name,
        "target": job.target.kind().as_str(),
        "target_label": label,
        "devices": devices.len(),
        "offline": offline.as_str(),
        "queued": counts.get(&DeviceState::Queued).copied().unwrap_or(0),
        "waiting": counts.get(&DeviceState::Waiting).copied().unwrap_or(0),
        "skipped": counts.get(&DeviceState::Skipped).copied().unwrap_or(0),
    });
    if let Some(run_as) = run_as {
        metadata["run_as"] = run_as.into();
    }
    if let Some(destination) = destination {
        metadata["destination"] = destination.into();
    }
    if let Some(categories) = categories {
        metadata["categories"] = json!(categories);
    }
    if let Some(reboot) = reboot {
        metadata["reboot"] = reboot.into();
        metadata["reboot_delay_minutes"] = reboot_delay_minutes.into();
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
    Ok(Created {
        id,
        devices: devices.len() as i64,
    })
}

fn initial_state(
    job: &NewJob,
    device: Device<'_>,
    connected: &HashSet<String>,
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

/// Jobs by ID, with their counts, whoever started them.
pub async fn by_ids(
    executor: &mut impl Executor,
    ids: &[String],
    now: i64,
) -> db::Result<Vec<Job>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<JobRow> = executor
        .fetch_all(
            &job_select()
                .and_where(Expr::col((Jobs::Table, Jobs::Id)).is_in(ids.iter().cloned()))
                .to_owned(),
        )
        .await?;
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
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
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

/// The columns that join a job's device to its run, delivery or update run:
/// the status, whether it succeeded, and whether it's overdue at `now`.
fn outcome_columns(select: &mut sea_query::SelectStatement, now: i64) {
    let run_deadline = format!(
        "script_runs.created_at + script_runs.timeout_seconds * {SECOND_MS} + {RUN_REPORT_GRACE_MS}"
    );
    let delivery_deadline = format!("file_deliveries.created_at + {DELIVERY_WINDOW_MS}");
    let wake_deadline = format!(
        "wake_requests.created_at + {}",
        crate::wake::ONLINE_WINDOW_MS
    );
    let update_deadline = format!(
        "update_runs.created_at + CASE update_runs.kind WHEN 'scan' THEN {SCAN_REPORT_WINDOW_MS} \
         ELSE {INSTALL_REPORT_WINDOW_MS} END"
    );
    select
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.status, file_deliveries.status, update_runs.status, \
                 CASE WHEN wake_requests.id IS NULL THEN NULL \
                 WHEN wake_requests.online_at IS NOT NULL THEN 'succeeded' \
                 WHEN wake_requests.status IN ('sending', 'sent') THEN 'pending' \
                 ELSE 'failed' END)",
            ),
            "status",
        )
        .expr_as(
            Expr::cust(
                "CAST(CASE WHEN script_runs.status = 'completed' AND script_runs.exit_code = 0 THEN 1 \
                 WHEN file_deliveries.status = 'delivered' THEN 1 \
                 WHEN update_runs.status = 'succeeded' THEN 1 \
                 WHEN wake_requests.online_at IS NOT NULL THEN 1 ELSE 0 END AS BIGINT)",
            ),
            "succeeded",
        )
        .expr_as(
            Expr::cust(format!(
                "CAST(CASE WHEN script_runs.status = 'pending' AND {run_deadline} < {now} THEN 1 \
                 WHEN file_deliveries.status = 'pending' AND {delivery_deadline} < {now} THEN 1 \
                 WHEN update_runs.status = 'pending' AND {update_deadline} < {now} THEN 1 \
                 WHEN wake_requests.online_at IS NULL AND wake_requests.status IN ('sending', 'sent') \
                 AND {wake_deadline} < {now} THEN 1 \
                 ELSE 0 END AS BIGINT)"
            )),
            "overdue",
        )
        .left_join(
            ScriptRuns::Table,
            Expr::col((ScriptRuns::Table, ScriptRuns::JobId))
                .equals((JobDevices::Table, JobDevices::JobId))
                .and(
                    Expr::col((ScriptRuns::Table, ScriptRuns::DeviceId))
                        .equals((JobDevices::Table, JobDevices::DeviceId)),
                ),
        )
        .left_join(
            FileDeliveries::Table,
            Expr::col((FileDeliveries::Table, FileDeliveries::JobId))
                .equals((JobDevices::Table, JobDevices::JobId))
                .and(
                    Expr::col((FileDeliveries::Table, FileDeliveries::DeviceId))
                        .equals((JobDevices::Table, JobDevices::DeviceId)),
                ),
        )
        .left_join(
            UpdateRuns::Table,
            Expr::col((UpdateRuns::Table, UpdateRuns::JobId))
                .equals((JobDevices::Table, JobDevices::JobId))
                .and(
                    Expr::col((UpdateRuns::Table, UpdateRuns::DeviceId))
                        .equals((JobDevices::Table, JobDevices::DeviceId)),
                ),
        )
        .left_join(
            WakeRequests::Table,
            Expr::col((WakeRequests::Table, WakeRequests::JobId))
                .equals((JobDevices::Table, JobDevices::JobId))
                .and(
                    Expr::col((WakeRequests::Table, WakeRequests::DeviceId))
                        .equals((JobDevices::Table, JobDevices::DeviceId)),
                ),
        );
}

#[derive(sqlx::FromRow)]
struct CountRow {
    job_id: String,
    state: String,
    status: Option<String>,
    succeeded: i64,
    overdue: i64,
    devices: i64,
}

/// Each job's counts.
pub async fn counts(
    executor: &mut impl Executor,
    ids: &[String],
    now: i64,
) -> db::Result<HashMap<String, Counts>> {
    let mut counts: HashMap<String, Counts> = HashMap::new();
    for chunk in ids.chunks(200) {
        let mut select = Query::select();
        select.columns([
            (JobDevices::Table, JobDevices::JobId),
            (JobDevices::Table, JobDevices::State),
        ]);
        outcome_columns(&mut select, now);
        select
            .expr_as(Expr::cust("CAST(COUNT(*) AS BIGINT)"), "devices")
            .from(JobDevices::Table)
            .and_where(
                Expr::col((JobDevices::Table, JobDevices::JobId)).is_in(chunk.iter().cloned()),
            )
            .add_group_by([
                Expr::col((JobDevices::Table, JobDevices::JobId)),
                Expr::col((JobDevices::Table, JobDevices::State)),
                Expr::cust("3"),
                Expr::cust("4"),
                Expr::cust("5"),
            ]);
        let rows: Vec<CountRow> = executor.fetch_all(&select).await?;
        for row in rows {
            let Some(state) = DeviceState::parse(&row.state) else {
                continue;
            };
            counts.entry(row.job_id).or_default().add(
                outcome(
                    state,
                    row.status.as_deref(),
                    row.succeeded != 0,
                    row.overdue != 0,
                ),
                row.devices,
            );
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
    pub exit_code: Option<i64>,
    /// Where a delivered file was saved.
    pub path: Option<String>,
    pub started_at_unix_ms: Option<i64>,
    pub completed_at_unix_ms: Option<i64>,
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
    updates_installed: Option<i64>,
    updates_failed: Option<i64>,
    exit_code: Option<i64>,
    error: Option<String>,
    path: Option<String>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    status: Option<String>,
    succeeded: i64,
    overdue: i64,
}

/// The job's devices, by name.
pub async fn devices(
    executor: &mut impl Executor,
    job_id: &str,
    now: i64,
) -> db::Result<Vec<JobDevice>> {
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
                "COALESCE(script_runs.error, file_deliveries.error, update_runs.error, wake_requests.detail)",
            ),
            "error",
        )
        .column((FileDeliveries::Table, FileDeliveries::Path))
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.created_at, file_deliveries.created_at, update_runs.created_at, \
                 wake_requests.created_at)",
            ),
            "started_at",
        )
        .expr_as(
            Expr::cust(
                "COALESCE(script_runs.completed_at, file_deliveries.completed_at, update_runs.completed_at, \
                 wake_requests.online_at)",
            ),
            "completed_at",
        );
    outcome_columns(&mut select, now);
    select
        .from(JobDevices::Table)
        .left_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((JobDevices::Table, JobDevices::DeviceId)),
        )
        .and_where(Expr::col((JobDevices::Table, JobDevices::JobId)).eq(job_id));
    let rows: Vec<JobDeviceRow> = executor.fetch_all(&select).await?;
    let mut devices = rows
        .into_iter()
        .filter_map(|row| {
            let state = DeviceState::parse(&row.state)?;
            let outcome = outcome(
                state,
                row.status.as_deref(),
                row.succeeded != 0,
                row.overdue != 0,
            );
            // An install that went partly wrong says how much.
            let installs = match (row.updates_installed, row.updates_failed) {
                (Some(installed), Some(failed)) if failed > 0 => Some(format!(
                    "{failed} of {} updates failed to install",
                    installed + failed
                )),
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
                detail: row.error.or(installs).or(unwoken).or(row.detail),
                run_id: row.run_id,
                delivery_id: row.delivery_id,
                update_run_id: row.update_run_id,
                wake_id: row.wake_id,
                exit_code: row.exit_code,
                path: row.path,
                started_at_unix_ms: row.started_at,
                completed_at_unix_ms: row.completed_at,
            })
        })
        .collect::<Vec<_>>();
    devices.sort_by_cached_key(|device| {
        (
            device
                .device_name
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            device.device_id.clone(),
        )
    });
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
        assert_eq!(outcome(Queued, None, false, false), Outcome::Queued);
        assert_eq!(outcome(Waiting, None, false, false), Outcome::Waiting);
        assert_eq!(outcome(Skipped, None, false, false), Outcome::Skipped);
        assert_eq!(outcome(Missed, None, false, false), Outcome::Missed);
        assert_eq!(outcome(Cancelled, None, false, false), Outcome::Cancelled);
        assert_eq!(
            outcome(Started, Some("pending"), false, false),
            Outcome::Running
        );
        assert_eq!(
            outcome(Started, Some("pending"), false, true),
            Outcome::NoResult
        );
        assert_eq!(
            outcome(Started, Some("completed"), true, false),
            Outcome::Succeeded
        );
        assert_eq!(
            outcome(Started, Some("completed"), false, false),
            Outcome::Failed,
            "a non-zero exit code fails"
        );
        assert_eq!(
            outcome(Started, Some("failed"), false, false),
            Outcome::Failed
        );
        assert_eq!(
            outcome(Started, Some("timed_out"), false, false),
            Outcome::TimedOut
        );
        assert_eq!(
            outcome(Started, Some("lost"), false, false),
            Outcome::NoResult
        );
        assert_eq!(
            outcome(Started, Some("delivered"), true, false),
            Outcome::Succeeded
        );
        assert_eq!(outcome(Started, None, false, false), Outcome::NoResult);
        // Update runs.
        assert_eq!(
            outcome(Started, Some("succeeded"), true, false),
            Outcome::Succeeded
        );
        assert_eq!(
            outcome(Started, Some("partial"), false, false),
            Outcome::Failed
        );
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
    fn targets_need_a_choice() {
        let id = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a".to_owned();
        assert!(Target::from_request(TargetKind::Devices, vec![], vec![]).is_err());
        assert!(Target::from_request(TargetKind::Companies, vec![id.clone()], vec![]).is_err());
        assert!(Target::from_request(TargetKind::Devices, vec!["x".into()], vec![]).is_err());
        assert_eq!(
            Target::from_request(TargetKind::Devices, vec![id.clone(), id.clone()], vec![])
                .unwrap(),
            Target::Devices(vec![id.clone()])
        );
        assert_eq!(
            Target::from_request(TargetKind::All, vec![id], vec![]).unwrap(),
            Target::All
        );
        assert!(TargetKind::All.is_wide() && !TargetKind::Devices.is_wide());
    }
}
