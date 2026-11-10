//! Devices' hardware and software, from the snapshots their Agents send on
//! the control connection when they start, every few hours, and when a user
//! asks for a refresh.
//!
//! Each device keeps only its latest snapshot: the fields the website and the
//! API search on as columns of `device_system_info`, its IP addresses in
//! `device_ip_addresses` and its applications in `device_applications`, and
//! the rest as JSON. Each new list of applications is compared with the last
//! one, and what was installed, removed or updated is kept in
//! `device_application_changes` for [`CHANGES_RETENTION_MS`]. The public
//! address a device connects from is recorded whenever its Agent connects.
//!
//! The server tells each Agent which list it holds, by
//! [`applications_hash`], when it connects and after storing a list, and the
//! Agent leaves an unchanged list out of its snapshots. The server keeps its
//! list for such a snapshot only if the hash it names is the one stored, and
//! otherwise asks for the list ([`Stored::Unknown`]): a disagreement can cost
//! a whole list sent, never a wrong list kept.
pub mod changes;
pub mod search;

use std::{collections::HashMap, net::IpAddr};

use meshrmm_protocol_types::{
    Hardware, Network, OperatingSystem, SYSTEM_INFO_INTERVAL_SECONDS, SignedInUser, SystemInfo,
    applications_hash,
};
use sea_query::{Expr, ExprTrait, Func, LockType, OnConflict, Order, Query};
use serde::Serialize;

use self::changes::{Change, StoredApplication};
use crate::{
    agents,
    db::{
        self, Database, Executor,
        tables::{
            DeviceApplicationChanges, DeviceApplications, DeviceIpAddresses, DeviceSystemInfo,
        },
    },
    time::{DAY_MS, HOUR_MS, SECOND_MS, now_ms},
    users::new_id,
};

/// A snapshot older than this, by when the server received it, is replaced
/// when the Agent next connects: the Agent sends one every
/// [`SYSTEM_INFO_INTERVAL_SECONDS`], so it missed one.
pub const STALE_AFTER_MS: i64 = SYSTEM_INFO_INTERVAL_SECONDS as i64 * SECOND_MS + HOUR_MS;
/// How long application changes are kept.
pub const CHANGES_RETENTION_MS: i64 = 90 * DAY_MS;
/// Rows per insert, well within both databases' bind parameter limits.
const INSERT_CHUNK: usize = 500;

/// What the device list shows and searches for each device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The operating system's name and version.
    pub os: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub serial_number: Option<String>,
    /// The adapters' addresses, without loopback and link-local ones.
    pub ip_addresses: Vec<String>,
    pub public_ip: Option<String>,
    /// When the server received the snapshot.
    pub received_at: i64,
}

/// A device's latest snapshot, as the API returns it. Its applications are
/// listed separately.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub device_id: String,
    /// When the Agent collected it, by the device's clock.
    pub collected_at: i64,
    /// When the server received it.
    pub received_at: i64,
    /// The address the device's Agent last connected from.
    pub public_ip: Option<String>,
    pub application_count: i64,
    pub os: OperatingSystem,
    pub hardware: Hardware,
    pub network: Network,
    pub users: Vec<SignedInUser>,
    /// The parts the Agent could not collect.
    pub problems: Vec<String>,
}

/// An application change, as the API returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct ChangeRecord {
    pub observed_at: i64,
    pub change: String,
    pub name: String,
    pub version: Option<String>,
    pub previous_version: Option<String>,
    pub source: String,
}

/// What became of a snapshot's applications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// The snapshot listed them, and the device now has that list, with
    /// this hash.
    Listed { applications_hash: String },
    /// The snapshot said they are the list the device has, which it keeps.
    Unchanged,
    /// The snapshot said they are a list the device doesn't have (after its
    /// rows were removed, say): it keeps the one it has, with this hash, or
    /// none, until the Agent sends its list.
    Unknown { applications_hash: Option<String> },
}

/// What a newly connected Agent's device has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    /// Its snapshot is missing or older than [`STALE_AFTER_MS`], so the Agent
    /// should send a new one.
    pub stale: bool,
    /// The hash of its application list, if it has one.
    pub applications_hash: Option<String>,
}

/// Text as PostgreSQL can store it: it can't hold NUL. Snapshots are
/// sanitized, which removes control characters, so this is a safeguard.
fn storable(text: &str) -> String {
    text.replace('\0', "")
}

fn storable_option(text: &Option<String>) -> Option<String> {
    text.as_deref().map(storable)
}

/// Stores a device's new snapshot, received on a connection from
/// `public_ip`, replacing its last one. `applications_unchanged` is the hash
/// the snapshot named instead of listing its applications, which only keeps
/// the stored list if it is that list's. Returns `None`, storing nothing, if
/// the device is no longer enrolled.
pub async fn store(
    database: &Database,
    device_id: &str,
    public_ip: IpAddr,
    info: SystemInfo,
    applications_unchanged: Option<String>,
) -> db::Result<Option<Stored>> {
    let mut info = info.sanitized();
    // A snapshot that names its list lists nothing to keep.
    let applications = std::mem::take(&mut info.applications);
    let listed = applications_unchanged
        .is_none()
        .then(|| applications_hash(&applications));
    let now = now_ms();
    let mut transaction = database.begin().await?;
    if !agents::is_active(&mut transaction, device_id).await? {
        return Ok(None);
    }
    let previous: Option<(Option<String>,)> = transaction
        .fetch_optional(
            &Query::select()
                .column(DeviceSystemInfo::ApplicationsHash)
                .from(DeviceSystemInfo::Table)
                .and_where(Expr::col(DeviceSystemInfo::DeviceId).eq(device_id))
                // Another store for the device waits until this one
                // commits, so it compares with this one's list. SQLite's
                // write transactions are serialized anyway.
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let details = storable(&serde_json::to_string(&info).expect("snapshots serialize"));
    let columns = [
        DeviceSystemInfo::DeviceId,
        DeviceSystemInfo::CollectedAt,
        DeviceSystemInfo::ReceivedAt,
        DeviceSystemInfo::PublicIp,
        DeviceSystemInfo::OsId,
        DeviceSystemInfo::OsName,
        DeviceSystemInfo::OsVersion,
        DeviceSystemInfo::OsBuild,
        DeviceSystemInfo::Architecture,
        DeviceSystemInfo::Manufacturer,
        DeviceSystemInfo::Model,
        DeviceSystemInfo::SerialNumber,
        DeviceSystemInfo::Hostname,
        DeviceSystemInfo::ApplicationsHash,
        DeviceSystemInfo::Details,
    ];
    transaction
        .execute(
            &Query::insert()
                .into_table(DeviceSystemInfo::Table)
                .columns(columns)
                // A row first stored without a list has none.
                .values_panic([
                    device_id.into(),
                    info.collected_at_unix_ms.into(),
                    now.into(),
                    public_ip.to_string().into(),
                    storable(&info.os.id).into(),
                    storable(&info.os.name).into(),
                    storable(&info.os.version).into(),
                    storable_option(&info.os.build).into(),
                    storable(&info.os.architecture).into(),
                    storable_option(&info.hardware.manufacturer).into(),
                    storable_option(&info.hardware.model).into(),
                    storable_option(&info.hardware.serial_number).into(),
                    storable(&info.network.hostname).into(),
                    listed.clone().into(),
                    details.into(),
                ])
                .on_conflict(
                    OnConflict::column(DeviceSystemInfo::DeviceId)
                        // The hash stays the stored list's unless a list
                        // replaces it.
                        .update_columns(columns.into_iter().skip(1).filter(|column| {
                            listed.is_some()
                                || !matches!(column, DeviceSystemInfo::ApplicationsHash)
                        }))
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    replace_addresses(&mut transaction, device_id, &info).await?;
    let held = previous.and_then(|(hash,)| hash);
    let Some(hash) = listed else {
        // No application work at all: the stored list stays whichever it is.
        // The snapshot named a hash, so a device without a list never
        // matches it.
        transaction.commit().await?;
        return Ok(Some(if held == applications_unchanged {
            Stored::Unchanged
        } else {
            Stored::Unknown {
                applications_hash: held,
            }
        }));
    };
    if held.as_deref() != Some(hash.as_str()) {
        let new: Vec<StoredApplication> = applications
            .into_iter()
            .map(|application| StoredApplication {
                name: storable(&application.name),
                version: storable_option(&application.version),
                publisher: storable_option(&application.publisher),
                architecture: storable_option(&application.architecture),
                installed_at: application.installed_at_unix_ms,
                source: application.source.as_str().to_owned(),
            })
            .collect();
        // A device's first list is where its history starts, not a list of
        // installs.
        if held.is_some() {
            let old = stored_applications(&mut transaction, device_id).await?;
            insert_changes(&mut transaction, device_id, now, &changes::diff(&old, &new)).await?;
        }
        replace_applications(&mut transaction, device_id, &new).await?;
    }
    transaction.commit().await?;
    Ok(Some(Stored::Listed {
        applications_hash: hash,
    }))
}

async fn replace_addresses(
    executor: &mut impl Executor,
    device_id: &str,
    info: &SystemInfo,
) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(DeviceIpAddresses::Table)
                .and_where(Expr::col(DeviceIpAddresses::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for adapter in &info.network.adapters {
        for address in &adapter.addresses {
            if seen.insert((adapter.name.as_str(), address.address.as_str())) {
                rows.push((adapter, address));
            }
        }
    }
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(DeviceIpAddresses::Table).columns([
            DeviceIpAddresses::DeviceId,
            DeviceIpAddresses::Adapter,
            DeviceIpAddresses::Address,
            DeviceIpAddresses::PrefixLength,
            DeviceIpAddresses::MacAddress,
            DeviceIpAddresses::Kind,
            DeviceIpAddresses::Network,
        ]);
        for (adapter, address) in chunk {
            insert.values_panic([
                device_id.into(),
                storable(&adapter.name).into(),
                address.address.as_str().into(),
                i64::from(address.prefix_length).into(),
                adapter.mac_address.clone().into(),
                adapter.kind.as_str().into(),
                crate::wake::relays::network(&address.address, address.prefix_length).into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

async fn stored_applications(
    executor: &mut impl Executor,
    device_id: &str,
) -> db::Result<Vec<StoredApplication>> {
    executor
        .fetch_all(
            &Query::select()
                .columns([
                    DeviceApplications::Name,
                    DeviceApplications::Version,
                    DeviceApplications::Publisher,
                    DeviceApplications::Architecture,
                    DeviceApplications::InstalledAt,
                    DeviceApplications::Source,
                ])
                .from(DeviceApplications::Table)
                .and_where(Expr::col(DeviceApplications::DeviceId).eq(device_id))
                .order_by_expr(
                    Func::lower(Expr::col(DeviceApplications::Name)).into(),
                    Order::Asc,
                )
                .order_by(DeviceApplications::Name, Order::Asc)
                .order_by(DeviceApplications::Version, Order::Asc)
                .to_owned(),
        )
        .await
}

async fn replace_applications(
    executor: &mut impl Executor,
    device_id: &str,
    applications: &[StoredApplication],
) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(DeviceApplications::Table)
                .and_where(Expr::col(DeviceApplications::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    for chunk in applications.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(DeviceApplications::Table).columns([
            DeviceApplications::DeviceId,
            DeviceApplications::Name,
            DeviceApplications::Version,
            DeviceApplications::Publisher,
            DeviceApplications::Architecture,
            DeviceApplications::InstalledAt,
            DeviceApplications::Source,
        ]);
        for application in chunk {
            insert.values_panic([
                device_id.into(),
                application.name.as_str().into(),
                application.version.clone().into(),
                application.publisher.clone().into(),
                application.architecture.clone().into(),
                application.installed_at.into(),
                application.source.as_str().into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

async fn insert_changes(
    executor: &mut impl Executor,
    device_id: &str,
    observed_at: i64,
    changes: &[Change],
) -> db::Result<()> {
    for chunk in changes.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(DeviceApplicationChanges::Table).columns([
            DeviceApplicationChanges::Id,
            DeviceApplicationChanges::DeviceId,
            DeviceApplicationChanges::ObservedAt,
            DeviceApplicationChanges::Change,
            DeviceApplicationChanges::Name,
            DeviceApplicationChanges::Version,
            DeviceApplicationChanges::PreviousVersion,
            DeviceApplicationChanges::Source,
        ]);
        for change in chunk {
            insert.values_panic([
                new_id().into(),
                device_id.into(),
                observed_at.into(),
                change.kind.as_str().into(),
                change.name.as_str().into(),
                change.version.clone().into(),
                change.previous_version.clone().into(),
                change.source.as_str().into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    Ok(())
}

/// Records the address the device's Agent just connected from, and returns
/// what the device has: whether its snapshot is missing or old, and its
/// application list.
pub async fn connected(
    database: &Database,
    device_id: &str,
    public_ip: IpAddr,
) -> db::Result<Connected> {
    let public_ip = public_ip.to_string();
    let stored: Option<(i64, Option<String>, Option<String>)> = database
        .fetch_optional(
            &Query::select()
                .columns([
                    DeviceSystemInfo::ReceivedAt,
                    DeviceSystemInfo::PublicIp,
                    DeviceSystemInfo::ApplicationsHash,
                ])
                .from(DeviceSystemInfo::Table)
                .and_where(Expr::col(DeviceSystemInfo::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    // Agents mostly reconnect from where they were, and writing only a
    // change keeps their connect from taking SQLite's write lock. A snapshot
    // stored meanwhile records the address itself.
    if let Some((_, stored_ip, _)) = &stored
        && stored_ip.as_deref() != Some(public_ip.as_str())
    {
        database
            .execute(
                &Query::update()
                    .table(DeviceSystemInfo::Table)
                    .value(DeviceSystemInfo::PublicIp, public_ip)
                    .and_where(Expr::col(DeviceSystemInfo::DeviceId).eq(device_id))
                    .to_owned(),
            )
            .await?;
    }
    Ok(match stored {
        Some((received, _, applications_hash)) => Connected {
            stale: now_ms() - received > STALE_AFTER_MS,
            applications_hash,
        },
        None => Connected {
            stale: true,
            applications_hash: None,
        },
    })
}

/// Removes a deleted device's snapshot and history.
pub async fn forget(database: &Database, device_id: &str) -> db::Result<()> {
    let mut transaction = database.begin().await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceSystemInfo::Table)
                .and_where(Expr::col(DeviceSystemInfo::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceIpAddresses::Table)
                .and_where(Expr::col(DeviceIpAddresses::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceApplications::Table)
                .and_where(Expr::col(DeviceApplications::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceApplicationChanges::Table)
                .and_where(Expr::col(DeviceApplicationChanges::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    transaction.commit().await
}

/// Whether an address is worth showing in the device list: not loopback,
/// link-local or unspecified.
fn listed_address(address: &str) -> bool {
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => {
            !ip.is_loopback() && !ip.is_unspecified() && !ip.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

type SummaryRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

/// The device list's summary of each device that has a snapshot, or only of
/// `device_id`.
pub async fn summaries(
    database: &Database,
    device_id: Option<&str>,
) -> db::Result<HashMap<String, Summary>> {
    let mut select = Query::select();
    select
        .columns([
            DeviceSystemInfo::DeviceId,
            DeviceSystemInfo::OsName,
            DeviceSystemInfo::OsVersion,
            DeviceSystemInfo::Manufacturer,
            DeviceSystemInfo::Model,
            DeviceSystemInfo::SerialNumber,
            DeviceSystemInfo::PublicIp,
            DeviceSystemInfo::ReceivedAt,
        ])
        .from(DeviceSystemInfo::Table);
    let mut addresses = Query::select();
    addresses
        .columns([DeviceIpAddresses::DeviceId, DeviceIpAddresses::Address])
        .from(DeviceIpAddresses::Table)
        .order_by(DeviceIpAddresses::Adapter, Order::Asc)
        .order_by(DeviceIpAddresses::Address, Order::Asc);
    if let Some(device_id) = device_id {
        select.and_where(Expr::col(DeviceSystemInfo::DeviceId).eq(device_id));
        addresses.and_where(Expr::col(DeviceIpAddresses::DeviceId).eq(device_id));
    }
    let rows: Vec<SummaryRow> = database.fetch_all(&select).await?;
    let addresses: Vec<(String, String)> = database.fetch_all(&addresses).await?;
    let mut summaries: HashMap<String, Summary> = rows
        .into_iter()
        .map(
            |(
                id,
                os_name,
                os_version,
                manufacturer,
                model,
                serial_number,
                public_ip,
                received_at,
            )| {
                let os = format!("{os_name} {os_version}").trim().to_owned();
                (
                    id,
                    Summary {
                        os,
                        manufacturer,
                        model,
                        serial_number,
                        ip_addresses: Vec::new(),
                        public_ip,
                        received_at,
                    },
                )
            },
        )
        .collect();
    for (id, address) in addresses {
        if let Some(summary) = summaries.get_mut(&id)
            && listed_address(&address)
            && !summary.ip_addresses.contains(&address)
        {
            summary.ip_addresses.push(address);
        }
    }
    Ok(summaries)
}

pub(crate) type SnapshotRow = (String, i64, i64, Option<String>, String);

/// The device's latest snapshot, if it has one.
pub async fn load(database: &Database, device_id: &str) -> db::Result<Option<Snapshot>> {
    let row: Option<SnapshotRow> = database
        .fetch_optional(
            &Query::select()
                .columns([
                    DeviceSystemInfo::DeviceId,
                    DeviceSystemInfo::CollectedAt,
                    DeviceSystemInfo::ReceivedAt,
                    DeviceSystemInfo::PublicIp,
                    DeviceSystemInfo::Details,
                ])
                .from(DeviceSystemInfo::Table)
                .and_where(Expr::col(DeviceSystemInfo::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let counts = search::application_counts(database, Some(device_id)).await?;
    Ok(snapshot(row, &counts))
}

/// A stored row as the API returns it, or `None` if its details can't be
/// read, which only a snapshot from a release with another format can cause.
pub(crate) fn snapshot(
    (device_id, collected_at, received_at, public_ip, details): SnapshotRow,
    application_counts: &HashMap<String, i64>,
) -> Option<Snapshot> {
    let info: SystemInfo = match serde_json::from_str(&details) {
        Ok(info) => info,
        Err(error) => {
            tracing::warn!(device_id, %error, "could not read a stored system information snapshot");
            return None;
        }
    };
    Some(Snapshot {
        application_count: application_counts.get(&device_id).copied().unwrap_or(0),
        device_id,
        collected_at,
        received_at,
        public_ip,
        os: info.os,
        hardware: info.hardware,
        network: info.network,
        users: info.users,
        problems: info.problems,
    })
}

/// The device's applications, by name.
pub async fn applications(
    database: &Database,
    device_id: &str,
) -> db::Result<Vec<StoredApplication>> {
    stored_applications(&mut &*database, device_id).await
}

/// The device's application changes, newest first.
pub async fn recent_changes(
    database: &Database,
    device_id: &str,
    limit: u64,
) -> db::Result<Vec<ChangeRecord>> {
    database
        .fetch_all(
            &Query::select()
                .columns([
                    DeviceApplicationChanges::ObservedAt,
                    DeviceApplicationChanges::Change,
                    DeviceApplicationChanges::Name,
                    DeviceApplicationChanges::Version,
                    DeviceApplicationChanges::PreviousVersion,
                    DeviceApplicationChanges::Source,
                ])
                .from(DeviceApplicationChanges::Table)
                .and_where(Expr::col(DeviceApplicationChanges::DeviceId).eq(device_id))
                .order_by(DeviceApplicationChanges::ObservedAt, Order::Desc)
                .order_by(DeviceApplicationChanges::Name, Order::Asc)
                .limit(limit)
                .to_owned(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_list_leaves_out_local_only_addresses() {
        for address in ["10.0.0.5", "2001:db8::5", "fd00::1"] {
            assert!(listed_address(address), "{address}");
        }
        for address in [
            "127.0.0.1",
            "169.254.1.1",
            "::1",
            "fe80::1",
            "0.0.0.0",
            "bad",
        ] {
            assert!(!listed_address(address), "{address}");
        }
    }
}
