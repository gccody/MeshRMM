//! Each device's latest application scan: the package managers it has, the
//! packages they have installed, and the updates they offer.
use std::collections::{HashMap, HashSet};

use meshrmm_protocol_types::{
    ApplicationScan, Held, ManagerState, PackageKey, PackageSource, ScannedPackage,
};
use sea_query::{Expr, ExprTrait, LockType, OnConflict, Query};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    library::Application,
    outdated::{self, Delta, stored_key},
    tables::{DeviceApplicationStatus, DevicePackages},
};
use crate::{
    agents,
    db::{self, Database, Executor},
    secrets::hex,
};

/// Rows per insert, well within both databases' bind parameter limits.
const INSERT_CHUNK: usize = 300;

/// Text as PostgreSQL can store it. Scans are sanitized, which removes
/// control characters, so this is a safeguard.
fn storable(text: &str) -> String {
    text.replace('\0', "")
}

/// The manager whose scan lists a source's packages: Homebrew lists casks.
pub fn manager_of(source: PackageSource) -> PackageSource {
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

/// The package managers the device's last scan found, whether or not their
/// scan worked; `None` before its first scan.
pub async fn managers(
    executor: &mut impl Executor,
    device_id: &str,
) -> db::Result<Option<Vec<PackageSource>>> {
    Ok(status(executor, device_id).await?.map(|status| {
        status
            .managers
            .iter()
            .map(|manager| manager.source)
            .collect()
    }))
}

/// What storing a scan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// The device is no longer enrolled: nothing was stored.
    NotEnrolled,
    /// The scan's packages replaced the device's.
    Replaced,
    /// The scan would have stored the packages already stored, so only its
    /// status was written.
    Unchanged,
}

/// Identifies what a scan stores of a device's packages, in any order: the
/// sources it replaces (the others' managers failed, keeping their
/// packages) and its packages from those. A sanitized scan has one package
/// per key.
fn packages_hash(replaced: &[PackageSource], packages: &[&ScannedPackage]) -> String {
    let mut packages = packages.to_vec();
    packages.sort_by(|a, b| (a.source.as_str(), &a.id).cmp(&(b.source.as_str(), &b.id)));
    let json = serde_json::to_vec(&(replaced, packages)).expect("packages serialize");
    hex(&Sha256::digest(json))
}

/// Stores a scan the device's Agent reported, received at `now`. A manager
/// whose scan failed keeps the packages it listed last time; one the device
/// no longer has loses them. A scan that would store the packages already
/// stored writes only its status.
pub async fn store_scan(
    database: &Database,
    device_id: &str,
    scan: ApplicationScan,
    now: i64,
) -> db::Result<Stored> {
    let scan = scan.sanitized();
    let mut transaction = database.begin().await?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Ok(Stored::NotEnrolled);
    }
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
    let packages = scan
        .packages
        .iter()
        .filter(|package| replaced.contains(&package.source))
        .collect::<Vec<_>>();
    let hash = packages_hash(&replaced, &packages);
    // Another report for the device waits until this one commits, so it
    // compares with this one's packages. SQLite's write transactions are
    // serialized anyway.
    let previous: Option<(String,)> = transaction
        .fetch_optional(
            &Query::select()
                .column(DeviceApplicationStatus::PackagesHash)
                .from(DeviceApplicationStatus::Table)
                .and_where(Expr::col(DeviceApplicationStatus::DeviceId).eq(device_id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let unchanged = previous.is_some_and(|(previous,)| previous == hash);
    let managers = serde_json::to_string(&scan.managers).expect("managers serialize");
    let columns = [
        DeviceApplicationStatus::DeviceId,
        DeviceApplicationStatus::Managers,
        DeviceApplicationStatus::ScannedAt,
        DeviceApplicationStatus::ReceivedAt,
        // In the transaction that writes the packages, so it never
        // identifies ones that weren't.
        DeviceApplicationStatus::PackagesHash,
    ];
    transaction
        .execute(
            &Query::insert()
                .into_table(DeviceApplicationStatus::Table)
                .columns(columns)
                .values_panic([
                    device_id.into(),
                    storable(&managers).into(),
                    scan.scanned_at_unix_ms.into(),
                    now.into(),
                    hash.into(),
                ])
                .on_conflict(
                    OnConflict::column(DeviceApplicationStatus::DeviceId)
                        .update_columns(columns.into_iter().skip(1))
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    if !unchanged {
        replace_packages(&mut transaction, device_id, &replaced, &packages, now).await?;
    }
    transaction.commit().await?;
    Ok(if unchanged {
        Stored::Unchanged
    } else {
        Stored::Replaced
    })
}

/// A device's stored package, as replacing and forgetting its packages
/// read it.
#[derive(sqlx::FromRow)]
struct StoredRow {
    source: String,
    package_id: String,
    package_key: String,
    name: String,
    available_version: Option<String>,
    outdated_since: Option<i64>,
}

fn stored_rows() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            DevicePackages::Source,
            DevicePackages::PackageId,
            DevicePackages::PackageKey,
            DevicePackages::Name,
            DevicePackages::AvailableVersion,
            DevicePackages::OutdatedSince,
        ])
        .from(DevicePackages::Table)
        .to_owned()
}

/// The outdated ones of a device's stored packages.
fn outdated_of<'a>(rows: impl IntoIterator<Item = &'a StoredRow>) -> outdated::OutdatedSet {
    outdated::set_of(
        rows.into_iter()
            .filter(|row| row.outdated_since.is_some())
            .map(|row| {
                (
                    row.source.as_str(),
                    row.package_id.as_str(),
                    row.package_key.as_str(),
                    row.name.as_str(),
                    row.available_version.as_deref(),
                )
            }),
    )
}

/// Replaces the device's packages from the `replaced` sources with
/// `packages`, keeping when each was first found outdated, and changes the
/// fleet's outdated packages by the difference.
async fn replace_packages(
    executor: &mut impl Executor,
    device_id: &str,
    replaced: &[PackageSource],
    packages: &[&ScannedPackage],
    now: i64,
) -> db::Result<()> {
    let previous: Vec<StoredRow> = executor
        .fetch_all(
            &stored_rows()
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    let outdated_since = previous
        .iter()
        .filter_map(|row| {
            let source = PackageSource::parse(&row.source)?;
            Some((
                PackageKey {
                    source,
                    id: row.package_id.clone(),
                },
                row.outdated_since?,
            ))
        })
        .collect::<HashMap<_, _>>();
    // The outdated packages the replaced sources had, and those they get.
    let replaced_sources = replaced
        .iter()
        .map(|source| source.as_str())
        .collect::<HashSet<_>>();
    let before = outdated_of(
        previous
            .iter()
            .filter(|row| replaced_sources.contains(row.source.as_str())),
    );
    let ids = packages
        .iter()
        .map(|package| storable(&package.id))
        .collect::<Vec<_>>();
    let names = packages
        .iter()
        .map(|package| storable(&package.name))
        .collect::<Vec<_>>();
    let keys = packages
        .iter()
        .zip(&ids)
        .map(|(package, id)| stored_key(package.source.as_str(), id))
        .collect::<Vec<_>>();
    let available = packages
        .iter()
        .map(|package| package.available_version.as_deref().map(storable))
        .collect::<Vec<_>>();
    let after = outdated::set_of(
        packages
            .iter()
            .enumerate()
            .filter(|(_, package)| package.outdated())
            .map(|(index, package)| {
                (
                    package.source.as_str(),
                    ids[index].as_str(),
                    keys[index].as_str(),
                    names[index].as_str(),
                    available[index].as_deref(),
                )
            }),
    );
    executor
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
    let indexes = (0..packages.len()).collect::<Vec<_>>();
    for chunk in indexes.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(DevicePackages::Table).columns([
            DevicePackages::DeviceId,
            DevicePackages::Source,
            DevicePackages::PackageId,
            DevicePackages::PackageKey,
            DevicePackages::Name,
            DevicePackages::Version,
            DevicePackages::AvailableVersion,
            DevicePackages::OutdatedSince,
        ]);
        for &index in chunk {
            let package = packages[index];
            let since = package.outdated().then(|| {
                outdated_since
                    .get(&PackageKey {
                        source: package.source,
                        id: ids[index].clone(),
                    })
                    .copied()
                    .unwrap_or(now)
            });
            insert.values_panic([
                device_id.into(),
                package.source.as_str().into(),
                ids[index].as_str().into(),
                keys[index].as_str().into(),
                names[index].as_str().into(),
                package.version.as_deref().map(storable).into(),
                available[index].clone().into(),
                since.into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    let delta = Delta::between(&before, &after);
    if !delta.is_empty() {
        outdated::adjust(executor, &delta).await?;
    }
    Ok(())
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

    /// The packages of the library applications `wanted` holds for, in key
    /// order.
    pub fn keys_where(&self, wanted: impl Fn(&Application) -> bool) -> Vec<PackageKey> {
        let mut keys = self
            .by_key
            .iter()
            .filter(|(_, application)| wanted(application))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    pub fn held(&self, key: &PackageKey) -> bool {
        self.application(key)
            .is_some_and(|application| application.pinned_version.is_some())
    }

    /// The pinned library applications' packages.
    pub fn held_keys(&self) -> Vec<PackageKey> {
        self.pinned().into_iter().map(|held| held.package).collect()
    }

    /// The pinned library applications' packages, with the versions they
    /// are pinned to.
    pub fn pinned(&self) -> Vec<Held> {
        let mut held = self
            .by_key
            .iter()
            .filter_map(|(key, application)| {
                Some(Held {
                    package: key.clone(),
                    version: application.pinned_version.clone()?,
                })
            })
            .collect::<Vec<_>>();
        held.sort_by(|a, b| a.package.cmp(&b.package));
        held
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

/// A package a device's latest scan found outdated, for alert rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedPackage {
    /// `None` for a source this server doesn't know, which nothing holds
    /// back.
    pub key: Option<PackageKey>,
    pub name: String,
    /// When a scan first found it outdated.
    pub since: i64,
}

/// Each scanned device's outdated packages, of the `only` devices or of
/// every device; for alert rules. Devices that never scanned are missing.
pub async fn outdated_packages(
    executor: &mut impl Executor,
    only: Option<&[String]>,
) -> db::Result<HashMap<String, Vec<OutdatedPackage>>> {
    let mut scanned = Query::select();
    scanned
        .column(DeviceApplicationStatus::DeviceId)
        .from(DeviceApplicationStatus::Table);
    let mut outdated = Query::select();
    outdated
        .columns([
            DevicePackages::DeviceId,
            DevicePackages::Source,
            DevicePackages::PackageId,
            DevicePackages::Name,
            DevicePackages::OutdatedSince,
        ])
        .from(DevicePackages::Table)
        .and_where(Expr::col(DevicePackages::OutdatedSince).is_not_null());
    if let Some(ids) = only {
        scanned.and_where(Expr::col(DeviceApplicationStatus::DeviceId).is_in(ids.iter().cloned()));
        outdated.and_where(Expr::col(DevicePackages::DeviceId).is_in(ids.iter().cloned()));
    }
    let scanned: Vec<(String,)> = executor.fetch_all(&scanned).await?;
    let mut packages = scanned
        .into_iter()
        .map(|(device_id,)| (device_id, Vec::new()))
        .collect::<HashMap<_, _>>();
    let rows: Vec<(String, String, String, String, i64)> = executor.fetch_all(&outdated).await?;
    for (device_id, source, package_id, name, since) in rows {
        if let Some(outdated) = packages.get_mut(&device_id) {
            outdated.push(OutdatedPackage {
                key: PackageSource::parse(&source).map(|source| PackageKey {
                    source,
                    id: package_id,
                }),
                name,
                since,
            });
        }
    }
    Ok(packages)
}

/// The device was deleted: its scan goes with it, and its outdated packages
/// leave the fleet's.
pub async fn forget(database: &Database, device_id: &str) -> db::Result<()> {
    let mut transaction = database.begin().await?;
    // Waits for a scan of the device that is being stored, as storing one
    // locks the row.
    transaction
        .fetch_optional::<(String,), _>(
            &Query::select()
                .column(DeviceApplicationStatus::DeviceId)
                .from(DeviceApplicationStatus::Table)
                .and_where(Expr::col(DeviceApplicationStatus::DeviceId).eq(device_id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let rows: Vec<StoredRow> = transaction
        .fetch_all(
            &stored_rows()
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .and_where(Expr::col(DevicePackages::OutdatedSince).is_not_null())
                .to_owned(),
        )
        .await?;
    let delta = Delta::between(&outdated_of(&rows), &outdated::OutdatedSet::new());
    transaction
        .execute(
            &Query::delete()
                .from_table(DevicePackages::Table)
                .and_where(Expr::col(DevicePackages::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceApplicationStatus::Table)
                .and_where(Expr::col(DeviceApplicationStatus::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    if !delta.is_empty() {
        outdated::adjust(&mut transaction, &delta).await?;
    }
    transaction.commit().await?;
    Ok(())
}
