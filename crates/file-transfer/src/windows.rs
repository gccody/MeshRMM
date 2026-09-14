use anyhow::Context;
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{
            Com::*, DataExchange::*, Ole::*, RemoteDesktop::*, SystemServices::MODIFIERKEYS_FLAGS,
        },
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{BOOL, HRESULT, Interface, Ref, implement},
};
thread_local! { static USER_TOKEN: std::cell::Cell<HANDLE> = const { std::cell::Cell::new(HANDLE(std::ptr::null_mut())) }; }
static DISPLAYS: OnceLock<Mutex<Vec<meshrmm_protocol::Display>>> = OnceLock::new();
pub fn set_displays(displays: Vec<meshrmm_protocol::Display>) {
    *DISPLAYS.get_or_init(Mutex::default).lock().unwrap() = displays;
}
pub struct NativeGuard {
    token: HANDLE,
}
impl Drop for NativeGuard {
    fn drop(&mut self) {
        unsafe {
            OleUninitialize();
            if !self.token.is_invalid() {
                let _ = windows::Win32::Security::RevertToSelf();
                let _ = CloseHandle(self.token);
            }
        }
    }
}
pub fn initialize() -> anyhow::Result<NativeGuard> {
    unsafe {
        let mut token = HANDLE::default();
        if WTSQueryUserToken(WTSGetActiveConsoleSessionId(), &mut token).is_ok()
            && let Err(e) = windows::Win32::Security::ImpersonateLoggedOnUser(token)
        {
            let _ = CloseHandle(token);
            return Err(e.into());
        }
        USER_TOKEN.set(token);
        tracing::debug!(
            impersonating = !token.is_invalid(),
            "initializing native file operations"
        );
        OleInitialize(None)?;
        Ok(NativeGuard { token })
    }
}
fn wide(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}
pub fn documents() -> anyhow::Result<PathBuf> {
    unsafe {
        let token = USER_TOKEN.get();
        let result = SHGetKnownFolderPath(
            &FOLDERID_Documents,
            KF_FLAG_DEFAULT,
            (!token.is_invalid()).then_some(token),
        );
        let text = result?;
        let path = PathBuf::from(text.to_string()?);
        CoTaskMemFree(Some(text.0.cast()));
        Ok(path)
    }
}
pub fn pick() -> anyhow::Result<Vec<PathBuf>> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(
            FOS_ALLOWMULTISELECT | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_FILEMUSTEXIST,
        )?;
        dialog.SetTitle(windows::core::w!("Send files and folders"))?;
        let selected = std::sync::Arc::new(Mutex::new(Vec::new()));
        let events: IFileDialogEvents = PickerEvents {
            selected: selected.clone(),
        }
        .into();
        let customize: IFileDialogCustomize = dialog.cast()?;
        customize.AddPushButton(100, windows::core::w!("Send selected files / folders"))?;
        let cookie = dialog.Advise(&events)?;
        let result = dialog.Show(None);
        let _ = dialog.Unadvise(cookie);
        let chosen = selected.lock().unwrap().clone();
        if !chosen.is_empty() {
            return Ok(chosen);
        }
        if result.is_err() {
            return Ok(Vec::new());
        }
        let items = dialog.GetResults()?;
        let mut paths = Vec::new();
        for index in 0..items.GetCount()? {
            let text = items.GetItemAt(index)?.GetDisplayName(SIGDN_FILESYSPATH)?;
            paths.push(PathBuf::from(text.to_string()?));
            CoTaskMemFree(Some(text.0.cast()));
        }
        Ok(paths)
    }
}
pub fn clipboard_sequence() -> u64 {
    u64::from(unsafe { GetClipboardSequenceNumber() })
}

pub fn clipboard_files() -> anyhow::Result<Vec<PathBuf>> {
    unsafe {
        if IsClipboardFormatAvailable(15).is_err() {
            return Ok(Vec::new());
        }
        OpenClipboard(None)?;
        let result = GetClipboardData(15).map(|handle| paths_from_drop(HDROP(handle.0)));
        let _ = CloseClipboard();
        Ok(result?)
    }
}
/// # Safety
/// `drop` must be a live CF_HDROP handle retained by the caller for this call.
pub unsafe fn paths_from_drop(drop: HDROP) -> Vec<PathBuf> {
    unsafe {
        let count = DragQueryFileW(drop, u32::MAX, None).min(100_000);
        (0..count)
            .map(|index| {
                let length = DragQueryFileW(drop, index, None);
                let mut buffer = vec![0; length as usize + 1];
                DragQueryFileW(drop, index, Some(&mut buffer));
                PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]))
            })
            .collect()
    }
}
fn drop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: 15,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    }
}
#[implement(IDataObject)]
struct FileDataObject {
    paths: Vec<PathBuf>,
}
impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        tracing::trace!("native drop requested file data");
        self.QueryGetData(format).ok()?;
        let memory = allocate_drop(&self.paths)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: memory },
            ..Default::default()
        })
    }
    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        if format.is_null() {
            return E_POINTER;
        }
        let format = unsafe { &*format };
        tracing::trace!(
            format = format.cfFormat,
            aspect = format.dwAspect,
            tymed = format.tymed,
            "native drop queried format"
        );
        if format.cfFormat == 15
            && format.dwAspect == DVASPECT_CONTENT.0
            && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
        {
            HRESULT(0)
        } else {
            DV_E_FORMATETC
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            unsafe {
                (*out).ptd = std::ptr::null_mut();
            }
        }
        E_NOTIMPL
    }
    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&[drop_format()]) }
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}
fn data_object(paths: &[PathBuf]) -> anyhow::Result<IDataObject> {
    Ok(FileDataObject {
        paths: paths.to_vec(),
    }
    .into())
}
fn allocate_drop(paths: &[PathBuf]) -> windows::core::Result<HGLOBAL> {
    use windows::Win32::System::Memory::*;
    unsafe {
        let mut names = Vec::<u16>::new();
        for path in paths {
            names.extend(wide(path));
        }
        names.push(0);
        let bytes = std::mem::size_of::<DROPFILES>() + names.len() * 2;
        let memory = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes)?;
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(E_OUTOFMEMORY.into());
        }
        pointer.cast::<DROPFILES>().write(DROPFILES {
            pFiles: std::mem::size_of::<DROPFILES>() as u32,
            fWide: BOOL(1),
            ..Default::default()
        });
        std::ptr::copy_nonoverlapping(
            names.as_ptr(),
            pointer
                .cast::<u8>()
                .add(std::mem::size_of::<DROPFILES>())
                .cast::<u16>(),
            names.len(),
        );
        let _ = GlobalUnlock(memory);
        Ok(memory)
    }
}
pub fn set_clipboard_files(paths: &[PathBuf]) -> anyhow::Result<()> {
    unsafe {
        // Publish fully materialized CF_HDROP data. No delayed-rendering COM
        // callback or continued message pumping is needed by Explorer.
        let memory = allocate_drop(paths)?;
        let owner = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("STATIC"),
            windows::core::w!("MeshRMM file clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        );
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                let _ = GlobalFree(Some(memory));
                return Err(error.into());
            }
        };
        let result = (|| -> anyhow::Result<()> {
            if let Err(error) = OpenClipboard(Some(owner)) {
                let _ = GlobalFree(Some(memory));
                return Err(error.into());
            }
            let result = EmptyClipboard()
                .and_then(|_| SetClipboardData(15, Some(HANDLE(memory.0))).map(|_| ()));
            let _ = CloseClipboard();
            if result.is_err() {
                let _ = GlobalFree(Some(memory));
            }
            result?;
            Ok(())
        })();
        let _ = DestroyWindow(owner);
        result?;
        anyhow::ensure!(
            clipboard_files()?.len() == paths.len(),
            "Windows did not publish the file clipboard format"
        );
    }
    Ok(())
}
#[implement(IDropSource)]
struct DropSource;
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & 1 == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }
    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
thread_local! {
    static DROP_POINT: std::cell::Cell<Option<(HWND, POINT)>> = const { std::cell::Cell::new(None) };
}
struct DropQueue {
    window: HWND,
    hook: Option<HHOOK>,
}
impl Drop for DropQueue {
    fn drop(&mut self) {
        DROP_POINT.set(None);
        unsafe {
            if let Some(hook) = self.hook {
                let _ = UnhookWindowsHookEx(hook);
            }
            let _ = DestroyWindow(self.window);
        }
    }
}
unsafe extern "system" fn drop_message_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0
            && let Some((window, point)) = DROP_POINT.get()
        {
            let message = &mut *(lparam.0 as *mut MSG);
            if message.hwnd == window && matches!(message.message, WM_MOUSEMOVE | WM_LBUTTONUP) {
                // Posted messages otherwise inherit (0,0) on this non-input
                // thread. OLE reads MSG.pt when locating the native drop target.
                message.pt = point;
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}
pub fn drop_files(
    paths: &[PathBuf],
    display_id: meshrmm_protocol::DisplayId,
    x: u16,
    y: u16,
) -> anyhow::Result<bool> {
    let displays = DISPLAYS.get_or_init(Mutex::default).lock().unwrap();
    let display = displays
        .iter()
        .find(|d| d.id == display_id)
        .context("drop targeted a disconnected display")?;
    let x = display.x + (u64::from(x) * u64::from(display.width.saturating_sub(1)) / 65535) as i32;
    let y = display.y + (u64::from(y) * u64::from(display.height.saturating_sub(1)) / 65535) as i32;
    drop(displays);
    unsafe {
        SetCursorPos(x, y)?;
        let source: IDropSource = DropSource.into();
        let data = data_object(paths)?;
        tracing::debug!(x, y, "starting native file drop");
        let mut effect = DROPEFFECT_NONE;
        // OLE's initialization waits for a mouse message on its own queue.
        // The physical drag happened in the viewer, so seed our source window's
        // queue explicitly; SendInput would deliver it to the target app.
        let host = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            windows::core::w!("STATIC"),
            windows::core::w!("MeshRMM file drop"),
            WS_POPUP,
            -32000,
            -32000,
            1,
            1,
            None,
            None,
            None,
            None,
        )?;
        let mut queue = DropQueue {
            window: host,
            hook: None,
        };
        DROP_POINT.set(Some((host, POINT { x, y })));
        queue.hook = Some(SetWindowsHookExW(
            WH_GETMESSAGE,
            Some(drop_message_hook),
            None,
            windows::Win32::System::Threading::GetCurrentThreadId(),
        )?);
        PostMessageW(Some(host), WM_MOUSEMOVE, WPARAM(1), LPARAM(0))?;
        let host_value = host.0 as isize;
        let wake = std::thread::spawn(move || {
            // Browser targets negotiate the allowed effect with their renderer.
            // Keep pumping drag-over events before releasing the button.
            for _ in 0..6 {
                std::thread::sleep(std::time::Duration::from_millis(80));
                let _ = PostMessageW(
                    Some(HWND(host_value as *mut _)),
                    WM_MOUSEMOVE,
                    WPARAM(1),
                    LPARAM(0),
                );
            }
            PostMessageW(
                Some(HWND(host_value as *mut _)),
                WM_LBUTTONUP,
                WPARAM(0),
                LPARAM(0),
            )
        });
        let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        let _ = wake.join();
        drop(queue);
        tracing::info!(?effect, ?result, "native file drop completed");
        Ok(effect == DROPEFFECT_COPY)
    }
}

#[implement(IFileDialogEvents, IFileDialogControlEvents)]
struct PickerEvents {
    selected: std::sync::Arc<Mutex<Vec<PathBuf>>>,
}
impl IFileDialogControlEvents_Impl for PickerEvents_Impl {
    fn OnButtonClicked(
        &self,
        customize: Ref<IFileDialogCustomize>,
        _: u32,
    ) -> windows::core::Result<()> {
        unsafe {
            let dialog: IFileOpenDialog = customize.as_ref().unwrap().cast()?;
            let items = dialog.GetSelectedItems()?;
            let mut paths = Vec::new();
            for index in 0..items.GetCount()? {
                let text = items.GetItemAt(index)?.GetDisplayName(SIGDN_FILESYSPATH)?;
                paths.push(PathBuf::from(text.to_string()?));
                CoTaskMemFree(Some(text.0.cast()));
            }
            if !paths.is_empty() {
                *self.selected.lock().unwrap() = paths;
                dialog.Close(HRESULT(0))?;
            }
        }
        Ok(())
    }
    fn OnItemSelected(
        &self,
        _: Ref<IFileDialogCustomize>,
        _: u32,
        _: u32,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnCheckButtonToggled(
        &self,
        _: Ref<IFileDialogCustomize>,
        _: u32,
        _: BOOL,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnControlActivating(
        &self,
        _: Ref<IFileDialogCustomize>,
        _: u32,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}
impl IFileDialogEvents_Impl for PickerEvents_Impl {
    fn OnFileOk(&self, _: Ref<IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnFolderChanging(
        &self,
        _: Ref<IFileDialog>,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnFolderChange(&self, _: Ref<IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSelectionChange(&self, _: Ref<IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnShareViolation(
        &self,
        _: Ref<IFileDialog>,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<FDE_SHAREVIOLATION_RESPONSE> {
        Ok(FDESVR_DEFAULT)
    }
    fn OnTypeChange(&self, _: Ref<IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnOverwrite(
        &self,
        _: Ref<IFileDialog>,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<FDE_OVERWRITE_RESPONSE> {
        Ok(FDEOR_DEFAULT)
    }
}

pub fn paste_files(display_id: meshrmm_protocol::DisplayId) -> anyhow::Result<()> {
    anyhow::ensure!(
        DISPLAYS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.id == display_id),
        "paste targeted a disconnected display"
    );
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let events: Vec<INPUT> = [
        (VK_CONTROL, false),
        (VIRTUAL_KEY(0x56), false),
        (VIRTUAL_KEY(0x56), true),
        (VK_CONTROL, true),
    ]
    .into_iter()
    .map(|(key, up)| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                ..Default::default()
            },
        },
    })
    .collect();
    anyhow::ensure!(
        unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) } == events.len() as u32,
        "Windows rejected file paste input"
    );
    Ok(())
}

pub fn cache() -> anyhow::Result<PathBuf> {
    unsafe {
        let text = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)?;
        let path = PathBuf::from(text.to_string()?)
            .join("MeshRMM")
            .join("FileTransferCache");
        CoTaskMemFree(Some(text.0.cast()));
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_file_data_object_advertises_native_file_drop_format() {
        let _guard = initialize().unwrap();
        let folder = std::env::temp_dir().join(format!("meshrmm-shell-test-{}", crate::id()));
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("file.txt");
        std::fs::write(&file, "fixture").unwrap();
        let data = data_object(std::slice::from_ref(&file)).unwrap();
        let format = FORMATETC {
            cfFormat: 15,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        unsafe {
            data.QueryGetData(&format).ok().unwrap();
            let mut medium = data.GetData(&format).unwrap();
            assert_eq!(paths_from_drop(HDROP(medium.u.hGlobal.0)), vec![file]);
            ReleaseStgMedium(&mut medium);
            let formats = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
            let mut entries = [FORMATETC::default()];
            formats.Next(&mut entries, None).ok().unwrap();
            assert_eq!(entries[0].cfFormat, 15);
        }
        drop(data);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
