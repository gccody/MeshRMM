//! File operations that run off the window thread.
use super::*;

pub(super) enum Work {
    List(PathBuf),
    Preview(PathBuf),
    Open(PathBuf),
    NewFolder(PathBuf, PathBuf),
    NewFile(PathBuf, PathBuf),
    Rename(PathBuf, PathBuf, PathBuf),
    #[cfg(test)]
    Copy(PathBuf, PathBuf, PathBuf),
    Transfer(PathBuf, PathBuf, Vec<PathBuf>, bool, bool),
    Delete(PathBuf, Vec<PathBuf>),
    Search(PathBuf, String),
    Undo(PathBuf, Vec<UndoAction>),
}
pub(super) enum ResultData {
    Opened(PathBuf),
    List {
        path: PathBuf,
        rows: Vec<Entry>,
        status: String,
        rename: Option<PathBuf>,
        search: bool,
        clipboard: Option<Vec<PathBuf>>,
        undo: Vec<UndoAction>,
    },
    Preview {
        path: PathBuf,
        text: String,
    },
}
impl Work {
    #[cfg(test)]
    pub(super) fn execute(self) -> anyhow::Result<ResultData> {
        self.execute_cancellable(&AtomicBool::new(false))
    }
    pub(super) fn execute_cancellable(self, cancelled: &AtomicBool) -> anyhow::Result<ResultData> {
        ensure!(!cancelled.load(Ordering::Relaxed), "Operation cancelled.");
        let mut edit = None;
        let mut clipboard = None;
        let mut undo_actions = Vec::new();
        let (path, status) = match self {
            Self::List(path) => (path, String::new()),
            Self::Undo(mut path, actions) => {
                let status = match undo(&actions) {
                    Ok(()) => "Operation undone. ".into(),
                    Err(error) => format!("Undo stopped: {error:#}"),
                };
                while !virtual_location(&path) && !path.is_dir() {
                    path = path.parent().unwrap_or(Path::new("")).to_path_buf();
                }
                (path, status)
            }
            Self::Open(path) => {
                launch::open(&path)?;
                return Ok(ResultData::Opened(path));
            }
            Self::Preview(path) => {
                return Ok(ResultData::Preview {
                    text: preview(&path)?,
                    path,
                });
            }
            Self::Search(path, query) => return search(path, &query, cancelled),
            Self::NewFolder(path, target) => {
                std::fs::create_dir(&target)?;
                if let Ok(action) = UndoAction::created(&target) {
                    undo_actions.push(action);
                }
                edit = Some(target);
                (path, String::new())
            }
            Self::NewFile(path, target) => {
                std::fs::File::create_new(&target)?;
                if let Ok(action) = UndoAction::created(&target) {
                    undo_actions.push(action);
                }
                edit = Some(target);
                (path, String::new())
            }
            Self::Rename(path, source, target) => {
                rename(&source, &target)?;
                if let Ok(action) = UndoAction::moved(&source, &target) {
                    undo_actions.push(action);
                }
                (path, "Renamed. ".into())
            }
            #[cfg(test)]
            Self::Copy(path, source, target) => {
                copy_file(&source, &target)?;
                (path, "Copied. ".into())
            }
            Self::Transfer(path, destination, sources, cut, update_clipboard) => {
                let (status, remaining) =
                    transfer(&destination, sources, cut, cancelled, &mut undo_actions)?;
                if cut && update_clipboard {
                    clipboard = Some(remaining);
                }
                (path, status)
            }
            Self::Delete(path, sources) => (path, delete(sources, cancelled)),
        };
        Ok(ResultData::List {
            rows: entries(&path)?,
            path,
            status,
            rename: edit,
            search: false,
            clipboard,
            undo: undo_actions,
        })
    }
}

fn search(path: PathBuf, query: &str, cancelled: &AtomicBool) -> anyhow::Result<ResultData> {
    let mut rows = Vec::new();
    let mut pending = vec![path.clone()];
    let query = query.to_lowercase();
    let mut skipped = 0;
    while let Some(folder) = pending.pop() {
        ensure!(!cancelled.load(Ordering::Relaxed), "Search cancelled.");
        match entries(&folder) {
            Ok(children) => {
                for entry in children {
                    use std::os::windows::fs::MetadataExt;
                    if entry.directory
                        && std::fs::symlink_metadata(&entry.path).is_ok_and(|m| {
                            m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
                        })
                    {
                        pending.push(entry.path.clone());
                    }
                    if search_matches(&entry.name, &query) {
                        rows.push(entry);
                    }
                    ensure!(
                        rows.len() < MAX_ENTRIES,
                        "Search exceeds {MAX_ENTRIES} results. Use a more specific search."
                    );
                }
            }
            Err(_) => skipped += 1,
        }
    }
    Ok(ResultData::List {
        path,
        rows,
        status: if skipped > 0 {
            format!("{skipped} inaccessible folders skipped. ")
        } else {
            String::new()
        },
        rename: None,
        search: true,
        clipboard: None,
        undo: Vec::new(),
    })
}

/// Copies or moves `sources` into `destination`, returning the status text and the
/// sources that were not moved.
fn transfer(
    destination: &Path,
    sources: Vec<PathBuf>,
    cut: bool,
    cancelled: &AtomicBool,
    undo_actions: &mut Vec<UndoAction>,
) -> anyhow::Result<(String, Vec<PathBuf>)> {
    let mut completed = 0;
    let mut remaining = sources.clone();
    let mut errors = Vec::new();
    for source in sources {
        if cancelled.load(Ordering::Relaxed) {
            errors.push("Operation cancelled; completed items were kept.".into());
            break;
        }
        let target = if source.parent() == Some(destination) && !cut {
            unique_target(destination, &source)
        } else {
            destination.join(source.file_name().context("Invalid source")?)
        };
        let result = if cut {
            rename(&source, &target)
        } else {
            copy_tree_cancellable(&source, &target, cancelled)
        };
        match result {
            Ok(()) => {
                completed += 1;
                match if cut {
                    UndoAction::moved(&source, &target)
                } else {
                    UndoAction::created(&target)
                } {
                    Ok(action) => undo_actions.push(action),
                    Err(error) => errors.push(format!(
                        "Item completed, but Undo is unavailable: {error:#}"
                    )),
                }
                remaining.retain(|path| path != &source);
            }
            Err(e) => errors.push(format!("{}: {e:#}", source.display())),
        }
    }
    let status = format!(
        "{completed} item(s) {}. {}",
        if cut { "moved" } else { "copied" },
        errors.join("; ")
    );
    Ok((status, remaining))
}

fn delete(sources: Vec<PathBuf>, cancelled: &AtomicBool) -> String {
    let mut errors = Vec::new();
    for source in sources {
        if cancelled.load(Ordering::Relaxed) {
            errors.push("Operation cancelled; completed items were kept.".into());
            break;
        }
        if let Err(e) = delete_tree(&source) {
            errors.push(format!("{}: {e:#}", source.display()));
        }
    }
    errors.join("; ")
}
