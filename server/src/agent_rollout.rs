//! Which devices the server offers the Agent release it ships.
//!
//! In the default mode, *everyone*, each release is offered to every device
//! as soon as the server ships it. In *staged* mode a release is offered
//! first to the devices whose rollout bucket is below the first-stage
//! percentage, and an administrator raises the percentage by hand, or pauses
//! the rollout, from the website. Each device's bucket is a fixed number from
//! 0 to 99 made from its ID ([`bucket`]), so the stages are nested and the
//! same devices go first for every release.
//!
//! The stage belongs to one release: a server that starts shipping another
//! one starts the rollout again at the first percentage ([`begin_release`]).
//! The Agent asks for its update as itself (`POST
//! /v1/agents/{id}/update-check`), so the server decides; an Agent learns of
//! a change at its next check, about every six hours.
use std::collections::BTreeMap;

use meshrmm_self_update::{MANIFEST_SCHEMA_VERSION, UpdateManifest};
use sea_query::{Expr, ExprTrait, Query};
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    db::{
        self, Executor,
        tables::{Agents, Settings as SettingsTable},
    },
    settings::Settings,
};

pub const EVERYONE: &str = "everyone";
pub const STAGED: &str = "staged";
pub const DEFAULT_FIRST_PERCENT: u8 = 10;

/// The device's place in a staged rollout, 0 to 99: the first eight bytes of
/// the SHA-256 of its ID, big-endian, modulo 100. Fixed for the device, so
/// it is stored when the device enrolls; the release is deliberately not
/// part of it, so the same devices go first every time.
pub fn bucket(device_id: &str) -> u8 {
    let digest = Sha256::digest(device_id.as_bytes());
    let mut first = [0; 8];
    first.copy_from_slice(&digest[..8]);
    // Below 100, so it fits.
    (u64::from_be_bytes(first) % 100) as u8
}

/// The rollout as the settings row holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollout {
    pub staged: bool,
    pub first_percent: u8,
    pub percent: u8,
    pub paused: bool,
}

impl Rollout {
    pub fn from_settings(settings: &Settings) -> Self {
        let percent = |value: i64| u8::try_from(value.clamp(0, 100)).unwrap_or(100);
        Self {
            staged: settings.agent_rollout_mode == STAGED,
            first_percent: percent(settings.agent_rollout_first_percent),
            percent: percent(settings.agent_rollout_percent),
            paused: settings.agent_rollout_paused,
        }
    }

    /// The share of devices, 0 to 100, offered the release now.
    pub fn offered_percent(&self) -> u8 {
        match (self.staged, self.paused) {
            (false, _) => 100,
            (true, true) => 0,
            (true, false) => self.percent,
        }
    }

    /// Whether a device in `bucket` that doesn't have the release yet is
    /// offered it.
    pub fn offers(&self, bucket: u8) -> bool {
        bucket < self.offered_percent()
    }
}

/// Whether an Agent reporting `current` has `release` already, or a newer
/// one. A version that isn't one doesn't.
pub fn has_release(current: Option<&str>, release: &str) -> bool {
    match (
        current.and_then(|current| Version::parse(current).ok()),
        Version::parse(release),
    ) {
        (Some(current), Ok(release)) => current >= release,
        _ => false,
    }
}

/// The manifest that offers an Agent nothing.
pub fn empty_manifest() -> Vec<u8> {
    serde_json::to_vec(&UpdateManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        releases: BTreeMap::new(),
    })
    .expect("an empty manifest serializes")
}

/// Starts the rollout again at the first-stage percentage, not paused, when
/// the release the server ships, `version`, isn't the one the stored stage
/// belongs to. Run once at start-up, before any request is served, and as one
/// conditional update, so it is idempotent. Returns whether it started one.
pub async fn begin_release(executor: &mut impl Executor, version: &str) -> db::Result<bool> {
    let changed = executor
        .execute(
            &Query::update()
                .table(SettingsTable::Table)
                .value(SettingsTable::AgentRolloutVersion, version)
                .value(
                    SettingsTable::AgentRolloutPercent,
                    Expr::col(SettingsTable::AgentRolloutFirstPercent),
                )
                .value(SettingsTable::AgentRolloutPaused, false)
                .and_where(Expr::col(SettingsTable::Id).eq(1))
                .and_where(
                    Expr::col(SettingsTable::AgentRolloutVersion)
                        .is_null()
                        .or(Expr::col(SettingsTable::AgentRolloutVersion).ne(version)),
                )
                .to_owned(),
        )
        .await?;
    Ok(changed > 0)
}

/// Records the Agent version a device reported, writing only when it changed.
pub async fn record_version(
    executor: &mut impl Executor,
    device_id: &str,
    version: &str,
) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(Agents::Table)
                .value(Agents::AgentVersion, version)
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .and_where(
                    Expr::col(Agents::AgentVersion)
                        .is_null()
                        .or(Expr::col(Agents::AgentVersion).ne(version)),
                )
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Where the enrolled devices stand with `release`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// Reported the release, or a newer one, at their last check.
    pub on_release: i64,
    /// Offered the release and not on it yet: they get it at their next check.
    pub offered: i64,
    /// Not offered it yet.
    pub waiting: i64,
}

/// Counts the enrolled devices that aren't deleted by where they stand with
/// `release` under `rollout`, from one grouped read of their reported
/// versions and buckets: at most 100 rows per version reported.
pub async fn progress(
    executor: &mut impl Executor,
    release: &str,
    rollout: &Rollout,
) -> db::Result<Progress> {
    let rows: Vec<(Option<String>, i64, i64)> = executor
        .fetch_all(
            &Query::select()
                .column(Agents::AgentVersion)
                .column(Agents::RolloutBucket)
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(Agents::Table)
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .group_by_col(Agents::AgentVersion)
                .group_by_col(Agents::RolloutBucket)
                .to_owned(),
        )
        .await?;
    let mut progress = Progress::default();
    for (version, bucket, devices) in rows {
        let bucket = u8::try_from(bucket).unwrap_or(u8::MAX);
        if has_release(version.as_deref(), release) {
            progress.on_release += devices;
        } else if rollout.offers(bucket) {
            progress.offered += devices;
        } else {
            progress.waiting += devices;
        }
    }
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_pinned() {
        // Worked out separately, with Node's crypto module.
        for (id, expected) in [
            ("00000000-0000-0000-0000-000000000000", 68),
            ("3f2b8c4e-1d6a-4e7b-9c3d-5a8f2e1b7c90", 20),
            ("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d", 59),
            ("ffffffff-ffff-ffff-ffff-ffffffffffff", 46),
        ] {
            assert_eq!(bucket(id), expected, "{id}");
        }
    }

    #[test]
    fn each_percentage_holds_about_its_share() {
        const DEVICES: usize = 5000;
        let buckets: Vec<u8> = (0..DEVICES as u128)
            .map(|index| {
                // Spread over the ID like random UUIDs, not counting up in one place.
                let id = uuid::Uuid::from_u128(
                    index.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c834),
                );
                bucket(&id.hyphenated().to_string())
            })
            .collect();
        assert!(buckets.iter().all(|bucket| *bucket < 100));
        for percent in [1_u8, 5, 10, 25, 50, 75, 100] {
            let offered = buckets.iter().filter(|bucket| **bucket < percent).count();
            let share = offered as f64 * 100.0 / DEVICES as f64;
            assert!(
                (share - f64::from(percent)).abs() < 2.5,
                "{percent} % holds {share:.1} %"
            );
        }
        for value in 0..100 {
            let devices = buckets.iter().filter(|bucket| **bucket == value).count();
            assert!((20..=90).contains(&devices), "bucket {value} has {devices}");
        }
    }

    #[test]
    fn stages_are_nested_and_pausing_offers_nothing() {
        let staged = |percent, paused| Rollout {
            staged: true,
            first_percent: 10,
            percent,
            paused,
        };
        for bucket in 0..100 {
            assert!(
                Rollout {
                    staged: false,
                    ..staged(10, true)
                }
                .offers(bucket)
            );
            assert!(!staged(100, true).offers(bucket));
            assert_eq!(staged(10, false).offers(bucket), bucket < 10);
            // Whoever 10 % offers, 25 % offers too.
            assert!(!staged(10, false).offers(bucket) || staged(25, false).offers(bucket));
        }
        assert!(staged(100, false).offers(99));
    }

    #[test]
    fn a_device_has_the_release_once_it_reports_it_or_newer() {
        assert!(has_release(Some("1.2.0"), "1.2.0"));
        assert!(has_release(Some("1.10.0"), "1.2.0"));
        assert!(!has_release(Some("1.1.9"), "1.2.0"));
        assert!(!has_release(Some("1.2.0-beta.1"), "1.2.0"));
        assert!(!has_release(Some("not a version"), "1.2.0"));
        assert!(!has_release(None, "1.2.0"));
    }

    #[test]
    fn the_empty_manifest_offers_nothing() {
        let manifest = UpdateManifest::parse(&empty_manifest()).unwrap();
        assert!(manifest.releases.is_empty());
    }
}
