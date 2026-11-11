//! The devices a report covers: every device, the devices of chosen
//! companies, or the devices with any or all of chosen tags, as they are when
//! the report is made.
//!
//! Every query a report makes takes its devices from [`Scope::devices`], so
//! what a report may cover is decided here and nowhere else. A report made
//! by a user whose `reports.view` reaches only some companies keeps those
//! companies (`within`), and covers only their devices whatever it chose.
use sea_query::{Expr, ExprTrait, IntoColumnRef, SelectStatement, SimpleExpr};
use serde::Deserialize;

use crate::{
    access::CompanySet,
    db::{self, Executor, tables::Agents},
    http::ApiError,
    targets::{Choice, TagMatch, Target, TargetKind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    target: Target,
    /// The companies its devices are taken from: those its maker reached.
    within: CompanySet,
}

/// The part of a stored scope that says which companies its maker reached;
/// absent for every company.
#[derive(Deserialize)]
struct StoredReach {
    #[serde(default)]
    within: Option<Vec<String>>,
}

/// A scope as requests send it and reports store it.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeInput {
    pub target: TargetKind,
    #[serde(default)]
    pub company_ids: Vec<String>,
    #[serde(default)]
    pub tag_ids: Vec<String>,
    #[serde(default)]
    pub tag_match: TagMatch,
}

impl Scope {
    pub fn all() -> Self {
        Self::of(Target::All)
    }

    fn of(target: Target) -> Self {
        Self {
            target,
            within: CompanySet::All,
        }
    }

    /// The scope, covering only the devices of companies in `within`.
    pub fn within(mut self, within: CompanySet) -> Self {
        self.within = within;
        self
    }

    /// The companies its devices are taken from.
    pub fn reach(&self) -> &CompanySet {
        &self.within
    }

    /// From a request: every device, companies or tags; chosen devices and
    /// outdated packages are for jobs.
    pub fn from_input(input: ScopeInput) -> Result<Self, ApiError> {
        if !matches!(
            input.target,
            TargetKind::All | TargetKind::Companies | TargetKind::Tags
        ) {
            return Err(ApiError::bad_request(
                "a report covers every device, chosen companies or tagged devices",
            )
            .with_code("invalid_report"));
        }
        Target::from_request(
            input.target,
            Choice {
                company_ids: input.company_ids,
                tag_ids: input.tag_ids,
                tag_match: input.tag_match,
                ..Choice::default()
            },
        )
        .map(Self::of)
        .map_err(|error| error.with_code("invalid_report"))
    }

    /// From a stored report's or schedule's columns.
    pub fn from_stored(
        target: &str,
        tag_match: &str,
        company_ids: Vec<String>,
        tag_ids: Vec<String>,
    ) -> Option<Self> {
        let target = Target::from_stored(
            target,
            tag_match,
            Choice {
                company_ids,
                tag_ids,
                ..Choice::default()
            },
        )?;
        matches!(
            target,
            Target::All | Target::Companies(_) | Target::Tags { .. }
        )
        .then_some(Self::of(target))
    }

    /// From the JSON a report keeps.
    pub fn from_json(text: &str) -> Option<Self> {
        let input: ScopeInput = serde_json::from_str(text).ok()?;
        let reach: StoredReach = serde_json::from_str(text).ok()?;
        let scope = Self::from_stored(
            input.target.as_str(),
            input.tag_match.as_str(),
            input.company_ids,
            input.tag_ids,
        )?;
        Some(scope.within(match reach.within {
            None => CompanySet::All,
            Some(ids) => CompanySet::Only(ids.into_iter().collect()),
        }))
    }

    pub fn to_json(&self) -> String {
        let mut json = serde_json::json!({
            "target": self.target.kind(),
            "company_ids": self.target.company_ids(),
            "tag_ids": self.target.tag_ids(),
            "tag_match": self.target.tag_match(),
        });
        if let CompanySet::Only(ids) = &self.within {
            json["within"] = ids.iter().cloned().collect::<Vec<_>>().into();
        }
        json.to_string()
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The IDs of the enrolled devices the report covers, as a subquery.
    pub fn devices(&self) -> SelectStatement {
        let mut devices = crate::agents::active();
        if let Some(condition) = self.target.condition() {
            devices.and_where(condition);
        }
        if let Some(condition) = self
            .within
            .company_condition((Agents::Table, Agents::CompanyId))
        {
            devices.and_where(condition);
        }
        devices
    }

    /// `column` holds the ID of a device the report covers.
    pub fn covers(&self, column: impl IntoColumnRef) -> SimpleExpr {
        Expr::col(column).in_subquery(self.devices())
    }

    /// Checks that the chosen companies and tags exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        self.target.check_references(executor).await
    }

    /// The scope in words: "Every device", "Acme and Globex" or "Tagged
    /// servers or london".
    pub async fn label(&self, executor: &mut impl Executor) -> db::Result<String> {
        Ok(self
            .target
            .names(executor)
            .await?
            .unwrap_or_else(|| "Every device".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "00000000-0000-4000-8000-00000000000a";

    #[test]
    fn scopes_are_every_device_companies_or_tags() {
        let input = |target: TargetKind, companies: &[&str], tags: &[&str]| ScopeInput {
            target,
            company_ids: companies.iter().map(|id| (*id).to_owned()).collect(),
            tag_ids: tags.iter().map(|id| (*id).to_owned()).collect(),
            tag_match: TagMatch::All,
        };
        assert!(Scope::from_input(input(TargetKind::Devices, &[], &[])).is_err());
        assert!(Scope::from_input(input(TargetKind::Outdated, &[], &[])).is_err());
        assert!(Scope::from_input(input(TargetKind::Companies, &[], &[])).is_err());
        let tags = Scope::from_input(input(TargetKind::Tags, &[], &[A])).unwrap();
        assert_eq!(Scope::from_json(&tags.to_json()), Some(tags.clone()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&tags.to_json()).unwrap(),
            serde_json::json!({ "target": "tags", "company_ids": [], "tag_ids": [A], "tag_match": "all" })
        );
        assert_eq!(Scope::from_stored("devices", "any", vec![], vec![]), None);
        assert_eq!(
            Scope::from_json(&Scope::all().to_json()),
            Some(Scope::all())
        );
        // The companies a limited maker reached are kept, and stored only
        // when limited.
        let limited = tags.clone().within(CompanySet::Only([A.to_owned()].into()));
        assert_eq!(Scope::from_json(&limited.to_json()), Some(limited.clone()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&limited.to_json()).unwrap()["within"],
            serde_json::json!([A])
        );
        assert_ne!(limited, tags);
    }
}
