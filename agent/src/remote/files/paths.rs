//! Paths and names from the website: checked, then normalized the way a
//! file manager reads them.
use std::path::{Component, Path, PathBuf};

use meshrmm_protocol::{FileError, FileErrorCode, MAX_FILE_PATH_BYTES};

use super::invalid_path;

/// An absolute path, with `.` and `..` resolved by name as a file manager
/// does (`/a/b/..` is `/a`) and no trailing separator.
pub(super) fn parse(text: &str) -> Result<PathBuf, FileError> {
    if text.is_empty() {
        return Err(invalid_path("Enter a path."));
    }
    if text.len() > MAX_FILE_PATH_BYTES || text.contains('\0') {
        return Err(invalid_path("That isn't a path this computer can use."));
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(invalid_path(if cfg!(windows) {
            "Enter a full path, such as C:\\Users."
        } else {
            "Enter a full path, such as /home."
        }));
    }
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next()
        && !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_) | std::path::Prefix::UNC(..)
        )
    {
        return Err(invalid_path(
            "Device paths and \\\\?\\ paths can't be browsed; use a drive letter or a share.",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                // The root has no parent; `/..` is `/`.
                if normalized.parent().is_some() {
                    normalized.pop();
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    Ok(normalized)
}

/// The path's text, as the website shows and sends it back.
pub(super) fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The folder above `path`, if it isn't a root.
pub(super) fn parent(path: &Path) -> Option<&Path> {
    path.parent()
}

/// The last part of `path`, or the path itself for a root.
pub(super) fn name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| text(path))
}

/// Checks a name for a new or renamed entry: one part of a path that this
/// computer allows.
pub(super) fn check_name(name: &str) -> Result<(), FileError> {
    let invalid = |message: &str| Err(FileError::new(FileErrorCode::InvalidName, message));
    if name.is_empty() || name == "." || name == ".." {
        return invalid("Enter a name.");
    }
    if name.contains('\0') || name.contains('/') || (cfg!(windows) && name.contains('\\')) {
        return invalid("A name can't contain a slash.");
    }
    let length = if cfg!(windows) {
        name.encode_utf16().count()
    } else {
        name.len()
    };
    if length > 255 {
        return invalid("That name is too long.");
    }
    if cfg!(windows) {
        if name
            .chars()
            .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return invalid("Windows doesn't allow < > : \" | ? * in names.");
        }
        if name.ends_with(['.', ' ']) {
            return invalid("Windows doesn't allow names ending in a dot or a space.");
        }
        if meshrmm_protocol::reserved_windows_name(name) {
            return invalid("That name is reserved by Windows.");
        }
    }
    Ok(())
}

/// `name` in `folder`, once it's a name this computer allows.
pub(super) fn child(folder: &Path, name: &str) -> Result<PathBuf, FileError> {
    check_name(name)?;
    Ok(folder.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn paths_are_absolute_and_normalized() {
        assert_eq!(parse("/").unwrap(), PathBuf::from("/"));
        assert_eq!(parse("/home/ada/").unwrap(), PathBuf::from("/home/ada"));
        assert_eq!(
            parse("/home/./ada/../bob").unwrap(),
            PathBuf::from("/home/bob")
        );
        assert_eq!(parse("/..").unwrap(), PathBuf::from("/"));
        assert_eq!(parse("//tmp").unwrap(), PathBuf::from("/tmp"));
        for bad in ["", "home", "./x", "/a\0b"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parent(Path::new("/")), None);
        assert_eq!(name(Path::new("/")), "/");
        assert_eq!(name(Path::new("/var/log")), "log");
    }

    #[cfg(windows)]
    #[test]
    fn paths_are_absolute_and_normalized() {
        assert_eq!(parse("C:\\").unwrap(), PathBuf::from("C:\\"));
        assert_eq!(
            parse("C:\\Users\\ada\\..\\bob\\").unwrap(),
            PathBuf::from("C:\\Users\\bob")
        );
        assert_eq!(
            parse("c:/Windows/./System32").unwrap(),
            PathBuf::from("c:\\Windows\\System32")
        );
        assert_eq!(parse("C:\\..").unwrap(), PathBuf::from("C:\\"));
        assert_eq!(
            parse("\\\\server\\share\\x").unwrap(),
            PathBuf::from("\\\\server\\share\\x")
        );
        for bad in [
            "",
            "C:",
            "C:foo",
            "foo\\bar",
            "\\\\?\\C:\\x",
            "\\\\.\\PhysicalDrive0",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(name(Path::new("C:\\")), "C:\\");
    }

    #[test]
    fn names_follow_the_platforms_rules() {
        for good in ["notes.txt", "a b", ".profile", "日本語"] {
            check_name(good).unwrap();
        }
        for bad in ["", ".", "..", "a/b", "a\0b", &"x".repeat(256)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            check_name("con.txt").is_err(),
            cfg!(windows),
            "reserved only on Windows"
        );
        assert_eq!(check_name("a:b").is_err(), cfg!(windows));
        assert_eq!(check_name("trailing.").is_err(), cfg!(windows));
        assert!(child(Path::new("/tmp"), "x/y").is_err());
    }
}
