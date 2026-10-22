//! Shells on a pseudo-terminal on macOS and Linux.
//!
//! The shell leads a new session with the pseudo-terminal as its controlling
//! terminal, starts as a login shell (`-zsh`), and gets a login environment.
//! As root it runs as root, or as the user signed in on the console: on Linux
//! with that user's groups and session bus, and on a Mac through `login`, so
//! it joins the user's login context. Ending a terminal hangs up its session
//! and, after a grace period, kills whatever in it is left. Programs that
//! start a session of their own, as daemons do, are left running.
use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use meshrmm_protocol::{MAX_TERMINAL_OUTPUT_BYTES, RunAs, TerminalRequest, TerminalShell};
use tokio::sync::{mpsc, oneshot};

use super::{Shell, channels, relay::Resize};
use crate::remote::config::ExecutionMode;

#[cfg(target_os = "macos")]
const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
#[cfg(target_os = "macos")]
const TIOCSWINSZ: libc::c_ulong = 0x8008_7467;
#[cfg(target_os = "linux")]
use libc::{TIOCSCTTY, TIOCSWINSZ};

#[cfg(target_os = "macos")]
const PATH: &str = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(target_os = "linux")]
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// How long a hung-up session's programs get to exit before they're killed.
const HANG_UP_GRACE: Duration = Duration::from_secs(2);
/// How often the reader and writer threads look for the terminal's end.
const POLL_INTERVAL_MS: libc::c_int = 250;

/// An account from the user database.
#[derive(Debug, Clone)]
struct Account {
    uid: u32,
    gid: u32,
    name: String,
    home: PathBuf,
    shell: PathBuf,
}

impl Account {
    fn of(uid: u32) -> Option<Self> {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the sizes given.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if status != 0 || found.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r filled in the entry, whose strings live in `buffer`.
        let entry = unsafe { entry.assume_init() };
        let text = |field: *const libc::c_char| {
            if field.is_null() {
                OsString::new()
            } else {
                // SAFETY: the entry's strings are NUL-terminated.
                OsString::from(std::ffi::OsStr::from_bytes(
                    unsafe { CStr::from_ptr(field) }.to_bytes(),
                ))
            }
        };
        Some(Self {
            uid,
            gid: entry.pw_gid,
            name: text(entry.pw_name).to_string_lossy().into_owned(),
            home: PathBuf::from(text(entry.pw_dir)),
            shell: PathBuf::from(text(entry.pw_shell)),
        })
    }

    /// The account's supplementary groups, its primary group included.
    #[cfg(target_os = "linux")]
    fn groups(&self) -> Vec<libc::gid_t> {
        let mut groups = vec![0 as libc::gid_t; 256];
        let mut count = groups.len() as libc::c_int;
        if let Ok(name) = CString::new(self.name.clone()) {
            // SAFETY: the list holds `count` entries.
            if unsafe {
                libc::getgrouplist(name.as_ptr(), self.gid, groups.as_mut_ptr(), &mut count)
            } < 0
            {
                count = 0;
            }
        } else {
            count = 0;
        }
        groups.truncate(count.max(0) as usize);
        if groups.is_empty() {
            groups.push(self.gid);
        }
        groups
    }

    /// The home folder, or `/` when it's gone.
    fn directory(&self) -> PathBuf {
        if self.home.is_dir() {
            self.home.clone()
        } else {
            PathBuf::from("/")
        }
    }
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The user signed in on the console.
fn console_user() -> Option<u32> {
    #[cfg(target_os = "macos")]
    return crate::remote::macos::session_close::console_user();
    #[cfg(target_os = "linux")]
    return crate::remote::linux::session_close::console_user();
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Where a named shell is, if it's installed.
fn find(shell: TerminalShell) -> Option<PathBuf> {
    let name = match shell {
        TerminalShell::Bash => "bash",
        TerminalShell::Zsh => "zsh",
        TerminalShell::Sh => "sh",
        _ => return None,
    };
    let mut folders = vec!["/bin", "/usr/bin"];
    if cfg!(target_os = "macos") && shell != TerminalShell::Sh {
        folders.extend(["/usr/local/bin", "/opt/homebrew/bin"]);
    }
    folders
        .into_iter()
        .map(|folder| Path::new(folder).join(name))
        .find(|path| executable(path))
}

/// The program that runs `shell` for `account`.
fn resolve(shell: TerminalShell, account: &Account) -> Option<PathBuf> {
    match shell {
        TerminalShell::Login if executable(&account.shell) => Some(account.shell.clone()),
        TerminalShell::Login => find(TerminalShell::Sh),
        shell => find(shell),
    }
}

pub(super) fn available_shells() -> Vec<TerminalShell> {
    let mut shells = vec![TerminalShell::Login];
    shells.extend(
        [TerminalShell::Bash, TerminalShell::Zsh, TerminalShell::Sh]
            .into_iter()
            .filter(|&shell| find(shell).is_some()),
    );
    shells
}

/// How a shell is started.
#[derive(Debug)]
struct Launch {
    program: PathBuf,
    /// The name the program sees as its own; `-zsh` makes a login shell.
    argv0: Option<OsString>,
    arguments: Vec<OsString>,
    environment: Vec<(&'static str, OsString)>,
    directory: PathBuf,
    /// The account to switch to before running the program, with its
    /// groups.
    switch_to: Option<(u32, u32, Vec<libc::gid_t>)>,
    /// What the browser is told it runs as, and in.
    account: String,
    shell: PathBuf,
}

/// `-name`, which tells a shell it's a login shell.
fn login_name(shell: &Path) -> OsString {
    let mut name = OsString::from("-");
    name.push(shell.file_name().unwrap_or(shell.as_os_str()));
    name
}

fn language() -> OsString {
    std::env::var_os("LANG")
        .filter(|language| !language.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "en_US.UTF-8".into()
            } else {
                "C.UTF-8".into()
            }
        })
}

/// The login environment of `account` running `shell`.
fn environment(account: &Account, shell: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("HOME", account.home.clone().into()),
        ("USER", account.name.clone().into()),
        ("LOGNAME", account.name.clone().into()),
        ("SHELL", shell.into()),
        ("TERM", "xterm-256color".into()),
        ("COLORTERM", "truecolor".into()),
        ("LANG", language()),
        ("PATH", PATH.into()),
    ]
}

/// How to start `request`'s shell, or why it can't start.
fn plan(mode: ExecutionMode, request: &TerminalRequest) -> Result<Launch, String> {
    let root = running_as_root() && mode != ExecutionMode::Console;
    // SAFETY: geteuid has no preconditions.
    let own = unsafe { libc::geteuid() };
    let (uid, as_user) = match (root, request.run_as) {
        // A development Agent has only its own account.
        (false, _) => (own, false),
        (true, RunAs::System) => (0, false),
        (true, RunAs::User) => match console_user() {
            Some(uid) => (uid, true),
            None => {
                return Err(
                    "Nobody is signed in to this computer, so there is no user to open a terminal as."
                        .into(),
                );
            }
        },
    };
    let account = Account::of(uid)
        .ok_or_else(|| format!("The account with user ID {uid} is not in the user database."))?;
    let shell = resolve(request.shell, &account).ok_or_else(|| {
        format!(
            "{} is not installed on this computer.",
            request.shell.label()
        )
    })?;
    let mut environment = environment(&account, &shell);
    if as_user && cfg!(target_os = "macos") {
        return Ok(mac_user_launch(&account, request.shell, shell, environment));
    }
    let switch_to = if as_user {
        let runtime = format!("/run/user/{}", account.uid);
        environment.push((
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus").into(),
        ));
        environment.push(("XDG_RUNTIME_DIR", runtime.into()));
        #[cfg(target_os = "linux")]
        let groups = account.groups();
        #[cfg(not(target_os = "linux"))]
        let groups = vec![account.gid];
        Some((account.uid, account.gid, groups))
    } else {
        None
    };
    Ok(Launch {
        argv0: Some(login_name(&shell)),
        program: shell.clone(),
        arguments: Vec::new(),
        environment,
        directory: account.directory(),
        switch_to,
        account: account.name,
        shell,
    })
}

/// A Mac user's shell, started by `login` so it joins their login context
/// (`pam_launchd` moves it into their launchd domain, which `open`, the
/// pasteboard and the keychain need). Root may skip the password with `-f`;
/// `-p` keeps the environment, and `-q` the last-login message. Given a
/// program, `login` runs it as a login shell; without one, it runs the
/// account's own.
fn mac_user_launch(
    account: &Account,
    requested: TerminalShell,
    shell: PathBuf,
    environment: Vec<(&'static str, OsString)>,
) -> Launch {
    let mut arguments: Vec<OsString> = vec!["-pfq".into(), account.name.clone().into()];
    if requested != TerminalShell::Login {
        arguments.push(shell.clone().into());
    }
    Launch {
        program: PathBuf::from("/usr/bin/login"),
        argv0: None,
        arguments,
        environment,
        directory: account.directory(),
        switch_to: None,
        account: account.name.clone(),
        shell,
    }
}

/// Starts `request`'s shell, or says why it couldn't.
pub(super) fn start(mode: ExecutionMode, request: &TerminalRequest) -> Result<Shell, String> {
    let launch = plan(mode, request)?;
    spawn(launch, request.columns, request.rows)
        .map_err(|error| format!("The shell could not start: {error:#}"))
}

/// A pseudo-terminal's two ends.
struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn open_pty(columns: u16, rows: u16) -> std::io::Result<Pty> {
    // SAFETY: plain system calls on descriptors this function owns.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(last_error());
        }
        let master = OwnedFd::from_raw_fd(master);
        let fd = master.as_raw_fd();
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
            || libc::grantpt(fd) < 0
            || libc::unlockpt(fd) < 0
        {
            return Err(last_error());
        }
        let name = slave_name(fd)?;
        let slave = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        if slave < 0 {
            return Err(last_error());
        }
        let slave = OwnedFd::from_raw_fd(slave);
        resize(fd, columns, rows)?;
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(last_error());
        }
        Ok(Pty { master, slave })
    }
}

#[cfg(target_os = "linux")]
fn slave_name(master: RawFd) -> std::io::Result<CString> {
    let mut name = vec![0 as libc::c_char; 128];
    // SAFETY: the buffer holds as many bytes as passed.
    let status = unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    // SAFETY: ptsname_r wrote a NUL-terminated name.
    Ok(unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned())
}

#[cfg(target_os = "macos")]
fn slave_name(master: RawFd) -> std::io::Result<CString> {
    // ptsname returns static storage, so callers take turns.
    static PTSNAME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = PTSNAME.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the name is copied before the lock is released.
    let name = unsafe { libc::ptsname(master) };
    if name.is_null() {
        return Err(last_error());
    }
    // SAFETY: ptsname returned a NUL-terminated name.
    Ok(unsafe { CStr::from_ptr(name) }.to_owned())
}

fn resize(master: RawFd, columns: u16, rows: u16) -> std::io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads a winsize.
    if unsafe { libc::ioctl(master, TIOCSWINSZ as _, &size) } < 0 {
        return Err(last_error());
    }
    Ok(())
}

/// A running shell's session. Dropping it ends the session.
pub(in crate::remote) struct Process {
    session: libc::pid_t,
    master: File,
    stopped: Arc<AtomicBool>,
}

impl Resize for Process {
    fn resize(&self, columns: u16, rows: u16) -> anyhow::Result<()> {
        // The kernel tells the foreground programs with SIGWINCH.
        Ok(resize(self.master.as_raw_fd(), columns, rows)?)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let session = self.session;
        let ended = std::thread::Builder::new()
            .name("meshrmm-terminal-end".into())
            .spawn(move || end_session(session));
        if ended.is_err() {
            signal_session(session, libc::SIGKILL);
        }
    }
}

fn spawn(launch: Launch, columns: u16, rows: u16) -> anyhow::Result<Shell> {
    use anyhow::Context;

    let Pty { master, slave } = open_pty(columns, rows).context("no pseudo-terminal")?;
    let mut command = Command::new(&launch.program);
    if let Some(argv0) = &launch.argv0 {
        command.arg0(argv0);
    }
    command
        .args(&launch.arguments)
        .env_clear()
        .envs(
            launch
                .environment
                .iter()
                .map(|(name, value)| (*name, value)),
        )
        .current_dir(&launch.directory)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    let switch_to = launch.switch_to;
    // SAFETY: the closure neither allocates nor takes locks; it makes only
    // async-signal-safe system calls.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(0, TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some((uid, gid, groups)) = &switch_to
                && (libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                    || libc::setgid(*gid) != 0
                    || libc::setuid(*uid) != 0)
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {}", launch.program.display()))?;
    // The child has its own copies of the slave now.
    drop(command);
    let session = child.id() as libc::pid_t;
    let master = File::from(master);
    let stopped = Arc::new(AtomicBool::new(false));
    let (io, ends) = channels();
    let super::ShellEnds {
        output,
        input,
        exited,
    } = ends;
    let started = (|| -> std::io::Result<()> {
        let reader = master.try_clone()?;
        let writer = master.try_clone()?;
        let stop = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("meshrmm-terminal-output".into())
            .spawn(move || read_output(reader, output, &stop))?;
        let stop = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("meshrmm-terminal-input".into())
            .spawn(move || write_input(writer, input, &stop))?;
        std::thread::Builder::new()
            .name("meshrmm-terminal-wait".into())
            .spawn(move || wait(&mut child, exited))?;
        Ok(())
    })();
    let process = Process {
        session,
        master,
        stopped,
    };
    // Dropping the process ends the shell.
    started.context("could not start the terminal's threads")?;
    Ok(Shell {
        account: launch.account,
        program: launch.shell.display().to_string(),
        io,
        process,
    })
}

/// Waits up to [`POLL_INTERVAL_MS`] for `events` on `fd`. Returns whether
/// they came.
fn ready(fd: RawFd, events: libc::c_short) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut poll, 1, POLL_INTERVAL_MS) > 0 }
}

fn read_output(mut master: File, output: mpsc::Sender<Vec<u8>>, stopped: &AtomicBool) {
    let mut chunk = vec![0; MAX_TERMINAL_OUTPUT_BYTES];
    while !stopped.load(Ordering::SeqCst) {
        if !ready(master.as_raw_fd(), libc::POLLIN) {
            continue;
        }
        match master.read(&mut chunk) {
            // Every copy of the slave is closed.
            Ok(0) => break,
            Ok(read) => {
                if output.blocking_send(chunk[..read].to_vec()).is_err() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            // EIO once the slave is closed, on Linux.
            Err(_) => break,
        }
    }
}

fn write_input(mut master: File, mut input: mpsc::Receiver<Vec<u8>>, stopped: &AtomicBool) {
    while let Some(bytes) = input.blocking_recv() {
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            if stopped.load(Ordering::SeqCst) {
                return;
            }
            match master.write(rest) {
                Ok(written) => rest = &rest[written..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ready(master.as_raw_fd(), libc::POLLOUT);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }
}

fn wait(child: &mut std::process::Child, exited: oneshot::Sender<Option<i32>>) {
    let code = child.wait().ok().and_then(|status| status.code());
    let _ = exited.send(code);
}

/// Every process.
#[cfg(target_os = "linux")]
fn processes() -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<libc::pid_t> {
    // SAFETY: a null buffer asks for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; count + 64];
    // SAFETY: the buffer holds as many bytes as passed.
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    pids.truncate(usize::try_from(count).unwrap_or(0));
    pids
}

/// The processes in `session`.
fn members(session: libc::pid_t) -> Vec<libc::pid_t> {
    processes()
        .into_iter()
        // SAFETY: getsid has no preconditions.
        .filter(|&pid| pid > 1 && unsafe { libc::getsid(pid) } == session)
        .collect()
}

fn signal_session(session: libc::pid_t, signal: libc::c_int) -> usize {
    let members = members(session);
    for &pid in &members {
        // SAFETY: kill has no memory preconditions; `pid` is in the session.
        unsafe { libc::kill(pid, signal) };
    }
    members.len()
}

/// Hangs up `session` as closing a terminal does, then kills what is left
/// of it once [`HANG_UP_GRACE`] passes.
fn end_session(session: libc::pid_t) {
    if session <= 1 {
        return;
    }
    if signal_session(session, libc::SIGHUP) == 0 {
        return;
    }
    signal_session(session, libc::SIGCONT);
    let deadline = Instant::now() + HANG_UP_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if members(session).is_empty() {
            return;
        }
    }
    signal_session(session, libc::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_account() -> Account {
        // SAFETY: geteuid has no preconditions.
        Account::of(unsafe { libc::geteuid() }).unwrap()
    }

    fn sh(script: &str) -> Launch {
        let account = own_account();
        let shell = PathBuf::from("/bin/sh");
        Launch {
            program: shell.clone(),
            argv0: None,
            arguments: vec!["-c".into(), script.into()],
            environment: environment(&account, &shell),
            directory: account.directory(),
            switch_to: None,
            account: account.name,
            shell,
        }
    }

    /// Everything the shell writes until it exits, and its exit code.
    fn run(shell: Shell, input: &[u8]) -> (String, Option<i32>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let Shell {
            io: mut terminal,
            process,
            ..
        } = shell;
        runtime.block_on(async {
            if !input.is_empty() {
                terminal.input.send(input.to_vec()).await.unwrap();
            }
            let mut output = Vec::new();
            let code = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    tokio::select! {
                        chunk = terminal.output.recv() => match chunk {
                            Some(chunk) => output.extend(chunk),
                            None => break,
                        },
                        code = &mut terminal.exited => {
                            let code = code.unwrap();
                            // What's left in the pseudo-terminal.
                            while let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_millis(200), terminal.output.recv()).await {
                                output.extend(chunk);
                            }
                            return code;
                        }
                    }
                }
                (&mut terminal.exited).await.unwrap()
            })
            .await
            .expect("the shell didn't exit");
            drop(process);
            (String::from_utf8_lossy(&output).into_owned(), code)
        })
    }

    #[test]
    fn shells_run_on_a_pty_and_report_their_exit_code() {
        let shell = spawn(
            sh("test -t 0 && test -t 1 && echo \"tty $TERM $LANG\"; exit 7"),
            80,
            24,
        )
        .unwrap();
        assert_eq!(shell.program, "/bin/sh");
        let (output, code) = run(shell, b"");
        assert!(output.contains("tty xterm-256color"), "{output:?}");
        assert_eq!(code, Some(7));
    }

    #[test]
    fn the_size_and_input_reach_the_shell() {
        let shell = spawn(sh("stty size; read line; echo \"got $line\""), 132, 43).unwrap();
        shell.process.resize(132, 43).unwrap();
        let (output, code) = run(shell, b"hello\r");
        assert!(output.contains("43 132"), "{output:?}");
        assert!(output.contains("got hello"), "{output:?}");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn ending_a_terminal_ends_what_its_shell_started() {
        let marker = format!("/tmp/meshrmm-terminal-test-{}", std::process::id());
        let shell = spawn(sh(&format!("sleep 600 & echo $! > {marker}; wait")), 80, 24).unwrap();
        let session = shell.process.session;
        let deadline = Instant::now() + Duration::from_secs(5);
        let child = loop {
            if let Some(pid) = std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the shell didn't start sleep");
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = std::fs::remove_file(&marker);
        assert!(members(session).contains(&child));
        drop(shell);
        let deadline = Instant::now() + Duration::from_secs(5);
        while members(session).contains(&child) {
            assert!(Instant::now() < deadline, "sleep outlived its terminal");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn shells_resolve_to_installed_programs() {
        let account = own_account();
        let sh = resolve(TerminalShell::Sh, &account).unwrap();
        assert!(sh.ends_with("sh") && executable(&sh));
        assert!(resolve(TerminalShell::Login, &account).is_some_and(|shell| executable(&shell)));
        assert_eq!(resolve(TerminalShell::Cmd, &account), None);
        let missing = Account {
            shell: PathBuf::from("/nonexistent/shell"),
            ..account
        };
        assert_eq!(
            resolve(TerminalShell::Login, &missing),
            find(TerminalShell::Sh)
        );
        assert_eq!(login_name(Path::new("/bin/zsh")), "-zsh");
        assert_eq!(available_shells()[0], TerminalShell::Login);
    }

    #[test]
    fn a_development_agent_opens_its_own_account() {
        let request = TerminalRequest {
            terminal_id: "one".into(),
            shell: TerminalShell::Sh,
            run_as: RunAs::User,
            columns: 80,
            rows: 24,
            notification: None,
        };
        let launch = plan(ExecutionMode::Console, &request).unwrap();
        assert_eq!(launch.account, own_account().name);
        assert!(launch.switch_to.is_none());
        assert_eq!(launch.argv0.as_deref(), Some(std::ffi::OsStr::new("-sh")));
        let missing = TerminalRequest {
            shell: TerminalShell::Zsh,
            ..request
        };
        if find(TerminalShell::Zsh).is_none() {
            assert_eq!(
                plan(ExecutionMode::Console, &missing).unwrap_err(),
                "zsh is not installed on this computer."
            );
        }
    }

    #[test]
    fn mac_users_get_their_shell_through_login() {
        let account = own_account();
        let shell = PathBuf::from("/bin/zsh");
        let launch = mac_user_launch(
            &account,
            TerminalShell::Zsh,
            shell.clone(),
            environment(&account, &shell),
        );
        assert_eq!(launch.program, Path::new("/usr/bin/login"));
        assert_eq!(
            launch.arguments,
            vec![
                OsString::from("-pfq"),
                account.name.clone().into(),
                "/bin/zsh".into()
            ]
        );
        let login = mac_user_launch(
            &account,
            TerminalShell::Login,
            shell.clone(),
            environment(&account, &shell),
        );
        assert_eq!(login.arguments.len(), 2);
    }
}
