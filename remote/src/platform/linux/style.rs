//! How the viewer's controls look: dark, in the toolbar's colors, the same
//! on every desktop whatever its theme.

use crate::toolbar;

pub(super) fn color(color: toolbar::Color) -> egui::Color32 {
    egui::Color32::from_rgb(color.0, color.1, color.2)
}

/// The surface of menus, panels and dialogs.
pub(super) const SURFACE: egui::Color32 = egui::Color32::from_rgb(0x24, 0x24, 0x27);
pub(super) const TEXT: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xe8, 0xea);
pub(super) const WEAK_TEXT: egui::Color32 = egui::Color32::from_rgb(0x9a, 0x9a, 0xa0);
pub(super) const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4d, 0xa3, 0xff);
const HOVER: egui::Color32 = egui::Color32::from_rgb(0x34, 0x34, 0x39);
const PRESSED: egui::Color32 = egui::Color32::from_rgb(0x40, 0x40, 0x46);
const BUTTON: egui::Color32 = egui::Color32::from_rgb(0x2f, 0x2f, 0x33);

/// The font sizes of the controls, in points.
pub(super) const BODY: f32 = 13.0;
pub(super) const SMALL: f32 = 11.5;
pub(super) const HEADING: f32 = 16.0;

pub(super) fn apply(context: &egui::Context) {
    // The desktop's light or dark setting does not change the viewer.
    context.set_theme(egui::ThemePreference::Dark);
    let border = egui::Stroke::new(1.0, color(toolbar::SEPARATOR));
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(WEAK_TEXT);
    visuals.panel_fill = color(toolbar::BACKGROUND);
    visuals.window_fill = SURFACE;
    visuals.window_stroke = border;
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x14, 0x14, 0x16);
    visuals.selection.bg_fill = egui::Color32::from_rgb(0x1f, 0x3a, 0x5c);
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.window_corner_radius = egui::CornerRadius::same(8);
    visuals.menu_corner_radius = egui::CornerRadius::same(8);
    let radius = egui::CornerRadius::same(toolbar::RADIUS as u8);
    for (widget, fill) in [
        (&mut visuals.widgets.noninteractive, SURFACE),
        (&mut visuals.widgets.inactive, BUTTON),
        (&mut visuals.widgets.hovered, HOVER),
        (&mut visuals.widgets.active, PRESSED),
        (&mut visuals.widgets.open, PRESSED),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.corner_radius = radius;
        widget.bg_stroke = egui::Stroke::NONE;
        widget.fg_stroke = egui::Stroke::new(1.0, TEXT);
        widget.expansion = 0.0;
    }
    visuals.widgets.noninteractive.bg_stroke = border;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, WEAK_TEXT);
    context.set_visuals_of(egui::Theme::Dark, visuals);
    context.all_styles_mut(|style| {
        for (text_style, size) in [
            (egui::TextStyle::Small, SMALL),
            (egui::TextStyle::Body, BODY),
            (egui::TextStyle::Button, BODY),
            (egui::TextStyle::Monospace, 12.0),
            (egui::TextStyle::Heading, HEADING),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.menu_margin = egui::Margin::same(5);
        style.spacing.interact_size.y = 26.0;
        // Text is read, not selected, except where a control says so.
        style.interaction.selectable_labels = false;
    });
}
