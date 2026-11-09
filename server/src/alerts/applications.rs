//! Application alert rules. "Application install failed" is checked when an
//! install or update run finishes or is found lost, raising an alert for
//! each application that failed (its name is the alert's subject); its
//! alerts stay until a person resolves them. "Application updates pending"
//! is judged with the device-state rules, from when each package was first
//! found outdated.
use std::collections::HashMap;

use super::{
    announce,
    evaluate::{Breach, Verdict},
    rules::{self, Condition, Rule, plural},
    store::{self, Raise},
};
use crate::{
    applications::runs::{self, Action},
    db::{self, Executor},
    http::AppState,
    time::{DAY_MS, now_ms},
};

/// Each scanned device's outdated packages, but those pinned library
/// applications hold back, when an enabled rule watches them.
pub async fn outdated(
    executor: &mut impl Executor,
    rules: &[Rule],
) -> db::Result<HashMap<String, Vec<(String, i64)>>> {
    if !rules
        .iter()
        .any(|rule| rule.enabled && rule.condition == Condition::AppUpdatesPending)
    {
        return Ok(HashMap::new());
    }
    let library = crate::applications::library::load(executor, None).await?;
    let held = crate::applications::store::Matches::new(&library).held_keys();
    crate::applications::store::outdated_ages(executor, &held).await
}

/// Judges an application updates rule for a device whose outdated packages
/// (by name, with when each was first found outdated) are `outdated`;
/// `None` for a device that never scanned.
pub fn judge(rule: &Rule, outdated: Option<&Vec<(String, i64)>>, now: i64) -> Verdict {
    let Some(packages) = outdated else {
        return Verdict::Unknown;
    };
    let days = rule.days.unwrap_or(1);
    let mut overdue = packages
        .iter()
        .filter(|(_, since)| now - since >= days * DAY_MS)
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
            };
            if let Some(id) = store::raise(&mut database, &raise, now).await? {
                tracing::info!(
                    rule_id = rule.id,
                    run_id,
                    alert_id = id,
                    "raised an application install failure alert"
                );
                announce(state, &rule, &id, true).await?;
            } else if let Some(id) = store::recur(&mut database, &raise, now).await? {
                announce(state, &rule, &id, false).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn application_updates_raise_once_pending_long_enough() {
        let now = 30 * DAY_MS;
        let packages = vec![
            ("Firefox".to_owned(), now - 8 * DAY_MS),
            ("Git".to_owned(), now - 10 * DAY_MS),
            ("7-Zip".to_owned(), now - DAY_MS),
        ];
        assert_eq!(judge(&rule(7), None, now), Verdict::Unknown);
        let Verdict::Known(breaches) = judge(&rule(7), Some(&packages), now) else {
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
            judge(&rule(30), Some(&packages), now),
            Verdict::Known(Vec::new())
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
