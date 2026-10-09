use super::*;
use crate::toolbar::{self, Command, MenuEntry, Rect};
use windows::Win32::Foundation::POINT;

impl WindowContext {
    /// Shows `entries` under the item at `rect` and returns the chosen
    /// command.
    pub(super) unsafe fn show_menu(
        &self,
        window: HWND,
        entries: &[MenuEntry],
        rect: Rect,
    ) -> Option<Command> {
        let menu = unsafe { CreatePopupMenu() }.ok()?;
        let mut next = 0;
        unsafe { append_entries(menu, entries, &mut next) };
        let anchor = self.device_rect(rect);
        let mut point = POINT {
            x: anchor.left,
            y: anchor.bottom + self.px(2),
        };
        let _ = unsafe { ClientToScreen(self.controls().toolbar, &mut point) };
        let chosen = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                point.x,
                point.y,
                None,
                window,
                None,
            )
        }
        .0;
        let _ = unsafe { DestroyMenu(menu) };
        let index = usize::try_from(chosen).ok()?.checked_sub(1)?;
        toolbar::commands(entries).get(index).copied().flatten()
    }
}

/// Appends `entries` to `menu`. Each gets the ID one past its place in
/// [`toolbar::commands`], which counts submenus and their entries in order;
/// zero is no choice. Destroying `menu` destroys its submenus.
unsafe fn append_entries(menu: HMENU, entries: &[MenuEntry], next: &mut usize) {
    for entry in entries {
        let id = *next + 1;
        *next += 1;
        unsafe {
            let _ = match entry {
                MenuEntry::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
                MenuEntry::Item {
                    label,
                    checked,
                    enabled,
                    command,
                } => {
                    let mut flags = MF_STRING;
                    if *checked {
                        flags |= MF_CHECKED;
                    }
                    if !*enabled || command.is_none() {
                        flags |= MF_GRAYED;
                    }
                    AppendMenuW(menu, flags, id, &HSTRING::from(menu_label(label)))
                }
                MenuEntry::Submenu { label, entries } => match CreatePopupMenu() {
                    Ok(submenu) => {
                        append_entries(submenu, entries, next);
                        AppendMenuW(
                            menu,
                            MF_POPUP | MF_STRING,
                            submenu.0 as usize,
                            &HSTRING::from(menu_label(label)),
                        )
                    }
                    Err(error) => Err(error),
                },
            };
        }
    }
}

/// A label shown as written: `&` marks a menu's access key otherwise.
fn menu_label(label: &str) -> String {
    label.replace('&', "&&")
}
