//! Starting jobs' runs, deliveries and update runs on their devices, a few
//! at a time.
//!
//! A pass starts at most [`Limits::batch`] queued devices, while fewer than
//! [`Limits::in_flight`] of their [`Class`]'s job runs await a report, and
//! while the device has fewer than [`MAX_PER_DEVICE`] of its own; both
//! limits grow with the connected Agents (see [`limits`]). Quick runs
//! (scripts, file deliveries, update scans) and long ones (update installs,
//! application runs) each have their own ceiling, so a fleet-wide install
//! can't hold the slots a script on one device needs. A pass takes devices
//! from the queued jobs in turn: the first of every job, then the second of
//! every job, and so on, with jobs that have fewer devices left to start
//! first in each round and the older first among equals. So a job on one
//! device starts within a pass or two however large the jobs queued before
//! it, and a job on a thousand devices still reaches them over a minute or
//! so, its results trickling in rather than arriving together. A queued
//! device that can't start yet, because it is at its limit, installing
//! updates or busy with applications, is left out of the pass rather than
//! holding up the devices queued after it, and starts on a pass after its
//! runs report. Waiting devices are queued when their Agent connects, and
//! missed when the job expires first. A wake job's queued devices are
//! offline ones: they are woken together, through the relays the wake
//! requests pick, on the next pass however full the dispatcher is, since
//! they hold no run.
use std::collections::{BTreeMap, HashMap, HashSet};

use meshrmm_protocol_types::{
    AgentCommand, FileDeliveryDestination, FileDeliveryRequest, RebootBehavior, RunAs,
    ScriptLanguage, ScriptRunRequest,
};
use sea_query::{Cond, Expr, ExprTrait, Order, Query};

use super::jobs::{DeviceState, Kind, Offline};
use crate::{
    db::{
        self, Executor,
        tables::{
            Agents, AutomationPolicyDevices, FileDeliveries, JobDevices, Jobs, ScriptRuns,
            UpdateRuns,
        },
    },
    http::AppState,
    time::SECOND_MS,
    toolbox::{self, DELIVERY_WINDOW_MS, NewDelivery, NewRun, RUN_REPORT_GRACE_MS, Source},
    updates::runs::{
        self as update_runs, INSTALL_REPORT_WINDOW_MS, Install, Kind as UpdateKind,
        SCAN_REPORT_WINDOW_MS,
    },
    users::new_id,
};

/// The devices a pass starts however few Agents are connected: a small
/// server works as it always has.
pub const MIN_BATCH: u64 = 25;
/// The most devices a pass starts however many Agents are connected.
pub const MAX_BATCH: u64 = 250;
/// Connected Agents for each device a pass starts.
const AGENTS_PER_START: u64 = 100;
/// The runs of a class that may await a report however few Agents are
/// connected.
pub const MIN_IN_FLIGHT: i64 = 500;
/// The most runs of a class awaiting a report however many Agents are
/// connected.
pub const MAX_IN_FLIGHT: i64 = 5_000;
/// Connected Agents for each run of a class that may await a report.
const AGENTS_PER_RUN: i64 = 4;
/// The most of a device's job runs and deliveries awaiting a report at once,
/// well under the eight scripts an Agent runs at a time.
pub const MAX_PER_DEVICE: i64 = 2;

/// How many devices a pass may start, and how many runs of each class may
/// await a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Devices started per pass, and wake jobs' devices woken besides.
    pub batch: u64,
    /// Runs of one class awaiting a report.
    pub in_flight: i64,
}

impl Limits {
    /// The most queued devices a pass reads, leaving room for devices that
    /// reach their limit during the pass.
    fn page(self) -> u64 {
        self.batch * 4
    }
}

/// The limits with `connected` Agents: a device started per pass for every
/// 100, and a run of each class in flight for every 4, between today's
/// floors and the caps.
///
/// | Connected | Per pass | In flight, each class |
/// |---|---|---|
/// | 100 | 25 | 500 |
/// | 1,000 | 25 | 500 |
/// | 10,000 | 100 | 2,500 |
/// | 100,000 | 250 | 5,000 |
///
/// The engine passes every second while devices are queued, so the batch
/// is the starts per second. A start is one transaction of a few statements
/// (the run, the device's part, and for updates and applications a check of
/// what the device runs), and its report later is another few: 250 a second
/// is about 500 small write transactions a second, well within what
/// SQLite's single writer commits in WAL mode without a sync per commit,
/// leaving it most of each second for heartbeats, metrics and reports by
/// hand. A fleet-wide script job on 100,000 devices is handed out in about
/// seven minutes. The in-flight cap bounds the reports that can arrive at
/// once, 10,000 across both classes, and the devices a pass names as at
/// their limit, at most half of that; at 5,000 installs of half an hour,
/// 100,000 devices are patched in about ten hours.
pub fn limits(connected: usize) -> Limits {
    let connected = u64::try_from(connected).unwrap_or(u64::MAX);
    Limits {
        batch: (connected / AGENTS_PER_START).clamp(MIN_BATCH, MAX_BATCH),
        in_flight: (i64::try_from(connected).unwrap_or(i64::MAX) / AGENTS_PER_RUN)
            .clamp(MIN_IN_FLIGHT, MAX_IN_FLIGHT),
    }
}

/// How long a job's runs await a report, which decides whose in-flight
/// ceiling they count against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Scripts, file deliveries and update scans: seconds to minutes.
    Quick,
    /// Update installs and application runs: half an hour or more.
    Long,
}

impl Class {
    const ALL: [Self; 2] = [Self::Quick, Self::Long];

    /// The class of a job's runs. A wake holds no run, so has none.
    pub fn of(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Script | Kind::File | Kind::UpdateScan => Some(Self::Quick),
            Kind::UpdateInstall | Kind::AppInstall | Kind::AppUpdate | Kind::AppUninstall => {
                Some(Self::Long)
            }
            Kind::Wake => None,
        }
    }

    fn kinds(self) -> &'static [Kind] {
        match self {
            Self::Quick => &[Kind::Script, Kind::File, Kind::UpdateScan],
            Self::Long => &[
                Kind::UpdateInstall,
                Kind::AppInstall,
                Kind::AppUpdate,
                Kind::AppUninstall,
            ],
        }
    }
}

const EXPIRED: &str = "The device didn't connect before the job expired";
const DELETED: &str = "The device was deleted";
const FILE_GONE: &str = "The file was deleted from the toolbox before it was sent";

/// Misses the waiting devices of the unfinished jobs that expired by `now`,
/// or of those that expired after `since`, so each step looks only at the
/// jobs that expired since the last. Returns the jobs.
pub async fn expire(state: &AppState, since: Option<i64>, now: i64) -> db::Result<Vec<String>> {
    let database = &state.database;
    let mut select = Query::select();
    select
        .column(Jobs::Id)
        .from(Jobs::Table)
        .and_where(Expr::col(Jobs::FinishedAt).is_null())
        .and_where(Expr::col(Jobs::ExpiresAt).lte(now));
    if let Some(since) = since {
        select.and_where(Expr::col(Jobs::ExpiresAt).gt(since));
    }
    let expired: Vec<(String,)> = database.fetch_all(&select).await?;
    let expired = expired.into_iter().map(|(id,)| id).collect::<Vec<_>>();
    for chunk in expired.chunks(QUEUE_CHUNK) {
        database
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, DeviceState::Missed.as_str())
                    .value(JobDevices::Detail, EXPIRED)
                    .value(JobDevices::UpdatedAt, now)
                    .and_where(Expr::col(JobDevices::JobId).is_in(chunk.iter().cloned()))
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    Ok(expired)
}

/// Misses the waiting and queued parts of devices that were deleted. Their
/// deletion misses them at once; this is for a server that stopped first.
/// Returns how many.
pub async fn miss_deleted(state: &AppState, now: i64) -> db::Result<u64> {
    state
        .database
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Missed.as_str())
                .value(JobDevices::Detail, DELETED)
                .value(JobDevices::UpdatedAt, now)
                .and_where(
                    Expr::col(JobDevices::State)
                        .is_in([DeviceState::Waiting.as_str(), DeviceState::Queued.as_str()]),
                )
                // Looks up the agent of each waiting or queued part, rather
                // than reading every agent.
                .and_where(
                    Expr::exists(
                        crate::agents::active()
                            .and_where(
                                Expr::col((Agents::Table, Agents::Id))
                                    .equals((JobDevices::Table, JobDevices::DeviceId)),
                            )
                            .to_owned(),
                    )
                    .not(),
                )
                .to_owned(),
        )
        .await
}

/// The most devices or jobs one statement names.
pub const QUEUE_CHUNK: usize = 500;

/// Queues the waiting parts of these devices, whose Agent connected, in
/// jobs that haven't expired. Returns the devices with parts queued.
pub async fn queue_devices(
    state: &AppState,
    device_ids: &[String],
    now: i64,
) -> db::Result<Vec<String>> {
    let waiting_in = |devices: &[String]| {
        Cond::all()
            .add(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
            .add(Expr::col(JobDevices::DeviceId).is_in(devices.iter().cloned()))
            // Expired jobs' waiting devices are missed, not started.
            .add(
                Expr::col(JobDevices::JobId).in_subquery(
                    Query::select()
                        .column(Jobs::Id)
                        .from(Jobs::Table)
                        .and_where(Expr::col(Jobs::FinishedAt).is_null())
                        .and_where(Expr::col(Jobs::ExpiresAt).gt(now))
                        .to_owned(),
                ),
            )
    };
    let mut queued = Vec::new();
    for chunk in device_ids.chunks(QUEUE_CHUNK) {
        // Most devices that connect wait in no job: looking first keeps the
        // pass from taking SQLite's write lock for them. A device that starts
        // waiting after the look, as the update alone could have missed too,
        // is queued by the next full pass.
        let waiting: Vec<(String,)> = state
            .database
            .fetch_all(
                &Query::select()
                    .distinct()
                    .column(JobDevices::DeviceId)
                    .from(JobDevices::Table)
                    .cond_where(waiting_in(chunk))
                    .to_owned(),
            )
            .await?;
        if waiting.is_empty() {
            continue;
        }
        let waiting = waiting.into_iter().map(|(id,)| id).collect::<Vec<_>>();
        let updated = state
            .database
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, DeviceState::Queued.as_str())
                    .value(JobDevices::UpdatedAt, now)
                    .cond_where(waiting_in(&waiting))
                    .to_owned(),
            )
            .await?;
        if updated > 0 {
            queued.extend(waiting);
        }
    }
    Ok(queued)
}

/// The devices waiting in these jobs.
pub async fn waiting_devices(state: &AppState, job_ids: &[String]) -> db::Result<Vec<String>> {
    let mut devices = Vec::new();
    for chunk in job_ids.chunks(QUEUE_CHUNK) {
        let rows: Vec<(String,)> = state
            .database
            .fetch_all(
                &Query::select()
                    .column(JobDevices::DeviceId)
                    .from(JobDevices::Table)
                    .and_where(Expr::col(JobDevices::JobId).is_in(chunk.iter().cloned()))
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                    .to_owned(),
            )
            .await?;
        devices.extend(rows.into_iter().map(|(id,)| id));
    }
    Ok(devices)
}

/// Queues every waiting device whose Agent is connected, for what the steps
/// missed. Returns how many.
pub async fn queue_connected(state: &AppState, now: i64) -> db::Result<u64> {
    let database = &state.database;
    let waiting: Vec<(String,)> = database
        .fetch_all(
            &Query::select()
                .distinct()
                .column(JobDevices::DeviceId)
                .from(JobDevices::Table)
                .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                .to_owned(),
        )
        .await?;
    let connected = state.agents.connected();
    let online = waiting
        .into_iter()
        .map(|(id,)| id)
        .filter(|id| connected.contains(id))
        .collect::<Vec<_>>();
    let mut queued = 0;
    for chunk in online.chunks(500) {
        queued += database
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, DeviceState::Queued.as_str())
                    .value(JobDevices::UpdatedAt, now)
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Waiting.as_str()))
                    .and_where(Expr::col(JobDevices::DeviceId).is_in(chunk.iter().cloned()))
                    .to_owned(),
            )
            .await?;
    }
    Ok(queued)
}

/// The SQL that is true of a job's run or delivery still awaiting a report.
fn awaiting_run(now: i64) -> sea_query::SimpleExpr {
    Expr::col(ScriptRuns::JobId)
        .is_not_null()
        .and(
            Expr::col(ScriptRuns::Status)
                .eq("pending")
                .and(Expr::cust(format!(
                    "created_at + timeout_seconds * {SECOND_MS} + {RUN_REPORT_GRACE_MS} >= {now}"
                ))),
        )
}

fn awaiting_delivery(now: i64) -> sea_query::SimpleExpr {
    Expr::col(FileDeliveries::JobId).is_not_null().and(
        Expr::col(FileDeliveries::Status)
            .eq("pending")
            .and(Expr::col(FileDeliveries::CreatedAt).gte(now - DELIVERY_WINDOW_MS)),
    )
}

fn awaiting_update_run(now: i64) -> sea_query::SimpleExpr {
    Expr::col(UpdateRuns::JobId)
        .is_not_null()
        .and(
            Expr::col(UpdateRuns::Status)
                .eq("pending")
                .and(Expr::cust(format!(
                    "created_at + CASE kind WHEN 'scan' THEN {SCAN_REPORT_WINDOW_MS} \
                 ELSE {INSTALL_REPORT_WINDOW_MS} END >= {now}"
                ))),
        )
}

/// The jobs' runs, deliveries, update runs and application runs awaiting a
/// report.
#[derive(Debug, Default)]
struct InFlight {
    quick: i64,
    long: i64,
    /// On each device that has any.
    per_device: HashMap<String, i64>,
}

impl InFlight {
    fn of(&mut self, class: Class) -> &mut i64 {
        match class {
            Class::Quick => &mut self.quick,
            Class::Long => &mut self.long,
        }
    }

    /// The classes with room for another run.
    fn open(&mut self, limits: Limits) -> Vec<Class> {
        Class::ALL
            .into_iter()
            .filter(|class| *self.of(*class) < limits.in_flight)
            .collect()
    }
}

/// How many of the jobs' runs, deliveries, update runs and application runs
/// await a report, in each class and on each device that has any.
async fn in_flight(state: &AppState, now: i64) -> db::Result<InFlight> {
    let database = &state.database;
    let mut in_flight = InFlight::default();
    let runs: Vec<(String, i64)> = database
        .fetch_all(
            &Query::select()
                .column(ScriptRuns::DeviceId)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(ScriptRuns::Table)
                .and_where(awaiting_run(now))
                .group_by_col(ScriptRuns::DeviceId)
                .to_owned(),
        )
        .await?;
    let deliveries: Vec<(String, i64)> = database
        .fetch_all(
            &Query::select()
                .column(FileDeliveries::DeviceId)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(FileDeliveries::Table)
                .and_where(awaiting_delivery(now))
                .group_by_col(FileDeliveries::DeviceId)
                .to_owned(),
        )
        .await?;
    // Scans are quick and installs long, so by kind.
    let updates: Vec<(String, String, i64)> = database
        .fetch_all(
            &Query::select()
                .columns([UpdateRuns::DeviceId, UpdateRuns::Kind])
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(UpdateRuns::Table)
                .and_where(awaiting_update_run(now))
                .group_by_col(UpdateRuns::DeviceId)
                .group_by_col(UpdateRuns::Kind)
                .to_owned(),
        )
        .await?;
    let applications = crate::applications::runs::awaiting(&mut &state.database, now).await?;
    let quick = runs
        .into_iter()
        .chain(deliveries)
        .map(|row| (row, Class::Quick));
    let updates = updates.into_iter().map(|(device_id, kind, count)| {
        let class = if kind == UpdateKind::Install.as_str() {
            Class::Long
        } else {
            Class::Quick
        };
        ((device_id, count), class)
    });
    let applications = applications.into_iter().map(|row| (row, Class::Long));
    for ((device_id, count), class) in quick.chain(updates).chain(applications) {
        *in_flight.of(class) += count;
        *in_flight.per_device.entry(device_id).or_default() += count;
    }
    Ok(in_flight)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct QueuedRow {
    job_id: String,
    device_id: String,
}

/// What the dispatcher needs of a job.
#[derive(Debug, Clone, sqlx::FromRow)]
struct JobRow {
    id: String,
    kind: String,
    item_id: String,
    item_name: String,
    language: Option<String>,
    body: Option<String>,
    timeout_seconds: Option<i64>,
    run_as: Option<String>,
    destination: Option<String>,
    update_categories: Option<String>,
    reboot: Option<String>,
    reboot_delay_minutes: Option<i64>,
    offline: String,
    expires_at: i64,
    requested_by_user_id: String,
    policy_name: Option<String>,
}

/// The SQL that is true of a queued device's part that could start now, as
/// far as the database can tell before the pass starts anything: its job's
/// class has room, and the device isn't at its limit and isn't running
/// what the job's kind waits for. [`start`] checks the last two again.
/// Wakes are started apart, by [`wake_queued`].
fn startable(per_device: &HashMap<String, i64>, open: &[Class]) -> Cond {
    let device = || Expr::col((JobDevices::Table, JobDevices::DeviceId));
    let kind = || Expr::col((Jobs::Table, Jobs::Kind));
    let at_limit = per_device
        .iter()
        .filter(|(_, count)| **count >= MAX_PER_DEVICE)
        .map(|(device_id, _)| device_id.clone())
        .collect::<Vec<_>>();
    let open = open
        .iter()
        .flat_map(|class| class.kinds())
        .map(|kind| kind.as_str());
    let mut free = Cond::all()
        .add(kind().is_in(open))
        .add(
            kind()
                .ne(Kind::UpdateInstall.as_str())
                .or(device().not_in_subquery(update_runs::pending_devices(UpdateKind::Install))),
        )
        .add(
            kind()
                .is_not_in(
                    [Kind::AppInstall, Kind::AppUpdate, Kind::AppUninstall].map(Kind::as_str),
                )
                .or(device().not_in_subquery(crate::applications::busy_devices())),
        );
    // Each has MAX_PER_DEVICE of the runs in flight, so the list stays under
    // half the ceilings together.
    if !at_limit.is_empty() {
        free = free.add(device().is_not_in(at_limit));
    }
    free
}

/// The order a pass takes queued parts in: the jobs in turn, one device of
/// each per round, and in each round the jobs with fewer parts left to
/// start first, then the older. Within a job, devices go in the order they
/// always have. So a small job isn't left behind a large one, even when
/// one slot frees at a time.
fn in_turn(select: &mut sea_query::SelectStatement) {
    select
        .order_by_expr(
            Expr::cust(
                "ROW_NUMBER() OVER (PARTITION BY job_devices.job_id ORDER BY job_devices.device_id)",
            ),
            Order::Asc,
        )
        .order_by_expr(
            Expr::cust("COUNT(*) OVER (PARTITION BY job_devices.job_id)"),
            Order::Asc,
        )
        .order_by((Jobs::Table, Jobs::CreatedAt), Order::Asc)
        .order_by((JobDevices::Table, JobDevices::JobId), Order::Asc)
        .order_by((JobDevices::Table, JobDevices::DeviceId), Order::Asc);
}

/// The queued parts of jobs, joined to their job.
fn queued_parts() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            (JobDevices::Table, JobDevices::JobId),
            (JobDevices::Table, JobDevices::DeviceId),
        ])
        .from(JobDevices::Table)
        .inner_join(
            Jobs::Table,
            Expr::col((Jobs::Table, Jobs::Id)).equals((JobDevices::Table, JobDevices::JobId)),
        )
        .and_where(
            Expr::col((JobDevices::Table, JobDevices::State)).eq(DeviceState::Queued.as_str()),
        )
        .to_owned()
}

/// Starts queued devices' runs and deliveries within the limits for the
/// Agents connected now, and wakes queued wake jobs' devices. Returns
/// whether another pass straight away could start more: it stopped at the
/// batch with devices of a class with room left, or more queued devices
/// may be ready than it read. A class at its ceiling isn't a reason: its
/// devices start once its runs report, which wakes the engine. Nor are
/// devices that can't start yet, for the same reason.
pub async fn dispatch(state: &AppState, now: i64) -> anyhow::Result<bool> {
    let database = &state.database;
    let limits = limits(state.agents.count());
    let more_wakes = wake_queued(state, limits.batch, now).await?;
    let mut in_flight = in_flight(state, now).await?;
    let open = in_flight.open(limits);
    if open.is_empty() {
        return Ok(more_wakes);
    }
    // Only parts that could start, so those that can't don't keep the ones
    // queued after them from being read.
    let mut select = queued_parts();
    select
        .cond_where(startable(&in_flight.per_device, &open))
        .limit(limits.page());
    in_turn(&mut select);
    let queued: Vec<QueuedRow> = database.fetch_all(&select).await?;
    if queued.is_empty() {
        return Ok(more_wakes);
    }
    let jobs = jobs_of(state, &queued).await?;
    let mut started = 0;
    let mut stopped = false;
    for row in &queued {
        let Some(job) = jobs.get(&row.job_id) else {
            continue;
        };
        let Some(class) = Kind::parse(&job.kind).and_then(Class::of) else {
            continue;
        };
        // It filled in this pass: the rest of the class waits for a report.
        if *in_flight.of(class) >= limits.in_flight {
            continue;
        }
        if started >= limits.batch {
            stopped = true;
            break;
        }
        // It reached its limit earlier in this pass.
        let busy = in_flight
            .per_device
            .entry(row.device_id.clone())
            .or_default();
        if *busy >= MAX_PER_DEVICE {
            continue;
        }
        // Its counts may change below.
        state.automation.recount(&job.id);
        if start(state, job, &row.device_id, now).await? {
            started += 1;
            *busy += 1;
            *in_flight.of(class) += 1;
        }
    }
    // Devices that reached their limit in this pass are left out of the next
    // one, so a full page may have more behind it that can start.
    let more_behind = queued.len() as u64 >= limits.page() && !in_flight.open(limits).is_empty();
    Ok(more_wakes || stopped || more_behind)
}

/// Wakes the devices of queued wake jobs, at most `batch`, oldest job
/// first, whatever the other jobs hold: a wake holds no run. Returns
/// whether more may be queued.
async fn wake_queued(state: &AppState, batch: u64, now: i64) -> anyhow::Result<bool> {
    let mut select = queued_parts();
    select
        .and_where(Expr::col((Jobs::Table, Jobs::Kind)).eq(Kind::Wake.as_str()))
        .order_by((Jobs::Table, Jobs::CreatedAt), Order::Asc)
        .order_by((JobDevices::Table, JobDevices::JobId), Order::Asc)
        .order_by((JobDevices::Table, JobDevices::DeviceId), Order::Asc)
        .limit(batch);
    let queued: Vec<QueuedRow> = state.database.fetch_all(&select).await?;
    if queued.is_empty() {
        return Ok(false);
    }
    let jobs = jobs_of(state, &queued).await?;
    let mut wakes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for row in &queued {
        wakes
            .entry(&row.job_id)
            .or_default()
            .push(row.device_id.clone());
    }
    for (job_id, devices) in wakes {
        if let Some(job) = jobs.get(job_id) {
            state.automation.recount(job_id);
            start_wakes(state, job, &devices, now).await?;
        }
    }
    Ok(queued.len() as u64 >= batch)
}

/// The jobs of these queued parts.
async fn jobs_of(state: &AppState, queued: &[QueuedRow]) -> db::Result<HashMap<String, JobRow>> {
    let job_ids = queued
        .iter()
        .map(|row| row.job_id.clone())
        .collect::<HashSet<_>>();
    let jobs: Vec<JobRow> = state
        .database
        .fetch_all(
            &Query::select()
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
                    Jobs::Offline,
                    Jobs::ExpiresAt,
                    Jobs::RequestedByUserId,
                    Jobs::PolicyName,
                ])
                .from(Jobs::Table)
                .and_where(Expr::col(Jobs::Id).is_in(job_ids))
                .to_owned(),
        )
        .await?;
    Ok(jobs.into_iter().map(|job| (job.id.clone(), job)).collect())
}

/// Wakes a wake job's queued devices that are still offline, and skips
/// those that came online meanwhile.
async fn start_wakes(
    state: &AppState,
    job: &JobRow,
    devices: &[String],
    now: i64,
) -> anyhow::Result<()> {
    let mut claimed = Vec::new();
    let mut transaction = state.database.begin().await?;
    for device_id in devices {
        let online = state.agents.is_connected(device_id);
        let (next, detail) = if online {
            (DeviceState::Skipped, Some("The device is online already"))
        } else {
            (DeviceState::Started, None)
        };
        let updated = transaction
            .execute(
                &Query::update()
                    .table(JobDevices::Table)
                    .value(JobDevices::State, next.as_str())
                    .value(JobDevices::Detail, detail)
                    .value(JobDevices::UpdatedAt, now)
                    .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                    .and_where(Expr::col(JobDevices::DeviceId).eq(device_id.as_str()))
                    .and_where(Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()))
                    .to_owned(),
            )
            .await?;
        if updated == 1 && !online {
            claimed.push(device_id.clone());
        }
    }
    transaction.commit().await?;
    if !claimed.is_empty() {
        crate::wake::start(
            state,
            &claimed,
            crate::wake::Requester {
                user_id: &job.requested_by_user_id,
                job_id: Some(&job.id),
                actor: None,
            },
        )
        .await?;
    }
    Ok(())
}

/// Starts the job on a queued device: records the run or delivery and
/// hands it to the Agent. A device that went offline waits again, or is
/// missed. Returns whether it started.
async fn start(state: &AppState, job: &JobRow, device_id: &str, now: i64) -> anyhow::Result<bool> {
    if !state.agents.is_connected(device_id) {
        unqueue(
            &mut &state.database,
            job,
            device_id,
            DeviceState::Queued,
            now,
        )
        .await?;
        return Ok(false);
    }
    let Some(kind) = Kind::parse(&job.kind) else {
        tracing::error!(job_id = job.id, "stored job is invalid");
        return Ok(false);
    };
    // A device installs one thing at a time; this waits for the install it
    // is running.
    if kind == Kind::UpdateInstall
        && update_runs::pending(&mut &state.database, device_id, UpdateKind::Install)
            .await?
            .is_some()
    {
        return Ok(false);
    }
    if kind.is_application() && crate::applications::busy(&mut &state.database, device_id).await? {
        return Ok(false);
    }
    let id = new_id();
    let mut transaction = state.database.begin().await?;
    let command = match kind {
        Kind::Script => {
            let (Some(language), Some(body), Some(timeout_seconds), Some(run_as)) = (
                job.language.as_deref().and_then(ScriptLanguage::parse),
                &job.body,
                job.timeout_seconds,
                job.run_as.as_deref().and_then(RunAs::parse),
            ) else {
                tracing::error!(job_id = job.id, "stored script job is invalid");
                return Ok(false);
            };
            toolbox::insert_run(
                &mut transaction,
                NewRun {
                    id: &id,
                    device_id,
                    script_id: &job.item_id,
                    script_name: &job.item_name,
                    language,
                    requested_by_user_id: &job.requested_by_user_id,
                    source: Source::Job,
                    run_as,
                    timeout_seconds,
                    job_id: Some(&job.id),
                    created_at: now,
                },
            )
            .await?;
            Some(AgentCommand::RunScript {
                run: ScriptRunRequest {
                    run_id: id.clone(),
                    language,
                    body: body.clone(),
                    run_as,
                    timeout_seconds: u32::try_from(timeout_seconds)?,
                },
            })
        }
        Kind::File => {
            let Some(destination) = job
                .destination
                .as_deref()
                .and_then(FileDeliveryDestination::parse)
            else {
                tracing::error!(job_id = job.id, "stored file job is invalid");
                return Ok(false);
            };
            let file = toolbox::file_by_id(&mut transaction, &job.item_id).await?;
            toolbox::insert_delivery(
                &mut transaction,
                NewDelivery {
                    id: &id,
                    device_id,
                    file_id: &job.item_id,
                    file_name: file.as_ref().map_or(&job.item_name, |file| &file.name),
                    size_bytes: file.as_ref().map_or(0, |file| file.size_bytes),
                    requested_by_user_id: &job.requested_by_user_id,
                    destination,
                    job_id: Some(&job.id),
                    created_at: now,
                },
            )
            .await?;
            match file {
                Some(file) => Some(AgentCommand::DeliverFile {
                    delivery: FileDeliveryRequest {
                        delivery_id: id.clone(),
                        file_name: file.name.clone(),
                        size_bytes: file.size_bytes(),
                        sha256: file.sha256.clone(),
                        destination,
                    },
                }),
                None => {
                    transaction
                        .execute(
                            &Query::update()
                                .table(FileDeliveries::Table)
                                .value(FileDeliveries::Status, "failed")
                                .value(FileDeliveries::Error, FILE_GONE)
                                .value(FileDeliveries::CompletedAt, now)
                                .and_where(Expr::col(FileDeliveries::Id).eq(id.as_str()))
                                .to_owned(),
                        )
                        .await?;
                    None
                }
            }
        }
        Kind::UpdateScan | Kind::UpdateInstall => {
            let (update_kind, install) = if kind == Kind::UpdateInstall {
                let Some(reboot) = job.reboot.as_deref().and_then(RebootBehavior::parse) else {
                    tracing::error!(job_id = job.id, "stored update job is invalid");
                    return Ok(false);
                };
                (
                    UpdateKind::Install,
                    Some(Install {
                        update_ids: Vec::new(),
                        categories: Install::parse_categories(job.update_categories.as_deref()),
                        reboot,
                        reboot_delay_minutes: u32::try_from(job.reboot_delay_minutes.unwrap_or(5))
                            .unwrap_or(5),
                    }),
                )
            } else {
                (UpdateKind::Scan, None)
            };
            update_runs::insert(
                &mut transaction,
                &update_runs::NewRun {
                    id: &id,
                    device_id,
                    kind: update_kind,
                    requested_by_user_id: &job.requested_by_user_id,
                    policy_name: job.policy_name.as_deref(),
                    job_id: Some(&job.id),
                    install: install.as_ref(),
                    created_at: now,
                },
            )
            .await?;
            Some(update_runs::command(&id, update_kind, install.as_ref()))
        }
        // Started together by `start_wakes`.
        Kind::Wake => return Ok(false),
        Kind::AppInstall | Kind::AppUpdate | Kind::AppUninstall => {
            let started = crate::applications::start_job_run(
                &mut transaction,
                &crate::applications::Start {
                    run_id: &id,
                    device_id,
                    requested_by_user_id: &job.requested_by_user_id,
                    policy_name: job.policy_name.as_deref(),
                    job_id: Some(&job.id),
                    now,
                },
            )
            .await;
            match started {
                Ok(crate::applications::JobStart::Run(command)) => command,
                // Nothing in it is for the device: skipped, not failed.
                Ok(crate::applications::JobStart::Skip(reason)) => {
                    transaction
                        .execute(
                            &Query::update()
                                .table(JobDevices::Table)
                                .value(JobDevices::State, DeviceState::Skipped.as_str())
                                .value(JobDevices::Detail, reason)
                                .value(JobDevices::UpdatedAt, now)
                                .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                                .and_where(
                                    Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()),
                                )
                                .to_owned(),
                        )
                        .await?;
                    transaction.commit().await?;
                    // Skipped, not started: the device is free for the next job.
                    return Ok(false);
                }
                // A run started on the device since `busy` looked; the
                // device's part waits for it.
                Err(error) if crate::applications::runs::is_already_pending(&error) => {
                    transaction.rollback().await?;
                    return Ok(false);
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let claimed = transaction
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Started.as_str())
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(Expr::col(JobDevices::State).eq(DeviceState::Queued.as_str()))
                .to_owned(),
        )
        .await?;
    if claimed == 0 {
        // Cancelled meanwhile.
        transaction.rollback().await?;
        return Ok(false);
    }
    transaction.commit().await?;
    let Some(command) = command else {
        return Ok(true);
    };
    if state.agents.send(device_id, command) {
        if matches!(kind, Kind::UpdateScan | Kind::UpdateInstall) {
            crate::updates::publish_run(state, &id).await;
            crate::updates::publish_device(state, device_id).await;
        }
        return Ok(true);
    }
    // The Agent went away before it got it: the run never existed.
    let mut transaction = state.database.begin().await?;
    match kind {
        Kind::Script => {
            transaction
                .execute(
                    &Query::delete()
                        .from_table(ScriptRuns::Table)
                        .and_where(Expr::col(ScriptRuns::Id).eq(id.as_str()))
                        .and_where(Expr::col(ScriptRuns::Status).eq("pending"))
                        .to_owned(),
                )
                .await?;
        }
        Kind::File => {
            transaction
                .execute(
                    &Query::delete()
                        .from_table(FileDeliveries::Table)
                        .and_where(Expr::col(FileDeliveries::Id).eq(id.as_str()))
                        .and_where(Expr::col(FileDeliveries::Status).eq("pending"))
                        .to_owned(),
                )
                .await?;
        }
        Kind::UpdateScan | Kind::UpdateInstall => {
            update_runs::discard(&mut transaction, &id).await?;
        }
        Kind::AppInstall | Kind::AppUpdate | Kind::AppUninstall => {
            crate::applications::runs::discard(&mut transaction, &id).await?;
        }
        Kind::Wake => {}
    }
    unqueue(&mut transaction, job, device_id, DeviceState::Started, now).await?;
    transaction.commit().await?;
    Ok(false)
}

/// A device in state `from` turned out to be offline: it waits again, or is
/// missed.
async fn unqueue(
    executor: &mut impl Executor,
    job: &JobRow,
    device_id: &str,
    from: DeviceState,
    now: i64,
) -> db::Result<()> {
    let waits = Offline::parse(&job.offline) == Some(Offline::Wait) && job.expires_at > now;
    let (next, detail) = if waits {
        (DeviceState::Waiting, None)
    } else {
        (DeviceState::Missed, Some("The device was offline"))
    };
    executor
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, next.as_str())
                .value(JobDevices::Detail, detail)
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::JobId).eq(job.id.as_str()))
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(Expr::col(JobDevices::State).eq(from.as_str()))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Misses the device's queued and waiting parts of jobs, now it is deleted,
/// and takes it out of the policies that chose it. Returns the jobs whose
/// parts it missed.
pub async fn device_removed(
    executor: &mut impl Executor,
    device_id: &str,
    now: i64,
) -> db::Result<Vec<String>> {
    let unstarted = || {
        Expr::col(JobDevices::State)
            .is_in([DeviceState::Waiting.as_str(), DeviceState::Queued.as_str()])
    };
    let jobs: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(JobDevices::JobId)
                .from(JobDevices::Table)
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(unstarted())
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::update()
                .table(JobDevices::Table)
                .value(JobDevices::State, DeviceState::Missed.as_str())
                .value(JobDevices::Detail, DELETED)
                .value(JobDevices::UpdatedAt, now)
                .and_where(Expr::col(JobDevices::DeviceId).eq(device_id))
                .and_where(unstarted())
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(AutomationPolicyDevices::Table)
                .and_where(Expr::col(AutomationPolicyDevices::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(jobs.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_leave_room_on_each_device() {
        // An Agent refuses a ninth script; jobs leave room for runs by hand.
        const { assert!(MAX_PER_DEVICE < 8) };
        const { assert!(MAX_BATCH as i64 <= MIN_IN_FLIGHT) };
    }

    #[test]
    fn limits_grow_with_the_connected_agents() {
        let at = |connected| {
            let limits = limits(connected);
            (limits.batch, limits.in_flight)
        };
        // A small server works as it always has.
        assert_eq!(at(0), (25, 500));
        assert_eq!(at(100), (25, 500));
        assert_eq!(at(1_000), (25, 500));
        assert_eq!(at(2_600), (26, 650));
        assert_eq!(at(10_000), (100, 2_500));
        assert_eq!(at(100_000), (250, 5_000));
        assert_eq!(at(usize::MAX), (MAX_BATCH, MAX_IN_FLIGHT));
    }

    #[test]
    fn every_kind_but_a_wake_has_a_class() {
        for kind in [
            Kind::Script,
            Kind::File,
            Kind::UpdateScan,
            Kind::UpdateInstall,
            Kind::Wake,
            Kind::AppInstall,
            Kind::AppUpdate,
            Kind::AppUninstall,
        ] {
            match Class::of(kind) {
                Some(class) => assert!(class.kinds().contains(&kind), "{kind:?}"),
                None => assert_eq!(kind, Kind::Wake),
            }
        }
        assert_eq!(Class::of(Kind::Script), Some(Class::Quick));
        assert_eq!(Class::of(Kind::UpdateInstall), Some(Class::Long));
        assert_eq!(Class::of(Kind::AppUninstall), Some(Class::Long));
    }
}
