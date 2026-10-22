//! What changed between two of a device's application lists.
use std::collections::BTreeMap;

use serde::Serialize;

/// An installed application, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct StoredApplication {
    pub name: String,
    pub version: Option<String>,
    pub publisher: Option<String>,
    pub architecture: Option<String>,
    pub installed_at: Option<i64>,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Installed,
    Removed,
    Updated,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Removed => "removed",
            Self::Updated => "updated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub name: String,
    /// The version installed, or for a removal the one removed.
    pub version: Option<String>,
    /// The version an update replaced.
    pub previous_version: Option<String>,
    pub source: String,
}

/// Orders versions with their numbers compared as numbers, so `8.0.10`
/// follows `8.0.9`, and a missing version first.
fn compare_versions(first: Option<&str>, second: Option<&str>) -> std::cmp::Ordering {
    fn parts(version: &str) -> Vec<Result<u64, &str>> {
        let mut parts = Vec::new();
        let mut rest = version;
        while let Some(first) = rest.chars().next() {
            let digit = first.is_ascii_digit();
            let end = rest
                .find(|c: char| c.is_ascii_digit() != digit)
                .unwrap_or(rest.len());
            let (part, tail) = rest.split_at(end);
            parts.push(if digit {
                part.parse().map_err(|_| part)
            } else {
                Err(part)
            });
            rest = tail;
        }
        parts
    }
    match (first, second) {
        (Some(first), Some(second)) => parts(first).cmp(&parts(second)),
        (first, second) => first.cmp(&second),
    }
}

/// An application's identity: its name, source and architecture.
type Key<'a> = (&'a str, &'a str, Option<&'a str>);

/// Each application's versions, by identity.
fn group(list: &[StoredApplication]) -> BTreeMap<Key<'_>, Vec<Option<&str>>> {
    let mut groups: BTreeMap<Key<'_>, Vec<Option<&str>>> = BTreeMap::new();
    for application in list {
        groups
            .entry((
                application.name.as_str(),
                application.source.as_str(),
                application.architecture.as_deref(),
            ))
            .or_default()
            .push(application.version.as_deref());
    }
    groups
}

/// The installs, removals and updates that turn `old` into `new`. The same
/// name from the same source and for the same architecture is one
/// application; one version replacing another is an update. A name can be
/// installed in several versions at once, such as side-by-side runtimes, so
/// versions are compared as multisets.
pub fn diff(old: &[StoredApplication], new: &[StoredApplication]) -> Vec<Change> {
    let (old, new) = (group(old), group(new));
    let mut keys: Vec<&Key> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut changes = Vec::new();
    for key in keys {
        let mut before = old.get(key).cloned().unwrap_or_default();
        let mut after = new.get(key).cloned().unwrap_or_default();
        before.sort_by(|first, second| compare_versions(*first, *second));
        after.sort_by(|first, second| compare_versions(*first, *second));
        // What both lists have is unchanged.
        let mut kept_before = Vec::new();
        for version in before {
            match after.iter().position(|candidate| *candidate == version) {
                Some(index) => {
                    after.remove(index);
                }
                None => kept_before.push(version),
            }
        }
        let (name, source, _) = *key;
        let change = |kind, version: Option<&str>, previous_version: Option<&str>| Change {
            kind,
            name: name.to_owned(),
            version: version.map(str::to_owned),
            previous_version: previous_version.map(str::to_owned),
            source: source.to_owned(),
        };
        let paired = kept_before.len().min(after.len());
        for index in 0..paired {
            changes.push(change(
                ChangeKind::Updated,
                after[index],
                kept_before[index],
            ));
        }
        for version in &after[paired..] {
            changes.push(change(ChangeKind::Installed, *version, None));
        }
        for version in &kept_before[paired..] {
            changes.push(change(ChangeKind::Removed, *version, None));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, version: Option<&str>) -> StoredApplication {
        StoredApplication {
            name: name.into(),
            version: version.map(str::to_owned),
            publisher: None,
            architecture: None,
            installed_at: None,
            source: "windows".into(),
        }
    }

    fn summary(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .map(|change| {
                format!(
                    "{} {} {:?} {:?}",
                    change.kind.as_str(),
                    change.name,
                    change.version,
                    change.previous_version
                )
            })
            .collect()
    }

    #[test]
    fn installs_removals_and_updates_are_found() {
        let old = [
            app("7-Zip", Some("23.01")),
            app("Firefox", Some("130.0")),
            app("Notepad++", None),
        ];
        let new = [
            app("7-Zip", Some("24.08")),
            app("Firefox", Some("130.0")),
            app("VLC", Some("3.0.21")),
        ];
        assert_eq!(
            summary(&diff(&old, &new)),
            [
                r#"updated 7-Zip Some("24.08") Some("23.01")"#,
                "removed Notepad++ None None",
                r#"installed VLC Some("3.0.21") None"#,
            ]
        );
        assert!(diff(&new, &new).is_empty());
    }

    #[test]
    fn side_by_side_versions_are_compared_as_multisets() {
        let old = [
            app(".NET Runtime", Some("6.0")),
            app(".NET Runtime", Some("8.0.1")),
        ];
        let new = [
            app(".NET Runtime", Some("6.0")),
            app(".NET Runtime", Some("8.0.2")),
            app(".NET Runtime", Some("9.0")),
        ];
        assert_eq!(
            summary(&diff(&old, &new)),
            [
                r#"updated .NET Runtime Some("8.0.2") Some("8.0.1")"#,
                r#"installed .NET Runtime Some("9.0") None"#,
            ]
        );
    }

    #[test]
    fn leftover_versions_pair_in_numeric_order() {
        let old = [app("SDK", Some("8.0.9"))];
        let new = [app("SDK", Some("8.0.10")), app("SDK", Some("10.0"))];
        assert_eq!(
            summary(&diff(&old, &new)),
            [
                r#"updated SDK Some("8.0.10") Some("8.0.9")"#,
                r#"installed SDK Some("10.0") None"#,
            ]
        );
    }

    #[test]
    fn sources_and_architectures_are_separate_applications() {
        let mut x86 = app("Runtime", Some("1"));
        x86.architecture = Some("x86".into());
        let mut user = app("Runtime", Some("2"));
        user.source = "windows_user".into();
        let changes = diff(&[x86.clone()], &[x86, user]);
        assert_eq!(summary(&changes), [r#"installed Runtime Some("2") None"#]);
        assert_eq!(changes[0].source, "windows_user");
    }
}
