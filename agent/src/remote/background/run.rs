//! The taskbar's Run dialog. Windows' own needs a host: `rundll32 shell32.dll,#61`
//! calls it with rundll32's entry-point arguments, and it then ignores full paths.
//! This one lives on the workspace thread, so the workspace can start what it
//! runs in its job and move keyboard input to it at once.
use crate::win32::wide;
use std::cell::Cell;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const CLASS: PCWSTR = w!("MeshRMMBackgroundRun");
const WIDTH: i32 = 412;
const HEIGHT: i32 = 232;
const EDIT: i32 = 100;
const ERROR: i32 = 101;
/// A static control that shows an icon.
const SS_ICON: u32 = 3;

/// What the dialog asked the workspace to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Action {
    #[default]
    None,
    Run,
    Close,
}

pub(super) struct Run {
    pub(super) window: HWND,
    pub(super) edit: HWND,
    error: HWND,
    font: HFONT,
    /// Set by the window procedure, which can't reach the workspace.
    action: Box<Cell<Action>>,
    /// Where keyboard input went before the dialog opened.
    pub(super) previous: HWND,
}

impl Run {
    pub(super) fn new(icon: HICON) -> anyhow::Result<Self> {
        let _styles = crate::remote::background_files::controls::VisualStyles::activate()?;
        let action = Box::new(Cell::new(Action::None));
        unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                lpszClassName: CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
            let window = CreateWindowExW(
                WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
                CLASS,
                w!("Run"),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                0,
                0,
                WIDTH,
                HEIGHT,
                None,
                None,
                None,
                None,
            )?;
            SetWindowLongPtrW(window, GWLP_USERDATA, &*action as *const _ as isize);
            let mut run = Self {
                window,
                edit: HWND::default(),
                error: HWND::default(),
                font: CreateFontW(
                    -12,
                    0,
                    0,
                    0,
                    400,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    DEFAULT_PITCH.0 as u32,
                    w!("Segoe UI"),
                ),
                action,
                previous: HWND::default(),
            };
            if !icon.is_invalid() {
                SendMessageW(
                    window,
                    WM_SETICON,
                    Some(WPARAM(ICON_BIG as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
                SendMessageW(
                    window,
                    WM_SETICON,
                    Some(WPARAM(ICON_SMALL as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
                let image = run.control(
                    w!("STATIC"),
                    "",
                    -1,
                    WINDOW_STYLE(SS_ICON),
                    (16, 16, 32, 32),
                )?;
                SendMessageW(image, STM_SETICON, Some(WPARAM(icon.0 as usize)), None);
            }
            run.control(
                w!("STATIC"),
                "Type the name of a program, folder, or document, and it will open \
                 on this background desktop.",
                -1,
                WINDOW_STYLE(0),
                (64, 16, 324, 36),
            )?;
            run.control(
                w!("STATIC"),
                "&Open:",
                -1,
                WINDOW_STYLE(0),
                (16, 71, 44, 20),
            )?;
            run.edit = run.control(
                w!("EDIT"),
                "",
                EDIT,
                WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                (64, 67, 324, 24),
            )?;
            run.control(
                w!("STATIC"),
                "It runs as SYSTEM in Session 0.",
                -1,
                WINDOW_STYLE(0),
                (64, 97, 324, 20),
            )?;
            run.error =
                run.control(w!("STATIC"), "", ERROR, WINDOW_STYLE(0), (16, 117, 372, 32))?;
            run.control(
                w!("BUTTON"),
                "OK",
                IDOK.0,
                WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
                (212, 161, 85, 26),
            )?;
            run.control(
                w!("BUTTON"),
                "Cancel",
                IDCANCEL.0,
                WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                (303, 161, 85, 26),
            )?;
            Ok(run)
        }
    }

    fn control(
        &self,
        class: PCWSTR,
        text: &str,
        id: i32,
        style: WINDOW_STYLE,
        (x, y, width, height): (i32, i32, i32, i32),
    ) -> anyhow::Result<HWND> {
        let text = wide(text);
        unsafe {
            let control = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                width,
                height,
                Some(self.window),
                Some(HMENU(id as isize as *mut _)),
                None,
                None,
            )?;
            SendMessageW(
                control,
                WM_SETFONT,
                Some(WPARAM(self.font.0 as usize)),
                Some(LPARAM(1)),
            );
            Ok(control)
        }
    }

    /// Shows the dialog above the taskbar's left end, as Windows does, with
    /// the last command selected.
    pub(super) fn show(&self, work_area: RECT) {
        unsafe {
            if !self.visible() {
                let _ = SetWindowPos(
                    self.window,
                    Some(HWND_TOP),
                    work_area.left + 12,
                    work_area.bottom - HEIGHT - 12,
                    0,
                    0,
                    SWP_NOSIZE | SWP_SHOWWINDOW,
                );
            }
            let _ = SetForegroundWindow(self.window);
            let _ = SetFocus(Some(self.edit));
            SendMessageW(self.edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
        self.set_error("");
    }

    pub(super) fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.window, SW_HIDE);
        }
    }

    pub(super) fn visible(&self) -> bool {
        unsafe { GetWindowLongPtrW(self.window, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 }
    }

    pub(super) fn take_action(&self) -> Action {
        self.action.take()
    }

    pub(super) fn command(&self) -> String {
        unsafe {
            let mut text = vec![0_u16; GetWindowTextLengthW(self.edit) as usize + 1];
            let length = GetWindowTextW(self.edit, &mut text) as usize;
            String::from_utf16_lossy(&text[..length])
        }
    }

    pub(super) fn set_error(&self, message: &str) {
        let message = wide(message);
        unsafe {
            let _ = SetWindowTextW(self.error, PCWSTR(message.as_ptr()));
            // Capture copies the window's retained surface, so a label
            // change must repaint it.
            let _ = InvalidateRect(Some(self.error), None, true);
        }
    }

    /// Gives the dialog Tab, Enter, and Esc, as a dialog manager would.
    pub(super) fn dialog_message(&self, message: &MSG) -> bool {
        unsafe {
            (message.hwnd == self.window || IsChild(self.window, message.hwnd).as_bool())
                && IsDialogMessageW(self.window, message).as_bool()
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.window, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.window);
            let _ = DeleteObject(self.font.into());
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let action = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Cell<Action>;
        match message {
            WM_COMMAND if !action.is_null() => {
                match (wparam.0 & 0xffff) as i32 {
                    id if id == IDOK.0 => (*action).set(Action::Run),
                    id if id == IDCANCEL.0 => (*action).set(Action::Close),
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                if !action.is_null() {
                    (*action).set(Action::Close);
                }
                return LRESULT(0);
            }
            WM_CTLCOLORSTATIC => {
                let dc = HDC(wparam.0 as *mut _);
                if GetDlgCtrlID(HWND(lparam.0 as *mut _)) == ERROR {
                    SetTextColor(dc, COLORREF(0x0000c0));
                }
                SetBkColor(dc, COLORREF(GetSysColor(COLOR_BTNFACE)));
                return LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
