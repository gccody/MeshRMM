//! Console clients need real console input records, not synthetic WM_CHAR.
//! A disposable helper attaches only to a console launched by the workspace;
//! attaching never changes the GUI helper's inherited IPC standard handles.
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use meshrmm_protocol::RemoteInput;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Console::*;
use windows::Win32::System::JobObjects::AssignProcessToJobObject;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::core::{BOOL, w};

#[derive(Serialize, Deserialize)]
enum ConsoleCommand {
    Input(RemoteInput),
    Release,
}

pub struct ConsoleInput {
    pub window: windows::Win32::Foundation::HWND,
    child: Child,
    sender: Option<mpsc::SyncSender<ConsoleCommand>>,
    writer: Option<JoinHandle<()>>,
}

impl ConsoleInput {
    pub fn start(process_id: u32, job: HANDLE) -> anyhow::Result<Self> {
        let executable = std::env::current_exe()?;
        // Cargo's unit-test harness is not the production executable.
        // Build the actual helper binary before running the ignored GUI test.
        let executable = if cfg!(test) {
            executable
                .parent()
                .and_then(|p| p.parent())
                .context("test binary directory")?
                .join("meshrmm-agent.exe")
        } else {
            executable
        };
        let mut child = Command::new(executable)
            .arg("--background-console-input")
            .arg(process_id.to_string())
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("could not start console input helper")?;
        if let Err(error) = unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        let output = child
            .stdout
            .take()
            .context("console handshake pipe missing")?;
        let (handshake, received) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line).map(|_| line);
            let _ = handshake.send(result);
        });
        let result = received
            .recv_timeout(Duration::from_secs(5))
            .context("console input helper startup timed out")
            .and_then(|line| Ok(line?.trim().parse::<usize>()?));
        let window = match result {
            Ok(window) if window != 0 => window,
            result => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(result
                    .err()
                    .unwrap_or_else(|| anyhow::anyhow!("console has no GUI window")));
            }
        };
        let _ = reader.join();
        let input = child.stdin.take().context("console command pipe missing")?;
        let (sender, commands) = mpsc::sync_channel(1024);
        let writer = std::thread::spawn(move || {
            let mut input = BufWriter::new(input);
            for command in commands {
                if serde_json::to_writer(&mut input, &command).is_err()
                    || input.write_all(b"\n").is_err()
                    || input.flush().is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            window: HWND(window as *mut _),
            child,
            sender: Some(sender),
            writer: Some(writer),
        })
    }

    pub fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
        self.send(ConsoleCommand::Input(input))
    }

    pub fn release(&self) {
        let _ = self.send(ConsoleCommand::Release);
    }

    fn send(&self, command: ConsoleCommand) -> anyhow::Result<()> {
        self.sender
            .as_ref()
            .context("console input is closed")?
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("console input queue is full or closed"))
    }
}

impl Drop for ConsoleInput {
    fn drop(&mut self) {
        self.sender = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

pub fn run_child() -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    let process_id = std::env::args()
        .nth(2)
        .context("missing console process")?
        .parse::<u32>()?;
    let mut session = u32::MAX;
    unsafe {
        windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(process_id, &mut session)?;
    }
    anyhow::ensure!(session == 0, "console input must stay in Session 0");
    // Command's redirected handles are retained explicitly before attachment.
    // This helper creates no threads and restores the handles before any I/O.
    let handles = unsafe {
        [
            GetStdHandle(STD_INPUT_HANDLE)?,
            GetStdHandle(STD_OUTPUT_HANDLE)?,
            GetStdHandle(STD_ERROR_HANDLE)?,
        ]
    };
    // CREATE_NO_WINDOW can still give a console-subsystem executable an
    // invisible console. Detach before attaching to the application's console.
    let _ = unsafe { FreeConsole() };
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match unsafe { AttachConsole(process_id) } {
            Ok(()) => break,
            Err(error) if Instant::now() >= deadline => {
                return Err(error).context("could not attach to the background console");
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        for (kind, handle) in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .zip(handles)
        {
            SetStdHandle(kind, handle)?;
        }
    }
    let input = unsafe {
        CreateFileW(
            w!("CONIN$"),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };
    let result = run_console_commands(input);
    unsafe {
        let _ = CloseHandle(input);
        let _ = FreeConsole();
    }
    result
}

fn run_console_commands(input: HANDLE) -> anyhow::Result<()> {
    writeln!(
        std::io::stdout(),
        "{}",
        unsafe { GetConsoleWindow() }.0 as usize
    )?;
    std::io::stdout().flush()?;
    let mut keyboard = ConsoleKeyboard {
        input,
        keys: [0; 256],
    };
    let mut commands = BufReader::new(std::io::stdin().lock());
    loop {
        let mut line = String::new();
        // Only the trusted parent writes here; still bound malformed IPC input.
        let count = std::io::Read::take(&mut commands, 512 * 1024).read_line(&mut line)?;
        if count == 0 {
            break;
        }
        anyhow::ensure!(
            line.ends_with('\n'),
            "console command exceeds its size limit"
        );
        match serde_json::from_str(&line)? {
            ConsoleCommand::Input(event) => keyboard.apply(event)?,
            ConsoleCommand::Release => keyboard.release()?,
        }
    }
    keyboard.release()
}

struct ConsoleKeyboard {
    input: HANDLE,
    keys: [u8; 256],
}

impl ConsoleKeyboard {
    fn apply(&mut self, event: RemoteInput) -> anyhow::Result<()> {
        match event {
            RemoteInput::Key {
                scan_code,
                extended,
                pressed,
                ..
            } => {
                let key = unsafe {
                    MapVirtualKeyW(
                        u32::from(scan_code) | if extended { 0xe000 } else { 0 },
                        MAPVK_VSC_TO_VK_EX,
                    )
                } as usize;
                if key == 0 || key >= 256 {
                    return Ok(());
                }
                self.keys[key] = if pressed { 128 } else { 0 };
                self.keys[VK_SHIFT.0 as usize] =
                    self.keys[VK_LSHIFT.0 as usize] | self.keys[VK_RSHIFT.0 as usize];
                self.keys[VK_CONTROL.0 as usize] =
                    self.keys[VK_LCONTROL.0 as usize] | self.keys[VK_RCONTROL.0 as usize];
                self.keys[VK_MENU.0 as usize] =
                    self.keys[VK_LMENU.0 as usize] | self.keys[VK_RMENU.0 as usize];
                let mut characters = [0; 8];
                let count = unsafe {
                    ToUnicodeEx(
                        key as u32,
                        scan_code as u32,
                        &self.keys,
                        &mut characters,
                        0,
                        Some(GetKeyboardLayout(0)),
                    )
                };
                let character = if count > 0 { characters[0] } else { 0 };
                self.write(
                    key as u16,
                    scan_code,
                    pressed,
                    character,
                    self.control_state() | if extended { ENHANCED_KEY } else { 0 },
                )?;
            }
            RemoteInput::TypeText { text, .. } => {
                for character in text.encode_utf16() {
                    let mapped = unsafe { VkKeyScanW(character) };
                    let key = if mapped >= 0 {
                        (mapped & 0xff) as u16
                    } else {
                        0
                    };
                    let scan = unsafe { MapVirtualKeyW(key as u32, MAPVK_VK_TO_VSC) } as u16;
                    self.write(key, scan, true, character, 0)?;
                    self.write(key, scan, false, character, 0)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn control_state(&self) -> u32 {
        [
            (VK_LCONTROL, LEFT_CTRL_PRESSED),
            (VK_RCONTROL, RIGHT_CTRL_PRESSED),
            (VK_LMENU, LEFT_ALT_PRESSED),
            (VK_RMENU, RIGHT_ALT_PRESSED),
            (VK_SHIFT, SHIFT_PRESSED),
        ]
        .into_iter()
        .filter(|(key, _)| self.keys[key.0 as usize] != 0)
        .fold(0, |state, (_, flag)| state | flag)
    }

    fn write(
        &self,
        key: u16,
        scan: u16,
        down: bool,
        character: u16,
        control: u32,
    ) -> anyhow::Result<()> {
        // Console readers filter generic modifier VKs. Side-specific VKs
        // belong in dwControlKeyState; .NET otherwise treats them as text.
        let key = match key {
            value if value == VK_LSHIFT.0 || value == VK_RSHIFT.0 => VK_SHIFT.0,
            value if value == VK_LCONTROL.0 || value == VK_RCONTROL.0 => VK_CONTROL.0,
            value if value == VK_LMENU.0 || value == VK_RMENU.0 => VK_MENU.0,
            value => value,
        };
        let character = if [VK_SHIFT.0, VK_CONTROL.0, VK_MENU.0].contains(&key) {
            0
        } else {
            character
        };
        let record = INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: BOOL::from(down),
                    wRepeatCount: 1,
                    wVirtualKeyCode: key,
                    wVirtualScanCode: scan,
                    uChar: KEY_EVENT_RECORD_0 {
                        UnicodeChar: character,
                    },
                    dwControlKeyState: control,
                },
            },
        };
        let mut written = 0;
        unsafe {
            WriteConsoleInputW(self.input, &[record], &mut written)?;
        }
        anyhow::ensure!(written == 1, "console did not accept the key event");
        Ok(())
    }

    fn release(&mut self) -> anyhow::Result<()> {
        for key in 0..256 {
            if self.keys[key] != 0 {
                self.write(
                    key as u16,
                    unsafe { MapVirtualKeyW(key as u32, MAPVK_VK_TO_VSC) } as u16,
                    false,
                    0,
                    0,
                )?;
            }
        }
        self.keys = [0; 256];
        Ok(())
    }
}
