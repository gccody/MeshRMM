//! Secure attention must originate in the Session 0 coordinator, never the
//! desktop helper: ordinary SendInput key events cannot generate Ctrl+Alt+Del.

use anyhow::{Context, bail};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::w;

pub(super) fn send() -> anyhow::Result<()> {
    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }
        .context("could not determine the Agent session")?;
    if session_id != 0 {
        bail!("Ctrl+Alt+Del requires the installed Windows Agent service");
    }

    // SendSAS returns no status. Check policy first so a blocked request produces
    // a useful viewer error instead of silently doing nothing. Do not override
    // an administrator's Windows security policy.
    let mut policy = 0u32;
    let mut size = std::mem::size_of_val(&policy) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\System"),
            w!("SoftwareSASGeneration"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut policy as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if result != ERROR_FILE_NOT_FOUND {
        result
            .ok()
            .context("could not read Windows secure attention policy")?;
    }
    if !matches!(policy, 1 | 3) {
        bail!(
            "Windows policy blocks Ctrl+Alt+Del. Enable 'Disable or enable software Secure Attention Sequence' under Computer Configuration > Administrative Templates > Windows Components > Windows Logon Options, and allow Services"
        );
    }

    // FALSE identifies a service caller and targets the active console session,
    // which is also the session captured by MeshRMM's desktop helper.
    unsafe { SendSAS(false) };
    Ok(())
}
