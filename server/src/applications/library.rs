//! The application library: each application's name, its ways to install
//! on each platform in order of preference, and the version it may be
//! pinned to.
//!
//! A package manager's source is a package ID, checked against the
//! characters that manager's IDs use, so it can never reach the Agent's
//! command line as an option. A custom installer is a shared toolbox file
//! with the arguments that make it silent, the command that uninstalls it,
//! and the rule that tells whether it is installed; it runs as SYSTEM or
//! root, so only users with `applications.manage` add one, and only from
//! files everyone who manages shared files can see.
use std::collections::HashMap;

use meshrmm_protocol_types::{
    CustomInstaller, DetectionRule, DevicePlatform, InstallItem, MAX_COMMAND_CHARS, PackageKey,
    PackageRef, PackageSource, valid_version,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use super::tables::{ApplicationSources, Applications, DevicePackages};
use crate::{
    db::{self, Executor, tables::ToolboxFiles},
    http::ApiError,
};

pub const MAX_NAME_CHARS: usize = 120;
pub const MAX_DESCRIPTION_BYTES: usize = 1000;
/// The most ways to install one application.
pub const MAX_SOURCES: usize = 12;

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_application")
}

/// One way to install an application, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub source: PackageSource,
    pub platform: DevicePlatform,
    /// The package manager's ID; empty for a custom installer.
    pub package_id: String,
    /// A custom installer's toolbox file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// The file's name, size and SHA-256 when the library was read; `None`
    /// once the file is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uninstall_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionRule>,
}

impl Source {
    /// The package, for a package manager's source.
    pub fn key(&self) -> Option<PackageKey> {
        (self.source != PackageSource::Custom).then(|| PackageKey {
            source: self.source,
            id: self.package_id.clone(),
        })
    }

    /// How the Agent installs from it, at the pinned `version`; `None` for
    /// a custom installer whose file is gone.
    pub fn package_ref(&self, version: Option<&str>) -> Option<PackageRef> {
        let custom = if self.source == PackageSource::Custom {
            Some(CustomInstaller {
                file_id: self.file_id.clone()?,
                file_name: self.file_name.clone()?,
                size_bytes: u64::try_from(self.file_size_bytes?).ok()?,
                sha256: self.file_sha256.clone()?,
                arguments: self.install_arguments.clone().unwrap_or_default(),
                uninstall_command: self.uninstall_command.clone(),
                detection: self.detection.clone(),
            })
        } else {
            None
        };
        Some(PackageRef {
            source: self.source,
            id: self.package_id.clone(),
            version: version
                .filter(|_| self.source != PackageSource::Custom)
                .map(str::to_owned),
            custom,
        })
    }
}

/// A library application, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Installed where the package manager can choose a version, and never
    /// updated.
    pub pinned_version: Option<String>,
    pub sources: Vec<Source>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Application {
    pub fn sources_on(&self, platform: DevicePlatform) -> impl Iterator<Item = &Source> {
        self.sources
            .iter()
            .filter(move |source| source.platform == platform)
    }

    /// Whether it has a way to install on `platform`.
    pub fn runs_on(&self, platform: DevicePlatform) -> bool {
        self.sources_on(platform).next().is_some()
    }

    /// Its packages, from every package manager.
    pub fn keys(&self) -> impl Iterator<Item = PackageKey> + '_ {
        self.sources.iter().filter_map(Source::key)
    }

    /// What the Agent installs on `platform`, or `None` if nothing.
    pub fn install_item(&self, platform: DevicePlatform) -> Option<InstallItem> {
        let sources = self
            .sources_on(platform)
            .filter_map(|source| source.package_ref(self.pinned_version.as_deref()))
            .collect::<Vec<_>>();
        (!sources.is_empty()).then(|| InstallItem {
            name: self.name.clone(),
            sources,
        })
    }

    /// The platforms it installs on, in words: "Windows and macOS".
    pub fn platforms(&self) -> Vec<DevicePlatform> {
        let mut platforms = self
            .sources
            .iter()
            .map(|source| source.platform)
            .collect::<Vec<_>>();
        platforms.sort_by_key(|platform| platform.as_str());
        platforms.dedup();
        platforms
    }
}

/// A source as the website sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub source: PackageSource,
    /// Required for a custom installer; a package manager's is its own.
    #[serde(default)]
    pub platform: Option<DevicePlatform>,
    #[serde(default)]
    pub package_id: String,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub install_arguments: Option<String>,
    #[serde(default)]
    pub uninstall_command: Option<String>,
    #[serde(default)]
    pub detection: Option<DetectionRule>,
}

/// An application as the website sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pinned_version: Option<String>,
    pub sources: Vec<SourceInput>,
}

/// A checked application, ready to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Valid {
    pub name: String,
    pub description: String,
    pub pinned_version: Option<String>,
    pub sources: Vec<Source>,
}

impl Valid {
    pub fn has_custom_installer(&self) -> bool {
        self.sources
            .iter()
            .any(|source| source.source == PackageSource::Custom)
    }
}

/// A command line or argument string: at most [`MAX_COMMAND_CHARS`], on one
/// line. `None` when blank.
fn command_line(text: Option<String>, what: &str) -> Result<Option<String>, ApiError> {
    let Some(text) = text.map(|text| text.trim().to_owned()) else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_COMMAND_CHARS || text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "the {what} must be at most {MAX_COMMAND_CHARS} characters on one line"
        )));
    }
    Ok(Some(text))
}

fn trimmed_rule(rule: DetectionRule) -> DetectionRule {
    let blank = |text: Option<String>| {
        text.map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    match rule {
        DetectionRule::Application { name, min_version } => DetectionRule::Application {
            name: name.trim().to_owned(),
            min_version: blank(min_version),
        },
        DetectionRule::File { path } => DetectionRule::File {
            path: path.trim().to_owned(),
        },
        DetectionRule::Registry { key, value } => DetectionRule::Registry {
            key: key.trim().to_owned(),
            value: blank(value),
        },
    }
}

impl ApplicationInput {
    pub fn validate(self) -> Result<Valid, ApiError> {
        let name = self.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_NAME_CHARS
            || name.chars().any(char::is_control)
        {
            return Err(invalid(format!(
                "the name must be 1 to {MAX_NAME_CHARS} characters with no control characters"
            )));
        }
        let description = self.description.trim();
        if description.len() > MAX_DESCRIPTION_BYTES {
            return Err(invalid(format!(
                "the description must be at most {MAX_DESCRIPTION_BYTES} bytes"
            )));
        }
        let pinned_version = self
            .pinned_version
            .map(|version| version.trim().to_owned())
            .filter(|version| !version.is_empty());
        if let Some(version) = &pinned_version
            && !valid_version(version)
        {
            return Err(invalid(
                "the pinned version may use letters, digits and . - _ + ~ : only",
            ));
        }
        if self.sources.is_empty() {
            return Err(invalid("add at least one way to install the application"));
        }
        if self.sources.len() > MAX_SOURCES {
            return Err(invalid(format!(
                "an application has at most {MAX_SOURCES} ways to install"
            )));
        }
        let mut sources = Vec::with_capacity(self.sources.len());
        for input in self.sources {
            sources.push(validate_source(input)?);
        }
        let mut seen = std::collections::HashSet::new();
        for source in &sources {
            let key = (
                source.source,
                source.platform,
                source.package_id.to_ascii_lowercase(),
                source.file_id.clone(),
            );
            if !seen.insert(key) {
                return Err(invalid("the application lists the same source twice"));
            }
        }
        Ok(Valid {
            name: name.to_owned(),
            description: description.to_owned(),
            pinned_version,
            sources,
        })
    }
}

fn validate_source(input: SourceInput) -> Result<Source, ApiError> {
    if input.source != PackageSource::Custom {
        let platform = input
            .source
            .platform()
            .expect("package managers run on one platform");
        if input.platform.is_some_and(|chosen| chosen != platform) {
            return Err(invalid(format!(
                "{} packages install on {} only",
                input.source.label(),
                platform.label()
            )));
        }
        let package_id = input.package_id.trim().to_owned();
        if !input.source.valid_id(&package_id) {
            return Err(invalid(format!(
                "\u{201c}{package_id}\u{201d} isn't a {} package ID",
                input.source.label()
            )));
        }
        if input.file_id.is_some()
            || input.install_arguments.is_some()
            || input.uninstall_command.is_some()
            || input.detection.is_some()
        {
            return Err(invalid(
                "only custom installers take a file, arguments, an uninstall command or a detection rule",
            ));
        }
        return Ok(Source {
            source: input.source,
            platform,
            package_id,
            file_id: None,
            file_name: None,
            file_size_bytes: None,
            file_sha256: None,
            install_arguments: None,
            uninstall_command: None,
            detection: None,
        });
    }
    let platform = input
        .platform
        .ok_or_else(|| invalid("choose the operating system the custom installer is for"))?;
    if !input.package_id.trim().is_empty() {
        return Err(invalid("a custom installer has no package ID"));
    }
    let file_id = input
        .file_id
        .ok_or_else(|| invalid("choose the toolbox file the custom installer runs"))?;
    let file_id = crate::agents::parse_id(&file_id, "file")?;
    let detection = trimmed_rule(input.detection.ok_or_else(|| {
        invalid("a custom installer needs a detection rule, to tell whether it is installed")
    })?);
    if let Some(problem) = detection.problem() {
        return Err(invalid(problem));
    }
    if matches!(detection, DetectionRule::Registry { .. }) && platform != DevicePlatform::Windows {
        return Err(invalid("registry detection rules are for Windows only"));
    }
    Ok(Source {
        source: PackageSource::Custom,
        platform,
        package_id: String::new(),
        file_id: Some(file_id),
        file_name: None,
        file_size_bytes: None,
        file_sha256: None,
        install_arguments: command_line(input.install_arguments, "installer arguments")?,
        uninstall_command: command_line(input.uninstall_command, "uninstall command")?,
        detection: Some(detection),
    })
}

/// Checks that each custom installer's file exists and is shared.
pub async fn check_files(executor: &mut impl Executor, valid: &Valid) -> Result<(), ApiError> {
    for source in &valid.sources {
        let Some(file_id) = &source.file_id else {
            continue;
        };
        let file: Option<(bool, String)> = executor
            .fetch_optional(
                &Query::select()
                    .columns([ToolboxFiles::Shared, ToolboxFiles::Name])
                    .from(ToolboxFiles::Table)
                    .and_where(Expr::col(ToolboxFiles::Id).eq(file_id.as_str()))
                    .to_owned(),
            )
            .await?;
        match file {
            None => return Err(ApiError::not_found("the installer file no longer exists")),
            Some((false, name)) => {
                return Err(ApiError::bad_request(format!(
                    "custom installers use shared toolbox files; share \u{201c}{name}\u{201d} first"
                ))
                .with_code("file_not_shared"));
            }
            Some((true, _)) => {}
        }
    }
    Ok(())
}

/// Whether another application than `except` has the name, ignoring case.
pub async fn name_taken(
    executor: &mut impl Executor,
    name: &str,
    except: Option<&str>,
) -> db::Result<bool> {
    let rows: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([Applications::Id, Applications::Name])
                .from(Applications::Table)
                .to_owned(),
        )
        .await?;
    let name = name.to_lowercase();
    Ok(rows
        .iter()
        .any(|(id, other)| Some(id.as_str()) != except && other.to_lowercase() == name))
}

#[derive(sqlx::FromRow)]
struct ApplicationRow {
    id: String,
    name: String,
    description: String,
    pinned_version: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct SourceRow {
    application_id: String,
    source: String,
    platform: String,
    package_id: String,
    file_id: Option<String>,
    install_arguments: Option<String>,
    uninstall_command: Option<String>,
    detection: Option<String>,
    file_name: Option<String>,
    size_bytes: Option<i64>,
    sha256: Option<String>,
}

/// Library applications, by name: every one, or those in `ids`.
pub async fn load(
    executor: &mut impl Executor,
    ids: Option<&[String]>,
) -> db::Result<Vec<Application>> {
    let mut select = Query::select();
    select
        .columns([
            Applications::Id,
            Applications::Name,
            Applications::Description,
            Applications::PinnedVersion,
            Applications::CreatedAt,
            Applications::UpdatedAt,
        ])
        .from(Applications::Table);
    let mut sources = Query::select();
    sources
        .columns([
            (ApplicationSources::Table, ApplicationSources::ApplicationId),
            (ApplicationSources::Table, ApplicationSources::Source),
            (ApplicationSources::Table, ApplicationSources::Platform),
            (ApplicationSources::Table, ApplicationSources::PackageId),
            (ApplicationSources::Table, ApplicationSources::FileId),
            (
                ApplicationSources::Table,
                ApplicationSources::InstallArguments,
            ),
            (
                ApplicationSources::Table,
                ApplicationSources::UninstallCommand,
            ),
            (ApplicationSources::Table, ApplicationSources::Detection),
        ])
        .expr_as(
            Expr::col((ToolboxFiles::Table, ToolboxFiles::Name)),
            "file_name",
        )
        .columns([
            (ToolboxFiles::Table, ToolboxFiles::SizeBytes),
            (ToolboxFiles::Table, ToolboxFiles::Sha256),
        ])
        .from(ApplicationSources::Table)
        .left_join(
            ToolboxFiles::Table,
            Expr::col((ToolboxFiles::Table, ToolboxFiles::Id))
                .equals((ApplicationSources::Table, ApplicationSources::FileId)),
        )
        .order_by(
            (ApplicationSources::Table, ApplicationSources::ApplicationId),
            Order::Asc,
        )
        .order_by(
            (ApplicationSources::Table, ApplicationSources::Position),
            Order::Asc,
        );
    if let Some(ids) = ids {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        select.and_where(Expr::col(Applications::Id).is_in(ids.iter().cloned()));
        sources.and_where(
            Expr::col((ApplicationSources::Table, ApplicationSources::ApplicationId))
                .is_in(ids.iter().cloned()),
        );
    }
    let rows: Vec<ApplicationRow> = executor.fetch_all(&select).await?;
    let source_rows: Vec<SourceRow> = executor.fetch_all(&sources).await?;
    let mut by_application: HashMap<String, Vec<Source>> = HashMap::new();
    for row in source_rows {
        let (Some(source), Some(platform)) = (
            PackageSource::parse(&row.source),
            DevicePlatform::parse(&row.platform),
        ) else {
            tracing::error!(
                application_id = row.application_id,
                "stored application source is invalid"
            );
            continue;
        };
        let detection = row
            .detection
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        by_application
            .entry(row.application_id)
            .or_default()
            .push(Source {
                source,
                platform,
                package_id: row.package_id,
                // A deleted file leaves the source without one.
                file_id: row.file_id.filter(|_| row.file_name.is_some()),
                file_name: row.file_name,
                file_size_bytes: row.size_bytes,
                file_sha256: row.sha256,
                install_arguments: row.install_arguments,
                uninstall_command: row.uninstall_command,
                detection,
            });
    }
    let mut applications = rows
        .into_iter()
        .map(|row| Application {
            sources: by_application.remove(&row.id).unwrap_or_default(),
            id: row.id,
            name: row.name,
            description: row.description,
            pinned_version: row.pinned_version,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
        .collect::<Vec<_>>();
    applications.sort_by_cached_key(|application| {
        (application.name.to_lowercase(), application.id.clone())
    });
    Ok(applications)
}

pub async fn get(executor: &mut impl Executor, id: &str) -> db::Result<Option<Application>> {
    Ok(load(executor, Some(&[id.to_owned()]))
        .await?
        .into_iter()
        .next())
}

/// Writes the application `id`: a new one, or over the existing one.
/// Returns whether it was written (an existing one may be gone).
pub async fn save(
    executor: &mut impl Executor,
    id: &str,
    valid: &Valid,
    user_id: &str,
    now: i64,
    create: bool,
) -> db::Result<bool> {
    if create {
        executor
            .execute(
                &Query::insert()
                    .into_table(Applications::Table)
                    .columns([
                        Applications::Id,
                        Applications::Name,
                        Applications::Description,
                        Applications::PinnedVersion,
                        Applications::CreatedAt,
                        Applications::UpdatedAt,
                        Applications::UpdatedByUserId,
                    ])
                    .values_panic([
                        id.into(),
                        valid.name.as_str().into(),
                        valid.description.as_str().into(),
                        valid.pinned_version.as_deref().into(),
                        now.into(),
                        now.into(),
                        user_id.into(),
                    ])
                    .to_owned(),
            )
            .await?;
    } else {
        let updated = executor
            .execute(
                &Query::update()
                    .table(Applications::Table)
                    .value(Applications::Name, valid.name.as_str())
                    .value(Applications::Description, valid.description.as_str())
                    .value(Applications::PinnedVersion, valid.pinned_version.as_deref())
                    .value(Applications::UpdatedAt, now)
                    .value(Applications::UpdatedByUserId, user_id)
                    .and_where(Expr::col(Applications::Id).eq(id))
                    .to_owned(),
            )
            .await?;
        if updated == 0 {
            return Ok(false);
        }
        executor
            .execute(
                &Query::delete()
                    .from_table(ApplicationSources::Table)
                    .and_where(Expr::col(ApplicationSources::ApplicationId).eq(id))
                    .to_owned(),
            )
            .await?;
    }
    let mut insert = Query::insert();
    insert.into_table(ApplicationSources::Table).columns([
        ApplicationSources::ApplicationId,
        ApplicationSources::Position,
        ApplicationSources::Source,
        ApplicationSources::Platform,
        ApplicationSources::PackageId,
        ApplicationSources::FileId,
        ApplicationSources::InstallArguments,
        ApplicationSources::UninstallCommand,
        ApplicationSources::Detection,
    ]);
    for (position, source) in valid.sources.iter().enumerate() {
        insert.values_panic([
            id.into(),
            (position as i64).into(),
            source.source.as_str().into(),
            source.platform.as_str().into(),
            source.package_id.as_str().into(),
            source.file_id.as_deref().into(),
            source.install_arguments.as_deref().into(),
            source.uninstall_command.as_deref().into(),
            source
                .detection
                .as_ref()
                .map(|rule| serde_json::to_string(rule).expect("rules serialize"))
                .into(),
        ]);
    }
    executor.execute(&insert).await?;
    Ok(true)
}

/// Deletes the application. Returns whether it existed.
pub async fn delete(executor: &mut impl Executor, id: &str) -> db::Result<bool> {
    // Sources go with it (ON DELETE CASCADE), but SQLite enforces that only
    // with foreign keys on, so they are removed here too.
    executor
        .execute(
            &Query::delete()
                .from_table(ApplicationSources::Table)
                .and_where(Expr::col(ApplicationSources::ApplicationId).eq(id))
                .to_owned(),
        )
        .await?;
    let deleted = executor
        .execute(
            &Query::delete()
                .from_table(Applications::Table)
                .and_where(Expr::col(Applications::Id).eq(id))
                .to_owned(),
        )
        .await?;
    Ok(deleted == 1)
}

/// The library applications whose custom installers use the file, by name.
pub async fn using_file(executor: &mut impl Executor, file_id: &str) -> db::Result<Vec<String>> {
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .distinct()
                .column((Applications::Table, Applications::Name))
                .from(ApplicationSources::Table)
                .inner_join(
                    Applications::Table,
                    Expr::col((Applications::Table, Applications::Id))
                        .equals((ApplicationSources::Table, ApplicationSources::ApplicationId)),
                )
                .and_where(
                    Expr::col((ApplicationSources::Table, ApplicationSources::FileId)).eq(file_id),
                )
                .order_by((Applications::Table, Applications::Name), Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Refuses to delete or unshare a toolbox file the library's custom
/// installers use.
pub async fn refuse_if_file_used(
    executor: &mut impl Executor,
    file_id: &str,
) -> Result<(), ApiError> {
    let applications = using_file(executor, file_id).await?;
    let Some(first) = applications.first() else {
        return Ok(());
    };
    let message = if applications.len() == 1 {
        format!(
            "The application \u{201c}{first}\u{201d} installs with this file. Change or delete it first."
        )
    } else {
        format!(
            "{} applications install with this file, including \u{201c}{first}\u{201d}. Change or delete them first.",
            applications.len()
        )
    };
    Err(ApiError::conflict(message).with_code("file_in_use"))
}

/// How many devices have each library application's packages installed, and
/// how many of them have an update for one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub installed: i64,
    pub outdated: i64,
}

/// Each application's usage, by ID, from the devices' latest scans.
pub async fn usage(
    executor: &mut impl Executor,
    applications: &[Application],
) -> db::Result<HashMap<String, Usage>> {
    let rows: Vec<(String, String, String, Option<String>)> = executor
        .fetch_all(
            &Query::select()
                .columns([
                    (DevicePackages::Table, DevicePackages::DeviceId),
                    (DevicePackages::Table, DevicePackages::Source),
                    (DevicePackages::Table, DevicePackages::PackageId),
                    (DevicePackages::Table, DevicePackages::AvailableVersion),
                ])
                .from(DevicePackages::Table)
                .inner_join(
                    ApplicationSources::Table,
                    Expr::col((ApplicationSources::Table, ApplicationSources::Source))
                        .equals((DevicePackages::Table, DevicePackages::Source))
                        .and(
                            Expr::col((ApplicationSources::Table, ApplicationSources::PackageId))
                                .equals((DevicePackages::Table, DevicePackages::PackageId)),
                        ),
                )
                .to_owned(),
        )
        .await?;
    // A package can be in more than one application, and an application on
    // a device in more than one package; each device counts once.
    let mut devices: HashMap<(String, String), Vec<(String, bool)>> = HashMap::new();
    for (device_id, source, package_id, available) in rows {
        devices
            .entry((source, package_id))
            .or_default()
            .push((device_id, available.is_some()));
    }
    let mut usage = HashMap::new();
    for application in applications {
        let mut installed: HashMap<&str, bool> = HashMap::new();
        for key in application.keys() {
            for (device_id, outdated) in devices
                .get(&(key.source.as_str().to_owned(), key.id))
                .into_iter()
                .flatten()
            {
                *installed.entry(device_id).or_default() |= outdated;
            }
        }
        usage.insert(
            application.id.clone(),
            Usage {
                installed: installed.len() as i64,
                outdated: installed.values().filter(|outdated| **outdated).count() as i64,
            },
        );
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn input(value: serde_json::Value) -> Result<Valid, ApiError> {
        serde_json::from_value::<ApplicationInput>(value)
            .unwrap()
            .validate()
    }

    #[test]
    fn package_sources_take_their_platform_and_a_safe_id() {
        let valid = input(json!({
            "name": " Firefox ",
            "pinned_version": " ",
            "sources": [
                { "source": "winget", "package_id": "Mozilla.Firefox" },
                { "source": "homebrew_cask", "package_id": "firefox" },
                { "source": "flatpak", "package_id": "org.mozilla.firefox" },
            ],
        }))
        .unwrap();
        assert_eq!(valid.name, "Firefox");
        assert_eq!(valid.pinned_version, None);
        assert_eq!(
            valid
                .sources
                .iter()
                .map(|source| source.platform)
                .collect::<Vec<_>>(),
            [
                DevicePlatform::Windows,
                DevicePlatform::Macos,
                DevicePlatform::Linux
            ]
        );
        for id in ["--override calc", "Mozilla.Firefox & calc", "", "-x"] {
            assert!(
                input(
                    json!({ "name": "x", "sources": [{ "source": "winget", "package_id": id }] })
                )
                .is_err(),
                "{id}"
            );
        }
        assert!(
            input(json!({
                "name": "x",
                "sources": [{ "source": "winget", "platform": "linux", "package_id": "A.B" }],
            }))
            .is_err()
        );
        assert!(
            input(json!({
                "name": "x",
                "sources": [{ "source": "winget", "package_id": "A.B", "install_arguments": "/S" }],
            }))
            .is_err()
        );
        assert!(input(json!({ "name": "x", "sources": [] })).is_err());
        assert!(
            input(json!({
                "name": "x",
                "pinned_version": "1; calc",
                "sources": [{ "source": "winget", "package_id": "A.B" }],
            }))
            .is_err()
        );
    }

    #[test]
    fn custom_installers_need_a_file_and_a_detection_rule() {
        let file_id = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a";
        let valid = input(json!({
            "name": "Agent",
            "sources": [{
                "source": "custom",
                "platform": "windows",
                "file_id": file_id,
                "install_arguments": " /qn /norestart ",
                "uninstall_command": "",
                "detection": { "kind": "registry", "key": "HKLM\\SOFTWARE\\Vendor" },
            }],
        }))
        .unwrap();
        assert!(valid.has_custom_installer());
        let source = &valid.sources[0];
        assert_eq!(source.install_arguments.as_deref(), Some("/qn /norestart"));
        assert_eq!(source.uninstall_command, None);
        let without = |field: &str| {
            let mut value = json!({
                "source": "custom",
                "platform": "linux",
                "file_id": file_id,
                "detection": { "kind": "file", "path": "/opt/agent" },
            });
            value.as_object_mut().unwrap().remove(field);
            input(json!({ "name": "x", "sources": [value] }))
        };
        assert!(without("nothing").is_ok());
        assert!(without("platform").is_err());
        assert!(without("file_id").is_err());
        assert!(without("detection").is_err());
        assert!(
            input(json!({
                "name": "x",
                "sources": [{
                    "source": "custom",
                    "platform": "linux",
                    "file_id": file_id,
                    "detection": { "kind": "registry", "key": "HKLM\\SOFTWARE\\Vendor" },
                }],
            }))
            .is_err(),
            "registry rules are for Windows"
        );
        assert!(
            input(json!({
                "name": "x",
                "sources": [{
                    "source": "custom",
                    "platform": "windows",
                    "file_id": file_id,
                    "install_arguments": "/S\r\ncalc",
                    "detection": { "kind": "file", "path": "C:\\x" },
                }],
            }))
            .is_err(),
            "arguments are one line"
        );
    }

    #[test]
    fn install_items_take_the_platforms_sources_in_order_with_the_pin() {
        let source = |source: PackageSource, id: &str| Source {
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
        };
        let application = Application {
            id: "a".into(),
            name: "7-Zip".into(),
            description: String::new(),
            pinned_version: Some("24.08".into()),
            sources: vec![
                source(PackageSource::Chocolatey, "7zip"),
                source(PackageSource::HomebrewCask, "sevenzip"),
                source(PackageSource::Winget, "7zip.7zip"),
                // A custom installer whose file is gone.
                source(PackageSource::Custom, ""),
            ],
            created_at: 0,
            updated_at: 0,
        };
        let item = application.install_item(DevicePlatform::Windows).unwrap();
        assert_eq!(
            item.sources
                .iter()
                .map(|source| (source.source, source.version.as_deref()))
                .collect::<Vec<_>>(),
            [
                (PackageSource::Chocolatey, Some("24.08")),
                (PackageSource::Winget, Some("24.08"))
            ]
        );
        assert!(application.install_item(DevicePlatform::Linux).is_none());
        assert_eq!(
            application.platforms(),
            [DevicePlatform::Macos, DevicePlatform::Windows]
        );
    }
}
