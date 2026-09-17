//! Filesystem operations run off the UI thread. Never follow reparse points during recursive mutations.
use super::{MAX_ENTRIES, MAX_PREVIEW, path_wide, wide};
use anyhow::{Context, ensure};
use std::{
    io::Read,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{Storage::FileSystem::*, UI::Shell::*},
    core::PCWSTR,
};
pub(super) fn location(text: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !text.contains('\0') && text.len() <= 32767,
        "Invalid location."
    );
    if text.eq_ignore_ascii_case("This PC") {
        return Ok(PathBuf::new());
    }
    if text.eq_ignore_ascii_case("Quick access") {
        return Ok(PathBuf::from("::QuickAccess"));
    }
    let path = PathBuf::from(text);
    ensure!(
        path.is_absolute(),
        "Enter an absolute path, for example C:\\Windows."
    );
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        ensure!(
            !matches!(
                prefix.kind(),
                std::path::Prefix::DeviceNS(_) | std::path::Prefix::Verbatim(_)
            ),
            "Device paths cannot be browsed."
        );
    }
    Ok(path)
}
pub(super) fn child_path(directory: &Path, name: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.encode_utf16().count() <= 255
            && name != "."
            && name != ".."
            && !name.ends_with(['.', ' '])
            && !name.chars().any(|c| c < ' ' || "\\/:*?\"<>|".contains(c)),
        "Enter a single file or folder name without path separators."
    );
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    ensure!(!reserved, "That name is reserved by Windows.");
    Ok(directory.join(name))
}

#[derive(Clone)]
pub(super) struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
    pub size: Option<u64>,
    pub modified: u64,
    pub hidden: bool,
    pub kind: String,
    pub icon: i32,
}
pub(super) fn virtual_location(path: &Path) -> bool {
    path.as_os_str().is_empty() || path == Path::new("::QuickAccess")
}
pub(super) fn location_label(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "This PC".into()
    } else if path == Path::new("::QuickAccess") {
        "Quick access".into()
    } else {
        path.to_string_lossy().into_owned()
    }
}
pub(super) fn entries(path: &Path) -> anyhow::Result<Vec<Entry>> {
    if virtual_location(path) {
        let public =
            PathBuf::from(std::env::var("PUBLIC").unwrap_or_else(|_| "C:\\Users\\Public".into()));
        let mut places = Vec::new();
        for name in [
            "Desktop",
            "Documents",
            "Downloads",
            "Music",
            "Pictures",
            "Videos",
        ] {
            let folder = public.join(name);
            if folder.is_dir() {
                places.push((name.to_owned(), folder, "File folder"));
            }
        }
        if path.as_os_str().is_empty() {
            let drives = unsafe { GetLogicalDrives() };
            for index in 0..26 {
                if drives & (1 << index) != 0 {
                    let letter = (b'A' + index) as char;
                    places.push((
                        format!("Local Disk ({letter}:)"),
                        PathBuf::from(format!("{letter}:\\")),
                        "Local Disk",
                    ));
                }
            }
        }
        return Ok(places
            .into_iter()
            .map(|(name, path, kind)| {
                let mut info = SHFILEINFOW::default();
                unsafe {
                    SHGetFileInfoW(
                        PCWSTR(path_wide(&path).as_ptr()),
                        FILE_ATTRIBUTE_DIRECTORY,
                        Some(&mut info),
                        std::mem::size_of::<SHFILEINFOW>() as u32,
                        SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
                    );
                }
                Entry {
                    path,
                    name,
                    directory: true,
                    size: None,
                    modified: 0,
                    hidden: false,
                    kind: kind.into(),
                    icon: info.iIcon,
                }
            })
            .collect());
    }
    let mut result = Vec::new();
    for entry in
        std::fs::read_dir(path).with_context(|| format!("Cannot read {}", path.display()))?
    {
        let entry = entry?;
        ensure!(
            result.len() < MAX_ENTRIES,
            "This folder exceeds the {MAX_ENTRIES} entry limit. Enter a subfolder path."
        );
        let metadata = entry.metadata();
        let directory = metadata.as_ref().is_ok_and(|m| m.is_dir());
        let modified = metadata.as_ref().ok().map_or(0, |m| m.last_write_time());
        let hidden = metadata
            .as_ref()
            .is_ok_and(|m| m.file_attributes() & 2 != 0);
        let size = metadata.ok().filter(|m| m.is_file()).map(|m| m.len());
        let mut info = SHFILEINFOW::default();
        unsafe {
            SHGetFileInfoW(
                PCWSTR(path_wide(&entry.path()).as_ptr()),
                if directory {
                    FILE_ATTRIBUTE_DIRECTORY
                } else {
                    FILE_ATTRIBUTE_NORMAL
                },
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
            );
        }
        let kind = String::from_utf16_lossy(
            &info.szTypeName[..info
                .szTypeName
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(info.szTypeName.len())],
        );
        result.push(Entry {
            modified,
            hidden,
            kind,
            icon: info.iIcon,
            path: entry.path(),
            name: entry.file_name().to_string_lossy().into_owned(),
            directory,
            size,
        });
    }
    sort_entries(&mut result, 0, false);
    Ok(result)
}
pub(super) fn preview(path: &Path) -> anyhow::Result<String> {
    ensure!(path.is_file(), "Select a regular file to preview.");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_PREVIEW + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PREVIEW,
        "Text preview is limited to 1 MiB."
    );
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        ensure!(bytes.len().is_multiple_of(2), "Invalid UTF-16 text.");
        let little = bytes[0] == 0xff;
        let units: Vec<_> = bytes[2..]
            .chunks_exact(2)
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        String::from_utf16(&units).context("This file is not valid text.")?
    } else {
        String::from_utf8(bytes)
            .context("Preview supports UTF-8 and BOM-marked UTF-16 text files.")?
            .trim_start_matches('\u{feff}')
            .to_owned()
    };
    ensure!(
        !text.contains('\0'),
        "This file is binary and cannot be displayed as text."
    );
    Ok(text.replace("\r\n", "\n").replace('\n', "\r\n"))
}
pub(super) fn copy_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    ensure!(source.is_file(), "Select a regular file to copy.");
    unsafe {
        CopyFileW(
            PCWSTR(path_wide(source).as_ptr()),
            PCWSTR(path_wide(destination).as_ptr()),
            true,
        )?;
    }
    Ok(())
}
pub(super) fn rename(source: &Path, destination: &Path) -> anyhow::Result<()> {
    unsafe {
        MoveFileW(
            PCWSTR(path_wide(source).as_ptr()),
            PCWSTR(path_wide(destination).as_ptr()),
        )?;
    }
    Ok(())
}

pub(super) fn search_matches(name: &str, query: &str) -> bool {
    if query.contains(['*', '?']) {
        unsafe {
            PathMatchSpecW(PCWSTR(wide(name).as_ptr()), PCWSTR(wide(query).as_ptr())).as_bool()
        }
    } else {
        name.to_lowercase().contains(&query.to_lowercase())
    }
}
pub(super) fn sort_entries(rows: &mut [Entry], column: usize, descending: bool) {
    rows.sort_by(|a, b| {
        let folders = b.directory.cmp(&a.directory);
        if folders != std::cmp::Ordering::Equal {
            return folders;
        }
        let order = match column {
            1 => a.modified.cmp(&b.modified),
            2 => a.kind.cmp(&b.kind),
            3 => a.size.cmp(&b.size),
            _ => unsafe {
                StrCmpLogicalW(
                    PCWSTR(wide(&a.name).as_ptr()),
                    PCWSTR(wide(&b.name).as_ptr()),
                )
                .cmp(&0)
            },
        }
        .then_with(|| a.name.cmp(&b.name));
        if descending { order.reverse() } else { order }
    });
}
#[cfg(test)]
pub(super) fn copy_tree(source: &Path, target: &Path) -> anyhow::Result<()> {
    copy_tree_cancellable(source, target, &std::sync::atomic::AtomicBool::new(false))
}
pub(super) fn copy_tree_cancellable(
    source: &Path,
    target: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    ensure!(
        !cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "Copy cancelled; completed items were kept."
    );
    let metadata = std::fs::symlink_metadata(source)?;
    ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "Copying junctions or symbolic links is not supported: {}",
        source.display()
    );
    if metadata.is_dir() {
        let source_real = std::fs::canonicalize(source)?;
        let parent_real = std::fs::canonicalize(target.parent().context("Invalid destination")?)?;
        ensure!(
            !parent_real.starts_with(&source_real),
            "A folder cannot be copied into itself."
        );
        std::fs::create_dir(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree_cancellable(&entry.path(), &target.join(entry.file_name()), cancelled)?;
        }
        Ok(())
    } else {
        copy_file(source, target)
    }
}
pub(super) fn unique_target(directory: &Path, source: &Path) -> PathBuf {
    let name = source.file_name().unwrap_or_default();
    let original = directory.join(name);
    if !original.exists() {
        return original;
    }
    let stem = if source.is_dir() {
        name
    } else {
        source.file_stem().unwrap_or(name)
    }
    .to_string_lossy();
    let extension = if source.is_dir() {
        String::new()
    } else {
        source
            .extension()
            .map_or(String::new(), |e| format!(".{}", e.to_string_lossy()))
    };
    for index in 1.. {
        let suffix = if index == 1 {
            " - Copy".to_owned()
        } else {
            format!(" - Copy ({index})")
        };
        let candidate = directory.join(format!("{stem}{suffix}{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}
pub(super) fn delete_tree(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    // remove_dir removes a junction itself, never its target.
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            std::fs::remove_dir(path)?;
        } else {
            std::fs::remove_dir_all(path)?;
        }
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
#[derive(Default)]
pub(super) struct History {
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
}
impl History {
    pub fn visit(&mut self, previous: &Path, next: &Path) {
        if previous != next {
            self.back.push(previous.to_path_buf());
            self.forward.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("meshrmm-explorer-model-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn recursive_copy_refuses_collisions_and_self_descendants() {
        let f = Fixture::new();
        let source = f.0.join("source");
        let target = f.0.join("target");
        std::fs::create_dir_all(source.join("child")).unwrap();
        std::fs::write(source.join("child/data.txt"), b"preserved").unwrap();
        assert!(copy_tree(&source, &source.join("nested")).is_err());
        assert!(!source.join("nested").exists());
        copy_tree(&source, &target).unwrap();
        assert_eq!(
            std::fs::read(target.join("child/data.txt")).unwrap(),
            b"preserved"
        );
        std::fs::write(target.join("child/data.txt"), b"existing").unwrap();
        assert!(copy_tree(&source, &target).is_err());
        assert_eq!(
            std::fs::read(target.join("child/data.txt")).unwrap(),
            b"existing"
        );
        delete_tree(&target).unwrap();
        assert!(source.join("child/data.txt").exists());
    }
    #[test]
    fn natural_sort_hidden_metadata_and_duplicate_names() {
        let f = Fixture::new();
        for name in ["file10.txt", "file2.txt", "file1.txt"] {
            std::fs::write(f.0.join(name), name).unwrap();
        }
        std::fs::create_dir(f.0.join("z folder")).unwrap();
        let mut rows = entries(&f.0).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["z folder", "file1.txt", "file2.txt", "file10.txt"]
        );
        sort_entries(&mut rows, 0, true);
        assert_eq!(rows[0].name, "z folder");
        assert_eq!(rows[1].name, "file10.txt");
        let first = unique_target(&f.0, &f.0.join("file1.txt"));
        assert_eq!(first.file_name().unwrap(), "file1 - Copy.txt");
        std::fs::write(&first, b"occupied").unwrap();
        assert_eq!(
            unique_target(&f.0, &f.0.join("file1.txt"))
                .file_name()
                .unwrap(),
            "file1 - Copy (2).txt"
        );
        unsafe {
            SetFileAttributesW(PCWSTR(path_wide(&first).as_ptr()), FILE_ATTRIBUTE_HIDDEN).unwrap();
        }
        assert!(
            entries(&f.0)
                .unwrap()
                .iter()
                .find(|e| e.path == first)
                .unwrap()
                .hidden
        );
    }
    #[test]
    fn search_supports_literal_names_and_windows_wildcards() {
        assert!(search_matches("Résumé.TXT", "RÉSUMÉ"));
        assert!(search_matches("report.txt", "*.TXT"));
        assert!(search_matches("file2.log", "file?.log"));
        assert!(!search_matches("file20.log", "file?.log"));
        assert!(!search_matches("photo.png", "*.txt"));
    }
    #[test]
    fn cancellation_does_not_start_a_copy() {
        let f = Fixture::new();
        let source = f.0.join("source.txt");
        let target = f.0.join("target.txt");
        std::fs::write(&source, b"preserve").unwrap();
        assert!(
            copy_tree_cancellable(&source, &target, &std::sync::atomic::AtomicBool::new(true))
                .is_err()
        );
        assert!(!target.exists());
        assert_eq!(std::fs::read(source).unwrap(), b"preserve");
    }
    #[test]
    fn junction_copy_is_refused_and_delete_preserves_the_target() {
        let f = Fixture::new();
        let source = f.0.join("target");
        let link = f.0.join("junction");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("keep.txt"), b"keep").unwrap();
        let result = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "Could not create junction fixture: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(copy_tree(&link, &f.0.join("copy")).is_err());
        delete_tree(&link).unwrap();
        assert!(!link.exists());
        assert_eq!(std::fs::read(source.join("keep.txt")).unwrap(), b"keep");
    }
    #[test]
    fn navigation_history_keeps_refresh_and_branches_correctly() {
        let mut history = History::default();
        history.visit(Path::new("C:\\"), Path::new("C:\\Windows"));
        history.forward.push(PathBuf::from("C:\\Users"));
        history.visit(Path::new("C:\\Windows"), Path::new("C:\\Windows"));
        assert_eq!(history.back.len(), 1);
        assert_eq!(history.forward.len(), 1);
        history.visit(Path::new("C:\\Windows"), Path::new("C:\\Temp"));
        assert_eq!(history.back.len(), 2);
        assert!(history.forward.is_empty());
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
    attributes: u32,
    size: u64,
    written: u64,
}
fn identity(path: &Path) -> anyhow::Result<FileIdentity> {
    unsafe {
        let handle = CreateFileW(
            PCWSTR(path_wide(path).as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let result = GetFileInformationByHandle(handle, &mut info);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        result?;
        Ok(FileIdentity {
            volume: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            attributes: info.dwFileAttributes,
            size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            written: (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(info.ftLastWriteTime.dwLowDateTime),
        })
    }
}
fn snapshot_tree(path: &Path) -> anyhow::Result<Vec<(PathBuf, FileIdentity)>> {
    let mut pending = vec![path.to_path_buf()];
    let mut result = Vec::new();
    while let Some(path) = pending.pop() {
        let info = identity(&path)?;
        ensure!(
            info.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
            "Undo does not traverse symbolic links or junctions."
        );
        if info.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
        result.push((path, info));
        ensure!(
            result.len() <= MAX_ENTRIES,
            "This operation is too large to record for Undo."
        );
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
#[derive(Clone)]
pub(super) struct UndoAction {
    kind: UndoKind,
}
#[derive(Clone)]
enum UndoKind {
    Created {
        path: PathBuf,
        snapshot: Vec<(PathBuf, FileIdentity)>,
    },
    Moved {
        original: PathBuf,
        current: PathBuf,
        identity: FileIdentity,
    },
}
impl UndoAction {
    pub fn created(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            kind: UndoKind::Created {
                path: path.to_path_buf(),
                snapshot: snapshot_tree(path)?,
            },
        })
    }
    pub fn moved(original: &Path, current: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            kind: UndoKind::Moved {
                original: original.to_path_buf(),
                current: current.to_path_buf(),
                identity: identity(current)?,
            },
        })
    }
    fn validate(&self) -> anyhow::Result<()> {
        match &self.kind {
            UndoKind::Created { path, snapshot } => ensure!(
                &snapshot_tree(path)? == snapshot,
                "{} changed after the operation; Undo will not remove it.",
                path.display()
            ),
            UndoKind::Moved {
                original,
                current,
                identity: expected,
            } => {
                ensure!(
                    !original.exists(),
                    "{} already exists; Undo will not replace it.",
                    original.display()
                );
                let actual = identity(current)?;
                ensure!(
                    actual.volume == expected.volume && actual.index == expected.index,
                    "{} was replaced after the operation; Undo was stopped.",
                    current.display()
                );
            }
        }
        Ok(())
    }
    fn apply(&self) -> anyhow::Result<()> {
        self.validate()?;
        match &self.kind {
            UndoKind::Created { path, .. } => delete_tree(path),
            UndoKind::Moved {
                original, current, ..
            } => rename(current, original),
        }
    }
}
pub(super) fn undo(actions: &[UndoAction]) -> anyhow::Result<()> {
    // Validate the entire batch first. A changed/replaced file is never silently
    // deleted to undo an earlier copy or create operation.
    for action in actions {
        action.validate()?;
    }
    for action in actions.iter().rev() {
        action.apply()?;
    }
    Ok(())
}

#[cfg(test)]
mod undo_tests {
    use super::*;
    #[test]
    fn undo_refuses_modified_copies_and_existing_move_destinations() {
        let root = std::env::temp_dir().join(format!("meshrmm-undo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("created.txt");
        std::fs::write(&file, b"original").unwrap();
        let action = UndoAction::created(&file).unwrap();
        std::fs::write(&file, b"changed by another application").unwrap();
        assert!(undo(&[action]).is_err());
        assert!(file.exists());
        let renamed = root.join("renamed.txt");
        rename(&file, &renamed).unwrap();
        let action = UndoAction::moved(&file, &renamed).unwrap();
        std::fs::write(&file, b"new file at original location").unwrap();
        assert!(undo(std::slice::from_ref(&action)).is_err());
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"new file at original location"
        );
        std::fs::remove_file(&file).unwrap();
        undo(&[action]).unwrap();
        assert!(file.exists());
        assert!(!renamed.exists());
        let action = UndoAction::created(&file).unwrap();
        undo(&[action]).unwrap();
        assert!(!file.exists());
        std::fs::remove_dir(root).unwrap();
    }
}
