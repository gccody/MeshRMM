//! CF_HDROP permits copying between background Explorer windows and desktop apps.
use super::*;
use windows::Win32::System::{DataExchange::*, Memory::*};
pub(super) fn write(paths: &[PathBuf], cut: bool) -> anyhow::Result<()> {
    meshrmm_file_transfer::windows::set_clipboard_files(paths)?;
    unsafe {
        let format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        ensure!(format != 0, "Could not register the file clipboard format");
        let memory = GlobalAlloc(GMEM_MOVEABLE, 4)?;
        let pointer = GlobalLock(memory) as *mut u32;
        if pointer.is_null() {
            let _ = GlobalFree(Some(memory));
            anyhow::bail!("Could not allocate file clipboard data");
        }
        pointer.write(if cut { 2 } else { 1 });
        let _ = GlobalUnlock(memory);
        if let Err(error) = OpenClipboard(None) {
            let _ = GlobalFree(Some(memory));
            return Err(error.into());
        }
        let result = SetClipboardData(format, Some(HANDLE(memory.0)));
        let _ = CloseClipboard();
        if let Err(error) = result {
            let _ = GlobalFree(Some(memory));
            return Err(error.into());
        }
    }
    Ok(())
}
pub(super) fn read() -> anyhow::Result<(Vec<PathBuf>, bool)> {
    let paths = meshrmm_file_transfer::windows::clipboard_files()?;
    let mut cut = false;
    unsafe {
        OpenClipboard(None)?;
        let format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        if let Ok(data) = GetClipboardData(format) {
            let memory = HGLOBAL(data.0);
            if GlobalSize(memory) >= 4 {
                let pointer = GlobalLock(memory) as *const u32;
                if !pointer.is_null() {
                    cut = pointer.read() & 2 != 0;
                    let _ = GlobalUnlock(memory);
                }
            }
        }
        let _ = CloseClipboard();
    }
    for path in &paths {
        location(&path.to_string_lossy())?;
    }
    Ok((paths, cut))
}
