//! Starts programs on the Session 0 background desktop.
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use windows::Win32::Storage::FileSystem::SearchPathW;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CreateProcessW, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, STARTF_USEPOSITION, STARTF_USESIZE, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR, ASSOCSTR_COMMAND, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::win32::{OwnedHandle, wide};

/// What to start and how.
#[derive(Default)]
pub(crate) struct Launch<'a> {
    /// The program, or `None` to take it from the start of `command`.
    pub(crate) executable: Option<&'a Path>,
    pub(crate) command: &'a str,
    /// The working directory, or `None` for the Agent's.
    pub(crate) directory: Option<&'a Path>,
    pub(crate) flags: PROCESS_CREATION_FLAGS,
    /// The first window's position and size.
    pub(crate) window: Option<(i32, i32, u32, u32)>,
}

pub(crate) struct DesktopProcess {
    pub(crate) id: u32,
    pub(crate) process: OwnedHandle,
    pub(crate) thread: OwnedHandle,
}

/// Starts a process on the background desktop without ShellExecute or DDE,
/// which could activate it in another session. It inherits the caller's
/// kill-on-close job unless the flags say otherwise.
pub(crate) fn launch(launch: Launch) -> anyhow::Result<DesktopProcess> {
    let executable = launch.executable.map(wide);
    let mut command = wide(launch.command);
    let directory = launch.directory.map(wide);
    let mut desktop = wide(meshrmm_remote_screen::background::desktop_path()?);
    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    if let Some((x, y, width, height)) = launch.window {
        startup.dwFlags |= STARTF_USEPOSITION | STARTF_USESIZE;
        startup.dwX = x as u32;
        startup.dwY = y as u32;
        startup.dwXSize = width;
        startup.dwYSize = height;
    }
    let pointer = |value: &Option<Vec<u16>>| {
        value
            .as_ref()
            .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr()))
    };
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            pointer(&executable),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            launch.flags,
            None,
            pointer(&directory),
            &startup,
            &mut information,
        )
    }
    .context("could not start a program on the background desktop")?;
    Ok(DesktopProcess {
        id: information.dwProcessId,
        process: OwnedHandle(information.hProcess),
        thread: OwnedHandle(information.hThread),
    })
}

/// What a command typed into Run opens.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Program {
        executable: PathBuf,
        command: String,
    },
    /// Shown by the built-in File Explorer. Empty for its list of drives.
    Folder(PathBuf),
    /// Session 0's own Task Manager exits without a window, and Explorer stays
    /// hidden, so the built-in tools stand in for them.
    TaskManager,
}

/// Resolves a command the way the Windows Run dialog does, but without
/// ShellExecute: a program, document or folder, by path or on the search path,
/// with `PATHEXT` extensions tried, followed by its arguments. An unquoted path
/// may contain spaces; the shortest prefix that exists wins, as in `CreateProcess`.
pub(crate) fn resolve(input: &str) -> anyhow::Result<Target> {
    let input = expand(input.trim())?;
    let candidates = candidates(&input);
    let Some(&(name, _)) = candidates.first() else {
        bail!("Type the name of a program, folder or document.");
    };
    ensure!(
        !name.contains("://"),
        "Session 0 has no web browser to open {name}."
    );
    for (name, arguments) in candidates {
        if let Some(path) = find(name) {
            return open(&path, arguments);
        }
    }
    bail!(
        "Windows cannot find '{name}'. Make sure you typed the name correctly, and then try again."
    )
}

/// Starts a program or opens a folder in a new built-in File Explorer, which
/// is `agent` run with `--background-file-browser`. `flags` are added to the
/// process's creation flags.
pub(crate) fn start(
    target: &Target,
    agent: &Path,
    flags: PROCESS_CREATION_FLAGS,
) -> anyhow::Result<DesktopProcess> {
    match target {
        Target::Program {
            executable,
            command,
        } => launch(Launch {
            executable: Some(executable),
            command,
            flags: flags | CREATE_NEW_CONSOLE,
            ..Default::default()
        }),
        Target::Folder(folder) => launch(Launch {
            executable: Some(agent),
            command: &format!(
                "{} --background-file-browser {}",
                quote(agent),
                quote(folder)
            ),
            flags: flags | CREATE_NO_WINDOW,
            ..Default::default()
        }),
        Target::TaskManager => bail!("Task Manager opens from the taskbar."),
    }
}

/// The program that opens `path` and its command line, with `arguments`
/// passed where its association takes them.
pub(crate) fn command_for(path: &Path, arguments: &str) -> anyhow::Result<(PathBuf, String)> {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .context("No program is associated with files that have no extension.")?;
    Ok(match extension.as_str() {
        "exe" | "com" => (path.to_path_buf(), join(quote(path), arguments)),
        "bat" | "cmd" => {
            let shell = crate::win32::windows_directory()?.join("System32\\cmd.exe");
            // cmd strips the outer quotes and keeps the rest verbatim.
            let command = format!("{} /c \"{}\"", quote(&shell), join(quote(path), arguments));
            (shell, command)
        }
        _ => {
            let extension = format!(".{extension}");
            (
                association(&extension, ASSOCSTR_EXECUTABLE)?.into(),
                substitute(
                    &association(&extension, ASSOCSTR_COMMAND)?,
                    &path.to_string_lossy(),
                    arguments,
                )?,
            )
        }
    })
}

fn open(path: &Path, arguments: &str) -> anyhow::Result<Target> {
    if path.is_dir() {
        return Ok(Target::Folder(path.to_path_buf()));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase());
    match name.as_deref() {
        Some("explorer.exe") => {
            let folder = Path::new(arguments.trim_matches('"'));
            return Ok(Target::Folder(if folder.is_dir() {
                folder.to_path_buf()
            } else {
                PathBuf::new()
            }));
        }
        Some("taskmgr.exe") => return Ok(Target::TaskManager),
        _ => {}
    }
    let (executable, command) = command_for(path, arguments)?;
    Ok(Target::Program {
        executable,
        command,
    })
}

/// Each way to split `input` into a program and its arguments, shortest program first.
fn candidates(input: &str) -> Vec<(&str, &str)> {
    if let Some(quoted) = input.strip_prefix('"') {
        let end = quoted.find('"').unwrap_or(quoted.len());
        let arguments = quoted.get(end + 1..).unwrap_or_default().trim_start();
        return vec![(&quoted[..end], arguments)];
    }
    input
        .char_indices()
        .filter(|&(index, character)| {
            character.is_whitespace() && !input[..index].ends_with(char::is_whitespace)
        })
        .map(|(index, _)| (&input[..index], input[index..].trim_start()))
        .chain((!input.is_empty()).then_some((input, "")))
        .collect()
}

fn find(name: &str) -> Option<PathBuf> {
    let mut names = vec![name.to_owned()];
    if Path::new(name).extension().is_none() {
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        names.extend(
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{name}{extension}")),
        );
    }
    if name.contains(['\\', '/', ':']) {
        return names.into_iter().find_map(|name| {
            // "C:" alone would be the drive's current folder.
            let path = PathBuf::from(if name.len() == 2 && name.ends_with(':') {
                format!("{name}\\")
            } else {
                name
            });
            path.exists()
                .then(|| std::path::absolute(&path).unwrap_or(path))
        });
    }
    names.iter().find_map(|name| search(name))
}

/// The file the system search path finds: the Agent's folder, System32, the
/// Windows folder, then `PATH`.
fn search(name: &str) -> Option<PathBuf> {
    let name = crate::win32::wide(name);
    let mut buffer = vec![0_u16; 260];
    loop {
        let length = unsafe {
            SearchPathW(
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                PCWSTR::null(),
                Some(&mut buffer),
                None,
            )
        } as usize;
        if length == 0 {
            return None;
        }
        if length < buffer.len() {
            let path = PathBuf::from(String::from_utf16_lossy(&buffer[..length]));
            return path.is_file().then_some(path);
        }
        buffer.resize(length, 0);
    }
}

fn expand(input: &str) -> anyhow::Result<String> {
    let source = crate::win32::wide(input);
    let mut buffer = vec![0_u16; source.len().max(260)];
    loop {
        let length =
            unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(&mut buffer)) }
                as usize;
        ensure!(length > 0, "Could not expand environment variables.");
        if length <= buffer.len() {
            return Ok(String::from_utf16_lossy(&buffer[..length - 1]));
        }
        buffer.resize(length, 0);
    }
}

fn association(extension: &str, kind: ASSOCSTR) -> anyhow::Result<String> {
    let extension = crate::win32::wide(extension);
    let mut length = 0;
    unsafe {
        let _ = AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            kind,
            PCWSTR(extension.as_ptr()),
            PCWSTR::null(),
            None,
            &mut length,
        );
        ensure!(
            length > 0 && length <= 32768,
            "No desktop application is associated with this file type."
        );
        let mut value = vec![0u16; length as usize];
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            kind,
            PCWSTR(extension.as_ptr()),
            PCWSTR::null(),
            Some(PWSTR(value.as_mut_ptr())),
            &mut length,
        )
        .ok()?;
        Ok(String::from_utf16_lossy(
            &value[..value.iter().position(|v| *v == 0).unwrap_or(value.len())],
        ))
    }
}

/// Fills an association's command template with `path` and `arguments`.
fn substitute(template: &str, path: &str, arguments: &str) -> anyhow::Result<String> {
    ensure!(
        !path.contains('"') && !path.contains('\0'),
        "Invalid filename."
    );
    let mut command = String::new();
    let mut remaining = template;
    let mut found = false;
    while !remaining.is_empty() {
        let mut replaced = false;
        for token in ["%1", "%L", "%l", "%V", "%v"] {
            let quoted = format!("\"{token}\"");
            let length = if remaining.starts_with(&quoted) {
                quoted.len()
            } else if remaining.starts_with(token) {
                token.len()
            } else {
                0
            };
            if length > 0 {
                command.push('"');
                command.push_str(path);
                command.push('"');
                remaining = &remaining[length..];
                found = true;
                replaced = true;
                break;
            }
        }
        if replaced {
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("%*") {
            command.push_str(arguments);
            remaining = rest;
            continue;
        }
        let c = remaining.chars().next().unwrap();
        command.push(c);
        remaining = &remaining[c.len_utf8()..];
    }
    ensure!(found, "This file association does not accept a filename.");
    Ok(command)
}

/// Quotes a path as one command-line argument. Paths can't contain quotes,
/// but trailing backslashes would escape the closing one.
fn quote(path: &Path) -> String {
    let path = path.to_string_lossy();
    let trailing = path.len() - path.trim_end_matches('\\').len();
    format!("\"{path}{}\"", "\\".repeat(trailing))
}

fn join(program: String, arguments: &str) -> String {
    if arguments.is_empty() {
        program
    } else {
        format!("{program} {arguments}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn association_arguments_keep_filenames_quoted() {
        let path = r"C:\Folder & files\a b.txt";
        assert_eq!(
            substitute(r#""C:\app.exe" "%1" %*"#, path, "").unwrap(),
            r#""C:\app.exe" "C:\Folder & files\a b.txt" "#
        );
        assert_eq!(
            substitute("app.exe %L", path, "").unwrap(),
            r#"app.exe "C:\Folder & files\a b.txt""#
        );
        assert!(substitute("app.exe --activate", path, "").is_err());
        assert_eq!(
            substitute("app.exe %1", r"C:\%1-%L.txt", "").unwrap(),
            r#"app.exe "C:\%1-%L.txt""#
        );
        assert_eq!(
            substitute(r#"%SystemRoot%\system32\mmc.exe "%1" %*"#, "a.msc", "/s").unwrap(),
            r#"%SystemRoot%\system32\mmc.exe "a.msc" /s"#
        );
    }

    #[test]
    fn commands_split_into_program_and_arguments() {
        assert!(candidates("").is_empty());
        assert_eq!(candidates("notepad"), [("notepad", "")]);
        assert_eq!(
            candidates(r"C:\Program Files\app.exe  a b"),
            [
                (r"C:\Program", r"Files\app.exe  a b"),
                (r"C:\Program Files\app.exe", "a b"),
                (r"C:\Program Files\app.exe  a", "b"),
                (r"C:\Program Files\app.exe  a b", ""),
            ]
        );
        assert_eq!(
            candidates(r#""C:\a b\c.exe"  x "y z""#),
            [(r"C:\a b\c.exe", r#"x "y z""#)]
        );
        assert_eq!(candidates(r#""C:\a b"#), [(r"C:\a b", "")]);
        assert_eq!(quote(Path::new(r"C:\")), r#""C:\\""#);
        assert_eq!(quote(Path::new(r"C:\a b")), r#""C:\a b""#);
    }

    fn same(path: &Path, expected: &Path) -> bool {
        path.as_os_str().eq_ignore_ascii_case(expected)
    }

    #[test]
    fn run_resolves_programs_documents_and_folders() -> anyhow::Result<()> {
        let windows = crate::win32::windows_directory()?;
        let system32 = windows.join("System32");
        let Target::Program {
            executable,
            command,
        } = resolve("notepad")?
        else {
            panic!("notepad is a program");
        };
        assert!(
            same(&executable, &system32.join("notepad.exe"))
                || same(&executable, &windows.join("notepad.exe")),
            "{executable:?}"
        );
        assert_eq!(command, quote(&executable));
        let Target::Program {
            executable,
            command,
        } = resolve("%SystemRoot%\\System32\\cmd /k echo  hi")?
        else {
            panic!("cmd is a program");
        };
        assert!(
            same(&executable, &system32.join("cmd.exe")),
            "{executable:?}"
        );
        assert!(command.ends_with("\" /k echo  hi"), "{command}");
        let Target::Program {
            executable,
            command,
            ..
        } = resolve("diskmgmt.msc")?
        else {
            panic!("diskmgmt.msc opens in MMC");
        };
        assert!(
            same(&executable, &system32.join("mmc.exe")),
            "{executable:?}"
        );
        assert!(
            command.to_ascii_lowercase().contains(
                &format!("\"{}\"", system32.join("diskmgmt.msc").display()).to_ascii_lowercase()
            ),
            "{command}"
        );
        let Target::Program { command, .. } = resolve("sysdm.cpl")? else {
            panic!("sysdm.cpl opens in Control Panel");
        };
        assert!(
            command.to_ascii_lowercase().contains("sysdm.cpl"),
            "{command}"
        );
        assert_eq!(resolve(r"C:")?, Target::Folder(PathBuf::from(r"C:\")));
        assert_eq!(
            resolve(&format!("\"{}\"", system32.display()))?,
            Target::Folder(system32.clone())
        );
        assert_eq!(
            resolve(&format!("explorer {}", system32.display()))?,
            Target::Folder(system32)
        );
        assert_eq!(resolve("taskmgr")?, Target::TaskManager);
        assert!(resolve("no-such-program-meshrmm").is_err());
        assert!(resolve("https://example.com").is_err());
        assert!(resolve(" ").is_err());
        Ok(())
    }
}
