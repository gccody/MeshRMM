//! Remote terminals: an interactive shell on a device, which a signed-in
//! user reaches from the website.
//!
//! The server asks the Agent for one over the control connection
//! ([`crate::AgentCommand::OpenTerminal`]). The Agent starts the shell on a
//! pseudo-terminal and connects a WebSocket of its own for it,
//! `/v1/agents/{id}/terminals/{terminal_id}`, and the server relays between
//! that socket and the browser's, `/v1/terminals/{terminal_id}`. Terminal
//! traffic never shares the control connection.
//!
//! On both sockets a binary message carries the terminal's bytes: input
//! towards the Agent, output from it, in no particular encoding (shells write
//! UTF-8, and the browser decodes it across message boundaries). Text
//! messages are JSON: [`TerminalControl`] towards the Agent and
//! [`TerminalEvent`] from it.
//!
//! Output is flow controlled end to end: the Agent stops reading the shell's
//! output while more than [`TERMINAL_WINDOW_BYTES`] it sent are
//! unacknowledged, and the browser acknowledges output once its terminal has
//! drawn it. A flood therefore waits in the device's pseudo-terminal, not in
//! the server's or the browser's memory.
use serde::{Deserialize, Serialize};

use crate::{DevicePlatform, RunAs};

/// Output the Agent may have sent that the browser has not acknowledged.
pub const TERMINAL_WINDOW_BYTES: u64 = 512 * 1024;
/// The largest message either side sends on a terminal socket. The Agent
/// sends output in pieces of at most [`MAX_TERMINAL_OUTPUT_BYTES`]; the
/// browser splits a long paste.
pub const MAX_TERMINAL_MESSAGE_BYTES: usize = 64 * 1024;
/// The most output the Agent sends in one message.
pub const MAX_TERMINAL_OUTPUT_BYTES: usize = 16 * 1024;
pub const MAX_TERMINAL_COLUMNS: u16 = 1000;
/// How long a terminal may go without input before the server closes it,
/// unless the company chose another of [`crate::IDLE_DISCONNECT_MINUTES`].
pub const DEFAULT_TERMINAL_IDLE_MINUTES: u32 = 30;
pub const MAX_TERMINAL_ROWS: u16 = 500;

/// The shell a terminal runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalShell {
    /// The account's login shell, from its user database entry.
    Login,
    Bash,
    Zsh,
    Sh,
    /// Windows PowerShell 5.1.
    Powershell,
    /// PowerShell 7, where it is installed.
    Pwsh,
    /// Command Prompt.
    Cmd,
}

impl TerminalShell {
    pub const ALL: [Self; 7] = [
        Self::Login,
        Self::Bash,
        Self::Zsh,
        Self::Sh,
        Self::Powershell,
        Self::Pwsh,
        Self::Cmd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::Powershell => "powershell",
            Self::Pwsh => "pwsh",
            Self::Cmd => "cmd",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shell| shell.as_str() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Login => "Login shell",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::Powershell => "Windows PowerShell",
            Self::Pwsh => "PowerShell 7",
            Self::Cmd => "Command Prompt",
        }
    }

    /// Whether an Agent on `platform` can run the shell, if it is
    /// installed: PowerShell and Command Prompt on Windows, the Unix shells
    /// on macOS and Linux.
    pub fn runs_on(self, platform: DevicePlatform) -> bool {
        match self {
            Self::Powershell | Self::Pwsh | Self::Cmd => platform == DevicePlatform::Windows,
            Self::Login | Self::Bash | Self::Zsh | Self::Sh => platform != DevicePlatform::Windows,
        }
    }

    /// What a device on `platform` offers before it has reported its
    /// shells: the ones every such computer has.
    pub fn defaults(platform: DevicePlatform) -> &'static [Self] {
        match platform {
            DevicePlatform::Windows => &[Self::Powershell, Self::Cmd],
            DevicePlatform::Macos | DevicePlatform::Linux => &[Self::Login, Self::Sh],
        }
    }
}

/// Whether `minutes` is an idle time a company may choose for terminals: the
/// same choices as a remote session's idle disconnect.
pub fn valid_terminal_idle_minutes(minutes: u32) -> bool {
    crate::valid_idle_disconnect_minutes(minutes)
}

/// Whether a terminal of `columns` by `rows` is one the Agent will size.
pub fn valid_terminal_size(columns: u16, rows: u16) -> bool {
    (1..=MAX_TERMINAL_COLUMNS).contains(&columns) && (1..=MAX_TERMINAL_ROWS).contains(&rows)
}

/// What the server asks the Agent to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRequest {
    pub terminal_id: String,
    pub shell: TerminalShell,
    /// The signed-in user's account, or the Agent's own (SYSTEM or root).
    /// Unlike a script, a terminal for the signed-in user is refused when
    /// nobody is signed in.
    pub run_as: RunAs,
    pub columns: u16,
    pub rows: u16,
    /// The company's connection notification, rendered, for the computer's
    /// user to see; `None` when company policy shows none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<String>,
}

/// What the browser sends the Agent, through the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalControl {
    /// The browser's terminal changed size.
    Resize { columns: u16, rows: u16 },
    /// The browser has drawn this many bytes of output, counted from the
    /// start of the terminal.
    Ack { bytes: u64 },
}

/// What the Agent tells the browser, through the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalEvent {
    /// The shell is running as `account`, such as `NT AUTHORITY\SYSTEM` or
    /// `alice`. `shell` is the program, such as `/bin/zsh`.
    Started { account: String, shell: String },
    /// The shell could not start; the socket closes after.
    Failed { message: String },
    /// The shell exited with `code` (`None` when a signal ended it); the
    /// socket closes after.
    Exited {
        #[serde(default)]
        code: Option<i32>,
    },
}

/// The longest account, shell or message text a [`TerminalEvent`] carries.
pub const MAX_TERMINAL_EVENT_TEXT_CHARS: usize = 512;

impl TerminalEvent {
    /// The event with its text cut to [`MAX_TERMINAL_EVENT_TEXT_CHARS`] and
    /// without control characters.
    pub fn sanitized(self) -> Self {
        let clean = |text: String| -> String {
            text.chars()
                .filter(|character| !character.is_control())
                .take(MAX_TERMINAL_EVENT_TEXT_CHARS)
                .collect()
        };
        match self {
            Self::Started { account, shell } => Self::Started {
                account: clean(account),
                shell: clean(shell),
            },
            Self::Failed { message } => Self::Failed {
                message: clean(message),
            },
            Self::Exited { code } => Self::Exited { code },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_round_trip_and_belong_to_their_platforms() {
        for shell in TerminalShell::ALL {
            assert_eq!(TerminalShell::parse(shell.as_str()), Some(shell));
            assert_eq!(
                serde_json::to_value(shell).unwrap(),
                serde_json::Value::String(shell.as_str().into())
            );
        }
        assert_eq!(TerminalShell::parse("fish"), None);
        assert!(TerminalShell::Cmd.runs_on(DevicePlatform::Windows));
        assert!(!TerminalShell::Cmd.runs_on(DevicePlatform::Linux));
        assert!(TerminalShell::Zsh.runs_on(DevicePlatform::Macos));
        assert!(!TerminalShell::Login.runs_on(DevicePlatform::Windows));
        for platform in [
            DevicePlatform::Windows,
            DevicePlatform::Macos,
            DevicePlatform::Linux,
        ] {
            assert!(
                TerminalShell::defaults(platform)
                    .iter()
                    .all(|shell| shell.runs_on(platform))
            );
        }
    }

    #[test]
    fn sizes_are_bounded() {
        assert!(valid_terminal_size(80, 24));
        assert!(valid_terminal_size(1, 1));
        assert!(valid_terminal_size(MAX_TERMINAL_COLUMNS, MAX_TERMINAL_ROWS));
        assert!(!valid_terminal_size(0, 24));
        assert!(!valid_terminal_size(80, 0));
        assert!(!valid_terminal_size(MAX_TERMINAL_COLUMNS + 1, 24));
        assert!(!valid_terminal_size(80, MAX_TERMINAL_ROWS + 1));
    }

    #[test]
    fn messages_have_their_wire_form() {
        assert_eq!(
            serde_json::from_str::<TerminalControl>(r#"{"type":"resize","columns":120,"rows":40}"#)
                .unwrap(),
            TerminalControl::Resize {
                columns: 120,
                rows: 40
            }
        );
        assert_eq!(
            serde_json::from_str::<TerminalControl>(r#"{"type":"ack","bytes":65536}"#).unwrap(),
            TerminalControl::Ack { bytes: 65536 }
        );
        assert!(serde_json::from_str::<TerminalControl>(r#"{"type":"input"}"#).is_err());
        assert_eq!(
            serde_json::to_string(&TerminalEvent::Exited { code: Some(2) }).unwrap(),
            r#"{"type":"exited","code":2}"#
        );
        let request = TerminalRequest {
            terminal_id: "one".into(),
            shell: TerminalShell::Pwsh,
            run_as: RunAs::User,
            columns: 80,
            rows: 24,
            notification: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["shell"], "pwsh");
        assert_eq!(json["run_as"], "user");
        assert!(json.get("notification").is_none());
        assert_eq!(
            serde_json::from_value::<TerminalRequest>(json).unwrap(),
            request
        );
    }

    #[test]
    fn event_text_is_cleaned_and_bounded() {
        let event = TerminalEvent::Started {
            account: "bad\u{1b}[31mname".into(),
            shell: "x".repeat(2000),
        }
        .sanitized();
        let TerminalEvent::Started { account, shell } = event else {
            unreachable!()
        };
        assert_eq!(account, "bad[31mname");
        assert_eq!(shell.chars().count(), MAX_TERMINAL_EVENT_TEXT_CHARS);
    }
}
