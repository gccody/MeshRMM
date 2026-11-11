//! What each template's sections hold, made from [`super::queries`].
use std::collections::{BTreeMap, HashMap, HashSet};

use meshrmm_protocol_types::SystemInfo;

use super::{
    Template,
    document::{
        Cell, Column, ColumnKind, Section, Table, Tone, format_bytes, format_count, format_date_ms,
        format_duration, format_percent, percent, plural,
    },
    period::Period,
    queries::{self, AlertCounts, AlertKey, AlertsIn, Device},
    scope::Scope,
};
use crate::{
    db::{self, Database},
    time::DAY_MS,
};

/// How recent a device's last update scan must be for it to count as
/// compliant.
pub const SCAN_FRESH_DAYS: i64 = 7;
/// Devices the executive summary lists by alerts.
const TOP_DEVICES: usize = 10;
/// Outdated packages the executive summary lists.
const TOP_OUTDATED: u64 = 10;
/// Alerts the alerts report lists one by one.
const LISTED_ALERTS: u64 = super::document::MAX_TABLE_ROWS as u64;

/// What a report is made of: its template on its devices for its period.
pub struct Request<'a> {
    pub template: Template,
    pub scope: &'a Scope,
    pub period: Period,
    pub stale_days: i64,
    pub now: i64,
    /// The devices whose Agent is connected.
    pub online: &'a HashSet<String>,
}

impl Request<'_> {
    fn range(&self) -> (i64, i64) {
        (self.period.start_ms, self.period.end_ms)
    }
}

/// The report's sections and what limits them.
pub struct Built {
    pub sections: Vec<Section>,
    pub coverage: Vec<String>,
}

/// Reads and assembles the report.
pub async fn build(database: &Database, request: &Request<'_>) -> db::Result<Built> {
    let mut coverage = Coverage::new(request);
    let sections = match request.template {
        Template::ExecutiveSummary => executive_summary(database, request, &mut coverage).await?,
        Template::PatchCompliance => patch_compliance(database, request, &mut coverage).await?,
        Template::HardwareInventory => hardware_inventory(database, request, &mut coverage).await?,
        Template::SoftwareInventory => software_inventory(database, request, &mut coverage).await?,
        Template::Alerts => alerts(database, request, &mut coverage).await?,
    };
    Ok(Built {
        sections,
        coverage: coverage.notes,
    })
}

/// Notes on what the server no longer holds for the period, or holds only
/// as it is now.
struct Coverage {
    start: i64,
    now: i64,
    zone: chrono_tz::Tz,
    notes: Vec<String>,
}

impl Coverage {
    fn new(request: &Request<'_>) -> Self {
        Self {
            start: request.period.start_ms,
            now: request.now,
            zone: request.period.zone,
            notes: Vec::new(),
        }
    }

    /// Notes that `what`, kept for `retention_ms`, is missing before then
    /// if the period starts earlier.
    fn retained(&mut self, what: &str, retention_ms: i64) {
        let kept_from = self.now - retention_ms;
        if self.start < kept_from {
            self.notes.push(format!(
                "{what} are kept for {} days, so those before {} are no longer counted.",
                retention_ms / DAY_MS,
                format_date_ms(kept_from, self.zone),
            ));
        }
    }

    fn current(&mut self, note: &str) {
        self.notes.push(note.to_owned());
    }
}

const CURRENT_STATE: &str = "Device counts, online state, pending updates and restarts, and outdated applications are as the devices are now, not as they were during the period. Deleted devices aren't counted.";

pub fn condition_label(condition: &str) -> &str {
    match condition {
        "device_offline" => "Device offline",
        "disk_space" => "Low disk space",
        "cpu" => "High CPU",
        "memory" => "High memory",
        "script_failed" => "Script failed",
        "updates_pending" => "Updates pending",
        "update_install_failed" => "Update install failed",
        "reboot_pending" => "Restart pending",
        "app_install_failed" => "Application install failed",
        "app_updates_pending" => "Application updates pending",
        "service" => "Service monitor",
        "process" => "Process monitor",
        "event_log" => "Event log monitor",
        "ping" => "Ping monitor",
        "tcp_port" => "TCP port monitor",
        "http" => "Website monitor",
        "certificate" => "Certificate monitor",
        "script_check" => "Script check",
        other => other,
    }
}

fn category_label(category: &str) -> &str {
    match category {
        "security" => "Security",
        "critical" => "Critical",
        "definition" => "Definitions",
        "driver" => "Drivers",
        "feature_pack" => "Feature packs",
        "service_pack" => "Service packs",
        "update_rollup" => "Update rollups",
        "upgrade" => "Upgrades",
        "tool" => "Tools",
        "other" => "Other",
        other => other,
    }
}

const SEVERITIES: [&str; 4] = ["critical", "important", "moderate", "low"];

fn severity_label(severity: Option<&str>) -> &str {
    match severity {
        Some("critical") => "Critical",
        Some("important") => "Important",
        Some("moderate") => "Moderate",
        Some("low") => "Low",
        Some(other) => other,
        None => "Unrated",
    }
}

fn source_label(source: &str) -> &str {
    match source {
        "winget" => "winget",
        "chocolatey" => "Chocolatey",
        "homebrew" => "Homebrew",
        "homebrew_cask" => "Homebrew cask",
        "flatpak" => "Flatpak",
        "snap" => "Snap",
        other => other,
    }
}

fn manager_label(manager: &str) -> &str {
    match manager {
        "windows_update" => "Windows Update",
        "softwareupdate" => "Software Update",
        "apt" => "APT",
        "dnf" => "DNF",
        "yum" => "YUM",
        "pacman" => "pacman",
        other => other,
    }
}

fn yes_no(value: bool) -> Cell {
    Cell::from(if value { "Yes" } else { "No" })
}

/// `count` and its share of `whole`, such as "12 (40%)".
fn with_share(count: i64, whole: i64) -> String {
    match percent(count, whole) {
        Some(share) => format!("{} ({})", format_count(count), format_percent(share)),
        None => format_count(count),
    }
}

fn mean(counts: &AlertCounts) -> Cell {
    counts.mean_resolve_ms().map_or(Cell::Missing, Cell::Int)
}

// ---------------------------------------------------------------------------
// Executive summary

async fn executive_summary(
    database: &Database,
    request: &Request<'_>,
    coverage: &mut Coverage,
) -> db::Result<Vec<Section>> {
    let scope = request.scope;
    let range = request.range();
    let devices = queries::devices(database, scope, request.online).await?;
    let total = devices.len() as i64;
    coverage.current(CURRENT_STATE);
    coverage.retained("Resolved alerts", crate::alerts::RESOLVED_RETENTION_MS);
    coverage.retained("Jobs", crate::maintenance::TOOLBOX_HISTORY_MS);
    coverage.retained("Update installs", crate::updates::INSTALL_HISTORY_MS);
    coverage.retained("Application runs", crate::applications::HISTORY_MS);

    // Devices.
    let mut platforms: BTreeMap<&str, i64> = BTreeMap::new();
    for device in &devices {
        *platforms.entry(device.platform_label()).or_default() += 1;
    }
    let enrolled = devices
        .iter()
        .filter(|device| (range.0..range.1).contains(&device.created_at))
        .count() as i64;
    let systems = queries::operating_systems(database, scope).await?;
    let inventoried: i64 = systems.iter().map(|(_, _, devices)| devices).sum();
    let mut os_rows: Vec<Vec<Cell>> = systems
        .iter()
        .map(|(name, version, count)| {
            vec![
                Cell::from(name.as_str()),
                Cell::from(version.as_str()),
                Cell::Int(*count),
            ]
        })
        .collect();
    if total > inventoried {
        os_rows.push(vec![
            Cell::from("No inventory yet"),
            Cell::Missing,
            Cell::Int(total - inventoried),
        ]);
    }
    let mut section = Section::new("devices", "Devices")
        .figure("Devices", format_count(total), Tone::Neutral)
        .figure_with(
            "Enrolled in the period",
            format_count(enrolled),
            "new devices",
            Tone::Neutral,
        );
    for (platform, count) in &platforms {
        section = section.figure(platform, format_count(*count), Tone::Neutral);
    }
    let devices_section = section
        .table(
            Table::new(
                "devices-by-platform",
                "By platform",
                vec![Column::text("Platform"), Column::count("Devices")],
                "No devices",
            )
            .with_rows(
                platforms
                    .iter()
                    .map(|(platform, count)| vec![Cell::from(*platform), Cell::Int(*count)]),
            ),
        )
        .table(
            Table::new(
                "devices-by-os",
                "By operating system",
                vec![
                    Column::text("Operating system"),
                    Column::text("Version"),
                    Column::count("Devices"),
                ],
                "No devices",
            )
            .with_rows(os_rows),
        );

    // Online state.
    let stale_cutoff = request.now - request.stale_days * DAY_MS;
    let online = devices.iter().filter(|device| device.online).count() as i64;
    let mut stale: Vec<&Device> = devices
        .iter()
        .filter(|device| device.last_seen(request.now) < stale_cutoff)
        .collect();
    stale.sort_by_key(|device| device.last_seen(request.now));
    let online_section = Section::new("online", "Online state")
        .figure("Online now", with_share(online, total), Tone::Neutral)
        .figure("Offline", with_share(total - online, total), Tone::Neutral)
        .figure_with(
            &format!("Not seen in {}", plural(request.stale_days, "day")),
            format_count(stale.len() as i64),
            "devices",
            Tone::warn_if(stale.len() as i64),
        )
        .note(LAST_SEEN_NOTE)
        .table(
            Table::new(
                "not-seen",
                &format!("Not seen in {}", plural(request.stale_days, "day")),
                vec![
                    Column::text("Device"),
                    Column::text("Company"),
                    Column::text("Platform"),
                    Column::new("Last seen", ColumnKind::Time),
                    Column::new("Not seen for", ColumnKind::Duration),
                ],
                "Every device was seen recently.",
            )
            .with_rows(stale.iter().map(|device| {
                let seen = device.last_seen(request.now);
                vec![
                    Cell::from(device.name.as_str()),
                    Cell::from(device.company.clone()),
                    Cell::from(device.platform_label()),
                    Cell::Int(seen),
                    Cell::Int(request.now - seen),
                ]
            })),
        );

    // Alerts.
    let raised =
        queries::alert_groups(database, scope, range, AlertsIn::Raised, AlertKey::Kind).await?;
    let resolved =
        queries::alert_groups(database, scope, range, AlertsIn::Resolved, AlertKey::Kind).await?;
    let open_now = queries::open_alerts(database, scope).await?;
    let mut kinds: BTreeMap<String, (AlertCounts, AlertCounts)> = BTreeMap::new();
    for (kind, counts) in &raised {
        kinds.entry(kind.clone()).or_default().0 = *counts;
    }
    for (kind, counts) in &resolved {
        kinds.entry(kind.clone()).or_default().1 = *counts;
    }
    let (mut raised_total, mut resolved_total) = (AlertCounts::default(), AlertCounts::default());
    for (raised, resolved) in kinds.values() {
        raised_total.add(raised);
        resolved_total.add(resolved);
    }
    let mut kind_rows: Vec<(&String, &(AlertCounts, AlertCounts))> = kinds.iter().collect();
    kind_rows.sort_by(|left, right| {
        right
            .1
            .0
            .alerts
            .cmp(&left.1.0.alerts)
            .then(left.0.cmp(right.0))
    });
    let alerts_section = Section::new("alerts", "Alerts")
        .figure("Raised", format_count(raised_total.alerts), Tone::warn_if(raised_total.alerts))
        .figure("Resolved", format_count(resolved_total.alerts), Tone::Neutral)
        .figure("Open now", format_count(open_now), Tone::warn_if(open_now))
        .figure(
            "Mean time to resolve",
            resolved_total
                .mean_resolve_ms()
                .map_or_else(|| "—".to_owned(), format_duration),
            Tone::Neutral,
        )
        .note("Raised counts the alerts raised in the period; resolved, and the time to resolve them, the alerts resolved in it, whenever they were raised.")
        .table(
            Table::new(
                "alerts-by-kind",
                "By kind",
                vec![
                    Column::text("Kind"),
                    Column::count("Raised"),
                    Column::count("Resolved"),
                    Column::new("Mean time to resolve", ColumnKind::Duration),
                ],
                "No alerts were raised or resolved in the period.",
            )
            .with_rows(kind_rows.into_iter().map(|(kind, (raised, resolved))| {
                vec![
                    Cell::from(condition_label(kind)),
                    Cell::Int(raised.alerts),
                    Cell::Int(resolved.alerts),
                    mean(resolved),
                ]
            })),
        );

    // Patch posture.
    let statuses = queries::update_statuses(database, scope).await?;
    let failed = queries::failed_installs(database, scope, range).await?;
    let posture = PatchPosture::of(&devices, &statuses, request.now);
    let failed_runs: i64 = failed.values().map(|(runs, _)| runs).sum();
    let patch_section = Section::new("patches", "Patch posture")
        .figure_with(
            "Fully patched",
            with_share(posture.fully_patched, posture.scanned),
            "of the devices that scanned",
            Tone::Good,
        )
        .figure(
            "Security updates pending",
            format_count(posture.security),
            Tone::bad_if(posture.security),
        )
        .figure(
            "Failed installs",
            format_count(failed_runs),
            Tone::bad_if(failed_runs),
        )
        .figure(
            "Restarts pending",
            format_count(posture.reboot),
            Tone::warn_if(posture.reboot),
        )
        .note("Failed installs are the installs started in the period that failed on some or all of their updates, on any number of devices.")
        .table(
            Table::new(
                "patch-posture",
                "Devices by update state",
                vec![Column::text("State"), Column::count("Devices")],
                "No devices",
            )
            .with_rows([
                vec![Cell::from("Fully patched"), Cell::Int(posture.fully_patched)],
                vec![
                    Cell::from("Security or critical updates pending"),
                    Cell::Int(posture.security),
                ],
                vec![
                    Cell::from("Only other updates pending"),
                    Cell::Int(posture.other_only),
                ],
                vec![Cell::from("Restart pending"), Cell::Int(posture.reboot)],
                vec![
                    Cell::from("An install failed in the period"),
                    Cell::Int(failed.len() as i64),
                ],
                vec![Cell::from("Not scanned yet"), Cell::Int(posture.never_scanned)],
                vec![
                    Cell::from("No update manager MeshRMM supports"),
                    Cell::Int(posture.unsupported),
                ],
            ]),
        );

    // Application posture.
    let (outdated_installs, outdated_devices) = queries::outdated_totals(database, scope).await?;
    let failed_apps = queries::failed_application_runs(database, scope, range).await?;
    let top_outdated = queries::outdated_packages(database, scope, Some(TOP_OUTDATED)).await?;
    let failed_of = |action: &str| failed_apps.get(action).copied().unwrap_or_default();
    let (install_runs, install_devices) = failed_of("install");
    let (update_runs, update_devices) = failed_of("update");
    let applications_section = Section::new("applications", "Application posture")
        .figure_with(
            "Devices with outdated applications",
            with_share(outdated_devices, total),
            format!("{} in all", plural(outdated_installs, "outdated package")),
            Tone::warn_if(outdated_devices),
        )
        .figure_with(
            "Failed installs",
            format_count(install_runs),
            format!("on {}", plural(install_devices, "device")),
            Tone::bad_if(install_runs),
        )
        .figure_with(
            "Failed updates",
            format_count(update_runs),
            format!("on {}", plural(update_devices, "device")),
            Tone::bad_if(update_runs),
        )
        .table(outdated_table(
            "outdated-top",
            "Most outdated packages",
            &top_outdated,
        ));

    // Jobs and policies.
    let jobs = queries::jobs(database, scope, range).await?;
    let mut by_users = queries::JobCounts::default();
    let mut by_policies = queries::JobCounts::default();
    let mut policy_rows = Vec::new();
    for counts in &jobs {
        let total = if counts.policy_id.is_some() {
            policy_rows.push(counts);
            &mut by_policies
        } else {
            &mut by_users
        };
        total.jobs += counts.jobs;
        total.failed_jobs += counts.failed_jobs;
        total.succeeded += counts.succeeded;
        total.failed += counts.failed;
        total.not_run += counts.not_run;
    }
    policy_rows.sort_by(|left, right| {
        right
            .jobs
            .cmp(&left.jobs)
            .then_with(|| left.policy_name.cmp(&right.policy_name))
    });
    let job_row = |label: Cell, counts: &queries::JobCounts| {
        vec![
            label,
            Cell::Int(counts.jobs),
            Cell::Int(counts.failed_jobs),
            Cell::Int(counts.succeeded),
            Cell::Int(counts.failed),
            Cell::Int(counts.not_run),
        ]
    };
    let job_columns = |first: &str| {
        vec![
            Column::text(first),
            Column::count("Jobs"),
            Column::count("Jobs with failures"),
            Column::count("Devices succeeded"),
            Column::count("Devices failed"),
            Column::count("Devices not run"),
        ]
    };
    let jobs_section = Section::new("jobs", "Jobs and policies")
        .figure_with(
            "Jobs started by people",
            format_count(by_users.jobs),
            format!("{} with failures", format_count(by_users.failed_jobs)),
            Tone::Neutral,
        )
        .figure_with(
            "Policy runs",
            format_count(by_policies.jobs),
            format!("{} with failures", format_count(by_policies.failed_jobs)),
            Tone::Neutral,
        )
        .figure(
            "Device runs succeeded",
            format_count(by_users.succeeded + by_policies.succeeded),
            Tone::Good,
        )
        .figure(
            "Device runs failed",
            format_count(by_users.failed + by_policies.failed),
            Tone::bad_if(by_users.failed + by_policies.failed),
        )
        .note("Counts the jobs started in the period on the report's devices: scripts, files, update scans and installs, application installs and updates, and wakes. Failed includes runs that timed out or never reported.")
        .table(
            Table::new("jobs", "By who started them", job_columns("Started by"), "No jobs")
                .with_rows([
                    job_row(Cell::from("People"), &by_users),
                    job_row(Cell::from("Policies"), &by_policies),
                ]),
        )
        .table(
            Table::new(
                "policy-runs",
                "Policy runs",
                job_columns("Policy"),
                "No policy ran in the period.",
            )
            .with_rows(policy_rows.into_iter().map(|counts| {
                job_row(
                    Cell::from(counts.policy_name.clone().unwrap_or_default()),
                    counts,
                )
            })),
        );

    // Top devices by alerts.
    let by_device =
        queries::alert_groups(database, scope, range, AlertsIn::Raised, AlertKey::Device).await?;
    let names = by_id(&devices);
    let mut top: Vec<(&Device, AlertCounts)> = by_device
        .iter()
        .filter_map(|(id, counts)| names.get(id.as_str()).map(|device| (*device, *counts)))
        .collect();
    top.sort_by(|left, right| {
        right
            .1
            .alerts
            .cmp(&left.1.alerts)
            .then_with(|| left.0.name.cmp(&right.0.name))
    });
    top.truncate(TOP_DEVICES);
    let top_section = Section::new("top-devices", "Top devices by alerts").table(
        Table::new(
            "top-devices",
            &format!("The {TOP_DEVICES} devices with the most alerts raised in the period"),
            vec![
                Column::text("Device"),
                Column::text("Company"),
                Column::count("Alerts"),
                Column::count("Still open"),
                Column::new("Mean time to resolve", ColumnKind::Duration),
            ],
            "No alerts were raised in the period.",
        )
        .with_rows(top.iter().map(|(device, counts)| {
            vec![
                Cell::from(device.name.as_str()),
                Cell::from(device.company.clone()),
                Cell::Int(counts.alerts),
                Cell::Int(counts.open),
                mean(counts),
            ]
        })),
    );

    Ok(vec![
        devices_section,
        online_section,
        alerts_section,
        patch_section,
        applications_section,
        jobs_section,
        top_section,
    ])
}

const LAST_SEEN_NOTE: &str = "Last seen is when the server last heard from a device: now while its Agent is connected, otherwise its last resource reading (kept 7 days), its last hardware snapshot (sent every 6 hours) or its enrollment.";

fn by_id(devices: &[Device]) -> HashMap<&str, &Device> {
    devices
        .iter()
        .map(|device| (device.id.as_str(), device))
        .collect()
}

/// How the devices stand on operating system updates.
#[derive(Debug, Default)]
struct PatchPosture {
    /// Devices with a scan that worked.
    scanned: i64,
    fully_patched: i64,
    /// With security or critical updates pending.
    security: i64,
    other_only: i64,
    reboot: i64,
    never_scanned: i64,
    unsupported: i64,
    /// Compliant: scanned in the last [`SCAN_FRESH_DAYS`] with no security
    /// or critical update pending.
    compliant: i64,
    stale_scans: i64,
}

impl PatchPosture {
    fn of(devices: &[Device], statuses: &HashMap<String, queries::UpdateStatus>, now: i64) -> Self {
        let mut posture = Self::default();
        for device in devices {
            match compliance(statuses.get(&device.id), now) {
                Compliance::Unsupported => posture.unsupported += 1,
                Compliance::NotScanned => posture.never_scanned += 1,
                _ => {}
            }
            let Some(status) = statuses.get(&device.id) else {
                continue;
            };
            if status.reboot_pending {
                posture.reboot += 1;
            }
            if !status.supported || status.scanned_at.is_none() {
                continue;
            }
            posture.scanned += 1;
            if status.pending_count == 0 {
                posture.fully_patched += 1;
            } else if status.security_count + status.critical_count > 0 {
                posture.security += 1;
            } else {
                posture.other_only += 1;
            }
            match compliance(Some(status), now) {
                Compliance::Compliant => posture.compliant += 1,
                Compliance::StaleScan => posture.stale_scans += 1,
                _ => {}
            }
        }
        posture
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compliance {
    Compliant,
    UpdatesPending,
    StaleScan,
    NotScanned,
    Unsupported,
}

impl Compliance {
    fn label(self) -> &'static str {
        match self {
            Self::Compliant => "Compliant",
            Self::UpdatesPending => "Updates pending",
            Self::StaleScan => "Scan out of date",
            Self::NotScanned => "Not scanned",
            Self::Unsupported => "Unsupported",
        }
    }
}

fn compliance(status: Option<&queries::UpdateStatus>, now: i64) -> Compliance {
    let Some(status) = status else {
        return Compliance::NotScanned;
    };
    if !status.supported {
        return Compliance::Unsupported;
    }
    let Some(scanned_at) = status.scanned_at else {
        return Compliance::NotScanned;
    };
    if status.security_count + status.critical_count > 0 {
        Compliance::UpdatesPending
    } else if scanned_at < now - SCAN_FRESH_DAYS * DAY_MS {
        Compliance::StaleScan
    } else {
        Compliance::Compliant
    }
}

fn outdated_table(id: &str, title: &str, packages: &[queries::Outdated]) -> Table {
    Table::new(
        id,
        title,
        vec![
            Column::text("Package"),
            Column::text("ID"),
            Column::text("Manager"),
            Column::text("Newest version"),
            Column::count("Devices"),
        ],
        "No package is outdated.",
    )
    .with_rows(packages.iter().map(|package| {
        vec![
            Cell::from(package.name.as_str()),
            Cell::from(package.package_id.as_str()),
            Cell::from(source_label(&package.source)),
            Cell::from(package.available_version.clone()),
            Cell::Int(package.devices),
        ]
    }))
}

// ---------------------------------------------------------------------------
// Patch compliance

async fn patch_compliance(
    database: &Database,
    request: &Request<'_>,
    coverage: &mut Coverage,
) -> db::Result<Vec<Section>> {
    let scope = request.scope;
    let range = request.range();
    coverage.current("Pending updates and restarts are as each device's last scan found them, not as they were during the period; failed installs are those started in the period. Deleted devices aren't counted.");
    coverage.retained("Update installs", crate::updates::INSTALL_HISTORY_MS);
    let devices = queries::devices(database, scope, request.online).await?;
    let statuses = queries::update_statuses(database, scope).await?;
    let failed = queries::failed_installs(database, scope, range).await?;
    let pending = queries::pending_updates(database, scope).await?;
    let posture = PatchPosture::of(&devices, &statuses, request.now);

    // Pending updates per device, and across the devices.
    #[derive(Default)]
    struct Pending {
        categories: HashMap<String, i64>,
        severities: HashMap<Option<String>, i64>,
    }
    let mut per_device: HashMap<&str, Pending> = HashMap::new();
    let mut categories: BTreeMap<String, (i64, HashSet<&str>)> = BTreeMap::new();
    let mut severities: BTreeMap<Option<String>, (i64, HashSet<&str>)> = BTreeMap::new();
    for (device, category, severity, count) in &pending {
        let entry = per_device.entry(device.as_str()).or_default();
        *entry.categories.entry(category.clone()).or_default() += count;
        *entry.severities.entry(severity.clone()).or_default() += count;
        let fleet = categories.entry(category.clone()).or_default();
        fleet.0 += count;
        fleet.1.insert(device.as_str());
        let fleet = severities.entry(severity.clone()).or_default();
        fleet.0 += count;
        fleet.1.insert(device.as_str());
    }
    let assessable = devices.len() as i64 - posture.unsupported;
    let not_compliant = assessable - posture.compliant;
    let failed_runs: i64 = failed.values().map(|(runs, _)| runs).sum();
    let failed_updates: i64 = failed.values().map(|(_, updates)| updates).sum();
    let summary = Section::new("compliance", "Compliance")
        .figure_with(
            "Compliance",
            percent(posture.compliant, assessable).map_or_else(|| "—".to_owned(), format_percent),
            format!(
                "{} of {} compliant",
                format_count(posture.compliant),
                plural(assessable, "device")
            ),
            if not_compliant > 0 { Tone::Warning } else { Tone::Good },
        )
        .figure(
            "Security or critical updates pending",
            format_count(posture.security),
            Tone::bad_if(posture.security),
        )
        .figure(
            &format!("Not scanned in {}", plural(SCAN_FRESH_DAYS, "day")),
            format_count(posture.stale_scans + posture.never_scanned),
            Tone::warn_if(posture.stale_scans + posture.never_scanned),
        )
        .figure(
            "Restarts pending",
            format_count(posture.reboot),
            Tone::warn_if(posture.reboot),
        )
        .figure_with(
            "Failed installs",
            format_count(failed_runs),
            format!(
                "{} not installed, on {}",
                plural(failed_updates, "update"),
                plural(failed.len() as i64, "device")
            ),
            Tone::bad_if(failed_runs),
        )
        .note(format!(
            "A device is compliant when its last update scan worked, was in the last {} and found no security or critical updates pending. Devices without an update manager MeshRMM supports ({}) aren't counted; devices that never scanned count as not compliant.",
            plural(SCAN_FRESH_DAYS, "day"),
            format_count(posture.unsupported)
        ))
        .table(
            Table::new(
                "compliance",
                "Devices by compliance",
                vec![Column::text("State"), Column::count("Devices")],
                "No devices",
            )
            .with_rows(
                [
                    (Compliance::Compliant, posture.compliant),
                    (Compliance::UpdatesPending, posture.security),
                    (Compliance::StaleScan, posture.stale_scans),
                    (Compliance::NotScanned, posture.never_scanned),
                    (Compliance::Unsupported, posture.unsupported),
                ]
                .map(|(state, count)| vec![Cell::from(state.label()), Cell::Int(count)]),
            ),
        );

    let mut category_rows: Vec<_> = categories.iter().collect();
    category_rows.sort_by(|left, right| right.1.0.cmp(&left.1.0).then(left.0.cmp(right.0)));
    let pending_section = Section::new("pending", "Pending updates")
        .table(
            Table::new(
                "pending-by-category",
                "By category",
                vec![
                    Column::text("Category"),
                    Column::count("Updates"),
                    Column::count("Devices"),
                ],
                "No updates are pending.",
            )
            .with_rows(category_rows.into_iter().map(|(category, (count, devices))| {
                vec![
                    Cell::from(category_label(category)),
                    Cell::Int(*count),
                    Cell::Int(devices.len() as i64),
                ]
            })),
        )
        .table(
            Table::new(
                "pending-by-severity",
                "By severity",
                vec![
                    Column::text("Severity"),
                    Column::count("Updates"),
                    Column::count("Devices"),
                ],
                "No updates are pending.",
            )
            .with_rows(
                SEVERITIES
                    .iter()
                    .map(|severity| Some((*severity).to_owned()))
                    .chain([None])
                    .filter_map(|severity| {
                        severities.get(&severity).map(|(count, devices)| {
                            vec![
                                Cell::from(severity_label(severity.as_deref())),
                                Cell::Int(*count),
                                Cell::Int(devices.len() as i64),
                            ]
                        })
                    }),
            ),
        )
        .note("Windows Update rates updates by severity; other managers mostly don't, so their updates are unrated.");

    let empty = Pending::default();
    let device_rows = devices.iter().map(|device| {
        let status = statuses.get(&device.id);
        let pending = per_device.get(device.id.as_str()).unwrap_or(&empty);
        let category = |name: &str| pending.categories.get(name).copied().unwrap_or(0);
        let severity = |name: &str| {
            Cell::Int(
                pending
                    .severities
                    .get(&Some(name.to_owned()))
                    .copied()
                    .unwrap_or(0),
            )
        };
        let total: i64 = pending.categories.values().sum();
        let (failed_runs, _) = failed.get(&device.id).copied().unwrap_or_default();
        vec![
            Cell::from(device.name.as_str()),
            Cell::from(device.company.clone()),
            Cell::from(compliance(status, request.now).label()),
            status.and_then(|status| status.scanned_at).into(),
            Cell::from(
                status
                    .and_then(|status| status.manager.as_deref())
                    .map(manager_label),
            ),
            Cell::Int(total),
            Cell::Int(category("security")),
            Cell::Int(category("critical")),
            Cell::Int(total - category("security") - category("critical")),
            severity("critical"),
            severity("important"),
            severity("moderate"),
            severity("low"),
            Cell::Int(failed_runs),
            match status {
                Some(status) if status.reboot_pending => match status.reboot_pending_since {
                    Some(since) => Cell::from(format!(
                        "Since {}",
                        format_date_ms(since, request.period.zone)
                    )),
                    None => Cell::from("Yes"),
                },
                Some(_) => Cell::from("No"),
                None => Cell::Missing,
            },
        ]
    });
    let devices_section = Section::new("devices", "Devices").table(
        Table::new(
            "devices",
            "Each device",
            vec![
                Column::text("Device"),
                Column::text("Company"),
                Column::text("Compliance"),
                Column::new("Last scan", ColumnKind::Time),
                Column::text("Manager"),
                Column::count("Pending"),
                Column::count("Security"),
                Column::count("Critical"),
                Column::count("Other"),
                Column::count("Critical severity"),
                Column::count("Important"),
                Column::count("Moderate"),
                Column::count("Low"),
                Column::count("Failed installs"),
                Column::text("Restart pending"),
            ],
            "No devices",
        )
        .with_rows(device_rows),
    );
    Ok(vec![summary, pending_section, devices_section])
}

// ---------------------------------------------------------------------------
// Hardware asset inventory

async fn hardware_inventory(
    database: &Database,
    request: &Request<'_>,
    coverage: &mut Coverage,
) -> db::Result<Vec<Section>> {
    let scope = request.scope;
    coverage.current("Hardware is as each device's latest snapshot describes it; free space is from its last resource reading, which is kept 7 days. Deleted devices aren't listed.");
    let devices = queries::devices(database, scope, request.online).await?;
    let snapshots = queries::snapshots(database, scope).await?;
    let models = queries::models(database, scope).await?;
    let systems = queries::operating_systems(database, scope).await?;
    let mut memory_total = 0i64;
    let mut disk_total = 0i64;
    let mut rows = Vec::with_capacity(devices.len());
    for device in &devices {
        let snapshot = snapshots.get(&device.id);
        let info = snapshot.and_then(|(_, _, _, _, _, _, _, details)| {
            serde_json::from_str::<SystemInfo>(details).ok()
        });
        let hardware = info.as_ref().map(|info| &info.hardware);
        let memory = hardware
            .map(|hardware| hardware.memory_bytes as i64)
            .filter(|bytes| *bytes > 0);
        memory_total += memory.unwrap_or(0);
        let disks = hardware.map(|hardware| {
            hardware
                .disks
                .iter()
                .map(|disk| {
                    disk_total += disk.size_bytes as i64;
                    let name = disk.model.as_deref().unwrap_or(&disk.name);
                    format!("{name} ({})", format_bytes(disk.size_bytes as i64))
                })
                .collect::<Vec<_>>()
                .join("; ")
        });
        let processor = hardware.and_then(|hardware| hardware.processor.as_ref());
        let free = match (device.storage_total, device.storage_used) {
            (Some(total), Some(used)) if total > 0 => Some((total - used).max(0)),
            _ => None,
        };
        let os = snapshot.map(|(_, name, version, build, ..)| match build {
            Some(build) if !build.is_empty() => format!("{name} {version} ({build})"),
            _ => format!("{name} {version}"),
        });
        rows.push(vec![
            Cell::from(device.name.as_str()),
            Cell::from(device.company.clone()),
            Cell::from(device.platform_label()),
            Cell::from(snapshot.and_then(|row| row.4.clone())),
            Cell::from(snapshot.and_then(|row| row.5.clone())),
            Cell::from(snapshot.and_then(|row| row.6.clone())),
            Cell::from(processor.map(|processor| processor.name.clone())),
            Cell::from(
                processor.map(|processor| format!("{} / {}", processor.cores, processor.threads)),
            ),
            memory.into(),
            Cell::from(disks.filter(|disks| !disks.is_empty())),
            free.into(),
            device.storage_total.filter(|total| *total > 0).into(),
            Cell::from(os.map(|os| os.trim().to_owned())),
            Cell::from(device.agent_version.clone()),
            Cell::Int(device.last_seen(request.now)),
            yes_no(device.online),
        ]);
    }
    let summary = Section::new("summary", "Summary")
        .figure("Devices", format_count(devices.len() as i64), Tone::Neutral)
        .figure_with(
            "With an inventory",
            format_count(snapshots.len() as i64),
            "have sent a hardware snapshot",
            Tone::Neutral,
        )
        .figure("Memory in all", format_bytes(memory_total), Tone::Neutral)
        .figure(
            "Disk capacity in all",
            format_bytes(disk_total),
            Tone::Neutral,
        )
        .table(
            Table::new(
                "models",
                "By model",
                vec![
                    Column::text("Manufacturer"),
                    Column::text("Model"),
                    Column::count("Devices"),
                ],
                "No device has sent a hardware snapshot yet.",
            )
            .with_rows(models.iter().map(|(manufacturer, model, count)| {
                vec![
                    Cell::from(manufacturer.clone()),
                    Cell::from(model.clone()),
                    Cell::Int(*count),
                ]
            })),
        )
        .table(
            Table::new(
                "operating-systems",
                "By operating system",
                vec![
                    Column::text("Operating system"),
                    Column::text("Version"),
                    Column::count("Devices"),
                ],
                "No device has sent a hardware snapshot yet.",
            )
            .with_rows(systems.iter().map(|(name, version, count)| {
                vec![
                    Cell::from(name.as_str()),
                    Cell::from(version.as_str()),
                    Cell::Int(*count),
                ]
            })),
        );
    let devices_section = Section::new("devices", "Devices")
        .note(LAST_SEEN_NOTE)
        .table(
            Table::new(
                "devices",
                "Each device",
                vec![
                    Column::text("Device"),
                    Column::text("Company"),
                    Column::text("Platform"),
                    Column::text("Manufacturer"),
                    Column::text("Model"),
                    Column::text("Serial number"),
                    Column::text("Processor"),
                    Column::text("Cores / threads"),
                    Column::new("Memory", ColumnKind::Bytes),
                    Column::text("Disks"),
                    Column::new("Free space", ColumnKind::Bytes),
                    Column::new("Storage", ColumnKind::Bytes),
                    Column::text("Operating system"),
                    Column::text("Agent version"),
                    Column::new("Last seen", ColumnKind::Time),
                    Column::text("Online"),
                ],
                "No devices",
            )
            .with_rows(rows),
        );
    Ok(vec![summary, devices_section])
}

// ---------------------------------------------------------------------------
// Software inventory

async fn software_inventory(
    database: &Database,
    request: &Request<'_>,
    coverage: &mut Coverage,
) -> db::Result<Vec<Section>> {
    let scope = request.scope;
    coverage.current("Applications are as each device's latest snapshot lists them, and outdated packages as its latest application scan found them. Deleted devices aren't counted.");
    let applications = queries::applications(database, scope).await?;
    let versions = queries::application_versions(database, scope).await?;
    let inventoried = queries::devices_with_applications(database, scope).await?;
    let outdated = queries::outdated_packages(database, scope, None).await?;
    let (outdated_installs, outdated_devices) = queries::outdated_totals(database, scope).await?;
    let summary = Section::new("summary", "Summary")
        .figure(
            "Applications",
            format_count(applications.len() as i64),
            Tone::Neutral,
        )
        .figure(
            "Versions",
            format_count(versions.len() as i64),
            Tone::Neutral,
        )
        .figure_with(
            "Devices with an inventory",
            format_count(inventoried),
            "list their applications",
            Tone::Neutral,
        )
        .figure_with(
            "Outdated packages",
            format_count(outdated.len() as i64),
            format!(
                "{} on {}",
                plural(outdated_installs, "install"),
                plural(outdated_devices, "device")
            ),
            Tone::warn_if(outdated_installs),
        );
    let applications_section = Section::new("applications", "Applications").table(
        Table::new(
            "applications",
            "Applications by how many devices have them",
            vec![
                Column::text("Application"),
                Column::count("Versions"),
                Column::count("Devices"),
            ],
            "No device has listed its applications yet.",
        )
        .with_rows(applications.iter().map(|software| {
            vec![
                Cell::from(software.name.as_str()),
                Cell::Int(software.versions),
                Cell::Int(software.devices),
            ]
        })),
    );
    let versions_section = Section::new("versions", "Versions").table(
        Table::new(
            "versions",
            "Each version, by application",
            vec![
                Column::text("Application"),
                Column::text("Version"),
                Column::text("Publisher"),
                Column::count("Devices"),
            ],
            "No device has listed its applications yet.",
        )
        .with_rows(versions.iter().map(|(name, version, publisher, devices)| {
            let text = |value: &String| {
                if value.is_empty() {
                    Cell::Missing
                } else {
                    Cell::from(value.as_str())
                }
            };
            vec![
                Cell::from(name.as_str()),
                text(version),
                text(publisher),
                Cell::Int(*devices),
            ]
        })),
    );
    let outdated_section = Section::new("outdated", "Outdated packages")
        .note("Packages the devices' package managers (winget, Chocolatey, Homebrew, Flatpak and Snap) offer a newer version of.")
        .table(outdated_table(
            "outdated",
            "Outdated packages by how many devices have them",
            &outdated,
        ));
    Ok(vec![
        summary,
        applications_section,
        versions_section,
        outdated_section,
    ])
}

// ---------------------------------------------------------------------------
// Alerts

async fn alerts(
    database: &Database,
    request: &Request<'_>,
    coverage: &mut Coverage,
) -> db::Result<Vec<Section>> {
    let scope = request.scope;
    let range = request.range();
    coverage.current("Alerts of deleted rules and devices aren't counted.");
    coverage.retained("Resolved alerts", crate::alerts::RESOLVED_RETENTION_MS);
    let devices = queries::devices(database, scope, request.online).await?;
    let names = by_id(&devices);
    let rules = queries::rules(database).await?;
    let by_kind =
        queries::alert_groups(database, scope, range, AlertsIn::Raised, AlertKey::Kind).await?;
    let by_rule =
        queries::alert_groups(database, scope, range, AlertsIn::Raised, AlertKey::Rule).await?;
    let by_device =
        queries::alert_groups(database, scope, range, AlertsIn::Raised, AlertKey::Device).await?;
    let resolved_in =
        queries::alert_groups(database, scope, range, AlertsIn::Resolved, AlertKey::Kind).await?;
    let listed = queries::alerts(database, scope, range, LISTED_ALERTS + 1).await?;
    let mut total = AlertCounts::default();
    for (_, counts) in &by_kind {
        total.add(counts);
    }
    let mut resolved_total = AlertCounts::default();
    for (_, counts) in &resolved_in {
        resolved_total.add(counts);
    }
    let mut critical = 0;
    for (rule_id, counts) in &by_rule {
        if rules
            .get(rule_id)
            .is_some_and(|rule| rule.severity == "critical")
        {
            critical += counts.alerts;
        }
    }
    let summary = Section::new("summary", "Summary")
        .figure("Raised", format_count(total.alerts), Tone::warn_if(total.alerts))
        .figure("Critical", format_count(critical), Tone::bad_if(critical))
        .figure_with(
            "Still open",
            format_count(total.open),
            "of those raised in the period",
            Tone::warn_if(total.open),
        )
        .figure(
            "Resolved",
            format_count(total.resolved),
            Tone::Neutral,
        )
        .figure_with(
            "Mean time to resolve",
            total
                .mean_resolve_ms()
                .map_or_else(|| "—".to_owned(), format_duration),
            "of those raised in the period",
            Tone::Neutral,
        )
        .figure_with(
            "Resolved in the period",
            format_count(resolved_total.alerts),
            format!(
                "whenever raised; {} to resolve on average",
                resolved_total
                    .mean_resolve_ms()
                    .map_or_else(|| "—".to_owned(), format_duration)
            ),
            Tone::Neutral,
        )
        .note("Each table counts the alerts raised in the period: still open ones, and resolved ones with how long they took.");
    let count_columns = |first: Vec<Column>| {
        let mut columns = first;
        columns.extend([
            Column::count("Raised"),
            Column::count("Open"),
            Column::count("Resolved"),
            Column::new("Mean time to resolve", ColumnKind::Duration),
            Column::new("Longest", ColumnKind::Duration),
        ]);
        columns
    };
    let count_cells = |mut first: Vec<Cell>, counts: &AlertCounts| {
        first.extend([
            Cell::Int(counts.alerts),
            Cell::Int(counts.open),
            Cell::Int(counts.resolved),
            mean(counts),
            counts.longest_ms.into(),
        ]);
        first
    };
    let sorted = |groups: &[(String, AlertCounts)]| {
        let mut groups = groups.to_vec();
        groups.sort_by(|left, right| {
            right
                .1
                .alerts
                .cmp(&left.1.alerts)
                .then(left.0.cmp(&right.0))
        });
        groups
    };
    let kinds_section = Section::new("kinds", "By kind").table(
        Table::new(
            "by-kind",
            "Alerts by kind",
            count_columns(vec![Column::text("Kind")]),
            "No alerts were raised in the period.",
        )
        .with_rows(
            sorted(&by_kind)
                .iter()
                .map(|(kind, counts)| count_cells(vec![Cell::from(condition_label(kind))], counts)),
        ),
    );
    let rules_section = Section::new("rules", "By rule").table(
        Table::new(
            "by-rule",
            "Alerts by rule",
            count_columns(vec![
                Column::text("Rule"),
                Column::text("Severity"),
                Column::text("Kind"),
            ]),
            "No alerts were raised in the period.",
        )
        .with_rows(sorted(&by_rule).iter().filter_map(|(rule_id, counts)| {
            let rule = rules.get(rule_id)?;
            Some(count_cells(
                vec![
                    Cell::from(rule.name.as_str()),
                    Cell::from(severity_title(&rule.severity)),
                    Cell::from(condition_label(&rule.condition)),
                ],
                counts,
            ))
        })),
    );
    let mut device_groups: Vec<(&Device, AlertCounts)> = by_device
        .iter()
        .filter_map(|(id, counts)| names.get(id.as_str()).map(|device| (*device, *counts)))
        .collect();
    device_groups.sort_by(|left, right| {
        right
            .1
            .alerts
            .cmp(&left.1.alerts)
            .then_with(|| left.0.name.cmp(&right.0.name))
    });
    let devices_section = Section::new("devices", "By device").table(
        Table::new(
            "by-device",
            "Alerts by device",
            count_columns(vec![Column::text("Device"), Column::text("Company")]),
            "No alerts were raised in the period.",
        )
        .with_rows(device_groups.iter().map(|(device, counts)| {
            count_cells(
                vec![
                    Cell::from(device.name.as_str()),
                    Cell::from(device.company.clone()),
                ],
                counts,
            )
        })),
    );
    let more = listed.len() as u64 > LISTED_ALERTS;
    let mut list_section = Section::new("list", "Alerts").table(
        Table::new(
            "alerts",
            "Every alert raised in the period, newest first",
            vec![
                Column::new("Raised", ColumnKind::Time),
                Column::text("Rule"),
                Column::text("Severity"),
                Column::text("Device"),
                Column::text("Detail"),
                Column::text("State"),
                Column::new("Resolved", ColumnKind::Time),
                Column::new("Time to resolve", ColumnKind::Duration),
                Column::text("Resolution"),
            ],
            "No alerts were raised in the period.",
        )
        .with_rows(listed.iter().take(LISTED_ALERTS as usize).map(|alert| {
            let rule = rules.get(&alert.rule_id);
            let detail = if alert.subject.is_empty() {
                alert.detail.clone()
            } else {
                format!("{}: {}", alert.subject, alert.detail)
            };
            vec![
                Cell::Int(alert.raised_at),
                Cell::from(rule.map(|rule| rule.name.clone())),
                Cell::from(rule.map(|rule| severity_title(&rule.severity))),
                Cell::from(
                    names
                        .get(alert.device_id.as_str())
                        .map(|device| device.name.clone()),
                ),
                Cell::from(detail),
                Cell::from(state_label(&alert.state)),
                alert.resolved_at.into(),
                alert
                    .resolved_at
                    .map(|resolved| resolved - alert.raised_at)
                    .into(),
                Cell::from(alert.resolution.as_deref().map(resolution_label)),
            ]
        })),
    );
    if more {
        list_section = list_section.note(format!(
            "Only the newest {} alerts are listed; the counts above take every one.",
            format_count(LISTED_ALERTS as i64)
        ));
    }
    Ok(vec![
        summary,
        kinds_section,
        rules_section,
        devices_section,
        list_section,
    ])
}

fn severity_title(severity: &str) -> String {
    match severity {
        "info" => "Info",
        "warning" => "Warning",
        "critical" => "Critical",
        other => other,
    }
    .to_owned()
}

fn state_label(state: &str) -> &str {
    match state {
        "open" => "Open",
        "acknowledged" => "Acknowledged",
        "resolved" => "Resolved",
        other => other,
    }
}

fn resolution_label(resolution: &str) -> String {
    match resolution {
        "cleared" => "Cleared by itself",
        "manual" => "Resolved by a person",
        "not_applicable" => "Rule no longer applies",
        "remediated" => "Self-healed",
        other => other,
    }
    .to_owned()
}
