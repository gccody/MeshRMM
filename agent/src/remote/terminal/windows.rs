//! Shells on a Windows pseudo-console (ConPTY).
//!
//! The coordinator runs as LocalSystem in Session 0, so a SYSTEM shell starts
//! there; a signed-in user's starts with their limited token, environment
//! and profile folder, on their desktop, as their scripts do. The shell and
//! everything it starts share a job object that kills them all when the
//! terminal ends. Input reaches the pseudo-console as UTF-8, and its output
//! is UTF-8 with VT sequences, which the browser's terminal draws.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use meshrmm_protocol::{MAX_TERMINAL_OUTPUT_BYTES, RunAs, TerminalRequest, TerminalShell};
use tokio::sync::{mpsc, oneshot};
use windows::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, w};

use super::{Shell, channels, relay::Resize};
use crate::remote::config::ExecutionMode;
use crate::remote::toolbox::windows::{UserLogon, own_account, signed_in_user, system_directory};
use crate::win32::{OwnedHandle, create_pipe, wide};

/// A kernel handle the relay's task may share between threads.
struct Shared(OwnedHandle);

// SAFETY: kernel handles may be used from any thread.
unsafe impl Sync for Shared {}

/// A running shell's pseudo-console and job. Dropping it ends both, and
/// with them every process the shell started.
pub(in crate::remote) struct Process {
    console: Mutex<Option<isize>>,
    job: Shared,
}

impl Resize for Process {
    fn resize(&self, columns: u16, rows: u16) -> anyhow::Result<()> {
        let console = self.console.lock().unwrap_or_else(|e| e.into_inner());
        let console = console.context("the pseudo-console is closed")?;
        unsafe { ResizePseudoConsole(HPCON(console), size(columns, rows)) }
            .context("Windows could not resize the pseudo-console")
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = unsafe { TerminateJobObject(self.job.0.0, 1) };
        if let Some(console) = self
            .console
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            // Older Windows waits for the output to be read before closing,
            // which the output thread goes on doing until the pipe breaks.
            let closed = std::thread::Builder::new()
                .name("meshrmm-terminal-close".into())
                .spawn(move || unsafe { ClosePseudoConsole(HPCON(console)) });
            if let Err(error) = closed {
                tracing::warn!(%error, "could not close a pseudo-console");
            }
        }
    }
}

fn size(columns: u16, rows: u16) -> COORD {
    COORD {
        X: columns.min(i16::MAX as u16) as i16,
        Y: rows.min(i16::MAX as u16) as i16,
    }
}

/// PowerShell 7's executable: where its installer registered it, or its
/// default folder.
fn pwsh() -> Option<PathBuf> {
    let mut buffer = vec![0_u16; 1024];
    let mut length = (buffer.len() * 2) as u32;
    let registered = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\pwsh.exe"),
            PCWSTR::null(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut length),
        )
    }
    .is_ok()
    .then(|| {
        let units = (length as usize / 2).min(buffer.len());
        let end = buffer[..units]
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(units);
        PathBuf::from(String::from_utf16_lossy(&buffer[..end]).trim_matches('"'))
    });
    let folder = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(|folder| PathBuf::from(folder).join(r"PowerShell\7\pwsh.exe"));
    [registered, folder]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
}

/// The program and command line for `shell`, if it's installed.
fn command_line(shell: TerminalShell) -> anyhow::Result<Option<(PathBuf, String)>> {
    let system = system_directory()?;
    let (program, arguments) = match shell {
        TerminalShell::Powershell => (
            system.join(r"WindowsPowerShell\v1.0\powershell.exe"),
            " -NoLogo",
        ),
        TerminalShell::Cmd => (system.join("cmd.exe"), ""),
        TerminalShell::Pwsh => match pwsh() {
            Some(program) => (program, " -NoLogo"),
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    if !program.is_file() {
        return Ok(None);
    }
    let line = format!("\"{}\"{arguments}", program.display());
    Ok(Some((program, line)))
}

pub(super) fn available_shells() -> Vec<TerminalShell> {
    [
        TerminalShell::Powershell,
        TerminalShell::Cmd,
        TerminalShell::Pwsh,
    ]
    .into_iter()
    .filter(|&shell| command_line(shell).is_ok_and(|line| line.is_some()))
    .collect()
}

/// Starts `request`'s shell, or says why it couldn't.
pub(super) fn start(mode: ExecutionMode, request: &TerminalRequest) -> Result<Shell, String> {
    let user = match (mode, request.run_as) {
        // A development Agent has only its own account.
        (ExecutionMode::Console, _) | (_, RunAs::System) => None,
        (_, RunAs::User) => Some(signed_in_user(mode).ok_or_else(|| {
            "Nobody is signed in to this computer, so there is no user to open a terminal as."
                .to_owned()
        })?),
    };
    let (program, line) = match command_line(request.shell) {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Err(format!(
                "{} is not installed on this computer.",
                request.shell.label()
            ));
        }
        Err(error) => return Err(format!("The shell could not start: {error:#}")),
    };
    let directory = match &user {
        Some(user) => user.profile.clone(),
        None => system_directory().map_err(|error| format!("{error:#}"))?,
    };
    let account = user
        .as_ref()
        .map_or_else(own_account, |user| user.name.clone());
    spawn(
        &line,
        &directory,
        user.as_ref(),
        request.columns,
        request.rows,
    )
    .map(|(io, process)| Shell {
        account,
        program: program.display().to_string(),
        io,
        process,
    })
    .map_err(|error| format!("The shell could not start: {error:#}"))
}

/// A process attribute list holding a pseudo-console.
struct ConsoleAttribute {
    // u64 storage keeps the opaque list pointer-aligned.
    buffer: Vec<u64>,
}

impl ConsoleAttribute {
    fn new(console: HPCON) -> anyhow::Result<Self> {
        let mut size = 0;
        // The sizing call reports ERROR_INSUFFICIENT_BUFFER by design.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        anyhow::ensure!(size > 0, "Windows reported no process attribute list size");
        let mut buffer = vec![0_u64; size.div_ceil(std::mem::size_of::<u64>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
            .context("could not create a process attribute list")?;
        let attribute = Self { buffer };
        // The value is the pseudo-console handle itself.
        unsafe {
            UpdateProcThreadAttribute(
                attribute.list(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(console.0 as *const std::ffi::c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )
        }
        .context("could not attach the pseudo-console")?;
        Ok(attribute)
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_ptr().cast_mut().cast())
    }
}

impl Drop for ConsoleAttribute {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// A job that kills its processes when its last handle closes.
fn kill_on_close_job() -> anyhow::Result<OwnedHandle> {
    let job = OwnedHandle(
        unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("could not create a job")?,
    );
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("could not limit the job")?;
    Ok(job)
}

fn spawn(
    command_line: &str,
    directory: &Path,
    user: Option<&UserLogon>,
    columns: u16,
    rows: u16,
) -> anyhow::Result<(super::relay::Io, Process)> {
    let (console_input, input) = create_pipe()?;
    let (output, console_output) = create_pipe()?;
    let console =
        unsafe { CreatePseudoConsole(size(columns, rows), console_input.0, console_output.0, 0) }
            .context("Windows could not create a pseudo-console")?;
    // The pseudo-console has its own copies.
    drop(console_input);
    drop(console_output);
    let job = match kill_on_close_job() {
        Ok(job) => job,
        Err(error) => {
            unsafe { ClosePseudoConsole(console) };
            return Err(error);
        }
    };
    let process = Process {
        console: Mutex::new(Some(console.0)),
        job: Shared(job),
    };
    // From here on, dropping `process` closes the pseudo-console and job.
    let (child, thread) = create_suspended(command_line, directory, user, console)?;
    unsafe { AssignProcessToJobObject(process.job.0.0, child.0) }
        .context("could not put the shell in its job")?;
    unsafe { ResumeThread(thread.0) };
    drop(thread);

    let (io, ends) = channels();
    let super::ShellEnds {
        output: output_tx,
        input: input_rx,
        exited,
    } = ends;
    let output = output.into_file();
    let input = input.into_file();
    std::thread::Builder::new()
        .name("meshrmm-terminal-output".into())
        .spawn(move || read_output(output, output_tx))?;
    std::thread::Builder::new()
        .name("meshrmm-terminal-input".into())
        .spawn(move || write_input(input, input_rx))?;
    std::thread::Builder::new()
        .name("meshrmm-terminal-wait".into())
        .spawn(move || wait(child, exited))?;
    Ok((io, process))
}

/// Starts the shell suspended on `console`, and returns its process and main
/// thread.
fn create_suspended(
    command_line: &str,
    directory: &Path,
    user: Option<&UserLogon>,
    console: HPCON,
) -> anyhow::Result<(OwnedHandle, OwnedHandle)> {
    let attribute = ConsoleAttribute::new(console)?;
    let mut desktop = wide(r"winsta0\default");
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            lpDesktop: if user.is_some() {
                PWSTR(desktop.as_mut_ptr())
            } else {
                PWSTR::null()
            },
            // Without this, a child of a process whose standard handles are
            // pipes, as the coordinator's are, can use those instead of the
            // pseudo-console.
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: INVALID_HANDLE_VALUE,
            hStdOutput: INVALID_HANDLE_VALUE,
            hStdError: INVALID_HANDLE_VALUE,
            ..Default::default()
        },
        lpAttributeList: attribute.list(),
    };
    let mut environment = std::ptr::null_mut();
    if let Some(user) = user {
        unsafe { CreateEnvironmentBlock(&mut environment, Some(user.token.0), false) }
            .context("could not load the user's environment")?;
    }
    let mut command_line = wide(command_line);
    let directory = wide(directory.as_os_str());
    let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
    if !environment.is_null() {
        flags |= CREATE_UNICODE_ENVIRONMENT;
    }
    let mut information = PROCESS_INFORMATION::default();
    let launched = unsafe {
        match user {
            Some(user) => CreateProcessAsUserW(
                Some(user.token.0),
                PCWSTR::null(),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                (!environment.is_null()).then_some(environment.cast_const()),
                PCWSTR(directory.as_ptr()),
                &startup.StartupInfo,
                &mut information,
            ),
            None => CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                None,
                PCWSTR(directory.as_ptr()),
                &startup.StartupInfo,
                &mut information,
            ),
        }
    };
    if !environment.is_null() {
        let _ = unsafe { DestroyEnvironmentBlock(environment) };
    }
    drop(attribute);
    launched.context("Windows could not start the shell")?;
    Ok((
        OwnedHandle(information.hProcess),
        OwnedHandle(information.hThread),
    ))
}

/// Passes output on until the pseudo-console closes. Once nobody takes it,
/// it is read and dropped, because closing an older pseudo-console waits for
/// its output to be read.
fn read_output(mut pipe: File, output: mpsc::Sender<Vec<u8>>) {
    let mut chunk = vec![0; MAX_TERMINAL_OUTPUT_BYTES];
    let mut taken = true;
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if taken {
                    taken = output.blocking_send(chunk[..read].to_vec()).is_ok();
                }
            }
        }
    }
}

fn write_input(mut pipe: File, mut input: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = input.blocking_recv() {
        if pipe.write_all(&bytes).is_err() {
            break;
        }
    }
}

fn wait(process: OwnedHandle, exited: oneshot::Sender<Option<i32>>) {
    let mut code = None;
    if unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_OBJECT_0 {
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(process.0, &mut exit_code) }.is_ok() {
            code = Some(exit_code as i32);
        }
    }
    let _ = exited.send(code);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Runs `line` as this account and returns its output and exit code.
    fn run(line: &str, input: &[u8]) -> (String, Option<i32>) {
        let directory = system_directory().unwrap();
        let (mut io, process) = spawn(line, &directory, None, 80, 25).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            if !input.is_empty() {
                io.input.send(input.to_vec()).await.unwrap();
            }
            let mut output = Vec::new();
            let code = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    tokio::select! {
                        Some(chunk) = io.output.recv() => output.extend(chunk),
                        code = &mut io.exited => {
                            while let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_millis(300), io.output.recv()).await {
                                output.extend(chunk);
                            }
                            return code.unwrap();
                        }
                    }
                }
            })
            .await
            .expect("the shell didn't exit");
            drop(process);
            (String::from_utf8_lossy(&output).into_owned(), code)
        })
    }

    #[test]
    fn command_prompt_runs_on_a_pseudo_console() {
        let cmd = system_directory().unwrap().join("cmd.exe");
        let (output, code) = run(
            &format!("\"{}\" /d /c echo hello from conpty& exit 7", cmd.display()),
            b"",
        );
        assert!(output.contains("hello from conpty"), "{output:?}");
        assert_eq!(code, Some(7));
    }

    #[test]
    fn input_reaches_the_shell() {
        let cmd = system_directory().unwrap().join("cmd.exe");
        let (output, code) = run(
            &format!("\"{}\" /d /q /k prompt $G", cmd.display()),
            b"echo typed-%COMPUTERNAME%\r\nexit 3\r\n",
        );
        assert!(output.contains("typed-"), "{output:?}");
        assert_eq!(code, Some(3));
    }

    #[test]
    fn windows_offers_its_built_in_shells() {
        let shells = available_shells();
        assert!(shells.contains(&TerminalShell::Powershell));
        assert!(shells.contains(&TerminalShell::Cmd));
        assert!(command_line(TerminalShell::Zsh).unwrap().is_none());
    }
}
