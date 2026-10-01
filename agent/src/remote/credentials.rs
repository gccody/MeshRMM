//! Credentials stay on the endpoint. Only DPAPI ciphertext crosses inherited
//! helper pipes or reaches disk; only status crosses the authenticated remote
//! control channel.
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, HLOCAL, HWND, LPARAM, LocalFree},
        Security::{
            Credentials::*, Cryptography::*, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT,
            LogonUserW,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Threading::*,
        },
        UI::{Accessibility::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::{BOOL, BSTR, PCWSTR, PWSTR, w},
};
use zeroize::{Zeroize, Zeroizing};

fn wide(s: &str) -> Zeroizing<Vec<u16>> {
    Zeroizing::new(s.encode_utf16().chain(Some(0)).collect())
}
fn end(s: &[u16]) -> usize {
    s.iter().position(|c| *c == 0).unwrap_or(s.len())
}

fn protect(data: &mut [u8], decrypt: bool) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len().try_into()?,
        pbData: data.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if decrypt {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        } else {
            CryptProtectData(
                &input,
                w!("MeshRMM saved credentials"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        }
        let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let result = Zeroizing::new(bytes.to_vec());
        bytes.zeroize();
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}

const MAX_PROTECTED_BYTES: u64 = 8192;

/// Ciphertext lives beside agent.json, whose directory only SYSTEM and
/// Administrators can access, and survives sessions, updates, and reboots until
/// explicitly forgotten. Only the LocalSystem DPAPI key can decrypt it.
pub fn saved(store: &Path) -> bool {
    store.is_file()
}
pub fn load(store: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::metadata(store) {
        Ok(metadata) => ensure!(
            metadata.len() <= MAX_PROTECTED_BYTES,
            "Saved credentials are invalid; forget them and prompt again"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not read saved credentials"),
    }
    Ok(Some(
        std::fs::read(store).context("Could not read saved credentials")?,
    ))
}
pub fn save(store: &Path, encrypted: &[u8]) -> anyhow::Result<()> {
    ensure!(
        encrypted.len() as u64 <= MAX_PROTECTED_BYTES,
        "Invalid protected credential data"
    );
    crate::installer::replace_file(store, encrypted)
}
pub fn forget(store: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(store) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).context("Could not delete saved credentials")
        }
        _ => Ok(()),
    }
}

/// The helper runs in the background without foreground rights, so Windows
/// would open the credential dialog behind the user's windows. Watches for the
/// dialog on the prompting thread and brings it to the front with keyboard focus.
struct RaisePrompt {
    done: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}
impl RaisePrompt {
    const TIMEOUT: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(20);

    fn start() -> Self {
        let owner = unsafe { GetCurrentThreadId() };
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let done = done.clone();
            thread::Builder::new()
                .name("meshrmm-credential-raise".into())
                .spawn(move || {
                    let deadline = Instant::now() + Self::TIMEOUT;
                    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                        if let Some(window) = shown_window(owner) {
                            bring_to_front(window);
                            return;
                        }
                        thread::sleep(Self::POLL);
                    }
                })
                .inspect_err(
                    |error| tracing::warn!(%error, "could not raise the credential dialog"),
                )
                .ok()
        };
        Self { done, watcher }
    }
}
impl Drop for RaisePrompt {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// The first shown top-level window of `thread`.
fn shown_window(thread: u32) -> Option<HWND> {
    unsafe extern "system" fn find(hwnd: HWND, context: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() {
                *(context.0 as *mut Option<HWND>) = Some(hwnd);
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut window = None;
    unsafe {
        let _ = EnumThreadWindows(
            thread,
            Some(find),
            LPARAM(&mut window as *mut Option<HWND> as isize),
        );
    }
    window
}

/// Keeps `window` above other windows and makes it the foreground window.
/// Windows lets the process that sent the last input take the foreground, so
/// send a mouse move that doesn't move the cursor first.
fn bring_to_front(window: HWND) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_MOVE,
                dwExtraInfo: super::input_block::NEUTRAL_TAG,
                ..Default::default()
            },
        },
    };
    unsafe {
        let _ = SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(window);
    }
}

/// Cancel and failed validation preserve any previously validated credentials.
/// One logon attempt per explicit dialog submission; never retry automatically.
pub fn prompt() -> anyhow::Result<Option<(Vec<u8>, String)>> {
    let mut user = Zeroizing::new(vec![0u16; 514]);
    let mut password = Zeroizing::new(vec![0u16; 257]);
    let info = CREDUI_INFOW {
        cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
        pszCaptionText: w!("MeshRMM — share credentials with your technician"),
        pszMessageText: w!(
            "Your technician requested Windows credentials. Windows will test them once and keep them encrypted on this computer, including after restarts, until the technician chooses Forget credentials. The technician can then fill Windows password prompts. Use DOMAIN\\user, user@domain, or .\\localuser. Enter your password, not your PIN."
        ),
        ..Default::default()
    };
    let _raise = RaisePrompt::start();
    let result = unsafe {
        CredUIPromptForCredentialsW(
            Some(&info),
            w!("MeshRMM"),
            None,
            0,
            &mut user,
            &mut password,
            None,
            CREDUI_FLAGS_GENERIC_CREDENTIALS
                | CREDUI_FLAGS_ALWAYS_SHOW_UI
                | CREDUI_FLAGS_DO_NOT_PERSIST,
        )
    };
    if result == ERROR_CANCELLED {
        return Ok(None);
    }
    result.ok().context("Windows credential dialog failed")?;
    ensure!(
        end(&password) > 0,
        "A Windows password is required; PINs and empty passwords are not supported"
    );
    let username = Zeroizing::new(String::from_utf16(&user[..end(&user)])?);
    let (domain, account) = match username.split_once('\\') {
        Some((d, u)) => (Some(wide(d)), wide(u)),
        None if username.contains('@') => (None, wide(&username)),
        None => (Some(wide(".")), wide(&username)),
    };
    let mut token = HANDLE::default();
    unsafe {
        LogonUserW(
            PCWSTR(account.as_ptr()),
            domain
                .as_ref()
                .map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            PCWSTR(password.as_ptr()),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    }
    .context("Windows could not validate these credentials; nothing was saved")?;
    unsafe {
        CloseHandle(token)?;
    }
    let mut plain = Zeroizing::new(Vec::new());
    // Fixed-size, bounded UTF-16 buffers simplify decoding and guarantee terminators.
    for c in user.iter().chain(password.iter()) {
        plain.extend_from_slice(&c.to_le_bytes());
    }
    Ok(Some((
        protect(&mut plain, false)?.to_vec(),
        username.to_string(),
    )))
}

// BSTR owns an additional plaintext copy; wipe it before its allocator frees it.
struct SecretBstr(BSTR);
impl Drop for SecretBstr {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            unsafe {
                std::slice::from_raw_parts_mut(self.0.as_ptr() as *mut u16, self.0.len()).zeroize();
            }
        }
    }
}
fn set_value(field: &IUIAutomationValuePattern, text: &[u16]) -> anyhow::Result<()> {
    let value = SecretBstr(BSTR::from_wide(&text[..end(text)]));
    unsafe {
        field.SetValue(&value.0)?;
    }
    Ok(())
}

fn trusted_prompt_process(path: &str, system_root: &str) -> bool {
    let path = path.to_lowercase();
    let system_root = system_root.to_lowercase();
    path == format!("{system_root}\\system32\\logonui.exe")
        || path == format!("{system_root}\\system32\\consent.exe")
}
fn supported_password_id(id: &str) -> bool {
    let id = id.to_lowercase();
    id == "password"
        || id == "passwordfield"
        || id
            .strip_prefix("passwordfield_")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit()))
}

fn process_path(pid: u32) -> anyhow::Result<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
        let mut path = [0u16; 32768];
        let mut length = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result?;
        Ok(String::from_utf16(&path[..length as usize])?)
    }
}

/// Visible top-level windows of trusted prompt processes on this desktop: the
/// foreground window first, then the rest from the top of the z-order.
fn prompt_windows() -> anyhow::Result<Vec<(HWND, u32)>> {
    unsafe extern "system" fn collect(hwnd: HWND, context: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
                (*(context.0 as *mut Vec<HWND>)).push(hwnd);
            }
        }
        BOOL(1)
    }
    let mut windows = Vec::new();
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(&mut windows as *mut Vec<HWND> as isize),
        )?;
    }
    let foreground = unsafe { GetForegroundWindow() };
    if let Some(index) = windows.iter().position(|hwnd| *hwnd == foreground) {
        windows[..=index].rotate_right(1);
    }
    let system = crate::win32::windows_directory()?
        .to_string_lossy()
        .to_lowercase();
    let mut trusted = HashMap::new();
    Ok(windows
        .into_iter()
        .filter_map(|hwnd| {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            let allowed = pid != 0
                && *trusted.entry(pid).or_insert_with(|| {
                    process_path(pid).is_ok_and(|path| trusted_prompt_process(&path, &system))
                });
            allowed.then_some((hwnd, pid))
        })
        .collect())
}

struct PromptFields {
    hwnd: HWND,
    pid: u32,
    username: Option<IUIAutomationValuePattern>,
    password: IUIAutomationValuePattern,
}
impl PromptFields {
    /// The verified prompt window still exists, is shown, and has the same owner.
    fn present(&self) -> bool {
        let mut pid = 0;
        unsafe {
            IsWindow(Some(self.hwnd)).as_bool()
                && IsWindowVisible(self.hwnd).as_bool()
                && GetWindowThreadProcessId(self.hwnd, Some(&mut pid)) != 0
                && pid == self.pid
        }
    }
}

pub struct Detector {
    automation: std::mem::ManuallyDrop<IUIAutomation>,
}
impl Detector {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
            Ok(automation) => Ok(Self {
                automation: std::mem::ManuallyDrop::new(automation),
            }),
            Err(error) => {
                unsafe {
                    CoUninitialize();
                }
                Err(error.into())
            }
        }
    }
    pub fn ready(&self) -> bool {
        self.fields().is_ok()
    }
    /// Tries each visible trusted prompt window, so a Windows prompt counts
    /// even when another window or control has keyboard focus.
    fn fields(&self) -> anyhow::Result<PromptFields> {
        let mut first_error = None;
        for (hwnd, pid) in prompt_windows()? {
            match self.window_fields(hwnd, pid) {
                Ok(fields) => return Ok(fields),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| anyhow!("No Windows credential prompt is open")))
    }
    fn window_fields(&self, hwnd: HWND, pid: u32) -> anyhow::Result<PromptFields> {
        unsafe {
            let root = self.automation.ElementFromHandle(hwnd)?;
            let all = root.FindAll(
                TreeScope_Descendants,
                &self.automation.CreateTrueCondition()?,
            )?;
            let mut username = None;
            let mut password = None;
            let count = all.Length()?;
            ensure!(
                count <= 256,
                "Windows credential tree is too large to verify"
            );
            for index in 0..count {
                let field = all.GetElement(index)?;
                if field.CurrentProcessId()? != pid as i32 {
                    continue;
                }
                if field.CurrentControlType()? != UIA_EditControlTypeId
                    || !field.CurrentIsEnabled()?.as_bool()
                    || field.CurrentIsOffscreen()?.as_bool()
                {
                    continue;
                }
                // PIN and third-party credential-provider fields must not receive a password.
                let id = field.CurrentAutomationId()?.to_string().to_lowercase();
                if field.CurrentIsPassword()?.as_bool() {
                    ensure!(
                        supported_password_id(&id),
                        "Unsupported password provider (or PIN selected)"
                    );
                    ensure!(password.is_none(), "Ambiguous Windows password fields");
                    let pattern: IUIAutomationValuePattern =
                        field.GetCurrentPatternAs(UIA_ValuePatternId)?;
                    ensure!(
                        !pattern.CurrentIsReadOnly()?.as_bool(),
                        "Password field is read-only"
                    );
                    password = Some(pattern);
                } else {
                    ensure!(username.is_none(), "Ambiguous Windows username fields");
                    username = Some(
                        field
                            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)?,
                    );
                }
            }
            Ok(PromptFields {
                hwnd,
                pid,
                username,
                password: password.context("Select the Windows password sign-in option")?,
            })
        }
    }
    pub fn fill(&self, encrypted: &mut [u8]) -> anyhow::Result<()> {
        let fields = self.fields()?;
        let plain = protect(encrypted, true)?;
        ensure!(
            plain.len() == (514 + 257) * 2,
            "Invalid protected credential data"
        );
        let chars = Zeroizing::new(
            plain
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        );
        ensure!(
            chars[513] == 0 && chars[770] == 0,
            "Invalid credential terminators"
        );
        ensure!(fields.present(), "Windows prompt changed; try again");
        if let Some(username) = &fields.username {
            set_value(username, &chars[..514])?;
        }
        ensure!(fields.present(), "Windows prompt changed; try again");
        // Target the verified control directly. No focus change, clipboard, global
        // keystrokes, tab-order assumptions, or automatic submission.
        set_value(&fields.password, &chars[514..])
            .context("This Windows password provider does not support autofill")?;
        Ok(())
    }
}
impl Drop for Detector {
    fn drop(&mut self) {
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.automation);
            CoUninitialize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_windows_password_prompts_are_allowed() {
        assert!(trusted_prompt_process(
            r"C:\WINDOWS\System32\LogonUI.exe",
            r"C:\Windows"
        ));
        assert!(trusted_prompt_process(
            r"C:\Windows\system32\consent.exe",
            r"C:\Windows"
        ));
        for path in [
            r"C:\Users\Public\consent.exe",
            r"C:\Windows\System32\consent.exe.fake",
            r"C:\Windows\System32\notepad.exe",
        ] {
            assert!(!trusted_prompt_process(path, r"C:\Windows"));
        }
        for id in ["Password", "PasswordField", "PasswordField_2"] {
            assert!(supported_password_id(id));
        }
        for id in [
            "",
            "PINField",
            "PasswordFieldPin",
            "PasswordField_",
            "PasswordField_otp",
        ] {
            assert!(!supported_password_id(id));
        }
    }
    #[test]
    fn saved_credentials_persist_until_forgotten() {
        let directory =
            std::env::temp_dir().join(format!("meshrmm-credentials-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = directory.join("autofill-credentials.dat");
        assert!(!saved(&store));
        assert!(load(&store).unwrap().is_none());
        save(&store, &[1, 2, 3]).unwrap();
        save(&store, &[4, 5]).unwrap();
        assert!(saved(&store));
        assert_eq!(load(&store).unwrap().unwrap(), [4, 5]);
        assert!(save(&store, &[0; 8193]).is_err());
        std::fs::write(&store, [0; 8193]).unwrap();
        assert!(load(&store).is_err());
        forget(&store).unwrap();
        forget(&store).unwrap();
        assert!(!saved(&store));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn dpapi_round_trip_and_tamper_rejection() {
        let mut plain = b"credential-test-only".to_vec();
        let mut encrypted = protect(&mut plain, false).unwrap();
        assert_ne!(&*encrypted, &plain);
        assert_eq!(&*protect(&mut encrypted, true).unwrap(), &plain);
        let last = encrypted.len() - 1;
        encrypted[last] ^= 1;
        assert!(protect(&mut encrypted, true).is_err());
    }
}
