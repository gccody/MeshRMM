//! Folders and several entries download as one ZIP archive, written as it
//! is sent: each file's sizes and checksum follow its data, so nothing is
//! read twice or held whole. Links inside a folder are left out, never
//! followed. Files that can't be read are left out too, and listed in a
//! note at the end of the archive.
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use meshrmm_protocol::{FileError, FileErrorCode};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use super::operations::format_bytes;
use super::{io_error, paths, platform};

/// The note listing what was left out.
pub(super) const SKIPPED_NOTE: &str = "MeshRMM - files left out.txt";
/// Files from this size get ZIP64 headers; a stream can't go back to add
/// them once a file turns out larger.
const LARGE_FILE_BYTES: u64 = 0x8000_0000;

/// What an archive held.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub files: u64,
    /// The files' bytes, before compression.
    pub bytes: u64,
    pub skipped: Vec<String>,
}

/// Writes an archive of `paths` to `out`, with at most `max_bytes` of file
/// content. Each entry goes in under its own name, a folder with
/// everything in it.
pub(super) fn write<W: Write>(
    paths: &[PathBuf],
    max_bytes: u64,
    out: W,
) -> Result<(Summary, W), FileError> {
    let mut archive = Archive {
        zip: ZipWriter::new_stream(out),
        summary: Summary::default(),
        max_bytes,
        buffer: vec![0; 256 * 1024],
    };
    for path in paths {
        let name = paths::name(path);
        // A link chosen itself is followed; links inside are not.
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => archive.folder(path, &name)?,
            Ok(metadata) if metadata.is_file() => archive.file(path, &name, &metadata)?,
            Ok(_) => archive.skip(&name, "it isn't a file or a folder"),
            Err(error) => return Err(io_error(&error, path)),
        }
    }
    if !archive.summary.skipped.is_empty() {
        let note = format!(
            "These entries couldn't be added to the archive:\r\n\r\n{}\r\n",
            archive.summary.skipped.join("\r\n")
        );
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        archive
            .zip
            .start_file(SKIPPED_NOTE, options)
            .and_then(|()| archive.zip.write_all(note.as_bytes()).map_err(Into::into))
            .map_err(failed)?;
    }
    let Archive { zip, summary, .. } = archive;
    let out = zip.finish().map_err(failed)?.into_inner();
    Ok((summary, out))
}

fn failed(error: zip::result::ZipError) -> FileError {
    match error {
        zip::result::ZipError::Io(error) => io_failed(error),
        error => FileError::new(
            FileErrorCode::Failed,
            format!("The archive failed: {error}"),
        ),
    }
}

/// A failure writing the archive out, which is the transfer's socket.
fn io_failed(error: io::Error) -> FileError {
    FileError::new(
        FileErrorCode::Failed,
        format!("The archive failed: {error}"),
    )
}

struct Archive<W: Write> {
    zip: ZipWriter<zip::write::StreamWriter<W>>,
    summary: Summary,
    max_bytes: u64,
    buffer: Vec<u8>,
}

impl<W: Write> Archive<W> {
    fn skip(&mut self, name: &str, why: &str) {
        self.summary.skipped.push(format!("{name}: {why}"));
    }

    fn folder(&mut self, path: &Path, name: &str) -> Result<(), FileError> {
        let options = SimpleFileOptions::default();
        self.zip
            .add_directory(format!("{name}/"), options)
            .map_err(failed)?;
        let mut items = match fs::read_dir(path) {
            Ok(listing) => listing.flatten().collect::<Vec<_>>(),
            Err(error) => {
                self.skip(&format!("{name}/"), &reason(&error, path));
                return Ok(());
            }
        };
        items.sort_by_key(|item| item.file_name());
        for item in items {
            let child = format!("{name}/{}", item.file_name().to_string_lossy());
            let path = item.path();
            match item.metadata() {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    self.skip(&child, "it's a link")
                }
                Ok(metadata) if metadata.is_dir() => self.folder(&path, &child)?,
                Ok(metadata) if metadata.is_file() => self.file(&path, &child, &metadata)?,
                Ok(_) => self.skip(&child, "it isn't a file or a folder"),
                Err(error) => self.skip(&child, &reason(&error, &path)),
            }
        }
        Ok(())
    }

    fn file(&mut self, path: &Path, name: &str, metadata: &fs::Metadata) -> Result<(), FileError> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                self.skip(name, &reason(&error, path));
                return Ok(());
            }
        };
        let mut options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(1))
            .large_file(metadata.len() >= LARGE_FILE_BYTES);
        if let Some(time) = metadata.modified().ok().and_then(zip_time) {
            options = options.last_modified_time(time);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            options = options.unix_permissions(metadata.permissions().mode() & 0o7777);
        }
        self.zip.start_file(name, options).map_err(failed)?;
        loop {
            let read = match file.read(&mut self.buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // The file is cut short in the archive; the note says so.
                Err(error) => {
                    self.skip(name, &format!("reading it failed partway: {error}"));
                    break;
                }
            };
            self.summary.bytes += read as u64;
            if self.summary.bytes > self.max_bytes {
                return Err(FileError::new(
                    FileErrorCode::TooLarge,
                    format!(
                        "The files grew past this server's {} transfer limit while they were sent.",
                        format_bytes(self.max_bytes)
                    ),
                ));
            }
            self.zip
                .write_all(&self.buffer[..read])
                .map_err(io_failed)?;
        }
        self.summary.files += 1;
        Ok(())
    }
}

fn reason(error: &io::Error, path: &Path) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "access is denied".into(),
        io::ErrorKind::NotFound => "it was deleted while the archive was written".into(),
        _ => io_error(error, path).message,
    }
}

/// `time` as a ZIP archive keeps it: the device's local time, to two
/// seconds, between 1980 and 2107.
fn zip_time(time: std::time::SystemTime) -> Option<DateTime> {
    let (year, month, day, hour, minute, second) = platform::local_civil(time)?;
    DateTime::from_date_and_time(year, month, day, hour, minute, second).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn folders_stream_into_archives_that_unpack() {
        let root = std::env::temp_dir().join(format!("meshrmm-zip-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("tree/empty")).unwrap();
        fs::create_dir_all(root.join("tree/sub")).unwrap();
        fs::write(root.join("tree/a.txt"), b"alpha").unwrap();
        fs::write(root.join("tree/sub/b.bin"), vec![7; 300_000]).unwrap();
        fs::write(root.join("single.txt"), b"one").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", root.join("tree/link")).unwrap();
        let (summary, bytes) = write(
            &[root.join("tree"), root.join("single.txt")],
            1_000_000,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(summary.files, 3);
        assert_eq!(summary.bytes, 300_008);
        assert_eq!(summary.skipped.len(), usize::from(cfg!(unix)));
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut names = archive.file_names().map(str::to_owned).collect::<Vec<_>>();
        names.sort();
        let mut expected = vec![
            "single.txt",
            "tree/",
            "tree/a.txt",
            "tree/empty/",
            "tree/sub/",
            "tree/sub/b.bin",
        ];
        if cfg!(unix) {
            expected.insert(0, SKIPPED_NOTE);
        }
        assert_eq!(names, expected);
        let mut content = Vec::new();
        archive
            .by_name("tree/sub/b.bin")
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, vec![7; 300_000]);
        assert_eq!(
            write(&[root.join("tree")], 10, Vec::new())
                .unwrap_err()
                .code,
            FileErrorCode::TooLarge
        );
        let _ = fs::remove_dir_all(&root);
    }
}
