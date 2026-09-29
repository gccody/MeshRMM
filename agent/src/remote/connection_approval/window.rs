//! The approval prompt: a card in the middle of the primary monitor's work
//! area, above other windows, with the message, the technician's reason, a
//! countdown and Accept and Deny buttons. It does not take focus when it
//! opens, so typing elsewhere cannot answer it; once clicked, Tab, Enter,
//! Space and Escape work. Closing it denies the connection.
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_FOCUS, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::{ApprovalPrompt, Decision, remaining_seconds};

const TITLE: &str = "Remote connection request";
const ACCEPT_ID: usize = 100;
const DENY_ID: usize = 101;
const COUNTDOWN_TIMER: usize = 1;
// Layout in 96-DPI pixels.
const WIDTH: i32 = 420;
const PADDING: i32 = 20;
const ACCENT: i32 = 4;
const GAP: i32 = 8;
const BUTTON_WIDTH: i32 = 92;
const BUTTON_HEIGHT: i32 = 32;
const MAX_TEXT_HEIGHT: i32 = 240;
const TITLE_FONT: i32 = 17;
const BODY_FONT: i32 = 15;
const SMALL_FONT: i32 = 13;
const BACKGROUND: COLORREF = COLORREF(0x00382b21);
const BORDER: COLORREF = COLORREF(0x0063554b);
const ACCENT_COLOR: COLORREF = COLORREF(0x00f16663);
const TITLE_COLOR: COLORREF = COLORREF(0x00ffffff);
const BODY_COLOR: COLORREF = COLORREF(0x00ebe5e2);
const MUTED_COLOR: COLORREF = COLORREF(0x00b8a394);
const PRESSED_ACCENT: COLORREF = COLORREF(0x00e5484f);
const SECONDARY_BUTTON: COLORREF = COLORREF(0x004a3a2d);
const PRESSED_SECONDARY: COLORREF = COLORREF(0x005e4b3b);

pub struct PromptWindow {
    thread_id: u32,
    #[cfg(test)]
    window: usize,
    answers: mpsc::Receiver<Decision>,
    thread: Option<JoinHandle<()>>,
}

impl PromptWindow {
    /// Shows `prompt`, counting down to `deadline`, when the connection is
    /// accepted for the user.
    pub fn show(prompt: &ApprovalPrompt, deadline: Instant) -> anyhow::Result<Self> {
        let prompt = prompt.clone();
        let (answer_tx, answers) = mpsc::sync_channel(1);
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("connection-approval".into())
            .spawn(move || unsafe {
                match create_window(&prompt, deadline, answer_tx) {
                    Ok(window) => {
                        if tx
                            .send(Ok((GetCurrentThreadId(), window.0 as usize)))
                            .is_ok()
                        {
                            run_messages(window);
                        }
                        let _ = DestroyWindow(window);
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                    }
                }
            })?;
        match rx.recv() {
            Ok(Ok((thread_id, _window))) => Ok(Self {
                thread_id,
                #[cfg(test)]
                window: _window,
                answers,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                anyhow::bail!("connection approval prompt failed to open: {result:?}")
            }
        }
    }

    /// The user's answer, waiting up to `wait` for it.
    pub fn answer(&self, wait: Duration) -> Option<Decision> {
        self.answers.recv_timeout(wait).ok()
    }
}

impl Drop for PromptWindow {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // The prompt may already have closed itself after an answer. The
            // unjoined thread keeps its ID from being reused.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = thread.join();
        }
    }
}

unsafe fn run_messages(window: HWND) {
    unsafe {
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            // Enter presses the focused button. A dialog manager would send
            // IDOK instead, which must not accept a connection.
            if message.message == WM_KEYDOWN && message.wParam.0 == usize::from(VK_RETURN.0) {
                let focus = GetFocus();
                if !focus.is_invalid() && GetParent(focus).is_ok_and(|parent| parent == window) {
                    SendMessageW(focus, BM_CLICK, None, None);
                    continue;
                }
            }
            if !IsDialogMessageW(window, &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

struct State {
    title: Vec<u16>,
    text: Vec<u16>,
    reason: Option<Vec<u16>>,
    timeout: Duration,
    deadline: Instant,
    answers: Option<mpsc::SyncSender<Decision>>,
    accept: HWND,
    deny: HWND,
    title_font: HFONT,
    body_font: HFONT,
    small_font: HFONT,
    dpi: i32,
    title_height: i32,
    text_height: i32,
    reason_height: i32,
    countdown_height: i32,
}

impl State {
    fn px(&self, value: i32) -> i32 {
        value * self.dpi / 96
    }

    fn countdown(&self) -> Vec<u16> {
        let elapsed = self
            .timeout
            .saturating_sub(self.deadline.saturating_duration_since(Instant::now()));
        let seconds = remaining_seconds(self.timeout, elapsed);
        format!(
            "Accepts automatically in {seconds} second{}.",
            if seconds == 1 { "" } else { "s" }
        )
        .encode_utf16()
        .collect()
    }

    /// Sends the user's answer once, then closes the prompt.
    fn answer(&mut self, window: HWND, decision: Decision) {
        if let Some(answers) = self.answers.take() {
            let _ = answers.try_send(decision);
        }
        unsafe {
            let _ = ShowWindow(window, SW_HIDE);
            PostQuitMessage(0);
        }
    }
}

/// Where a `width` by `height` prompt sits in the `work` area: centered, and
/// kept on screen.
fn placement(work: RECT, width: i32, height: i32) -> RECT {
    let left = (work.left + (work.right - work.left - width) / 2).max(work.left);
    let top = (work.top + (work.bottom - work.top - height) / 2).max(work.top);
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

unsafe fn font(height: i32, weight: FONT_WEIGHT) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(w!("Segoe UI").as_ptr()),
        )
    }
}

unsafe fn create_window(
    prompt: &ApprovalPrompt,
    deadline: Instant,
    answers: mpsc::SyncSender<Decision>,
) -> windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MeshRMMConnectionApproval");
        // A previous session may already have registered this process-wide class.
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        });
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
            class,
            w!("MeshRMM remote connection request"),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let button = |label: PCWSTR, id: usize| {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                label,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                0,
                0,
                0,
                0,
                Some(window),
                // For a child control this parameter is its numeric ID.
                Some(HMENU(std::ptr::without_provenance_mut(id))),
                Some(instance.into()),
                None,
            )
        };
        let buttons = button(w!("Deny"), DENY_ID)
            .and_then(|deny| button(w!("Accept"), ACCEPT_ID).map(|accept| (accept, deny)));
        let (accept, deny) = match buttons {
            Ok(buttons) => buttons,
            Err(error) => {
                let _ = DestroyWindow(window);
                return Err(error);
            }
        };
        // Unaware processes get 96 and are scaled by Windows.
        let dpi = GetDpiForSystem().max(96) as i32;
        let state = Box::new(State {
            title: TITLE.encode_utf16().collect(),
            text: prompt.text.encode_utf16().collect(),
            reason: (!prompt.reason.is_empty()).then(|| {
                format!("Reason: {}", prompt.reason)
                    .encode_utf16()
                    .collect()
            }),
            timeout: prompt.timeout,
            deadline,
            answers: Some(answers),
            accept,
            deny,
            title_font: font(TITLE_FONT * dpi / 96, FW_SEMIBOLD),
            body_font: font(BODY_FONT * dpi / 96, FW_NORMAL),
            small_font: font(SMALL_FONT * dpi / 96, FW_NORMAL),
            dpi,
            title_height: 0,
            text_height: 0,
            reason_height: 0,
            countdown_height: 0,
        });
        // WM_NCDESTROY frees the state, including on a failed placement.
        SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(state) as isize);
        if let Err(error) = place(window) {
            let _ = DestroyWindow(window);
            return Err(error);
        }
        SetTimer(Some(window), COUNTDOWN_TIMER, 1000, None);
        Ok(window)
    }
}

/// Measures `text` wrapped to `width` in `font`.
unsafe fn text_height(dc: HDC, font: HFONT, text: &mut [u16], width: i32) -> i32 {
    unsafe {
        SelectObject(dc, font.into());
        let mut rect = RECT {
            right: width,
            ..Default::default()
        };
        DrawTextW(
            dc,
            text,
            &mut rect,
            DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL | DT_CALCRECT,
        );
        rect.bottom - rect.top
    }
}

/// Sizes the prompt to its text and shows it without activating it.
unsafe fn place(window: HWND) -> windows::core::Result<()> {
    unsafe {
        let state = &mut *(GetWindowLongPtrW(window, GWLP_USERDATA) as *mut State);
        let mut work = RECT::default();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut work as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
        let width = state
            .px(WIDTH)
            .min(work.right - work.left)
            .max(state.px(PADDING * 3 + ACCENT + BUTTON_WIDTH * 2));
        let text_width = width - state.px(ACCENT + 2 * PADDING);
        let max_text = state.px(MAX_TEXT_HEIGHT).min((work.bottom - work.top) / 3);
        let dc = GetDC(Some(window));
        let old = SelectObject(dc, state.title_font.into());
        let mut title = state.title.clone();
        state.title_height = text_height(dc, state.title_font, &mut title, text_width);
        let mut text = state.text.clone();
        state.text_height = text_height(dc, state.body_font, &mut text, text_width).min(max_text);
        state.reason_height = match state.reason.clone() {
            Some(mut reason) => {
                text_height(dc, state.body_font, &mut reason, text_width).min(max_text)
            }
            None => 0,
        };
        let mut countdown = state.countdown();
        state.countdown_height = text_height(dc, state.small_font, &mut countdown, text_width);
        SelectObject(dc, old);
        ReleaseDC(Some(window), dc);
        let reason_block = if state.reason_height > 0 {
            state.px(GAP) + state.reason_height
        } else {
            0
        };
        let height = state.px(2 * PADDING + 3 * GAP + GAP + BUTTON_HEIGHT)
            + state.title_height
            + state.text_height
            + reason_block
            + state.countdown_height;
        let rect = placement(work, width, height);
        let button_top = height - state.px(PADDING + BUTTON_HEIGHT);
        let accept_left = width - state.px(PADDING + BUTTON_WIDTH);
        let deny_left = accept_left - state.px(GAP + BUTTON_WIDTH);
        for (button, left) in [(state.deny, deny_left), (state.accept, accept_left)] {
            SetWindowPos(
                button,
                None,
                left,
                button_top,
                state.px(BUTTON_WIDTH),
                state.px(BUTTON_HEIGHT),
                SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )?;
        let _ = InvalidateRect(Some(window), None, true);
        Ok(())
    }
}

unsafe fn paint(window: HWND, state: &mut State) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(window, &mut client);
        let fill = |rect: &RECT, color| {
            let brush = CreateSolidBrush(color);
            FillRect(dc, rect, brush);
            let _ = DeleteObject(brush.into());
        };
        fill(&client, BACKGROUND);
        fill(
            &RECT {
                right: state.px(ACCENT),
                ..client
            },
            ACCENT_COLOR,
        );
        let border = CreateSolidBrush(BORDER);
        FrameRect(dc, &client, border);
        let _ = DeleteObject(border.into());
        SetBkMode(dc, TRANSPARENT);
        let left = state.px(ACCENT + PADDING);
        let right = client.right - state.px(PADDING);
        let gap = state.px(GAP);
        let mut top = state.px(PADDING);
        let old = SelectObject(dc, state.title_font.into());
        let draw = |text: &mut [u16], font: HFONT, color, height: i32, top: &mut i32| {
            SelectObject(dc, font.into());
            SetTextColor(dc, color);
            DrawTextW(
                dc,
                text,
                &mut RECT {
                    left,
                    top: *top,
                    right,
                    bottom: *top + height,
                },
                DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL | DT_END_ELLIPSIS,
            );
            *top += height + gap;
        };
        let (title_font, body_font, small_font) =
            (state.title_font, state.body_font, state.small_font);
        let (title_height, text_height, reason_height, countdown_height) = (
            state.title_height,
            state.text_height,
            state.reason_height,
            state.countdown_height,
        );
        draw(
            &mut state.title,
            title_font,
            TITLE_COLOR,
            title_height,
            &mut top,
        );
        draw(
            &mut state.text,
            body_font,
            BODY_COLOR,
            text_height,
            &mut top,
        );
        if let Some(reason) = state.reason.as_mut() {
            draw(reason, body_font, MUTED_COLOR, reason_height, &mut top);
        }
        let mut countdown = state.countdown();
        draw(
            &mut countdown,
            small_font,
            MUTED_COLOR,
            countdown_height,
            &mut top,
        );
        SelectObject(dc, old);
        let _ = EndPaint(window, &ps);
    }
}

unsafe fn draw_button(state: &State, item: &DRAWITEMSTRUCT) {
    unsafe {
        let accept = item.CtlID as usize == ACCEPT_ID;
        let pressed = item.itemState.0 & ODS_SELECTED.0 != 0;
        let color = match (accept, pressed) {
            (true, false) => ACCENT_COLOR,
            (true, true) => PRESSED_ACCENT,
            (false, false) => SECONDARY_BUTTON,
            (false, true) => PRESSED_SECONDARY,
        };
        let dc = item.hDC;
        let brush = CreateSolidBrush(color);
        FillRect(dc, &item.rcItem, brush);
        let _ = DeleteObject(brush.into());
        let border = CreateSolidBrush(if accept { ACCENT_COLOR } else { BORDER });
        FrameRect(dc, &item.rcItem, border);
        let _ = DeleteObject(border.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, TITLE_COLOR);
        let old = SelectObject(dc, state.body_font.into());
        let mut label: Vec<u16> = if accept { "Accept" } else { "Deny" }
            .encode_utf16()
            .collect();
        let mut rect = item.rcItem;
        DrawTextW(
            dc,
            &mut label,
            &mut rect,
            DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old);
        if item.itemState.0 & ODS_FOCUS.0 != 0 {
            let inset = state.px(3);
            let focus = RECT {
                left: item.rcItem.left + inset,
                top: item.rcItem.top + inset,
                right: item.rcItem.right - inset,
                bottom: item.rcItem.bottom - inset,
            };
            let _ = DrawFocusRect(dc, &focus);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut State;
        match message {
            WM_COMMAND if !state.is_null() => {
                let id = wparam.0 & 0xffff;
                let notification = (wparam.0 >> 16) & 0xffff;
                match (id, notification as u32) {
                    (ACCEPT_ID, BN_CLICKED) => (*state).answer(window, Decision::Accepted),
                    (DENY_ID, BN_CLICKED) => (*state).answer(window, Decision::Declined),
                    // Escape.
                    (2, _) => (*state).answer(window, Decision::Declined),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE if !state.is_null() => {
                (*state).answer(window, Decision::Declined);
                LRESULT(0)
            }
            WM_DRAWITEM if !state.is_null() => {
                draw_button(&*state, &*(lparam.0 as *const DRAWITEMSTRUCT));
                LRESULT(1)
            }
            WM_TIMER if wparam.0 == COUNTDOWN_TIMER => {
                let _ = InvalidateRect(Some(window), None, false);
                LRESULT(0)
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE if !state.is_null() => {
                if let Err(error) = place(window) {
                    tracing::warn!(%error, "could not move the connection approval prompt");
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT if !state.is_null() => {
                paint(window, &mut *state);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                if !state.is_null() {
                    let state = Box::from_raw(state);
                    let _ = DeleteObject(state.title_font.into());
                    let _ = DeleteObject(state.body_font.into());
                    let _ = DeleteObject(state.small_font.into());
                }
                DefWindowProcW(window, message, wparam, lparam)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(reason: &str) -> ApprovalPrompt {
        ApprovalPrompt {
            text: "Ada Lovelace would like to connect.".into(),
            reason: reason.into(),
            timeout: Duration::from_secs(30),
            lock_idle: Duration::from_secs(60),
        }
    }

    fn show(reason: &str) -> PromptWindow {
        let prompt = prompt(reason);
        PromptWindow::show(&prompt, Instant::now() + prompt.timeout).unwrap()
    }

    // IsWindowVisible also checks the desktop, which is hidden when tests run
    // in a non-interactive session (e.g. over SSH), so read the window's style.
    fn visible(window: HWND) -> bool {
        unsafe {
            IsWindow(Some(window)).as_bool()
                && GetWindowLongW(window, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0
        }
    }

    fn button(window: HWND, id: usize) -> HWND {
        unsafe { GetDlgItem(Some(window), id as i32) }.unwrap()
    }

    #[test]
    fn placement_centers_the_prompt_and_keeps_it_on_screen() {
        let work = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1040,
        };
        assert_eq!(
            placement(work, 420, 200),
            RECT {
                left: -1170,
                top: 420,
                right: -750,
                bottom: 620,
            }
        );
        let tiny = RECT {
            left: 0,
            top: 0,
            right: 300,
            bottom: 80,
        };
        let rect = placement(tiny, 420, 200);
        assert_eq!((rect.left, rect.top), (0, 0), "the text must stay visible");
    }

    #[test]
    fn shows_centered_without_focus_with_labelled_buttons() {
        let prompt =
            show("Printer queue\nticket 42, which is long enough to wrap onto another line");
        let window = HWND(prompt.window as *mut _);
        unsafe {
            assert!(visible(window));
            assert_ne!(GetForegroundWindow(), window, "it must not take focus");
            let mut work = RECT::default();
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&mut work as *mut RECT).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .unwrap();
            let mut rect = RECT::default();
            GetWindowRect(window, &mut rect).unwrap();
            assert!(rect.left >= work.left && rect.right <= work.right);
            assert!(rect.top >= work.top && rect.bottom <= work.bottom);
            for (id, label) in [(ACCEPT_ID, "Accept"), (DENY_ID, "Deny")] {
                let button = button(window, id);
                assert!(visible(button));
                let mut text = [0u16; 16];
                let length = GetWindowTextW(button, &mut text) as usize;
                assert_eq!(String::from_utf16_lossy(&text[..length]), label);
            }
            let plain = show("");
            let mut short = RECT::default();
            GetWindowRect(HWND(plain.window as *mut _), &mut short).unwrap();
            assert!(
                rect.bottom - rect.top > short.bottom - short.top,
                "a reason must make the prompt taller"
            );
        }
        assert_eq!(prompt.answer(Duration::ZERO), None);
        drop(prompt);
        assert!(!unsafe { IsWindow(Some(window)) }.as_bool());
    }

    #[test]
    fn each_button_answers_once_and_closes_the_prompt() {
        for (id, decision) in [
            (ACCEPT_ID, Decision::Accepted),
            (DENY_ID, Decision::Declined),
        ] {
            let prompt = show("");
            let window = HWND(prompt.window as *mut _);
            unsafe {
                PostMessageW(
                    Some(window),
                    WM_COMMAND,
                    WPARAM(id | ((BN_CLICKED as usize) << 16)),
                    LPARAM(button(window, id).0 as isize),
                )
                .unwrap();
            }
            assert_eq!(prompt.answer(Duration::from_secs(2)), Some(decision));
            assert_eq!(prompt.answer(Duration::from_millis(50)), None);
        }
    }

    #[test]
    fn escape_or_closing_denies_but_enter_without_a_button_does_nothing() {
        let prompt = show("");
        let window = HWND(prompt.window as *mut _);
        unsafe {
            PostMessageW(Some(window), WM_COMMAND, WPARAM(1), LPARAM(0)).unwrap();
        }
        assert_eq!(
            prompt.answer(Duration::from_millis(200)),
            None,
            "IDOK must not accept"
        );
        unsafe {
            PostMessageW(Some(window), WM_COMMAND, WPARAM(2), LPARAM(0)).unwrap();
        }
        assert_eq!(
            prompt.answer(Duration::from_secs(2)),
            Some(Decision::Declined)
        );
        let closed = show("");
        unsafe {
            PostMessageW(
                Some(HWND(closed.window as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
            .unwrap();
        }
        assert_eq!(
            closed.answer(Duration::from_secs(2)),
            Some(Decision::Declined)
        );
    }
}
