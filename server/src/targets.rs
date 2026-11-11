//! Which devices a job, automation policy or alert rule covers: chosen
//! devices, whole companies, the devices with chosen tags, or every device;
//! and for a job that updates applications, the devices where chosen
//! packages, or any, are outdated.
//!
//! A target is kept as chosen and resolved each time it is used, so a policy
//! or rule also covers devices enrolled, moved or tagged after it was saved.
use std::collections::HashMap;

use meshrmm_protocol_types::{MAX_RUN_ITEMS, PackageKey};
use sea_query::{Expr, ExprTrait, Func, Order, Query, SimpleExpr};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

use crate::{
    applications::outdated,
    db::{
        self, Executor,
        tables::{Agents, Companies, DeviceTags, Tags},
    },
    http::ApiError,
};

/// The most devices a request may choose one by one.
pub const MAX_CHOSEN_DEVICES: usize = 10_000;

/// The kind of choice a target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// Chosen devices.
    Devices,
    /// Every device in chosen companies.
    Companies,
    /// Every device with any, or all, of chosen tags.
    Tags,
    /// Every device.
    All,
    /// The devices where chosen packages, or any, are outdated: only for a
    /// job that updates applications, resolved when it is created.
    Outdated,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Devices => "devices",
            Self::Companies => "companies",
            Self::Tags => "tags",
            Self::All => "all",
            Self::Outdated => "outdated",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "devices" => Some(Self::Devices),
            "companies" => Some(Self::Companies),
            "tags" => Some(Self::Tags),
            "all" => Some(Self::All),
            "outdated" => Some(Self::Outdated),
            _ => None,
        }
    }

    /// Whole companies, tags and every device can reach many devices at
    /// once, so jobs on them need [`crate::rbac::Permission::JobsRunWide`].
    /// Outdated packages' devices are only for application updates, which
    /// `applications.deploy` allows on any device, and only those that have
    /// what the update is for.
    pub fn is_wide(self) -> bool {
        !matches!(self, Self::Devices | Self::Outdated)
    }
}

/// Whether a device needs any of a target's tags, or all of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMatch {
    #[default]
    Any,
    All,
}

impl TagMatch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::All => "all",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "any" => Some(Self::Any),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// The devices something covers, as chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Devices(Vec<String>),
    Companies(Vec<String>),
    Tags {
        ids: Vec<String>,
        matching: TagMatch,
    },
    All,
    /// The devices with any of `packages` outdated; with none chosen, the
    /// devices with any package outdated but those in `except` (the ones
    /// pinned library applications hold back).
    Outdated {
        packages: Vec<PackageKey>,
        except: Vec<PackageKey>,
    },
}

/// A target's fields in a request, beside its kind.
#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub device_ids: Vec<String>,
    pub company_ids: Vec<String>,
    pub tag_ids: Vec<String>,
    pub tag_match: TagMatch,
    /// For [`TargetKind::Outdated`]: the packages, as `source:id`; none for
    /// any.
    pub packages: Vec<String>,
}

impl Target {
    pub fn kind(&self) -> TargetKind {
        match self {
            Self::Devices(_) => TargetKind::Devices,
            Self::Companies(_) => TargetKind::Companies,
            Self::Tags { .. } => TargetKind::Tags,
            Self::All => TargetKind::All,
            Self::Outdated { .. } => TargetKind::Outdated,
        }
    }

    /// From a request's fields: the kind, and the IDs it uses. Checks the
    /// IDs' form and that a choice isn't empty. Callers add their own error
    /// code.
    pub fn from_request(kind: TargetKind, choice: Choice) -> Result<Self, ApiError> {
        let ids = |mut ids: Vec<String>, what: &str| {
            for id in &ids {
                crate::agents::parse_id(id, what)?;
            }
            ids.sort();
            ids.dedup();
            Ok::<_, ApiError>(ids)
        };
        let empty = |what: &str| ApiError::bad_request(format!("choose at least one {what}"));
        match kind {
            TargetKind::Devices => {
                if choice.device_ids.is_empty() {
                    return Err(empty("device"));
                }
                if choice.device_ids.len() > MAX_CHOSEN_DEVICES {
                    return Err(ApiError::bad_request(format!(
                        "choose at most {MAX_CHOSEN_DEVICES} devices, or whole companies or tags"
                    )));
                }
                Ok(Self::Devices(ids(choice.device_ids, "device")?))
            }
            TargetKind::Companies => {
                if choice.company_ids.is_empty() {
                    return Err(empty("company"));
                }
                Ok(Self::Companies(ids(choice.company_ids, "company")?))
            }
            TargetKind::Tags => {
                if choice.tag_ids.is_empty() {
                    return Err(empty("tag"));
                }
                Ok(Self::Tags {
                    ids: ids(choice.tag_ids, "tag")?,
                    matching: choice.tag_match,
                })
            }
            TargetKind::All => Ok(Self::All),
            TargetKind::Outdated => {
                let packages = crate::applications::jobs::parse_packages(&choice.packages)?;
                if packages.len() > MAX_RUN_ITEMS {
                    return Err(ApiError::bad_request(format!(
                        "choose at most {MAX_RUN_ITEMS} outdated packages, or none for any"
                    )));
                }
                Ok(Self::Outdated {
                    packages,
                    except: Vec::new(),
                })
            }
        }
    }

    /// From stored rows: the kind, its tag match and its linked IDs. `None`
    /// when the kind or match is unknown, or for outdated packages, which
    /// nothing stores.
    pub fn from_stored(kind: &str, tag_match: &str, choice: Choice) -> Option<Self> {
        Some(match TargetKind::parse(kind)? {
            TargetKind::Devices => Self::Devices(choice.device_ids),
            TargetKind::Companies => Self::Companies(choice.company_ids),
            TargetKind::Tags => Self::Tags {
                ids: choice.tag_ids,
                matching: TagMatch::parse(tag_match)?,
            },
            TargetKind::All => Self::All,
            TargetKind::Outdated => return None,
        })
    }

    pub fn device_ids(&self) -> &[String] {
        match self {
            Self::Devices(ids) => ids,
            _ => &[],
        }
    }

    pub fn company_ids(&self) -> &[String] {
        match self {
            Self::Companies(ids) => ids,
            _ => &[],
        }
    }

    pub fn tag_ids(&self) -> &[String] {
        match self {
            Self::Tags { ids, .. } => ids,
            _ => &[],
        }
    }

    /// How a tags target matches; `any` for the other kinds.
    pub fn tag_match(&self) -> TagMatch {
        match self {
            Self::Tags { matching, .. } => *matching,
            _ => TagMatch::Any,
        }
    }

    /// An outdated packages target's packages; none for any, and for the
    /// other kinds.
    pub fn packages(&self) -> &[PackageKey] {
        match self {
            Self::Outdated { packages, .. } => packages,
            _ => &[],
        }
    }

    /// Whether the target covers `device`. Outdated packages' devices are
    /// only resolved by the database when a job is created ([`condition`]),
    /// so they cover none here.
    ///
    /// [`condition`]: Self::condition
    pub fn covers(&self, device: &Device) -> bool {
        match self {
            Self::Outdated { .. } => false,
            Self::All => true,
            Self::Devices(ids) => ids.contains(&device.id),
            Self::Companies(ids) => device
                .company_id
                .as_ref()
                .is_some_and(|company| ids.contains(company)),
            Self::Tags { ids, matching } => {
                let has = |id: &String| device.tag_ids.contains(id);
                match matching {
                    TagMatch::Any => ids.iter().any(has),
                    TagMatch::All => !ids.is_empty() && ids.iter().all(has),
                }
            }
        }
    }

    /// A condition on the `agents` table that holds for the devices the
    /// target covers; `None` for every device. Chosen devices are matched by
    /// ID, so callers with many should query in chunks instead.
    pub fn condition(&self) -> Option<SimpleExpr> {
        let id = || Expr::col((Agents::Table, Agents::Id));
        match self {
            Self::All => None,
            Self::Devices(ids) => Some(id().is_in(ids.iter().cloned())),
            Self::Companies(ids) => {
                Some(Expr::col((Agents::Table, Agents::CompanyId)).is_in(ids.iter().cloned()))
            }
            Self::Tags { ids, matching } => Some(id().in_subquery(tagged(ids, *matching))),
            Self::Outdated { packages, except } => {
                Some(id().in_subquery(outdated::devices_with_outdated(packages, except)))
            }
        }
    }

    /// Checks that the companies, tags or devices chosen exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        let missing = |what: &str| ApiError::not_found(format!("a chosen {what} no longer exists"));
        match self {
            Self::All | Self::Outdated { .. } => {}
            Self::Companies(ids) => {
                let found: Vec<(String,)> = executor
                    .fetch_all(
                        &Query::select()
                            .column(Companies::Id)
                            .from(Companies::Table)
                            .and_where(Expr::col(Companies::Id).is_in(ids.iter().cloned()))
                            .to_owned(),
                    )
                    .await?;
                if found.len() != ids.len() {
                    return Err(missing("company"));
                }
            }
            Self::Tags { ids, .. } => {
                let found: Vec<(String,)> = executor
                    .fetch_all(
                        &Query::select()
                            .column(Tags::Id)
                            .from(Tags::Table)
                            .and_where(Expr::col(Tags::Id).is_in(ids.iter().cloned()))
                            .to_owned(),
                    )
                    .await?;
                if found.len() != ids.len() {
                    return Err(missing("tag"));
                }
            }
            Self::Devices(ids) => {
                for chunk in ids.chunks(500) {
                    let found: Vec<(String,)> = executor
                        .fetch_all(
                            &crate::agents::active()
                                .and_where(Expr::col(Agents::Id).is_in(chunk.iter().cloned()))
                                .to_owned(),
                        )
                        .await?;
                    if found.len() != chunk.len() {
                        return Err(missing("device"));
                    }
                }
            }
        }
        Ok(())
    }

    /// The companies' or tags' names in words, such as "Acme, Globex and 2
    /// more" or "Tagged servers and london", or the outdated packages', such
    /// as "Where Firefox is outdated"; `None` for the other kinds.
    pub async fn names(&self, executor: &mut impl Executor) -> db::Result<Option<String>> {
        if let Self::Outdated { packages, .. } = self {
            return Ok(Some(match packages.as_slice() {
                [] => "Where any application is outdated".to_owned(),
                [_] => format!(
                    "Where {} is outdated",
                    outdated::names(executor, packages).await?[0]
                ),
                _ => {
                    let names =
                        outdated::names(executor, &packages[..packages.len().min(4)]).await?;
                    format!(
                        "Where any of {} is outdated",
                        list(&names, packages.len(), "and")
                    )
                }
            }));
        }
        let (names, total): (Vec<(String,)>, usize) = match self {
            Self::Companies(ids) => (
                executor
                    .fetch_all(
                        &Query::select()
                            .column(Companies::Name)
                            .from(Companies::Table)
                            .and_where(Expr::col(Companies::Id).is_in(ids.iter().cloned()))
                            .order_by(Companies::Name, Order::Asc)
                            .to_owned(),
                    )
                    .await?,
                ids.len(),
            ),
            Self::Tags { ids, .. } => (
                executor
                    .fetch_all(
                        &Query::select()
                            .column(Tags::Name)
                            .from(Tags::Table)
                            .and_where(Expr::col(Tags::Id).is_in(ids.iter().cloned()))
                            .order_by(Tags::Name, Order::Asc)
                            .to_owned(),
                    )
                    .await?,
                ids.len(),
            ),
            Self::Devices(_) | Self::All | Self::Outdated { .. } => return Ok(None),
        };
        let names = names.into_iter().map(|(name,)| name).collect::<Vec<_>>();
        Ok(Some(match self {
            Self::Tags { matching, .. } => {
                let joiner = match matching {
                    TagMatch::Any => "or",
                    TagMatch::All => "and",
                };
                format!("Tagged {}", list(&names, total, joiner))
            }
            _ => list(&names, total, "and"),
        }))
    }
}

/// "a, b and c", the first four of `total` names and how many more.
fn list(names: &[String], total: usize, joiner: &str) -> String {
    let shown = &names[..names.len().min(4)];
    let more = total.saturating_sub(shown.len());
    match (shown, more) {
        ([], _) => "none".to_owned(),
        ([only], 0) => only.clone(),
        (_, 0) => format!(
            "{} {joiner} {}",
            shown[..shown.len() - 1].join(", "),
            shown[shown.len() - 1]
        ),
        _ => format!("{} {joiner} {more} more", shown.join(", ")),
    }
}

/// The devices with any, or all, of `ids`.
pub fn tagged(ids: &[String], matching: TagMatch) -> sea_query::SelectStatement {
    let mut select = Query::select();
    select
        .column(DeviceTags::DeviceId)
        .from(DeviceTags::Table)
        .and_where(Expr::col(DeviceTags::TagId).is_in(ids.iter().cloned()));
    if matching == TagMatch::All {
        select
            .group_by_col(DeviceTags::DeviceId)
            .and_having(Func::count(Expr::col(DeviceTags::TagId)).eq(ids.len() as i64));
    }
    select
}

/// Lists a target's kind and IDs the way requests send them:
/// `{target, device_ids, company_ids, tag_ids, tag_match}`, and for outdated
/// packages `packages` as `source:id`.
impl Serialize for Target {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let outdated = matches!(self, Self::Outdated { .. });
        let mut fields = serializer.serialize_struct("Target", 5 + usize::from(outdated))?;
        fields.serialize_field("target", &self.kind())?;
        fields.serialize_field("device_ids", self.device_ids())?;
        fields.serialize_field("company_ids", self.company_ids())?;
        fields.serialize_field("tag_ids", self.tag_ids())?;
        fields.serialize_field("tag_match", &self.tag_match())?;
        if outdated {
            let packages = self
                .packages()
                .iter()
                .map(PackageKey::text)
                .collect::<Vec<_>>();
            fields.serialize_field("packages", &packages)?;
        }
        fields.end()
    }
}

/// What a target needs to know about a device to say whether it covers it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub company_id: Option<String>,
    pub tag_ids: Vec<String>,
}

/// Every enrolled device that isn't deleted, or only `only`, with its
/// company and tags.
pub async fn devices(executor: &mut impl Executor, only: Option<&str>) -> db::Result<Vec<Device>> {
    let mut select = crate::agents::active();
    select.column(Agents::CompanyId);
    if let Some(id) = only {
        select.and_where(Expr::col(Agents::Id).eq(id));
    }
    let rows: Vec<(String, Option<String>)> = executor.fetch_all(&select).await?;
    let mut tags = crate::tags::device_tag_ids(executor, only).await?;
    Ok(rows
        .into_iter()
        .map(|(id, company_id)| Device {
            tag_ids: tags.remove(&id).unwrap_or_default(),
            id,
            company_id,
        })
        .collect())
}

/// The device, if it is enrolled and not deleted.
pub async fn device(executor: &mut impl Executor, id: &str) -> db::Result<Option<Device>> {
    Ok(devices(executor, Some(id)).await?.into_iter().next())
}

/// The devices by ID.
pub fn by_id(devices: &[Device]) -> HashMap<&str, &Device> {
    devices
        .iter()
        .map(|device| (device.id.as_str(), device))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "00000000-0000-4000-8000-00000000000a";
    const B: &str = "00000000-0000-4000-8000-00000000000b";

    fn device(company: Option<&str>, tags: &[&str]) -> Device {
        Device {
            id: "d".into(),
            company_id: company.map(Into::into),
            tag_ids: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    #[test]
    fn targets_need_a_choice() {
        let choice = |devices: &[&str], companies: &[&str], tags: &[&str]| Choice {
            device_ids: devices.iter().map(|id| (*id).to_owned()).collect(),
            company_ids: companies.iter().map(|id| (*id).to_owned()).collect(),
            tag_ids: tags.iter().map(|id| (*id).to_owned()).collect(),
            tag_match: TagMatch::All,
            packages: Vec::new(),
        };
        assert!(Target::from_request(TargetKind::Devices, choice(&[], &[A], &[A])).is_err());
        assert!(Target::from_request(TargetKind::Companies, choice(&[A], &[], &[])).is_err());
        assert!(Target::from_request(TargetKind::Tags, choice(&[], &[A], &[])).is_err());
        assert!(Target::from_request(TargetKind::Tags, choice(&[], &[], &["x"])).is_err());
        assert_eq!(
            Target::from_request(TargetKind::Tags, choice(&[], &[], &[B, A, B])).unwrap(),
            Target::Tags {
                ids: vec![A.into(), B.into()],
                matching: TagMatch::All
            }
        );
        assert_eq!(
            Target::from_request(TargetKind::All, choice(&[A], &[], &[])).unwrap(),
            Target::All
        );
        assert!(TargetKind::Tags.is_wide() && !TargetKind::Devices.is_wide());
        assert!(!TargetKind::Outdated.is_wide());
        let outdated = |packages: &[&str]| Choice {
            packages: packages.iter().map(|id| (*id).to_owned()).collect(),
            ..Choice::default()
        };
        assert_eq!(
            Target::from_request(
                TargetKind::Outdated,
                outdated(&["winget:Git.Git", "winget:git.git", "flatpak:org.gimp.GIMP"])
            )
            .unwrap()
            .packages()
            .iter()
            .map(PackageKey::text)
            .collect::<Vec<_>>(),
            ["winget:Git.Git", "flatpak:org.gimp.GIMP"],
            "one package however its ID is cased, where its manager ignores case"
        );
        assert!(Target::from_request(TargetKind::Outdated, outdated(&[])).is_ok());
        assert!(Target::from_request(TargetKind::Outdated, outdated(&["Git.Git"])).is_err());
        let many = (0..=MAX_RUN_ITEMS)
            .map(|n| format!("winget:Package.N{n}"))
            .collect::<Vec<_>>();
        assert!(
            Target::from_request(
                TargetKind::Outdated,
                Choice {
                    packages: many,
                    ..Choice::default()
                }
            )
            .is_err()
        );
        assert_eq!(
            Target::from_stored("outdated", "any", Choice::default()),
            None
        );
    }

    #[test]
    fn targets_cover_devices_by_company_and_tags() {
        let any = Target::Tags {
            ids: vec!["x".into(), "y".into()],
            matching: TagMatch::Any,
        };
        let all = Target::Tags {
            ids: vec!["x".into(), "y".into()],
            matching: TagMatch::All,
        };
        assert!(any.covers(&device(None, &["y"])));
        assert!(!any.covers(&device(None, &["z"])));
        assert!(!all.covers(&device(None, &["y"])));
        assert!(all.covers(&device(None, &["x", "z", "y"])));
        let none = Target::Tags {
            ids: vec![],
            matching: TagMatch::All,
        };
        assert!(!none.covers(&device(None, &["x"])));
        let companies = Target::Companies(vec!["acme".into()]);
        assert!(companies.covers(&device(Some("acme"), &[])));
        assert!(!companies.covers(&device(None, &[])));
        assert!(Target::Devices(vec!["d".into()]).covers(&device(None, &[])));
        assert!(Target::All.covers(&device(None, &[])));
    }

    #[test]
    fn names_are_listed_in_words() {
        let names = ["a", "b", "c", "d", "e"].map(String::from);
        assert_eq!(list(&names[..1], 1, "and"), "a");
        assert_eq!(list(&names[..3], 3, "or"), "a, b or c");
        assert_eq!(list(&names[..4], 6, "and"), "a, b, c, d and 2 more");
        assert_eq!(list(&[], 2, "and"), "none");
    }

    #[test]
    fn targets_serialize_as_requests_send_them() {
        let target = Target::Tags {
            ids: vec!["x".into()],
            matching: TagMatch::All,
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            serde_json::json!({ "target": "tags", "device_ids": [], "company_ids": [], "tag_ids": ["x"], "tag_match": "all" })
        );
    }
}
