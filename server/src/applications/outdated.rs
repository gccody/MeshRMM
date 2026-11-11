//! The fleet's outdated packages: how many devices have each one outdated,
//! kept in `outdated_packages` so `GET /v1/packages/outdated` reads a page
//! of a small summary instead of grouping every device's packages, and the
//! devices of one package, read a page at a time from
//! `idx_device_packages_outdated_key`.
//!
//! The summary changes in the transaction that changes a device's rows in
//! `device_packages`, by the difference between the outdated packages it
//! had and those it gets: when a scan replaces its packages
//! ([`super::store::store_scan`]) and when its rows are removed
//! ([`super::store::forget`], which deleting a device runs). Nothing else
//! writes `device_packages`, so the summary counts, for each package as its
//! manager compares IDs (`package_key`), the devices whose rows have it
//! outdated. A device whose deletion was asked for counts until its rows
//! are removed, which deleting it does at once. Each row keeps the newest
//! version a scan offered ([`compare_versions`]); two scans that first
//! offer different versions in the same moment on PostgreSQL can leave the
//! older one until a scan offers the newer again. A row whose count reaches
//! 0 is removed.
//!
//! Like `software_names`, a package outdated on many devices has a row
//! every changed scan of those devices may touch; each scan changes only
//! the rows its difference names, in key order, so on PostgreSQL two scans
//! wait for each other rather than deadlock. SQLite has one writer anyway.
use std::collections::BTreeMap;

use meshrmm_protocol_types::{PackageKey, PackageSource, compare_versions};
use sea_query::{
    Alias, Cond, Condition, Expr, ExprTrait, Func, LockType, OnConflict, Order, Query,
    SelectStatement,
};
use serde::Serialize;

use super::{
    store::Matches,
    tables::{DevicePackages, OutdatedPackages},
};
use crate::{
    access::CompanySet,
    db::{self, Executor, tables::Agents},
    system_info::search::column_contains,
};

/// Rows per statement, well within both databases' bind parameter limits.
const CHUNK: usize = 200;

/// A package's ID as its manager compares it, as `device_packages` and
/// `outdated_packages` key it: lowercase where the manager ignores case. A
/// source this server doesn't know keeps the ID as it is.
pub fn stored_key(source: &str, id: &str) -> String {
    match PackageSource::parse(source) {
        Some(source) if source.ignores_case() => id.to_ascii_lowercase(),
        _ => id.to_owned(),
    }
}

/// `(source, package_key)`, as the summary keys a package.
type Key = (String, String);

fn key_of(package: &PackageKey) -> Key {
    (
        package.source.as_str().to_owned(),
        package.folded().into_owned(),
    )
}

/// A device's outdated package: how it lists it, and the version its
/// manager offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub package_id: String,
    pub name: String,
    pub available_version: Option<String>,
}

/// A device's outdated packages, by key.
pub(crate) type OutdatedSet = BTreeMap<Key, Listed>;

/// One change to a package's row: the devices it gains (+1), loses (-1) or
/// keeps (0, when only the version offered changed), and the version a
/// device now offers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    key: Key,
    listed: Listed,
    devices: i64,
}

/// What replacing a device's outdated packages `old` with `new` does to the
/// summary, in key order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Delta {
    changes: Vec<Change>,
}

impl Delta {
    pub(crate) fn between(old: &OutdatedSet, new: &OutdatedSet) -> Self {
        let mut changes = Vec::new();
        for (key, listed) in new {
            let devices = match old.get(key) {
                None => 1,
                Some(before) if before.available_version != listed.available_version => 0,
                Some(_) => continue,
            };
            changes.push(Change {
                key: key.clone(),
                listed: listed.clone(),
                devices,
            });
        }
        for (key, listed) in old {
            if !new.contains_key(key) {
                changes.push(Change {
                    key: key.clone(),
                    listed: Listed {
                        available_version: None,
                        ..listed.clone()
                    },
                    devices: -1,
                });
            }
        }
        changes.sort_by(|a, b| a.key.cmp(&b.key));
        Self { changes }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

fn is_key(key: &Key) -> Condition {
    Cond::all()
        .add(Expr::col((OutdatedPackages::Table, OutdatedPackages::Source)).eq(key.0.as_str()))
        .add(Expr::col((OutdatedPackages::Table, OutdatedPackages::PackageKey)).eq(key.1.as_str()))
}

/// Applies a device's change to the summary, inside the transaction that
/// changes its rows.
pub(crate) async fn adjust(executor: &mut impl Executor, delta: &Delta) -> db::Result<()> {
    for chunk in delta.changes.chunks(CHUNK) {
        // The versions stored for the packages a device now offers a version
        // of, locked on PostgreSQL until the change is written.
        let offered = chunk
            .iter()
            .filter(|change| change.listed.available_version.is_some())
            .map(|change| is_key(&change.key))
            .fold(Cond::any(), |any, condition| any.add(condition));
        let stored: BTreeMap<Key, Option<String>> = if offered.is_empty() {
            BTreeMap::new()
        } else {
            let rows: Vec<(String, String, Option<String>)> = executor
                .fetch_all(
                    &Query::select()
                        .columns([
                            OutdatedPackages::Source,
                            OutdatedPackages::PackageKey,
                            OutdatedPackages::AvailableVersion,
                        ])
                        .from(OutdatedPackages::Table)
                        .cond_where(offered)
                        .order_by(OutdatedPackages::Source, Order::Asc)
                        .order_by(OutdatedPackages::PackageKey, Order::Asc)
                        .lock(LockType::Update)
                        .to_owned(),
                )
                .await?;
            rows.into_iter()
                .map(|(source, key, version)| ((source, key), version))
                .collect()
        };
        let mut insert = Query::insert();
        insert.into_table(OutdatedPackages::Table).columns([
            OutdatedPackages::Source,
            OutdatedPackages::PackageKey,
            OutdatedPackages::PackageId,
            OutdatedPackages::Name,
            OutdatedPackages::AvailableVersion,
            OutdatedPackages::Devices,
        ]);
        for change in chunk {
            // The newer of the stored version and the one offered; none for
            // a device that stops having it outdated, which keeps the stored.
            let version = match (
                stored.get(&change.key).cloned().flatten(),
                change.listed.available_version.clone(),
            ) {
                (Some(stored), Some(offered)) if compare_versions(&stored, &offered).is_ge() => {
                    Some(stored)
                }
                (_, offered) => offered,
            };
            insert.values_panic([
                change.key.0.as_str().into(),
                change.key.1.as_str().into(),
                change.listed.package_id.as_str().into(),
                change.listed.name.as_str().into(),
                version.into(),
                change.devices.into(),
            ]);
        }
        insert.on_conflict(
            OnConflict::columns([OutdatedPackages::Source, OutdatedPackages::PackageKey])
                .value(
                    OutdatedPackages::Devices,
                    Expr::cust("outdated_packages.devices + excluded.devices"),
                )
                .value(
                    OutdatedPackages::AvailableVersion,
                    Expr::cust(
                        "COALESCE(excluded.available_version, outdated_packages.available_version)",
                    ),
                )
                .to_owned(),
        );
        executor.execute(&insert).await?;
        // Only the rows a device left can have reached 0. `<= 0` also
        // removes a count taken below 0, so none is ever kept.
        let removed = chunk
            .iter()
            .filter(|change| change.devices < 0)
            .map(|change| is_key(&change.key))
            .fold(Cond::any(), |any, condition| any.add(condition));
        if !removed.is_empty() {
            executor
                .execute(
                    &Query::delete()
                        .from_table(OutdatedPackages::Table)
                        .cond_where(removed)
                        .and_where(Expr::col(OutdatedPackages::Devices).lte(0))
                        .to_owned(),
                )
                .await?;
        }
    }
    Ok(())
}

/// The device's outdated packages, as its rows in `device_packages` hold
/// them.
pub(crate) fn set_of<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str, &'a str, Option<&'a str>)>,
) -> OutdatedSet {
    rows.into_iter()
        .map(|(source, package_id, key, name, available)| {
            (
                (source.to_owned(), key.to_owned()),
                Listed {
                    package_id: package_id.to_owned(),
                    name: name.to_owned(),
                    available_version: available.map(str::to_owned),
                },
            )
        })
        .collect()
}

/// Holds for the rows of `device_packages` of any of `packages`.
fn device_package_is_one_of(packages: &[PackageKey]) -> Condition {
    packages
        .iter()
        .map(|package| {
            Cond::all()
                .add(
                    Expr::col((DevicePackages::Table, DevicePackages::Source))
                        .eq(package.source.as_str()),
                )
                .add(
                    Expr::col((DevicePackages::Table, DevicePackages::PackageKey))
                        .eq(package.folded().into_owned()),
                )
        })
        .fold(Cond::any(), |any, condition| any.add(condition))
}

/// The devices with any of `packages` outdated, or with none chosen the
/// devices with any package outdated but those in `except`: for a job's
/// target, as a subquery of device IDs.
pub fn devices_with_outdated(packages: &[PackageKey], except: &[PackageKey]) -> SelectStatement {
    let outdated =
        || Expr::col((DevicePackages::Table, DevicePackages::OutdatedSince)).is_not_null();
    let mut select = Query::select();
    select
        .column((DevicePackages::Table, DevicePackages::DeviceId))
        .from(DevicePackages::Table);
    if packages.is_empty() {
        select.and_where(outdated());
        if !except.is_empty() {
            select.cond_where(device_package_is_one_of(except).not());
        }
    } else {
        // Each package's own term says it is outdated, so SQLite, which
        // plans each term of an OR by itself, finds each in the partial
        // index of outdated rows.
        select.cond_where(
            packages
                .iter()
                .map(|package| {
                    Cond::all()
                        .add(device_package_is_one_of(std::slice::from_ref(package)))
                        .add(outdated())
                })
                .fold(Cond::any(), |any, condition| any.add(condition)),
        );
    }
    select
}

/// The names of `packages` as the summary lists them, in their order; the
/// ID for one no device has outdated.
pub async fn names(
    executor: &mut impl Executor,
    packages: &[PackageKey],
) -> db::Result<Vec<String>> {
    let mut found: BTreeMap<Key, String> = BTreeMap::new();
    for chunk in packages.chunks(CHUNK) {
        let rows: Vec<(String, String, String)> = executor
            .fetch_all(
                &Query::select()
                    .columns([
                        OutdatedPackages::Source,
                        OutdatedPackages::PackageKey,
                        OutdatedPackages::Name,
                    ])
                    .from(OutdatedPackages::Table)
                    .cond_where(
                        chunk
                            .iter()
                            .map(|package| is_key(&key_of(package)))
                            .fold(Cond::any(), |any, condition| any.add(condition)),
                    )
                    .to_owned(),
            )
            .await?;
        found.extend(
            rows.into_iter()
                .map(|(source, key, name)| ((source, key), name)),
        );
    }
    Ok(packages
        .iter()
        .map(|package| {
            found
                .get(&key_of(package))
                .cloned()
                .unwrap_or_else(|| package.id.clone())
        })
        .collect())
}

/// What the list of outdated packages shows: those whose name or ID
/// contains `query`, or whose library application's name does (`named`),
/// and which library packages are pinned (`held`).
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub query: Option<String>,
    /// The packages of the library applications whose name contains the
    /// query.
    pub named: Vec<PackageKey>,
    /// The packages pinned library applications hold back.
    pub held: Vec<PackageKey>,
    /// Only devices in these companies. The summary counts every device,
    /// so for some companies the counts come from their devices' rows.
    pub within: CompanySet,
}

impl ListFilter {
    /// From the library: the applications `query` names, and those pinned.
    pub fn new(query: Option<String>, matches: &Matches<'_>) -> Self {
        let needle = query.as_deref().map(str::to_lowercase);
        let named = match &needle {
            Some(needle) => matches.keys_where(|application| {
                application.name.to_lowercase().contains(needle.as_str())
            }),
            None => Vec::new(),
        };
        Self {
            query,
            named,
            held: matches.held_keys(),
            within: CompanySet::All,
        }
    }

    fn matches(&self) -> Option<Condition> {
        let text = self.query.as_deref()?;
        let mut condition = Cond::any()
            .add(column_contains(
                (OutdatedPackages::Table, OutdatedPackages::Name),
                text,
            ))
            .add(column_contains(
                (OutdatedPackages::Table, OutdatedPackages::PackageId),
                text,
            ));
        for package in &self.named {
            condition = condition.add(is_key(&key_of(package)));
        }
        Some(condition)
    }

    fn held(&self) -> Option<Condition> {
        (!self.held.is_empty()).then(|| {
            self.held
                .iter()
                .map(|package| is_key(&key_of(package)))
                .fold(Cond::any(), |any, condition| any.add(condition))
        })
    }
}

/// Adds the summary to `select` as its table: `outdated_packages` itself, or
/// for devices in some companies the same columns grouped from those
/// devices' rows, named `outdated_packages`. Grouped, a package's newest
/// version is the greatest as text, where the summary compares versions.
fn from_summary(select: &mut SelectStatement, within: &CompanySet) {
    let Some(companies) = within.company_condition((Agents::Table, Agents::CompanyId)) else {
        select.from(OutdatedPackages::Table);
        return;
    };
    let column = |column| Expr::col((DevicePackages::Table, column));
    let grouped = Query::select()
        .column((DevicePackages::Table, DevicePackages::Source))
        .column((DevicePackages::Table, DevicePackages::PackageKey))
        .expr_as(
            Func::min(column(DevicePackages::PackageId)),
            OutdatedPackages::PackageId,
        )
        .expr_as(
            Func::min(column(DevicePackages::Name)),
            OutdatedPackages::Name,
        )
        .expr_as(
            Func::max(column(DevicePackages::AvailableVersion)),
            OutdatedPackages::AvailableVersion,
        )
        .expr_as(
            Func::cast_as(
                Func::count(column(DevicePackages::DeviceId)),
                Alias::new("BIGINT"),
            ),
            OutdatedPackages::Devices,
        )
        .from(DevicePackages::Table)
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((DevicePackages::Table, DevicePackages::DeviceId)),
        )
        .and_where(column(DevicePackages::OutdatedSince).is_not_null())
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .and_where(companies)
        .group_by_col((DevicePackages::Table, DevicePackages::Source))
        .group_by_col((DevicePackages::Table, DevicePackages::PackageKey))
        .to_owned();
    select.from_subquery(grouped, OutdatedPackages::Table);
}

/// `SUM(CASE WHEN condition THEN value ELSE 0 END)` as a BIGINT, never NULL.
fn sum_where(condition: Option<Condition>, value: Expr) -> Expr {
    let summed = match condition {
        Some(condition) => Expr::case(condition, value).finally(Expr::cust("0")).into(),
        None => value,
    };
    Func::cast_as(
        Func::coalesce([Func::sum(summed).into(), Expr::cust("0")]),
        Alias::new("BIGINT"),
    )
    .into()
}

/// How many packages the filter matches, and of every package that isn't
/// held back, how many there are and how many devices have them outdated:
/// one read of the summary, one row per package rather than per device.
pub fn counts_query(filter: &ListFilter) -> SelectStatement {
    let not_held = filter.held().map(Condition::not);
    let mut select = Query::select();
    select
        .expr_as(sum_where(filter.matches(), Expr::cust("1")), "matching")
        .expr_as(sum_where(not_held.clone(), Expr::cust("1")), "packages")
        .expr_as(
            sum_where(
                not_held,
                Expr::col((OutdatedPackages::Table, OutdatedPackages::Devices)),
            ),
            "updates",
        );
    from_summary(&mut select, &filter.within);
    select
}

/// The page of `limit` packages from `offset` that the filter matches,
/// outdated on the most devices first, then by name. The source and key
/// settle ties, so pages neither repeat nor miss a package; the summary's
/// index on (devices, name, source, package_key) gives this order.
pub fn page_query(filter: &ListFilter, limit: u64, offset: u64) -> SelectStatement {
    let mut select = Query::select();
    select.columns([
        OutdatedPackages::Source,
        OutdatedPackages::PackageId,
        OutdatedPackages::Name,
        OutdatedPackages::AvailableVersion,
        OutdatedPackages::Devices,
    ]);
    from_summary(&mut select, &filter.within);
    if let Some(condition) = filter.matches() {
        select.cond_where(condition);
    }
    select
        .order_by(OutdatedPackages::Devices, Order::Desc)
        .order_by(OutdatedPackages::Name, Order::Asc)
        .order_by(OutdatedPackages::Source, Order::Asc)
        .order_by(OutdatedPackages::PackageKey, Order::Asc)
        .limit(limit)
        .offset(offset)
        .to_owned()
}

/// An outdated package in the fleet's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outdated {
    pub source: String,
    pub package_id: String,
    pub name: String,
    pub application_id: Option<String>,
    pub application_name: Option<String>,
    /// Its library application pins a version, so updates leave it alone.
    pub held: bool,
    /// The newest version a scan offered.
    pub available_version: Option<String>,
    /// How many devices have it outdated.
    pub devices: i64,
}

/// The packages that aren't held back, and their devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Totals {
    /// Packages outdated on any device.
    pub packages: i64,
    /// Pairs of a device and a package it has outdated.
    pub updates: i64,
}

/// A page of the fleet's outdated packages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page {
    pub packages: Vec<Outdated>,
    /// The packages the search matches, on this page or not.
    pub matching: i64,
    pub totals: Totals,
}

/// A page of the fleet's outdated packages: two reads of the summary, for
/// a fleet of any size.
pub async fn page(
    executor: &mut impl Executor,
    filter: &ListFilter,
    matches: &Matches<'_>,
    limit: u64,
    offset: u64,
) -> db::Result<Page> {
    let (matching, packages, updates): (i64, i64, i64) =
        executor.fetch_one(&counts_query(filter)).await?;
    let rows: Vec<(String, String, String, Option<String>, i64)> =
        if limit == 0 || offset >= u64::try_from(matching).unwrap_or(0) {
            Vec::new()
        } else {
            executor
                .fetch_all(&page_query(filter, limit, offset))
                .await?
        };
    let packages_page = rows
        .into_iter()
        .map(|(source, package_id, name, available_version, devices)| {
            let application = PackageSource::parse(&source).and_then(|parsed| {
                matches.application(&PackageKey {
                    source: parsed,
                    id: package_id.clone(),
                })
            });
            Outdated {
                application_id: application.map(|application| application.id.clone()),
                application_name: application.map(|application| application.name.clone()),
                held: application.is_some_and(|application| application.pinned_version.is_some()),
                source,
                package_id,
                name,
                available_version,
                devices,
            }
        })
        .collect();
    Ok(Page {
        packages: packages_page,
        matching,
        totals: Totals { packages, updates },
    })
}

/// A device with a package outdated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct OutdatedDevice {
    pub device_id: String,
    pub device_name: String,
    pub version: Option<String>,
    pub available_version: Option<String>,
    /// When a scan first found it outdated.
    pub outdated_since: Option<i64>,
}

/// A page of the devices with a package outdated, in device ID order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DevicePage {
    pub devices: Vec<OutdatedDevice>,
    /// The `after` of the next page; `None` on the last.
    pub next: Option<String>,
}

/// Up to `limit` devices that aren't deleted with `package` outdated, after
/// the device `after`, in device ID order: read from
/// `idx_device_packages_outdated_key` in that order, so about as many
/// entries as the page holds, joined to their devices by key.
pub fn devices_query(
    package: &PackageKey,
    within: &CompanySet,
    after: Option<&str>,
    limit: u64,
) -> SelectStatement {
    let mut select = Query::select();
    select
        .column((DevicePackages::Table, DevicePackages::DeviceId))
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .columns([
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
        .and_where(
            Expr::col((DevicePackages::Table, DevicePackages::Source)).eq(package.source.as_str()),
        )
        .and_where(
            Expr::col((DevicePackages::Table, DevicePackages::PackageKey))
                .eq(package.folded().into_owned()),
        )
        .and_where(Expr::col((DevicePackages::Table, DevicePackages::OutdatedSince)).is_not_null())
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .and_where_option(within.company_condition((Agents::Table, Agents::CompanyId)));
    if let Some(after) = after {
        select.and_where(Expr::col((DevicePackages::Table, DevicePackages::DeviceId)).gt(after));
    }
    // The database's order, which `after` follows.
    select
        .order_by(
            (DevicePackages::Table, DevicePackages::DeviceId),
            Order::Asc,
        )
        .limit(limit)
        .to_owned()
}

/// A page of the devices with `package` outdated: one read, for a package
/// on any number of devices.
pub async fn devices(
    executor: &mut impl Executor,
    package: &PackageKey,
    within: &CompanySet,
    after: Option<&str>,
    limit: u64,
) -> db::Result<DevicePage> {
    if limit == 0 {
        return Ok(DevicePage {
            devices: Vec::new(),
            next: None,
        });
    }
    // One more than the page says whether another follows.
    let mut devices: Vec<OutdatedDevice> = executor
        .fetch_all(&devices_query(package, within, after, limit + 1))
        .await?;
    let more = devices.len() as u64 > limit;
    devices.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let next = more
        .then(|| devices.last().map(|device| device.device_id.clone()))
        .flatten();
    Ok(DevicePage { devices, next })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(id: &str, version: Option<&str>) -> Listed {
        Listed {
            package_id: id.into(),
            name: id.into(),
            available_version: version.map(Into::into),
        }
    }

    fn set(entries: &[(&str, &str, Option<&str>)]) -> OutdatedSet {
        entries
            .iter()
            .map(|(source, id, version)| {
                (
                    ((*source).to_owned(), stored_key(source, id)),
                    listed(id, *version),
                )
            })
            .collect()
    }

    #[test]
    fn keys_fold_case_where_the_manager_does() {
        assert_eq!(stored_key("winget", "Git.Git"), "git.git");
        assert_eq!(stored_key("homebrew_cask", "Firefox"), "firefox");
        assert_eq!(
            stored_key("flatpak", "Org.Mozilla.Firefox"),
            "Org.Mozilla.Firefox"
        );
        assert_eq!(stored_key("unknown", "Abc"), "Abc");
    }

    #[test]
    fn a_delta_counts_each_package_a_device_gains_or_loses() {
        let old = set(&[
            ("winget", "Git.Git", Some("2.1")),
            ("winget", "7zip.7zip", Some("24")),
            ("flatpak", "org.gimp.GIMP", Some("3.0")),
        ]);
        let new = set(&[
            // The same package in another case: unchanged.
            ("winget", "git.git", Some("2.1")),
            // A newer version offered: no device more, the version changes.
            ("winget", "7zip.7zip", Some("25")),
            ("winget", "Mozilla.Firefox", Some("131")),
        ]);
        let delta = Delta::between(&old, &new);
        let changes = delta
            .changes
            .iter()
            .map(|change| {
                (
                    change.key.1.as_str(),
                    change.devices,
                    change.listed.available_version.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            changes,
            [
                ("org.gimp.GIMP", -1, None),
                ("7zip.7zip", 0, Some("25")),
                ("mozilla.firefox", 1, Some("131")),
            ]
        );
        assert!(Delta::between(&new, &new).is_empty());
    }
}
