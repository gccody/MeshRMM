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
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join("Documents")
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
                metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == owner,
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
                return Err(error).with_context(|| format!("could not write {}", path.display()));
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
