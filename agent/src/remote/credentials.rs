//! Credentials stay on the endpoint. Only DPAPI ciphertext crosses inherited
//! helper pipes; only status crosses the authenticated remote control channel.
use anyhow::{Context, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, HLOCAL, HWND, LocalFree},
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
        UI::{Accessibility::*, WindowsAndMessaging::*},
    },
    core::{BSTR, PCWSTR, PWSTR, w},
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
                w!("MeshRMM session credentials"),
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

/// Cancel and failed validation preserve any previously validated credentials.
/// One logon attempt per explicit dialog submission; never retry automatically.
pub fn prompt() -> anyhow::Result<Option<(Vec<u8>, String)>> {
    let mut user = Zeroizing::new(vec![0u16; 514]);
    let mut password = Zeroizing::new(vec![0u16; 257]);
    let info = CREDUI_INFOW {
        cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
        pszCaptionText: w!("MeshRMM — share credentials for this remote session"),
        pszMessageText: w!(
            "Your technician requested Windows credentials. Windows will test them once and keep them encrypted on this computer until this remote session ends. The technician can then fill Windows password prompts. Use DOMAIN\\user, user@domain, or .\\localuser. Enter your password, not your PIN."
        ),
        ..Default::default()
    };
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
    fn fields(
        &self,
    ) -> anyhow::Result<(
        HWND,
        Option<IUIAutomationValuePattern>,
        IUIAutomationValuePattern,
    )> {
        unsafe {
            let hwnd = GetForegroundWindow();
            ensure!(!hwnd.is_invalid(), "No foreground Windows prompt");
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
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
            let path = String::from_utf16(&path[..length as usize])?.to_lowercase();
            let system = std::env::var("SystemRoot")?.to_lowercase();
            ensure!(
                trusted_prompt_process(&path, &system),
                "The foreground window is not a Windows credential prompt"
            );
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
            Ok((
                hwnd,
                username,
                password.context("Select the Windows password sign-in option")?,
            ))
        }
    }
    pub fn fill(&self, encrypted: &mut [u8]) -> anyhow::Result<()> {
        let (hwnd, username, password) = self.fields()?;
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
        unsafe {
            ensure!(
                GetForegroundWindow() == hwnd,
                "Windows prompt changed; try again"
            );
            if let Some(username) = username {
                set_value(&username, &chars[..514])?;
            }
            ensure!(
                GetForegroundWindow() == hwnd,
                "Windows prompt changed; try again"
            );
            // Target the verified control directly. No clipboard, global keystrokes,
            // tab-order assumptions, or automatic submission.
            set_value(&password, &chars[514..])
                .context("This Windows password provider does not support autofill")?;
        }
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
