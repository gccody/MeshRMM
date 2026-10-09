//! Session 0 checks of the background Run dialog.
use super::*;

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn run_opens_programs_documents_and_folders() -> anyhow::Result<()> {
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let mut workspace = Workspace::new()?;
        workspace.pump();
        let run_pin = PINS.iter().position(|pin| pin.kind == Kind::Run).unwrap();
        let button = unsafe { GetDlgItem(Some(workspace.shell), run_pin as i32 + 1)? };
        let type_command = |workspace: &mut Workspace, command: &str| -> anyhow::Result<()> {
            workspace.apply(RemoteInput::TypeText {
                display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                text: command.into(),
            })?;
            key(workspace, 0x1c, false)?;
            settle(workspace, 300);
            Ok(())
        };
        let mut first = true;
        for (command, class, title) in [
            ("notepad", "Notepad", "Untitled - Notepad"),
            ("diskmgmt.msc", "MMCMainFrame", "Disk Management"),
            ("sysdm.cpl", "#32770", "System Properties"),
            (
                r"%SystemRoot%\System32",
                "MeshRMMBackgroundFiles",
                "File Explorer",
            ),
        ] {
            // The pin opens Run the first time, and Win+R after that.
            if first {
                click_task(&mut workspace, button)?;
                first = false;
            } else {
                press_win_r(&mut workspace)?;
            }
            wait_until(
                &mut workspace,
                "Run to take keyboard input",
                5,
                &run_has_input,
            )?;
            let run = workspace.run.as_ref().context("Run did not open")?;
            if command == "notepad" {
                // Like Windows' Run, it has a taskbar button.
                let window = run.window;
                let deadline = Instant::now() + Duration::from_secs(5);
                while !workspace.tasks.iter().any(|task| task.window == window) {
                    anyhow::ensure!(Instant::now() < deadline, "Run got no taskbar button");
                    workspace.pump();
                    std::thread::sleep(Duration::from_millis(50));
                }
                proof("background-run-open.bmp")?;
            }
            // The previous command is selected, so typing replaces it.
            type_command(&mut workspace, command)?;
            let window = wait_for_job_window(&mut workspace, command, &|window_class, text| {
                window_class == class && text.contains(title)
            })?;
            if class == "MeshRMMBackgroundFiles" {
                let location = window_text(unsafe { GetDlgItem(Some(window), 201)? });
                anyhow::ensure!(
                    location.ends_with("System32"),
                    "File Explorer opened {location:?}"
                );
            }
            let run = workspace.run.as_ref().context("Run went away")?;
            anyhow::ensure!(!run.visible(), "Run stayed open after starting {command}");
            anyhow::ensure!(window_text(run.edit) == command, "Run lost {command}");
            proof(&format!(
                "background-run-{}.bmp",
                command.replace(['%', '\\', '.'], "")
            ))?;
            close_job_window(&mut workspace, window)?;
        }

        // A console program gets its own console.
        press_win_r(&mut workspace)?;
        type_command(&mut workspace, "cmd /k title Run console")?;
        let console = wait_for_job_window(&mut workspace, "cmd", &|class, text| {
            class == "ConsoleWindowClass" && text.contains("Run console")
        })?;
        close_job_window(&mut workspace, console)?;

        // Errors show in the dialog, which stays open.
        press_win_r(&mut workspace)?;
        wait_until(
            &mut workspace,
            "Run to take keyboard input",
            5,
            &run_has_input,
        )?;
        type_command(&mut workspace, "no-such-program-meshrmm")?;
        let run = workspace.run.as_ref().context("Run went away")?;
        anyhow::ensure!(run.visible(), "Run closed on an error");
        let error = window_text(unsafe { GetDlgItem(Some(run.window), 101)? });
        anyhow::ensure!(
            error.contains("no-such-program-meshrmm"),
            "Run showed {error:?}"
        );
        proof("background-run-error.bmp")?;
        // Clicking Cancel closes it.
        let cancel = unsafe { GetDlgItem(Some(run.window), IDCANCEL.0)? };
        click_task(&mut workspace, cancel)?;
        let run = workspace.run.as_ref().context("Run went away")?;
        anyhow::ensure!(!run.visible(), "Cancel did not close Run");
        Ok(())
    })
    .join()
    .expect("Run test panicked")
}
