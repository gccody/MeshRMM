//! Each device's latest application scan: the package managers it has, the
//! packages they have installed, and the updates they offer.
use std::collections::{HashMap, HashSet};

use meshrmm_protocol_types::{ApplicationScan, ManagerState, PackageKey, PackageSource};
use sea_query::{Expr, ExprTrait, OnConflict, Query};
use serde::Serialize;

use super::{
    library::Application,
    tables::{DeviceApplicationStatus, DevicePackages},
};
use crate::{
    agents,
    db::{self, Database, Executor, tables::Agents},
};

/// Rows per insert, well within both databases' bind parameter limits.
const INSERT_CHUNK: usize = 300;

/// Text as PostgreSQL can store it. Scans are sanitized, which removes
/// control characters, so this is a safeguard.
fn storable(text: &str) -> String {
    text.replace('\0', "")
}

/// The manager whose scan lists a source's packages: Homebrew lists casks.
fn manager_of(source: PackageSource) -> PackageSource {
    if source == PackageSource::HomebrewCask {
        PackageSource::Homebrew
    } else {
        source
    }
}

/// A device's scan status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub managers: Vec<ManagerState>,
    /// When the device scanned, by its clock.
    pub scanned_at: i64,
    /// When the report arrived.
    pub received_at: i64,
}

pub async fn status(executor: &mut impl Executor, device_id: &str) -> db::Result<Option<Status>> {
    let row: Option<(String, i64, i64)> = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    DeviceApplicationStatus::Managers,
                    DeviceApplicationStatus::ScannedAt,
                    DeviceApplicationStatus::ReceivedAt,
                ])
                .from(DeviceApplicationStatus::Table)
                .and_where(Expr::col(DeviceApplicationStatus::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(managers, scanned_at, received_at)| Status {
        managers: serde_json::from_str(&managers).unwrap_or_default(),
        scanned_at,
        received_at,
    }))
}

/// Stores a scan the device's Agent reported, received at `now`. A manager
/// whose scan failed keeps the packages it listed last time; one the device
/// no longer has loses them. Returns `false`, storing nothing, if the device
/// is no longer enrolled.
pub async fn store_scan(
    database: &Database,
    device_id: &str,
    scan: ApplicationScan,
    now: i64,
) -> db::Result<bool> {
    let scan = scan.sanitized();
    let mut transaction = database.begin().await?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Ok(false);
    }
    let managers = serde_json::to_string(&scan.managers).expect("managers serialize");
    transaction
        .execute(
            &Query::insert()
                .into_table(DeviceApplicationStatus::Table)
                .columns([
                    DeviceApplicationStatus::DeviceId,
                    DeviceApplicationStatus::Managers,
                    DeviceApplicationStatus::ScannedAt,
                    DeviceApplicationStatus::ReceivedAt,
                ])
                .values_panic([
                    device_id.into(),
                    storable(&managers).into(),
                    scan.scanned_at_unix_ms.into(),
                    now.into(),
                ])
                .on_conflict(
                    OnConflict::column(DeviceApplicationStatus::DeviceId)
                        .update_columns([
                            DeviceApplicationStatus::Managers,
                            DeviceApplicationStatus::ScannedAt,
                            DeviceApplicationStatus::ReceivedAt,
                        ])
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    let failed = scan
        .managers
        .iter()
        .filter(|manager| manager.error.is_some())
        .map(|manager| manager.source)
        .collect::<HashSet<_>>();
    let replaced = PackageSource::ALL
        .into_iter()
        .filter(|source| *source != PackageSource::Custom && !failed.contains(&manager_of(*source)))
        .collect::<Vec<_>>();
    let previous: Vec<(String, String, Option<i64>)> = transaction
        .fetch_all(
            &Query::select()
                .columns([
                    DevicePackages::Source,
                    DevicePackages::PackageId,
                    DevicePackages::OutdatedSince,
                ])
                .from(DevicePackages::Table)
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    let outdated_since = previous
        .into_iter()
        .filter_map(|(source, id, since)| Some(((source, id), since?)))
        .collect::<HashMap<_, _>>();
    transaction
        .execute(
            &Query::delete()
                .from_table(DevicePackages::Table)
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .and_where(
                    Expr::col(DevicePackages::Source)
                        .is_in(replaced.iter().map(|source| source.as_str())),
                )
                .to_owned(),
        )
        .await?;
    let packages = scan
        .packages
        .iter()
        .filter(|package| replaced.contains(&package.source))
        .collect::<Vec<_>>();
    for chunk in packages.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(DevicePackages::Table).columns([
            DevicePackages::DeviceId,
            DevicePackages::Source,
            DevicePackages::PackageId,
            DevicePackages::Name,
            DevicePackages::Version,
            DevicePackages::AvailableVersion,
            DevicePackages::OutdatedSince,
        ]);
        for package in chunk {
            let id = storable(&package.id);
            let since = package.outdated().then(|| {
                outdated_since
                    .get(&(package.source.as_str().to_owned(), id.clone()))
                    .copied()
                    .unwrap_or(now)
            });
            insert.values_panic([
                device_id.into(),
                package.source.as_str().into(),
                id.into(),
                storable(&package.name).into(),
                package.version.as_deref().map(storable).into(),
                package.available_version.as_deref().map(storable).into(),
                since.into(),
            ]);
        }
        transaction.execute(&insert).await?;
    }
    transaction.commit().await?;
    Ok(true)
}

/// A package a device has installed, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Package {
    pub source: String,
    pub package_id: String,
    pub name: String,
    pub version: Option<String>,
    pub available_version: Option<String>,
    /// When a scan first found it outdated.
    pub outdated_since: Option<i64>,
    /// The library application it belongs to, if any.
    #[sqlx(skip)]
    pub application_id: Option<String>,
    #[sqlx(skip)]
    pub application_name: Option<String>,
    /// The library pins its version, so updates leave it alone.
    #[sqlx(skip)]
    pub held: bool,
}

impl Package {
    pub fn key(&self) -> Option<PackageKey> {
        Some(PackageKey {
            source: PackageSource::parse(&self.source)?,
            id: self.package_id.clone(),
        })
    }
}

fn package_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            DevicePackages::Source,
            DevicePackages::PackageId,
            DevicePackages::Name,
            DevicePackages::Version,
            DevicePackages::AvailableVersion,
            DevicePackages::OutdatedSince,
        ])
        .from(DevicePackages::Table)
        .to_owned()
}

/// The library application each package belongs to, and the packages it
/// pins.
pub struct Matches<'a> {
    by_key: HashMap<PackageKey, &'a Application>,
}

impl<'a> Matches<'a> {
    pub fn new(library: &'a [Application]) -> Self {
        let mut by_key = HashMap::new();
        for application in library {
            for key in application.keys() {
                by_key.entry(key).or_insert(application);
            }
        }
        Self { by_key }
    }

    pub fn application(&self, key: &PackageKey) -> Option<&'a Application> {
        self.by_key.get(key).copied()
    }

    pub fn held(&self, key: &PackageKey) -> bool {
        self.application(key)
            .is_some_and(|application| application.pinned_version.is_some())
    }

    /// The pinned library applications' packages.
    pub fn held_keys(&self) -> Vec<PackageKey> {
        let mut keys = self
            .by_key
            .iter()
            .filter(|(_, application)| application.pinned_version.is_some())
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    fn annotate(&self, package: &mut Package) {
        if let Some(key) = package.key()
            && let Some(application) = self.application(&key)
        {
            package.application_id = Some(application.id.clone());
            package.application_name = Some(application.name.clone());
            package.held = application.pinned_version.is_some();
        }
    }
}

/// The device's packages: outdated first, then by name.
pub async fn packages(
    executor: &mut impl Executor,
    device_id: &str,
    matches: &Matches<'_>,
) -> db::Result<Vec<Package>> {
    let mut packages: Vec<Package> = executor
        .fetch_all(
            &package_select()
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    for package in &mut packages {
        matches.annotate(package);
    }
    packages.sort_by_cached_key(|package| {
        (
            package.available_version.is_none(),
            package.name.to_lowercase(),
            package.source.clone(),
            package.package_id.clone(),
        )
    });
    Ok(packages)
}

/// A device with an outdated package, in the fleet's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutdatedDevice {
    pub device_id: String,
    pub device_name: String,
    pub version: Option<String>,
    pub available_version: Option<String>,
    pub outdated_since: Option<i64>,
}

/// An outdated package, with the devices it is outdated on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outdated {
    pub source: String,
    pub package_id: String,
    pub name: String,
    pub application_id: Option<String>,
    pub application_name: Option<String>,
    pub held: bool,
    pub devices: Vec<OutdatedDevice>,
}

#[derive(sqlx::FromRow)]
struct OutdatedRow {
    device_id: String,
    device_name: String,
    source: String,
    package_id: String,
    name: String,
    version: Option<String>,
    available_version: Option<String>,
    outdated_since: Option<i64>,
}

/// Every outdated package on enrolled devices: most devices first.
pub async fn outdated(
    executor: &mut impl Executor,
    matches: &Matches<'_>,
) -> db::Result<Vec<Outdated>> {
    let rows: Vec<OutdatedRow> = executor
        .fetch_all(
            &Query::select()
                .column((DevicePackages::Table, DevicePackages::DeviceId))
                .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
                .columns([
                    (DevicePackages::Table, DevicePackages::Source),
                    (DevicePackages::Table, DevicePackages::PackageId),
                    (DevicePackages::Table, DevicePackages::Name),
                    (DevicePackages::Table, DevicePackages::Version),
                    (DevicePackages::Table, DevicePackages::AvailableVersion),
                    (DevicePackages::Table, DevicePackages::OutdatedSince),
                ])
                .from(DevicePackages::Table)
                .inner_join(
                    Agents::Table,
                    Expr::col((Agents::Table, Agents::Id))
                        .equals((DevicePackages::Table, DevicePackages::DeviceId)),
                )
                .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
                .and_where(
                    Expr::col((DevicePackages::Table, DevicePackages::AvailableVersion))
                        .is_not_null(),
                )
                .to_owned(),
        )
        .await?;
    let mut grouped: HashMap<(String, String), Outdated> = HashMap::new();
    for row in rows {
        let entry = grouped
            .entry((row.source.clone(), row.package_id.clone()))
            .or_insert_with(|| {
                let mut package = Package {
                    source: row.source.clone(),
                    package_id: row.package_id.clone(),
                    name: row.name.clone(),
                    version: None,
                    available_version: None,
                    outdated_since: None,
                    application_id: None,
                    application_name: None,
                    held: false,
                };
                matches.annotate(&mut package);
                Outdated {
                    source: package.source,
                    package_id: package.package_id,
                    name: package.name,
                    application_id: package.application_id,
                    application_name: package.application_name,
                    held: package.held,
                    devices: Vec::new(),
                }
            });
        entry.devices.push(OutdatedDevice {
            device_id: row.device_id,
            device_name: row.device_name,
            version: row.version,
            available_version: row.available_version,
            outdated_since: row.outdated_since,
        });
    }
    let mut outdated = grouped.into_values().collect::<Vec<_>>();
    for package in &mut outdated {
        package.devices.sort_by_cached_key(|device| {
            (device.device_name.to_lowercase(), device.device_id.clone())
        });
    }
    outdated.sort_by_cached_key(|package| {
        (
            std::cmp::Reverse(package.devices.len()),
            package.name.to_lowercase(),
            package.source.clone(),
            package.package_id.clone(),
        )
    });
    Ok(outdated)
}

/// Each scanned device's outdated packages, by name, with when each was
/// first found outdated; for alert rules. Devices that never scanned are
/// missing.
pub async fn outdated_ages(
    executor: &mut impl Executor,
    held: &[PackageKey],
) -> db::Result<HashMap<String, Vec<(String, i64)>>> {
    let scanned: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(DeviceApplicationStatus::DeviceId)
                .from(DeviceApplicationStatus::Table)
                .to_owned(),
        )
        .await?;
    let mut ages = scanned
        .into_iter()
        .map(|(device_id,)| (device_id, Vec::new()))
        .collect::<HashMap<_, _>>();
    let rows: Vec<(String, String, String, String, i64)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    DevicePackages::DeviceId,
                    DevicePackages::Source,
                    DevicePackages::PackageId,
                    DevicePackages::Name,
                    DevicePackages::OutdatedSince,
                ])
                .from(DevicePackages::Table)
                .and_where(Expr::col(DevicePackages::OutdatedSince).is_not_null())
                .to_owned(),
        )
        .await?;
    for (device_id, source, package_id, name, since) in rows {
        // Pinned library applications are held back on purpose.
        let is_held = held
            .iter()
            .any(|key| key.source.as_str() == source && key.id == package_id);
        if is_held {
            continue;
        }
        if let Some(packages) = ages.get_mut(&device_id) {
            packages.push((name, since));
        }
    }
    Ok(ages)
}

/// The device was deleted: its scan goes with it.
pub async fn forget(executor: &mut impl Executor, device_id: &str) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(DevicePackages::Table)
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(DeviceApplicationStatus::Table)
                .and_where(Expr::col(DeviceApplicationStatus::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(())
}
