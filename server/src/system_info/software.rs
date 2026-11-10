//! How many devices have each application across the fleet, kept in
//! `software_names` and `software_versions` so `GET /v1/software` reads a
//! small summary instead of grouping every device's list.
//!
//! The summary changes in the transaction that changes a device's rows in
//! `device_applications`, by the difference between the rows it had and the
//! rows it gets: when a new list replaces the stored one
//! ([`super::store`]) and when the device's rows are removed
//! ([`super::forget`], which deleting a device runs). Nothing else writes
//! `device_applications`, so the summary is always the grouping of its rows:
//! `software_names` counts the devices with any version of each name, and
//! `software_versions` the devices with each name, version and publisher,
//! `''` standing for none. A row whose count reaches 0 is removed.
//!
//! An application on every device has a row every changed list of every
//! device may touch. Each list changes only the counts its difference names,
//! one statement per table, and only when the list changed, which is seldom:
//! a snapshot that names an unchanged list touches nothing. On PostgreSQL
//! two devices whose lists add or remove the same application wait for each
//! other's row lock until the first commits, which the short store
//! transaction bounds; each transaction changes its rows in the same order
//! (by key, names before versions), so they wait rather than deadlock. SQLite
//! has one writer, so stores are serialized anyway.
use std::collections::BTreeSet;

use sea_query::{Expr, ExprTrait, OnConflict, Query, SimpleExpr};

use super::{INSERT_CHUNK, changes::StoredApplication};
use crate::db::{
    self, Executor,
    tables::{SoftwareNames, SoftwareVersions},
};

/// A version or publisher as the summary keys it: `''` for none, which a
/// sanitized snapshot never holds as text.
pub(crate) fn key_text(text: Option<&str>) -> &str {
    text.unwrap_or("")
}

/// `''` back to none.
pub(crate) fn from_key_text(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

type VersionKey = (String, String, String);

/// What one device's change does to the summary: each name and each
/// version it starts (+1) or stops (-1) having, in key order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Delta {
    names: Vec<(String, i64)>,
    versions: Vec<(VersionKey, i64)>,
}

/// +1 for each key `after` has and `before` hasn't, -1 for the reverse, by
/// key.
fn difference<K: Ord + Clone>(before: &BTreeSet<K>, after: &BTreeSet<K>) -> Vec<(K, i64)> {
    let mut changes: Vec<(K, i64)> = after
        .difference(before)
        .map(|key| (key.clone(), 1))
        .chain(before.difference(after).map(|key| (key.clone(), -1)))
        .collect();
    changes.sort();
    changes
}

impl Delta {
    /// What replacing a device's rows `old` with `new` does. A device counts
    /// once for a name or version however many rows it has of it, such as
    /// one version from two sources.
    pub(crate) fn between(old: &[StoredApplication], new: &[StoredApplication]) -> Self {
        let names = |list: &[StoredApplication]| {
            list.iter()
                .map(|application| application.name.clone())
                .collect::<BTreeSet<_>>()
        };
        let versions = |list: &[StoredApplication]| {
            list.iter()
                .map(|application| {
                    (
                        application.name.clone(),
                        key_text(application.version.as_deref()).to_owned(),
                        key_text(application.publisher.as_deref()).to_owned(),
                    )
                })
                .collect::<BTreeSet<_>>()
        };
        Self {
            names: difference(&names(old), &names(new)),
            versions: difference(&versions(old), &versions(new)),
        }
    }
}

/// `table.devices + excluded.devices`: the change added to the stored count.
fn added(table: &str) -> SimpleExpr {
    Expr::cust(format!("{table}.devices + excluded.devices"))
}

/// Applies a device's change to the summary, inside the transaction that
/// changes its rows.
pub(crate) async fn adjust(executor: &mut impl Executor, delta: &Delta) -> db::Result<()> {
    // A count that a removal brings to 0 is stored for a moment and then
    // removed, so no row outlives its last device.
    for chunk in delta.names.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert
            .into_table(SoftwareNames::Table)
            .columns([SoftwareNames::Name, SoftwareNames::Devices]);
        for (name, change) in chunk {
            insert.values_panic([name.as_str().into(), (*change).into()]);
        }
        insert.on_conflict(
            OnConflict::column(SoftwareNames::Name)
                .value(SoftwareNames::Devices, added("software_names"))
                .to_owned(),
        );
        executor.execute(&insert).await?;
    }
    for chunk in delta.versions.chunks(INSERT_CHUNK) {
        let mut insert = Query::insert();
        insert.into_table(SoftwareVersions::Table).columns([
            SoftwareVersions::Name,
            SoftwareVersions::Version,
            SoftwareVersions::Publisher,
            SoftwareVersions::Devices,
        ]);
        for ((name, version, publisher), change) in chunk {
            insert.values_panic([
                name.as_str().into(),
                version.as_str().into(),
                publisher.as_str().into(),
                (*change).into(),
            ]);
        }
        insert.on_conflict(
            OnConflict::columns([
                SoftwareVersions::Name,
                SoftwareVersions::Version,
                SoftwareVersions::Publisher,
            ])
            .value(SoftwareVersions::Devices, added("software_versions"))
            .to_owned(),
        );
        executor.execute(&insert).await?;
    }
    // Only the names a removal touched can have reached 0. `<= 0` also
    // removes a count a removal took below 0, so none is ever kept.
    let removed_names: Vec<&str> = delta
        .names
        .iter()
        .filter(|(_, change)| *change < 0)
        .map(|(name, _)| name.as_str())
        .collect();
    for chunk in removed_names.chunks(INSERT_CHUNK) {
        executor
            .execute(
                &Query::delete()
                    .from_table(SoftwareNames::Table)
                    .and_where(Expr::col(SoftwareNames::Name).is_in(chunk.iter().copied()))
                    .and_where(Expr::col(SoftwareNames::Devices).lte(0))
                    .to_owned(),
            )
            .await?;
    }
    let mut removed_versions: Vec<&str> = delta
        .versions
        .iter()
        .filter(|(_, change)| *change < 0)
        .map(|((name, _, _), _)| name.as_str())
        .collect();
    removed_versions.dedup();
    for chunk in removed_versions.chunks(INSERT_CHUNK) {
        executor
            .execute(
                &Query::delete()
                    .from_table(SoftwareVersions::Table)
                    .and_where(Expr::col(SoftwareVersions::Name).is_in(chunk.iter().copied()))
                    .and_where(Expr::col(SoftwareVersions::Devices).lte(0))
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, version: Option<&str>, publisher: Option<&str>) -> StoredApplication {
        StoredApplication {
            name: name.into(),
            version: version.map(str::to_owned),
            publisher: publisher.map(str::to_owned),
            architecture: None,
            installed_at: None,
            source: "windows".into(),
        }
    }

    fn key(name: &str, version: &str, publisher: &str) -> VersionKey {
        (name.into(), version.into(), publisher.into())
    }

    #[test]
    fn a_change_counts_what_the_device_starts_and_stops_having() {
        let old = [
            app("7-Zip", Some("23.01"), Some("Igor Pavlov")),
            app("Firefox", Some("130.0"), Some("Mozilla")),
            app("Notepad++", None, None),
        ];
        let new = [
            app("7-Zip", Some("24.08"), Some("Igor Pavlov")),
            app("Firefox", Some("130.0"), Some("Mozilla")),
            app("VLC", Some("3.0"), None),
        ];
        let delta = Delta::between(&old, &new);
        assert_eq!(
            delta.names,
            [("Notepad++".to_owned(), -1), ("VLC".to_owned(), 1)]
        );
        assert_eq!(
            delta.versions,
            [
                (key("7-Zip", "23.01", "Igor Pavlov"), -1),
                (key("7-Zip", "24.08", "Igor Pavlov"), 1),
                (key("Notepad++", "", ""), -1),
                (key("VLC", "3.0", ""), 1),
            ]
        );
        assert_eq!(Delta::between(&new, &new), Delta::default());
    }

    #[test]
    fn a_device_counts_once_however_many_rows_it_has() {
        let mut user = app("Runtime", Some("1"), None);
        user.source = "windows_user".into();
        let both = [app("Runtime", Some("1"), None), user];
        let delta = Delta::between(&[], &both);
        assert_eq!(delta.names, [("Runtime".to_owned(), 1)]);
        assert_eq!(delta.versions, [(key("Runtime", "1", ""), 1)]);
        // Losing one of the two rows keeps the device counted.
        assert_eq!(Delta::between(&both, &both[..1]), Delta::default());
        assert_eq!(
            Delta::between(&both, &[]).names,
            [("Runtime".to_owned(), -1)]
        );
    }
}
