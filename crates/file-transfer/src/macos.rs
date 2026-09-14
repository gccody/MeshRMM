use anyhow::{Context, ensure};
use objc2::{MainThreadMarker, runtime::ProtocolObject};
use objc2_app_kit::{NSOpenPanel, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSString, NSURL};
use std::path::PathBuf;
pub struct NativeGuard;
pub fn initialize() -> anyhow::Result<NativeGuard> {
    Ok(NativeGuard)
}
fn main_thread<T: Send + 'static>(f: impl FnOnce(MainThreadMarker) -> T + Send + 'static) -> T {
    if let Some(mtm) = MainThreadMarker::new() {
        return f(mtm);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    dispatch2::DispatchQueue::main().exec_async(move || {
        let _ = tx.send(f(MainThreadMarker::new().unwrap()));
    });
    rx.recv().unwrap()
}
pub fn documents() -> anyhow::Result<PathBuf> {
    main_thread(|_| {
        let manager = objc2_foundation::NSFileManager::defaultManager();
        let urls = manager.URLsForDirectory_inDomains(
            objc2_foundation::NSSearchPathDirectory::DocumentDirectory,
            objc2_foundation::NSSearchPathDomainMask::UserDomainMask,
        );
        Ok(PathBuf::from(
            urls.firstObject()
                .and_then(|u| u.path())
                .context("Documents folder unavailable")?
                .to_string(),
        ))
    })
}
pub fn pick() -> anyhow::Result<Vec<PathBuf>> {
    main_thread(|mtm| {
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(true);
        panel.setAllowsMultipleSelection(true);
        panel.setTitle(Some(&NSString::from_str("Send files and folders")));
        if panel.runModal() != 1 {
            return Ok(Vec::new());
        }
        Ok(panel
            .URLs()
            .iter()
            .filter_map(|u| u.path().map(|s| PathBuf::from(s.to_string())))
            .collect())
    })
}
pub fn paths_from_pasteboard(board: &NSPasteboard) -> Vec<PathBuf> {
    board
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let value = item.stringForType(unsafe { NSPasteboardTypeFileURL })?;
                    let url = NSURL::URLWithString(&value)?;
                    if !url.isFileURL() {
                        return None;
                    }
                    url.path().map(|p| PathBuf::from(p.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}
pub fn clipboard_sequence() -> u64 {
    main_thread(|_| NSPasteboard::generalPasteboard().changeCount() as u64)
}

pub fn clipboard_files() -> anyhow::Result<Vec<PathBuf>> {
    Ok(main_thread(|_| {
        paths_from_pasteboard(&NSPasteboard::generalPasteboard())
    }))
}
pub fn set_clipboard_files(paths: &[PathBuf]) -> anyhow::Result<()> {
    let paths = paths.to_vec();
    main_thread(move |_| {
        let urls: Vec<_> = paths
            .iter()
            .map(|p| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy())))
            .collect();
        let writers: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = urls
            .iter()
            .map(|u| ProtocolObject::from_ref(&**u))
            .collect();
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        ensure!(
            board.writeObjects(&NSArray::from_slice(&writers)),
            "could not publish files to clipboard"
        );
        Ok(())
    })
}
pub fn drop_files(
    _: &[PathBuf],
    _: meshrmm_protocol::DisplayId,
    _: u16,
    _: u16,
) -> anyhow::Result<bool> {
    Ok(false)
}

pub fn paste_files(_: meshrmm_protocol::DisplayId) -> anyhow::Result<()> {
    anyhow::bail!("Remote paste is supported on Windows agents")
}

pub fn cache() -> anyhow::Result<PathBuf> {
    main_thread(|_| {
        let urls = objc2_foundation::NSFileManager::defaultManager().URLsForDirectory_inDomains(
            objc2_foundation::NSSearchPathDirectory::CachesDirectory,
            objc2_foundation::NSSearchPathDomainMask::UserDomainMask,
        );
        Ok(PathBuf::from(
            urls.firstObject()
                .and_then(|u| u.path())
                .context("Caches folder unavailable")?
                .to_string(),
        )
        .join("MeshRMM"))
    })
}

struct ProgressWindow {
    window: objc2::rc::Retained<objc2_app_kit::NSWindow>,
    bar: objc2::rc::Retained<objc2_app_kit::NSProgressIndicator>,
    name: objc2::rc::Retained<objc2_app_kit::NSTextField>,
    detail: objc2::rc::Retained<objc2_app_kit::NSTextField>,
}
thread_local! {
    static PROGRESS: std::cell::RefCell<std::collections::HashMap<u64, ProgressWindow>> = std::cell::RefCell::new(std::collections::HashMap::new());
}
pub struct Progress(u64);
impl Progress {
    pub fn new(id: u64) -> anyhow::Result<Self> {
        main_thread(move |mtm| {
            use objc2::MainThreadOnly;
            use objc2_app_kit::*;
            use objc2_foundation::{NSPoint, NSRect, NSSize};
            let rect = |x, y, width, height| NSRect {
                origin: NSPoint { x, y },
                size: NSSize { width, height },
            };
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    rect(0., 0., 460., 135.),
                    NSWindowStyleMask::Titled,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe {
                window.setReleasedWhenClosed(false);
            }
            window.setTitle(&NSString::from_str("MeshRMM — Receiving files"));
            let view = window.contentView().unwrap();
            let name =
                NSTextField::labelWithString(&NSString::from_str("Preparing transfer…"), mtm);
            name.setFrame(rect(22., 88., 416., 24.));
            view.addSubview(&name);
            let bar = NSProgressIndicator::new(mtm);
            bar.setStyle(NSProgressIndicatorStyle::Bar);
            bar.setIndeterminate(false);
            bar.setMinValue(0.);
            bar.setMaxValue(100.);
            bar.setFrame(rect(22., 58., 416., 20.));
            view.addSubview(&bar);
            let detail =
                NSTextField::labelWithString(&NSString::from_str("Waiting for file details…"), mtm);
            detail.setFrame(rect(22., 22., 416., 24.));
            view.addSubview(&detail);
            window.center();
            window.orderFrontRegardless();
            PROGRESS.with(|windows| {
                windows.borrow_mut().insert(
                    id,
                    ProgressWindow {
                        window,
                        bar,
                        name,
                        detail,
                    },
                );
            });
        });
        Ok(Self(id))
    }
    pub fn update(&self, bytes: u64, total: u64, name: &str, entries: usize, total_entries: u64) {
        let id = self.0;
        let name = name.to_owned();
        let detail = crate::progress_detail(bytes, total, entries, total_entries);
        main_thread(move |_| {
            PROGRESS.with(|windows| {
                if let Some(ui) = windows.borrow().get(&id) {
                    let fraction = if total == 0 {
                        entries as f64 / total_entries.max(1) as f64
                    } else {
                        bytes as f64 / total as f64
                    };
                    ui.bar.setDoubleValue((fraction * 100.).clamp(0., 100.));
                    ui.name.setStringValue(&NSString::from_str(&name));
                    ui.detail.setStringValue(&NSString::from_str(&detail));
                }
            });
        });
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        let id = self.0;
        main_thread(move |_| {
            PROGRESS.with(|windows| {
                if let Some(ui) = windows.borrow_mut().remove(&id) {
                    ui.window.orderOut(None);
                }
            });
        });
    }
}
