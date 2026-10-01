//! Restarts the computer for the technician, optionally into Safe Mode with
//! Networking. Windows starts only the services registered under
//! `SafeBoot\Network` there, so the Agent registers itself before setting the
//! boot option. It also leaves a marker, and the service clears the option once
//! Windows is back, so a later restart returns to normal mode even if the
//! technician cannot reconnect.

/// Whether `bcdedit /enum {current}` output lists a Safe Mode boot option.
/// Element names are not localized; descriptions and headers are.
#[cfg_attr(not(windows), allow(dead_code))]
fn lists_safe_boot(output: &str) -> bool {
    output.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("safeboot"))
    })
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(windows)]
mod windows_impl {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    use anyhow::{Context, bail};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_OPTION_NON_VOLATILE, REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegSetValueExW,
    };
    use windows::Win32::System::Shutdown::{
        InitiateShutdownW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
        SHTDN_REASON_MINOR_OTHER, SHUTDOWN_FORCE_OTHERS, SHUTDOWN_FORCE_SELF,
        SHUTDOWN_GRACE_OVERRIDE, SHUTDOWN_RESTART,
    };
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, GetCurrentProcess, OpenProcessToken,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CLEANBOOT};
    use windows::core::{HSTRING, PCWSTR, w};

    use crate::win32::OwnedHandle;

    const SAFE_BOOT_SERVICE_KEY: PCWSTR =
        w!(r"SYSTEM\CurrentControlSet\Control\SafeBoot\Network\MeshRMMAgent");
    /// Written beside `agent.json` while the Agent's Safe Mode boot option is set.
    const SAFE_MODE_MARKER: &str = "safe-mode-restart";

    /// Whether Windows started in Safe Mode, with or without networking.
    pub fn booted_in_safe_mode() -> bool {
        unsafe { GetSystemMetrics(SM_CLEANBOOT) != 0 }
    }

    /// Lets the service start in Safe Mode with Networking.
    pub fn register_safe_mode_service() -> anyhow::Result<()> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                SAFE_BOOT_SERVICE_KEY,
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_WOW64_64KEY,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .context("could not register the Agent service for Safe Mode")?;
        let value: Vec<u8> = "Service\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let result = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(&value)) };
        let _ = unsafe { RegCloseKey(key) };
        result
            .ok()
            .context("could not register the Agent service for Safe Mode")
    }

    pub fn unregister_safe_mode_service() -> anyhow::Result<()> {
        let result = unsafe {
            RegDeleteKeyExW(
                HKEY_LOCAL_MACHINE,
                SAFE_BOOT_SERVICE_KEY,
                KEY_WOW64_64KEY.0,
                None,
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        result
            .ok()
            .context("could not remove the Agent's Safe Mode registration")
    }

    /// Restarts Windows at once, closing applications without saving.
    /// `safe_mode` starts it in Safe Mode with Networking; otherwise it starts
    /// normally, even when it is in Safe Mode now.
    pub fn restart(safe_mode: bool) -> anyhow::Result<()> {
        let marker = crate::installer::config_directory()?.join(SAFE_MODE_MARKER);
        if safe_mode {
            register_safe_mode_service()?;
            std::fs::write(&marker, b"")
                .with_context(|| format!("could not write {}", marker.display()))?;
            if let Err(error) = bcdedit(&["/set", "{current}", "safeboot", "network"]) {
                let _ = std::fs::remove_file(&marker);
                return Err(error.context("could not set the Safe Mode boot option"));
            }
        } else {
            clear_safe_boot().context("could not clear the Safe Mode boot option")?;
            let _ = std::fs::remove_file(&marker);
        }
        if let Err(error) = initiate_restart() {
            if safe_mode && clear_safe_boot().is_ok() {
                let _ = std::fs::remove_file(&marker);
            }
            return Err(error);
        }
        tracing::info!(safe_mode, "Windows is restarting for the technician");
        Ok(())
    }

    /// Run once when the service starts: removes the Safe Mode boot option
    /// the Agent set for the restart that just happened.
    pub fn finish_safe_mode_restart() {
        let marker = match crate::installer::config_directory() {
            Ok(directory) => directory.join(SAFE_MODE_MARKER),
            Err(error) => {
                tracing::warn!(error = ?error, "could not find the Agent configuration directory");
                return;
            }
        };
        if !marker.exists() {
            return;
        }
        match clear_safe_boot() {
            Ok(()) => {
                let _ = std::fs::remove_file(&marker);
                tracing::info!(
                    safe_mode = booted_in_safe_mode(),
                    "cleared the Safe Mode boot option; the next restart starts Windows normally"
                );
            }
            Err(error) => {
                tracing::error!(error = ?error, "could not clear the Safe Mode boot option")
            }
        }
    }

    fn clear_safe_boot() -> anyhow::Result<()> {
        if super::lists_safe_boot(&bcdedit(&["/enum", "{current}"])?) {
            bcdedit(&["/deletevalue", "{current}", "safeboot"])?;
        }
        Ok(())
    }

    fn bcdedit(arguments: &[&str]) -> anyhow::Result<String> {
        let program = system32()?.join("bcdedit.exe");
        let output = Command::new(&program)
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW.0)
            .output()
            .with_context(|| format!("could not run {}", program.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            bail!(
                "bcdedit {} failed ({}): {}",
                arguments.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim().to_owned() + stdout.trim()
            );
        }
        Ok(stdout)
    }

    fn system32() -> anyhow::Result<PathBuf> {
        Ok(crate::win32::windows_directory()?.join("System32"))
    }

    fn initiate_restart() -> anyhow::Result<()> {
        enable_shutdown_privilege()?;
        let message = HSTRING::from("A MeshRMM technician restarted this computer.");
        let result = unsafe {
            InitiateShutdownW(
                PCWSTR::null(),
                &message,
                0,
                SHUTDOWN_RESTART
                    | SHUTDOWN_FORCE_OTHERS
                    | SHUTDOWN_FORCE_SELF
                    | SHUTDOWN_GRACE_OVERRIDE,
                SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
            )
        };
        windows::Win32::Foundation::WIN32_ERROR(result)
            .ok()
            .context("Windows refused to restart")
    }

    /// LocalSystem holds the privilege disabled. The computer is about to
    /// restart, so it stays enabled.
    fn enable_shutdown_privilege() -> anyhow::Result<()> {
        let mut token = Default::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
        }
        .context("could not open the Agent process token")?;
        let token = OwnedHandle(token);
        let mut luid = LUID::default();
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_SHUTDOWN_NAME, &mut luid) }
            .context("could not look up the shutdown privilege")?;
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        unsafe { AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None) }
            .context("could not enable the shutdown privilege")?;
        // AdjustTokenPrivileges succeeds without assigning a privilege the token lacks.
        let last = windows::core::Error::from_thread();
        if last.code() == windows::Win32::Foundation::ERROR_NOT_ALL_ASSIGNED.to_hresult() {
            bail!("the Agent does not hold the shutdown privilege");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::lists_safe_boot;

    #[test]
    fn detects_the_safe_boot_element_only() {
        let normal = "Windows Boot Loader\n-------------------\nidentifier              {current}\ndevice                  partition=C:\nnx                      OptIn\n";
        assert!(!lists_safe_boot(normal));
        assert!(lists_safe_boot(&format!(
            "{normal}safeboot                Network\n"
        )));
        // Alternate-shell is a different element and description text may mention it.
        assert!(!lists_safe_boot(&format!(
            "{normal}safebootalternateshell  Yes\ndescription             safeboot test\n"
        )));
    }
}
