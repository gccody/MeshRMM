//! Installs the Linux viewer for the current user. A viewer started from
//! anywhere else, such as the Downloads folder, copies itself to
//! `~/.local/share/meshrmm-remote` and hands its launch to that copy, so
//! links never start a downloaded file. The installed viewer keeps a
//! launcher in `~/.local/share/applications`, which makes it the desktop's
//! handler for `meshrmm:` links; `--uninstall` removes all of it.
//!
//! The launcher is written only when it differs, and the link is passed
//! after `--`, so text in a link cannot become a command-line option.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use super::app::APPLICATION_ID;

const PRODUCT: &str = "MeshRMM Remote";
const EXECUTABLE: &str = "meshrmm-remote";
/// Configuration a viewer reads from beside its executable; see `config.rs`.
const CONFIG_FILE: &str = "remote.json";
const LINK_TYPE: &str = "x-scheme-handler/meshrmm";
const ICON: &[u8] = include_bytes!("../../../assets/meshrmm-remote.svg");

pub fn is_uninstall_invocation() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--uninstall")
}

/// Where the desktop keeps a user's programs and launchers.
fn data_home() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home =
        std::env::var_os("HOME").context("Linux did not provide HOME to install the viewer in")?;
    Ok(PathBuf::from(home).join(".local").join("share"))
}

/// What the viewer installs under `data_home`.
struct Places {
    directory: PathBuf,
    executable: PathBuf,
    applications: PathBuf,
    launcher: PathBuf,
    icon: PathBuf,
}

impl Places {
    fn under(data_home: &Path) -> Self {
        let directory = data_home.join(APPLICATION_ID);
        let applications = data_home.join("applications");
        Self {
            executable: directory.join(EXECUTABLE),
            launcher: applications.join(format!("{APPLICATION_ID}.desktop")),
            icon: data_home
                .join("icons/hicolor/scalable/apps")
                .join(format!("{APPLICATION_ID}.svg")),
            directory,
            applications,
        }
    }
}

fn is_same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Whether the user owns `path`. A viewer the system's packages installed
/// is root's; it stays where it is.
fn owned_by_user(path: &Path) -> bool {
    // Safety: geteuid has no preconditions.
    let user = unsafe { libc::geteuid() };
    std::fs::metadata(path).is_ok_and(|metadata| metadata.uid() == user)
}

/// Installs this viewer unless it is the installed one. Returns whether it
/// did, in which case the installed copy has taken over this launch (or,
/// without a link, the user has been told the viewer is ready) and this
/// process should exit.
pub fn install_if_needed() -> anyhow::Result<bool> {
    let current = std::env::current_exe().context("could not locate the viewer executable")?;
    let places = Places::under(&data_home()?);
    if is_same_file(&current, &places.executable) || !owned_by_user(&current) {
        return Ok(false);
    }
    install_files(&current, &places)?;
    register_in(&places.executable, &places)?;
    tracing::info!(target = %places.executable.display(), "installed the viewer");
    let arguments = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    if arguments.is_empty() {
        crate::platform::show_notice(
            PRODUCT,
            "MeshRMM Remote is installed. Choose Connect on a device in the MeshRMM website to \
             start a session.\n\nYou can delete the file you downloaded.",
        );
    } else {
        Command::new(&places.executable)
            .args(arguments)
            .spawn()
            .with_context(|| {
                format!(
                    "could not start the installed viewer {}",
                    places.executable.display()
                )
            })?;
    }
    Ok(true)
}

/// Copies `current`, and the configuration beside it if any, into the
/// install folder. A running installed viewer keeps running from the file
/// it was started from, which the new one replaces by name.
fn install_files(current: &Path, places: &Places) -> anyhow::Result<()> {
    std::fs::create_dir_all(&places.directory)
        .with_context(|| format!("could not create {}", places.directory.display()))?;
    let staged = places
        .directory
        .join(format!("{EXECUTABLE}.install-{}", std::process::id()));
    let installed = std::fs::copy(current, &staged)
        .and_then(|_| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&staged, &places.executable));
    if let Err(error) = installed {
        let _ = std::fs::remove_file(&staged);
        return Err(error).with_context(|| {
            format!(
                "could not install the viewer in {}",
                places.directory.display()
            )
        });
    }
    if let Some(source) = current.parent().map(|parent| parent.join(CONFIG_FILE))
        && source.is_file()
    {
        std::fs::copy(&source, places.directory.join(CONFIG_FILE))
            .with_context(|| format!("could not copy {}", source.display()))?;
    }
    Ok(())
}

/// Quotes `path` as an argument of a launcher's `Exec` line.
fn exec_argument(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for character in path.to_string_lossy().chars() {
        match character {
            // What a quoted argument escapes, and then the backslash itself
            // again, as every launcher value does.
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            // A field code's sign, doubled to stand for itself.
            '%' => quoted.push_str("%%"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

/// The launcher that starts `executable` for a `meshrmm:` link.
fn launcher(executable: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={PRODUCT}\n\
         Comment=Remote desktop sessions started from the MeshRMM website\n\
         Exec={} -- %u\n\
         Icon={APPLICATION_ID}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         StartupWMClass={APPLICATION_ID}\n\
         Categories=Network;RemoteAccess;\n\
         MimeType={LINK_TYPE};\n",
        exec_argument(executable)
    )
}

/// Writes `contents` to `path` unless it is there already. Returns whether
/// it wrote.
fn write_if_changed(path: &Path, contents: &[u8]) -> anyhow::Result<bool> {
    if std::fs::read(path).is_ok_and(|existing| existing == contents) {
        return Ok(false);
    }
    let parent = path.parent().context("the path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    std::fs::write(path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(true)
}

/// Runs one of the desktop's own tools, which not every desktop has.
fn run_tool(command: &mut Command) {
    let program = command.get_program().to_string_lossy().into_owned();
    match command.output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            program,
            status = %output.status,
            error = %String::from_utf8_lossy(&output.stderr).trim(),
            "a desktop tool failed"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(program, "this desktop does not have the tool");
        }
        Err(error) => tracing::warn!(program, %error, "could not run a desktop tool"),
    }
}

/// Makes `executable` the desktop's handler for `meshrmm:` links, writing
/// only what changed.
pub fn register(executable: &Path) -> anyhow::Result<()> {
    register_in(executable, &Places::under(&data_home()?))
}

fn register_in(executable: &Path, places: &Places) -> anyhow::Result<()> {
    if let Err(error) = write_if_changed(&places.icon, ICON) {
        tracing::warn!(error = ?error, "could not install the viewer's icon");
    }
    if !write_if_changed(&places.launcher, launcher(executable).as_bytes())? {
        return Ok(());
    }
    // The desktop's index of which launcher opens what, then the choice of
    // this one for the links.
    run_tool(Command::new("update-desktop-database").arg(&places.applications));
    run_tool(Command::new("xdg-mime").args([
        "default",
        &format!("{APPLICATION_ID}.desktop"),
        LINK_TYPE,
    ]));
    tracing::info!(launcher = %places.launcher.display(), "registered the meshrmm: link handler");
    Ok(())
}

/// Removes the launcher, and with it the link handler, the icon and the
/// installed files. The user's settings, identity and logs stay.
pub fn uninstall() -> anyhow::Result<()> {
    let places = Places::under(&data_home()?);
    for file in [&places.launcher, &places.icon] {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("could not remove {}", file.display()));
            }
        }
    }
    run_tool(Command::new("update-desktop-database").arg(&places.applications));
    // A running viewer keeps the file it was started from until it exits.
    match std::fs::remove_dir_all(&places.directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("could not remove {}", places.directory.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("meshrmm-install-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn the_launcher_passes_the_link_after_the_options_and_handles_the_scheme() {
        let launcher = launcher(Path::new(
            "/home/sam/.local/share/meshrmm-remote/meshrmm-remote",
        ));
        assert!(launcher.starts_with("[Desktop Entry]\n"));
        assert!(
            launcher.contains(
                "\nExec=\"/home/sam/.local/share/meshrmm-remote/meshrmm-remote\" -- %u\n"
            )
        );
        assert!(launcher.contains("\nMimeType=x-scheme-handler/meshrmm;\n"));
        assert!(launcher.contains("\nIcon=meshrmm-remote\n"));
    }

    #[test]
    fn a_path_is_one_argument_whatever_is_in_it() {
        assert_eq!(
            exec_argument(Path::new("/home/a b/viewer")),
            "\"/home/a b/viewer\""
        );
        assert_eq!(
            exec_argument(Path::new("/home/50%/$x/`y`/\"z\"/a\\b")),
            "\"/home/50%%/\\\\$x/\\\\`y\\\\`/\\\\\"z\\\\\"/a\\\\\\\\b\""
        );
    }

    #[test]
    fn installing_copies_the_viewer_and_its_configuration_and_uninstalling_removes_them() {
        let directory = scratch("copy");
        let download = directory.join("Downloads");
        std::fs::create_dir_all(&download).unwrap();
        let current = download.join("meshrmm-remote-linux-x64");
        std::fs::write(&current, b"viewer").unwrap();
        std::fs::write(download.join(CONFIG_FILE), b"{}").unwrap();
        let places = Places::under(&directory.join("share"));

        install_files(&current, &places).unwrap();
        assert_eq!(std::fs::read(&places.executable).unwrap(), b"viewer");
        assert_eq!(
            std::fs::metadata(&places.executable)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read(places.directory.join(CONFIG_FILE)).unwrap(),
            b"{}"
        );
        // A newer download replaces it, and leaves nothing else behind.
        std::fs::write(&current, b"newer viewer").unwrap();
        install_files(&current, &places).unwrap();
        assert_eq!(std::fs::read(&places.executable).unwrap(), b"newer viewer");
        assert_eq!(std::fs::read_dir(&places.directory).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn the_launcher_is_written_once_until_it_changes() {
        let directory = scratch("launcher");
        let path = directory.join("applications/meshrmm-remote.desktop");
        assert!(write_if_changed(&path, b"one").unwrap());
        assert!(!write_if_changed(&path, b"one").unwrap());
        assert!(write_if_changed(&path, b"two").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(directory);
    }
}
