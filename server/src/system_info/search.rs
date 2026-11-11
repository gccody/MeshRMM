//! Searching devices by their snapshots, and the fleet's applications, a
//! page at a time. Filters, counts and pages are the database's work, and
//! no read groups every device's applications: a device's count is on its
//! snapshot's row, and the whole fleet's applications are in
//! [`super::software`]'s summary.
use std::collections::{BTreeMap, HashMap};

use sea_query::{
    Alias, BinOper, Cond, Expr, ExprTrait, Func, Order, Query, SelectStatement, SimpleExpr,
};
use serde::Serialize;

use super::{SNAPSHOT_COLUMNS, Snapshot, snapshot, software::from_key_text};
use crate::{
    access::CompanySet,
    db::{
        self, Database,
        tables::{
            Agents, DeviceApplications, DeviceIpAddresses, DeviceSystemInfo, SoftwareNames,
            SoftwareVersions,
        },
    },
    targets::{TagMatch, tagged},
};

/// The longest search text.
pub const MAX_SEARCH_CHARS: usize = 200;

/// Devices in one company, or in none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanyFilter {
    Company(String),
    None,
}

/// Which devices a fleet-wide search looks at: those in a company, or in
/// none, and those with a tag, among those in the companies the
/// searcher reaches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub company: Option<CompanyFilter>,
    pub tag: Option<String>,
    /// The companies whose devices the searcher may see.
    pub within: CompanySet,
}

impl Scope {
    fn apply(&self, select: &mut SelectStatement) {
        select.and_where_option(
            self.within
                .company_condition((Agents::Table, Agents::CompanyId)),
        );
        if let Some(company) = &self.company {
            select.and_where(company_matches(company));
        }
        if let Some(tag) = &self.tag {
            select.and_where(
                Expr::col((Agents::Table, Agents::Id))
                    .in_subquery(tagged(std::slice::from_ref(tag), TagMatch::Any)),
            );
        }
    }
}

/// What devices must match; every text matches anywhere, ignoring case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceFilter {
    /// Matches the name, operating system, model, serial number, an IP
    /// address or the host name.
    pub query: Option<String>,
    pub os: Option<String>,
    /// Matches the model or the manufacturer.
    pub model: Option<String>,
    pub serial_number: Option<String>,
    /// Matches an adapter's address or the public address.
    pub ip: Option<String>,
    /// Matches an installed application's name.
    pub application: Option<String>,
    pub scope: Scope,
}

/// A `LIKE` pattern that matches `text` anywhere, with its wildcards
/// escaped by `\`.
fn contains(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// `lower(column) LIKE lower('%text%')`. Each database folds both sides the
/// same way, so SQLite, whose `lower` knows only ASCII letters, still
/// matches other letters in their exact case.
pub(crate) fn column_contains(column: impl sea_query::IntoColumnRef, text: &str) -> SimpleExpr {
    expr_contains(Expr::col(column), text)
}

/// [`column_contains`] for any text expression.
fn expr_contains(expr: SimpleExpr, text: &str) -> SimpleExpr {
    let pattern = Expr::Binary(
        Box::new(Func::lower(Expr::val(contains(text))).into()),
        BinOper::Escape,
        Box::new(Expr::Constant('\\'.into())),
    );
    Expr::expr(Func::lower(expr)).binary(BinOper::Like, pattern)
}

fn info_column(column: DeviceSystemInfo) -> (DeviceSystemInfo, DeviceSystemInfo) {
    (DeviceSystemInfo::Table, column)
}

fn os_matches(text: &str) -> Cond {
    Cond::any()
        .add(column_contains(info_column(DeviceSystemInfo::OsName), text))
        .add(column_contains(
            info_column(DeviceSystemInfo::OsVersion),
            text,
        ))
        .add(column_contains(
            info_column(DeviceSystemInfo::OsBuild),
            text,
        ))
        .add(column_contains(info_column(DeviceSystemInfo::OsId), text))
}

fn model_matches(text: &str) -> Cond {
    Cond::any()
        .add(column_contains(info_column(DeviceSystemInfo::Model), text))
        .add(column_contains(
            info_column(DeviceSystemInfo::Manufacturer),
            text,
        ))
}

fn ip_matches(text: &str) -> Cond {
    Cond::any()
        .add(column_contains(
            info_column(DeviceSystemInfo::PublicIp),
            text,
        ))
        .add(Expr::exists(
            Query::select()
                .expr(Expr::val(1))
                .from(DeviceIpAddresses::Table)
                .and_where(
                    Expr::col((DeviceIpAddresses::Table, DeviceIpAddresses::DeviceId))
                        .equals(info_column(DeviceSystemInfo::DeviceId)),
                )
                .and_where(column_contains(
                    (DeviceIpAddresses::Table, DeviceIpAddresses::Address),
                    text,
                ))
                .to_owned(),
        ))
}

/// An installed application's name contains `text`. The names come from the
/// fleet's summary, which has every name in `device_applications`, so only
/// the rows of matching names are read, by their index.
fn application_matches(text: &str) -> SimpleExpr {
    Expr::col(info_column(DeviceSystemInfo::DeviceId)).in_subquery(
        Query::select()
            .column((DeviceApplications::Table, DeviceApplications::DeviceId))
            .from(DeviceApplications::Table)
            .and_where(
                Expr::col((DeviceApplications::Table, DeviceApplications::Name)).in_subquery(
                    Query::select()
                        .column(SoftwareNames::Name)
                        .from(SoftwareNames::Table)
                        .and_where(column_contains(
                            (SoftwareNames::Table, SoftwareNames::Name),
                            text,
                        ))
                        .to_owned(),
                ),
            )
            .to_owned(),
    )
}

fn company_matches(company: &CompanyFilter) -> SimpleExpr {
    let column = Expr::col((Agents::Table, Agents::CompanyId));
    match company {
        CompanyFilter::Company(id) => column.eq(id.as_str()),
        CompanyFilter::None => column.is_null(),
    }
}

/// A device that matched, with its snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceMatch {
    pub device_name: String,
    pub platform: String,
    pub company_id: Option<String>,
    #[serde(flatten)]
    pub snapshot: Snapshot,
}

/// A page of the devices a filter matches, and how many match in all.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DevicePage {
    pub devices: Vec<DeviceMatch>,
    /// Devices the filter matches, on this page or not.
    pub matching: i64,
}

/// The enrolled devices with a snapshot that match `filter`, without the
/// columns to read.
fn matching_devices(filter: &DeviceFilter) -> SelectStatement {
    let mut select = Query::select();
    select
        .from(DeviceSystemInfo::Table)
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id)).equals(info_column(DeviceSystemInfo::DeviceId)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null());
    if let Some(text) = &filter.query {
        select.cond_where(
            Cond::any()
                .add(column_contains((Agents::Table, Agents::Name), text))
                .add(column_contains(
                    info_column(DeviceSystemInfo::Hostname),
                    text,
                ))
                .add(column_contains(
                    info_column(DeviceSystemInfo::SerialNumber),
                    text,
                ))
                .add(os_matches(text))
                .add(model_matches(text))
                .add(ip_matches(text)),
        );
    }
    if let Some(text) = &filter.os {
        select.cond_where(os_matches(text));
    }
    if let Some(text) = &filter.model {
        select.cond_where(model_matches(text));
    }
    if let Some(text) = &filter.serial_number {
        select.and_where(column_contains(
            info_column(DeviceSystemInfo::SerialNumber),
            text,
        ));
    }
    if let Some(text) = &filter.ip {
        select.cond_where(ip_matches(text));
    }
    if let Some(text) = &filter.application {
        select.and_where(application_matches(text));
    }
    filter.scope.apply(&mut select);
    select
}

/// How many devices `filter` matches.
pub fn devices_count_query(filter: &DeviceFilter) -> SelectStatement {
    matching_devices(filter)
        .expr(Func::count(Expr::col(info_column(
            DeviceSystemInfo::DeviceId,
        ))))
        .to_owned()
}

/// The page of `limit` devices from `offset` that `filter` matches, by name
/// then ID. The page's devices are chosen first, from the columns the
/// filter and the order read, and only their snapshots are read: sorting
/// every matching device's snapshot to keep a page of them would read the
/// whole fleet's.
pub fn devices_page_query(filter: &DeviceFilter, limit: u64, offset: u64) -> SelectStatement {
    let page = Alias::new("page");
    let id = Alias::new("id");
    let by_name = || Func::lower(Expr::col((Agents::Table, Agents::Name)));
    let mut ids = matching_devices(filter);
    ids.expr_as(
        Expr::col(info_column(DeviceSystemInfo::DeviceId)),
        id.clone(),
    )
    .order_by_expr(by_name().into(), Order::Asc)
    .order_by((Agents::Table, Agents::Id), Order::Asc)
    .limit(limit)
    .offset(offset);
    Query::select()
        .column((Agents::Table, Agents::Name))
        .column((Agents::Table, Agents::Platform))
        .column((Agents::Table, Agents::CompanyId))
        .columns(SNAPSHOT_COLUMNS.map(info_column))
        .from_subquery(ids, page.clone())
        .inner_join(
            DeviceSystemInfo::Table,
            Expr::col(info_column(DeviceSystemInfo::DeviceId)).equals((page.clone(), id.clone())),
        )
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id)).equals((page, id)),
        )
        .order_by_expr(by_name().into(), Order::Asc)
        .order_by((Agents::Table, Agents::Id), Order::Asc)
        .to_owned()
}

type MatchRow = (
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    Option<String>,
    i64,
    String,
);

/// The page of `limit` enrolled devices with a snapshot from `offset` that
/// match `filter`, by name, and how many match. A `limit` of 0 counts only.
pub async fn devices(
    database: &Database,
    filter: &DeviceFilter,
    limit: u64,
    offset: u64,
) -> db::Result<DevicePage> {
    let (matching,): (i64,) = database.fetch_one(&devices_count_query(filter)).await?;
    if limit == 0 || offset >= u64::try_from(matching).unwrap_or(0) {
        return Ok(DevicePage {
            devices: Vec::new(),
            matching,
        });
    }
    let rows: Vec<MatchRow> = database
        .fetch_all(&devices_page_query(filter, limit, offset))
        .await?;
    let devices = rows
        .into_iter()
        .map(
            |(
                device_name,
                platform,
                company_id,
                device_id,
                collected_at,
                received_at,
                public_ip,
                application_count,
                details,
            )| {
                // A snapshot that can't be read fails the page rather than
                // leaving a matching device out of it unseen.
                let snapshot = snapshot((
                    device_id,
                    collected_at,
                    received_at,
                    public_ip,
                    application_count,
                    details,
                ))
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                Ok(DeviceMatch {
                    device_name,
                    platform,
                    company_id,
                    snapshot,
                })
            },
        )
        .collect::<db::Result<Vec<_>>>()?;
    Ok(DevicePage { devices, matching })
}

/// An application across the fleet, with how many devices have each version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetApplication {
    pub name: String,
    pub publisher: Option<String>,
    /// Devices with any version.
    pub devices: i64,
    /// Most common first.
    pub versions: Vec<FleetVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetVersion {
    pub version: Option<String>,
    pub devices: i64,
}

/// A page of the applications that matched, and how many matched in all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SoftwarePage {
    pub applications: Vec<FleetApplication>,
    /// Applications that matched, on this page or not.
    pub matching: i64,
    /// More matched after this page.
    pub truncated: bool,
}

/// Applications on the enrolled devices in `scope`.
fn fleet_applications(scope: &Scope) -> SelectStatement {
    let mut select = Query::select();
    select
        .from(DeviceApplications::Table)
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((DeviceApplications::Table, DeviceApplications::DeviceId)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null());
    scope.apply(&mut select);
    select
}

/// Whether a search reads the fleet's summary: it can only count the whole
/// fleet, so a company or tag reads the devices' own rows.
fn whole_fleet(scope: &Scope) -> bool {
    scope == &Scope::default()
}

/// How many applications match `query`, or with none how many there are.
pub fn software_count_query(query: Option<&str>, scope: &Scope) -> SelectStatement {
    if whole_fleet(scope) {
        let mut select = Query::select();
        select
            .expr(Func::count(Expr::col(SoftwareNames::Name)))
            .from(SoftwareNames::Table);
        if let Some(text) = query {
            select.and_where(column_contains(
                (SoftwareNames::Table, SoftwareNames::Name),
                text,
            ));
        }
        return select;
    }
    let name = (DeviceApplications::Table, DeviceApplications::Name);
    let mut select = fleet_applications(scope);
    select.expr(Func::count_distinct(Expr::col(name)));
    if let Some(text) = query {
        select.and_where(column_contains(name, text));
    }
    select
}

/// The page of `limit` application names from `offset` with their device
/// counts: those containing `query` by name, or with none the ones on the
/// most devices.
pub fn software_names_query(
    query: Option<&str>,
    scope: &Scope,
    limit: u64,
    offset: u64,
) -> SelectStatement {
    let devices = Alias::new("devices");
    let (mut select, name) = if whole_fleet(scope) {
        let mut select = Query::select();
        select
            .column(SoftwareNames::Name)
            .expr_as(Expr::col(SoftwareNames::Devices), devices.clone())
            .from(SoftwareNames::Table);
        (
            select,
            Expr::col((SoftwareNames::Table, SoftwareNames::Name)),
        )
    } else {
        let name = (DeviceApplications::Table, DeviceApplications::Name);
        let mut select = fleet_applications(scope);
        select
            .column(name)
            .expr_as(
                Func::count_distinct(Expr::col((
                    DeviceApplications::Table,
                    DeviceApplications::DeviceId,
                ))),
                devices.clone(),
            )
            .group_by_col(name);
        (select, Expr::col(name))
    };
    match query {
        Some(text) => {
            select
                .and_where(expr_contains(name.clone(), text))
                .order_by_expr(Func::lower(name.clone()).into(), Order::Asc);
        }
        // The summary's index on (devices, name) gives this order.
        None => {
            select.order_by(devices, Order::Desc);
        }
    }
    select
        .order_by_expr(name, Order::Asc)
        .limit(limit)
        .offset(offset);
    select
}

/// How many devices have each version of each of `names` from each
/// publisher.
pub fn software_versions_query(names: Vec<String>, scope: &Scope) -> SelectStatement {
    if whole_fleet(scope) {
        return Query::select()
            .columns([
                SoftwareVersions::Name,
                SoftwareVersions::Version,
                SoftwareVersions::Publisher,
                SoftwareVersions::Devices,
            ])
            .from(SoftwareVersions::Table)
            .and_where(Expr::col(SoftwareVersions::Name).is_in(names))
            .to_owned();
    }
    let application = |column| (DeviceApplications::Table, column);
    let mut select = fleet_applications(scope);
    select
        .column(application(DeviceApplications::Name))
        .expr(Func::coalesce([
            Expr::col(application(DeviceApplications::Version)),
            Expr::val(""),
        ]))
        .expr(Func::coalesce([
            Expr::col(application(DeviceApplications::Publisher)),
            Expr::val(""),
        ]))
        .expr(Func::count_distinct(Expr::col(application(
            DeviceApplications::DeviceId,
        ))))
        .and_where(Expr::col(application(DeviceApplications::Name)).is_in(names))
        .group_by_col(application(DeviceApplications::Name))
        .group_by_col(application(DeviceApplications::Version))
        .group_by_col(application(DeviceApplications::Publisher));
    select
}

/// A name, version and publisher (`''` for none), and how many devices have
/// them.
type VersionRow = (String, String, String, i64);

/// Each version of an application: how many devices have it, and its
/// publisher.
type Versions = BTreeMap<Option<String>, (i64, Option<String>)>;

/// The applications named in `names`, in their order, from their versions'
/// counts. A version's count adds up its publishers', and its publisher is
/// the greatest one any device names; the application's publisher is that
/// of its most common version that has one.
fn assemble(names: Vec<(String, i64)>, rows: Vec<VersionRow>) -> Vec<FleetApplication> {
    let mut by_name: HashMap<String, Versions> = HashMap::new();
    for (name, version, publisher, devices) in rows {
        let entry = by_name
            .entry(name)
            .or_default()
            .entry(from_key_text(version))
            .or_default();
        entry.0 += devices;
        entry.1 = Ord::max(entry.1.take(), from_key_text(publisher));
    }
    names
        .into_iter()
        .map(|(name, devices)| {
            let mut versions: Vec<_> = by_name
                .remove(&name)
                .unwrap_or_default()
                .into_iter()
                .collect();
            versions.sort_by(
                |(first, (first_devices, _)), (second, (second_devices, _))| {
                    second_devices
                        .cmp(first_devices)
                        .then_with(|| second.cmp(first))
                },
            );
            FleetApplication {
                publisher: versions
                    .iter()
                    .find_map(|(_, (_, publisher))| publisher.clone()),
                versions: versions
                    .into_iter()
                    .map(|(version, (devices, _))| FleetVersion { version, devices })
                    .collect(),
                name,
                devices,
            }
        })
        .collect()
}

/// The page of `limit` applications from `offset` whose name contains
/// `query`, by name, or with no query the ones on the most devices, and how
/// many match. On the whole fleet they are read from the summary; in a
/// company or with a tag, from those devices' own rows.
pub async fn software(
    database: &Database,
    query: Option<&str>,
    scope: &Scope,
    limit: u64,
    offset: u64,
) -> db::Result<SoftwarePage> {
    let (matching,): (i64,) = database
        .fetch_one(&software_count_query(query, scope))
        .await?;
    let names: Vec<(String, i64)> = if limit == 0 || offset >= u64::try_from(matching).unwrap_or(0)
    {
        Vec::new()
    } else {
        database
            .fetch_all(&software_names_query(query, scope, limit, offset))
            .await?
    };
    let truncated =
        offset.saturating_add(names.len() as u64) < u64::try_from(matching).unwrap_or(0);
    if names.is_empty() {
        return Ok(SoftwarePage {
            applications: Vec::new(),
            matching,
            truncated,
        });
    }
    let rows: Vec<VersionRow> = database
        .fetch_all(&software_versions_query(
            names.iter().map(|(name, _)| name.clone()).collect(),
            scope,
        ))
        .await?;
    Ok(SoftwarePage {
        applications: assemble(names, rows),
        matching,
        truncated,
    })
}

/// A device that has an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct InstalledOn {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub company_id: Option<String>,
    pub version: Option<String>,
    pub publisher: Option<String>,
    pub architecture: Option<String>,
    pub installed_at: Option<i64>,
    pub source: String,
}

/// A page of the devices that have an application, and how many copies of
/// it match in all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledPage {
    /// One entry per copy: a device with two versions has two.
    pub devices: Vec<InstalledOn>,
    /// Copies that match, on this page or not.
    pub matching: i64,
}

/// The copies of the application named exactly `name`, in `version` if
/// given (`Some(None)` for no version), on enrolled devices in `scope`.
fn copies(name: &str, version: Option<Option<&str>>, scope: &Scope) -> SelectStatement {
    let application = |column| (DeviceApplications::Table, column);
    let mut select = fleet_applications(scope);
    select.and_where(Expr::col(application(DeviceApplications::Name)).eq(name));
    match version {
        Some(Some(version)) => {
            select.and_where(Expr::col(application(DeviceApplications::Version)).eq(version));
        }
        Some(None) => {
            select.and_where(Expr::col(application(DeviceApplications::Version)).is_null());
        }
        None => {}
    }
    select
}

/// How many copies [`installed_on`] finds.
pub fn installed_count_query(
    name: &str,
    version: Option<Option<&str>>,
    scope: &Scope,
) -> SelectStatement {
    copies(name, version, scope)
        .expr(Func::count(Expr::col((
            DeviceApplications::Table,
            DeviceApplications::DeviceId,
        ))))
        .to_owned()
}

/// The page of `limit` copies from `offset`, by device name, then each
/// device's copies in one order, so pages neither overlap nor skip one.
pub fn installed_page_query(
    name: &str,
    version: Option<Option<&str>>,
    scope: &Scope,
    limit: u64,
    offset: u64,
) -> SelectStatement {
    let application = |column| (DeviceApplications::Table, column);
    copies(name, version, scope)
        .expr_as(
            Expr::col(application(DeviceApplications::DeviceId)),
            Alias::new("device_id"),
        )
        .expr_as(
            Expr::col((Agents::Table, Agents::Name)),
            Alias::new("device_name"),
        )
        .column((Agents::Table, Agents::Platform))
        .column((Agents::Table, Agents::CompanyId))
        .column(application(DeviceApplications::Version))
        .column(application(DeviceApplications::Publisher))
        .column(application(DeviceApplications::Architecture))
        .column(application(DeviceApplications::InstalledAt))
        .column(application(DeviceApplications::Source))
        .order_by_expr(
            Func::lower(Expr::col((Agents::Table, Agents::Name))).into(),
            Order::Asc,
        )
        .order_by((Agents::Table, Agents::Id), Order::Asc)
        .order_by(application(DeviceApplications::Version), Order::Asc)
        .order_by(application(DeviceApplications::Source), Order::Asc)
        .order_by(application(DeviceApplications::Architecture), Order::Asc)
        .order_by(application(DeviceApplications::Publisher), Order::Asc)
        .order_by(application(DeviceApplications::InstalledAt), Order::Asc)
        .limit(limit)
        .offset(offset)
        .to_owned()
}

/// The page of `limit` copies from `offset` of the application named
/// exactly `name`, in `version` if given (`Some(None)` for no version), by
/// device name, and how many there are. A `limit` of 0 counts only.
pub async fn installed_on(
    database: &Database,
    name: &str,
    version: Option<Option<&str>>,
    scope: &Scope,
    limit: u64,
    offset: u64,
) -> db::Result<InstalledPage> {
    let (matching,): (i64,) = database
        .fetch_one(&installed_count_query(name, version, scope))
        .await?;
    let devices = if limit == 0 || offset >= u64::try_from(matching).unwrap_or(0) {
        Vec::new()
    } else {
        database
            .fetch_all(&installed_page_query(name, version, scope, limit, offset))
            .await?
    };
    Ok(InstalledPage { devices, matching })
}

#[cfg(test)]
mod tests {
    use sea_query::{PostgresQueryBuilder, SqliteQueryBuilder};

    use super::*;

    #[test]
    fn search_text_is_matched_literally() {
        assert_eq!(contains("50%_off\\"), "%50\\%\\_off\\\\%");
        let select = Query::select()
            .expr(Expr::val(1))
            .and_where(column_contains(DeviceSystemInfo::Model, "Ab"))
            .to_owned();
        for sql in [
            select.to_string(SqliteQueryBuilder),
            select.to_string(PostgresQueryBuilder),
        ] {
            assert!(
                sql.contains(r#"LOWER("model") LIKE LOWER('%Ab%') ESCAPE "#),
                "{sql}"
            );
        }
    }
}
