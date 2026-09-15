//! Session-scoped desktop hooks. Only MeshRMM's tagged SendInput events pass.
//! Windows removes hooks if the helper exits; Drop removes them on normal stop.
use std::{sync::mpsc, thread};
use windows::Win32::{Foundation::*, System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId}, UI::WindowsAndMessaging::*};

pub const INPUT_TAG: usize = 0x4d524d4d;

pub struct InputBlock {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputBlock {
    pub fn start() -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new().name("local-input-block".into()).spawn(move || unsafe {
            // Force creation of the message queue before publishing the thread id.
            let mut message = MSG::default();
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            let hooks = (|| -> windows::core::Result<(HHOOK, HHOOK)> {
                let module = GetModuleHandleW(None)?;
                let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(module.into()), 0)?;
                match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(module.into()), 0) {
                    Ok(mouse) => Ok((keyboard, mouse)),
                    Err(error) => { let _ = UnhookWindowsHookEx(keyboard); Err(error) }
                }
            })();
            match hooks {
                Ok((keyboard, mouse)) => {
                    if tx.send(Ok(GetCurrentThreadId())).is_ok() {
                        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    let _ = UnhookWindowsHookEx(mouse);
                    let _ = UnhookWindowsHookEx(keyboard);
                }
                Err(error) => { let _ = tx.send(Err(error.to_string())); }
            }
        })?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self { thread_id, thread: Some(thread) }),
            result => { let _ = thread.join(); anyhow::bail!("Could not block endpoint input: {result:?}") }
        }
    }
}
impl Drop for InputBlock {
    fn drop(&mut self) {
        unsafe { let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)); }
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}
unsafe extern "system" fn keyboard_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 && unsafe { (*(l.0 as *const KBDLLHOOKSTRUCT)).dwExtraInfo } != INPUT_TAG { return LRESULT(1); }
    unsafe { CallNextHookEx(None, code, w, l) }
}
unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 && unsafe { (*(l.0 as *const MSLLHOOKSTRUCT)).dwExtraInfo } != INPUT_TAG { return LRESULT(1); }
    unsafe { CallNextHookEx(None, code, w, l) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn untagged_keyboard_and_mouse_are_suppressed() {
        let keyboard = KBDLLHOOKSTRUCT::default();
        let mouse = MSLLHOOKSTRUCT::default();
        unsafe {
            assert_eq!(keyboard_hook(0, WPARAM(0), LPARAM(&keyboard as *const _ as isize)), LRESULT(1));
            assert_eq!(mouse_hook(0, WPARAM(0), LPARAM(&mouse as *const _ as isize)), LRESULT(1));
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use std::time::Duration;

    #[test]
    #[ignore = "requires an interactive Windows desktop; briefly blocks physical input"]
    fn live_input_block_allows_tagged_input_and_restores_local_input() {
        unsafe fn key(pressed: bool, tag: usize) {
            let input = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
                wVk: VK_F24, dwFlags: if pressed { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP },
                dwExtraInfo: tag, ..Default::default()
            } } };
            assert_eq!(unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) }, 1);
            thread::sleep(Duration::from_millis(100));
        }
        struct ReleaseKey;
        impl Drop for ReleaseKey { fn drop(&mut self) { unsafe { key(false, INPUT_TAG); } } }
        let _release = ReleaseKey;
        let guard = InputBlock::start().unwrap();
        unsafe {
            key(true, 0);
            assert_eq!(GetAsyncKeyState(VK_F24.0 as i32) & i16::MIN, 0, "untagged input was not blocked");
            key(true, INPUT_TAG);
            assert_ne!(GetAsyncKeyState(VK_F24.0 as i32) & i16::MIN, 0, "technician input was blocked");
            key(false, INPUT_TAG);
        }
        drop(guard);
        unsafe {
            key(true, 0);
            assert_ne!(GetAsyncKeyState(VK_F24.0 as i32) & i16::MIN, 0, "input did not recover on teardown");
            key(false, 0);
        }
    }
}
