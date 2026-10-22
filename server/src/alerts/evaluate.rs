//! Judging device-state rules: what the engine remembers about each device
//! (its recent CPU and memory readings and since when it has been offline)
//! and whether a rule's condition holds for a device now.
use std::collections::{HashMap, HashSet, VecDeque};

use meshrmm_protocol_types::{SystemMetrics, UpdateCategory};

use super::rules::{Condition, MAX_SUSTAINED_MINUTES, Rule, ThresholdUnit, format_number, plural};
use crate::time::{DAY_MS, MINUTE_MS, SECOND_MS};

/// Readings this far apart still cover a window without a gap: two of the
/// Agent's reporting intervals.
const COVERAGE_SLACK_MS: i64 = 10 * SECOND_MS;
/// A device whose latest reading is older than this has stopped reporting,
/// so its loads are unknown.
const STALE_MS: i64 = 30 * SECOND_MS;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: i64,
    cpu_percent: f64,
    memory_percent: f64,
}

/// What the engine remembers between evaluations. It starts empty with the
/// server, so after a restart a device counts as offline from the first
/// evaluation, and sustained loads need a full window of new readings.
#[derive(Debug, Default)]
pub struct Tracker {
    /// Each connected device's readings over the longest window, oldest
    /// first. A disconnect clears them, so they never span a gap.
    samples: HashMap<String, VecDeque<Sample>>,
    offline_since: HashMap<String, i64>,
}

impl Tracker {
    /// Remembers a reading the device's Agent reported at `at`.
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

    /// Notes which of `devices` are connected at `now`. A device counts as
    /// offline from the first time it's seen disconnected.
    pub fn presence(&mut self, devices: &[String], connected: &HashSet<String>, now: i64) {
        for device_id in devices {
            if connected.contains(device_id) {
                self.offline_since.remove(device_id);
            } else {
                self.offline_since.entry(device_id.clone()).or_insert(now);
                self.samples.remove(device_id);
            }
        }
        let known = devices.iter().collect::<HashSet<_>>();
        self.offline_since.retain(|id, _| known.contains(id));
        self.samples.retain(|id, _| known.contains(id));
    }

    pub fn forget(&mut self, device_id: &str) {
        self.samples.remove(device_id);
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
        value: fn(&Sample) -> f64,
    ) -> Option<f64> {
        let samples = self.samples.get(device_id)?;
        let from = now - minutes * MINUTE_MS;
        if samples.front()?.at > from + COVERAGE_SLACK_MS || samples.back()?.at < now - STALE_MS {
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
            let (what, value): (&str, fn(&Sample) -> f64) = if rule.condition == Condition::Cpu {
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
        Condition::ScriptFailed | Condition::UpdateInstallFailed => Verdict::Unknown,
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
            all_companies: true,
            company_ids: vec![],
            channel_ids: vec![],
            notify_on_resolve: false,
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
