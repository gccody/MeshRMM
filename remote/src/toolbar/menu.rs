use super::{Action, QUALITY_PRESETS, State, quality_label};
use meshrmm_protocol::{ChromaMode, QualityPreset, RunAs};

/// What a menu item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Session(usize),
    Display(usize),
    Quality(QualityPreset),
    Chroma(ChromaMode),
    PromptCredentials,
    AutofillCredentials,
    ForgetCredentials,
    SendFiles,
    ReceiveFiles,
    Restart {
        safe_mode: bool,
    },
    /// Runs the toolbox script at this place in the offered listing.
    RunScript {
        index: usize,
        run_as: RunAs,
    },
    /// Sends the toolbox file at this place in the offered listing.
    SendToolboxFile(usize),
    RefreshToolbox,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuEntry {
    Item {
        label: String,
        checked: bool,
        enabled: bool,
        /// `None` for status lines.
        command: Option<Command>,
    },
    Separator,
    /// A nested menu.
    Submenu {
        label: String,
        entries: Vec<MenuEntry>,
    },
}

fn entry(label: impl Into<String>, checked: bool, enabled: bool, command: Command) -> MenuEntry {
    MenuEntry::Item {
        label: label.into(),
        checked,
        enabled,
        command: Some(command),
    }
}

fn status(label: impl Into<String>) -> MenuEntry {
    MenuEntry::Item {
        label: label.into(),
        checked: false,
        enabled: false,
        command: None,
    }
}

/// The menu an item opens. The settings menu is the platform's own.
pub fn menu(action: Action, state: &State) -> Vec<MenuEntry> {
    match action {
        Action::User => state
            .sessions
            .iter()
            .enumerate()
            .map(|(index, label)| {
                entry(
                    label.clone(),
                    index == state.session,
                    true,
                    Command::Session(index),
                )
            })
            .collect(),
        Action::Display => state
            .displays
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let label = if state.pointer_display == Some(index) {
                    format!("➤ {label}")
                } else {
                    label.clone()
                };
                entry(label, index == state.display, true, Command::Display(index))
            })
            .collect(),
        Action::Quality => {
            let mut entries: Vec<MenuEntry> = QUALITY_PRESETS
                .into_iter()
                .map(|preset| {
                    entry(
                        quality_label(preset),
                        preset == state.quality,
                        true,
                        Command::Quality(preset),
                    )
                })
                .collect();
            if let Some((chroma, crisp_available)) = state.chroma {
                entries.push(MenuEntry::Separator);
                entries.push(entry(
                    "4:2:0 efficient color",
                    chroma == ChromaMode::Yuv420,
                    true,
                    Command::Chroma(ChromaMode::Yuv420),
                ));
                entries.push(entry(
                    "4:4:4 crisp color",
                    chroma == ChromaMode::Yuv444,
                    crisp_available,
                    Command::Chroma(ChromaMode::Yuv444),
                ));
            }
            entries
        }
        Action::Credentials => {
            let credentials = &state.credentials;
            let allowed = !state.input_blocked;
            // Status and errors stay in the key icon's tooltip.
            vec![
                entry(
                    "Autofill saved credentials",
                    false,
                    allowed && credentials.can_autofill,
                    Command::AutofillCredentials,
                ),
                entry(
                    "Prompt for credentials",
                    false,
                    allowed && credentials.available && !credentials.prompt_active,
                    Command::PromptCredentials,
                ),
                entry(
                    "Forget saved credentials",
                    false,
                    allowed && credentials.saved && !credentials.prompt_active,
                    Command::ForgetCredentials,
                ),
            ]
        }
        Action::Power => {
            let Some(safe_mode) = state.power else {
                return Vec::new();
            };
            let mut entries = Vec::new();
            if safe_mode {
                entries.push(status("Windows is in Safe Mode"));
                entries.push(MenuEntry::Separator);
            }
            entries.push(entry(
                if safe_mode {
                    "Restart normally…"
                } else {
                    "Restart…"
                },
                false,
                true,
                Command::Restart { safe_mode: false },
            ));
            // Apple silicon Macs start in Safe Mode only from the power button.
            if !state.device_is_mac {
                entries.push(entry(
                    "Restart in Safe Mode with Networking…",
                    false,
                    true,
                    Command::Restart { safe_mode: true },
                ));
            }
            entries
        }
        Action::Files => {
            let mut entries = vec![
                entry("Send files…", false, true, Command::SendFiles),
                entry("Receive files…", false, true, Command::ReceiveFiles),
            ];
            if !state.file_status.is_empty() {
                entries.push(MenuEntry::Separator);
                entries.push(status(state.file_status.clone()));
            }
            entries
        }
        _ => Vec::new(),
    }
}

/// What the viewer asks before restarting the remote computer: the title,
/// the explanation, and the confirming button's label.
pub fn restart_confirmation(safe_mode: bool) -> (&'static str, &'static str, &'static str) {
    if safe_mode {
        (
            "Restart the remote computer in Safe Mode?",
            "Windows restarts now in Safe Mode with Networking, closing applications without \
             saving. The session reconnects once the agent is back online. The restart after \
             that returns Windows to normal mode.",
            "Restart in Safe Mode",
        )
    } else {
        (
            "Restart the remote computer?",
            "The remote computer restarts now, closing applications without saving. The \
             session reconnects once the agent is back online.",
            "Restart",
        )
    }
}

/// The commands of `entries`, in order, for platforms that number menu
/// items. A submenu takes a place itself, followed by its entries. Status
/// lines, separators and submenus have no command.
pub fn commands(entries: &[MenuEntry]) -> Vec<Option<Command>> {
    fn collect(entries: &[MenuEntry], commands: &mut Vec<Option<Command>>) {
        for entry in entries {
            match entry {
                MenuEntry::Item { command, .. } => commands.push(*command),
                MenuEntry::Separator => commands.push(None),
                MenuEntry::Submenu { entries, .. } => {
                    commands.push(None);
                    collect(entries, commands);
                }
            }
        }
    }
    let mut commands = Vec::new();
    collect(entries, &mut commands);
    commands
}

/// Menu entries in nested folders: the folders first, as submenus, then
/// the entries at that level.
#[derive(Default)]
struct FolderTree {
    folders: Vec<(String, FolderTree)>,
    entries: Vec<MenuEntry>,
}

impl FolderTree {
    fn insert(&mut self, folder: &str, entry: MenuEntry) {
        let mut node = self;
        for name in folder.split('/').filter(|name| !name.is_empty()) {
            let position = match node.folders.iter().position(|(folder, _)| folder == name) {
                Some(position) => position,
                None => {
                    node.folders.push((name.to_owned(), FolderTree::default()));
                    node.folders.len() - 1
                }
            };
            node = &mut node.folders[position].1;
        }
        node.entries.push(entry);
    }

    fn into_entries(self) -> Vec<MenuEntry> {
        let mut entries: Vec<MenuEntry> = self
            .folders
            .into_iter()
            .map(|(label, tree)| MenuEntry::Submenu {
                label,
                entries: tree.into_entries(),
            })
            .collect();
        entries.extend(self.entries);
        entries
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The toolbox's menu: each script, in its folders, with a choice of
/// account, then each library file, then a refresh and the latest status.
pub fn toolbox_menu(snapshot: &crate::toolbox::Snapshot) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    match &snapshot.listing {
        None if snapshot.loading || snapshot.error.is_none() => {
            entries.push(status("Loading the toolbox…"));
        }
        None => entries.push(status(format!(
            "The toolbox could not be loaded: {}",
            snapshot.error.as_deref().unwrap_or_default()
        ))),
        Some(listing) => {
            entries.push(status("Run a script"));
            if listing.scripts.is_empty() {
                entries.push(status(
                    "No scripts yet. Add them in the dashboard's Toolbox.",
                ));
            }
            let mut scripts = FolderTree::default();
            for (index, script) in listing.scripts.iter().enumerate() {
                scripts.insert(
                    &script.folder,
                    MenuEntry::Submenu {
                        label: script.name.clone(),
                        entries: vec![
                            entry(
                                "As the signed-in user",
                                false,
                                true,
                                Command::RunScript {
                                    index,
                                    run_as: RunAs::User,
                                },
                            ),
                            entry(
                                "As SYSTEM",
                                false,
                                true,
                                Command::RunScript {
                                    index,
                                    run_as: RunAs::System,
                                },
                            ),
                        ],
                    },
                );
            }
            entries.extend(scripts.into_entries());
            entries.push(MenuEntry::Separator);
            entries.push(status("Send a file to Documents"));
            if listing.files.is_empty() {
                entries.push(status(
                    "No files yet. Upload them in the dashboard's Toolbox.",
                ));
            }
            let mut files = FolderTree::default();
            for (index, file) in listing.files.iter().enumerate() {
                files.insert(
                    &file.folder,
                    entry(
                        format!("{} ({})", file.name, format_size(file.size_bytes)),
                        false,
                        true,
                        Command::SendToolboxFile(index),
                    ),
                );
            }
            entries.extend(files.into_entries());
        }
    }
    entries.push(MenuEntry::Separator);
    entries.push(entry(
        "Refresh the toolbox",
        false,
        snapshot.available && !snapshot.loading,
        Command::RefreshToolbox,
    ));
    if !snapshot.status.is_empty() {
        entries.push(status(snapshot.status.clone()));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(95 * 1024 * 1024), "95 MiB");
    }
}
