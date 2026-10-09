use super::*;

mod rendering;
mod run_dialog;
mod session_zero;
mod shell_keys;
mod taskbar;
mod tools;

/// Pumps the workspace thread's messages for `millis`. Real input reaches its
/// target asynchronously, through Session 0's input queue.
pub(super) fn settle(workspace: &mut Workspace, millis: u64) {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        workspace.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Pumps until `done` holds, for up to `seconds`.
pub(super) fn wait_until(
    workspace: &mut Workspace,
    what: &str,
    seconds: u64,
    done: &dyn Fn(&Workspace) -> bool,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done(workspace) {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        workspace.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Windows on a thread of their own, bound to the background desktop. A menu,
/// or caption or button tracking, runs a modal loop that waits for more input,
/// so it can't run on the thread that sends the input.
pub(super) struct UiThread {
    thread: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl UiThread {
    /// Runs `create` on the new thread, then pumps its messages until dropped.
    pub(super) fn start<T: Send + 'static>(
        create: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<(Self, T)> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let created = (|| {
                let binding = background::Desktop::bind()?;
                anyhow::Ok((binding, create()?))
            })();
            let (_binding, value) = match created {
                Ok(created) => created,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            let _ = sender.send(Ok((unsafe { GetCurrentThreadId() }, value)));
            let mut message = MSG::default();
            while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
        let (thread, value) = receiver.recv()??;
        Ok((
            Self {
                thread,
                handle: Some(handle),
            },
            value,
        ))
    }
}

impl Drop for UiThread {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A window handle that crossed a thread boundary as an integer.
pub(super) fn hwnd(value: isize) -> HWND {
    HWND(value as *mut _)
}

pub(super) fn pack(point: POINT) -> isize {
    (point.x as u16 as u32 | ((point.y as u16 as u32) << 16)) as isize
}

pub(super) fn click_task(workspace: &mut Workspace, button: HWND) -> anyhow::Result<()> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(button, &mut rect)? };
    let x = (rect.left + rect.right) / 2;
    let y = (rect.top + rect.bottom) / 2;
    workspace.move_pointer(
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    workspace.button(PointerButton::Left, false)?;
    settle(workspace, 200);
    Ok(())
}

fn send_key(workspace: &mut Workspace, scan_code: u16, pressed: bool) -> anyhow::Result<()> {
    workspace.apply(RemoteInput::Key {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        scan_code,
        extended: false,
        pressed,
    })
}

pub(super) fn key(workspace: &mut Workspace, scan_code: u16, extended: bool) -> anyhow::Result<()> {
    for pressed in [true, false] {
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code,
            extended,
            pressed,
        })?;
    }
    Ok(())
}

fn press_win_r(workspace: &mut Workspace) -> anyhow::Result<()> {
    for (scan_code, extended, pressed) in [
        (0x5b, true, true),
        (0x13, false, true),
        (0x13, false, false),
        (0x5b, true, false),
    ] {
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code,
            extended,
            pressed,
        })?;
    }
    Ok(())
}

/// The window with the keyboard focus on `window`'s thread.
fn thread_focus(window: HWND) -> HWND {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetGUIThreadInfo(GetWindowThreadProcessId(window, None), &mut info);
    }
    info.hwndFocus
}

/// Whether Run is showing with keyboard input.
fn run_has_input(workspace: &Workspace) -> bool {
    workspace.run.as_ref().is_some_and(|run| unsafe {
        run.visible() && GetForegroundWindow() == run.window && thread_focus(run.window) == run.edit
    })
}

/// Checks that Win+R opened Run with keyboard input, then closes it with
/// Escape, which must hand keyboard input back to `previous`'s window.
fn close_run(workspace: &mut Workspace, previous: HWND) -> anyhow::Result<()> {
    wait_until(workspace, "Run to take keyboard input", 5, &run_has_input)?;
    key(workspace, 0x01, false)?;
    let previous = unsafe { GetAncestor(previous, GA_ROOT) };
    wait_until(workspace, "Escape to close Run", 5, &|workspace| {
        workspace.run.as_ref().is_some_and(|run| !run.visible())
            && unsafe { GetForegroundWindow() } == previous
    })
}

/// The open popup menu's window and item labels, if a menu is showing.
pub(super) fn open_menu() -> anyhow::Result<Option<(HWND, Vec<String>)>> {
    for window in background::windows()? {
        let mut class = [0_u16; 16];
        let length = unsafe { GetClassNameW(window, &mut class) };
        let visible = unsafe { GetWindowLongW(window, GWL_STYLE) } as u32 & WS_VISIBLE.0 != 0;
        if !visible || String::from_utf16_lossy(&class[..length as usize]) != "#32768" {
            continue;
        }
        let mut menu = 0;
        unsafe {
            SendMessageTimeoutW(
                window,
                MN_GETHMENU,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                500,
                Some(&mut menu),
            );
        }
        let menu = HMENU(menu as *mut _);
        let items = (0..unsafe { GetMenuItemCount(Some(menu)) }.max(0))
            .map(|position| {
                let mut label = [0_u16; 128];
                let length = unsafe {
                    GetMenuStringW(menu, position as u32, Some(&mut label), MF_BYPOSITION)
                };
                String::from_utf16_lossy(&label[..length.max(0) as usize]).replace('&', "")
            })
            .collect();
        return Ok(Some((window, items)));
    }
    Ok(None)
}

/// Clicks item `index` of `window`'s menu bar, returns the items of the menu
/// that opens, and closes it with Escape.
pub(super) fn menu_bar_opens(
    workspace: &mut Workspace,
    window: HWND,
    index: u32,
) -> anyhow::Result<Vec<String>> {
    let menu = unsafe { GetMenu(window) };
    anyhow::ensure!(!menu.is_invalid(), "the window has no menu bar");
    let mut item = RECT::default();
    unsafe { GetMenuItemRect(Some(window), menu, index, &mut item)? };
    click_at(
        workspace,
        (item.left + item.right) / 2,
        (item.top + item.bottom) / 2,
    )?;
    wait_until(workspace, "a menu-bar click to open its menu", 5, &|_| {
        open_menu().ok().flatten().is_some()
    })
    .inspect_err(|_| {
        let _ = proof("background-menu-bar-failure.bmp");
    })?;
    let (_, items) = open_menu()?.context("the menu closed")?;
    // The first Escape closes the menu, the second leaves the menu bar.
    key(workspace, 0x01, false)?;
    key(workspace, 0x01, false)?;
    wait_until(workspace, "Escape to close the menu", 5, &|_| {
        open_menu().ok().flatten().is_none()
    })?;
    Ok(items)
}

pub(super) fn window_text(window: HWND) -> String {
    let mut text = [0_u16; 512];
    let mut length = 0;
    unsafe {
        SendMessageTimeoutW(
            window,
            WM_GETTEXT,
            WPARAM(text.len()),
            LPARAM(text.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            500,
            Some(&mut length),
        );
    }
    String::from_utf16_lossy(&text[..length.min(text.len())])
}

/// The first visible descendant of `parent` with class `class` that satisfies
/// `accept`, largest first.
pub(super) fn child(parent: HWND, class: &str, accept: &dyn Fn(HWND) -> bool) -> Option<HWND> {
    unsafe extern "system" fn collect(window: HWND, parameter: LPARAM) -> windows::core::BOOL {
        unsafe { (*(parameter.0 as *mut Vec<HWND>)).push(window) };
        true.into()
    }
    let mut children = Vec::<HWND>::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(parent),
            Some(collect),
            LPARAM(&mut children as *mut _ as isize),
        );
    }
    let area = |window: HWND| {
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(window, &mut rect) };
        (rect.right - rect.left) * (rect.bottom - rect.top)
    };
    children
        .into_iter()
        .filter(|window| {
            let mut name = [0_u16; 64];
            let length = unsafe { GetClassNameW(*window, &mut name) };
            String::from_utf16_lossy(&name[..length as usize]) == class
                && unsafe { GetWindowLongW(*window, GWL_STYLE) } as u32 & WS_VISIBLE.0 != 0
                && accept(*window)
        })
        .max_by_key(|window| area(*window))
}

pub(super) fn click_at(workspace: &mut Workspace, x: i32, y: i32) -> anyhow::Result<()> {
    anyhow::ensure!(
        (0..WIDTH as i32).contains(&x) && (0..HEIGHT as i32).contains(&y),
        "({x}, {y}) is off the canvas"
    );
    workspace.move_pointer(
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    workspace.button(PointerButton::Left, false)
}

/// The top-level window a process in the workspace's job shows, if any.
fn job_window(
    workspace: &Workspace,
    accept: &dyn Fn(&str, &str) -> bool,
) -> anyhow::Result<Option<HWND>> {
    for window in background::windows()? {
        let mut class = [0_u16; 64];
        let length = unsafe { GetClassNameW(window, &mut class) } as usize;
        if accept(
            &String::from_utf16_lossy(&class[..length]),
            &window_text(window),
        ) && workspace.owns_window(window)
        {
            return Ok(Some(window));
        }
    }
    Ok(None)
}

pub(super) fn wait_for_job_window(
    workspace: &mut Workspace,
    what: &str,
    accept: &dyn Fn(&str, &str) -> bool,
) -> anyhow::Result<HWND> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        workspace.pump();
        if let Some(window) = job_window(workspace, accept)? {
            return Ok(window);
        }
        if Instant::now() >= deadline {
            let windows = background::windows()?
                .into_iter()
                .map(window_text)
                .collect::<Vec<_>>();
            anyhow::bail!("{what} did not open in the workspace's job; windows: {windows:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn close_job_window(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid)? };
    unsafe {
        let _ = TerminateProcess(process, 1);
        WaitForSingleObject(process, 5000);
        CloseHandle(process)?;
    }
    workspace.pump();
    Ok(())
}

pub(super) fn proof(name: &str) -> anyhow::Result<()> {
    std::fs::write(
        std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(name),
        background::snapshot_bmp()?,
    )?;
    Ok(())
}
