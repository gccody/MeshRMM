//! Graphical sessions and their helpers. The coordinator asks logind for the
//! computer's sessions, and starts a session helper in each X11 or Wayland
//! session, the login screen's included, as the session's user and with the
//! session's environment, which it reads from the session's own processes.
//! A helper inherits one end of a socket pair, its link to the coordinator,
//! and the uinput descriptors it injects input through.
//!
//! The session on seat0 that logind calls active is the console: remote
//! sessions and thumbnails use its helper.
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::remote::helper::coordinator::{Connection, Registry};

const LOGIN1: &str = "org.freedesktop.login1";
const POLL: Duration = Duration::from_secs(1);
/// How long a helper that stopped waits before it starts again.
const RESTART_DELAY: Duration = Duration::from_secs(5);
/// The variable that tells a helper which descriptor reaches the coordinator.
const SOCKET_VARIABLE: &str = "MESHRMM_HELPER_FD";
/// Set for a helper at a login screen.
const LOGIN_WINDOW_VARIABLE: &str = "MESHRMM_LOGIN_WINDOW";
/// Session variables a helper takes from the session's processes.
const SESSION_VARIABLES: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "DESKTOP_SESSION",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
];

/// The console session's ID, as the supervisor last saw it.
static CONSOLE: Mutex<Option<String>> = Mutex::new(None);

/// A logind session with a display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub id: String,
    pub uid: u32,
    pub user: String,
    /// `x11` or `wayland`.
    pub kind: String,
    /// Whether it is a login screen rather than a user's session.
    pub greeter: bool,
    /// The X display logind recorded, for X11 sessions.
    pub display: String,
    pub seat: String,
    pub active: bool,
}

/// Talks to logind on the system bus.
pub(crate) struct Logind {
    bus: zbus::blocking::Connection,
}

impl Logind {
    pub(crate) fn connect() -> anyhow::Result<Self> {
        Ok(Self {
            bus: zbus::blocking::Connection::system()
                .context("could not connect to the system D-Bus")?,
        })
    }

    fn properties(
        &self,
        path: &str,
        interface: &str,
    ) -> anyhow::Result<HashMap<String, OwnedValue>> {
        Ok(self
            .bus
            .call_method(
                Some(LOGIN1),
                path,
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &(interface,),
            )?
            .body()
            .deserialize()?)
    }

    /// The sessions that run a display server.
    pub(crate) fn graphical_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let listed: Vec<(String, u32, String, String, OwnedObjectPath)> = self
            .bus
            .call_method(
                Some(LOGIN1),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "ListSessions",
                &(),
            )
            .context("logind could not list the sessions")?
            .body()
            .deserialize()?;
        let mut sessions = Vec::new();
        for (id, uid, user, seat, path) in listed {
            let Ok(properties) = self.properties(path.as_str(), "org.freedesktop.login1.Session")
            else {
                continue;
            };
            let text = |name: &str| {
                properties
                    .get(name)
                    .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
                    .unwrap_or_default()
            };
            let kind = text("Type");
            let state = text("State");
            if !matches!(kind.as_str(), "x11" | "wayland") || state == "closing" {
                continue;
            }
            let active = properties
                .get("Active")
                .and_then(|value| bool::try_from(value).ok())
                .unwrap_or(false);
            sessions.push(Session {
                id,
                uid,
                user,
                greeter: text("Class") == "greeter",
                display: text("Display"),
                kind,
                seat,
                active,
            });
        }
        Ok(sessions)
    }

    /// The ID of seat0's active session.
    pub(crate) fn active_session(&self) -> anyhow::Result<Option<String>> {
        let properties = self.properties(
            "/org/freedesktop/login1/seat/seat0",
            "org.freedesktop.login1.Seat",
        )?;
        Ok(properties
            .get("ActiveSession")
            .and_then(|value| <(String, OwnedObjectPath)>::try_from(value.try_clone().ok()?).ok())
            .map(|(id, _)| id)
            .filter(|id| !id.is_empty()))
    }

    /// Unlocks the session: logind tells its screen locker to stand down.
    pub(crate) fn unlock_session(&self, id: &str) -> anyhow::Result<()> {
        self.bus.call_method(
            Some(LOGIN1),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "UnlockSession",
            &(id,),
        )?;
        Ok(())
    }

    /// Locks the session as its screen locker does when asked by logind.
    pub(crate) fn lock_session(&self, id: &str) -> anyhow::Result<()> {
        self.bus.call_method(
            Some(LOGIN1),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "LockSession",
            &(id,),
        )?;
        Ok(())
    }
}

/// The console's graphical session: the coordinator's last reading, or
/// logind's answer in a console Agent.
pub(crate) fn console_session() -> Option<String> {
    if crate::remote::helper::coordinator::registry().is_ok() {
        return CONSOLE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    }
    let logind = Logind::connect().ok()?;
    let active = logind.active_session().ok()??;
    logind
        .graphical_sessions()
        .ok()?
        .into_iter()
        .any(|session| session.id == active)
        .then_some(active)
}

/// The console's session, with its user, when a user is signed in there.
pub(crate) fn console_user_session() -> Option<Session> {
    let console = console_session()?;
    Logind::connect()
        .ok()?
        .graphical_sessions()
        .ok()?
        .into_iter()
        .find(|session| session.id == console && !session.greeter)
}

/// Starts supervising session helpers.
pub(crate) fn supervise() -> anyhow::Result<()> {
    let registry = crate::remote::helper::coordinator::create_registry();
    let logind = Logind::connect()?;
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    std::thread::Builder::new()
        .name("meshrmm-session-supervisor".into())
        .spawn(move || {
            let mut supervisor = Supervisor {
                logind,
                registry,
                executable,
                helpers: HashMap::new(),
                failed: HashMap::new(),
            };
            loop {
                if let Err(error) = supervisor.poll() {
                    tracing::debug!(error = ?error, "could not check the graphical sessions");
                }
                std::thread::sleep(POLL);
            }
        })
        .context("could not start the session supervisor")?;
    Ok(())
}

struct Helper {
    session: Session,
    child: Child,
    connection: Arc<Mutex<Option<Arc<Connection>>>>,
}

struct Supervisor {
    logind: Logind,
    registry: Arc<Registry>,
    executable: PathBuf,
    helpers: HashMap<String, Helper>,
    /// When each session's helper last stopped or failed to start.
    failed: HashMap<String, Instant>,
}

impl Supervisor {
    fn poll(&mut self) -> anyhow::Result<()> {
        let sessions = self.logind.graphical_sessions()?;
        let active = self.logind.active_session()?;
        let console = active.filter(|id| sessions.iter().any(|session| session.id == *id));
        let mut current = CONSOLE.lock().unwrap_or_else(|e| e.into_inner());
        if *current != console {
            tracing::info!(
                console = console.as_deref(),
                "the console's graphical session changed"
            );
            *current = console;
        }
        drop(current);

        // Helpers whose session ended, or that stopped.
        self.helpers.retain(|id, helper| {
            let ended = !sessions.iter().any(|session| session.id == *id);
            let exited = helper.child.try_wait().ok().flatten();
            let disconnected = helper
                .connection
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|connection| connection.closed());
            if ended || exited.is_some() || disconnected {
                let _ = helper.child.kill();
                let _ = helper.child.wait();
                if !ended {
                    tracing::warn!(session = %id, user = %helper.session.user, status = ?exited, "a session helper stopped; starting it again shortly");
                    self.failed.insert(id.clone(), Instant::now());
                }
                return false;
            }
            true
        });
        self.failed
            .retain(|id, _| sessions.iter().any(|session| session.id == *id));

        for session in sessions {
            if self.helpers.contains_key(&session.id)
                || self
                    .failed
                    .get(&session.id)
                    .is_some_and(|failed| failed.elapsed() < RESTART_DELAY)
            {
                continue;
            }
            match self.start(&session) {
                Ok(Some(helper)) => {
                    self.failed.remove(&session.id);
                    self.helpers.insert(session.id.clone(), helper);
                }
                // The display server is not up yet.
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(session = %session.id, user = %session.user, error = ?error, "could not start a session helper");
                    self.failed.insert(session.id.clone(), Instant::now());
                }
            }
        }
        Ok(())
    }

    /// Starts `session`'s helper, or returns `None` while the session's
    /// display server is not running yet.
    fn start(&self, session: &Session) -> anyhow::Result<Option<Helper>> {
        let Some(environment) = session_environment(session) else {
            return Ok(None);
        };
        let account = Account::of(session.uid)?;
        let (coordinator, helper) =
            UnixStream::pair().context("could not create the helper's socket")?;
        let keyboard = crate::remote::linux::input::open_uinput()
            .inspect_err(|error| tracing::warn!(error = ?error, "remote input will be unavailable"))
            .ok();
        let pointer = keyboard
            .as_ref()
            .and_then(|_| crate::remote::linux::input::open_uinput().ok());
        let inherited: Vec<OwnedFd> = [
            Some(OwnedFd::from(helper)),
            keyboard.map(OwnedFd::from),
            pointer.map(OwnedFd::from),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut command = Command::new(&self.executable);
        command
            .arg("--session-helper")
            .env_clear()
            .envs(&environment)
            .env("HOME", &account.home)
            .env("USER", &session.user)
            .env("LOGNAME", &session.user)
            .env("SHELL", &account.shell)
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("XDG_RUNTIME_DIR", runtime_directory(session.uid))
            .env("XDG_SESSION_ID", &session.id)
            .env("XDG_SESSION_TYPE", &session.kind)
            .env(SOCKET_VARIABLE, "3")
            .current_dir(&account.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if inherited.len() == 3 {
            command.env(crate::remote::linux::input::DESCRIPTORS_VARIABLE, "4,5");
        }
        if session.greeter {
            command.env(LOGIN_WINDOW_VARIABLE, "1");
        }
        if !environment.contains_key("DBUS_SESSION_BUS_ADDRESS") {
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", runtime_directory(session.uid).display()),
            );
        }
        let mut raw = [-1; 3];
        for (slot, fd) in raw.iter_mut().zip(&inherited) {
            *slot = fd.as_raw_fd();
        }
        let (uid, gid, groups) = (account.uid, account.gid, account.groups.clone());
        // SAFETY: the closure neither allocates nor takes locks; it makes
        // only async-signal-safe system calls.
        unsafe {
            command.pre_exec(move || {
                // Move the descriptors out of the way, then to 3, 4 and 5,
                // where dup2 also makes them survive exec.
                let mut moved = [-1; 3];
                for (high, &fd) in moved.iter_mut().zip(&raw) {
                    if fd < 0 {
                        continue;
                    }
                    *high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 100);
                    if *high < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (index, &fd) in moved.iter().enumerate() {
                    if fd >= 0 && libc::dup2(fd, 3 + index as i32) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // The helper ends with the coordinator.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("could not start the session helper")?;
        drop(inherited);
        tracing::info!(
            session = %session.id,
            user = %session.user,
            kind = %session.kind,
            greeter = session.greeter,
            pid = child.id(),
            "started a session helper"
        );
        let connection = Arc::new(Mutex::new(None));
        let (registry, slot, id, uid) = (
            Arc::clone(&self.registry),
            Arc::clone(&connection),
            session.id.clone(),
            session.uid,
        );
        std::thread::Builder::new()
            .name("meshrmm-helper-attach".into())
            .spawn(move || match registry.attach(coordinator, uid, Some(id.clone())) {
                Ok(attached) => {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(attached);
                }
                Err(error) => {
                    tracing::warn!(session = %id, error = ?error, "a session helper did not connect")
                }
            })?;
        Ok(Some(Helper {
            session: session.clone(),
            child,
            connection,
        }))
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        for helper in self.helpers.values_mut() {
            let _ = helper.child.kill();
        }
    }
}

fn runtime_directory(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

/// A user's account details from the password and group databases.
struct Account {
    uid: u32,
    gid: u32,
    home: PathBuf,
    shell: String,
    groups: Vec<libc::gid_t>,
}

impl Account {
    fn of(uid: u32) -> anyhow::Result<Self> {
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        // SAFETY: the structure is plain data for getpwuid_r to fill in.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and the buffer's length is given.
        let result = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if result != 0 || found.is_null() {
            bail!("user {uid} has no account");
        }
        // SAFETY: getpwuid_r filled in NUL-terminated strings in the buffer.
        let text = |field: *const libc::c_char| unsafe {
            std::ffi::CStr::from_ptr(field)
                .to_string_lossy()
                .into_owned()
        };
        let name = text(entry.pw_name);
        let mut groups = vec![0 as libc::gid_t; 256];
        let mut count = groups.len() as libc::c_int;
        let c_name = std::ffi::CString::new(name.clone())?;
        // SAFETY: the list holds `count` entries.
        if unsafe {
            libc::getgrouplist(
                c_name.as_ptr(),
                entry.pw_gid,
                groups.as_mut_ptr(),
                &mut count,
            )
        } < 0
        {
            // Too many groups: keep the primary one.
            count = 0;
        }
        groups.truncate(count.max(0) as usize);
        if groups.is_empty() {
            groups.push(entry.pw_gid);
        }
        Ok(Self {
            uid,
            gid: entry.pw_gid,
            home: PathBuf::from(text(entry.pw_dir)),
            shell: text(entry.pw_shell),
            groups,
        })
    }
}

/// The session's display variables, from the session's processes: those
/// logind placed in the session, else those of the user's service manager,
/// where GNOME and KDE keep them. `None` until the display server is up.
fn session_environment(session: &Session) -> Option<HashMap<String, String>> {
    let needed = if session.kind == "wayland" {
        "WAYLAND_DISPLAY"
    } else {
        "DISPLAY"
    };
    let mut best: Option<(u8, HashMap<String, String>)> = None;
    for pid in processes_of(session.uid) {
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let environment = parse_environment(&raw);
        if !environment.contains_key(needed) {
            continue;
        }
        let rank = match environment.get("XDG_SESSION_ID") {
            Some(id) if *id == session.id => 3,
            Some(_) => continue,
            None if in_user_manager(pid, session.uid) => 2,
            None => 1,
        };
        if best.as_ref().is_none_or(|(ranked, _)| rank > *ranked) {
            best = Some((rank, environment));
        }
    }
    let mut chosen: HashMap<String, String> = match best {
        Some((_, environment)) => environment,
        None => fallback_environment(session)?,
    }
    .into_iter()
    .filter(|(name, _)| SESSION_VARIABLES.contains(&name.as_str()))
    .collect();
    // An X11 session's display comes from logind when its processes say none.
    if session.kind == "x11" && !session.display.is_empty() {
        chosen
            .entry("DISPLAY".to_owned())
            .or_insert_with(|| session.display.clone());
    }
    Some(chosen)
}

/// Without a process to copy from: the session's Wayland socket, or its X
/// display with the user's own X authority file.
fn fallback_environment(session: &Session) -> Option<HashMap<String, String>> {
    let mut environment = HashMap::new();
    if session.kind == "wayland" {
        let socket = std::fs::read_dir(runtime_directory(session.uid))
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
            .min()?;
        environment.insert("WAYLAND_DISPLAY".to_owned(), socket);
    } else {
        if session.display.is_empty() {
            return None;
        }
        environment.insert("DISPLAY".to_owned(), session.display.clone());
        let authority = Account::of(session.uid).ok()?.home.join(".Xauthority");
        if authority.exists() {
            environment.insert(
                "XAUTHORITY".to_owned(),
                authority.to_string_lossy().into_owned(),
            );
        }
    }
    Some(environment)
}

fn processes_of(uid: u32) -> impl Iterator<Item = u32> {
    use std::os::unix::fs::MetadataExt;

    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(move |entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            (entry.metadata().ok()?.uid() == uid).then_some(pid)
        })
}

fn in_user_manager(pid: u32, uid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .is_ok_and(|cgroup| cgroup.contains(&format!("/user@{uid}.service/")))
}

fn parse_environment(raw: &[u8]) -> HashMap<String, String> {
    raw.split(|&byte| byte == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (name, value) = entry.split_once('=')?;
            Some((name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Entry point of a session helper the coordinator started.
pub(crate) fn run_helper() -> anyhow::Result<()> {
    use std::os::fd::FromRawFd;

    let fd: i32 = std::env::var(SOCKET_VARIABLE)
        .context("the session helper must be started by the MeshRMM Agent")?
        .parse()
        .context("the helper's socket descriptor is invalid")?;
    // SAFETY: the coordinator passed this descriptor, which this process
    // owns from here on.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let login_window = std::env::var_os(LOGIN_WINDOW_VARIABLE).is_some();
    tracing::info!(
        session = std::env::var("XDG_SESSION_ID").unwrap_or_default(),
        kind = std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        "session helper starting"
    );
    crate::remote::helper::host::run_started(stream, login_window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_process_environment() {
        let environment =
            parse_environment(b"DISPLAY=:0\0WAYLAND_DISPLAY=wayland-0\0BROKEN\0EMPTY=\0");
        assert_eq!(environment["DISPLAY"], ":0");
        assert_eq!(environment["WAYLAND_DISPLAY"], "wayland-0");
        assert_eq!(environment["EMPTY"], "");
        assert!(!environment.contains_key("BROKEN"));
    }

    #[test]
    fn finds_this_users_account() {
        // SAFETY: getuid has no preconditions.
        let account = Account::of(unsafe { libc::getuid() }).unwrap();
        assert!(account.home.is_absolute());
        assert!(!account.groups.is_empty());
    }
}
