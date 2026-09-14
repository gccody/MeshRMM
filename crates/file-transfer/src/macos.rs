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
