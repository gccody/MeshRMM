//! Searching devices by their snapshots, and the fleet's applications.
use std::collections::HashMap;

use sea_query::{
    Alias, BinOper, Cond, Expr, ExprTrait, Func, Order, Query, SelectStatement, SimpleExpr,
};
use serde::Serialize;

use super::{Snapshot, snapshot};
use crate::db::{
    self, Database,
    tables::{Agents, DeviceApplications, DeviceIpAddresses, DeviceSystemInfo},
};

/// The longest search text.
pub const MAX_SEARCH_CHARS: usize = 200;

/// Devices in one company, or in none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanyFilter {
    Company(String),
    None,
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
    pub company: Option<CompanyFilter>,
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
fn column_contains(column: impl sea_query::IntoColumnRef, text: &str) -> SimpleExpr {
    let pattern = Expr::Binary(
        Box::new(Func::lower(Expr::val(contains(text))).into()),
        BinOper::Escape,
        Box::new(Expr::Constant('\\'.into())),
    );
    Expr::expr(Func::lower(Expr::col(column))).binary(BinOper::Like, pattern)
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

fn application_matches(text: &str) -> SimpleExpr {
    Expr::exists(
        Query::select()
            .expr(Expr::val(1))
            .from(DeviceApplications::Table)
            .and_where(
                Expr::col((DeviceApplications::Table, DeviceApplications::DeviceId))
                    .equals(info_column(DeviceSystemInfo::DeviceId)),
            )
            .and_where(column_contains(
                (DeviceApplications::Table, DeviceApplications::Name),
                text,
            ))
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

type MatchRow = (
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    Option<String>,
    String,
);

/// Enrolled devices with a snapshot that match `filter`, by name.
pub async fn devices(database: &Database, filter: &DeviceFilter) -> db::Result<Vec<DeviceMatch>> {
    let mut select = Query::select();
    select
        .column((Agents::Table, Agents::Name))
        .column((Agents::Table, Agents::Platform))
        .column((Agents::Table, Agents::CompanyId))
        .columns([
            info_column(DeviceSystemInfo::DeviceId),
            info_column(DeviceSystemInfo::CollectedAt),
            info_column(DeviceSystemInfo::ReceivedAt),
            info_column(DeviceSystemInfo::PublicIp),
            info_column(DeviceSystemInfo::Details),
        ])
        .from(DeviceSystemInfo::Table)
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id)).equals(info_column(DeviceSystemInfo::DeviceId)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .order_by_expr(
            Func::lower(Expr::col((Agents::Table, Agents::Name))).into(),
            Order::Asc,
        )
        .order_by((Agents::Table, Agents::Id), Order::Asc);
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
    if let Some(company) = &filter.company {
        select.and_where(company_matches(company));
    }
    let rows: Vec<MatchRow> = database.fetch_all(&select).await?;
    let counts = application_counts(database, None).await?;
    Ok(rows
        .into_iter()
        .filter_map(
            |(
                device_name,
                platform,
                company_id,
                device_id,
                collected_at,
                received_at,
                public_ip,
                details,
            )| {
                let snapshot = snapshot(
                    (device_id, collected_at, received_at, public_ip, details),
                    &counts,
                )?;
                Some(DeviceMatch {
                    device_name,
                    platform,
                    company_id,
                    snapshot,
                })
            },
        )
        .collect())
}

/// How many applications each device, or only `device_id`, has.
pub async fn application_counts(
    database: &Database,
    device_id: Option<&str>,
) -> db::Result<HashMap<String, i64>> {
    let mut select = Query::select();
    select
        .column(DeviceApplications::DeviceId)
        .expr(Func::count(Expr::col(DeviceApplications::Name)))
        .from(DeviceApplications::Table)
        .group_by_col(DeviceApplications::DeviceId);
    if let Some(device_id) = device_id {
        select.and_where(Expr::col(DeviceApplications::DeviceId).eq(device_id));
    }
    let rows: Vec<(String, i64)> = database.fetch_all(&select).await?;
    Ok(rows.into_iter().collect())
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

/// Applications on enrolled devices of `company`, or of every company.
fn fleet_applications(company: Option<&CompanyFilter>) -> SelectStatement {
    let mut select = Query::select();
    select
        .from(DeviceApplications::Table)
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((DeviceApplications::Table, DeviceApplications::DeviceId)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null());
    if let Some(company) = company {
        select.and_where(company_matches(company));
    }
    select
}

type VersionRow = (String, Option<String>, Option<String>, i64);

/// The applications whose name contains `query`, by name, or with no query
/// the ones on the most devices; at most `limit` names, and whether there
/// were more.
pub async fn software(
    database: &Database,
    query: Option<&str>,
    company: Option<&CompanyFilter>,
    limit: usize,
) -> db::Result<(Vec<FleetApplication>, bool)> {
    let application = |column| (DeviceApplications::Table, column);
    let devices = Alias::new("devices");
    let mut names = fleet_applications(company);
    names
        .column(application(DeviceApplications::Name))
        .expr_as(
            Func::count_distinct(Expr::col(application(DeviceApplications::DeviceId))),
            devices.clone(),
        )
        .group_by_col(application(DeviceApplications::Name))
        .limit(limit as u64 + 1);
    match query {
        Some(text) => {
            names
                .and_where(column_contains(application(DeviceApplications::Name), text))
                .order_by_expr(
                    Func::lower(Expr::col(application(DeviceApplications::Name))).into(),
                    Order::Asc,
                );
        }
        None => {
            names.order_by(devices, Order::Desc).order_by_expr(
                Func::lower(Expr::col(application(DeviceApplications::Name))).into(),
                Order::Asc,
            );
        }
    }
    names.order_by(application(DeviceApplications::Name), Order::Asc);
    let mut rows: Vec<(String, i64)> = database.fetch_all(&names).await?;
    let truncated = rows.len() > limit;
    rows.truncate(limit);
    if rows.is_empty() {
        return Ok((Vec::new(), truncated));
    }
    let mut versions = fleet_applications(company);
    versions
        .column(application(DeviceApplications::Name))
        .column(application(DeviceApplications::Version))
        .expr(Func::max(Expr::col(application(
            DeviceApplications::Publisher,
        ))))
        .expr(Func::count_distinct(Expr::col(application(
            DeviceApplications::DeviceId,
        ))))
        .and_where(
            Expr::col(application(DeviceApplications::Name))
                .is_in(rows.iter().map(|(name, _)| name.clone())),
        )
        .group_by_col(application(DeviceApplications::Name))
        .group_by_col(application(DeviceApplications::Version));
    let version_rows: Vec<VersionRow> = database.fetch_all(&versions).await?;
    let mut by_name: HashMap<String, Vec<VersionRow>> = HashMap::new();
    for row in version_rows {
        by_name.entry(row.0.clone()).or_default().push(row);
    }
    Ok((
        rows.into_iter()
            .map(|(name, devices)| {
                let mut entries = by_name.remove(&name).unwrap_or_default();
                entries.sort_by(|first, second| {
                    second.3.cmp(&first.3).then_with(|| second.1.cmp(&first.1))
                });
                FleetApplication {
                    publisher: entries.iter().find_map(|entry| entry.2.clone()),
                    versions: entries
                        .into_iter()
                        .map(|(_, version, _, devices)| FleetVersion { version, devices })
                        .collect(),
                    name,
                    devices,
                }
            })
            .collect(),
        truncated,
    ))
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

/// The devices that have the application named exactly `name`, in
/// `version` if given (`Some(None)` for no version), by device name.
pub async fn installed_on(
    database: &Database,
    name: &str,
    version: Option<Option<&str>>,
    company: Option<&CompanyFilter>,
) -> db::Result<Vec<InstalledOn>> {
    let application = |column| (DeviceApplications::Table, column);
    let mut select = fleet_applications(company);
    select
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
        .and_where(Expr::col(application(DeviceApplications::Name)).eq(name))
        .order_by_expr(
            Func::lower(Expr::col((Agents::Table, Agents::Name))).into(),
            Order::Asc,
        )
        .order_by((Agents::Table, Agents::Id), Order::Asc)
        .order_by(application(DeviceApplications::Version), Order::Asc);
    match version {
        Some(Some(version)) => {
            select.and_where(Expr::col(application(DeviceApplications::Version)).eq(version));
        }
        Some(None) => {
            select.and_where(Expr::col(application(DeviceApplications::Version)).is_null());
        }
        None => {}
    }
    database.fetch_all(&select).await
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
