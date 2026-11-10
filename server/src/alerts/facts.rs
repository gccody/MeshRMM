//! What update and application rules judge about each device, kept between
//! evaluations: its pending updates and when each was first seen, since
//! when it must restart, and its outdated packages. These change only when
//! the device reports a scan, so an evaluation reads them again only for the
//! devices that reported since the last one ([`Changes::devices`]), and for
//! every device when the engine starts, after a rule that judges them was
//! saved or deleted, and every [`super::FACTS_REFRESH_INTERVAL`]. Ages are
//! judged against the evaluation's time, which needs no read.
//!
//! A device whose scans the engine keeps costs its ID and, for update rules,
//! 16 bytes for each pending update; for application rules, each outdated
//! package's key and name. Nothing is kept for devices that never scanned,
//! or for either kind while no enabled rule judges it.
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use meshrmm_protocol_types::UpdateCategory;

use super::{FACTS_REFRESH_INTERVAL, evaluate::UpdateFacts};
use crate::{
    applications::store::OutdatedPackage,
    db::{self, Executor},
};

/// Devices whose facts one statement reads: well within both databases'
/// bind parameter limits.
const CHUNK: usize = 500;

/// What the engine was told since an evaluation last took this.
#[derive(Debug, Default)]
pub(super) struct Changes {
    /// Devices that reported something update or application rules judge.
    pub devices: HashSet<String>,
    /// Devices deleted: their facts go.
    pub removed: HashSet<String>,
    /// A rule that judges updates or restarts was saved or deleted.
    pub update_rules: bool,
    /// A rule that judges outdated applications was saved or deleted.
    pub application_rules: bool,
}

/// What a scanned device's update scans say.
#[derive(Debug, Default)]
struct DeviceUpdates {
    pending: Vec<(UpdateCategory, i64)>,
    reboot_pending_since: Option<i64>,
}

/// Facts of one kind, by device, and when they were all last read.
#[derive(Debug)]
struct Kept<T> {
    by_device: HashMap<String, T>,
    read_at: Instant,
}

impl<T> Kept<T> {
    /// Whether every device's facts should be read again.
    fn stale(kept: &Option<Self>, rules_changed: bool) -> bool {
        rules_changed
            || kept
                .as_ref()
                .is_none_or(|kept| kept.read_at.elapsed() >= FACTS_REFRESH_INTERVAL)
    }

    /// Replaces the `devices`' facts with those `read`: a device missing from
    /// it has none now.
    fn replace(&mut self, devices: &[String], mut read: HashMap<String, T>) {
        for id in devices {
            if let Some(facts) = read.remove(id) {
                self.by_device.insert(id.clone(), facts);
            } else {
                self.by_device.remove(id);
            }
        }
    }
}

/// The facts the engine keeps. Only the evaluation, one at a time, uses it.
#[derive(Debug, Default)]
pub(super) struct Facts {
    updates: Option<Kept<DeviceUpdates>>,
    applications: Option<Kept<Vec<OutdatedPackage>>>,
}

impl Facts {
    /// Brings the facts up to date for an evaluation after `changes`, for
    /// the kinds enabled rules judge, of the `active` devices. Returns how
    /// many rows it read.
    pub async fn refresh(
        &mut self,
        executor: &mut impl Executor,
        changes: &Changes,
        active: &HashSet<&str>,
        judges_updates: bool,
        judges_applications: bool,
    ) -> db::Result<u64> {
        for id in &changes.removed {
            self.forget(id);
        }
        let reported = changes
            .devices
            .iter()
            .filter(|id| active.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let mut rows = 0;
        if !judges_updates {
            self.updates = None;
        } else if Kept::stale(&self.updates, changes.update_rules) {
            let (mut read, count) = read_updates(executor, None).await?;
            read.retain(|id, _| active.contains(id.as_str()));
            rows += count;
            self.updates = Some(Kept {
                by_device: read,
                read_at: Instant::now(),
            });
        } else if let Some(kept) = &mut self.updates {
            for chunk in reported.chunks(CHUNK) {
                let (read, count) = read_updates(executor, Some(chunk)).await?;
                rows += count;
                kept.replace(chunk, read);
            }
        }
        if !judges_applications {
            self.applications = None;
        } else if Kept::stale(&self.applications, changes.application_rules) {
            let mut read = crate::applications::store::outdated_packages(executor, None).await?;
            read.retain(|id, _| active.contains(id.as_str()));
            rows += application_rows(&read);
            self.applications = Some(Kept {
                by_device: read,
                read_at: Instant::now(),
            });
        } else if let Some(kept) = &mut self.applications {
            for chunk in reported.chunks(CHUNK) {
                let read =
                    crate::applications::store::outdated_packages(executor, Some(chunk)).await?;
                rows += application_rows(&read);
                kept.replace(chunk, read);
            }
        }
        Ok(rows)
    }

    /// What the device's update scans say; `None` if it never reported one.
    pub fn updates(&self, device_id: &str) -> Option<UpdateFacts<'_>> {
        let updates = self.updates.as_ref()?.by_device.get(device_id)?;
        Some(UpdateFacts {
            pending: &updates.pending,
            reboot_pending_since: updates.reboot_pending_since,
        })
    }

    /// The device's outdated packages; `None` if it never scanned them.
    pub fn applications(&self, device_id: &str) -> Option<&[OutdatedPackage]> {
        self.applications
            .as_ref()?
            .by_device
            .get(device_id)
            .map(Vec::as_slice)
    }

    fn forget(&mut self, device_id: &str) {
        if let Some(kept) = &mut self.updates {
            kept.by_device.remove(device_id);
        }
        if let Some(kept) = &mut self.applications {
            kept.by_device.remove(device_id);
        }
    }

    /// How many devices' update and application facts are kept.
    pub fn kept(&self) -> (usize, usize) {
        (
            self.updates.as_ref().map_or(0, |kept| kept.by_device.len()),
            self.applications
                .as_ref()
                .map_or(0, |kept| kept.by_device.len()),
        )
    }
}

/// The scanned devices' update facts, of the `only` devices or of every
/// device, and how many rows that read.
async fn read_updates(
    executor: &mut impl Executor,
    only: Option<&[String]>,
) -> db::Result<(HashMap<String, DeviceUpdates>, u64)> {
    let mut pending = crate::updates::store::pending_ages(executor, only).await?;
    let mut reboots = crate::updates::store::reboots_pending(executor, only).await?;
    let scanned = crate::updates::store::scanned_devices(executor, only).await?;
    let rows = pending.values().map(Vec::len).sum::<usize>() + reboots.len() + scanned.len();
    let updates = scanned
        .into_iter()
        .map(|id| {
            let updates = DeviceUpdates {
                pending: pending.remove(&id).unwrap_or_default(),
                reboot_pending_since: reboots.remove(&id),
            };
            (id, updates)
        })
        .collect();
    Ok((updates, rows as u64))
}

fn application_rows(read: &HashMap<String, Vec<OutdatedPackage>>) -> u64 {
    (read.len() + read.values().map(Vec::len).sum::<usize>()) as u64
}
