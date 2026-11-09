//! Third-party applications: what an Agent finds installed and outdated in
//! the package managers it supports, and the installs, updates and
//! uninstalls the server asks for. See docs/application-deployment.md.
//!
//! The server asks on the control connection
//! ([`crate::AgentCommand::ScanApplications`],
//! [`crate::AgentCommand::RunApplications`]), and the Agent reports on it
//! ([`crate::AgentStatusMessage::ApplicationScan`],
//! [`crate::AgentStatusMessage::ApplicationProgress`],
//! [`crate::AgentStatusMessage::ApplicationResult`]).
//!
//! Package IDs and versions reach a package manager's command line as their
//! own arguments, never through a shell, and are limited to the characters
//! the managers use, never starting with `-`, so none can be read as an
//! option.
use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::{DevicePlatform, clean_optional, clean_text};

/// The most installed packages a scan lists.
pub const MAX_SCANNED_PACKAGES: usize = 5_000;
/// The most applications one run installs, updates or uninstalls.
pub const MAX_RUN_ITEMS: usize = 50;
/// The longest package ID.
pub const MAX_PACKAGE_ID_CHARS: usize = 200;
/// The longest version.
pub const MAX_VERSION_CHARS: usize = 64;
/// The longest installer arguments, uninstall command, or detection path.
pub const MAX_COMMAND_CHARS: usize = 2_000;
/// The most output kept from installing one application.
pub const MAX_ITEM_OUTPUT_CHARS: usize = 16 * 1024;
/// The most output, in bytes, a run's result keeps for all its
/// applications together, shared evenly, so the result stays well inside
/// the control connection's message limit.
pub const MAX_RUN_OUTPUT_BYTES: usize = 1024 * 1024;
/// How long an Agent lets one package manager's scan run, all of its
/// commands together.
pub const APPLICATION_SCAN_TIMEOUT_SECONDS: u64 = 15 * 60;
/// How long an Agent lets a whole scan run, every package manager
/// together. A device has at most two.
pub const APPLICATION_SCAN_TOTAL_SECONDS: u64 = 2 * APPLICATION_SCAN_TIMEOUT_SECONDS;
/// How long an Agent lets one install, update or uninstall run.
pub const APPLICATION_ITEM_TIMEOUT_SECONDS: u64 = 30 * 60;
/// How long an Agent works on one run's applications before failing what
/// is left of them; the scans before and after are on top. An application
/// started near the end gets only what is left.
pub const APPLICATION_RUN_TIMEOUT_SECONDS: u64 = 4 * 60 * 60;

/// Where an application comes from: a package manager, or an installer from
/// the toolbox's file library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSource {
    /// The Windows Package Manager.
    Winget,
    Chocolatey,
    /// A Homebrew formula.
    Homebrew,
    /// A Homebrew cask.
    HomebrewCask,
    /// A Flatpak in the system installation.
    Flatpak,
    Snap,
    /// An installer file from the toolbox library.
    Custom,
}

impl PackageSource {
    pub const ALL: [Self; 7] = [
        Self::Winget,
        Self::Chocolatey,
        Self::Homebrew,
        Self::HomebrewCask,
        Self::Flatpak,
        Self::Snap,
        Self::Custom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Winget => "winget",
            Self::Chocolatey => "chocolatey",
            Self::Homebrew => "homebrew",
            Self::HomebrewCask => "homebrew_cask",
            Self::Flatpak => "flatpak",
            Self::Snap => "snap",
            Self::Custom => "custom",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == text)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Winget => "winget",
            Self::Chocolatey => "Chocolatey",
            Self::Homebrew => "Homebrew",
            Self::HomebrewCask => "Homebrew cask",
            Self::Flatpak => "Flatpak",
            Self::Snap => "Snap",
            Self::Custom => "Custom installer",
        }
    }

    /// The platform a package manager runs on; `None` for a custom
    /// installer, which names its own.
    pub fn platform(self) -> Option<DevicePlatform> {
        match self {
            Self::Winget | Self::Chocolatey => Some(DevicePlatform::Windows),
            Self::Homebrew | Self::HomebrewCask => Some(DevicePlatform::Macos),
            Self::Flatpak | Self::Snap => Some(DevicePlatform::Linux),
            Self::Custom => None,
        }
    }

    /// Whether the manager reads package IDs ignoring case, so
    /// `mozilla.firefox` and `Mozilla.Firefox` are one package: winget,
    /// Chocolatey and Homebrew do. Flatpak application IDs are
    /// case-sensitive, and snap names are lowercase.
    pub fn ignores_case(self) -> bool {
        matches!(
            self,
            Self::Winget | Self::Chocolatey | Self::Homebrew | Self::HomebrewCask
        )
    }

    /// Whether `id` is a package ID this manager could have: 1 to
    /// [`MAX_PACKAGE_ID_CHARS`] of the characters its IDs use, starting with
    /// a letter or digit.
    pub fn valid_id(self, id: &str) -> bool {
        let extra: &[u8] = match self {
            // `Microsoft.VisualStudioCode`, `9NBLGGH4NNS1`, `Python.Python.3.12`.
            Self::Winget => b"._-+",
            Self::Chocolatey => b"._-",
            // `node@20`, `homebrew/cask-fonts/font-fira-code`, `c++filt`.
            Self::Homebrew | Self::HomebrewCask => b"._-+@/",
            // `org.mozilla.firefox`.
            Self::Flatpak => b"._-",
            Self::Snap => b"-",
            Self::Custom => return id.is_empty(),
        };
        !id.is_empty()
            && id.chars().count() <= MAX_PACKAGE_ID_CHARS
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || extra.contains(&byte))
            && !id.contains("..")
            // Snap names are lowercase.
            && (self != Self::Snap || !id.bytes().any(|byte| byte.is_ascii_uppercase()))
    }
}

/// Whether `version` can be passed to a package manager: 1 to
/// [`MAX_VERSION_CHARS`] letters, digits and `.`, `-`, `_`, `+`, `~`, `:`,
/// starting with a letter or digit.
pub fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_CHARS
        && version.as_bytes()[0].is_ascii_alphanumeric()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_+~:".contains(&byte))
}

/// Compares two versions the way people read them: numbers by value, other
/// parts by text, ignoring case, so `1.10` is newer than `1.9` and
/// `2.0.0-3` than `2.0.0-2`. Missing parts count as nothing, so `1.2` is
/// older than `1.2.1`.
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    fn parts(version: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let bytes = version.as_bytes();
        let mut start = None;
        let mut digits = false;
        for (index, byte) in bytes.iter().enumerate() {
            let alphanumeric = byte.is_ascii_alphanumeric();
            match start {
                Some(from) if !alphanumeric || byte.is_ascii_digit() != digits => {
                    parts.push(&version[from..index]);
                    start = alphanumeric.then_some(index);
                    digits = byte.is_ascii_digit();
                }
                None if alphanumeric => {
                    start = Some(index);
                    digits = byte.is_ascii_digit();
                }
                _ => {}
            }
        }
        if let Some(from) = start {
            parts.push(&version[from..]);
        }
        parts
    }
    let (left, right) = (parts(left.trim()), parts(right.trim()));
    for index in 0..left.len().max(right.len()) {
        let ordering = match (left.get(index), right.get(index)) {
            (Some(a), Some(b)) => {
                let numeric = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
                if numeric(a) && numeric(b) {
                    let a = a.trim_start_matches('0');
                    let b = b.trim_start_matches('0');
                    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                } else {
                    a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())
                }
            }
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Whether `name` matches `pattern`, ignoring case, where `*` in the
/// pattern matches any text: `7-Zip*` matches `7-Zip 24.08 (x64)`.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    let name = name.trim().to_lowercase();
    let pieces = pattern.split('*').collect::<Vec<_>>();
    if pieces.len() == 1 {
        return pattern == name;
    }
    let mut rest = name.as_str();
    for (index, piece) in pieces.iter().enumerate() {
        if index == 0 {
            let Some(after) = rest.strip_prefix(piece) else {
                return false;
            };
            rest = after;
        } else if index == pieces.len() - 1 {
            return rest.ends_with(piece);
        } else {
            match rest.find(piece) {
                Some(at) => rest = &rest[at + piece.len()..],
                None => return false,
            }
        }
    }
    true
}

/// How the Agent tells whether a custom installer's application is
/// installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DetectionRule {
    /// An installed application, as the inventory names it (`*` matches any
    /// text), at least `min_version` if given.
    Application {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_version: Option<String>,
    },
    /// A file or folder exists.
    File { path: String },
    /// A Windows registry key exists, such as
    /// `HKLM\SOFTWARE\Vendor\Product`, with the value `value` if given.
    Registry {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
}

impl DetectionRule {
    /// Why the rule can't be used, if it can't.
    pub fn problem(&self) -> Option<&'static str> {
        let fits =
            |text: &str| !text.trim().is_empty() && text.chars().count() <= MAX_COMMAND_CHARS;
        match self {
            Self::Application { name, min_version } => {
                if !fits(name) || name.chars().any(char::is_control) {
                    Some("the detection rule needs an application name")
                } else if min_version.as_deref().is_some_and(|v| !valid_version(v)) {
                    Some("the detection rule's minimum version isn't a version")
                } else {
                    None
                }
            }
            Self::File { path } => (!fits(path) || path.chars().any(char::is_control))
                .then_some("the detection rule needs a file path"),
            Self::Registry { key, value } => {
                let root = key
                    .split('\\')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                if !fits(key)
                    || key.chars().any(char::is_control)
                    || !matches!(
                        root.as_str(),
                        "HKLM"
                            | "HKEY_LOCAL_MACHINE"
                            | "HKCU"
                            | "HKEY_CURRENT_USER"
                            | "HKU"
                            | "HKEY_USERS"
                            | "HKCR"
                            | "HKEY_CLASSES_ROOT"
                    )
                {
                    Some(
                        "the detection rule needs a registry key starting with HKLM, HKCU, HKU or HKCR",
                    )
                } else if value.as_deref().is_some_and(|value| {
                    value.chars().count() > MAX_COMMAND_CHARS || value.chars().any(char::is_control)
                }) {
                    Some("the registry value name isn't valid")
                } else {
                    None
                }
            }
        }
    }
}

/// An installer file from the toolbox library, how to run it silently, and
/// how to tell it worked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomInstaller {
    /// The library file, which the Agent downloads for the run.
    pub file_id: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    /// The arguments that make it install silently, such as `/qn
    /// /norestart` for an MSI or `/S` for many EXEs.
    #[serde(default)]
    pub arguments: String,
    /// The command line that uninstalls it, run by `cmd.exe` on Windows and
    /// `/bin/sh` elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uninstall_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionRule>,
}

/// One way to install an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRef {
    pub source: PackageSource,
    /// The package manager's ID; empty for a custom installer.
    #[serde(default)]
    pub id: String,
    /// The version to install, where the library pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomInstaller>,
}

/// A package by manager and ID, as scans report and updates choose them.
///
/// Keys compare, hash and sort as their manager reads IDs: ignoring ASCII
/// case for winget, Chocolatey and Homebrew
/// ([`PackageSource::ignores_case`]), exactly otherwise. The ID keeps the
/// case it was written with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageKey {
    pub source: PackageSource,
    pub id: String,
}

impl PartialEq for PackageKey {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.folded() == other.folded()
    }
}

impl Eq for PackageKey {}

impl std::hash::Hash for PackageKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.source.hash(state);
        self.folded().hash(state);
    }
}

impl PartialOrd for PackageKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackageKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.source
            .cmp(&other.source)
            .then_with(|| self.folded().cmp(&other.folded()))
    }
}

impl PackageKey {
    /// The ID as keys compare it.
    fn folded(&self) -> std::borrow::Cow<'_, str> {
        if self.source.ignores_case() {
            std::borrow::Cow::Owned(self.id.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(&self.id)
        }
    }

    /// Whether `source` and `id` name this package.
    pub fn is(&self, source: PackageSource, id: &str) -> bool {
        self.source == source
            && if source.ignores_case() {
                self.id.eq_ignore_ascii_case(id)
            } else {
                self.id == id
            }
    }

    /// `winget:Mozilla.Firefox`, as the API names packages.
    pub fn text(&self) -> String {
        format!("{}:{}", self.source.as_str(), self.id)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (source, id) = text.split_once(':')?;
        let source = PackageSource::parse(source)?;
        (source != PackageSource::Custom && source.valid_id(id)).then(|| Self {
            source,
            id: id.to_owned(),
        })
    }
}

/// An application to install: its name, and its ways to install in order
/// of preference. The Agent uses the first whose package manager it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallItem {
    pub name: String,
    pub sources: Vec<PackageRef>,
}

/// Which outdated packages an update covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    /// Every outdated package.
    #[default]
    All,
    /// Only the packages named.
    Only,
    /// Every outdated package except those named.
    Except,
}

impl UpdateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only => "only",
            Self::Except => "except",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" => Some(Self::All),
            "only" => Some(Self::Only),
            "except" => Some(Self::Except),
            _ => None,
        }
    }
}

/// A package a pinned library application holds back from updates, and
/// the version the library pins it to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    #[serde(flatten)]
    pub package: PackageKey,
    pub version: String,
}

/// The outdated packages an update covers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateChoice {
    #[serde(default)]
    pub mode: UpdateMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageKey>,
    /// Packages the library pins: never updated past their version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<Held>,
}

impl UpdateChoice {
    /// The version the library pins `package` to, if it does.
    pub fn pinned(&self, package: &PackageKey) -> Option<&str> {
        self.held
            .iter()
            .find(|held| held.package == *package)
            .map(|held| held.version.as_str())
    }

    /// Whether the update covers `package`.
    pub fn covers(&self, package: &PackageKey) -> bool {
        if self.pinned(package).is_some() {
            return false;
        }
        match self.mode {
            UpdateMode::All => true,
            UpdateMode::Only => self.packages.contains(package),
            UpdateMode::Except => !self.packages.contains(package),
        }
    }
}

/// An application to uninstall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum UninstallItem {
    /// A package, by its manager.
    Package { name: String, package: PackageKey },
    /// A Windows program from the inventory, by its quiet uninstaller or its
    /// Windows Installer product code.
    Inventory {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// A custom installer's application, by its uninstall command.
    Command {
        name: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detection: Option<DetectionRule>,
    },
}

impl UninstallItem {
    pub fn name(&self) -> &str {
        match self {
            Self::Package { name, .. }
            | Self::Inventory { name, .. }
            | Self::Command { name, .. } => name,
        }
    }
}

/// What a run does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ApplicationOperation {
    /// Installs each application that isn't installed yet.
    Install {
        items: Vec<InstallItem>,
    },
    /// Scans, then updates the outdated packages `choice` covers.
    Update {
        choice: UpdateChoice,
    },
    Uninstall {
        items: Vec<UninstallItem>,
    },
}

/// A run the server asks an Agent for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationRunRequest {
    pub run_id: String,
    pub operation: ApplicationOperation,
}

/// A package manager the Agent found, and whether its scan worked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerState {
    pub source: PackageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A package a scan found installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannedPackage {
    pub source: PackageSource,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A newer version the manager offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
}

impl ScannedPackage {
    pub fn key(&self) -> PackageKey {
        PackageKey {
            source: self.source,
            id: self.id.clone(),
        }
    }

    pub fn outdated(&self) -> bool {
        self.available_version.is_some()
    }
}

/// What a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationScan {
    /// When it finished, by the device's clock.
    pub scanned_at_unix_ms: i64,
    /// Each supported package manager the device has.
    #[serde(default)]
    pub managers: Vec<ManagerState>,
    /// What those managers have installed, with updates they offer.
    #[serde(default)]
    pub packages: Vec<ScannedPackage>,
}

impl ApplicationScan {
    /// The scan with clean text, without unusable or repeated packages, and
    /// at most [`MAX_SCANNED_PACKAGES`] of them.
    pub fn sanitized(mut self) -> Self {
        for manager in &mut self.managers {
            manager.version = clean_optional(manager.version.as_deref());
            manager.error = clean_optional(manager.error.as_deref());
        }
        let mut seen = std::collections::HashSet::new();
        self.managers.retain(|manager| seen.insert(manager.source));
        let mut seen = std::collections::HashSet::new();
        self.packages = std::mem::take(&mut self.packages)
            .into_iter()
            .filter_map(|mut package| {
                package.id = clean_text(&package.id);
                if package.source == PackageSource::Custom || !package.source.valid_id(&package.id)
                {
                    return None;
                }
                package.name = clean_text(&package.name);
                if package.name.is_empty() {
                    package.name = package.id.clone();
                }
                package.version = clean_optional(package.version.as_deref());
                package.available_version = clean_optional(package.available_version.as_deref());
                Some(package)
            })
            .filter(|package| seen.insert(package.key()))
            .take(MAX_SCANNED_PACKAGES)
            .collect();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemOutcome {
    /// Installed, updated or uninstalled.
    Succeeded,
    /// Nothing to do: already installed, up to date, or not installed.
    Unchanged,
    /// Not for this device, and why: none of its ways to install is for a
    /// package manager the device has, or the library pins it. Not a
    /// failure.
    Skipped,
    Failed,
}

impl ItemOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "succeeded" => Some(Self::Succeeded),
            "unchanged" => Some(Self::Unchanged),
            "skipped" => Some(Self::Skipped),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// How one application of a run went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResult {
    /// The application's position in the request; for an update, which
    /// chooses its packages itself, its position among those updated.
    pub index: u32,
    pub name: String,
    /// The way it was installed, updated or uninstalled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PackageSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
    pub outcome: ItemOutcome,
    /// The version installed afterwards, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The end of what the installer or package manager wrote.
    #[serde(default)]
    pub output: String,
    /// It needs a restart to finish.
    #[serde(default)]
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Every application succeeded, had nothing to do, or was skipped.
    Succeeded,
    /// Some failed.
    Partial,
    /// Every one it tried failed, or the run couldn't start.
    Failed,
    /// Every application was skipped: none was for this device.
    Skipped,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    /// The status of a run whose applications went as `items` did.
    pub fn of(items: &[ItemResult]) -> Self {
        let failed = items
            .iter()
            .filter(|item| item.outcome == ItemOutcome::Failed)
            .count();
        if !items.is_empty()
            && items
                .iter()
                .all(|item| item.outcome == ItemOutcome::Skipped)
        {
            Self::Skipped
        } else if failed == 0 {
            Self::Succeeded
        } else if failed
            == items
                .iter()
                .filter(|item| item.outcome != ItemOutcome::Skipped)
                .count()
        {
            Self::Failed
        } else {
            Self::Partial
        }
    }
}

/// How a run went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationRunResult {
    pub status: RunStatus,
    /// Why the run as a whole failed, or was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub items: Vec<ItemResult>,
}

impl ApplicationRunResult {
    /// A run that did nothing because of `error`.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed,
            error: Some(error.into()),
            items: Vec::new(),
        }
    }

    /// The result with clean text, at most [`MAX_RUN_ITEMS`] applications,
    /// and each one's output cut to its end and to its share of
    /// [`MAX_RUN_OUTPUT_BYTES`].
    pub fn sanitized(mut self) -> Self {
        self.items.truncate(MAX_RUN_ITEMS);
        let share = MAX_RUN_OUTPUT_BYTES / self.items.len().max(1);
        for item in &mut self.items {
            item.name = clean_text(&item.name);
            item.package_id = clean_optional(item.package_id.as_deref());
            item.version = clean_optional(item.version.as_deref());
            item.error = item
                .error
                .as_deref()
                .map(clean_text)
                .filter(|error| !error.is_empty());
            item.output = tail_bytes(clean_output(&item.output), share);
        }
        self.error = clean_optional(self.error.as_deref());
        self
    }
}

/// The end of `text`, at most `bytes` long, cut between characters.
fn tail_bytes(text: String, bytes: usize) -> String {
    if text.len() <= bytes {
        return text;
    }
    let mut start = text.len() - bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// Program output as a result keeps it: its last [`MAX_ITEM_OUTPUT_CHARS`]
/// characters, with line breaks and tabs but no other control characters,
/// and with progress bars that rewrite a line (`\r`) reduced to their last
/// state.
pub fn clean_output(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let mut current = String::new();
        for segment in line.split('\r') {
            if !segment.trim().is_empty() {
                current = segment.to_owned();
            }
        }
        let mut cleaned = String::with_capacity(current.len());
        for character in current.chars() {
            match character {
                '\u{8}' => {
                    cleaned.pop();
                }
                '\t' => cleaned.push('\t'),
                character if character.is_control() => {}
                character => cleaned.push(character),
            }
        }
        let cleaned = cleaned.trim_end();
        // Spinners and progress bars on a line of their own.
        if cleaned.trim().chars().all(|c| "-\\|/█▒░ ".contains(c)) && !cleaned.trim().is_empty()
        {
            continue;
        }
        lines.push(cleaned.to_owned());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    let text = lines.join("\n");
    let count = text.chars().count();
    if count <= MAX_ITEM_OUTPUT_CHARS {
        text
    } else {
        text.chars().skip(count - MAX_ITEM_OUTPUT_CHARS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: PackageSource, id: &str) -> PackageKey {
        PackageKey {
            source,
            id: id.to_owned(),
        }
    }

    #[test]
    fn keys_compare_as_their_manager_reads_ids() {
        use std::collections::HashSet;
        let pinned = key(PackageSource::Winget, "mozilla.firefox");
        let scanned = key(PackageSource::Winget, "Mozilla.Firefox");
        assert_eq!(pinned, scanned);
        assert_eq!(pinned.cmp(&scanned), Ordering::Equal);
        assert!(HashSet::from([pinned.clone()]).contains(&scanned));
        assert_eq!(scanned.id, "Mozilla.Firefox", "the ID keeps its case");
        assert_eq!(
            key(PackageSource::Chocolatey, "googlechrome"),
            key(PackageSource::Chocolatey, "GoogleChrome")
        );
        assert_eq!(
            key(PackageSource::HomebrewCask, "Firefox"),
            key(PackageSource::HomebrewCask, "firefox")
        );
        assert_ne!(
            key(PackageSource::Flatpak, "org.mozilla.Firefox"),
            key(PackageSource::Flatpak, "org.mozilla.firefox")
        );
        assert_ne!(
            key(PackageSource::Winget, "Git.Git"),
            key(PackageSource::Chocolatey, "git.git")
        );
        assert!(scanned.is(PackageSource::Winget, "MOZILLA.FIREFOX"));
        assert!(!key(PackageSource::Flatpak, "a.B").is(PackageSource::Flatpak, "a.b"));

        let choice = UpdateChoice {
            mode: UpdateMode::Except,
            packages: vec![key(PackageSource::Chocolatey, "googlechrome")],
            held: vec![Held {
                package: pinned,
                version: "130.0".into(),
            }],
        };
        assert!(!choice.covers(&scanned), "held whatever the case");
        assert!(!choice.covers(&key(PackageSource::Chocolatey, "GoogleChrome")));
        assert!(choice.covers(&key(PackageSource::Chocolatey, "git")));
        let mut keys = vec![
            key(PackageSource::Winget, "Git.Git"),
            key(PackageSource::Winget, "git.git"),
        ];
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn names_round_trip() {
        for source in PackageSource::ALL {
            assert_eq!(PackageSource::parse(source.as_str()), Some(source));
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                serde_json::Value::String(source.as_str().into())
            );
        }
        for mode in [UpdateMode::All, UpdateMode::Only, UpdateMode::Except] {
            assert_eq!(UpdateMode::parse(mode.as_str()), Some(mode));
        }
        for outcome in [
            ItemOutcome::Succeeded,
            ItemOutcome::Unchanged,
            ItemOutcome::Failed,
        ] {
            assert_eq!(ItemOutcome::parse(outcome.as_str()), Some(outcome));
        }
        assert_eq!(
            PackageSource::Winget.platform(),
            Some(DevicePlatform::Windows)
        );
        assert_eq!(
            PackageSource::HomebrewCask.platform(),
            Some(DevicePlatform::Macos)
        );
        assert_eq!(PackageSource::Snap.platform(), Some(DevicePlatform::Linux));
        assert_eq!(PackageSource::Custom.platform(), None);
    }

    #[test]
    fn package_ids_cannot_be_options_or_shell_text() {
        for (source, id) in [
            (PackageSource::Winget, "Mozilla.Firefox"),
            (PackageSource::Winget, "9NBLGGH4NNS1"),
            (PackageSource::Winget, "Python.Python.3.12"),
            (PackageSource::Winget, "Notepad++.Notepad++"),
            (PackageSource::Chocolatey, "googlechrome"),
            (PackageSource::Chocolatey, "dotnet-8.0-runtime"),
            (PackageSource::Homebrew, "node@20"),
            (
                PackageSource::Homebrew,
                "homebrew/cask-fonts/font-fira-code",
            ),
            (PackageSource::HomebrewCask, "visual-studio-code"),
            (PackageSource::Flatpak, "org.mozilla.firefox"),
            (PackageSource::Snap, "code"),
        ] {
            assert!(source.valid_id(id), "{source:?} {id}");
        }
        for (source, id) in [
            (PackageSource::Winget, "--override"),
            (PackageSource::Winget, "-h"),
            (PackageSource::Winget, "Mozilla.Firefox; calc"),
            (PackageSource::Winget, "a b"),
            (PackageSource::Chocolatey, "x&calc"),
            (PackageSource::Chocolatey, "x+y"),
            (PackageSource::Homebrew, "../../etc/passwd"),
            (PackageSource::Homebrew, "$(id)"),
            (PackageSource::Flatpak, "org/mozilla"),
            (PackageSource::Snap, "Code"),
            (PackageSource::Snap, "code.x"),
            (PackageSource::Winget, ""),
            (PackageSource::Custom, "anything"),
        ] {
            assert!(!source.valid_id(id), "{source:?} {id}");
        }
        assert!(!PackageSource::Winget.valid_id(&"a".repeat(MAX_PACKAGE_ID_CHARS + 1)));
        assert!(PackageSource::Custom.valid_id(""));
        assert!(valid_version("1.2.3-rc.1+build~2:1"));
        assert!(!valid_version("-1"));
        assert!(!valid_version("1 2"));
        assert!(!valid_version("1;2"));
        assert!(!valid_version(""));
        assert_eq!(
            PackageKey::parse("winget:Mozilla.Firefox"),
            Some(PackageKey {
                source: PackageSource::Winget,
                id: "Mozilla.Firefox".into()
            })
        );
        assert_eq!(PackageKey::parse("winget:--all"), None);
        assert_eq!(PackageKey::parse("custom:"), None);
        assert_eq!(PackageKey::parse("Mozilla.Firefox"), None);
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        use Ordering::*;
        for (left, right, expected) in [
            ("1.10", "1.9", Greater),
            ("1.2", "1.2.0.1", Less),
            ("1.2", "1.2", Equal),
            ("01.2", "1.2", Equal),
            ("2.0.0-3", "2.0.0-2", Greater),
            ("130.0", "129.0.2", Greater),
            ("1.0b2", "1.0b10", Less),
            ("1.0RC", "1.0rc", Equal),
            ("8.0.30", "8.0.33", Less),
            ("13.791.0.6304", "13.822.0.6335", Less),
            ("v2", "v10", Less),
        ] {
            assert_eq!(compare_versions(left, right), expected, "{left} {right}");
        }
    }

    #[test]
    fn names_match_patterns() {
        assert!(name_matches("7-Zip*", "7-Zip 24.08 (x64)"));
        assert!(name_matches("google chrome", "Google Chrome"));
        assert!(!name_matches("Google Chrome", "Google Chrome Beta"));
        assert!(name_matches("*Chrome*", "Google Chrome Beta"));
        assert!(name_matches(
            "Microsoft * Runtime*",
            "Microsoft Windows Desktop Runtime - 8.0.30 (x64)"
        ));
        assert!(!name_matches("*Firefox", "Firefox ESR"));
        assert!(name_matches("*", "anything"));
    }

    #[test]
    fn detection_rules_check_their_fields() {
        let rule = |json: serde_json::Value| serde_json::from_value::<DetectionRule>(json).unwrap();
        assert_eq!(
            rule(
                serde_json::json!({"kind": "application", "name": "7-Zip*", "min_version": "24.0"})
            )
            .problem(),
            None
        );
        assert!(
            rule(serde_json::json!({"kind": "application", "name": " "}))
                .problem()
                .is_some()
        );
        assert!(
            rule(serde_json::json!({"kind": "application", "name": "x", "min_version": "1 2"}))
                .problem()
                .is_some()
        );
        assert_eq!(
            rule(serde_json::json!({"kind": "file", "path": "C:\\Program Files\\App\\app.exe"}))
                .problem(),
            None
        );
        assert_eq!(
            rule(serde_json::json!({"kind": "registry", "key": "HKLM\\SOFTWARE\\Vendor"}))
                .problem(),
            None
        );
        assert!(
            rule(serde_json::json!({"kind": "registry", "key": "SOFTWARE\\Vendor"}))
                .problem()
                .is_some()
        );
    }

    #[test]
    fn updates_cover_their_choice_but_never_held_packages() {
        let key = |id: &str| PackageKey {
            source: PackageSource::Winget,
            id: id.into(),
        };
        let held = |id: &str| Held {
            package: key(id),
            version: "1.0".into(),
        };
        let all = UpdateChoice {
            held: vec![held("pinned")],
            ..Default::default()
        };
        assert!(all.covers(&key("a")) && !all.covers(&key("pinned")));
        assert_eq!(all.pinned(&key("PINNED")), Some("1.0"));
        assert_eq!(all.pinned(&key("a")), None);
        let only = UpdateChoice {
            mode: UpdateMode::Only,
            packages: vec![key("a"), key("pinned")],
            held: vec![held("pinned")],
        };
        assert!(only.covers(&key("a")) && !only.covers(&key("b")) && !only.covers(&key("pinned")));
        let except = UpdateChoice {
            mode: UpdateMode::Except,
            packages: vec![key("a")],
            held: vec![],
        };
        assert!(!except.covers(&key("a")) && except.covers(&key("b")));
    }

    #[test]
    fn scans_drop_unusable_and_repeated_packages() {
        let package = |id: &str| ScannedPackage {
            source: PackageSource::Winget,
            id: id.into(),
            name: String::new(),
            version: Some("1".into()),
            available_version: Some(" ".into()),
        };
        let scan = ApplicationScan {
            scanned_at_unix_ms: 1,
            managers: vec![
                ManagerState {
                    source: PackageSource::Winget,
                    version: None,
                    error: None,
                },
                ManagerState {
                    source: PackageSource::Winget,
                    version: None,
                    error: Some("again".into()),
                },
            ],
            packages: vec![
                package("Git.Git"),
                package("Git.Git"),
                package("-x"),
                package("Bad…"),
            ],
        }
        .sanitized();
        assert_eq!(scan.managers.len(), 1);
        assert_eq!(scan.packages.len(), 1);
        assert_eq!(scan.packages[0].name, "Git.Git");
        assert!(!scan.packages[0].outdated(), "a blank version is none");
    }

    #[test]
    fn output_keeps_the_last_state_of_progress_lines() {
        let text = "Found Git [Git.Git]\r\n  -\r  \\\r  |\r\n  ██████████▒▒▒▒  1.00 MB / 2.00 MB\r  ████████████████  2.00 MB / 2.00 MB\nSuccessfully installed\n\n";
        assert_eq!(
            clean_output(text),
            "Found Git [Git.Git]\n  ████████████████  2.00 MB / 2.00 MB\nSuccessfully installed"
        );
        let long = "x".repeat(MAX_ITEM_OUTPUT_CHARS + 10);
        assert_eq!(clean_output(&long).chars().count(), MAX_ITEM_OUTPUT_CHARS);
        assert_eq!(clean_output("ab\u{8}c\u{1b}d"), "acd");
    }

    #[test]
    fn run_status_follows_its_items() {
        let item = |outcome| ItemResult {
            index: 0,
            name: "A".into(),
            source: None,
            package_id: None,
            outcome,
            version: None,
            exit_code: None,
            error: None,
            output: String::new(),
            reboot_required: false,
        };
        assert_eq!(RunStatus::of(&[]), RunStatus::Succeeded);
        assert_eq!(
            RunStatus::of(&[item(ItemOutcome::Unchanged), item(ItemOutcome::Succeeded)]),
            RunStatus::Succeeded
        );
        assert_eq!(
            RunStatus::of(&[item(ItemOutcome::Failed), item(ItemOutcome::Succeeded)]),
            RunStatus::Partial
        );
        assert_eq!(
            RunStatus::of(&[item(ItemOutcome::Failed)]),
            RunStatus::Failed
        );
        // Skipped applications aren't failures, and a run that skipped
        // every one was for another device.
        assert_eq!(
            RunStatus::of(&[item(ItemOutcome::Skipped), item(ItemOutcome::Skipped)]),
            RunStatus::Skipped
        );
        assert_eq!(
            RunStatus::of(&[item(ItemOutcome::Skipped), item(ItemOutcome::Succeeded)]),
            RunStatus::Succeeded
        );
        assert_eq!(
            RunStatus::of(&[item(ItemOutcome::Skipped), item(ItemOutcome::Failed)]),
            RunStatus::Failed,
            "every one it tried failed"
        );
    }

    #[test]
    fn results_fit_a_control_message() {
        // Every application wrote its most, in characters that take four
        // bytes, and more applications than a run has.
        let output = format!(
            "first line
{}
the end",
            "😀".repeat(MAX_ITEM_OUTPUT_CHARS)
        );
        let items = (0..MAX_SCANNED_PACKAGES)
            .map(|index| ItemResult {
                index: index as u32,
                name: "A".repeat(10_000),
                source: Some(PackageSource::Winget),
                package_id: Some("A.B".into()),
                outcome: ItemOutcome::Failed,
                version: None,
                exit_code: Some(1),
                error: Some("e".repeat(10_000)),
                output: output.clone(),
                reboot_required: false,
            })
            .collect();
        let result = ApplicationRunResult {
            status: RunStatus::Failed,
            error: None,
            items,
        }
        .sanitized();
        assert_eq!(result.items.len(), MAX_RUN_ITEMS);
        for item in &result.items {
            assert!(item.output.len() <= MAX_RUN_OUTPUT_BYTES / MAX_RUN_ITEMS);
            assert!(item.output.ends_with("the end"), "the end is kept");
            assert!(output.ends_with(&item.output));
        }
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            json.len() < crate::MAX_SYSTEM_INFO_BYTES / 2,
            "{} bytes",
            json.len()
        );
        // One application keeps its whole output, up to its own limit.
        let one = ApplicationRunResult {
            items: result.items[..1].to_vec(),
            ..result
        };
        let mut one = one;
        one.items[0].output = output.clone();
        let one = one.sanitized();
        assert_eq!(one.items[0].output.chars().count(), MAX_ITEM_OUTPUT_CHARS);
    }
}
