//! Association launches stay on the current maintenance desktop and inherit its job.
use super::{path_wide, wide};
use anyhow::{Context, ensure};
use std::path::Path;
use windows::{
    Win32::{Foundation::CloseHandle, System::Threading::*, UI::Shell::*},
    core::{PCWSTR, PWSTR},
};

fn association(extension: &str, kind: ASSOCSTR) -> anyhow::Result<String> {
    let extension = wide(extension);
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
            "No desktop application is associated with this file type. Use Preview for text files."
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
fn substitute(template: &str, path: &str) -> anyhow::Result<String> {
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
        if remaining.starts_with("%*") {
            remaining = &remaining[2..];
            continue;
        }
        let c = remaining.chars().next().unwrap();
        command.push(c);
        remaining = &remaining[c.len_utf8()..];
    }
    ensure!(found, "This file association does not accept a filename.");
    Ok(command)
}

pub(super) fn open(path: &Path) -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    ensure!(path.is_file(), "The file no longer exists.");
    let extension = path
        .extension()
        .context("No file association. Use Preview for text files.")?
        .to_string_lossy();
    let (executable, command) = if extension.eq_ignore_ascii_case("exe") {
        (
            path.to_string_lossy().into_owned(),
            format!("\"{}\"", path.display()),
        )
    } else {
        let extension = format!(".{extension}");
        (
            association(&extension, ASSOCSTR_EXECUTABLE)?,
            substitute(
                &association(&extension, ASSOCSTR_COMMAND)?,
                &path.to_string_lossy(),
            )?,
        )
    };
    let executable = wide(&executable);
    let mut command = wide(&command);
    let mut desktop = wide(&meshrmm_remote_screen::background::desktop_path()?);
    let directory = path_wide(path.parent().context("File has no parent folder")?);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        // No ShellExecute/DDE activation: those can redirect into another session.
        // CreateProcess inherits the launcher's kill-on-close job by default.
        CreateProcessW(
            PCWSTR(executable.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE,
            None,
            PCWSTR(directory.as_ptr()),
            &startup,
            &mut process,
        )
        .context("The associated application could not start on the background desktop")?;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn association_arguments_keep_filenames_quoted() {
        let path = r"C:\Folder & files\a b.txt";
        assert_eq!(
            substitute(r#""C:\app.exe" "%1" %*"#, path).unwrap(),
            r#""C:\app.exe" "C:\Folder & files\a b.txt" "#
        );
        assert_eq!(
            substitute("app.exe %L", path).unwrap(),
            r#"app.exe "C:\Folder & files\a b.txt""#
        );
        assert!(substitute("app.exe --activate", path).is_err());
        assert_eq!(
            substitute("app.exe %1", r"C:\%1-%L.txt").unwrap(),
            r#"app.exe "C:\%1-%L.txt""#
        );
    }
}
