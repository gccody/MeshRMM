//! Reading what package managers print: installed packages and the updates
//! they offer. Pure functions, compiled on every platform so their tests run
//! everywhere against captured output in `fixtures/`.
use meshrmm_protocol::{PackageSource, ScannedPackage};

/// The width a terminal gives `character`: two for East Asian wide
/// characters and emoji, which winget pads by, one otherwise.
#[cfg_attr(not(windows), allow(dead_code))]
fn display_width(character: char) -> usize {
    let code = u32::from(character);
    let wide = matches!(code,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// A line as a terminal would end up showing it: the last thing written
/// after a carriage return, without control characters.
#[cfg_attr(not(windows), allow(dead_code))]
fn settled(line: &str) -> String {
    let last = line
        .split('\r')
        .rfind(|segment| !segment.trim().is_empty())
        .unwrap_or_default();
    last.chars().filter(|c| !c.is_control()).collect()
}

/// Whether display column `column` of `line` holds something other than a
/// space.
#[cfg_attr(not(windows), allow(dead_code))]
fn occupied(line: &str, column: usize) -> bool {
    let mut at = 0;
    for character in line.chars() {
        let width = display_width(character);
        if column < at + width {
            return character != ' ';
        }
        at += width;
    }
    false
}

/// The cells of winget's tables, as `winget list` and `winget upgrade`
/// print them: a header line, a line of dashes, then rows whose cells start
/// at the columns where the header's labels do, each column one space
/// after the last. The header is in the device's language, so cells are
/// found by position, not by name. Output can hold several tables, such as
/// upgrades that need explicit targeting, and lines between them.
///
/// A label can have a space in it (Korean's 사용 가능, "available"), which
/// reads as two columns. A line that runs into the second column (the ID)
/// isn't a row but a note such as "3 upgrades available."; a later column
/// start that a row's text runs across isn't a column. A label split that
/// no row crosses stays split; [`winget_list`] reads around it.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn winget_tables(output: &str) -> Vec<Vec<String>> {
    let lines = output.lines().map(settled).collect::<Vec<_>>();
    let dashes = |line: &str| {
        let line = line.trim();
        line.len() >= 10 && line.chars().all(|c| c == '-')
    };
    let mut rows = Vec::new();
    let mut index = 0;
    while index + 1 < lines.len() {
        if !dashes(&lines[index + 1]) || lines[index].trim().is_empty() {
            index += 1;
            continue;
        }
        // Where each column starts, in display columns.
        let header = &lines[index];
        let mut starts = Vec::new();
        let mut column = 0;
        let mut previous_space = true;
        for character in header.chars() {
            let space = character == ' ';
            if !space && previous_space {
                starts.push(column);
            }
            previous_space = space;
            column += display_width(character);
        }
        index += 2;
        let mut body = Vec::new();
        while index < lines.len() {
            let line = &lines[index];
            if line.trim().is_empty() || lines.get(index + 1).is_some_and(|next| dashes(next)) {
                break;
            }
            body.push(line.as_str());
            index += 1;
        }
        let crosses = |line: &str, start: usize| start > 0 && occupied(line, start - 1);
        body.retain(|line| starts.get(1).is_none_or(|id| !crosses(line, *id)));
        let starts = starts
            .iter()
            .enumerate()
            .filter(|(column, start)| {
                *column < 2 || !body.iter().any(|line| crosses(line, **start))
            })
            .map(|(_, start)| *start)
            .collect::<Vec<_>>();
        for line in body {
            let mut cells = vec![String::new(); starts.len()];
            let mut column = 0;
            for character in line.chars() {
                let cell = starts
                    .iter()
                    .rposition(|start| column >= *start)
                    .unwrap_or(0);
                cells[cell].push(character);
                column += display_width(character);
            }
            rows.push(
                cells
                    .into_iter()
                    .map(|cell| cell.trim().to_owned())
                    .collect(),
            );
        }
    }
    rows
}

/// A winget version cell: `Unknown`, in any language, is none.
#[cfg_attr(not(windows), allow(dead_code))]
fn winget_version(cell: &str) -> Option<String> {
    let cell = cell.trim();
    (!cell.is_empty() && cell.chars().any(|c| c.is_ascii_digit())).then(|| cell.to_owned())
}

/// Packages from `winget list`: name, ID, version, available version and
/// source. Programs winget can't match to a package of one of its sources
/// (an empty source, IDs such as `ARP\Machine\X64\…` or `MSIX\…`) are left
/// out, as are IDs winget cut short with `…`.
///
/// A package whose installed version winget doesn't know isn't outdated,
/// as plain `winget upgrade` sees it: updating it must be asked for by
/// name.
///
/// Output with rows that have a source but no package read from it is an
/// error, so a table this can't read never passes for an empty list.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn winget_list(output: &str) -> Result<Vec<ScannedPackage>, String> {
    let rows = winget_tables(output);
    let mut packages = Vec::new();
    let mut readable = 0;
    let mut sourced = 0;
    for cells in &rows {
        // Name, Id, Version, then Available and Source; with nothing
        // available winget leaves the Available column out, and a label
        // with a space in it can leave an extra one in.
        let [name, id, version, .., source] = cells.as_slice() else {
            continue;
        };
        readable += 1;
        if source.is_empty() {
            continue;
        }
        sourced += 1;
        if id.ends_with('…') || !PackageSource::Winget.valid_id(id) {
            continue;
        }
        let available = cells[3..cells.len() - 1]
            .iter()
            .filter(|cell| !cell.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let version = winget_version(version);
        let available_version = if version.is_some() {
            winget_version(&available)
        } else {
            None
        };
        packages.push(ScannedPackage {
            source: PackageSource::Winget,
            id: id.clone(),
            name: name.trim_end_matches('…').trim().to_owned(),
            version,
            available_version,
        });
    }
    if packages.is_empty() && (sourced > 0 || (readable == 0 && !output.trim().is_empty())) {
        return Err("winget's list couldn't be read".to_owned());
    }
    Ok(packages)
}

/// The ID from `winget show`'s first line, `Found Mozilla Firefox
/// [Mozilla.Firefox]` in the device's language: the last bracketed word.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn winget_shown_id(output: &str) -> Option<String> {
    output.lines().map(settled).find_map(|line| {
        let line = line.trim();
        let inner = line.strip_suffix(']')?;
        let id = &inner[inner.rfind('[')? + 1..];
        PackageSource::Winget.valid_id(id).then(|| id.to_owned())
    })
}

/// Whether a file starting with `head` is a program (ELF), rather than a
/// script.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn is_elf(head: &[u8]) -> bool {
    head.starts_with(b"\x7fELF")
}

/// Whether `locale -a` lists the C locale in UTF-8, which glibc spells
/// `C.utf8` and others `C.UTF-8`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn has_c_utf8(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim().to_ascii_lowercase().replace('-', "") == "c.utf8")
}

/// `winget --version`: `v1.29.380`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn winget_version_text(output: &str) -> Option<String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(line.trim_start_matches('v').to_owned())
}

/// What winget's exit codes mean, for the ones an install or uninstall
/// commonly ends with.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn winget_exit_code(code: u32) -> Option<&'static str> {
    Some(match code {
        0x8A15_0010 => "No installer matches this device (architecture, scope or locale)",
        0x8A15_0011 => "The installer's hash doesn't match its manifest",
        0x8A15_0014 => "No installed package matches",
        0x8A15_0016 => "More than one package matches the ID",
        0x8A15_0017 => "No package has this ID in winget's sources",
        0x8A15_002B => "No newer version applies to this device",
        0x8A15_0061 => "The package is installed already",
        0x8A15_0101 => "The application is in use; close it and try again",
        0x8A15_0102 => "Another installation is in progress",
        0x8A15_0103 => "A file the installer needs is in use",
        0x8A15_0104 => "A dependency is missing",
        0x8A15_0105 => "The disk is full",
        0x8A15_0106 => "There isn't enough memory",
        0x8A15_0107 => "The installer needs a network connection",
        0x8A15_0109 => "The computer must restart to finish the install",
        0x8A15_010A => "The computer must restart before the install can run",
        0x8A15_010B => "The installer restarted the computer",
        0x8A15_010D => "Another version is installed already",
        0x8A15_010E => "A newer version is installed already",
        0x8A15_010F => "Group policy blocks the install",
        _ => return None,
    })
}

/// `name|version` lines, as `choco list -r` prints them.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn chocolatey_list(output: &str) -> Vec<ScannedPackage> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim().split('|');
            let (id, version) = (fields.next()?.trim(), fields.next()?.trim());
            PackageSource::Chocolatey
                .valid_id(id)
                .then(|| ScannedPackage {
                    source: PackageSource::Chocolatey,
                    id: id.to_owned(),
                    name: id.to_owned(),
                    version: (!version.is_empty()).then(|| version.to_owned()),
                    available_version: None,
                })
        })
        .collect()
}

/// `name|installed|available|pinned` lines, as `choco outdated -r` prints
/// them.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn chocolatey_outdated(output: &str) -> Vec<ScannedPackage> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.trim().split('|').map(str::trim).collect::<Vec<_>>();
            let [id, installed, available, ..] = fields[..] else {
                return None;
            };
            (PackageSource::Chocolatey.valid_id(id) && !available.is_empty()).then(|| {
                ScannedPackage {
                    source: PackageSource::Chocolatey,
                    id: id.to_owned(),
                    name: id.to_owned(),
                    version: (!installed.is_empty()).then(|| installed.to_owned()),
                    available_version: Some(available.to_owned()),
                }
            })
        })
        .collect()
}

/// `name version…` lines, as `brew list --versions` prints them; a formula
/// with several versions installed lists each, the newest last.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn homebrew_list(output: &str, source: PackageSource) -> Vec<ScannedPackage> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?;
            let version = fields.next_back();
            source.valid_id(id).then(|| ScannedPackage {
                source,
                id: id.to_owned(),
                name: id.to_owned(),
                version: version.map(str::to_owned),
                available_version: None,
            })
        })
        .collect()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(serde::Deserialize)]
struct HomebrewOutdated {
    #[serde(default)]
    formulae: Vec<HomebrewEntry>,
    #[serde(default)]
    casks: Vec<HomebrewEntry>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(serde::Deserialize)]
struct HomebrewEntry {
    name: String,
    /// A list for formulae; casks of some Homebrew versions give a string.
    #[serde(default)]
    installed_versions: serde_json::Value,
    #[serde(default)]
    current_version: Option<String>,
}

/// Outdated formulae and casks from `brew outdated --json=v2`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn homebrew_outdated(output: &str) -> anyhow::Result<Vec<ScannedPackage>> {
    let outdated: HomebrewOutdated = serde_json::from_str(output.trim())
        .map_err(|error| anyhow::anyhow!("brew outdated printed something unexpected: {error}"))?;
    let entries = outdated
        .formulae
        .into_iter()
        .map(|entry| (PackageSource::Homebrew, entry))
        .chain(
            outdated
                .casks
                .into_iter()
                .map(|entry| (PackageSource::HomebrewCask, entry)),
        );
    Ok(entries
        .filter_map(|(source, entry)| {
            let installed = match &entry.installed_versions {
                serde_json::Value::Array(versions) => versions
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .next_back()
                    .map(str::to_owned),
                serde_json::Value::String(version) => Some(version.clone()),
                _ => None,
            };
            source.valid_id(&entry.name).then(|| ScannedPackage {
                source,
                name: entry.name.clone(),
                id: entry.name,
                version: installed,
                available_version: entry.current_version.filter(|version| !version.is_empty()),
            })
        })
        .collect())
}

/// `flatpak list --system --app --columns=application,name,version`, which
/// separates columns with tabs and prints a header only to a terminal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn flatpak_list(output: &str) -> Vec<ScannedPackage> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split('\t').map(str::trim).collect::<Vec<_>>();
            let [id, name, version] = fields[..] else {
                return None;
            };
            PackageSource::Flatpak.valid_id(id).then(|| ScannedPackage {
                source: PackageSource::Flatpak,
                id: id.to_owned(),
                name: if name.is_empty() { id } else { name }.to_owned(),
                version: (!version.is_empty()).then(|| version.to_owned()),
                available_version: None,
            })
        })
        .collect()
}

/// `flatpak remote-ls --system --updates --app
/// --columns=application,version`: the applications with updates, and the
/// version the update brings, which an update that changes only the build
/// leaves empty.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn flatpak_updates(output: &str) -> Vec<(String, Option<String>)> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t').map(str::trim);
            let id = fields.next()?;
            let version = fields.next().unwrap_or_default();
            PackageSource::Flatpak.valid_id(id).then(|| {
                (
                    id.to_owned(),
                    (!version.is_empty()).then(|| version.to_owned()),
                )
            })
        })
        .collect()
}

/// `flatpak remotes --system --columns=name`: one remote per line. A name
/// that could be read as an option is left out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn flatpak_remotes(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.starts_with('-'))
        .map(str::to_owned)
        .collect()
}

/// The snap types that are the system rather than applications, as
/// `snap list`'s notes name them (the `os` type reads as `core`).
const SYSTEM_SNAPS: [&str; 5] = ["base", "core", "snapd", "gadget", "kernel"];

/// `snap list --unicode=never`: name, version, revision, tracking,
/// publisher and notes under a header. Disabled revisions are left out, and
/// so are snaps the notes say are the system: bases, the core, snapd, gadgets
/// and kernels.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn snap_list(output: &str) -> Vec<ScannedPackage> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let [id, version, ..] = fields[..] else {
                return None;
            };
            let notes = fields.get(5).copied().unwrap_or_default();
            if notes
                .split(',')
                .any(|note| note == "disabled" || SYSTEM_SNAPS.contains(&note))
            {
                return None;
            }
            PackageSource::Snap.valid_id(id).then(|| ScannedPackage {
                source: PackageSource::Snap,
                id: id.to_owned(),
                name: id.to_owned(),
                version: Some(version.to_owned()),
                available_version: None,
            })
        })
        .collect()
}

/// The snaps that are applications, from snapd's `GET /v2/snaps`: of type
/// `app`, with at least one app (a command or service). Snaps without one
/// are runtimes and themes that other snaps use, such as `gnome-46-2404`
/// or `gtk-common-themes`, which snapd keeps up to date with them. `None`
/// when the answer isn't snapd's list.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn snapd_applications(body: &str) -> Option<std::collections::HashSet<String>> {
    #[derive(serde::Deserialize)]
    struct Answer {
        result: Vec<Snap>,
    }
    #[derive(serde::Deserialize)]
    struct Snap {
        name: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        apps: Vec<serde::de::IgnoredAny>,
    }
    let answer = serde_json::from_str::<Answer>(body).ok()?;
    Some(
        answer
            .result
            .into_iter()
            .filter(|snap| snap.kind == "app" && !snap.apps.is_empty())
            .map(|snap| snap.name)
            .collect(),
    )
}

/// `snap refresh --list --unicode=never`: the snaps with updates and the
/// version each brings, under a header of name, version, revision, size,
/// publisher and notes. With none it prints "All snaps up to date." (to
/// standard error), which has no rows.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn snap_updates(output: &str) -> Vec<(String, String)> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    if header.split_whitespace().count() < 4 {
        return Vec::new();
    }
    lines
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let [id, version, ..] = fields[..] else {
                return None;
            };
            PackageSource::Snap
                .valid_id(id)
                .then(|| (id.to_owned(), version.to_owned()))
        })
        .collect()
}

/// The installed packages with the updates offered for them: each update
/// sets its package's available version, and an update for a package the
/// installed list missed is added.
pub(crate) fn merge(
    mut installed: Vec<ScannedPackage>,
    outdated: Vec<ScannedPackage>,
) -> Vec<ScannedPackage> {
    for update in outdated {
        let key = update.key();
        match installed
            .iter_mut()
            .find(|package| key.is(package.source, &package.id))
        {
            Some(package) => {
                package.available_version = update.available_version;
                if package.version.is_none() {
                    package.version = update.version;
                }
            }
            None => installed.push(update),
        }
    }
    installed
}

/// Splits a command line into arguments as a POSIX shell would, for an
/// installer's arguments on macOS and Linux: by spaces, with `'…'` and
/// `"…"` quoting and `\` escaping. Nothing is expanded.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn split_arguments(text: &str) -> anyhow::Result<Vec<String>> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                started = true;
                loop {
                    match characters.next() {
                        Some('\'') => break,
                        Some(character) => current.push(character),
                        None => anyhow::bail!("the installer arguments have an unclosed '"),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match characters.next() {
                        Some('"') => break,
                        Some('\\') => match characters.next() {
                            Some(escaped @ ('"' | '\\' | '$' | '`')) => current.push(escaped),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => anyhow::bail!("the installer arguments end with \\"),
                        },
                        Some(character) => current.push(character),
                        None => anyhow::bail!("the installer arguments have an unclosed \""),
                    }
                }
            }
            '\\' => {
                started = true;
                match characters.next() {
                    Some(escaped) => current.push(escaped),
                    None => anyhow::bail!("the installer arguments end with \\"),
                }
            }
            character if character.is_whitespace() => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                started = true;
                current.push(character);
            }
        }
    }
    if started {
        arguments.push(current);
    }
    Ok(arguments)
}

/// A Windows `UninstallString` that runs Windows Installer, made quiet:
/// `MsiExec.exe /I{GUID}` or `/X{GUID}` becomes the product code to remove.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn msi_product_code(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("msiexec") {
        return None;
    }
    let open = text.find('{')?;
    let close = open + text[open..].find('}')?;
    let code = &text[open..=close];
    is_product_code(code).then(|| code.to_owned())
}

/// Whether `text` is a Windows Installer product code: a GUID in braces.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_product_code(text: &str) -> bool {
    let Some(inner) = text
        .strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
    else {
        return false;
    };
    let groups = inner.split('-').collect::<Vec<_>>();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, length)| {
            group.len() == length && group.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINGET_LIST: &str = include_str!("fixtures/winget-list.txt");
    const WINGET_UPGRADE: &str = include_str!("fixtures/winget-upgrade.txt");
    const WINGET_LIST_ONE: &str = include_str!("fixtures/winget-list-git.txt");
    const CHOCOLATEY_LIST: &str = include_str!("fixtures/choco-list.txt");
    const CHOCOLATEY_OUTDATED: &str = include_str!("fixtures/choco-outdated.txt");
    const HOMEBREW_OUTDATED: &str = include_str!("fixtures/brew-outdated.json");
    const HOMEBREW_LIST: &str = include_str!("fixtures/brew-list-versions.txt");
    const HOMEBREW_CASKS: &str = include_str!("fixtures/brew-list-cask-versions.txt");
    const FLATPAK_LIST: &str = include_str!("fixtures/flatpak-list.txt");
    const FLATPAK_UPDATES: &str = include_str!("fixtures/flatpak-remote-ls-updates.txt");
    const SNAP_LIST: &str = include_str!("fixtures/snap-list.txt");
    const SNAP_REFRESH: &str = include_str!("fixtures/snap-refresh-list.txt");
    const SNAPD_SNAPS: &str = include_str!("fixtures/snapd-snaps.json");

    /// `winget upgrade` prints the same columns as `winget list`, for the
    /// packages with updates.
    fn winget_upgrades(output: &str) -> Vec<ScannedPackage> {
        winget_list(output)
            .unwrap()
            .into_iter()
            .filter(|package| package.available_version.is_some())
            .collect()
    }

    fn find<'a>(packages: &'a [ScannedPackage], id: &str) -> &'a ScannedPackage {
        packages
            .iter()
            .find(|package| package.id == id)
            .unwrap_or_else(|| panic!("{id} in {packages:#?}"))
    }

    #[test]
    fn winget_lists_packages_from_its_sources_only() {
        let packages = winget_list(WINGET_LIST).unwrap();
        let ids = packages
            .iter()
            .map(|package| package.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "Microsoft.AppInstaller",
                "BlenderFoundation.Blender",
                "StreetPea.chiaki-ng",
                "Git.Git",
                "Microsoft.DotNet.DesktopRuntime.8",
                "CreativeTechnology.OpenAL",
                "Python.Launcher",
                "Roblox.Roblox",
                "WiresharkFoundation.Wireshark",
            ]
        );
        let git = find(&packages, "Git.Git");
        assert_eq!(git.name, "Git");
        assert_eq!(git.version.as_deref(), Some("2.55.0.3"));
        assert_eq!(git.available_version.as_deref(), Some("2.56.0.4"));
        let chiaki = find(&packages, "StreetPea.chiaki-ng");
        assert_eq!(chiaki.name, "chiaki-ng version 1.9.9");
        assert_eq!(chiaki.available_version, None);
        let runtime = find(&packages, "Microsoft.DotNet.DesktopRuntime.8");
        assert_eq!(
            runtime.name,
            "Microsoft Windows Desktop Runtime - 8.0.30 (x64)"
        );
        let roblox = find(&packages, "Roblox.Roblox");
        assert_eq!(roblox.version, None, "Unknown is no version");
        assert_eq!(
            roblox.available_version, None,
            "an unknown version isn't outdated, as plain winget upgrade sees it"
        );
        assert_eq!(
            find(&packages, "Python.Launcher").version.as_deref(),
            Some("< 3.14.7")
        );
    }

    #[test]
    fn winget_upgrades_list_outdated_packages() {
        let packages = winget_upgrades(WINGET_UPGRADE);
        // OpenAL and Roblox have an unknown installed version, which plain
        // winget upgrade doesn't count as outdated either.
        assert_eq!(packages.len(), 6, "{packages:#?}");
        assert!(!packages.iter().any(|package| {
            package.id == "Roblox.Roblox" || package.id == "CreativeTechnology.OpenAL"
        }));
        let hwinfo = find(&packages, "REALiX.HWiNFO");
        assert_eq!(hwinfo.name, "HWiNFO® 64");
        assert_eq!(hwinfo.version.as_deref(), Some("8.50"));
        assert_eq!(hwinfo.available_version.as_deref(), Some("8.54"));
        let miniforge = find(&packages, "CondaForge.Miniforge3");
        assert_eq!(miniforge.version.as_deref(), Some("26.3.2-3"));
        assert_eq!(
            find(&packages, "Microsoft.VisualStudio.2022.BuildTools")
                .available_version
                .as_deref(),
            Some("17.14.44")
        );
        let git = winget_list(WINGET_LIST_ONE).unwrap();
        assert_eq!(git.len(), 1);
        assert_eq!(git[0].version.as_deref(), Some("2.55.0.3"));
        assert!(
            winget_list("No installed package found matching input criteria.").is_err(),
            "text that isn't a table is no list"
        );
        assert_eq!(winget_list(""), Ok(Vec::new()));
        assert_eq!(
            winget_version_text("v1.29.380\r\n").as_deref(),
            Some("1.29.380")
        );
        assert_eq!(
            winget_shown_id(
                "   - \r\nFound Mozilla Firefox [Mozilla.Firefox]\r\nVersion: 129.0\r\n"
            )
            .as_deref(),
            Some("Mozilla.Firefox")
        );
        assert_eq!(
            winget_shown_id("No package found matching input criteria."),
            None
        );
        // Linux .bin and .run installers: programs run themselves, scripts
        // run with sh.
        assert!(is_elf(b"\x7fELF\x02\x01"));
        assert!(!is_elf(b"#!/bin/sh\n"));
        assert!(!is_elf(b"\x7fEL"));
        // Flatpak and snap run in C.UTF-8 where it exists.
        assert!(has_c_utf8("C\nC.utf8\nPOSIX\nen_US.utf8\n"));
        assert!(has_c_utf8("C.UTF-8\n"));
        assert!(!has_c_utf8("C\nPOSIX\nen_US.UTF-8\n"));
    }

    #[test]
    fn winget_tables_survive_progress_and_second_tables() {
        let output = "   - \r   \\ \r\r\nName  Id       Version Available Source\n-------------------------------------\n\
Foo   Foo.Foo  1.0     2.0       winget\n\
1 upgrades available.\n\n\
The following packages have an upgrade available, but require explicit targeting for upgrade:\n\
Name  Id       Version Available Source\n-------------------------------------\n\
Bar   Bar.Bar  3.0     4.0       winget\n";
        let packages = winget_upgrades(output);
        assert_eq!(
            packages
                .iter()
                .map(|package| package.id.as_str())
                .collect::<Vec<_>>(),
            ["Foo.Foo", "Bar.Bar"]
        );
        // Wide characters take two columns, as winget pads them.
        let wide = "Name     Id              Version Source\n---------------------------------------\n\
微信     Tencent.WeChat  3.9     winget\n";
        let rows = winget_tables(wide);
        assert_eq!(rows[0][0], "微信");
        assert_eq!(rows[0][1], "Tencent.WeChat");
        // A cut-short ID can't be installed by.
        let cut = "Name Id           Version Source\n-------------------------------\nA    Very.Long.I… 1.0     winget\nB    B.B          2.0     winget\n";
        assert_eq!(
            winget_list(cut)
                .unwrap()
                .iter()
                .map(|package| package.id.as_str())
                .collect::<Vec<_>>(),
            ["B.B"]
        );
        assert_eq!(
            winget_exit_code(0x8A15_0061),
            Some("The package is installed already")
        );
    }

    #[test]
    fn winget_tables_read_labels_with_spaces_and_refuse_unreadable_output() {
        // Korean winget's header has a label with a space in it, 사용 가능
        // ("available"); columns are padded by display width, one space
        // apart.
        fn pad(text: &str, width: usize) -> String {
            let used = text.chars().map(display_width).sum::<usize>();
            format!("{text}{}", " ".repeat(width - used))
        }
        fn line(cells: [&str; 5]) -> String {
            cells
                .iter()
                .zip([9, 15, 7, 9, 6])
                .map(|(cell, width)| pad(cell, width))
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end()
                .to_owned()
        }
        fn table(rows: &[[&str; 5]]) -> String {
            let mut text = format!(
                "{}\n{}\n",
                line(["이름", "ID", "버전", "사용 가능", "원본"]),
                "-".repeat(50)
            );
            for row in rows {
                text.push_str(&line(*row));
                text.push('\n');
            }
            text.push_str("2 업그레이드를 사용할 수 있습니다.\n");
            text
        }
        // A row runs across the label's space, so it isn't a column.
        let packages = winget_list(&table(&[
            ["Git", "Git.Git", "2.45.0", "2.46.0", "winget"],
            ["7-Zip", "7zip.7zip", "23.01", "", "winget"],
        ]))
        .unwrap();
        assert_eq!(packages.len(), 2, "{packages:#?}");
        assert_eq!(
            find(&packages, "Git.Git").available_version.as_deref(),
            Some("2.46.0")
        );
        assert_eq!(find(&packages, "7zip.7zip").available_version, None);
        // No row does: the extra column is read around.
        let packages = winget_list(&table(&[["Foo", "Foo.Foo", "1.0", "1.1", "winget"]])).unwrap();
        assert_eq!(packages[0].available_version.as_deref(), Some("1.1"));
        assert_eq!(packages[0].version.as_deref(), Some("1.0"));

        // Output that holds no row this can read is a failed scan, not an
        // empty one, so the server keeps the device's last list.
        let garbled = "Name Id Version Source\n----------------------\nThis line is not a row of the table at all, winget\n";
        assert!(winget_list(garbled).is_err());
        let shifted =
            "Name Id Version Source\n----------------------\nA    not an id here    winget\n";
        assert!(winget_list(shifted).is_err());
        // Rows without a source are programs winget can't manage: an empty
        // list is right.
        let unmanaged = "Name   Id                 Version Source\n----------------------------------------\nGame   ARP\\Machine\\X64\\G  1.0\n";
        assert_eq!(winget_list(unmanaged), Ok(Vec::new()));
    }

    #[test]
    fn chocolatey_lists_and_outdated_packages() {
        let installed = chocolatey_list(CHOCOLATEY_LIST);
        assert_eq!(
            find(&installed, "python314").version.as_deref(),
            Some("3.14.6")
        );
        assert!(installed.iter().any(|package| package.id == "KB2919355"));
        let outdated = chocolatey_outdated(CHOCOLATEY_OUTDATED);
        assert_eq!(outdated.len(), 6);
        let tools = find(&outdated, "visualstudio2026buildtools");
        assert_eq!(tools.version.as_deref(), Some("118.8.0"));
        assert_eq!(tools.available_version.as_deref(), Some("118.11.3"));
        let merged = merge(installed, outdated);
        assert_eq!(
            find(&merged, "python314").available_version.as_deref(),
            Some("3.14.10")
        );
        assert_eq!(find(&merged, "vcredist140").available_version, None);
        // Warnings and other lines without fields are ignored.
        assert!(chocolatey_outdated("Chocolatey v2.7.3\nOutdated Packages\n").is_empty());
    }

    #[test]
    fn homebrew_lists_formulae_casks_and_outdated_ones() {
        let formulae = homebrew_list(HOMEBREW_LIST, PackageSource::Homebrew);
        assert_eq!(
            find(&formulae, "openssl@3").version.as_deref(),
            Some("3.4.1")
        );
        assert_eq!(
            find(&formulae, "python@3.13").version.as_deref(),
            Some("3.13.2"),
            "the newest of two"
        );
        let casks = homebrew_list(HOMEBREW_CASKS, PackageSource::HomebrewCask);
        assert_eq!(find(&casks, "firefox").version.as_deref(), Some("135.0.1"));
        let outdated = homebrew_outdated(HOMEBREW_OUTDATED).unwrap();
        let openssl = find(&outdated, "openssl@3");
        assert_eq!(openssl.source, PackageSource::Homebrew);
        assert_eq!(openssl.version.as_deref(), Some("3.4.1"));
        assert_eq!(openssl.available_version.as_deref(), Some("3.4.2"));
        let firefox = find(&outdated, "firefox");
        assert_eq!(firefox.source, PackageSource::HomebrewCask);
        assert_eq!(firefox.available_version.as_deref(), Some("136.0"));
        assert!(homebrew_outdated("Error: not json").is_err());
        let merged = merge(formulae.into_iter().chain(casks).collect(), outdated);
        assert_eq!(
            find(&merged, "firefox").available_version.as_deref(),
            Some("136.0")
        );
    }

    #[test]
    fn flatpak_lists_system_applications_and_updates() {
        let installed = flatpak_list(FLATPAK_LIST);
        assert_eq!(installed.len(), 3);
        let firefox = find(&installed, "org.mozilla.firefox");
        assert_eq!(firefox.name, "Firefox");
        assert_eq!(firefox.version.as_deref(), Some("135.0.1"));
        assert_eq!(find(&installed, "com.example.NoVersion").version, None);
        let updates = flatpak_updates(FLATPAK_UPDATES);
        assert_eq!(
            updates,
            [
                ("org.mozilla.firefox".to_owned(), Some("136.0".to_owned())),
                ("org.gimp.GIMP".to_owned(), None)
            ]
        );
    }

    #[test]
    fn flatpak_lists_remotes() {
        assert_eq!(
            flatpak_remotes("fedora\nflathub\n\n--bad\n"),
            ["fedora", "flathub"]
        );
    }

    #[test]
    fn snap_lists_snaps_and_refreshes() {
        let installed = snap_list(SNAP_LIST);
        assert_eq!(
            installed
                .iter()
                .map(|package| package.id.as_str())
                .collect::<Vec<_>>(),
            ["code", "firefox", "gnome-46-2404"],
            "bases, the core and snapd are the system"
        );
        let applications = snapd_applications(SNAPD_SNAPS).unwrap();
        let mut names = applications.iter().map(String::as_str).collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            ["code", "firefox", "lxd"],
            "content snaps, bases, kernels and gadgets aren't applications"
        );
        assert_eq!(snapd_applications("<html>"), None);
        assert_eq!(
            snapd_applications(r#"{"type":"error","result":{"message":"x"}}"#),
            None
        );
        let updates = snap_updates(SNAP_REFRESH);
        assert_eq!(
            updates,
            [
                ("code".to_owned(), "1.98.0".to_owned()),
                ("firefox".to_owned(), "136.0-1".to_owned()),
                ("core22".to_owned(), "20250301".to_owned())
            ]
        );
        assert!(snap_updates("All snaps up to date.\n").is_empty());
    }

    #[test]
    fn arguments_split_like_a_shell_without_expanding() {
        assert_eq!(
            split_arguments(r#"--silent --dir "/opt/My App" 'it''s' a\ b "q\"x" $HOME"#).unwrap(),
            [
                "--silent",
                "--dir",
                "/opt/My App",
                "its",
                "a b",
                "q\"x",
                "$HOME"
            ]
        );
        assert_eq!(split_arguments("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_arguments("''").unwrap(), [""]);
        assert!(split_arguments("'open").is_err());
    }

    #[test]
    fn msi_uninstall_strings_name_their_product() {
        assert_eq!(
            msi_product_code("MsiExec.exe /I{23170F69-40C1-2702-2408-000001000000}").as_deref(),
            Some("{23170F69-40C1-2702-2408-000001000000}")
        );
        assert_eq!(
            msi_product_code("MsiExec.exe /X{23170F69-40C1-2702-2408-000001000000} /qn").as_deref(),
            Some("{23170F69-40C1-2702-2408-000001000000}")
        );
        assert_eq!(
            msi_product_code("\"C:\\Program Files\\7-Zip\\Uninstall.exe\""),
            None
        );
        assert_eq!(msi_product_code("MsiExec.exe /I{not-a-guid}"), None);
        assert!(is_product_code("{9A25302D-30C0-39D9-BD6F-21E6EC160475}"));
        assert!(!is_product_code("9A25302D-30C0-39D9-BD6F-21E6EC160475"));
    }
}
