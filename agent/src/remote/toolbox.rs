//! Runs toolbox scripts and saves toolbox files when the server asks, then
//! reports how they went. See docs/toolbox.md.
//!
//! Each run or delivery gets its own thread, off the coordinator's signaling
//! loop, and reports over HTTPS with the Agent's credential, so a result
//! survives a signaling reconnect.
//!
//! A script runs as SYSTEM, or as the signed-in user when asked; with nobody
//! signed in it runs as SYSTEM instead. A file goes to the signed-in user's
//! Documents transfer folder, where the viewer's own transfers go, or to
//! Public Documents for the background desktop or when nobody is signed in.
//! While a user is signed in, files are written with their token, so they
//! own them and a folder they control cannot redirect a SYSTEM write.
//! On a Mac, root takes SYSTEM's place and `/Users/Shared` Public Documents'.

use meshrmm_protocol::ScriptLanguage;

/// Runs past this many at once are refused rather than queued.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const MAX_RUNNING_SCRIPTS: usize = 8;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const MAX_RUNNING_DELIVERIES: usize = 4;

/// The script file's name and its bytes as the interpreter reads them.
/// Both interpreters want CRLF lines. Windows PowerShell 5.1 reads a script
/// without a byte-order mark in the ANSI code page, so the file gets one.
/// `cmd` reads a batch file a line at a time in the console code page, so a
/// first line switches it to UTF-8, which also makes the output UTF-8.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn script_file(language: ScriptLanguage, body: &str) -> (&'static str, Vec<u8>) {
    let body = body.replace("\r\n", "\n").replace('\n', "\r\n");
    match language {
        ScriptLanguage::Powershell => {
            let mut bytes = "\u{feff}".as_bytes().to_vec();
            bytes.extend_from_slice(body.as_bytes());
            ("script.ps1", bytes)
        }
        ScriptLanguage::Cmd => {
            let mut bytes = b"@chcp 65001>nul\r\n".to_vec();
            bytes.extend_from_slice(body.as_bytes());
            ("script.cmd", bytes)
        }
        ScriptLanguage::Shell => ("script.sh", body.replace("\r\n", "\n").into_bytes()),
    }
}

/// The arguments that run `script` in `language`'s interpreter. PowerShell
/// is told to write UTF-8, and its exit code is the script's `exit` code.
#[cfg_attr(not(windows), allow(dead_code))]
fn interpreter_arguments(language: ScriptLanguage, script: &str) -> String {
    match language {
        ScriptLanguage::Powershell => format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"try {{ [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) }} catch {{}}; & '{}'; exit $LASTEXITCODE\"",
            script.replace('\'', "''")
        ),
        ScriptLanguage::Cmd => format!("/D /S /C \"\"{script}\"\""),
        ScriptLanguage::Shell => format!("\"{script}\""),
    }
}

/// Output a script wrote, up to the limit the server keeps.
#[derive(Debug, Default)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
struct Output {
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl Output {
    fn push(&mut self, chunk: &[u8]) {
        let room = meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

/// Output bytes as text: UTF-8, which both interpreters are asked to write,
/// else `fallback`'s decoding, for programs that write the OEM code page.
/// A character cut at the end by the output limit is dropped.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn decode_output(bytes: &[u8], fallback: impl Fn(&[u8]) -> String) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => fallback(bytes),
    }
}

/// `name`, or `name (2)`, `name (3)`… before its extension, whichever
/// `taken` says is free.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    if !taken(name) {
        return Some(name.to_owned());
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (2..1000)
        .map(|number| format!("{stem} ({number}){extension}"))
        .find(|candidate| !taken(candidate))
}

/// Run IDs name a directory, so they must be plain.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(windows)]
use self::windows as platform;

#[cfg(any(windows, target_os = "macos"))]
mod shared {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::Context;
    use meshrmm_protocol::{
        FileDeliveryReport, FileDeliveryRequest, FileDeliveryStatus, ScriptRunReport,
        ScriptRunRequest, ScriptRunStatus,
    };
    use sha2::{Digest, Sha256};

    use super::{MAX_RUNNING_DELIVERIES, MAX_RUNNING_SCRIPTS, platform};
    use crate::remote::config::{Config, ExecutionMode};

    static RUNNING_SCRIPTS: AtomicUsize = AtomicUsize::new(0);
    static RUNNING_DELIVERIES: AtomicUsize = AtomicUsize::new(0);
    /// Reports are retried this many times, a little longer apart each time.
    const REPORT_ATTEMPTS: u32 = 6;
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    /// A place among a limited number of concurrent jobs, given back on drop.
    struct Slot(&'static AtomicUsize);

    impl Slot {
        fn take(counter: &'static AtomicUsize, limit: usize) -> Option<Self> {
            counter
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |running| {
                    (running < limit).then_some(running + 1)
                })
                .ok()
                .map(|_| Self(counter))
        }
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Runs `run` on its own thread and reports how it went.
    pub(crate) fn run_script(config: &Config, mode: ExecutionMode, run: ScriptRunRequest) {
        let config = config.clone();
        let slot = Slot::take(&RUNNING_SCRIPTS, MAX_RUNNING_SCRIPTS);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-toolbox-script".into())
            .spawn(move || {
                let report = match slot {
                    _ if !platform::runs(run.language) => failed(
                        platform::own_account(),
                        format!(
                            "{} scripts do not run on this device's operating system.",
                            run.language.label()
                        ),
                    ),
                    Some(_slot) => platform::execute_script(mode, &run),
                    None => failed(
                        platform::own_account(),
                        format!(
                            "The device is already running {MAX_RUNNING_SCRIPTS} toolbox scripts. Try again when one finishes."
                        ),
                    ),
                };
                tracing::info!(
                    run_id = %run.run_id,
                    status = report.status.as_str(),
                    exit_code = ?report.exit_code,
                    ran_as = %report.ran_as,
                    error = ?report.error,
                    "toolbox script finished"
                );
                send_report(&config, &["script-runs", &run.run_id, "result"], &report);
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for a toolbox script");
        }
    }

    /// Saves `delivery`'s file on its own thread and reports how it went.
    pub(crate) fn deliver_file(
        config: &Config,
        mode: ExecutionMode,
        delivery: FileDeliveryRequest,
    ) {
        let config = config.clone();
        let slot = Slot::take(&RUNNING_DELIVERIES, MAX_RUNNING_DELIVERIES);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-toolbox-file".into())
            .spawn(move || {
                let result = match slot {
                    Some(_slot) => platform::save_delivery(&config, mode, &delivery),
                    None => Err(anyhow::anyhow!(
                        "The device is already receiving {MAX_RUNNING_DELIVERIES} toolbox files. Try again when one finishes."
                    )),
                };
                let report = match result {
                    Ok(path) => {
                        tracing::info!(delivery_id = %delivery.delivery_id, path = %path.display(), "saved a toolbox file");
                        FileDeliveryReport {
                            status: FileDeliveryStatus::Delivered,
                            path: Some(path.display().to_string()),
                            error: None,
                        }
                    }
                    Err(error) => {
                        tracing::warn!(delivery_id = %delivery.delivery_id, error = ?error, "could not save a toolbox file");
                        FileDeliveryReport {
                            status: FileDeliveryStatus::Failed,
                            path: None,
                            error: Some(format!("{error:#}")),
                        }
                    }
                };
                send_report(
                    &config,
                    &["file-deliveries", &delivery.delivery_id, "result"],
                    &report,
                );
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for a toolbox file");
        }
    }

    pub(super) fn failed(ran_as: String, error: impl Into<String>) -> ScriptRunReport {
        ScriptRunReport {
            status: ScriptRunStatus::Failed,
            ran_as,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            output_truncated: false,
            error: Some(error.into()),
        }
    }

    /// A downloaded file waiting to be saved, removed with it.
    pub(super) struct Staged(pub(super) PathBuf);

    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Downloads the file into an administrator-only folder, checking its
    /// size and SHA-256 on the way.
    pub(super) fn download(
        config: &Config,
        mode: ExecutionMode,
        delivery: &FileDeliveryRequest,
    ) -> anyhow::Result<Staged> {
        let folder = platform::staging_folder(mode)?;
        let staged = Staged(folder.join(format!("{}.partial", delivery.delivery_id)));
        let url = meshrmm_signaling_client::endpoint_url(
            &config.server,
            &[
                "v1",
                "agents",
                &config.device_id,
                "file-deliveries",
                &delivery.delivery_id,
                "content",
            ],
            &[],
            false,
        )?;
        let response = http(DOWNLOAD_TIMEOUT)
            .get(url.as_str())
            .header("Authorization", &format!("Bearer {}", config.agent_token))
            .call()
            .context("the server did not provide the file")?;
        let mut body = response
            .into_body()
            .into_with_config()
            .limit(delivery.size_bytes.saturating_add(1))
            .reader();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged.0)
            .with_context(|| format!("could not create {}", staged.0.display()))?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let read = body
                .read(&mut chunk)
                .context("the download was interrupted")?;
            if read == 0 {
                break;
            }
            size += read as u64;
            anyhow::ensure!(
                size <= delivery.size_bytes,
                "the server sent more than the file's size"
            );
            digest.update(&chunk[..read]);
            file.write_all(&chunk[..read])
                .with_context(|| format!("could not write {}", staged.0.display()))?;
        }
        file.flush()?;
        anyhow::ensure!(size == delivery.size_bytes, "the download was incomplete");
        let actual: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        anyhow::ensure!(
            actual == delivery.sha256,
            "the download did not match the file's SHA-256"
        );
        Ok(staged)
    }

    fn http(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(true)
            .tls_config(crate::enrollment::https_tls_config())
            .build()
            .new_agent()
    }

    /// Posts a result to `/v1/agents/{device}/{path}`, retrying while the
    /// server cannot be reached. A refusal is final.
    fn send_report(config: &Config, path: &[&str], report: &impl serde::Serialize) {
        let mut segments = vec!["v1", "agents", config.device_id.as_str()];
        segments.extend_from_slice(path);
        let url =
            match meshrmm_signaling_client::endpoint_url(&config.server, &segments, &[], false) {
                Ok(url) => url,
                Err(error) => {
                    tracing::error!(%error, "could not build the toolbox report URL");
                    return;
                }
            };
        let http = http(Duration::from_secs(60));
        for attempt in 1..=REPORT_ATTEMPTS {
            match http
                .post(url.as_str())
                .header("Authorization", &format!("Bearer {}", config.agent_token))
                .send_json(report)
            {
                Ok(_) => return,
                Err(ureq::Error::StatusCode(status)) => {
                    tracing::warn!(status, url = %url, "the server refused a toolbox report");
                    return;
                }
                Err(error) if attempt < REPORT_ATTEMPTS => {
                    tracing::warn!(%error, attempt, "could not send a toolbox report; retrying");
                    std::thread::sleep(Duration::from_secs(2_u64.pow(attempt)));
                }
                Err(error) => {
                    tracing::error!(%error, url = %url, "could not send a toolbox report");
                }
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) use self::shared::{deliver_file, run_script};
#[cfg(any(windows, target_os = "macos"))]
use self::shared::{download, failed};

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use anyhow::Context;
    use meshrmm_protocol::{
        FileDeliveryDestination, FileDeliveryRequest, RunAs, ScriptRunReport, ScriptRunRequest,
        ScriptRunStatus,
    };
    use windows::Win32::Foundation::{
        HANDLE, HANDLE_FLAG_INHERIT, HLOCAL, LocalFree, SetHandleInformation, WAIT_TIMEOUT,
    };
    use windows::Win32::Globalization::{
        CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar,
    };
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{
        GetTokenInformation, ImpersonateLoggedOnUser, LookupAccountSidW, RevertToSelf,
        SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows::Win32::System::RemoteDesktop::{
        WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
        CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
        OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        STARTUPINFOW, TerminateProcess, WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::{
        FOLDERID_Documents, FOLDERID_PublicDocuments, GetUserProfileDirectoryW, KF_FLAG_CREATE,
        SHGetKnownFolderPath,
    };
    use windows::core::{GUID, PCWSTR, PWSTR};

    use super::{
        Output, decode_output, download, failed, interpreter_arguments, script_file, unique_name,
        valid_id,
    };
    use crate::remote::config::{Config, ExecutionMode};
    use crate::win32::{HandleListAttribute, OwnedHandle, create_pipe, wide};

    /// How long output readers may outlast the script. Programs the script
    /// started in the background can hold its output open indefinitely.
    const OUTPUT_DRAIN: Duration = Duration::from_secs(2);

    pub(super) fn execute_script(mode: ExecutionMode, run: &ScriptRunRequest) -> ScriptRunReport {
        let user = match run.run_as {
            RunAs::User => signed_in_user(mode),
            RunAs::System => None,
        };
        let ran_as = user
            .as_ref()
            .map_or_else(own_account, |user| user.name.clone());
        if !valid_id(&run.run_id) {
            return failed(ran_as, "The run has an invalid ID.");
        }
        let directory = match ScriptDirectory::create(mode, &run.run_id, user.as_ref()) {
            Ok(directory) => directory,
            Err(error) => {
                return failed(ran_as, format!("Could not prepare the script: {error:#}"));
            }
        };
        let (file_name, contents) = script_file(run.language, &run.body);
        let script = directory.0.join(file_name);
        if let Err(error) = std::fs::write(&script, contents) {
            return failed(ran_as, format!("Could not write the script: {error}"));
        }
        let outcome = interpreter(run.language).and_then(|interpreter| {
            let working_directory = match &user {
                Some(user) => user.profile.clone(),
                None => system_directory()?,
            };
            let command_line = format!(
                "\"{}\" {}",
                interpreter.display(),
                interpreter_arguments(run.language, &script.display().to_string())
            );
            execute(
                &interpreter,
                &command_line,
                &working_directory,
                user.as_ref(),
                Duration::from_secs(u64::from(run.timeout_seconds)),
            )
        });
        drop(directory);
        match outcome {
            Ok(outcome) => {
                let oem = |bytes: &[u8]| oem_text(bytes);
                ScriptRunReport {
                    status: if outcome.timed_out {
                        ScriptRunStatus::TimedOut
                    } else {
                        ScriptRunStatus::Completed
                    },
                    ran_as,
                    exit_code: Some(outcome.exit_code as i32),
                    output_truncated: outcome.stdout.truncated || outcome.stderr.truncated,
                    stdout: decode_output(&outcome.stdout.bytes, oem),
                    stderr: decode_output(&outcome.stderr.bytes, oem),
                    error: outcome.timed_out.then(|| {
                        format!(
                            "The script was stopped after {} seconds.",
                            run.timeout_seconds
                        )
                    }),
                }
            }
            Err(error) => failed(ran_as, format!("Could not run the script: {error:#}")),
        }
    }

    fn system_directory() -> anyhow::Result<PathBuf> {
        Ok(crate::win32::windows_directory()?.join("System32"))
    }

    fn interpreter(language: meshrmm_protocol::ScriptLanguage) -> anyhow::Result<PathBuf> {
        let system = system_directory()?;
        Ok(match language {
            meshrmm_protocol::ScriptLanguage::Powershell => system
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
            meshrmm_protocol::ScriptLanguage::Cmd => system.join("cmd.exe"),
            meshrmm_protocol::ScriptLanguage::Shell => {
                anyhow::bail!("shell scripts run only on Macs")
            }
        })
    }

    /// Text in the OEM code page, as console programs write it by default.
    fn oem_text(bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return String::new();
        }
        let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
        let length = unsafe { MultiByteToWideChar(CP_OEMCP, flags, bytes, None) };
        let Ok(length) = usize::try_from(length) else {
            return String::from_utf8_lossy(bytes).into_owned();
        };
        let mut text = vec![0_u16; length];
        let written = unsafe { MultiByteToWideChar(CP_OEMCP, flags, bytes, Some(&mut text)) };
        match usize::try_from(written) {
            Ok(written) if written > 0 => String::from_utf16_lossy(&text[..written]),
            _ => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    /// A signed-in user: their token, account and profile folder.
    struct UserLogon {
        token: OwnedHandle,
        name: String,
        sid: String,
        profile: PathBuf,
    }

    /// The user signed in to the console, or else to an active Remote
    /// Desktop session. `None` when nobody is, or when the Agent runs for
    /// development without the right to act for other users.
    fn signed_in_user(mode: ExecutionMode) -> Option<UserLogon> {
        if mode == ExecutionMode::Console {
            return None;
        }
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        let mut sessions = Vec::new();
        if console != u32::MAX {
            sessions.push(console);
        }
        sessions.extend(active_sessions().into_iter().filter(|&id| id != console));
        sessions.into_iter().find_map(|session| {
            let mut token = HANDLE::default();
            unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
            let token = OwnedHandle(token);
            match user_logon(token) {
                Ok(user) => Some(user),
                Err(error) => {
                    tracing::warn!(session, error = ?error, "could not read a signed-in user");
                    None
                }
            }
        })
    }

    /// Active sessions other than Session 0, which has no user.
    fn active_sessions() -> Vec<u32> {
        let mut sessions = std::ptr::null_mut::<WTS_SESSION_INFOW>();
        let mut count = 0;
        if unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count) }.is_err() {
            return Vec::new();
        }
        let active = unsafe { std::slice::from_raw_parts(sessions, count as usize) }
            .iter()
            .filter(|session| session.State == WTSActive && session.SessionId != 0)
            .map(|session| session.SessionId)
            .collect();
        unsafe { WTSFreeMemory(sessions.cast()) };
        active
    }

    fn user_logon(token: OwnedHandle) -> anyhow::Result<UserLogon> {
        let (name, sid) = token_account(&token)?;
        let mut length = 0;
        let _ = unsafe { GetUserProfileDirectoryW(token.0, None, &mut length) };
        let mut profile = vec![0_u16; length as usize];
        unsafe {
            GetUserProfileDirectoryW(token.0, Some(PWSTR(profile.as_mut_ptr())), &mut length)
        }
        .context("could not find the user's profile folder")?;
        let end = profile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(profile.len());
        Ok(UserLogon {
            token,
            name,
            sid,
            profile: PathBuf::from(OsString::from_wide(&profile[..end])),
        })
    }

    /// The token's account as `DOMAIN\name`, and its SID as a string.
    fn token_account(token: &OwnedHandle) -> anyhow::Result<(String, String)> {
        let mut length = 0;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
        anyhow::ensure!(length > 0, "Windows reported no token user");
        // u64 storage keeps the TOKEN_USER and its SID aligned.
        let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                length,
                &mut length,
            )
        }
        .context("could not read the token's user")?;
        let sid = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
        let mut name = vec![0_u16; 256];
        let mut domain = vec![0_u16; 256];
        let (mut name_length, mut domain_length) = (name.len() as u32, domain.len() as u32);
        let mut use_ = SID_NAME_USE::default();
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_length,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_length,
                &mut use_,
            )
        }
        .context("could not look up the token's account")?;
        let name = String::from_utf16_lossy(&name[..name_length as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_length as usize]);
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut text) }
            .context("could not read the token's SID")?;
        let sid_text = unsafe { text.to_string() };
        unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        let account = if domain.is_empty() {
            name
        } else {
            format!(r"{domain}\{name}")
        };
        Ok((account, sid_text?))
    }

    /// The Agent's own account, which SYSTEM scripts run as.
    pub(super) fn own_account() -> String {
        let mut token = HANDLE::default();
        let account = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(anyhow::Error::from)
            .and_then(|()| token_account(&OwnedHandle(token)).map(|(account, _)| account));
        account.unwrap_or_else(|_| r"NT AUTHORITY\SYSTEM".to_owned())
    }

    /// The directory a run's script file lives in, removed with it. Under the
    /// service it is an administrator-only folder in ProgramData, readable by
    /// the signed-in user when the script runs as them.
    struct ScriptDirectory(PathBuf);

    impl ScriptDirectory {
        fn create(
            mode: ExecutionMode,
            run_id: &str,
            user: Option<&UserLogon>,
        ) -> anyhow::Result<Self> {
            if mode == ExecutionMode::Console {
                let path = std::env::temp_dir().join("meshrmm-scripts").join(run_id);
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("could not create {}", path.display()))?;
                return Ok(Self(path));
            }
            let root = crate::installer::config_directory()?.join("scripts");
            crate::private_directory::secure(&root)?;
            let path = root.join(run_id);
            match user {
                Some(user) => crate::private_directory::create_new_readable_by(&path, &user.sid)?,
                None => crate::private_directory::create_new(&path)?,
            }
            Ok(Self(path))
        }
    }

    impl Drop for ScriptDirectory {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.0) {
                tracing::warn!(path = %self.0.display(), %error, "could not remove a toolbox script");
            }
        }
    }

    struct Outcome {
        exit_code: u32,
        timed_out: bool,
        stdout: Output,
        stderr: Output,
    }

    /// Runs `command_line` with `application`, as `user` or the Agent's own
    /// account, and collects its exit code and output. Its input is empty.
    /// The process and everything it starts are stopped after `timeout`.
    fn execute(
        application: &Path,
        command_line: &str,
        working_directory: &Path,
        user: Option<&UserLogon>,
        timeout: Duration,
    ) -> anyhow::Result<Outcome> {
        // Pipes start non-inheritable. Only the child's ends become
        // inheritable, and the handle list keeps a concurrent launch from
        // passing them on.
        let (child_input, parent_input) = create_pipe()?;
        let (parent_output, child_output) = create_pipe()?;
        let (parent_error, child_error) = create_pipe()?;
        drop(parent_input);
        let child_handles = [child_input.0, child_output.0, child_error.0];
        for handle in child_handles {
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
                .context("could not prepare the script's output pipes")?;
        }
        let handle_list = HandleListAttribute::new(&child_handles)?;
        let mut desktop = wide(r"winsta0\default");
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                // A user's script can show windows on their desktop.
                lpDesktop: if user.is_some() {
                    PWSTR(desktop.as_mut_ptr())
                } else {
                    PWSTR::null()
                },
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: child_input.0,
                hStdOutput: child_output.0,
                hStdError: child_error.0,
                ..Default::default()
            },
            lpAttributeList: handle_list.list(),
        };
        let mut environment = std::ptr::null_mut();
        if let Some(user) = user {
            unsafe { CreateEnvironmentBlock(&mut environment, Some(user.token.0), false) }
                .context("could not load the user's environment")?;
        }
        let application = wide(application.as_os_str());
        let mut command_line = wide(command_line);
        let working_directory = wide(working_directory.as_os_str());
        let mut flags = CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
        if !environment.is_null() {
            flags |= CREATE_UNICODE_ENVIRONMENT;
        }
        let mut information = PROCESS_INFORMATION::default();
        let launched = unsafe {
            match user {
                Some(user) => CreateProcessAsUserW(
                    Some(user.token.0),
                    PCWSTR(application.as_ptr()),
                    Some(PWSTR(command_line.as_mut_ptr())),
                    None,
                    None,
                    true,
                    flags,
                    (!environment.is_null()).then_some(environment.cast_const()),
                    PCWSTR(working_directory.as_ptr()),
                    &startup.StartupInfo,
                    &mut information,
                ),
                None => CreateProcessW(
                    PCWSTR(application.as_ptr()),
                    Some(PWSTR(command_line.as_mut_ptr())),
                    None,
                    None,
                    true,
                    flags,
                    None,
                    PCWSTR(working_directory.as_ptr()),
                    &startup.StartupInfo,
                    &mut information,
                ),
            }
        };
        // The child has its own copies now; close ours whether or not it started.
        drop(child_input);
        drop(child_output);
        drop(child_error);
        drop(handle_list);
        if !environment.is_null() {
            let _ = unsafe { DestroyEnvironmentBlock(environment) };
        }
        launched.context("Windows could not start the interpreter")?;
        let process = OwnedHandle(information.hProcess);
        let thread = OwnedHandle(information.hThread);
        // A job lets a timeout stop what the script started, too. It is not
        // closed with the job, so programs a finished script started keep
        // running.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .ok()
            .map(OwnedHandle)
            .filter(|job| unsafe { AssignProcessToJobObject(job.0, process.0) }.is_ok());
        unsafe { ResumeThread(thread.0) };
        drop(thread);

        let stdout = Arc::new(Mutex::new(Output::default()));
        let stderr = Arc::new(Mutex::new(Output::default()));
        let (drained_tx, drained) = mpsc::channel();
        for (pipe, output) in [(parent_output, &stdout), (parent_error, &stderr)] {
            let output = Arc::clone(output);
            let drained_tx = drained_tx.clone();
            std::thread::Builder::new()
                .name("meshrmm-toolbox-output".into())
                .spawn(move || {
                    let mut pipe = pipe.into_file();
                    let mut chunk = vec![0; 16 * 1024];
                    while let Ok(read) = pipe.read(&mut chunk) {
                        if read == 0 {
                            break;
                        }
                        output
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(&chunk[..read]);
                    }
                    let _ = drained_tx.send(());
                })
                .context("could not start a thread for the script's output")?;
        }
        drop(drained_tx);

        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        let timed_out = unsafe { WaitForSingleObject(process.0, milliseconds) } == WAIT_TIMEOUT;
        if timed_out {
            match &job {
                Some(job) => unsafe { TerminateJobObject(job.0, 1) },
                None => unsafe { TerminateProcess(process.0, 1) },
            }
            .context("could not stop the script after its timeout")?;
            unsafe { WaitForSingleObject(process.0, 5_000) };
        }
        let mut exit_code = 0;
        unsafe { GetExitCodeProcess(process.0, &mut exit_code) }
            .context("could not read the script's exit code")?;
        for _ in 0..2 {
            if drained.recv_timeout(OUTPUT_DRAIN).is_err() {
                break;
            }
        }
        let take = |output: &Arc<Mutex<Output>>| {
            std::mem::take(&mut *output.lock().unwrap_or_else(|error| error.into_inner()))
        };
        Ok(Outcome {
            exit_code,
            timed_out,
            stdout: take(&stdout),
            stderr: take(&stderr),
        })
    }

    /// Downloads, checks and saves `delivery`'s file, and returns where it went.
    pub(super) fn save_delivery(
        config: &Config,
        mode: ExecutionMode,
        delivery: &FileDeliveryRequest,
    ) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            valid_id(&delivery.delivery_id),
            "the delivery has an invalid ID"
        );
        anyhow::ensure!(
            meshrmm_protocol::valid_file_name(&delivery.file_name),
            "the file name is not one Windows allows"
        );
        let staged = download(config, mode, delivery)?;
        let mut source = File::open(&staged.0)
            .with_context(|| format!("could not reopen {}", staged.0.display()))?;
        let user = signed_in_user(mode);
        // The user's own Documents, unless they asked for the background
        // desktop's or nobody is signed in.
        let (documents, owner) = match (&user, delivery.destination) {
            (Some(user), FileDeliveryDestination::User) => (
                known_folder(&FOLDERID_Documents, Some(&user.token))?,
                Some(user),
            ),
            (user, _) => (
                known_folder(&FOLDERID_PublicDocuments, None)?,
                user.as_ref(),
            ),
        };
        let _impersonation = owner.map(Impersonation::start).transpose()?;
        // Where the file must end up, with any redirection of Documents
        // itself resolved. A redirected folder may be a share only the user
        // can reach, so this is done as them.
        let expected = std::fs::canonicalize(&documents)
            .with_context(|| format!("could not resolve {}", documents.display()))?
            .join(meshrmm_file_transfer::TRANSFER_FOLDER);
        let folder = documents.join(meshrmm_file_transfer::TRANSFER_FOLDER);
        std::fs::create_dir_all(&folder)
            .with_context(|| format!("could not create {}", folder.display()))?;
        // A folder anyone can change could point elsewhere; never follow it.
        let metadata = std::fs::symlink_metadata(&folder)
            .with_context(|| format!("could not inspect {}", folder.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink() && !is_reparse_point(&metadata),
            "{} is not a plain folder",
            folder.display()
        );
        let name = unique_name(&delivery.file_name, |candidate| {
            std::fs::symlink_metadata(folder.join(candidate)).is_ok()
        })
        .context("the transfer folder already has too many files with this name")?;
        let path = folder.join(&name);
        let mut destination = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        // The transfer folder could have been swapped for a link after the
        // check above. The open file says where it really is.
        let landed = final_path(&destination);
        if !landed
            .as_ref()
            .is_ok_and(|landed| same_path(landed, &expected.join(&name)))
        {
            drop(destination);
            if let Ok(landed) = &landed {
                let _ = std::fs::remove_file(landed);
            }
            anyhow::bail!("{} is not a plain folder", folder.display());
        }
        let copied = std::io::copy(&mut source, &mut destination)
            .and_then(|copied| destination.flush().map(|()| copied));
        if let Err(error) = copied {
            drop(destination);
            let _ = std::fs::remove_file(&path);
            return Err(error).with_context(|| format!("could not write {}", path.display()));
        }
        Ok(path)
    }

    /// Where an open file really is, through any links on its path.
    fn final_path(file: &File) -> anyhow::Result<PathBuf> {
        use std::os::windows::io::AsRawHandle;
        let mut buffer = vec![0_u16; 1024];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(file.as_raw_handle()),
                &mut buffer,
                FILE_NAME_NORMALIZED,
            )
        } as usize;
        anyhow::ensure!(
            length > 0 && length < buffer.len(),
            "Windows did not say where the file was saved"
        );
        Ok(PathBuf::from(OsString::from_wide(&buffer[..length])))
    }

    /// Whether two paths name the same place, as Windows compares names.
    fn same_path(left: &Path, right: &Path) -> bool {
        let text = |path: &Path| path.as_os_str().to_string_lossy().to_lowercase();
        text(left) == text(right)
    }

    fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    /// Whether this device runs `language`'s scripts.
    pub(super) fn runs(language: meshrmm_protocol::ScriptLanguage) -> bool {
        matches!(
            language,
            meshrmm_protocol::ScriptLanguage::Powershell | meshrmm_protocol::ScriptLanguage::Cmd
        )
    }

    /// Where downloads wait to be saved: an administrator-only folder.
    pub(super) fn staging_folder(mode: ExecutionMode) -> anyhow::Result<PathBuf> {
        if mode == ExecutionMode::Console {
            let folder = std::env::temp_dir().join("meshrmm-deliveries");
            std::fs::create_dir_all(&folder)?;
            return Ok(folder);
        }
        let folder = crate::installer::config_directory()?.join("deliveries");
        crate::private_directory::secure(&folder)?;
        Ok(folder)
    }

    /// A known folder, for `token`'s user or for the computer.
    fn known_folder(folder: &GUID, token: Option<&OwnedHandle>) -> anyhow::Result<PathBuf> {
        let path =
            unsafe { SHGetKnownFolderPath(folder, KF_FLAG_CREATE, token.map(|token| token.0)) }
                .context("Windows did not provide the Documents folder")?;
        let value = OsString::from_wide(unsafe { path.as_wide() });
        unsafe { CoTaskMemFree(Some(path.0.cast_const().cast())) };
        anyhow::ensure!(
            !value.is_empty(),
            "Windows did not provide the Documents folder"
        );
        Ok(PathBuf::from(value))
    }

    /// Acts as a signed-in user on this thread until dropped.
    struct Impersonation;

    impl Impersonation {
        fn start(user: &UserLogon) -> anyhow::Result<Self> {
            unsafe { ImpersonateLoggedOnUser(user.token.0) }
                .context("could not act as the signed-in user")?;
            Ok(Self)
        }
    }

    impl Drop for Impersonation {
        fn drop(&mut self) {
            if let Err(error) = unsafe { RevertToSelf() } {
                // A thread that cannot stop acting as the user must not do
                // anything else as the Agent.
                tracing::error!(%error, "could not stop acting as the signed-in user");
                std::process::abort();
            }
        }
    }
}

/// Scripts and files on a Mac. Scripts run with zsh, as root or as the user
/// signed in on the console, in their own process group so a timeout stops
/// everything they started. Files go to the user's Documents transfer folder,
/// written as the user, or to the shared folder when nobody is signed in.
#[cfg(target_os = "macos")]
mod macos {
    use std::io::{Read, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use meshrmm_protocol::{
        FileDeliveryDestination, FileDeliveryRequest, RunAs, ScriptLanguage, ScriptRunReport,
        ScriptRunRequest, ScriptRunStatus,
    };

    use super::{Output, decode_output, download, failed, script_file, unique_name, valid_id};
    use crate::remote::config::{Config, ExecutionMode};

    /// How long output readers may outlast the script. Programs the script
    /// started in the background can hold its output open indefinitely.
    const OUTPUT_DRAIN: Duration = Duration::from_secs(2);
    const PATH: &str = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";
    const SHARED_FOLDER: &str = "/Users/Shared";

    /// The user signed in on the console, for an Agent running as root.
    struct User {
        uid: u32,
        gid: u32,
        name: String,
        home: PathBuf,
    }

    /// A new folder holding one run's script, readable only by the account
    /// the script runs as, and removed with it.
    struct ScriptDirectory(PathBuf);

    impl ScriptDirectory {
        fn create(run_id: &str, user: Option<&User>) -> anyhow::Result<Self> {
            // Only root can reach into root's own temporary folder. In the
            // shared one, the sticky bit keeps others from replacing the
            // root-created folder before it is given to the user.
            let parent = match user {
                Some(_) => PathBuf::from("/private/tmp"),
                None => std::env::temp_dir(),
            };
            let path = parent.join(format!("meshrmm-script-{run_id}"));
            // A folder that already exists could be anyone's.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .with_context(|| format!("could not create {}", path.display()))?;
            let directory = Self(path);
            if let Some(user) = user {
                std::os::unix::fs::chown(&directory.0, Some(user.uid), Some(user.gid))?;
            }
            Ok(directory)
        }

        /// Writes the script. The folder is new and private, so the file
        /// cannot already exist; it is the user's own if the folder is.
        fn write(&self, run: &ScriptRunRequest) -> anyhow::Result<PathBuf> {
            let (name, contents) = script_file(run.language, &run.body);
            let path = self.0.join(name);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            file.write_all(&contents)?;
            let metadata = std::fs::metadata(&self.0)?;
            std::os::unix::fs::fchown(&file, Some(metadata.uid()), Some(metadata.gid()))?;
            Ok(path)
        }
    }

    impl Drop for ScriptDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn runs(language: ScriptLanguage) -> bool {
        language == ScriptLanguage::Shell
    }

    fn running_as_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    fn signed_in_user(mode: ExecutionMode) -> Option<User> {
        // A console Agent already runs as the user.
        if mode == ExecutionMode::Console || !running_as_root() {
            return None;
        }
        let uid = crate::remote::macos::session_close::console_user()?;
        // SAFETY: getpwuid returns static storage or null.
        let entry = unsafe { libc::getpwuid(uid).as_ref() }?;
        // SAFETY: a password entry's name and home are NUL-terminated.
        let text = |field| {
            unsafe { std::ffi::CStr::from_ptr(field) }
                .to_string_lossy()
                .into_owned()
        };
        Some(User {
            uid,
            gid: entry.pw_gid,
            name: text(entry.pw_name),
            home: PathBuf::from(text(entry.pw_dir)),
        })
    }

    fn user_name(uid: u32) -> String {
        // SAFETY: getpwuid returns static storage or null.
        let entry = unsafe { libc::getpwuid(uid) };
        // SAFETY: a non-null entry has a NUL-terminated name.
        unsafe { entry.as_ref() }
            .map(|entry| {
                unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| format!("user {uid}"))
    }

    pub(super) fn own_account() -> String {
        // SAFETY: geteuid has no preconditions.
        user_name(unsafe { libc::geteuid() })
    }

    /// A command that runs as `user` in their login session.
    fn as_user(user: &User, program: &str) -> Command {
        let mut command = Command::new("/bin/launchctl");
        command.args([
            "asuser",
            &user.uid.to_string(),
            "/usr/bin/sudo",
            "-u",
            &format!("#{}", user.uid),
            "-H",
            program,
        ]);
        command
    }

    pub(super) fn execute_script(mode: ExecutionMode, run: &ScriptRunRequest) -> ScriptRunReport {
        let user = match run.run_as {
            RunAs::User => signed_in_user(mode),
            RunAs::System => None,
        };
        let ran_as = user
            .as_ref()
            .map_or_else(own_account, |user| user.name.clone());
        if !valid_id(&run.run_id) {
            return failed(ran_as, "The run has an invalid ID.");
        }
        let directory = match ScriptDirectory::create(&run.run_id, user.as_ref()) {
            Ok(directory) => directory,
            Err(error) => {
                return failed(ran_as, format!("Could not prepare the script: {error:#}"));
            }
        };
        let script = match directory.write(run) {
            Ok(script) => script,
            Err(error) => return failed(ran_as, format!("Could not write the script: {error:#}")),
        };
        let (mut command, working_directory) = match &user {
            Some(user) => (as_user(user, "/bin/zsh"), user.home.clone()),
            None => (Command::new("/bin/zsh"), PathBuf::from("/")),
        };
        command
            .arg(&script)
            .current_dir(working_directory)
            .env("PATH", PATH)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        match execute(command, run) {
            Ok((status, exit_code, stdout, stderr, truncated)) => ScriptRunReport {
                status,
                ran_as,
                exit_code,
                stdout,
                stderr,
                output_truncated: truncated,
                error: (status == ScriptRunStatus::TimedOut).then(|| {
                    format!(
                        "The script was stopped after {} seconds.",
                        run.timeout_seconds
                    )
                }),
            },
            Err(error) => failed(ran_as, format!("Could not run the script: {error:#}")),
        }
    }

    type Outcome = (ScriptRunStatus, Option<i32>, String, String, bool);

    fn execute(mut command: Command, run: &ScriptRunRequest) -> anyhow::Result<Outcome> {
        let mut child = command.spawn().context("could not start zsh")?;
        let stdout = Arc::new(Mutex::new(Output::default()));
        let stderr = Arc::new(Mutex::new(Output::default()));
        let (drained_tx, drained) = mpsc::channel();
        for (pipe, output) in [
            (
                child
                    .stdout
                    .take()
                    .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
                &stdout,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
                &stderr,
            ),
        ] {
            let Some(mut pipe) = pipe else { continue };
            let output = Arc::clone(output);
            let drained_tx = drained_tx.clone();
            std::thread::spawn(move || {
                let mut chunk = [0; 16 * 1024];
                while let Ok(read) = pipe.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    output
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(&chunk[..read]);
                }
                let _ = drained_tx.send(());
            });
        }
        drop(drained_tx);
        let deadline = Instant::now() + Duration::from_secs(run.timeout_seconds.into());
        let pid = child.id() as libc::pid_t;
        let (status, exit_code) = loop {
            if let Some(exit) = child.try_wait()? {
                break (ScriptRunStatus::Completed, exit.code());
            }
            if Instant::now() >= deadline {
                // SAFETY: the script leads its own process group.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.wait();
                break (ScriptRunStatus::TimedOut, None);
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // Background programs the script left behind can keep its output open.
        for _ in 0..2 {
            if drained.recv_timeout(OUTPUT_DRAIN).is_err() {
                break;
            }
        }
        let take = |output: &Arc<Mutex<Output>>| {
            let output = std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner()));
            let text = decode_output(&output.bytes, |bytes| {
                String::from_utf8_lossy(bytes).into_owned()
            });
            (text, output.truncated)
        };
        let (stdout, stdout_truncated) = take(&stdout);
        let (stderr, stderr_truncated) = take(&stderr);
        Ok((
            status,
            exit_code,
            stdout,
            stderr,
            stdout_truncated || stderr_truncated,
        ))
    }

    /// Where downloads wait to be saved: a folder only the Agent's account can read.
    pub(super) fn staging_folder(mode: ExecutionMode) -> anyhow::Result<PathBuf> {
        let folder = if mode == ExecutionMode::Console {
            std::env::temp_dir().join("meshrmm-deliveries")
        } else {
            crate::installer::config_directory()?.join("deliveries")
        };
        std::fs::create_dir_all(&folder)?;
        Ok(folder)
    }

    pub(super) fn save_delivery(
        config: &Config,
        mode: ExecutionMode,
        delivery: &FileDeliveryRequest,
    ) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            valid_id(&delivery.delivery_id),
            "the delivery has an invalid ID"
        );
        anyhow::ensure!(
            meshrmm_protocol::valid_file_name(&delivery.file_name),
            "the file name is not one the dashboard allows"
        );
        let staged = download(config, mode, delivery)?;
        let user = signed_in_user(mode);
        let documents = match (&user, delivery.destination) {
            (Some(user), FileDeliveryDestination::User) => user.home.join("Documents"),
            (None, FileDeliveryDestination::User) if !running_as_root() => {
                PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                    .join("Documents")
            }
            _ => PathBuf::from(SHARED_FOLDER),
        };
        let folder = documents.join(meshrmm_file_transfer::TRANSFER_FOLDER);
        match &user {
            // Written as the user, so a link they control cannot redirect a
            // root write.
            Some(user) if delivery.destination == FileDeliveryDestination::User => {
                let status = as_user(user, "/bin/mkdir")
                    .arg("-p")
                    .arg(&folder)
                    .status()
                    .context("could not create the transfer folder")?;
                anyhow::ensure!(status.success(), "could not create {}", folder.display());
                let path = free_path(&folder, &delivery.file_name)?;
                let mut writer = as_user(user, "/bin/sh")
                    .args(["-c", "set -C && cat > \"$1\"", "sh"])
                    .arg(&path)
                    .stdin(Stdio::piped())
                    .spawn()
                    .context("could not save the file as the signed-in user")?;
                let mut source = std::fs::File::open(&staged.0)?;
                let copied = std::io::copy(&mut source, writer.stdin.as_mut().context("no input")?);
                drop(writer.stdin.take());
                let status = writer.wait()?;
                anyhow::ensure!(
                    copied.is_ok() && status.success(),
                    "could not write {}",
                    path.display()
                );
                Ok(path)
            }
            _ => {
                std::fs::create_dir_all(&folder)
                    .with_context(|| format!("could not create {}", folder.display()))?;
                // /Users/Shared is open to everyone, but its sticky bit keeps
                // others from replacing a folder this account owns there.
                let metadata = std::fs::symlink_metadata(&folder)?;
                // SAFETY: geteuid has no preconditions.
                let owner = unsafe { libc::geteuid() };
                anyhow::ensure!(
                    metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == owner,
                    "{} is not a plain folder",
                    folder.display()
                );
                let path = free_path(&folder, &delivery.file_name)?;
                let mut destination = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .with_context(|| format!("could not create {}", path.display()))?;
                let mut source = std::fs::File::open(&staged.0)?;
                if let Err(error) = std::io::copy(&mut source, &mut destination) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error)
                        .with_context(|| format!("could not write {}", path.display()));
                }
                Ok(path)
            }
        }
    }

    fn free_path(folder: &Path, name: &str) -> anyhow::Result<PathBuf> {
        let name = unique_name(name, |candidate| {
            std::fs::symlink_metadata(folder.join(candidate)).is_ok()
        })
        .context("the transfer folder already has too many files with this name")?;
        Ok(folder.join(name))
    }
}

#[cfg(target_os = "macos")]
use self::macos as platform;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn run_here(body: &str, timeout_seconds: u32) -> meshrmm_protocol::ScriptRunReport {
        macos::execute_script(
            crate::remote::config::ExecutionMode::Console,
            &meshrmm_protocol::ScriptRunRequest {
                run_id: format!("test-{}-{timeout_seconds}", std::process::id()),
                language: ScriptLanguage::Shell,
                body: body.into(),
                run_as: meshrmm_protocol::RunAs::User,
                timeout_seconds,
            },
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_scripts_run_in_zsh_and_report_their_output() {
        let report = run_here(
            "print -r -- héllo\r\nread line || echo no input >&2\nexit 3\n",
            30,
        );
        assert_eq!(report.status, meshrmm_protocol::ScriptRunStatus::Completed);
        assert_eq!(report.exit_code, Some(3));
        assert_eq!(report.stdout, "héllo\n");
        assert_eq!(report.stderr, "no input\n", "the script gets no input");
        assert!(!report.ran_as.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_scripts_that_overrun_are_stopped_with_what_they_started() {
        let started = std::time::Instant::now();
        let report = run_here("sleep 60 &\necho started\nsleep 60\n", 1);
        assert_eq!(report.status, meshrmm_protocol::ScriptRunStatus::TimedOut);
        assert_eq!(report.stdout, "started\n");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the background sleep does not hold the output open"
        );
    }

    #[test]
    fn script_files_use_crlf_and_the_interpreters_encoding() {
        let (name, bytes) = script_file(ScriptLanguage::Powershell, "Write-Output 'é'\nexit 3");
        assert_eq!(name, "script.ps1");
        assert_eq!(
            bytes,
            "\u{feff}Write-Output 'é'\r\nexit 3".as_bytes(),
            "PowerShell 5.1 needs the byte-order mark to read UTF-8"
        );
        let (name, bytes) = script_file(ScriptLanguage::Cmd, "@echo off\r\necho hi\n");
        assert_eq!(name, "script.cmd");
        assert_eq!(bytes, b"@chcp 65001>nul\r\n@echo off\r\necho hi\r\n");
    }

    #[test]
    fn interpreters_get_the_script_quoted() {
        let powershell =
            interpreter_arguments(ScriptLanguage::Powershell, r"C:\Users\O'Brien\script.ps1");
        assert!(
            powershell.contains(r"& 'C:\Users\O''Brien\script.ps1'; exit $LASTEXITCODE"),
            "{powershell}"
        );
        assert!(
            powershell.starts_with(
                "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \""
            )
        );
        assert_eq!(
            interpreter_arguments(ScriptLanguage::Cmd, r"C:\Program Data\script.cmd"),
            r#"/D /S /C ""C:\Program Data\script.cmd"""#
        );
    }

    #[test]
    fn output_stops_at_the_limit() {
        let mut output = Output::default();
        output.push(&vec![b'a'; meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES - 1]);
        assert!(!output.truncated);
        output.push(b"bc");
        assert!(output.truncated);
        assert_eq!(
            output.bytes.len(),
            meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES
        );
        assert_eq!(output.bytes.last(), Some(&b'b'));
        output.push(b"d");
        assert_eq!(
            output.bytes.len(),
            meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES
        );
    }

    #[test]
    fn output_is_utf8_or_the_fallback() {
        let fallback = |_: &[u8]| "fallback".to_owned();
        assert_eq!(decode_output("héllo".as_bytes(), fallback), "héllo");
        assert_eq!(decode_output(b"\xef\xbb\xbfbom", fallback), "bom");
        // A character cut by the limit is dropped, not a reason to fall back.
        assert_eq!(decode_output(b"ab\xc3", fallback), "ab");
        assert_eq!(decode_output(b"a\x82b", fallback), "fallback");
    }

    #[test]
    fn saved_files_get_a_free_name() {
        let taken = ["setup.exe", "setup (2).exe", "notes"];
        let taken = |name: &str| taken.contains(&name);
        assert_eq!(
            unique_name("other.exe", taken).as_deref(),
            Some("other.exe")
        );
        assert_eq!(
            unique_name("setup.exe", taken).as_deref(),
            Some("setup (3).exe")
        );
        assert_eq!(unique_name("notes", taken).as_deref(), Some("notes (2)"));
        assert_eq!(
            unique_name(".profile", |_| false).as_deref(),
            Some(".profile")
        );
        assert_eq!(
            unique_name(".profile", |name| name == ".profile").as_deref(),
            Some(".profile (2)")
        );
        assert_eq!(unique_name("full", |_| true), None);
    }

    #[test]
    fn ids_cannot_leave_their_directory() {
        assert!(valid_id("0f8a3c52-6c1e-4f1a-9a9e-2b0c7f1d2e3a"));
        for id in ["", "..", "a/b", r"a\b", "a.b", &"a".repeat(65)] {
            assert!(!valid_id(id), "{id}");
        }
    }
}
