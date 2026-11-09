//! Application policies: "Install applications" keeps chosen library
//! applications installed, firing on its schedule and on each newly
//! enrolled device it covers; "Update applications" updates all outdated
//! packages, chosen ones, or all but chosen ones, on its schedule.
//!
//! A policy names library applications by ID and packages as `source:id`,
//! and makes a fresh [`Spec`] from the library each time it fires. A
//! library application a policy names can't be deleted.
use std::collections::HashMap;

use meshrmm_protocol_types::{MAX_RUN_ITEMS, UpdateMode};
use sea_query::{Expr, ExprTrait, OnConflict, Query};
use serde::{Deserialize, Serialize};

use super::{
    jobs::{self, Request, RunAction, Spec},
    tables::{ApplicationPolicies, Applications},
};
use crate::{
    db::{self, Executor, tables::AutomationPolicies},
    http::ApiError,
};

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_policy")
}

/// What an application policy installs or updates, as stored and shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySpec {
    #[serde(default)]
    pub application_ids: Vec<String>,
    /// As `source:id`.
    #[serde(default)]
    pub packages: Vec<String>,
    /// For an update: which outdated packages it covers.
    #[serde(default)]
    pub mode: UpdateMode,
}

impl PolicySpec {
    /// Checks what an install (`install`) or update policy names.
    pub fn validate(input: Option<Self>, install: bool) -> Result<Self, ApiError> {
        let mut spec = input.unwrap_or_default();
        for id in &spec.application_ids {
            crate::agents::parse_id(id, "application")?;
        }
        spec.application_ids.sort();
        spec.application_ids.dedup();
        spec.packages = jobs::parse_packages(&spec.packages)?
            .iter()
            .map(meshrmm_protocol_types::PackageKey::text)
            .collect();
        if spec.application_ids.len() + spec.packages.len() > MAX_RUN_ITEMS {
            return Err(invalid(format!(
                "a policy covers at most {MAX_RUN_ITEMS} applications"
            )));
        }
        if install {
            if spec.application_ids.is_empty() || !spec.packages.is_empty() {
                return Err(invalid(
                    "choose the library applications the policy installs",
                ));
            }
            spec.mode = UpdateMode::All;
        } else {
            let chosen = !spec.application_ids.is_empty() || !spec.packages.is_empty();
            if (spec.mode == UpdateMode::All) == chosen {
                return Err(invalid(if chosen {
                    "choose only or except to update chosen applications"
                } else {
                    "choose the applications the policy updates, or update all"
                }));
            }
        }
        Ok(spec)
    }

    /// Checks that the library applications exist.
    pub async fn check_references(&self, executor: &mut impl Executor) -> Result<(), ApiError> {
        if self.application_ids.is_empty() {
            return Ok(());
        }
        let found: Vec<(String,)> = executor
            .fetch_all(
                &Query::select()
                    .column(Applications::Id)
                    .from(Applications::Table)
                    .and_where(Expr::col(Applications::Id).is_in(self.application_ids.clone()))
                    .to_owned(),
            )
            .await?;
        if found.len() != self.application_ids.len() {
            return Err(ApiError::not_found(
                "a chosen application is no longer in the library",
            ));
        }
        Ok(())
    }

    /// The job it starts now, from the library as it is.
    pub async fn spec(
        &self,
        executor: &mut impl Executor,
        install: bool,
    ) -> Result<Spec, ApiError> {
        jobs::build(
            executor,
            Request {
                action: if install {
                    RunAction::Install
                } else {
                    RunAction::Update
                },
                application_ids: self.application_ids.clone(),
                packages: self.packages.clone(),
                mode: self.mode,
                inventory: Vec::new(),
            },
        )
        .await
    }
}

/// Every application policy's spec, by policy ID, or only `id`'s.
pub async fn load(
    executor: &mut impl Executor,
    id: Option<&str>,
) -> db::Result<HashMap<String, PolicySpec>> {
    let mut select = Query::select();
    select
        .columns([ApplicationPolicies::PolicyId, ApplicationPolicies::Spec])
        .from(ApplicationPolicies::Table);
    if let Some(id) = id {
        select.and_where(Expr::col(ApplicationPolicies::PolicyId).eq(id));
    }
    let rows: Vec<(String, String)> = executor.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(policy_id, spec)| Some((policy_id, serde_json::from_str(&spec).ok()?)))
        .collect())
}

/// Saves the policy's spec, or removes it for a policy that is no longer an
/// application policy.
pub async fn save(
    executor: &mut impl Executor,
    policy_id: &str,
    spec: Option<&PolicySpec>,
) -> db::Result<()> {
    let Some(spec) = spec else {
        executor
            .execute(
                &Query::delete()
                    .from_table(ApplicationPolicies::Table)
                    .and_where(Expr::col(ApplicationPolicies::PolicyId).eq(policy_id))
                    .to_owned(),
            )
            .await?;
        return Ok(());
    };
    executor
        .execute(
            &Query::insert()
                .into_table(ApplicationPolicies::Table)
                .columns([ApplicationPolicies::PolicyId, ApplicationPolicies::Spec])
                .values_panic([
                    policy_id.into(),
                    serde_json::to_string(spec).expect("specs serialize").into(),
                ])
                .on_conflict(
                    OnConflict::column(ApplicationPolicies::PolicyId)
                        .update_column(ApplicationPolicies::Spec)
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// The policies that name the library application, by name.
pub async fn using_application(
    executor: &mut impl Executor,
    application_id: &str,
) -> db::Result<Vec<String>> {
    let rows: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .column((AutomationPolicies::Table, AutomationPolicies::Name))
                .column((ApplicationPolicies::Table, ApplicationPolicies::Spec))
                .from(ApplicationPolicies::Table)
                .inner_join(
                    AutomationPolicies::Table,
                    Expr::col((AutomationPolicies::Table, AutomationPolicies::Id))
                        .equals((ApplicationPolicies::Table, ApplicationPolicies::PolicyId)),
                )
                .to_owned(),
        )
        .await?;
    let mut names = rows
        .into_iter()
        .filter(|(_, spec)| {
            serde_json::from_str::<PolicySpec>(spec)
                .is_ok_and(|spec| spec.application_ids.iter().any(|id| id == application_id))
        })
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    names.sort_by_key(|name| name.to_lowercase());
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(ids: &[&str], packages: &[&str], mode: UpdateMode) -> PolicySpec {
        PolicySpec {
            application_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            packages: packages.iter().map(|id| (*id).to_owned()).collect(),
            mode,
        }
    }

    #[test]
    fn policies_name_what_they_cover() {
        let id = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a";
        assert!(PolicySpec::validate(Some(spec(&[id], &[], UpdateMode::All)), true).is_ok());
        assert!(PolicySpec::validate(None, true).is_err());
        assert!(
            PolicySpec::validate(
                Some(spec(&[id], &["winget:Git.Git"], UpdateMode::All)),
                true
            )
            .is_err(),
            "installs are of library applications"
        );
        assert!(PolicySpec::validate(None, false).is_ok(), "update all");
        assert!(
            PolicySpec::validate(Some(spec(&[], &["winget:Git.Git"], UpdateMode::All)), false)
                .is_err()
        );
        let except = PolicySpec::validate(
            Some(spec(
                &[],
                &[" winget:Git.Git", "winget:Git.Git"],
                UpdateMode::Except,
            )),
            false,
        )
        .unwrap();
        assert_eq!(except.packages, ["winget:Git.Git"]);
        assert!(
            PolicySpec::validate(Some(spec(&[], &["winget:--all"], UpdateMode::Only)), false)
                .is_err()
        );
        assert!(PolicySpec::validate(Some(spec(&["nope"], &[], UpdateMode::All)), true).is_err());
    }
}
