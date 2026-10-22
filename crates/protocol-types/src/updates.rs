//! Operating system updates: what an Agent finds pending when it scans, and
//! how an install the server asks for went. See docs/patch-management.md.
//!
//! The server asks for scans and installs on the control connection
//! ([`crate::AgentCommand::ScanUpdates`], [`crate::AgentCommand::InstallUpdates`]),
//! and the Agent reports on it ([`crate::AgentStatusMessage::UpdateScan`],
//! [`crate::AgentStatusMessage::UpdateProgress`],
//! [`crate::AgentStatusMessage::UpdateInstall`]).
use serde::{Deserialize, Serialize};

use crate::{clean_optional, clean_text};

/// The most pending updates a scan lists.
pub const MAX_PENDING_UPDATES: usize = 5_000;
/// The longest identifier an update may have.
pub const MAX_UPDATE_ID_CHARS: usize = 200;
/// How long an Agent lets a scan run before giving up on it.
pub const UPDATE_SCAN_TIMEOUT_SECONDS: u64 = 30 * 60;
/// How long an Agent lets an install run before giving up on it.
pub const UPDATE_INSTALL_TIMEOUT_SECONDS: u64 = 4 * 60 * 60;
/// The longest a restart after an install may be put off.
pub const MAX_REBOOT_DELAY_MINUTES: u32 = 24 * 60;

/// The update service or package manager that found and installs updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateManager {
    /// The Windows Update Agent.
    WindowsUpdate,
    /// macOS's `softwareupdate`.
    Softwareupdate,
    Apt,
    Dnf,
    Yum,
    Pacman,
}

impl UpdateManager {
    pub const ALL: [Self; 6] = [
        Self::WindowsUpdate,
        Self::Softwareupdate,
        Self::Apt,
        Self::Dnf,
        Self::Yum,
        Self::Pacman,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::WindowsUpdate => "windows_update",
            Self::Softwareupdate => "softwareupdate",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Yum => "yum",
            Self::Pacman => "pacman",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|manager| manager.as_str() == text)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::WindowsUpdate => "Windows Update",
            Self::Softwareupdate => "Software Update",
            Self::Apt => "APT",
            Self::Dnf => "DNF",
            Self::Yum => "YUM",
            Self::Pacman => "pacman",
        }
    }

    /// Whether it installs chosen updates on their own. Arch Linux supports
    /// only upgrading everything at once, so pacman installs all or nothing.
    pub fn installs_selected(self) -> bool {
        self != Self::Pacman
    }
}

/// What kind of update it is. Windows Update names each update's
/// classification; Linux package managers say which updates fix security
/// issues; macOS updates are classified by what they update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateCategory {
    Security,
    Critical,
    /// Antivirus definitions and similar data.
    Definition,
    Driver,
    FeaturePack,
    ServicePack,
    UpdateRollup,
    /// A new release of the operating system.
    Upgrade,
    Tool,
    /// Every other update, such as a package's bug fix release.
    Other,
}

impl UpdateCategory {
    pub const ALL: [Self; 10] = [
        Self::Security,
        Self::Critical,
        Self::Definition,
        Self::Driver,
        Self::FeaturePack,
        Self::ServicePack,
        Self::UpdateRollup,
        Self::Upgrade,
        Self::Tool,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Security => "security",
            Self::Critical => "critical",
            Self::Definition => "definition",
            Self::Driver => "driver",
            Self::FeaturePack => "feature_pack",
            Self::ServicePack => "service_pack",
            Self::UpdateRollup => "update_rollup",
            Self::Upgrade => "upgrade",
            Self::Tool => "tool",
            Self::Other => "other",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.as_str() == text)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Security => "Security",
            Self::Critical => "Critical",
            Self::Definition => "Definition",
            Self::Driver => "Driver",
            Self::FeaturePack => "Feature pack",
            Self::ServicePack => "Service pack",
            Self::UpdateRollup => "Update rollup",
            Self::Upgrade => "Upgrade",
            Self::Tool => "Tool",
            Self::Other => "Other",
        }
    }
}

/// How severe the issues an update fixes are, as Microsoft's Security
/// Response Center or a distribution's advisory rates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSeverity {
    Critical,
    Important,
    Moderate,
    Low,
}

impl UpdateSeverity {
    pub const ALL: [Self; 4] = [Self::Critical, Self::Important, Self::Moderate, Self::Low];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Important => "important",
            Self::Moderate => "moderate",
            Self::Low => "low",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|severity| severity.as_str() == text)
    }

    /// A rating as a vendor writes it, such as `Important` or `Medium`.
    pub fn from_vendor(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "important" | "high" => Some(Self::Important),
            "moderate" | "medium" => Some(Self::Moderate),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// An update the computer doesn't have yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUpdate {
    /// What an install names it by: Windows Update's update ID, a macOS
    /// label, or a package's name (with its architecture where the package
    /// manager needs it, such as `openssl.x86_64`).
    pub id: String,
    pub title: String,
    /// The Knowledge Base article of a Windows update, such as `KB5034441`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kb: Option<String>,
    /// The version it installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The version installed now, for a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    pub category: UpdateCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<UpdateSeverity>,
    /// The download size, where the platform says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Installing it may need a restart.
    #[serde(default)]
    pub reboot_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at_unix_ms: Option<i64>,
    /// Where it comes from: a repository, a pocket such as
    /// `noble-security`, or an advisory such as `ALSA-2025:1234`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl PendingUpdate {
    /// The update with clean text, or `None` if it has no usable ID.
    pub fn sanitized(mut self) -> Option<Self> {
        self.id = clean_text(&self.id);
        if self.id.is_empty() || self.id.chars().count() > MAX_UPDATE_ID_CHARS {
            return None;
        }
        self.title = clean_text(&self.title);
        if self.title.is_empty() {
            self.title = self.id.clone();
        }
        self.kb = clean_optional(self.kb.as_deref());
        self.version = clean_optional(self.version.as_deref());
        self.current_version = clean_optional(self.current_version.as_deref());
        self.source = clean_optional(self.source.as_deref());
        Some(self)
    }
}

/// What a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateScan {
    /// When it finished, by the device's clock.
    pub scanned_at_unix_ms: i64,
    /// `None` when the computer has no update service or package manager
    /// the Agent supports.
    #[serde(default)]
    pub manager: Option<UpdateManager>,
    #[serde(default)]
    pub updates: Vec<PendingUpdate>,
    /// The computer must restart to finish installing updates.
    #[serde(default)]
    pub reboot_pending: bool,
    /// Why the scan failed. A failed scan's other fields say nothing, except
    /// `reboot_pending` when it could still be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateScan {
    /// The scan with clean text, without duplicate or unusable updates, and
    /// at most [`MAX_PENDING_UPDATES`] of them.
    pub fn sanitized(mut self) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.updates = std::mem::take(&mut self.updates)
            .into_iter()
            .filter_map(PendingUpdate::sanitized)
            .filter(|update| seen.insert(update.id.clone()))
            .take(MAX_PENDING_UPDATES)
            .collect();
        self.error = clean_optional(self.error.as_deref());
        self
    }
}

/// When the Agent restarts the computer after installing updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebootBehavior {
    Never,
    /// When an installed update, or the computer, says it needs a restart.
    IfRequired,
    Always,
}

impl RebootBehavior {
    pub const ALL: [Self; 3] = [Self::Never, Self::IfRequired, Self::Always];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::IfRequired => "if_required",
            Self::Always => "always",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|behavior| behavior.as_str() == text)
    }
}

/// Which pending updates an install covers: those with `ids`, or else those
/// in `categories`, or else every pending update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSelection {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<UpdateCategory>,
}

impl UpdateSelection {
    pub fn all(&self) -> bool {
        self.ids.is_empty() && self.categories.is_empty()
    }

    /// Whether `update` is chosen.
    pub fn covers(&self, update: &PendingUpdate) -> bool {
        if !self.ids.is_empty() {
            return self.ids.contains(&update.id);
        }
        self.categories.is_empty() || self.categories.contains(&update.category)
    }
}

/// An install the server asks an Agent for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInstallRequest {
    pub run_id: String,
    #[serde(default)]
    pub selection: UpdateSelection,
    pub reboot: RebootBehavior,
    /// How long the signed-in user is warned before a restart.
    pub reboot_delay_minutes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateItemOutcome {
    Installed,
    Failed,
}

impl UpdateItemOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "installed" => Some(Self::Installed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// How one update of an install went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInstallItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kb: Option<String>,
    pub category: UpdateCategory,
    pub outcome: UpdateItemOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The update needs a restart to finish.
    #[serde(default)]
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateInstallStatus {
    /// Every chosen update installed, or none was pending.
    Succeeded,
    /// Some installed and some failed.
    Partial,
    /// None installed, or the install couldn't start.
    Failed,
}

impl UpdateInstallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }

    /// The status of an install whose updates went as `items` did.
    pub fn of(items: &[UpdateInstallItem]) -> Self {
        let failed = items
            .iter()
            .filter(|item| item.outcome == UpdateItemOutcome::Failed)
            .count();
        if failed == 0 {
            Self::Succeeded
        } else if failed == items.len() {
            Self::Failed
        } else {
            Self::Partial
        }
    }
}

/// How an install went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInstallResult {
    pub status: UpdateInstallStatus,
    /// Why the install as a whole failed, such as a package manager that
    /// another program holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub items: Vec<UpdateInstallItem>,
    /// The computer must restart to finish the install.
    #[serde(default)]
    pub reboot_required: bool,
    /// When the Agent set the computer to restart, by the device's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot_scheduled_at_unix_ms: Option<i64>,
    /// Why a restart the install called for couldn't be set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot_error: Option<String>,
}

impl UpdateInstallResult {
    /// A failed install that installed nothing.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: UpdateInstallStatus::Failed,
            error: Some(error.into()),
            items: Vec::new(),
            reboot_required: false,
            reboot_scheduled_at_unix_ms: None,
            reboot_error: None,
        }
    }

    pub fn sanitized(mut self) -> Self {
        self.items.truncate(MAX_PENDING_UPDATES);
        for item in &mut self.items {
            item.id = clean_text(&item.id);
            item.title = clean_text(&item.title);
            item.kb = clean_optional(item.kb.as_deref());
            item.error = clean_optional(item.error.as_deref());
        }
        self.items
            .retain(|item| !item.id.is_empty() && item.id.chars().count() <= MAX_UPDATE_ID_CHARS);
        self.error = clean_optional(self.error.as_deref());
        self.reboot_error = clean_optional(self.reboot_error.as_deref());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(id: &str, category: UpdateCategory) -> PendingUpdate {
        PendingUpdate {
            id: id.into(),
            title: String::new(),
            kb: None,
            version: Some("1.2".into()),
            current_version: None,
            category,
            severity: None,
            size_bytes: None,
            reboot_required: false,
            released_at_unix_ms: None,
            source: None,
        }
    }

    #[test]
    fn names_round_trip() {
        for manager in UpdateManager::ALL {
            assert_eq!(UpdateManager::parse(manager.as_str()), Some(manager));
            assert_eq!(
                serde_json::to_value(manager).unwrap(),
                serde_json::Value::String(manager.as_str().into())
            );
        }
        for category in UpdateCategory::ALL {
            assert_eq!(UpdateCategory::parse(category.as_str()), Some(category));
            assert_eq!(
                serde_json::to_value(category).unwrap(),
                serde_json::Value::String(category.as_str().into())
            );
        }
        for severity in UpdateSeverity::ALL {
            assert_eq!(UpdateSeverity::parse(severity.as_str()), Some(severity));
        }
        for behavior in RebootBehavior::ALL {
            assert_eq!(RebootBehavior::parse(behavior.as_str()), Some(behavior));
        }
        assert_eq!(
            UpdateSeverity::from_vendor(" Medium"),
            Some(UpdateSeverity::Moderate)
        );
        assert_eq!(UpdateSeverity::from_vendor("Unspecified"), None);
        assert!(!UpdateManager::Pacman.installs_selected());
        assert!(UpdateManager::Apt.installs_selected());
    }

    #[test]
    fn scans_drop_unusable_and_repeated_updates() {
        let scan = UpdateScan {
            scanned_at_unix_ms: 1,
            manager: Some(UpdateManager::Apt),
            updates: vec![
                update(" openssl\n", UpdateCategory::Security),
                update("openssl", UpdateCategory::Other),
                update("", UpdateCategory::Other),
                update(&"x".repeat(MAX_UPDATE_ID_CHARS + 1), UpdateCategory::Other),
            ],
            reboot_pending: true,
            error: Some("  ".into()),
        }
        .sanitized();
        assert_eq!(scan.updates.len(), 1);
        assert_eq!(scan.updates[0].id, "openssl");
        assert_eq!(
            scan.updates[0].title, "openssl",
            "a missing title is its ID"
        );
        assert_eq!(scan.updates[0].category, UpdateCategory::Security);
        assert_eq!(scan.error, None);
    }

    #[test]
    fn selections_choose_by_id_then_category() {
        let security = update("a", UpdateCategory::Security);
        let driver = update("b", UpdateCategory::Driver);
        let all = UpdateSelection::default();
        assert!(all.all() && all.covers(&security) && all.covers(&driver));
        let categories = UpdateSelection {
            ids: vec![],
            categories: vec![UpdateCategory::Security],
        };
        assert!(categories.covers(&security) && !categories.covers(&driver));
        let ids = UpdateSelection {
            ids: vec!["b".into()],
            categories: vec![UpdateCategory::Security],
        };
        assert!(!ids.covers(&security) && ids.covers(&driver));
    }

    #[test]
    fn install_status_follows_its_items() {
        let item = |outcome| UpdateInstallItem {
            id: "a".into(),
            title: "A".into(),
            kb: None,
            category: UpdateCategory::Other,
            outcome,
            error: None,
            reboot_required: false,
        };
        assert_eq!(UpdateInstallStatus::of(&[]), UpdateInstallStatus::Succeeded);
        assert_eq!(
            UpdateInstallStatus::of(&[item(UpdateItemOutcome::Installed)]),
            UpdateInstallStatus::Succeeded
        );
        assert_eq!(
            UpdateInstallStatus::of(&[
                item(UpdateItemOutcome::Installed),
                item(UpdateItemOutcome::Failed)
            ]),
            UpdateInstallStatus::Partial
        );
        assert_eq!(
            UpdateInstallStatus::of(&[item(UpdateItemOutcome::Failed)]),
            UpdateInstallStatus::Failed
        );
    }
}
