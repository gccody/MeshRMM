//! Maintenance windows: time a device spends with its alerts silenced and
//! its scheduled automation held back, while someone works on it. See
//! docs/maintenance-windows.md. (Periodic housekeeping is
//! [`crate::maintenance`], which purges these too once they are old.)
//!
//! A window covers one device, from when it starts until it ends or someone
//! ends it. While a device is in one:
//!
//! - its alert rules aren't judged: nothing is raised, resolved by its
//!   condition, notified or healed, and its alerts open when the window
//!   started stay as they were (people can still acknowledge and resolve
//!   them);
//! - the jobs its policies' schedules start skip it, as the window's
//!   [`Automation`] says, recorded as skipped with the reason, and are
//!   not queued for later;
//! - whatever a person starts (sessions, terminals, jobs, Run now) runs.
//!
//! When the last window of a device ends, its rules are judged at once and
//! its monitors' latest results again, so a condition that still holds
//! raises its alert then.
//!
//! The windows not over yet are kept in memory ([`MaintenanceWindows`]), so
//! the alert engine, the dispatcher and presence ask about a device without
//! reading the database. Every change goes through this module, which
//! writes the database and then reads the device's windows again. The task
//! [`spawn`] starts wakes at each window's start and end, and every
//! [`TICK_INTERVAL`] for the update installs that windows cover.
pub mod store;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::Duration,
};

use sea_query::{Expr, ExprTrait, Order};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Notify;

use crate::{
    audit::{self, Actor, Target as AuditTarget},
    automation::jobs::Kind,
    db::{self, Executor, Transaction, tables::Agents},
    http::{ApiError, AppState},
    targets::Target,
    time::{DAY_MS, MINUTE_MS, now_ms},
    users::new_id,
};

/// The longest a reason may be, in characters.
pub const MAX_REASON_CHARS: usize = 500;
/// The longest a window may be planned to last.
pub const MAX_LENGTH_MS: i64 = 365 * DAY_MS;
/// How far ahead a window may start.
pub const MAX_LEAD_MS: i64 = 365 * DAY_MS;
/// How long windows are kept after they end.
pub const RETENTION_MS: i64 = 90 * DAY_MS;
/// How often the task looks for update installs whose windows can end,
/// besides each window's own start and end.
pub const TICK_INTERVAL: Duration = Duration::from_secs(15);
/// How long an update install's window lasts after its restart is due, for
/// the computer to restart and its Agent to reconnect.
pub const RESTART_GRACE_MS: i64 = 15 * MINUTE_MS;
/// The longest an update install's window lasts, if its install never
/// reports: as long as the server waits for the report.
pub const INSTALL_WINDOW_MS: i64 = crate::updates::runs::INSTALL_REPORT_WINDOW_MS;

/// What a window lets scheduled automation do on its device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Automation {
    /// Every scheduled job skips the device.
    #[default]
    Skip,
    /// Update scans run; every other scheduled job skips the device.
    Scans,
    /// Scheduled jobs run; only alerts are silenced.
    Run,
}

impl Automation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Scans => "scans",
            Self::Run => "run",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "skip" => Some(Self::Skip),
            "scans" => Some(Self::Scans),
            "run" => Some(Self::Run),
            _ => None,
        }
    }

    /// Whether a scheduled job of `kind` skips a device in such a window.
    pub fn skips(self, kind: Kind) -> bool {
        match self {
            Self::Skip => true,
            Self::Scans => kind != Kind::UpdateScan,
            Self::Run => false,
        }
    }

    /// How many kinds of job it holds back, to find the strictest of
    /// several windows.
    fn strictness(self) -> u8 {
        match self {
            Self::Skip => 2,
            Self::Scans => 1,
            Self::Run => 0,
        }
    }

    /// In words, for the audit log and the website.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Skip => "scheduled automation skips the device",
            Self::Scans => "only scheduled update scans run",
            Self::Run => "scheduled automation runs",
        }
    }
}

/// A window that isn't over yet, as the server keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub id: String,
    pub device_id: String,
    pub starts_at: i64,
    /// `None` until someone ends it.
    pub ends_at: Option<i64>,
    pub reason: String,
    pub automation: Automation,
    pub created_by: String,
    /// The update install it covers.
    pub update_run_id: Option<String>,
}

impl Held {
    /// Whether it is in force at `now`.
    pub fn in_force(&self, now: i64) -> bool {
        self.starts_at <= now && self.ends_at.is_none_or(|end| now < end)
    }

    /// Whether it can't be in force at `now` or later: it ended, or was
    /// ended before it started.
    pub fn over(&self, now: i64) -> bool {
        self.ends_at
            .is_some_and(|end| end <= now || end <= self.starts_at)
    }
}

/// A device's maintenance now, as the website shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Current {
    /// The window in force that lasts longest.
    pub window_id: String,
    pub reason: String,
    pub created_by: String,
    /// When the earliest window in force started.
    pub starts_at: i64,
    /// When the device leaves maintenance as its windows in force stand:
    /// `None` while one of them lasts until someone ends it.
    pub ends_at: Option<i64>,
    /// The strictest of the windows in force.
    pub automation: Automation,
    /// How many windows are in force.
    pub windows: usize,
}

/// What presence shows of a device's maintenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceMaintenance {
    /// In force now, if any window is.
    pub current: Option<Current>,
    /// When the next window that hasn't started yet starts.
    pub next_starts_at: Option<i64>,
}

/// The windows not over yet, by device, kept in memory and read again
/// from the database after every change; and the task's state.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceWindows {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    held: RwLock<HashMap<String, Vec<Held>>>,
    /// The devices in maintenance as of the last tick.
    in_force: Mutex<HashSet<String>>,
    /// Wakes the task.
    changed: Notify,
    /// One tick at a time.
    ticking: tokio::sync::Mutex<()>,
}

impl MaintenanceWindows {
    fn held(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Vec<Held>>> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.inner
            .held
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn held_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Vec<Held>>> {
        self.inner
            .held
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads every window not over at `now` from the database, in place of
    /// what was held.
    pub async fn load(&self, executor: &mut impl Executor, now: i64) -> db::Result<()> {
        let windows = store::not_over(executor, None, now).await?;
        let mut held: HashMap<String, Vec<Held>> = HashMap::new();
        for window in windows {
            held.entry(window.device_id.clone())
                .or_default()
                .push(window);
        }
        *self.held_mut() = held;
        Ok(())
    }

    /// Reads the windows of `device_ids` not over at `now` again, after a
    /// change to them, and wakes the task to act on it.
    pub async fn reload(
        &self,
        executor: &mut impl Executor,
        device_ids: &[String],
        now: i64,
    ) -> db::Result<()> {
        let mut found: HashMap<String, Vec<Held>> = HashMap::new();
        for chunk in device_ids.chunks(500) {
            for window in store::not_over(executor, Some(chunk), now).await? {
                found
                    .entry(window.device_id.clone())
                    .or_default()
                    .push(window);
            }
        }
        {
            let mut held = self.held_mut();
            for device_id in device_ids {
                match found.remove(device_id) {
                    Some(windows) => {
                        held.insert(device_id.clone(), windows);
                    }
                    None => {
                        held.remove(device_id);
                    }
                }
            }
        }
        self.inner.changed.notify_one();
        Ok(())
    }

    /// Forgets a deleted device's windows.
    pub fn forget(&self, device_id: &str) {
        self.held_mut().remove(device_id);
        self.inner.changed.notify_one();
    }

    /// Whether the device is in maintenance at `now`.
    pub fn in_maintenance(&self, device_id: &str, now: i64) -> bool {
        self.held()
            .get(device_id)
            .is_some_and(|windows| windows.iter().any(|window| window.in_force(now)))
    }

    /// Whether the device's alerts are held at `now` by a window other than
    /// the one covering the update install `run_id`: an install's own
    /// result still raises its alert.
    pub fn holds_alerts_besides_install(&self, device_id: &str, run_id: &str, now: i64) -> bool {
        self.held().get(device_id).is_some_and(|windows| {
            windows.iter().any(|window| {
                window.in_force(now) && window.update_run_id.as_deref() != Some(run_id)
            })
        })
    }

    /// The device's maintenance at `now`, if it is in maintenance.
    pub fn current(&self, device_id: &str, now: i64) -> Option<Current> {
        let held = self.held();
        current_of(held.get(device_id)?, now)
    }

    /// What presence shows of the device's maintenance at `now`: `None`
    /// with no window in force or to come.
    pub fn device(&self, device_id: &str, now: i64) -> Option<DeviceMaintenance> {
        let held = self.held();
        let windows = held.get(device_id)?;
        let current = current_of(windows, now);
        let next_starts_at = windows
            .iter()
            .filter(|window| window.starts_at > now && !window.over(now))
            .map(|window| window.starts_at)
            .min();
        (current.is_some() || next_starts_at.is_some()).then_some(DeviceMaintenance {
            current,
            next_starts_at,
        })
    }

    /// Why a scheduled job of `kind` skips the device at `now`, if it does.
    pub fn skip_reason(&self, device_id: &str, kind: Kind, now: i64) -> Option<String> {
        let current = self.current(device_id, now)?;
        current
            .automation
            .skips(kind)
            .then(|| skip_detail(&current))
    }

    /// The devices in maintenance at `now`.
    fn in_force_devices(&self, now: i64) -> HashSet<String> {
        self.held()
            .iter()
            .filter(|(_, windows)| windows.iter().any(|window| window.in_force(now)))
            .map(|(device_id, _)| device_id.clone())
            .collect()
    }

    /// The first start or end of a held window after `now`.
    fn next_change(&self, now: i64) -> Option<i64> {
        self.held()
            .values()
            .flatten()
            .flat_map(|window| [Some(window.starts_at), window.ends_at])
            .flatten()
            .filter(|at| *at > now)
            .min()
    }

    /// Drops the windows over at `now`.
    fn prune(&self, now: i64) {
        let mut held = self.held_mut();
        held.retain(|_, windows| {
            windows.retain(|window| !window.over(now));
            !windows.is_empty()
        });
    }

    /// Wakes the task.
    pub fn wake(&self) {
        self.inner.changed.notify_one();
    }
}

/// The device's maintenance at `now` from its windows.
fn current_of(windows: &[Held], now: i64) -> Option<Current> {
    let in_force = windows
        .iter()
        .filter(|window| window.in_force(now))
        .collect::<Vec<_>>();
    // The one that lasts longest; of those, the latest set.
    let longest = in_force.iter().max_by_key(|window| {
        (
            window.ends_at.is_none(),
            window.ends_at.unwrap_or(i64::MAX),
            window.starts_at,
        )
    })?;
    Some(Current {
        window_id: longest.id.clone(),
        reason: longest.reason.clone(),
        created_by: longest.created_by.clone(),
        starts_at: in_force
            .iter()
            .map(|window| window.starts_at)
            .min()
            .unwrap_or(longest.starts_at),
        ends_at: longest.ends_at,
        automation: in_force
            .iter()
            .map(|window| window.automation)
            .max_by_key(|automation| automation.strictness())
            .unwrap_or_default(),
        windows: in_force.len(),
    })
}

/// What a skipped job device's detail says.
pub fn skip_detail(current: &Current) -> String {
    match current.ends_at {
        Some(end) => format!(
            "In maintenance until {}: {}",
            crate::alerts::format_time(end),
            current.reason
        ),
        None => format!("In maintenance: {}", current.reason),
    }
}

/// Acts on what changed since the last tick: ends the windows of update
/// installs that reported, republishes the devices that entered or left
/// maintenance, and for those that left judges their alert rules and
/// monitors again at once.
pub async fn tick(state: &AppState, now: i64) -> anyhow::Result<()> {
    let windows = &state.maintenance_windows;
    let _one = windows.inner.ticking.lock().await;
    let finished = store::end_finished_installs(&state.database, now).await?;
    if !finished.is_empty() {
        windows.reload(&mut &state.database, &finished, now).await?;
    }
    let in_force = windows.in_force_devices(now);
    let (entered, left) = {
        let mut known = windows
            .inner
            .in_force
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entered = in_force.difference(&known).cloned().collect::<Vec<_>>();
        let left = known.difference(&in_force).cloned().collect::<Vec<_>>();
        *known = in_force;
        (entered, left)
    };
    windows.prune(now);
    let mut changed = entered.clone();
    changed.extend(left.iter().cloned());
    changed.extend(finished);
    changed.sort_unstable();
    changed.dedup();
    if !changed.is_empty() {
        state.presence.maintenance_changed(&changed).await;
    }
    for device_id in &entered {
        tracing::info!(device_id, "a device entered maintenance");
    }
    left_maintenance(state, &left, now).await;
    Ok(())
}

/// Judges the devices that left maintenance again at once: their monitors'
/// latest results here, and their other rules by having the alert engine
/// evaluate now, so conditions that still hold raise their alerts.
async fn left_maintenance(state: &AppState, left: &[String], now: i64) {
    for device_id in left {
        tracing::info!(device_id, "a device left maintenance");
        if let Err(error) = crate::monitors::maintenance_ended(state, device_id, now).await {
            tracing::warn!(
                device_id,
                error = format!("{error:#}"),
                "could not judge a device's monitors as it left maintenance"
            );
        }
    }
    if !left.is_empty() {
        state.alerts.judge_now();
    }
}

/// Runs the windows' task until it is aborted: a tick at each window's
/// start and end, after every change, and at least every
/// [`TICK_INTERVAL`].
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let windows = state.maintenance_windows.clone();
        loop {
            let now = now_ms();
            if let Err(error) = tick(&state, now).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not act on maintenance windows"
                );
            }
            let now = now_ms();
            let wait = windows
                .next_change(now)
                .map(|at| Duration::from_millis(u64::try_from(at - now).unwrap_or(0) + 1))
                .unwrap_or(TICK_INTERVAL)
                .min(TICK_INTERVAL);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = windows.inner.changed.notified() => {}
            }
        }
    })
}

/// When windows start and end, why, and what they hold back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub starts_at: i64,
    /// `None` until someone ends them.
    pub ends_at: Option<i64>,
    pub reason: String,
    pub automation: Automation,
}

/// Checks a reason: trimmed, 1 to [`MAX_REASON_CHARS`] characters, and no
/// control characters.
pub fn reason(text: &str) -> Result<String, ApiError> {
    let reason = text.trim();
    if reason.is_empty()
        || reason.chars().count() > MAX_REASON_CHARS
        || reason.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "say why, in 1 to {MAX_REASON_CHARS} characters with no line breaks"
        ))
        .with_code("invalid_maintenance"));
    }
    Ok(reason.to_owned())
}

/// Who sets windows: a user, or a policy (whose owner is `user_id`).
#[derive(Debug, Clone)]
pub struct Setter<'a> {
    pub user_id: Option<&'a str>,
    /// Shown as who set them: the user's name, or `policy “…”`.
    pub label: &'a str,
    /// Recorded in the audit log as setting them.
    pub actor: &'a Actor,
    pub policy: Option<(&'a str, &'a str)>,
}

/// A device that windows are set on.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// The devices `target` covers that are enrolled and not deleted and in a
/// company of `within`, by ID.
pub async fn resolve(
    executor: &mut impl Executor,
    target: &Target,
    within: &crate::access::CompanySet,
) -> db::Result<Vec<Device>> {
    let mut devices: Vec<Device> = Vec::new();
    let select = || {
        crate::agents::active()
            .column(Agents::Name)
            .and_where_option(within.company_condition(Agents::CompanyId))
            .order_by(Agents::Id, Order::Asc)
            .to_owned()
    };
    match target {
        Target::Devices(ids) => {
            for chunk in ids.chunks(500) {
                devices.extend(
                    executor
                        .fetch_all::<Device, _>(
                            &select()
                                .and_where(Expr::col(Agents::Id).is_in(chunk.iter().cloned()))
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
            devices = executor.fetch_all(&select).await?;
        }
    }
    Ok(devices)
}

/// Writes a window by `plan` on each of `devices` in `transaction`, with an
/// audit event each, at `now`. Returns their IDs. Call [`changed`] once it
/// commits.
pub async fn write(
    transaction: &mut Transaction,
    devices: &[Device],
    plan: &Plan,
    setter: &Setter<'_>,
    now: i64,
) -> db::Result<Vec<String>> {
    let windows = devices
        .iter()
        .map(|device| store::NewWindow {
            id: new_id(),
            device_id: &device.id,
            starts_at: plan.starts_at,
            ends_at: plan.ends_at,
            reason: &plan.reason,
            automation: plan.automation,
            created_by_user_id: setter.user_id,
            created_by: setter.label,
            policy: setter.policy,
            update_run_id: None,
        })
        .collect::<Vec<_>>();
    store::insert(transaction, &windows, now).await?;
    let action = if plan.starts_at <= now {
        "maintenance_window.start"
    } else {
        "maintenance_window.schedule"
    };
    for (window, device) in windows.iter().zip(devices) {
        audit::record(
            transaction,
            setter.actor,
            action,
            AuditTarget::device(&device.id),
            json!({
                "name": device.name,
                "window_id": window.id,
                "reason": plan.reason,
                "starts_at": plan.starts_at,
                "ends_at": plan.ends_at,
                "automation": plan.automation,
                "policy_id": setter.policy.map(|(id, _)| id),
            }),
        )
        .await?;
    }
    Ok(windows.into_iter().map(|window| window.id).collect())
}

/// After windows of `device_ids` changed and committed: reads them again,
/// republishes the devices, judges those the change took out of
/// maintenance again at once, and wakes the task for the windows' new
/// starts and ends.
pub async fn changed(state: &AppState, device_ids: &[String], now: i64) {
    if device_ids.is_empty() {
        return;
    }
    let windows = &state.maintenance_windows;
    // As a tick does, so the two agree on who is in maintenance.
    let one = windows.inner.ticking.lock().await;
    if let Err(error) = windows.reload(&mut &state.database, device_ids, now).await {
        // The task's load after a restart puts it right; until then the
        // devices are as they were.
        tracing::warn!(%error, "could not read changed maintenance windows");
        return;
    }
    let left = {
        let mut known = windows
            .inner
            .in_force
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut left = Vec::new();
        for device_id in device_ids {
            if windows.in_maintenance(device_id, now) {
                known.insert(device_id.clone());
            } else if known.remove(device_id) {
                left.push(device_id.clone());
            }
        }
        left
    };
    drop(one);
    state.presence.maintenance_changed(device_ids).await;
    left_maintenance(state, &left, now).await;
}

/// The device was deleted: its windows go with it.
pub async fn device_removed(state: &AppState, device_id: &str) {
    if let Err(error) = store::delete_for_device(&mut &state.database, device_id).await {
        tracing::warn!(device_id, %error, "could not delete a deleted device's maintenance windows");
    }
    state.maintenance_windows.forget(device_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, starts_at: i64, ends_at: Option<i64>, automation: Automation) -> Held {
        Held {
            id: id.into(),
            device_id: "d".into(),
            starts_at,
            ends_at,
            reason: format!("reason {id}"),
            automation,
            created_by: "someone".into(),
            update_run_id: None,
        }
    }

    #[test]
    fn windows_are_in_force_from_their_start_until_their_end() {
        let held = window("a", 100, Some(200), Automation::Skip);
        assert!(!held.in_force(99));
        assert!(held.in_force(100) && held.in_force(199));
        assert!(!held.in_force(200));
        assert!(!held.over(199) && held.over(200));
        let open = window("b", 100, None, Automation::Skip);
        assert!(open.in_force(i64::MAX) && !open.over(i64::MAX));
        let cancelled = window("c", 100, Some(50), Automation::Skip);
        assert!(!cancelled.in_force(75) && !cancelled.in_force(100) && cancelled.over(0));
    }

    #[test]
    fn several_windows_add_up_to_the_longest_and_strictest() {
        let windows = [
            window("short", 100, Some(200), Automation::Skip),
            window("long", 150, Some(400), Automation::Run),
            window("later", 500, None, Automation::Scans),
        ];
        let current = current_of(&windows, 160).unwrap();
        assert_eq!(current.window_id, "long");
        assert_eq!(current.starts_at, 100);
        assert_eq!(current.ends_at, Some(400));
        assert_eq!(current.automation, Automation::Skip);
        assert_eq!(current.windows, 2);
        let current = current_of(&windows, 300).unwrap();
        assert_eq!((current.automation, current.windows), (Automation::Run, 1));
        assert_eq!(current_of(&windows, 450), None);
        let current = current_of(&windows, 600).unwrap();
        assert_eq!(
            (current.window_id.as_str(), current.ends_at),
            ("later", None)
        );
    }

    #[test]
    fn automation_holds_back_scheduled_jobs_by_kind() {
        assert!(Automation::Skip.skips(Kind::UpdateScan));
        assert!(Automation::Skip.skips(Kind::Wake));
        assert!(!Automation::Scans.skips(Kind::UpdateScan));
        assert!(Automation::Scans.skips(Kind::UpdateInstall));
        assert!(Automation::Scans.skips(Kind::Script));
        assert!(!Automation::Run.skips(Kind::UpdateInstall));
        for automation in [Automation::Skip, Automation::Scans, Automation::Run] {
            assert_eq!(Automation::parse(automation.as_str()), Some(automation));
        }
    }

    #[test]
    fn the_registry_answers_for_devices_and_finds_the_next_change() {
        let windows = MaintenanceWindows::default();
        windows.held_mut().insert(
            "d".into(),
            vec![
                window("now", 100, Some(200), Automation::Scans),
                window("later", 300, Some(400), Automation::Skip),
            ],
        );
        assert!(windows.in_maintenance("d", 150));
        assert!(!windows.in_maintenance("d", 250));
        assert!(!windows.in_maintenance("other", 150));
        assert_eq!(windows.skip_reason("d", Kind::UpdateScan, 150), None);
        assert_eq!(
            windows.skip_reason("d", Kind::Script, 150).as_deref(),
            Some("In maintenance until 1970-01-01 00:00:00 UTC: reason now")
        );
        assert_eq!(windows.next_change(150), Some(200));
        assert_eq!(windows.next_change(200), Some(300));
        let device = windows.device("d", 150).unwrap();
        assert_eq!(device.next_starts_at, Some(300));
        assert_eq!(device.current.unwrap().window_id, "now");
        windows.prune(250);
        assert_eq!(windows.held()["d"].len(), 1);
        windows.prune(400);
        assert!(windows.device("d", 400).is_none());
    }
}
