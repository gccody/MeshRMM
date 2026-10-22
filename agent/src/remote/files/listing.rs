//! Folder listings, a page at a time.
//!
//! A folder is read once; the website then pages through that reading,
//! sorted and filtered as it asks, for [`CACHE_LIFETIME`], so a folder of
//! 50,000 entries costs one read and one sort per order rather than one per
//! page. Anything that changes files forgets the readings.
use std::cmp::Ordering;
use std::fs::{self, DirEntry, Metadata};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meshrmm_protocol::{
    FileEntry, FileError, FileErrorCode, FileKind, FileListing, FileReply, FileSort,
    MAX_FILE_FILTER_CHARS, MAX_FILE_LIST_PAGE,
};

use super::{Account, io_error, now_ms, paths, platform, unix_ms};

/// How long a reading serves pages.
const CACHE_LIFETIME: Duration = Duration::from_secs(60);
/// Readings kept at once, and orders kept per reading.
const CACHED_READINGS: usize = 4;
const CACHED_ORDERS: usize = 4;
/// A folder's entries past this many aren't listed.
pub(super) const MAX_ENTRIES: usize = 250_000;

/// What part of a listing to send, and how.
pub(super) struct Page {
    pub offset: u32,
    pub limit: u32,
    pub sort: FileSort,
    pub descending: bool,
    pub hidden: bool,
    pub filter: Option<String>,
    pub refresh: bool,
}

/// A folder as it was read.
struct Reading {
    entries: Vec<FileEntry>,
    /// Each entry's name in lowercase, for sorting and filtering.
    folded: Vec<String>,
    truncated: bool,
    read_at: i64,
}

#[derive(Clone, PartialEq, Eq)]
struct OrderKey {
    sort: FileSort,
    descending: bool,
    hidden: bool,
    filter: String,
}

struct Cached {
    account: String,
    path: PathBuf,
    read: Instant,
    reading: Arc<Reading>,
    /// Entry indexes in each order asked for.
    orders: Vec<(OrderKey, Arc<Vec<u32>>)>,
}

/// Recent readings, newest last.
#[derive(Default)]
pub(super) struct Cache {
    readings: Mutex<Vec<Cached>>,
}

impl Cache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Cached>> {
        self.readings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forgets every reading, after something changed files.
    pub(super) fn clear(&self) {
        self.lock().clear();
    }

    fn reading(&self, account: &str, path: &Path) -> Option<Arc<Reading>> {
        let mut readings = self.lock();
        readings.retain(|cached| cached.read.elapsed() < CACHE_LIFETIME);
        readings
            .iter()
            .find(|cached| cached.account == account && cached.path == path)
            .map(|cached| Arc::clone(&cached.reading))
    }

    fn keep(&self, account: &str, path: &Path, reading: Arc<Reading>) {
        let mut readings = self.lock();
        readings.retain(|cached| !(cached.account == account && cached.path == path));
        if readings.len() >= CACHED_READINGS {
            readings.remove(0);
        }
        readings.push(Cached {
            account: account.to_owned(),
            path: path.to_path_buf(),
            read: Instant::now(),
            reading,
            orders: Vec::new(),
        });
    }

    /// `reading` in `key`'s order, sorted once.
    fn order(
        &self,
        account: &str,
        path: &Path,
        reading: &Reading,
        key: &OrderKey,
    ) -> Arc<Vec<u32>> {
        let found = self
            .lock()
            .iter()
            .find(|cached| cached.account == account && cached.path == path)
            .and_then(|cached| {
                cached
                    .orders
                    .iter()
                    .find(|(known, _)| known == key)
                    .map(|(_, order)| Arc::clone(order))
            });
        if let Some(order) = found {
            return order;
        }
        let order = Arc::new(sorted(reading, key));
        if let Some(cached) = self
            .lock()
            .iter_mut()
            .find(|cached| cached.account == account && cached.path == path)
        {
            if cached.orders.len() >= CACHED_ORDERS {
                cached.orders.remove(0);
            }
            cached.orders.push((key.clone(), Arc::clone(&order)));
        }
        order
    }
}

pub(super) fn list(
    account: &Account,
    cache: &Cache,
    path: &str,
    page: Page,
) -> Result<FileReply, FileError> {
    let path = paths::parse(path)?;
    if page.limit == 0 || page.limit > MAX_FILE_LIST_PAGE {
        return Err(FileError::new(
            FileErrorCode::Failed,
            format!("A page has 1 to {MAX_FILE_LIST_PAGE} entries."),
        ));
    }
    let filter = page.filter.unwrap_or_default().trim().to_lowercase();
    if filter.chars().count() > MAX_FILE_FILTER_CHARS {
        return Err(FileError::new(
            FileErrorCode::Failed,
            "The filter is too long.",
        ));
    }
    let cached = (!page.refresh)
        .then(|| cache.reading(&account.label, &path))
        .flatten();
    let reading = match cached {
        Some(reading) => reading,
        None => {
            let reading = Arc::new(account.run(|| read(&path))?);
            cache.keep(&account.label, &path, Arc::clone(&reading));
            reading
        }
    };
    let key = OrderKey {
        sort: page.sort,
        descending: page.descending,
        hidden: page.hidden,
        filter,
    };
    let order = cache.order(&account.label, &path, &reading, &key);
    let start = (page.offset as usize).min(order.len());
    let end = (start + page.limit as usize).min(order.len());
    Ok(FileReply::Listing(FileListing {
        parent: paths::parent(&path).map(paths::text),
        path: paths::text(&path),
        account: account.label.clone(),
        offset: start as u32,
        total: order.len() as u32,
        entries: order[start..end]
            .iter()
            .map(|&index| reading.entries[index as usize].clone())
            .collect(),
        read_at: reading.read_at,
        truncated: reading.truncated,
    }))
}

/// Reads `path`'s entries. Entries that vanish while it's read are left
/// out; a folder that can't be read is an error.
fn read(path: &Path) -> Result<Reading, FileError> {
    let metadata = fs::metadata(path).map_err(|error| io_error(&error, path))?;
    if !metadata.is_dir() {
        return Err(FileError::new(
            FileErrorCode::NotADirectory,
            format!("{} is a file, not a folder.", path.display()),
        ));
    }
    let listing = fs::read_dir(path).map_err(|error| io_error(&error, path))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in listing {
        let Ok(item) = item else { continue };
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        entries.push(entry_of(&item));
    }
    let folded = entries
        .iter()
        .map(|entry| entry.name.to_lowercase())
        .collect();
    Ok(Reading {
        entries,
        folded,
        truncated,
        read_at: now_ms(),
    })
}

fn kind_of(metadata: &Metadata) -> FileKind {
    if metadata.is_dir() {
        FileKind::Directory
    } else if metadata.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

/// An entry from what `metadata` (not following a link) says about it. A
/// link is described by its target, when there is one.
fn describe(name: String, path: &Path, metadata: &Metadata) -> FileEntry {
    let link = metadata.file_type().is_symlink();
    let target = link.then(|| fs::metadata(path).ok()).flatten();
    let shown = target.as_ref().unwrap_or(metadata);
    let kind = if link && target.is_none() {
        FileKind::Other
    } else {
        kind_of(shown)
    };
    FileEntry {
        hidden: platform::hidden(&name, metadata),
        system: platform::system(metadata),
        read_only: platform::read_only(metadata),
        size: (kind == FileKind::File).then_some(shown.len()),
        modified: shown.modified().ok().and_then(unix_ms),
        name,
        kind,
        link,
    }
}

fn entry_of(item: &DirEntry) -> FileEntry {
    let name = item.file_name().to_string_lossy().into_owned();
    match item.metadata() {
        Ok(metadata) => describe(name, &item.path(), &metadata),
        Err(_) => FileEntry {
            name,
            kind: FileKind::Other,
            link: false,
            size: None,
            modified: None,
            hidden: false,
            system: false,
            read_only: false,
        },
    }
}

/// The entry at `path`.
pub(super) fn entry_at(path: &Path) -> Result<FileEntry, FileError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(&error, path))?;
    Ok(describe(paths::name(path), path, &metadata))
}

/// The indexes of `reading`'s entries that `key` shows, in its order.
/// Folders come first either way.
fn sorted(reading: &Reading, key: &OrderKey) -> Vec<u32> {
    let mut order = (0..reading.entries.len() as u32)
        .filter(|&index| {
            let entry = &reading.entries[index as usize];
            (key.hidden || !(entry.hidden || entry.system))
                && (key.filter.is_empty() || reading.folded[index as usize].contains(&key.filter))
        })
        .collect::<Vec<_>>();
    let name = |index: u32| reading.folded[index as usize].as_str();
    let entry = |index: u32| &reading.entries[index as usize];
    order.sort_by(|&a, &b| {
        let folders =
            (entry(b).kind == FileKind::Directory).cmp(&(entry(a).kind == FileKind::Directory));
        if folders != Ordering::Equal {
            return folders;
        }
        let by_name = || natural(name(a), name(b)).then_with(|| entry(a).name.cmp(&entry(b).name));
        let order = match key.sort {
            FileSort::Name => by_name(),
            FileSort::Size => entry(a).size.cmp(&entry(b).size).then_with(by_name),
            FileSort::Modified => entry(a).modified.cmp(&entry(b).modified).then_with(by_name),
            FileSort::Type => extension(name(a))
                .cmp(extension(name(b)))
                .then_with(by_name),
        };
        if key.descending {
            order.reverse()
        } else {
            order
        }
    });
    order
}

/// A name's extension, empty for none or a leading dot alone.
fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => "",
    }
}

/// Compares names as people read them: runs of digits by their value, so
/// `file2` comes before `file10`.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let digits = |text: &[u8]| text.iter().take_while(|c| c.is_ascii_digit()).count();
                let (length_a, length_b) = (digits(a), digits(b));
                let trim = |run: &[u8]| {
                    let zeros = run.iter().take_while(|&&c| c == b'0').count();
                    run[zeros..].to_vec()
                };
                let (value_a, value_b) = (trim(&a[..length_a]), trim(&b[..length_b]));
                let order = value_a
                    .len()
                    .cmp(&value_b.len())
                    .then_with(|| value_a.cmp(&value_b))
                    .then_with(|| length_a.cmp(&length_b));
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[length_a..];
                b = &b[length_b..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    // Bytes of UTF-8 compare in code point order.
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::ExecutionMode;

    struct Folder(PathBuf);

    impl Folder {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("meshrmm-files-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn own() -> Account {
        Account::resolve(ExecutionMode::Console, meshrmm_protocol::RunAs::System).unwrap()
    }

    fn page(offset: u32, limit: u32, sort: FileSort, descending: bool) -> Page {
        Page {
            offset,
            limit,
            sort,
            descending,
            hidden: false,
            filter: None,
            refresh: false,
        }
    }

    fn names(reply: FileReply) -> (Vec<String>, u32) {
        let FileReply::Listing(listing) = reply else {
            panic!("{reply:?}")
        };
        (
            listing
                .entries
                .into_iter()
                .map(|entry| entry.name)
                .collect(),
            listing.total,
        )
    }

    #[test]
    fn names_sort_naturally() {
        let mut names = vec!["file10", "file2", "File1", "file02", "a", "b1c"];
        names.sort_by(|a, b| natural(&a.to_lowercase(), &b.to_lowercase()));
        assert_eq!(names, ["a", "b1c", "File1", "file2", "file02", "file10"]);
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension(".profile"), "");
    }

    #[test]
    fn folders_list_sorted_paged_and_filtered() {
        let folder = Folder::new();
        for (name, size) in [("b.txt", 30), ("a10.log", 10), ("a2.log", 20)] {
            fs::write(folder.0.join(name), vec![0; size]).unwrap();
        }
        fs::create_dir(folder.0.join("zeta")).unwrap();
        fs::write(folder.0.join(".hidden"), b"x").unwrap();
        let cache = Cache::default();
        let account = own();
        let path = paths::text(&folder.0);
        let (first, total) =
            names(list(&account, &cache, &path, page(0, 2, FileSort::Name, false)).unwrap());
        assert_eq!(total, 4, "hidden entries are left out");
        assert_eq!(first, ["zeta", "a2.log"]);
        let (rest, _) =
            names(list(&account, &cache, &path, page(2, 10, FileSort::Name, false)).unwrap());
        assert_eq!(rest, ["a10.log", "b.txt"]);
        let (by_size, _) =
            names(list(&account, &cache, &path, page(0, 10, FileSort::Size, true)).unwrap());
        assert_eq!(by_size, ["zeta", "b.txt", "a2.log", "a10.log"]);
        let (by_type, _) =
            names(list(&account, &cache, &path, page(0, 10, FileSort::Type, false)).unwrap());
        assert_eq!(by_type, ["zeta", "a2.log", "a10.log", "b.txt"]);
        let mut filtered = page(0, 10, FileSort::Name, false);
        filtered.filter = Some("LOG".into());
        assert_eq!(
            names(list(&account, &cache, &path, filtered).unwrap()).0,
            ["a2.log", "a10.log"]
        );
        let mut hidden = page(0, 10, FileSort::Name, false);
        hidden.hidden = true;
        assert_eq!(names(list(&account, &cache, &path, hidden).unwrap()).1, 5);

        // A new file shows once the folder is read again.
        fs::write(folder.0.join("c.txt"), b"").unwrap();
        assert_eq!(
            names(list(&account, &cache, &path, page(0, 10, FileSort::Name, false)).unwrap()).1,
            4
        );
        let mut fresh = page(0, 10, FileSort::Name, false);
        fresh.refresh = true;
        assert_eq!(names(list(&account, &cache, &path, fresh).unwrap()).1, 5);
    }

    #[test]
    fn listing_a_file_or_nothing_says_why() {
        let folder = Folder::new();
        fs::write(folder.0.join("file"), b"x").unwrap();
        let cache = Cache::default();
        let error = |path: PathBuf| match list(
            &own(),
            &cache,
            &paths::text(&path),
            page(0, 10, FileSort::Name, false),
        ) {
            Err(error) => error.code,
            Ok(reply) => panic!("{reply:?}"),
        };
        assert_eq!(error(folder.0.join("file")), FileErrorCode::NotADirectory);
        assert_eq!(error(folder.0.join("missing")), FileErrorCode::NotFound);
    }

    #[test]
    fn large_folders_page_quickly() {
        let folder = Folder::new();
        for index in 0..5000 {
            fs::write(folder.0.join(format!("entry {index}.txt")), b"").unwrap();
        }
        let cache = Cache::default();
        let account = own();
        let path = paths::text(&folder.0);
        let started = Instant::now();
        let mut seen = 0;
        let mut offset = 0;
        loop {
            let (entries, total) = names(
                list(
                    &account,
                    &cache,
                    &path,
                    page(offset, MAX_FILE_LIST_PAGE, FileSort::Name, false),
                )
                .unwrap(),
            );
            assert_eq!(total, 5000);
            if entries.is_empty() {
                break;
            }
            if offset == 0 {
                assert_eq!(entries[..3], ["entry 0.txt", "entry 1.txt", "entry 2.txt"]);
            }
            seen += entries.len();
            offset += entries.len() as u32;
        }
        assert_eq!(seen, 5000);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_described_by_their_targets() {
        let folder = Folder::new();
        fs::create_dir(folder.0.join("real")).unwrap();
        std::os::unix::fs::symlink(folder.0.join("real"), folder.0.join("link")).unwrap();
        std::os::unix::fs::symlink(folder.0.join("gone"), folder.0.join("broken")).unwrap();
        let link = entry_at(&folder.0.join("link")).unwrap();
        assert!(link.link);
        assert_eq!(link.kind, FileKind::Directory);
        let broken = entry_at(&folder.0.join("broken")).unwrap();
        assert_eq!(broken.kind, FileKind::Other);
    }
}
