//! Application alert rules. "Application install failed" is checked when an
//! install or update run finishes or is found lost, raising an alert for
//! each application that failed (its name is the alert's subject); its
//! alerts stay until a person resolves them. "Application updates pending"
//! is judged with the device-state rules, from when each package was first
//! found outdated.
use std::collections::HashSet;

use meshrmm_protocol_types::PackageKey;

use super::{
    announce,
    evaluate::{Breach, Verdict},
    raised,
    rules::{self, Condition, Rule, plural},
    store::{self, Raise},
};
use crate::{
    applications::{
        runs::{self, Action},
        store::OutdatedPackage,
    },
    db::{self, Executor},
    http::AppState,
    time::{DAY_MS, now_ms},
};

/// The packages of pinned library applications, which are held back on
/// purpose. The library changes without any device reporting, so it is read
/// for each evaluation rather than kept.
pub async fn held(executor: &mut impl Executor) -> db::Result<HashSet<PackageKey>> {
    let library = crate::applications::library::load(executor, None).await?;
    Ok(crate::applications::store::Matches::new(&library)
        .held_keys()
        .into_iter()
        .collect())
}

/// Judges an application updates rule for a device whose outdated packages
/// are `outdated`, but those `held` back; `None` for a device that never
/// scanned.
pub fn judge(
    rule: &Rule,
    outdated: Option<&[OutdatedPackage]>,
    held: &HashSet<PackageKey>,
    now: i64,
) -> Verdict {
    let Some(packages) = outdated else {
        return Verdict::Unknown;
    };
    let days = rule.days.unwrap_or(1);
    let mut overdue = packages
        .iter()
        .filter(|package| !package.key.as_ref().is_some_and(|key| held.contains(key)))
        .filter(|package| now - package.since >= days * DAY_MS)
        .map(|package| (&package.name, package.since))
        .collect::<Vec<_>>();
    overdue.sort_by_key(|(name, since)| (*since, name.to_lowercase()));
    let Some((oldest, since)) = overdue.first() else {
        return Verdict::Known(Vec::new());
    };
    Verdict::Known(vec![Breach {
        subject: String::new(),
        value: overdue.len() as f64,
        detail: format!(
            "{} pending for more than {}, the oldest ({oldest}) since {}",
            plural(overdue.len() as i64, "application update"),
            plural(days, "day"),
            super::format_time(*since)
        ),
    }])
}

/// What failed in a run: each failed application by name and why, or the
/// run as a whole with an empty subject.
fn failures(run: &runs::Run) -> Vec<(String, String)> {
    let verb = if run.action == Action::Update {
        "Updating"
    } else {
        "Installing"
    };
    if run.status == "lost" {
        return vec![(
            String::new(),
            format!("{} never reported how it went", run.summary),
        )];
    }
    let failed = run
        .items
        .iter()
        .flatten()
        .filter(|item| item.outcome == "failed")
        .map(|item| {
            let mut detail = format!("{verb} {} failed", item.name);
            if let Some(error) = &item.error {
                detail = format!("{detail}: {error}");
            }
            (item.name.clone(), detail)
        })
        .collect::<Vec<_>>();
    if failed.is_empty() && matches!(run.status.as_str(), "failed" | "partial") {
        return vec![(
            String::new(),
            format!(
                "{} failed: {}",
                run.summary,
                run.error.as_deref().unwrap_or("the device didn't say why")
            ),
        )];
    }
    failed
}

/// Checks an install or update that failed, partly or entirely, or was
/// found lost, against the application install failure rules.
pub async fn run_finished(state: &AppState, run_id: &str) {
    if let Err(error) = check(state, run_id, now_ms()).await {
        tracing::warn!(
            run_id,
            error = format!("{error:#}"),
            "could not check an application run against the alert rules"
        );
    }
}

async fn check(state: &AppState, run_id: &str, now: i64) -> anyhow::Result<()> {
    let mut database = &state.database;
    let Some(run) = runs::get(&mut database, run_id, true).await? else {
        return Ok(());
    };
    if !matches!(run.action, Action::Install | Action::Update) {
        return Ok(());
    }
    let failures = failures(&run);
    if failures.is_empty() {
        return Ok(());
    }
    let Some(device) = crate::targets::device(&mut database, &run.device_id).await? else {
        return Ok(());
    };
    for rule in rules::load(&mut database, None).await? {
        if !rule.enabled || rule.condition != Condition::AppInstallFailed || !rule.covers(&device) {
            continue;
        }
        for (subject, detail) in &failures {
            let raise = Raise {
                rule_id: &rule.id,
                device_id: &run.device_id,
                subject,
                value: Some(run.failed as f64),
                detail,
                script_run_id: None,
                update_run_id: None,
                output: "",
            };
            if let Some(id) = store::raise(&mut database, &raise, now).await? {
                tracing::info!(
                    rule_id = rule.id,
                    run_id,
                    alert_id = id,
                    "raised an application install failure alert"
                );
                raised(state, &rule, &id, now).await?;
            } else if let Some(id) = store::recur(&mut database, &raise, now).await? {
                announce(state, &rule, &id, None).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::PackageSource;

    use super::*;
    use crate::{alerts::rules::Severity, targets::Target};

    fn rule(days: i64) -> Rule {
        Rule {
            id: "r".into(),
            name: "Stale apps".into(),
            severity: Severity::Warning,
            enabled: true,
            condition: Condition::AppUpdatesPending,
            threshold: None,
            threshold_unit: None,
            duration_minutes: None,
            nonzero_exit_fails: false,
            update_category: None,
            days: Some(days),
            target: Target::All,
            channel_ids: Vec::new(),
            notify_on_resolve: false,
            monitor: None,
            remediation: None,
            notify_when: Default::default(),
            owner_user_id: None,
            check_script: None,
            remediation_script: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn application_updates_raise_once_pending_long_enough() {
        let now = 30 * DAY_MS;
        let package = |source, id: &str, name: &str, days| OutdatedPackage {
            key: Some(PackageKey {
                source,
                id: id.into(),
            }),
            name: name.into(),
            since: now - days * DAY_MS,
        };
        let packages = vec![
            package(PackageSource::Winget, "Mozilla.Firefox", "Firefox", 8),
            package(PackageSource::Winget, "Git.Git", "Git", 10),
            package(PackageSource::Winget, "7zip.7zip", "7-Zip", 1),
        ];
        let none = HashSet::new();
        assert_eq!(judge(&rule(7), None, &none, now), Verdict::Unknown);
        let Verdict::Known(breaches) = judge(&rule(7), Some(&packages), &none, now) else {
            panic!("a verdict");
        };
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].value, 2.0);
        assert!(
            breaches[0].detail.starts_with(
                "2 application updates pending for more than 7 days, the oldest (Git)"
            ),
            "{}",
            breaches[0].detail
        );
        assert_eq!(
            judge(&rule(30), Some(&packages), &none, now),
            Verdict::Known(Vec::new())
        );
        // A pinned application's package is held back, matched as its
        // manager reads IDs.
        let held = HashSet::from([PackageKey {
            source: PackageSource::Winget,
            id: "git.git".into(),
        }]);
        let Verdict::Known(breaches) = judge(&rule(7), Some(&packages), &held, now) else {
            panic!("a verdict");
        };
        assert_eq!(breaches[0].value, 1.0);
        assert!(
            breaches[0].detail.contains("the oldest (Firefox)"),
            "{}",
            breaches[0].detail
        );
    }

    #[test]
    fn failed_runs_name_each_failed_application() {
        let item = |name: &str, outcome: &str, error: Option<&str>| runs::Item {
            position: 0,
            name: name.into(),
            source: None,
            package_id: None,
            outcome: outcome.into(),
            version: None,
            exit_code: None,
            error: error.map(str::to_owned),
            output: None,
            reboot_required: false,
        };
        let mut run = runs::Run {
            id: "x".into(),
            device_id: "d".into(),
            device_name: None,
            action: Action::Install,
            summary: "Install Firefox and Git".into(),
            status: "partial".into(),
            progress: None,
            requested_by: "Ann".into(),
            requested_by_user_id: "u".into(),
            policy_name: None,
            job_id: None,
            error: None,
            succeeded: 1,
            unchanged: 0,
            skipped: 0,
            failed: 1,
            reboot_required: false,
            created_at: 0,
            completed_at: Some(1),
            items: Some(vec![
                item("Firefox", "succeeded", None),
                item("Git", "failed", Some("exit code 1603")),
            ]),
        };
        assert_eq!(
            failures(&run),
            [(
                "Git".to_owned(),
                "Installing Git failed: exit code 1603".to_owned()
            )]
        );
        run.status = "lost".into();
        assert_eq!(failures(&run)[0].0, "");
        run.status = "failed".into();
        run.items = Some(Vec::new());
        run.error = Some("winget isn't installed".into());
        assert_eq!(
            failures(&run)[0].1,
            "Install Firefox and Git failed: winget isn't installed"
        );
    }
}
