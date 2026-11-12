//! The rows of the viewer's menus: the toolbar's, which
//! [`crate::toolbar::menu`] describes, and the session controls.

use egui::{Pos2, Sense, Stroke};

use super::style::{BODY, SMALL, TEXT, WEAK_TEXT};
use crate::toolbar::{Command, MenuEntry};

/// The room a row leaves for its checkmark.
const GUTTER: f32 = 20.0;
/// The room after a row's text, so the longest row does not end at the edge.
const TRAILING: f32 = 12.0;
const DISABLED: egui::Color32 = egui::Color32::from_rgb(0x68, 0x68, 0x6c);

/// A menu item: `checked` is `None` in a menu without checkmarks.
pub(super) fn row(
    ui: &mut egui::Ui,
    label: &str,
    checked: Option<bool>,
    enabled: bool,
) -> egui::Response {
    let gutter = if checked.is_some() { GUTTER } else { 0.0 };
    let padding = ui.spacing().button_padding;
    let color = if enabled { TEXT } else { DISABLED };
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), egui::FontId::proportional(BODY), color);
    let wanted = gutter + galley.size().x + 2.0 * padding.x + TRAILING;
    // The first pass measures the menu; then each row is as wide as it.
    let width = if ui.is_sizing_pass() {
        wanted
    } else {
        wanted.max(ui.available_width())
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, ui.spacing().interact_size.y),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if enabled && response.hovered() {
            let visuals = &ui.visuals().widgets.hovered;
            painter.rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
        }
        if checked == Some(true) {
            let center = Pos2::new(rect.left() + padding.x + 6.0, rect.center().y);
            painter.add(egui::Shape::line(
                vec![
                    center + egui::vec2(-4.5, 0.0),
                    center + egui::vec2(-1.5, 3.5),
                    center + egui::vec2(5.0, -4.0),
                ],
                Stroke::new(1.6, color),
            ));
        }
        painter.galley(
            Pos2::new(
                rect.left() + padding.x + gutter,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            color,
        );
    }
    response
}

/// A line of the menu that says something and does nothing.
pub(super) fn status(ui: &mut egui::Ui, text: &str) {
    let padding = ui.spacing().button_padding;
    ui.horizontal(|ui| {
        ui.add_space(padding.x);
        ui.label(egui::RichText::new(text).color(WEAK_TEXT));
        ui.add_space(padding.x);
    });
}

/// The title of a section of the session controls.
pub(super) fn header(ui: &mut egui::Ui, title: &str) {
    let padding = ui.spacing().button_padding;
    ui.horizontal(|ui| {
        ui.add_space(padding.x);
        ui.label(
            egui::RichText::new(title)
                .size(SMALL)
                .strong()
                .color(WEAK_TEXT),
        );
    });
}

/// An item that opens a nested menu. `gutter` lines its label up with the
/// labels of rows that have a checkmark.
pub(super) fn submenu<R>(
    ui: &mut egui::Ui,
    label: &str,
    gutter: bool,
    enabled: bool,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    if !enabled {
        row(ui, label, gutter.then_some(false), false);
        return None;
    }
    let button = if gutter {
        egui::containers::menu::SubMenuButton::new((
            egui::Atom::custom(
                egui::IdSalt::new("menu gutter"),
                egui::vec2(GUTTER - ui.spacing().icon_spacing, 1.0),
            ),
            label,
        ))
    } else {
        egui::containers::menu::SubMenuButton::new(label)
    };
    button.ui(ui, content).1.map(|shown| shown.inner)
}

/// Shows `entries` and returns the command of the item that was chosen.
pub(super) fn show(ui: &mut egui::Ui, entries: &[MenuEntry]) -> Option<Command> {
    let gutter = entries
        .iter()
        .any(|entry| matches!(entry, MenuEntry::Item { checked: true, .. }));
    let mut chosen = None;
    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                ui.separator();
            }
            MenuEntry::Item {
                label,
                command: None,
                ..
            } => status(ui, label),
            MenuEntry::Item {
                label,
                checked,
                enabled,
                command: Some(command),
            } => {
                if row(ui, label, gutter.then_some(*checked), *enabled).clicked() {
                    chosen = Some(*command);
                }
            }
            MenuEntry::Submenu {
                label,
                entries: nested,
            } => {
                if let Some(command) =
                    submenu(ui, label, gutter, true, |ui| show(ui, nested)).flatten()
                {
                    chosen = Some(command);
                }
            }
        }
    }
    chosen
}
