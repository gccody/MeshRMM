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
use windows::Win32::Globalization::{CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, ImpersonateLoggedOnUser, LookupAccountSidW, RevertToSelf, SID_NAME_USE,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
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
    unsafe { GetUserProfileDirectoryW(token.0, Some(PWSTR(profile.as_mut_ptr())), &mut length) }
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
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.context("could not read the token's SID")?;
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
    fn create(mode: ExecutionMode, run_id: &str, user: Option<&UserLogon>) -> anyhow::Result<Self> {
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
    let path = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_CREATE, token.map(|token| token.0)) }
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
