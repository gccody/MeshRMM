//! Ribbon, address bar and menu commands of the file browser window.
use super::*;

impl State {
    pub(super) fn command(&mut self, command: usize) -> anyhow::Result<()> {
        if command == CANCEL {
            self.cancel();
            return Ok(());
        }
        ensure!(
            self.receiver.is_none(),
            "An operation is still running. Please wait."
        );
        if virtual_location(&self.path)
            && matches!(
                command,
                NEW_FOLDER | NEW_FILE | PASTE | SEARCH_GO | RENAME | DELETE | CUT
            )
        {
            anyhow::bail!("Open a drive or folder first.");
        }
        match command {
            UNDO => {
                let actions = self
                    .undo_stack
                    .pop()
                    .context("There is no file operation to undo.")?;
                self.start(Work::Undo(self.path.clone(), actions))
            }
            ADDRESS_EDIT => {
                self.edit_address();
                Ok(())
            }
            CRUMB..=599 => {
                let path = self
                    .crumbs
                    .get(command - CRUMB)
                    .context("Unknown path segment")?
                    .1
                    .clone();
                self.start(Work::List(path))
            }
            CLOSE => {
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                Ok(())
            }
            NEW_WINDOW => open_new_window(),
            GO => self.start(Work::List(location(&window_text(self.location))?)),
            UP => self.start(Work::List(
                self.path.parent().unwrap_or(Path::new("")).to_path_buf(),
            )),
            BACK | FORWARD => self.travel(command),
            REFRESH => {
                if self.searching {
                    self.command(SEARCH_GO)
                } else {
                    self.start(Work::List(self.path.clone()))
                }
            }
            SEARCH_GO => {
                let query = window_text(self.search);
                if query.trim().is_empty() {
                    self.start(Work::List(self.path.clone()))
                } else {
                    self.start(Work::Search(self.path.clone(), query))
                }
            }
            OPEN | PREVIEW => {
                let entry = self.selected()?;
                self.start(if entry.directory {
                    Work::List(entry.path)
                } else if command == PREVIEW {
                    Work::Preview(entry.path)
                } else {
                    Work::Open(entry.path)
                })
            }
            NEW_FOLDER | NEW_FILE => self.create_item(command == NEW_FOLDER),
            RENAME => self.begin_rename(),
            COPY | CUT => self.copy_selection(command == CUT),
            PASTE => self.paste(),
            DELETE => self.request_delete(),
            CONFIRM => {
                ensure!(!self.pending_delete.is_empty(), "No deletion is pending.");
                let paths = std::mem::take(&mut self.pending_delete);
                self.start(Work::Delete(self.path.clone(), paths))
            }
            PROPERTIES => self.show_properties(),
            COPY_PATH => self.copy_paths(),
            SELECT_ALL | SELECT_NONE | INVERT => {
                self.select(command == SELECT_ALL, command == INVERT);
                Ok(())
            }
            HOME | VIEW => {
                self.view_tab = command == VIEW;
                self.layout();
                Ok(())
            }
            DETAILS | SMALL | LARGE => {
                self.set_view(command);
                Ok(())
            }
            HIDDEN | EXTENSIONS => {
                if command == HIDDEN {
                    self.hidden = !self.hidden;
                } else {
                    self.extensions = !self.extensions;
                }
                self.render(None, None);
                Ok(())
            }
            SORT_NAME..=SORT_SIZE => {
                self.sort_by(command - SORT_NAME);
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn cancel(&mut self) {
        if self.receiver.is_some() {
            self.cancelled.store(true, Ordering::Relaxed);
            set_text(self.status, "Cancelling…");
            return;
        }
        self.address_edit = false;
        self.dragging.clear();
        self.pending_delete.clear();
        self.layout();
        self.selection_status();
    }
    fn edit_address(&mut self) {
        self.address_edit = true;
        self.layout();
        unsafe {
            let _ = SetFocus(Some(self.location));
            SendMessageW(self.location, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
    }
    fn travel(&mut self, command: usize) -> anyhow::Result<()> {
        let stack = if command == BACK {
            &self.history.back
        } else {
            &self.history.forward
        };
        let path = stack.last().context("No more history.")?.clone();
        self.travel = Some(command);
        self.start(Work::List(path))
    }
    fn create_item(&mut self, folder: bool) -> anyhow::Result<()> {
        let name = if folder {
            "New folder"
        } else {
            "New Text Document.txt"
        };
        let mut target = self.path.join(name);
        for number in 2.. {
            if !target.exists() {
                break;
            }
            target = self.path.join(if folder {
                format!("New folder ({number})")
            } else {
                format!("New Text Document ({number}).txt")
            });
        }
        self.start(if folder {
            Work::NewFolder(self.path.clone(), target)
        } else {
            Work::NewFile(self.path.clone(), target)
        })
    }
    fn begin_rename(&self) -> anyhow::Result<()> {
        ensure!(self.selection().len() == 1, "Select one item to rename.");
        unsafe {
            let index = SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            );
            let _ = SetFocus(Some(self.list));
            SendMessageW(
                self.list,
                LVM_EDITLABELW,
                Some(WPARAM(index.0 as usize)),
                None,
            );
        }
        Ok(())
    }
    fn copy_selection(&mut self, cut: bool) -> anyhow::Result<()> {
        let rows = self.selection();
        ensure!(!rows.is_empty(), "Select files or folders first.");
        self.copied = rows.into_iter().map(|e| e.path).collect();
        self.cut = cut;
        clipboard::write(&self.copied, self.cut)?;
        self.clipboard_sequence = meshrmm_file_transfer::clipboard_sequence();
        self.render(None, None);
        set_text(
            self.status,
            &format!(
                "{} item(s) ready to {}. Choose a destination and Paste.",
                self.copied.len(),
                if self.cut { "move" } else { "copy" }
            ),
        );
        Ok(())
    }
    fn paste(&mut self) -> anyhow::Result<()> {
        (self.copied, self.cut) = clipboard::read()?;
        self.clipboard_sequence = meshrmm_file_transfer::clipboard_sequence();
        ensure!(
            !self.copied.is_empty(),
            "Copy or cut files or folders first."
        );
        self.start(Work::Transfer(
            self.path.clone(),
            self.path.clone(),
            self.copied.clone(),
            self.cut,
            true,
        ))
    }
    fn request_delete(&mut self) -> anyhow::Result<()> {
        self.pending_delete = self.selection().into_iter().map(|e| e.path).collect();
        ensure!(
            !self.pending_delete.is_empty(),
            "Select files or folders first."
        );
        self.layout();
        set_text(
            self.status,
            &format!(
                "Permanently delete {} item(s)? They will not go to the Recycle Bin. {}",
                self.pending_delete.len(),
                self.pending_delete
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        Ok(())
    }
    fn show_properties(&self) -> anyhow::Result<()> {
        let rows = self.selection();
        ensure!(!rows.is_empty(), "Select an item first.");
        let text = rows.iter().map(|e| format!("Name: {}\r\nType: {}\r\nLocation: {}\r\nSize: {}\r\nDate modified: {}\r\nHidden: {}\r\n", e.name, e.kind, e.path.display(), e.size.map_or_else(|| "Folder".into(), size_text), modified_text(e.modified), e.hidden)).collect::<Vec<_>>().join("\r\n");
        show_preview(Path::new("Properties"), &text)
    }
    fn copy_paths(&self) -> anyhow::Result<()> {
        let text = self
            .selection()
            .iter()
            .map(|e| format!("\"{}\"", e.path.display()))
            .collect::<Vec<_>>()
            .join("\r\n");
        ensure!(!text.is_empty(), "Select an item first.");
        meshrmm_clipboard::ClipboardSync::new(false)?
            .apply(meshrmm_protocol::ClipboardContent::Text(text))?;
        set_text(self.status, "Paths copied to clipboard.");
        Ok(())
    }
    fn set_view(&self, command: usize) {
        unsafe {
            SendMessageW(
                self.list,
                LVM_SETVIEW,
                Some(WPARAM(match command {
                    LARGE => LV_VIEW_ICON,
                    SMALL => LV_VIEW_SMALLICON,
                    _ => LV_VIEW_DETAILS,
                } as usize)),
                None,
            );
        }
    }
    /// Sorts by `column`, reversing the order when it is already the sort column.
    fn sort_by(&mut self, column: usize) {
        self.descending = self.sort == column && !self.descending;
        self.sort = column;
        self.render(None, None);
    }
}

fn open_new_window() -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .arg("--background-file-browser")
        .creation_flags(0x08000000)
        .spawn()?;
    Ok(())
}
