use super::*;
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("meshrmm-browser-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn rejects_device_paths_and_unsafe_names() {
    let root = Path::new("C:\\");
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "a\\b",
        "C:foo",
        "name:stream",
        "CON.txt",
        "COM1",
        "LPT²",
        "trailing.",
        "trailing ",
        "bad\0name",
    ] {
        assert!(child_path(root, name).is_err(), "{name:?}");
    }
    assert_eq!(
        child_path(root, "résumé 2026.txt").unwrap(),
        root.join("résumé 2026.txt")
    );
    assert!(location("relative\\path").is_err());
    assert!(location("\\\\.\\PhysicalDrive0").is_err());
    assert!(location("\\\\?\\GLOBALROOT\\Device").is_err());
    assert!(location("C:\\Windows").is_ok());
    assert!(location("\\\\server\\share\\folder").is_ok());
}
#[test]
fn browse_create_rename_and_copy_preserve_existing_files() {
    let directory = Directory::new();
    let folder = child_path(&directory.0, "Subfolder").unwrap();
    Work::NewFolder(directory.0.clone(), folder.clone())
        .execute()
        .unwrap();
    let source = child_path(&directory.0, "résumé file.txt").unwrap();
    std::fs::write(&source, b"original").unwrap();
    let renamed = child_path(&directory.0, "renamed.txt").unwrap();
    Work::Rename(directory.0.clone(), source.clone(), renamed.clone())
        .execute()
        .unwrap();
    assert!(!source.exists());
    let destination = folder.join("renamed.txt");
    Work::Copy(folder.clone(), renamed.clone(), destination.clone())
        .execute()
        .unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), b"original");
    std::fs::write(&destination, b"keep this").unwrap();
    assert!(copy_file(&renamed, &destination).is_err());
    assert!(rename(&renamed, &destination).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), b"keep this");
    assert!(renamed.exists());
    let rows = entries(&directory.0).unwrap();
    assert!(rows[0].directory);
    assert_eq!(rows[1].name, "renamed.txt");
    assert!(entries(&directory.0.join("missing")).is_err());
}
#[test]
fn preview_is_bounded_and_supports_windows_text_encodings() {
    let directory = Directory::new();
    let path = directory.0.join("preview.txt");
    std::fs::write(&path, "Hello\n世界").unwrap();
    assert_eq!(preview(&path).unwrap(), "Hello\r\n世界");
    let bytes: Vec<_> = [0xff, 0xfe]
        .into_iter()
        .chain("Hello\r\n世界".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(preview(&path).unwrap(), "Hello\r\n世界");
    std::fs::write(&path, b"binary\0content").unwrap();
    assert!(preview(&path).is_err());
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_PREVIEW + 1).unwrap();
    drop(file);
    assert!(preview(&path).is_err());
}
