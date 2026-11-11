//! The devices a report covers: every device, the devices of chosen
//! companies, or the devices with any or all of chosen tags, as they are when
//! the report is made.
//!
//! Every query a report makes takes its devices from [`Scope::devices`], so
//! what a report may cover is decided here and nowhere else.
use sea_query::{Expr, ExprTrait, IntoColumnRef, SelectStatement, SimpleExpr};
use serde::Deserialize;

use crate::{
    db::{self, Executor},
    http::ApiError,
    targets::{Choice, TagMatch, Target, TargetKind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope(Target);

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
        Self(Target::All)
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
        .map(Self)
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
        .then_some(Self(target))
    }

    /// From the JSON a report keeps.
    pub fn from_json(text: &str) -> Option<Self> {
        let input: ScopeInput = serde_json::from_str(text).ok()?;
        Self::from_stored(
            input.target.as_str(),
            input.tag_match.as_str(),
            input.company_ids,
            input.tag_ids,
        )
    }

    pub fn to_json(&self) -> String {
        serde_json::json!({
            "target": self.0.kind(),
            "company_ids": self.0.company_ids(),
            "tag_ids": self.0.tag_ids(),
            "tag_match": self.0.tag_match(),
        })
        .to_string()
    }

    pub fn target(&self) -> &Target {
        &self.0
    }

    /// The IDs of the enrolled devices the report covers, as a subquery.
    pub fn devices(&self) -> SelectStatement {
        let mut devices = crate::agents::active();
        if let Some(condition) = self.0.condition() {
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
        self.0.check_references(executor).await
    }

    /// The scope in words: "Every device", "Acme and Globex" or "Tagged
    /// servers or london".
    pub async fn label(&self, executor: &mut impl Executor) -> db::Result<String> {
        Ok(self
            .0
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
    }
}
