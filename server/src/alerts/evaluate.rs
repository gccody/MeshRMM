//! Judging device-state rules: what the engine remembers about each device
//! (its recent CPU and memory readings and since when it has been offline)
//! and whether a rule's condition holds for a device now.
use std::collections::{HashMap, HashSet, VecDeque};

use meshrmm_protocol_types::{METRICS_INTERVAL_SECONDS, SystemMetrics, UpdateCategory};

use super::rules::{Condition, MAX_SUSTAINED_MINUTES, Rule, ThresholdUnit, format_number, plural};
use crate::time::{DAY_MS, MINUTE_MS, SECOND_MS};

/// Readings this far apart still cover a window without a gap: two of the
/// Agent's reporting intervals.
const COVERAGE_SLACK_MS: i64 = 10 * SECOND_MS;
/// A device whose latest reading is older than this has stopped reporting,
/// so its loads are unknown.
const STALE_MS: i64 = 30 * SECOND_MS;
/// Room for readings beyond one per reporting interval, for jitter, before
/// a device's readings have to grow.
const SPARE_READINGS: usize = 4;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A reading as the engine keeps it, in 12 bytes. The CPU load arrives as an
/// `f32`; memory use held as one is off by at most 0.000004 percentage
/// points.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Milliseconds after its device's [`Readings::base`].
    offset: u32,
    cpu_percent: f32,
    memory_percent: f32,
}

/// A connected device's readings over the longest window the enabled load
/// rules use, oldest first. A disconnect drops them, so they never span one.
#[derive(Debug)]
struct Readings {
    /// The Unix milliseconds that offsets count from.
    base: i64,
    /// When the newest reading dropped for being older than the window was
    /// taken: the device has reported since then, and every reading after
    /// it is here.
    dropped: Option<i64>,
    samples: VecDeque<Sample>,
}

/// How many readings a window of `minutes` holds at the Agent's interval.
fn room(minutes: i64) -> usize {
    let interval = i64::try_from(METRICS_INTERVAL_SECONDS).unwrap_or(5) * SECOND_MS;
    usize::try_from((minutes * MINUTE_MS + COVERAGE_SLACK_MS) / interval).unwrap_or(0)
        + 1
        + SPARE_READINGS
}

impl Readings {
    fn new(minutes: i64) -> Self {
        Self {
            base: 0,
            dropped: None,
            samples: VecDeque::with_capacity(room(minutes)),
        }
    }

    fn at(&self, sample: &Sample) -> i64 {
        self.base + i64::from(sample.offset)
    }

    fn latest(&self) -> Option<i64> {
        self.samples.back().map(|sample| self.at(sample))
    }

    /// Drops the readings taken before `oldest`.
    fn drop_before(&mut self, oldest: i64) {
        while let Some(first) = self.samples.front().map(|sample| self.at(sample))
            && first < oldest
        {
            self.dropped = Some(first);
            self.samples.pop_front();
        }
    }

    /// Keeps the readings of the `minutes` before the latest, with room for
    /// as many.
    fn keep(&mut self, minutes: i64) {
        if let Some(latest) = self.latest() {
            self.drop_before(latest - minutes * MINUTE_MS - COVERAGE_SLACK_MS);
        }
        let room = room(minutes);
        if self.samples.capacity() > room {
            self.samples.shrink_to(room);
        } else {
            self.samples
                .reserve_exact(room.saturating_sub(self.samples.len()));
        }
    }

    /// Adds a reading taken at `at`, after the others, keeping those of the
    /// `minutes` before it.
    fn push(&mut self, at: i64, cpu_percent: f32, memory_percent: f32, minutes: i64) {
        self.drop_before(at - minutes * MINUTE_MS - COVERAGE_SLACK_MS);
        let Some(first) = self.samples.front().map(|sample| sample.offset) else {
            self.base = at;
            self.samples.push_back(Sample {
                offset: 0,
                cpu_percent,
                memory_percent,
            });
            return;
        };
        if u32::try_from(at - self.base).is_err() {
            // After 49 days of one connection: count from the oldest reading
            // kept, which is within the window.
            for sample in &mut self.samples {
                sample.offset -= first;
            }
            self.base += i64::from(first);
        }
        self.samples.push_back(Sample {
            offset: u32::try_from(at - self.base).unwrap_or(u32::MAX),
            cpu_percent,
            memory_percent,
        });
    }
}

/// What the tracker holds of a device's readings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub readings: usize,
    /// The bytes they take with their room for more, not counting the
    /// device's id.
    pub bytes: usize,
}

/// The longest window of the enabled CPU and memory rules among `rules`.
fn longest_window<'a>(rules: impl IntoIterator<Item = &'a Rule>) -> Option<i64> {
    rules
        .into_iter()
        .filter(|rule| rule.enabled && matches!(rule.condition, Condition::Cpu | Condition::Memory))
        .map(|rule| rule.duration_minutes.unwrap_or(1))
        .max()
}

/// What the engine remembers between evaluations. It starts empty with the
/// server, so after a restart a device counts as offline from the first
/// evaluation, and sustained loads need a full window of new readings.
#[derive(Debug, Default)]
pub struct Tracker {
    /// The longest window of the enabled load rules, in minutes: each
    /// connected device's readings are kept that long, and not at all while
    /// there is no such rule.
    window_minutes: Option<i64>,
    readings: HashMap<String, Readings>,
    offline_since: HashMap<String, i64>,
}

impl Tracker {
    /// Keeps the readings the enabled load rules among `rules` need: those
    /// of their longest window, or none.
    pub fn keep_for(&mut self, rules: &[Rule]) {
        self.keep(longest_window(rules));
    }

    /// Keeps enough readings for `rule` too, as a rule just saved needs
    /// before the next evaluation reads them all.
    pub fn keep_also_for(&mut self, rule: &Rule) {
        self.keep(longest_window([rule]).max(self.window_minutes));
    }

    fn keep(&mut self, window_minutes: Option<i64>) {
        if window_minutes == self.window_minutes {
            return;
        }
        self.window_minutes = window_minutes;
        match window_minutes {
            Some(minutes) => {
                for readings in self.readings.values_mut() {
                    readings.keep(minutes);
                }
            }
            None => self.readings = HashMap::new(),
        }
    }

    /// Whether a rule reads the devices' loads.
    pub fn wants_readings(&self) -> bool {
        self.window_minutes.is_some()
    }

    /// Remembers a reading the device's Agent reported at `at`, if a rule
    /// needs it.
    pub fn observe(&mut self, device_id: &str, at: i64, metrics: &SystemMetrics) {
        let Some(minutes) = self.window_minutes else {
            return;
        };
        let memory_percent = if metrics.memory_total_bytes == 0 {
            0.0
        } else {
            metrics.memory_used_bytes as f64 / metrics.memory_total_bytes as f64 * 100.0
        };
        if !self.readings.contains_key(device_id) {
            self.readings
                .insert(device_id.to_owned(), Readings::new(minutes));
        }
        let Some(readings) = self.readings.get_mut(device_id) else {
            return;
        };
        if readings.latest().is_some_and(|latest| latest >= at) {
            return;
        }
        readings.push(at, metrics.cpu_percent, memory_percent as f32, minutes);
    }

    /// What it holds of the device's readings.
    pub fn held(&self, device_id: &str) -> Held {
        self.readings
            .get(device_id)
            .map_or_else(Held::default, |readings| Held {
                readings: readings.samples.len(),
                bytes: size_of::<Readings>() + readings.samples.capacity() * size_of::<Sample>(),
            })
    }

    /// Notes which of `devices` are connected at `now`. A device counts as
    /// offline from the first time it's seen disconnected.
    pub fn presence(&mut self, devices: &[String], connected: &HashSet<String>, now: i64) {
        for device_id in devices {
            if connected.contains(device_id) {
                self.offline_since.remove(device_id);
            } else {
                self.offline_since.entry(device_id.clone()).or_insert(now);
                self.readings.remove(device_id);
            }
        }
        let known = devices.iter().collect::<HashSet<_>>();
        self.offline_since.retain(|id, _| known.contains(id));
        self.readings.retain(|id, _| known.contains(id));
    }

    pub fn forget(&mut self, device_id: &str) {
        self.readings.remove(device_id);
        self.offline_since.remove(device_id);
    }

    fn offline_since(&self, device_id: &str) -> Option<i64> {
        self.offline_since.get(device_id).copied()
    }

    /// The device's average of `value` over the last `minutes`, if its
    /// readings cover all of them.
    fn sustained(
        &self,
        device_id: &str,
        minutes: i64,
        now: i64,
        value: fn(&Sample) -> f32,
    ) -> Option<f64> {
        let readings = self.readings.get(device_id)?;
        let latest = readings.latest()?;
        let from = now - minutes * MINUTE_MS;
        let covered = match readings.dropped {
            // A reading inside the window was dropped: the rule's window is
            // longer than what was kept until it was saved.
            Some(dropped) if dropped > from => false,
            // The device reported before the window. One quiet for longer
            // than any window, while connected, starts over instead.
            Some(dropped)
                if dropped >= latest - MAX_SUSTAINED_MINUTES * MINUTE_MS - COVERAGE_SLACK_MS =>
            {
                true
            }
            _ => readings.at(readings.samples.front()?) <= from + COVERAGE_SLACK_MS,
        };
        if !covered || latest < now - STALE_MS {
            return None;
        }
        let first = readings
            .samples
            .partition_point(|sample| readings.at(sample) <= from);
        let window = || {
            readings
                .samples
                .range(first..)
                .map(|sample| f64::from(value(sample)))
        };
        let count = window().count();
        (count > 0).then(|| window().sum::<f64>() / count as f64)
    }
}

/// What a device is like now, for judging rules.
#[derive(Debug, Clone, Copy)]
pub struct DeviceState<'a> {
    pub id: &'a str,
    pub connected: bool,
    /// The Agent went offline to install an update and isn't back yet.
    pub updating: bool,
    /// The latest reading, while the Agent is online.
    pub latest: Option<&'a SystemMetrics>,
    /// What its last update scan found; `None` if it never reported one.
    pub updates: Option<UpdateFacts<'a>>,
}

/// What a device's update scans say, for update rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateFacts<'a> {
    /// Each pending update's category, and when a scan first listed it.
    pub pending: &'a [(UpdateCategory, i64)],
    /// Since when the device has had to restart, if it has.
    pub reboot_pending_since: Option<i64>,
}

/// An alert a rule's condition calls for.
#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    /// The volume, for disk space; empty otherwise.
    pub subject: String,
    pub value: f64,
    pub detail: String,
}

/// A rule's verdict on one device.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Nothing can be said, such as a load rule while the device is offline:
    /// its alerts stay as they are.
    Unknown,
    /// These alerts should be open, and the rule's other alerts for the
    /// device resolved.
    Known(Vec<Breach>),
}

/// Judges a device-state rule for `device` at `now`.
pub fn judge(rule: &Rule, device: DeviceState<'_>, tracker: &Tracker, now: i64) -> Verdict {
    let minutes = rule.duration_minutes.unwrap_or(1);
    let threshold = rule.threshold.unwrap_or_default();
    match rule.condition {
        Condition::DeviceOffline => {
            if device.connected {
                return Verdict::Known(Vec::new());
            }
            match tracker.offline_since(device.id) {
                Some(since) if !device.updating && now - since >= minutes * MINUTE_MS => {
                    let offline = (now - since) / MINUTE_MS;
                    Verdict::Known(vec![Breach {
                        subject: String::new(),
                        value: offline as f64,
                        detail: format!("Offline for {}", plural(offline, "minute")),
                    }])
                }
                _ => Verdict::Unknown,
            }
        }
        Condition::DiskSpace => {
            let Some(latest) = device.latest.filter(|_| device.connected) else {
                return Verdict::Unknown;
            };
            let unit = rule.threshold_unit.unwrap_or(ThresholdUnit::Percent);
            Verdict::Known(
                latest
                    .volumes
                    .iter()
                    .filter(|volume| volume.total_bytes > 0)
                    .filter_map(|volume| {
                        let free_gb = volume.free_bytes as f64 / GIB;
                        let free_percent =
                            volume.free_bytes as f64 / volume.total_bytes as f64 * 100.0;
                        let value = match unit {
                            ThresholdUnit::Percent => free_percent,
                            ThresholdUnit::Gb => free_gb,
                        };
                        (value < threshold).then(|| Breach {
                            subject: volume.name.clone(),
                            value: round2(value),
                            detail: format!(
                                "{} has {} GB free ({}%)",
                                volume.name,
                                format_number(free_gb),
                                format_number(free_percent)
                            ),
                        })
                    })
                    .collect(),
            )
        }
        Condition::Cpu | Condition::Memory => {
            if !device.connected {
                return Verdict::Unknown;
            }
            let (what, value): (&str, fn(&Sample) -> f32) = if rule.condition == Condition::Cpu {
                ("CPU", |sample| sample.cpu_percent)
            } else {
                ("Memory", |sample| sample.memory_percent)
            };
            match tracker.sustained(device.id, minutes, now, value) {
                None => Verdict::Unknown,
                Some(average) if average > threshold => Verdict::Known(vec![Breach {
                    subject: String::new(),
                    value: round2(average),
                    detail: format!(
                        "{what} averaged {}% over {}",
                        format_number(average),
                        plural(minutes, "minute")
                    ),
                }]),
                Some(_) => Verdict::Known(Vec::new()),
            }
        }
        Condition::UpdatesPending => {
            let Some(updates) = device.updates else {
                return Verdict::Unknown;
            };
            let days = rule.days.unwrap_or(1);
            let category = rule.category();
            let overdue = updates
                .pending
                .iter()
                .filter(|(pending, _)| category.is_none_or(|wanted| wanted == *pending))
                .map(|(_, first_seen)| *first_seen)
                .filter(|first_seen| now - first_seen >= days * DAY_MS)
                .collect::<Vec<_>>();
            let Some(oldest) = overdue.iter().min() else {
                return Verdict::Known(Vec::new());
            };
            let noun = match category {
                Some(category) => format!("{} update", category.label().to_lowercase()),
                None => "update".to_owned(),
            };
            Verdict::Known(vec![Breach {
                subject: String::new(),
                value: overdue.len() as f64,
                detail: format!(
                    "{} pending for more than {}, the oldest since {}",
                    plural(overdue.len() as i64, &noun),
                    plural(days, "day"),
                    super::format_time(*oldest)
                ),
            }])
        }
        Condition::RebootPending => {
            let Some(updates) = device.updates else {
                return Verdict::Unknown;
            };
            let days = rule.days.unwrap_or(1);
            match updates.reboot_pending_since {
                Some(since) if now - since >= days * DAY_MS => Verdict::Known(vec![Breach {
                    subject: String::new(),
                    value: ((now - since) / DAY_MS) as f64,
                    detail: format!(
                        "Has had to restart to finish installing updates since {}",
                        super::format_time(since)
                    ),
                }]),
                _ => Verdict::Known(Vec::new()),
            }
        }
        // Failures are checked as runs finish, application rules in
        // super::applications, and monitors as their Agents report.
        Condition::ScriptFailed
        | Condition::UpdateInstallFailed
        | Condition::AppInstallFailed
        | Condition::AppUpdatesPending
        | Condition::Service
        | Condition::Process
        | Condition::EventLog
        | Condition::Ping
        | Condition::TcpPort
        | Condition::Http
        | Condition::Certificate
        | Condition::ScriptCheck => Verdict::Unknown,
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::VolumeUsage;

    use super::*;
    use crate::alerts::rules::Severity;

    const START: i64 = 1_000_000 * MINUTE_MS;

    fn rule(condition: Condition, threshold: Option<f64>, minutes: Option<i64>) -> Rule {
        Rule {
            id: "r".into(),
            name: "R".into(),
            severity: Severity::Warning,
            enabled: true,
            condition,
            threshold,
            threshold_unit: (condition == Condition::DiskSpace).then_some(ThresholdUnit::Percent),
            duration_minutes: minutes,
            nonzero_exit_fails: false,
            update_category: None,
            days: None,
            target: crate::targets::Target::All,
            channel_ids: vec![],
            notify_on_resolve: false,
            monitor: None,
            remediation: None,
            notify_when: Default::default(),
            owner_user_id: None,
            check_script: None,
            remediation_script: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn metrics(cpu: f32, memory_used: u64) -> SystemMetrics {
        SystemMetrics {
            cpu_percent: cpu,
            memory_used_bytes: memory_used,
            memory_total_bytes: 100,
            network_received_bytes_per_second: 0,
            network_sent_bytes_per_second: 0,
            uptime_seconds: 1,
            volumes: vec![
                VolumeUsage {
                    name: "C:".into(),
                    total_bytes: 100 * GIB as u64,
                    free_bytes: 4 * GIB as u64,
                },
                VolumeUsage {
                    name: "D:".into(),
                    total_bytes: 100 * GIB as u64,
                    free_bytes: 50 * GIB as u64,
                },
            ],
        }
    }

    fn online<'a>(latest: Option<&'a SystemMetrics>) -> DeviceState<'a> {
        DeviceState {
            id: "d",
            connected: true,
            updating: false,
            latest,
            updates: None,
        }
    }

    fn offline() -> DeviceState<'static> {
        DeviceState {
            id: "d",
            connected: false,
            updating: false,
            latest: None,
            updates: None,
        }
    }

    fn connected(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn devices_count_as_offline_from_the_server_start_or_their_disconnect() {
        let mut tracker = Tracker::default();
        let devices = vec!["d".to_owned()];
        let rule = rule(Condition::DeviceOffline, None, Some(10));
        tracker.presence(&devices, &connected(&[]), START);
        assert_eq!(
            judge(&rule, offline(), &tracker, START + 9 * MINUTE_MS),
            Verdict::Unknown,
            "not offline long enough since it was first seen offline"
        );
        let Verdict::Known(breaches) = judge(&rule, offline(), &tracker, START + 12 * MINUTE_MS)
        else {
            panic!("offline long enough");
        };
        assert_eq!(breaches[0].detail, "Offline for 12 minutes");
        let updating = DeviceState {
            updating: true,
            ..offline()
        };
        assert_eq!(
            judge(&rule, updating, &tracker, START + 12 * MINUTE_MS),
            Verdict::Unknown
        );

        tracker.presence(&devices, &connected(&["d"]), START + 13 * MINUTE_MS);
        assert_eq!(
            judge(&rule, online(None), &tracker, START + 13 * MINUTE_MS),
            Verdict::Known(vec![])
        );
        // Disconnecting again starts a new count from then.
        tracker.presence(&devices, &connected(&[]), START + 20 * MINUTE_MS);
        assert_eq!(
            judge(&rule, offline(), &tracker, START + 25 * MINUTE_MS),
            Verdict::Unknown
        );
        assert!(matches!(
            judge(&rule, offline(), &tracker, START + 30 * MINUTE_MS),
            Verdict::Known(breaches) if breaches.len() == 1
        ));
    }

    #[test]
    fn loads_need_a_full_window_and_compare_its_average() {
        let mut tracker = Tracker::default();
        let cpu = rule(Condition::Cpu, Some(80.0), Some(2));
        let memory = rule(Condition::Memory, Some(50.0), Some(2));
        tracker.keep_for(&[cpu.clone(), memory.clone()]);
        let mut at = START;
        for _ in 0..12 {
            tracker.observe("d", at, &metrics(90.0, 40));
            at += 5 * SECOND_MS;
        }
        // One minute of readings doesn't cover two.
        assert_eq!(judge(&cpu, online(None), &tracker, at), Verdict::Unknown);
        for _ in 0..12 {
            tracker.observe("d", at, &metrics(90.0, 40));
            at += 5 * SECOND_MS;
        }
        let Verdict::Known(breaches) = judge(&cpu, online(None), &tracker, at) else {
            panic!("two minutes are covered");
        };
        assert_eq!(breaches[0].value, 90.0);
        assert_eq!(breaches[0].detail, "CPU averaged 90% over 2 minutes");
        assert_eq!(
            judge(&memory, online(None), &tracker, at),
            Verdict::Known(vec![])
        );
        // A spike doesn't clear it, but a lower average does.
        for _ in 0..24 {
            tracker.observe("d", at, &metrics(10.0, 60));
            at += 5 * SECOND_MS;
        }
        assert_eq!(
            judge(&cpu, online(None), &tracker, at),
            Verdict::Known(vec![])
        );
        assert!(matches!(
            judge(&memory, online(None), &tracker, at),
            Verdict::Known(breaches) if breaches.len() == 1
        ));
        // Stale readings say nothing, and a disconnect forgets them.
        assert_eq!(
            judge(&cpu, online(None), &tracker, at + MINUTE_MS),
            Verdict::Unknown
        );
        tracker.presence(&["d".to_owned()], &connected(&[]), at);
        assert_eq!(judge(&cpu, online(None), &tracker, at), Verdict::Unknown);
    }

    /// Feeds the tracker a reading every five seconds over `minutes` from
    /// `at`, and returns when the next would be.
    fn report(tracker: &mut Tracker, mut at: i64, minutes: i64, cpu: f32) -> i64 {
        for _ in 0..minutes * 12 {
            tracker.observe("d", at, &metrics(cpu, 40));
            at += 5 * SECOND_MS;
        }
        at
    }

    #[test]
    fn readings_are_kept_only_while_an_enabled_rule_reads_loads() {
        let mut tracker = Tracker::default();
        report(&mut tracker, START, 120, 90.0);
        assert_eq!(tracker.held("d"), Held::default(), "no rules yet");
        assert!(!tracker.wants_readings());

        let disabled = Rule {
            enabled: false,
            ..rule(Condition::Cpu, Some(80.0), Some(10))
        };
        tracker.keep_for(&[
            rule(Condition::DeviceOffline, None, Some(10)),
            rule(Condition::DiskSpace, Some(10.0), None),
            disabled,
        ]);
        report(&mut tracker, START + 120 * MINUTE_MS, 120, 90.0);
        assert_eq!(tracker.held("d"), Held::default(), "no rule reads loads");
        assert!(!tracker.wants_readings());
    }

    #[test]
    fn readings_reach_back_as_far_as_the_longest_load_rule() {
        assert_eq!(size_of::<Sample>(), 12);
        let mut tracker = Tracker::default();
        let cpu = rule(Condition::Cpu, Some(80.0), Some(10));
        let hour = rule(Condition::Memory, Some(30.0), Some(60));
        tracker.keep_for(&[
            cpu.clone(),
            Rule {
                enabled: false,
                ..hour.clone()
            },
        ]);
        let mut at = report(&mut tracker, START, 120, 90.0);
        let ten_minutes = Held {
            // Ten minutes and the coverage slack, at one every five seconds.
            readings: 123,
            bytes: size_of::<Readings>() + room(10) * size_of::<Sample>(),
        };
        assert_eq!(tracker.held("d"), ten_minutes);
        assert_eq!(ten_minutes.bytes, 1_580);
        assert!(matches!(
            judge(&cpu, online(None), &tracker, at),
            Verdict::Known(breaches) if breaches.len() == 1
        ));

        // A longer rule has only what was kept until it was saved: it says
        // nothing until its own window fills, the ten minutes kept and fifty
        // more.
        tracker.keep_also_for(&hour);
        assert_eq!(judge(&hour, online(None), &tracker, at), Verdict::Unknown);
        at = report(&mut tracker, at, 49, 90.0);
        assert_eq!(judge(&hour, online(None), &tracker, at), Verdict::Unknown);
        at = report(&mut tracker, at, 1, 90.0);
        assert!(matches!(
            judge(&hour, online(None), &tracker, at),
            Verdict::Known(breaches) if breaches.len() == 1
        ));
        let an_hour = Held {
            readings: 723,
            bytes: size_of::<Readings>() + room(60) * size_of::<Sample>(),
        };
        assert_eq!(tracker.held("d"), an_hour);
        assert_eq!(an_hour.bytes, 8_780);
        // Keeping more for one rule doesn't change another's verdict.
        assert!(matches!(
            judge(&cpu, online(None), &tracker, at),
            Verdict::Known(breaches) if breaches.len() == 1
        ));

        // The hour rule is off again: back to ten minutes, and the room.
        tracker.keep_for(std::slice::from_ref(&cpu));
        assert_eq!(tracker.held("d"), ten_minutes);
        // A shorter rule saved doesn't shrink what is kept until the next
        // evaluation reads every rule.
        tracker.keep_also_for(&rule(Condition::Cpu, Some(80.0), Some(1)));
        assert_eq!(tracker.held("d"), ten_minutes);
        tracker.keep_for(&[]);
        assert_eq!(tracker.held("d"), Held::default());
        assert!(tracker.readings.is_empty());
        assert_eq!(judge(&cpu, online(None), &tracker, at), Verdict::Unknown);
    }

    #[test]
    fn readings_outlast_their_offsets() {
        let mut tracker = Tracker::default();
        let mut reference = before::Tracker::default();
        let cpu = rule(Condition::Cpu, Some(50.0), Some(1));
        tracker.keep_for(std::slice::from_ref(&cpu));
        let mut at = START;
        // Past the 49 days a `u32` of milliseconds counts.
        while at < START + i64::from(u32::MAX) + 2 * MINUTE_MS {
            let reading = metrics((at / SECOND_MS % 100) as f32, 40);
            tracker.observe("d", at, &reading);
            reference.observe("d", at, &reading);
            at += 15 * SECOND_MS;
        }
        assert!(tracker.readings["d"].base > START);
        let now = at - 15 * SECOND_MS;
        assert!(matches!(
            judge(&cpu, online(None), &tracker, now),
            Verdict::Known(breaches) if breaches.len() == 1
        ));
        assert_eq!(
            judge(&cpu, online(None), &tracker, now),
            before::judge(&cpu, true, &reference, now)
        );
    }

    /// A small deterministic generator, so failures repeat.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
            choices[self.below(choices.len() as u64) as usize]
        }
    }

    /// A load near `threshold`: often exactly at it, or just past it either
    /// way, otherwise anywhere.
    fn load(random: &mut Random, threshold: f32) -> f32 {
        match random.below(4) {
            0 => threshold,
            1 => threshold + random.pick(&[-0.01, 0.01, -2.0, 2.0]),
            _ => random.below(10_001) as f32 / 100.0,
        }
    }

    #[test]
    fn load_verdicts_match_keeping_an_hour_of_every_reading() {
        let mut random = Random(0x5eed);
        let mut judged = 0;
        let mut known = 0;
        for case in 0..400 {
            let rules = (0..1 + random.below(3))
                .map(|_| {
                    let condition = random.pick(&[Condition::Cpu, Condition::Memory]);
                    let minutes = if random.below(2) == 0 {
                        random.pick(&[1, 2, 10, 59, 60])
                    } else {
                        1 + random.below(60) as i64
                    };
                    let threshold = random.pick(&[25.0, 50.0, 80.0, 90.0]);
                    rule(condition, Some(threshold), Some(minutes))
                })
                .collect::<Vec<_>>();
            let mut tracker = Tracker::default();
            tracker.keep_for(&rules);
            let mut reference = before::Tracker::default();
            let devices = ["d".to_owned()];
            let threshold = rules[0].threshold.unwrap() as f32;
            let mut connected = true;
            let mut at = START + random.below(MINUTE_MS as u64) as i64;
            let end = at + (30 + random.below(150) as i64) * MINUTE_MS;
            while at < end {
                at += match random.below(200) {
                    // Readings spaced irregularly, as when a feed skips one.
                    0..=29 => 2 + random.below(19) as i64,
                    // Quiet for a while without disconnecting, up to well
                    // past the longest window.
                    30..=31 => 21 + random.below(100 * 60) as i64,
                    _ => 5,
                } * SECOND_MS;
                match random.below(400) {
                    // A disconnect the engine notices, then a reconnect.
                    0 => {
                        connected = false;
                        tracker.presence(&devices, &HashSet::new(), at);
                        reference.presence(&devices, &HashSet::new(), at);
                        continue;
                    }
                    1..=20 if !connected => {
                        connected = true;
                        tracker.presence(&devices, &connected_set(), at);
                        reference.presence(&devices, &connected_set(), at);
                    }
                    _ if !connected => continue,
                    _ => {}
                }
                // Memory of 1024 bytes holds percentages an `f32` keeps
                // exactly, so both trackers average the same values.
                let used = if random.below(4) == 0 {
                    (f64::from(threshold) * 1024.0 / 100.0) as u64
                } else {
                    random.below(1025)
                };
                let reading = SystemMetrics {
                    memory_used_bytes: used,
                    memory_total_bytes: if random.below(50) == 0 { 0 } else { 1024 },
                    ..metrics(load(&mut random, threshold), 0)
                };
                tracker.observe("d", at, &reading);
                reference.observe("d", at, &reading);
                if random.below(3) == 0 {
                    let now = at + random.below(40 * SECOND_MS as u64) as i64;
                    let device = DeviceState {
                        connected,
                        ..online(None)
                    };
                    for rule in &rules {
                        let verdict = judge(rule, device, &tracker, now);
                        assert_eq!(
                            verdict,
                            before::judge(rule, connected, &reference, now),
                            "case {case}, {rule:?} at {now}"
                        );
                        judged += 1;
                        known += usize::from(matches!(verdict, Verdict::Known(_)));
                    }
                }
                let held = tracker.held("d").readings;
                let longest = rules.iter().filter_map(|rule| rule.duration_minutes).max();
                assert!(
                    held <= room(longest.unwrap()),
                    "case {case}: {held} readings"
                );
            }
        }
        // The sequences exercise both outcomes, not only unknowns.
        assert!(known > judged / 4, "{known} of {judged}");
        assert!(known < judged, "{known} of {judged}");
    }

    #[test]
    fn memory_held_as_f32_moves_averages_by_a_few_millionths_of_a_point() {
        let mut random = Random(42);
        let mut tracker = Tracker::default();
        let mut reference = before::Tracker::default();
        let memory = rule(Condition::Memory, Some(50.0), Some(10));
        tracker.keep_for(std::slice::from_ref(&memory));
        let mut at = START;
        for _ in 0..12 * 60 {
            let total = (1 + random.below(1 << 20)) << random.below(24);
            let reading = SystemMetrics {
                memory_used_bytes: random.below(total + 1),
                memory_total_bytes: total,
                ..metrics(0.0, 0)
            };
            tracker.observe("d", at, &reading);
            reference.observe("d", at, &reading);
            let ours = tracker.sustained("d", 10, at, |sample| sample.memory_percent);
            let theirs = reference.sustained("d", 10, at, |sample| sample.memory_percent);
            assert_eq!(ours.is_some(), theirs.is_some());
            if let (Some(ours), Some(theirs)) = (ours, theirs) {
                assert!((ours - theirs).abs() < 4e-6, "{ours} and {theirs}");
            }
            at += 5 * SECOND_MS;
        }
    }

    fn connected_set() -> HashSet<String> {
        connected(&["d"])
    }

    /// The tracker as it was when it kept an hour of every device's readings
    /// whatever the rules, and how it judged loads: the reference the
    /// tracker's verdicts must match.
    mod before {
        use std::collections::{HashMap, HashSet, VecDeque};

        use meshrmm_protocol_types::SystemMetrics;

        use super::super::{Breach, COVERAGE_SLACK_MS, STALE_MS, Verdict, round2};
        use crate::{
            alerts::rules::{Condition, MAX_SUSTAINED_MINUTES, Rule, format_number, plural},
            time::MINUTE_MS,
        };

        #[derive(Debug, Clone, Copy)]
        pub struct Sample {
            at: i64,
            pub cpu_percent: f64,
            pub memory_percent: f64,
        }

        #[derive(Debug, Default)]
        pub struct Tracker {
            samples: HashMap<String, VecDeque<Sample>>,
        }

        impl Tracker {
            pub fn observe(&mut self, device_id: &str, at: i64, metrics: &SystemMetrics) {
                let memory_percent = if metrics.memory_total_bytes == 0 {
                    0.0
                } else {
                    metrics.memory_used_bytes as f64 / metrics.memory_total_bytes as f64 * 100.0
                };
                let samples = self.samples.entry(device_id.to_owned()).or_default();
                if samples.back().is_some_and(|last| last.at >= at) {
                    return;
                }
                samples.push_back(Sample {
                    at,
                    cpu_percent: f64::from(metrics.cpu_percent),
                    memory_percent,
                });
                let oldest = at - MAX_SUSTAINED_MINUTES * MINUTE_MS - COVERAGE_SLACK_MS;
                while samples.front().is_some_and(|sample| sample.at < oldest) {
                    samples.pop_front();
                }
            }

            pub fn presence(&mut self, devices: &[String], connected: &HashSet<String>, _now: i64) {
                for device_id in devices {
                    if !connected.contains(device_id) {
                        self.samples.remove(device_id);
                    }
                }
            }

            pub fn sustained(
                &self,
                device_id: &str,
                minutes: i64,
                now: i64,
                value: fn(&Sample) -> f64,
            ) -> Option<f64> {
                let samples = self.samples.get(device_id)?;
                let from = now - minutes * MINUTE_MS;
                if samples.front()?.at > from + COVERAGE_SLACK_MS
                    || samples.back()?.at < now - STALE_MS
                {
                    return None;
                }
                let window = samples
                    .iter()
                    .filter(|sample| sample.at > from)
                    .map(value)
                    .collect::<Vec<_>>();
                (!window.is_empty()).then(|| window.iter().sum::<f64>() / window.len() as f64)
            }
        }

        pub fn judge(rule: &Rule, connected: bool, tracker: &Tracker, now: i64) -> Verdict {
            let minutes = rule.duration_minutes.unwrap_or(1);
            let threshold = rule.threshold.unwrap_or_default();
            if !connected {
                return Verdict::Unknown;
            }
            let (what, value): (&str, fn(&Sample) -> f64) = if rule.condition == Condition::Cpu {
                ("CPU", |sample| sample.cpu_percent)
            } else {
                ("Memory", |sample| sample.memory_percent)
            };
            match tracker.sustained("d", minutes, now, value) {
                None => Verdict::Unknown,
                Some(average) if average > threshold => Verdict::Known(vec![Breach {
                    subject: String::new(),
                    value: round2(average),
                    detail: format!(
                        "{what} averaged {}% over {}",
                        format_number(average),
                        plural(minutes, "minute")
                    ),
                }]),
                Some(_) => Verdict::Known(Vec::new()),
            }
        }
    }

    #[test]
    fn disk_space_is_judged_per_volume() {
        let tracker = Tracker::default();
        let reading = metrics(0.0, 0);
        let percent = rule(Condition::DiskSpace, Some(10.0), None);
        let Verdict::Known(breaches) = judge(&percent, online(Some(&reading)), &tracker, START)
        else {
            panic!("an online device's volumes are known");
        };
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].subject, "C:");
        assert_eq!(breaches[0].value, 4.0);
        assert_eq!(breaches[0].detail, "C: has 4 GB free (4%)");
        let gigabytes = Rule {
            threshold: Some(60.0),
            threshold_unit: Some(ThresholdUnit::Gb),
            ..percent.clone()
        };
        assert!(matches!(
            judge(&gigabytes, online(Some(&reading)), &tracker, START),
            Verdict::Known(breaches) if breaches.len() == 2
        ));
        assert_eq!(
            judge(&percent, offline(), &tracker, START),
            Verdict::Unknown
        );
    }

    #[test]
    fn updates_and_restarts_raise_alerts_once_pending_long_enough() {
        let tracker = Tracker::default();
        let pending = [
            (UpdateCategory::Security, START - 8 * DAY_MS),
            (UpdateCategory::Security, START - 2 * DAY_MS),
            (UpdateCategory::Other, START - 30 * DAY_MS),
        ];
        let scanned = |reboot_pending_since| DeviceState {
            updates: Some(UpdateFacts {
                pending: &pending,
                reboot_pending_since,
            }),
            ..offline()
        };
        let security = Rule {
            update_category: Some("security".into()),
            days: Some(7),
            ..rule(Condition::UpdatesPending, None, None)
        };
        let Verdict::Known(breaches) = judge(&security, scanned(None), &tracker, START) else {
            panic!("a scanned device is judged, online or not");
        };
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].value, 1.0);
        assert!(
            breaches[0]
                .detail
                .starts_with("1 security update pending for more than 7 days, the oldest since "),
            "{}",
            breaches[0].detail
        );
        let any = Rule {
            update_category: Some("any".into()),
            days: Some(1),
            ..security.clone()
        };
        assert!(matches!(
            judge(&any, scanned(None), &tracker, START),
            Verdict::Known(breaches) if breaches[0].value == 3.0
        ));
        let drivers = Rule {
            update_category: Some("driver".into()),
            ..security.clone()
        };
        assert_eq!(
            judge(&drivers, scanned(None), &tracker, START),
            Verdict::Known(vec![])
        );
        assert_eq!(
            judge(&security, offline(), &tracker, START),
            Verdict::Unknown,
            "a device that never scanned says nothing"
        );

        let restart = Rule {
            days: Some(3),
            ..rule(Condition::RebootPending, None, None)
        };
        assert_eq!(
            judge(&restart, scanned(Some(START - 2 * DAY_MS)), &tracker, START),
            Verdict::Known(vec![])
        );
        assert!(matches!(
            judge(&restart, scanned(Some(START - 4 * DAY_MS)), &tracker, START),
            Verdict::Known(breaches) if breaches[0].value == 4.0
        ));
        assert_eq!(
            judge(&restart, scanned(None), &tracker, START),
            Verdict::Known(vec![])
        );
    }
}
