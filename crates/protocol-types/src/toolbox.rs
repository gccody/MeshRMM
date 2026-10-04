//! The toolbox: scripts and files a company keeps in the dashboard, which
//! technicians run on an Agent or send to it, from the dashboard or from a
//! remote session.
//!
//! The server stores the toolbox and hands each run or delivery to the Agent
//! over its coordinator connection. The Agent reports the outcome over HTTPS,
//! so a result survives a signaling reconnect.

use serde::{Deserialize, Serialize};

/// A script's source, in UTF-8. The server stores it and sends it to the
/// Agent inside one coordinator message.
pub const MAX_SCRIPT_BODY_BYTES: usize = 128 * 1024;
/// Each of a run's output streams keeps at most this much; the rest is cut.
pub const MAX_SCRIPT_OUTPUT_BYTES: usize = 512 * 1024;
pub const MAX_SCRIPT_DESCRIPTION_BYTES: usize = 1000;
/// How long a run may take before the Agent stops it.
pub const DEFAULT_SCRIPT_TIMEOUT_SECONDS: u32 = 300;
pub const MIN_SCRIPT_TIMEOUT_SECONDS: u32 = 10;
pub const MAX_SCRIPT_TIMEOUT_SECONDS: u32 = 3600;
/// Script names, in characters.
pub const MAX_TOOLBOX_NAME_CHARS: usize = 120;
/// File names, in characters, as Windows allows them.
pub const MAX_FILE_NAME_CHARS: usize = 255;
/// A folder path: up to [`MAX_FOLDER_DEPTH`] names joined by `/`.
pub const MAX_FOLDER_BYTES: usize = 255;
pub const MAX_FOLDER_DEPTH: usize = 8;
pub const MAX_FOLDER_NAME_CHARS: usize = 64;
/// A library file. Cloudflare refuses request bodies over 100 MB, and the
/// dashboard uploads a file in one request.
pub const MAX_TOOLBOX_FILE_BYTES: u64 = 95 * 1024 * 1024;

/// The interpreter a script runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptLanguage {
    Powershell,
    Cmd,
    /// A zsh script, for Macs.
    Shell,
}

impl ScriptLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Powershell => "powershell",
            Self::Cmd => "cmd",
            Self::Shell => "shell",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "powershell" => Some(Self::Powershell),
            "cmd" => Some(Self::Cmd),
            "shell" => Some(Self::Shell),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Powershell => "PowerShell",
            Self::Cmd => "Command Prompt",
            Self::Shell => "Shell (zsh)",
        }
    }
}

/// The account a script runs as. A run as the user falls back to SYSTEM
/// when nobody is signed in to the computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAs {
    System,
    User,
}

impl RunAs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "SYSTEM",
            Self::User => "Signed-in user",
        }
    }
}

/// What the server asks the Agent to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRunRequest {
    pub run_id: String,
    pub language: ScriptLanguage,
    pub body: String,
    pub run_as: RunAs,
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptRunStatus {
    /// Sent to the Agent, which has not reported back yet.
    Pending,
    /// The script ran and exited; its exit code says how it went.
    Completed,
    /// The script could not be started.
    Failed,
    /// The script ran past its timeout and the Agent stopped it.
    TimedOut,
    /// The Agent never reported back, for example because it went offline.
    Lost,
}

impl ScriptRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Lost => "lost",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "timed_out" => Some(Self::TimedOut),
            "lost" => Some(Self::Lost),
            _ => None,
        }
    }

    pub fn finished(self) -> bool {
        self != Self::Pending
    }
}

/// How a run went, as the Agent reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRunReport {
    /// Completed, failed or timed out.
    pub status: ScriptRunStatus,
    /// The account the script ran as, such as `SYSTEM` or `DOMAIN\user`.
    pub ran_as: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    /// Some output was cut at [`MAX_SCRIPT_OUTPUT_BYTES`].
    #[serde(default)]
    pub output_truncated: bool,
    /// Why the script could not run, or was stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where the Agent saves a delivered file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDeliveryDestination {
    /// The signed-in user's Documents transfer folder, where the viewer's
    /// own file transfers go. Public Documents when nobody is signed in.
    User,
    /// The Public Documents transfer folder, which is the Documents folder
    /// of the background desktop.
    Public,
}

impl FileDeliveryDestination {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Public => "public",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "public" => Some(Self::Public),
            _ => None,
        }
    }
}

/// What the server asks the Agent to download and save. The Agent fetches
/// the content from the server with its own credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDeliveryRequest {
    pub delivery_id: String,
    pub file_name: String,
    pub size_bytes: u64,
    /// The content's SHA-256, in lowercase hex. The Agent keeps the file
    /// only if it matches.
    pub sha256: String,
    pub destination: FileDeliveryDestination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDeliveryStatus {
    Pending,
    Delivered,
    Failed,
    /// The Agent never reported back.
    Lost,
}

impl FileDeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            "lost" => Some(Self::Lost),
            _ => None,
        }
    }

    pub fn finished(self) -> bool {
        self != Self::Pending
    }
}

/// How a delivery went, as the Agent reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDeliveryReport {
    /// Delivered or failed.
    pub status: FileDeliveryStatus,
    /// Where the file was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A script as the viewer lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxScript {
    pub id: String,
    pub name: String,
    /// `/`-separated folder names; empty at the top level.
    #[serde(default)]
    pub folder: String,
    pub language: ScriptLanguage,
    #[serde(default)]
    pub description: String,
    /// Shared with the whole company, rather than private to its owner.
    pub shared: bool,
}

/// A library file as the viewer lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxFile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub folder: String,
    pub size_bytes: u64,
    pub shared: bool,
}

/// The scripts and files a technician may use in a session: their own and
/// their company's shared ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxListing {
    pub scripts: Vec<ToolboxScript>,
    pub files: Vec<ToolboxFile>,
}

/// Starts a run of a toolbox script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartScriptRun {
    pub script_id: String,
    pub run_as: RunAs,
}

/// A run of a script on a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRun {
    pub id: String,
    pub device_id: String,
    pub script_id: String,
    pub script_name: String,
    pub language: ScriptLanguage,
    pub run_as: RunAs,
    pub status: ScriptRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub output_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_unix_ms: Option<u64>,
}

/// Sends a library file to the session's device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartFileDelivery {
    pub file_id: String,
    /// The technician is on the background desktop, so the file goes to
    /// Public Documents, which that desktop shows as Documents.
    #[serde(default)]
    pub background: bool,
}

/// A library file sent to a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDelivery {
    pub id: String,
    pub device_id: String,
    pub file_id: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub status: FileDeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_unix_ms: Option<u64>,
}

/// A script name: not blank, at most [`MAX_TOOLBOX_NAME_CHARS`] characters,
/// on one line.
pub fn valid_script_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= MAX_TOOLBOX_NAME_CHARS
        && !name.chars().any(char::is_control)
}

pub fn valid_script_description(description: &str) -> bool {
    description.len() <= MAX_SCRIPT_DESCRIPTION_BYTES
        && !description.chars().any(|c| c.is_control() && c != '\n')
}

/// A script's source: not blank, at most [`MAX_SCRIPT_BODY_BYTES`], and
/// without NUL, which neither interpreter accepts in a script file.
pub fn valid_script_body(body: &str) -> bool {
    !body.trim().is_empty() && body.len() <= MAX_SCRIPT_BODY_BYTES && !body.contains('\0')
}

pub fn valid_script_timeout(seconds: u32) -> bool {
    (MIN_SCRIPT_TIMEOUT_SECONDS..=MAX_SCRIPT_TIMEOUT_SECONDS).contains(&seconds)
}

/// A folder path in its stored form: names trimmed, empty names dropped, and
/// joined by `/`. `None` when a name or the path is too long or has a
/// control character, or the path is too deep. The top level is `""`.
pub fn normalize_folder(folder: &str) -> Option<String> {
    let names: Vec<&str> = folder
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    let valid = names.len() <= MAX_FOLDER_DEPTH
        && names.iter().all(|name| {
            name.chars().count() <= MAX_FOLDER_NAME_CHARS && !name.chars().any(char::is_control)
        });
    let folder = names.join("/");
    (valid && folder.len() <= MAX_FOLDER_BYTES).then_some(folder)
}

/// Names Windows reserves for devices, whatever the extension.
const RESERVED_FILE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A file name Windows can create in a folder: one path component, with no
/// characters Windows forbids, no trailing dot or space, and not a device
/// name.
pub fn valid_file_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    !name.is_empty()
        && name.chars().count() <= MAX_FILE_NAME_CHARS
        && !name.ends_with(['.', ' '])
        && !name.starts_with(' ')
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        && !RESERVED_FILE_NAMES
            .iter()
            .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// A SHA-256 digest in lowercase hex.
pub fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `text` cut to at most `max_bytes` on a character boundary, and whether
/// anything was cut.
pub fn truncate_output(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_their_stored_names() {
        for language in [ScriptLanguage::Powershell, ScriptLanguage::Cmd] {
            assert_eq!(ScriptLanguage::parse(language.as_str()), Some(language));
            assert_eq!(
                serde_json::to_string(&language).unwrap(),
                format!("\"{}\"", language.as_str())
            );
        }
        for run_as in [RunAs::System, RunAs::User] {
            assert_eq!(RunAs::parse(run_as.as_str()), Some(run_as));
            assert_eq!(
                serde_json::to_string(&run_as).unwrap(),
                format!("\"{}\"", run_as.as_str())
            );
        }
        for status in [
            ScriptRunStatus::Pending,
            ScriptRunStatus::Completed,
            ScriptRunStatus::Failed,
            ScriptRunStatus::TimedOut,
            ScriptRunStatus::Lost,
        ] {
            assert_eq!(ScriptRunStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.as_str())
            );
            assert_eq!(status.finished(), status != ScriptRunStatus::Pending);
        }
        for status in [
            FileDeliveryStatus::Pending,
            FileDeliveryStatus::Delivered,
            FileDeliveryStatus::Failed,
            FileDeliveryStatus::Lost,
        ] {
            assert_eq!(FileDeliveryStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.as_str())
            );
        }
        for destination in [
            FileDeliveryDestination::User,
            FileDeliveryDestination::Public,
        ] {
            assert_eq!(
                FileDeliveryDestination::parse(destination.as_str()),
                Some(destination)
            );
        }
        assert_eq!(ScriptLanguage::parse("bash"), None);
        assert_eq!(RunAs::parse("admin"), None);
    }

    #[test]
    fn folders_are_normalized_and_bounded() {
        assert_eq!(normalize_folder("").as_deref(), Some(""));
        assert_eq!(normalize_folder(" / ").as_deref(), Some(""));
        assert_eq!(
            normalize_folder(" Installers / Chrome ").as_deref(),
            Some("Installers/Chrome")
        );
        assert_eq!(
            normalize_folder(r"Maintenance\Disk//Cleanup/").as_deref(),
            Some("Maintenance/Disk/Cleanup")
        );
        assert_eq!(normalize_folder(&"a/".repeat(MAX_FOLDER_DEPTH + 1)), None);
        assert!(normalize_folder(&"a/".repeat(MAX_FOLDER_DEPTH)).is_some());
        assert_eq!(
            normalize_folder(&"x".repeat(MAX_FOLDER_NAME_CHARS + 1)),
            None
        );
        assert_eq!(normalize_folder("Tab\there"), None);
        let long = vec!["y".repeat(MAX_FOLDER_NAME_CHARS); 4].join("/");
        assert_eq!(
            normalize_folder(&long),
            None,
            "the whole path is bounded too"
        );
    }

    #[test]
    fn file_names_are_ones_windows_can_create() {
        for name in [
            "setup.exe",
            "Read me.txt",
            "archive.tar.gz",
            "Zoë 王.pdf",
            ".hidden",
        ] {
            assert!(valid_file_name(name), "{name}");
        }
        for name in [
            "",
            "a/b.txt",
            r"a\b.txt",
            "what?.txt",
            "trailing.",
            "trailing ",
            " leading",
            "con",
            "CON.txt",
            "lpt9.log",
            "nul .txt",
            "tab\t.txt",
            ".",
            "..",
        ] {
            assert!(!valid_file_name(name), "{name:?}");
        }
        assert!(valid_file_name(&"n".repeat(MAX_FILE_NAME_CHARS)));
        assert!(!valid_file_name(&"n".repeat(MAX_FILE_NAME_CHARS + 1)));
        assert!(
            valid_file_name("console.txt"),
            "only exact device names are reserved"
        );
    }

    #[test]
    fn scripts_are_bounded() {
        assert!(valid_script_name("Clear temp files"));
        assert!(!valid_script_name("  "));
        assert!(!valid_script_name("two\nlines"));
        assert!(!valid_script_name(&"n".repeat(MAX_TOOLBOX_NAME_CHARS + 1)));
        assert!(valid_script_body("Get-Process\r\n"));
        assert!(!valid_script_body(" \n "));
        assert!(!valid_script_body("echo\0"));
        assert!(!valid_script_body(&"x".repeat(MAX_SCRIPT_BODY_BYTES + 1)));
        assert!(valid_script_body(&"x".repeat(MAX_SCRIPT_BODY_BYTES)));
        assert!(valid_script_description("Line one\nline two"));
        assert!(!valid_script_description("tab\there"));
        assert!(valid_script_timeout(MIN_SCRIPT_TIMEOUT_SECONDS));
        assert!(valid_script_timeout(MAX_SCRIPT_TIMEOUT_SECONDS));
        assert!(!valid_script_timeout(MIN_SCRIPT_TIMEOUT_SECONDS - 1));
        assert!(!valid_script_timeout(MAX_SCRIPT_TIMEOUT_SECONDS + 1));
    }

    #[test]
    fn digests_are_lowercase_hex() {
        assert!(valid_sha256_hex(&"a1".repeat(32)));
        assert!(!valid_sha256_hex(&"A1".repeat(32)));
        assert!(!valid_sha256_hex(&"a1".repeat(31)));
        assert!(!valid_sha256_hex(&"g1".repeat(32)));
    }

    #[test]
    fn output_is_cut_on_a_character_boundary() {
        assert_eq!(truncate_output("short", 10), ("short", false));
        assert_eq!(truncate_output("exactly", 7), ("exactly", false));
        assert_eq!(truncate_output("abcdef", 3), ("abc", true));
        // "é" is two bytes; cutting inside it keeps only the "a".
        assert_eq!(truncate_output("aé", 2), ("a", true));
    }

    #[test]
    fn reports_decode_without_optional_fields() {
        let report: ScriptRunReport =
            serde_json::from_str(r#"{"status":"failed","ran_as":"SYSTEM","error":"no shell"}"#)
                .unwrap();
        assert_eq!(report.exit_code, None);
        assert!(report.stdout.is_empty() && !report.output_truncated);
        let delivery: FileDeliveryReport =
            serde_json::from_str(r#"{"status":"delivered","path":"C:\\x"}"#).unwrap();
        assert_eq!(delivery.status, FileDeliveryStatus::Delivered);
        assert_eq!(delivery.error, None);
    }
}
