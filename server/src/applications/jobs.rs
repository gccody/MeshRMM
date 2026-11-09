//! What users ask applications runs to do, on one device or as a job on
//! many, and what each device's Agent is sent.
//!
//! A request names library applications, packages (`winget:Mozilla.Firefox`)
//! and, for uninstalls on Windows, inventory programs. It becomes a
//! [`Spec`], which takes a snapshot of the library applications it names:
//! a job keeps its spec, so every device gets the same even if the library
//! changes while the job waits for offline devices. Each device's
//! [`ApplicationOperation`] is made from the spec for its platform, and for
//! uninstalls from what its last scan found installed.
use meshrmm_protocol_types::{
    ApplicationOperation, DevicePlatform, Held, MAX_RUN_ITEMS, PackageKey, PackageSource,
    UninstallItem, UpdateChoice, UpdateMode,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::{Deserialize, Serialize};

use super::{
    library::{self, Application},
    runs::Action,
    store,
    tables::ApplicationJobs,
};
use crate::{
    db::{self, Executor},
    http::ApiError,
};

pub fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_application_run")
}

/// What a run does, as users ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAction {
    Install,
    Update,
    Uninstall,
}

impl RunAction {
    pub fn action(self) -> Action {
        match self {
            Self::Install => Action::Install,
            Self::Update => Action::Update,
            Self::Uninstall => Action::Uninstall,
        }
    }
}

/// A Windows program from a device's inventory, to uninstall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryProgram {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// An install, update or uninstall, as the website asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub action: RunAction,
    /// Library applications: to install, update (with `mode`) or uninstall.
    #[serde(default)]
    pub application_ids: Vec<String>,
    /// Packages, as `source:id`: to update (with `mode`) or uninstall.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Which outdated packages an update covers: `all` of them, `only` the
    /// chosen ones, or all `except` them.
    #[serde(default)]
    pub mode: UpdateMode,
    /// Windows programs from the device's inventory, to uninstall.
    #[serde(default)]
    pub inventory: Vec<InventoryProgram>,
}

/// A request, checked, with a snapshot of the library applications it
/// names. A job keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub action: RunAction,
    /// What it does, in words, such as "Install Firefox and 7-Zip".
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<Application>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageKey>,
    #[serde(default)]
    pub mode: UpdateMode,
    /// The packages pinned library applications hold back from updates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<Held>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inventory: Vec<InventoryProgram>,
}

/// "Firefox", "Firefox and 7-Zip", or "3 applications".
fn names(names: &[&str]) -> String {
    match names {
        [] => "nothing".to_owned(),
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} and {second}"),
        many => format!("{} applications", many.len()),
    }
}

/// Checks package names given as `source:id`.
pub fn parse_packages(packages: &[String]) -> Result<Vec<PackageKey>, ApiError> {
    let mut keys = Vec::with_capacity(packages.len());
    for package in packages {
        let key = PackageKey::parse(package.trim()).ok_or_else(|| {
            invalid(format!(
                "\u{201c}{package}\u{201d} isn't a package; name packages as source:id, such as winget:Mozilla.Firefox"
            ))
        })?;
        keys.push(key);
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// The error for an update of only pinned applications, which would do
/// nothing: see [`Spec::all_pinned`].
pub fn pinned(reason: String) -> ApiError {
    ApiError::conflict(reason).with_code("application_pinned")
}

/// Checks a request and takes its snapshot of the library.
pub async fn build(executor: &mut impl Executor, request: Request) -> Result<Spec, ApiError> {
    let mut ids = request.application_ids;
    for id in &ids {
        crate::agents::parse_id(id, "application")?;
    }
    ids.sort();
    ids.dedup();
    let packages = parse_packages(&request.packages)?;
    let mut inventory = Vec::with_capacity(request.inventory.len());
    for program in request.inventory {
        let name = program.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 300 || name.chars().any(char::is_control) {
            return Err(invalid("an inventory program needs its name"));
        }
        let version = program
            .version
            .map(|version| version.trim().to_owned())
            .filter(|version| !version.is_empty() && version.chars().count() <= 100);
        inventory.push(InventoryProgram { name, version });
    }
    if ids.len() + packages.len() + inventory.len() > MAX_RUN_ITEMS {
        return Err(invalid(format!(
            "a run covers at most {MAX_RUN_ITEMS} applications"
        )));
    }
    let library = library::load(executor, None).await?;
    let applications = library
        .iter()
        .filter(|application| ids.contains(&application.id))
        .cloned()
        .collect::<Vec<_>>();
    if applications.len() != ids.len() {
        return Err(ApiError::not_found(
            "a chosen application is no longer in the library",
        ));
    }
    let mut application_names = applications
        .iter()
        .map(|application| application.name.as_str())
        .collect::<Vec<_>>();
    let package_names = packages.iter().map(|key| key.id.as_str());
    let inventory_names = inventory.iter().map(|program| program.name.as_str());
    let (summary, mode, held) = match request.action {
        RunAction::Install => {
            if applications.is_empty() || !packages.is_empty() || !inventory.is_empty() {
                return Err(invalid("choose the library applications to install"));
            }
            (
                format!("Install {}", names(&application_names)),
                UpdateMode::All,
                Vec::new(),
            )
        }
        RunAction::Update => {
            if !inventory.is_empty() {
                return Err(invalid("updates are by library application or package"));
            }
            let chosen = !applications.is_empty() || !packages.is_empty();
            if (request.mode == UpdateMode::All) == chosen {
                return Err(invalid(if chosen {
                    "choose only or except to update chosen applications"
                } else {
                    "choose the applications to update, or update all"
                }));
            }
            application_names.extend(package_names);
            let summary = match request.mode {
                UpdateMode::All => "Update all applications".to_owned(),
                UpdateMode::Only => format!("Update {}", names(&application_names)),
                UpdateMode::Except => format!(
                    "Update all applications except {}",
                    names(&application_names)
                ),
            };
            let held = store::Matches::new(&library).pinned();
            (summary, request.mode, held)
        }
        RunAction::Uninstall => {
            application_names.extend(package_names);
            application_names.extend(inventory_names);
            if application_names.is_empty() {
                return Err(invalid("choose the applications to uninstall"));
            }
            (
                format!("Uninstall {}", names(&application_names)),
                UpdateMode::All,
                Vec::new(),
            )
        }
    };
    Ok(Spec {
        action: request.action,
        summary,
        applications,
        packages,
        mode,
        held,
        inventory,
    })
}

impl Spec {
    /// Why a device on `platform` can't take it, or `None` if it can.
    pub fn unsupported_on(&self, platform: Option<DevicePlatform>) -> Option<String> {
        let platform = platform?;
        let fits = |key: &PackageKey| key.source.platform() == Some(platform);
        let usable = match self.action {
            RunAction::Install => self
                .applications
                .iter()
                .any(|application| application.runs_on(platform)),
            // An update of chosen packages is for their platforms.
            RunAction::Update => {
                self.mode != UpdateMode::Only
                    || self.packages.iter().any(fits)
                    || self
                        .applications
                        .iter()
                        .any(|application| application.runs_on(platform))
            }
            RunAction::Uninstall => {
                self.applications
                    .iter()
                    .any(|application| application.runs_on(platform))
                    || self.packages.iter().any(fits)
                    || (platform == DevicePlatform::Windows && !self.inventory.is_empty())
            }
        };
        (!usable).then(|| format!("None of the applications is for {}", platform.label()))
    }

    /// Why an install has nothing a device on `platform` whose package
    /// managers are `managers` can use, if so: every way to install it on
    /// the platform is a package manager the device doesn't have. A device
    /// on another platform is skipped already (see [`Spec::unsupported_on`]).
    pub fn unusable_with(
        &self,
        platform: DevicePlatform,
        managers: &[PackageSource],
    ) -> Option<String> {
        if self.action != RunAction::Install {
            return None;
        }
        let mut missing = Vec::new();
        for application in &self.applications {
            for source in application.sources_on(platform) {
                let manager = store::manager_of(source.source);
                if source.source == PackageSource::Custom || managers.contains(&manager) {
                    return None;
                }
                if !missing.contains(&manager.label()) {
                    missing.push(manager.label());
                }
            }
        }
        let (last, rest) = missing.split_last()?;
        let needed = if rest.is_empty() {
            (*last).to_owned()
        } else {
            format!("{} or {last}", rest.join(", "))
        };
        Some(format!(
            "The device has no {needed}, which {} with",
            if self.applications.len() == 1 {
                "the application installs"
            } else {
                "the applications install"
            }
        ))
    }

    /// Why an update of only chosen applications has nothing to do, if
    /// every one it names is pinned in the library, so updates leave it
    /// alone: `GNU Hello is pinned to 2.10 in the library`.
    pub fn all_pinned(&self) -> Option<String> {
        if self.action != RunAction::Update || self.mode != UpdateMode::Only {
            return None;
        }
        let pinned_package = |key: &PackageKey| {
            self.held
                .iter()
                .find(|held| held.package == *key)
                .map(|held| held.version.clone())
        };
        let mut pinned = Vec::new();
        for application in &self.applications {
            pinned.push((
                application.name.clone(),
                application.pinned_version.clone()?,
            ));
        }
        for key in &self.packages {
            pinned.push((key.id.clone(), pinned_package(key)?));
        }
        Some(match pinned.as_slice() {
            [] => return None,
            [(name, version)] => {
                format!("{name} is pinned to {version} in the library; unpin it to update it")
            }
            _ => format!(
                "{} are all pinned in the library; unpin them to update them",
                names(
                    &pinned
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                )
            ),
        })
    }

    /// The packages an update covers, for the Agent.
    fn choice(&self) -> UpdateChoice {
        let mut packages = self.packages.clone();
        packages.extend(self.applications.iter().flat_map(Application::keys));
        packages.sort();
        packages.dedup();
        UpdateChoice {
            mode: self.mode,
            packages,
            held: self.held.clone(),
        }
    }

    /// What the Agent of a device on `platform` is sent, given what its
    /// last scan found installed; or why there is nothing to send.
    pub fn operation(
        &self,
        platform: DevicePlatform,
        installed: &[store::Package],
    ) -> Result<ApplicationOperation, String> {
        if let Some(reason) = self.unsupported_on(Some(platform)) {
            return Err(reason);
        }
        Ok(match self.action {
            RunAction::Install => ApplicationOperation::Install {
                items: self
                    .applications
                    .iter()
                    .filter_map(|application| application.install_item(platform))
                    .collect(),
            },
            RunAction::Update => ApplicationOperation::Update {
                choice: self.choice(),
            },
            RunAction::Uninstall => {
                let installed_name = |key: &PackageKey| {
                    installed
                        .iter()
                        .find(|package| package.key().as_ref() == Some(key))
                        .map(|package| package.name.clone())
                };
                let mut items = Vec::new();
                for application in &self.applications {
                    let keys = application
                        .sources_on(platform)
                        .filter_map(library::Source::key)
                        .collect::<Vec<_>>();
                    let custom = application
                        .sources_on(platform)
                        .find(|source| source.uninstall_command.is_some());
                    let item =
                        if let Some(key) = keys.iter().find(|key| installed_name(key).is_some()) {
                            Some(UninstallItem::Package {
                                name: application.name.clone(),
                                package: key.clone(),
                            })
                        } else if let Some(custom) = custom {
                            Some(UninstallItem::Command {
                                name: application.name.clone(),
                                command: custom.uninstall_command.clone().unwrap_or_default(),
                                detection: custom.detection.clone(),
                            })
                        } else {
                            keys.first().map(|key| UninstallItem::Package {
                                name: application.name.clone(),
                                package: key.clone(),
                            })
                        };
                    items.extend(item);
                }
                for key in &self.packages {
                    if key.source.platform() == Some(platform) {
                        items.push(UninstallItem::Package {
                            name: installed_name(key).unwrap_or_else(|| key.id.clone()),
                            package: key.clone(),
                        });
                    }
                }
                if platform == DevicePlatform::Windows {
                    items.extend(
                        self.inventory
                            .iter()
                            .map(|program| UninstallItem::Inventory {
                                name: program.name.clone(),
                                version: program.version.clone(),
                            }),
                    );
                }
                if items.is_empty() {
                    return Err(format!(
                        "The applications have no way to uninstall on {}",
                        platform.label()
                    ));
                }
                ApplicationOperation::Uninstall { items }
            }
        })
    }

    /// What it does, for the audit log.
    pub fn audit(&self) -> serde_json::Value {
        serde_json::json!({
            "summary": self.summary,
            "applications": self
                .applications
                .iter()
                .map(|application| serde_json::json!({ "id": application.id, "name": application.name }))
                .collect::<Vec<_>>(),
            "packages": self.packages.iter().map(PackageKey::text).collect::<Vec<_>>(),
            "mode": self.mode.as_str(),
            "inventory": self.inventory,
        })
    }
}

/// Keeps the job's spec with it.
pub async fn save(executor: &mut impl Executor, job_id: &str, spec: &Spec) -> db::Result<()> {
    executor
        .execute(
            &Query::insert()
                .into_table(ApplicationJobs::Table)
                .columns([ApplicationJobs::JobId, ApplicationJobs::Spec])
                .values_panic([
                    job_id.into(),
                    serde_json::to_string(spec)
                        .expect("specs serialize")
                        .replace('\0', "")
                        .into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// The job's spec, if it is an application job.
pub async fn load(executor: &mut impl Executor, job_id: &str) -> db::Result<Option<Spec>> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(ApplicationJobs::Spec)
                .from(ApplicationJobs::Table)
                .and_where(Expr::col(ApplicationJobs::JobId).eq(job_id))
                .to_owned(),
        )
        .await?;
    Ok(row.and_then(|(spec,)| {
        serde_json::from_str(&spec)
            .inspect_err(
                |error| tracing::error!(job_id, %error, "stored application job is invalid"),
            )
            .ok()
    }))
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::PackageSource;

    use super::*;
    use crate::applications::library::Source;

    fn source(source: PackageSource, id: &str) -> Source {
        Source {
            source,
            platform: source.platform().unwrap_or(DevicePlatform::Windows),
            package_id: id.into(),
            file_id: None,
            file_name: None,
            file_size_bytes: None,
            file_sha256: None,
            install_arguments: None,
            uninstall_command: None,
            detection: None,
        }
    }

    fn application(name: &str, sources: Vec<Source>) -> Application {
        Application {
            id: name.to_lowercase(),
            name: name.into(),
            description: String::new(),
            pinned_version: None,
            sources,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn installed(source: &str, id: &str, name: &str) -> store::Package {
        store::Package {
            source: source.into(),
            package_id: id.into(),
            name: name.into(),
            version: None,
            available_version: None,
            outdated_since: None,
            application_id: None,
            application_name: None,
            held: false,
        }
    }

    fn spec(action: RunAction, applications: Vec<Application>) -> Spec {
        Spec {
            action,
            summary: String::new(),
            applications,
            packages: Vec::new(),
            mode: UpdateMode::All,
            held: Vec::new(),
            inventory: Vec::new(),
        }
    }

    #[test]
    fn summaries_name_up_to_two_applications() {
        assert_eq!(names(&["Firefox"]), "Firefox");
        assert_eq!(names(&["Firefox", "7-Zip"]), "Firefox and 7-Zip");
        assert_eq!(names(&["a", "b", "c"]), "3 applications");
    }

    #[test]
    fn devices_get_their_platforms_sources() {
        let firefox = application(
            "Firefox",
            vec![
                source(PackageSource::Winget, "Mozilla.Firefox"),
                source(PackageSource::Flatpak, "org.mozilla.firefox"),
            ],
        );
        let install = spec(RunAction::Install, vec![firefox.clone()]);
        let Ok(ApplicationOperation::Install { items }) =
            install.operation(DevicePlatform::Linux, &[])
        else {
            panic!("an install");
        };
        assert_eq!(items[0].sources.len(), 1);
        assert_eq!(items[0].sources[0].id, "org.mozilla.firefox");
        assert!(
            install
                .unsupported_on(Some(DevicePlatform::Macos))
                .is_some()
        );
        assert!(install.operation(DevicePlatform::Macos, &[]).is_err());
        let mut only = spec(RunAction::Update, vec![]);
        only.mode = UpdateMode::Only;
        only.packages = vec![PackageKey {
            source: PackageSource::Winget,
            id: "Git.Git".into(),
        }];
        assert!(only.unsupported_on(Some(DevicePlatform::Windows)).is_none());
        assert!(only.unsupported_on(Some(DevicePlatform::Linux)).is_some());
        only.mode = UpdateMode::Except;
        assert!(only.unsupported_on(Some(DevicePlatform::Linux)).is_none());
    }

    #[test]
    fn uninstalls_use_the_installed_package_or_the_uninstall_command() {
        let mut custom = source(PackageSource::Custom, "");
        custom.uninstall_command = Some("\"C:\\Program Files\\App\\uninstall.exe\" /S".into());
        let app = application(
            "App",
            vec![
                source(PackageSource::Winget, "Vendor.App"),
                source(PackageSource::Chocolatey, "app"),
                custom,
            ],
        );
        let uninstall = spec(RunAction::Uninstall, vec![app]);
        let item = |installed: &[store::Package]| match uninstall
            .operation(DevicePlatform::Windows, installed)
            .unwrap()
        {
            ApplicationOperation::Uninstall { mut items } => items.remove(0),
            _ => panic!("an uninstall"),
        };
        assert!(matches!(
            item(&[installed("chocolatey", "app", "App")]),
            UninstallItem::Package { package, .. } if package.source == PackageSource::Chocolatey
        ));
        assert!(matches!(item(&[]), UninstallItem::Command { .. }));
        let mut with_inventory = spec(RunAction::Uninstall, Vec::new());
        with_inventory.inventory = vec![InventoryProgram {
            name: "Old App".into(),
            version: None,
        }];
        assert!(
            with_inventory
                .operation(DevicePlatform::Linux, &[])
                .is_err()
        );
        assert!(
            with_inventory
                .operation(DevicePlatform::Windows, &[])
                .is_ok()
        );
    }

    #[test]
    fn updates_cover_chosen_applications_packages() {
        let mut update = spec(
            RunAction::Update,
            vec![application(
                "Git",
                vec![source(PackageSource::Winget, "Git.Git")],
            )],
        );
        update.mode = UpdateMode::Except;
        update.packages = vec![PackageKey {
            source: PackageSource::Snap,
            id: "code".into(),
        }];
        let Ok(ApplicationOperation::Update { choice }) =
            update.operation(DevicePlatform::Windows, &[])
        else {
            panic!("an update");
        };
        assert_eq!(choice.mode, UpdateMode::Except);
        assert_eq!(
            choice
                .packages
                .iter()
                .map(PackageKey::text)
                .collect::<Vec<_>>(),
            ["winget:Git.Git", "snap:code"]
        );
    }
}
