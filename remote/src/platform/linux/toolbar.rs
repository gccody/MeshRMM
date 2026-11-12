//! Paints the viewer's toolbar, which [`crate::toolbar`] lays out and
//! colors, and reports the mouse over its items.

use egui::{Color32, Pos2, Sense, Stroke};

use super::style::color;
use crate::toolbar::{self, Icon, Item, Paint, Segment};

/// The lines a curve is drawn as.
const CURVE_STEPS: usize = 8;

fn font() -> egui::FontId {
    egui::FontId::proportional(toolbar::FONT_SIZE as f32)
}

fn position(x: f64, y: f64) -> Pos2 {
    Pos2::new(x as f32, y as f32)
}

pub(super) fn rect(rect: toolbar::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        position(rect.x, rect.y),
        egui::vec2(rect.width as f32, rect.height as f32),
    )
}

/// The outlines `segments` describe, each with whether it is closed.
fn outlines(segments: &[Segment]) -> Vec<(Vec<Pos2>, bool)> {
    fn finish(outlines: &mut Vec<(Vec<Pos2>, bool)>, points: &mut Vec<Pos2>, closed: bool) {
        let mut points = std::mem::take(points);
        // A closed outline ends where it began; the point is drawn once.
        if closed
            && points.len() > 2
            && points
                .first()
                .zip(points.last())
                .is_some_and(|(first, last)| first.distance(*last) < 0.01)
        {
            points.pop();
        }
        if points.len() > 1 {
            outlines.push((points, closed));
        }
    }
    let mut outlines = Vec::new();
    let mut points = Vec::new();
    for segment in segments {
        match *segment {
            Segment::Move(x, y) => {
                finish(&mut outlines, &mut points, false);
                points.push(position(x, y));
            }
            Segment::Line(x, y) => points.push(position(x, y)),
            Segment::Cubic(ax, ay, bx, by, x, y) => {
                let start = points.last().copied().unwrap_or_else(|| position(ax, ay));
                let (a, b, end) = (position(ax, ay), position(bx, by), position(x, y));
                for step in 1..=CURVE_STEPS {
                    let t = step as f32 / CURVE_STEPS as f32;
                    let u = 1.0 - t;
                    points.push(Pos2::new(
                        u * u * u * start.x
                            + 3.0 * u * u * t * a.x
                            + 3.0 * u * t * t * b.x
                            + t * t * t * end.x,
                        u * u * u * start.y
                            + 3.0 * u * u * t * a.y
                            + 3.0 * u * t * t * b.y
                            + t * t * t * end.y,
                    ));
                }
            }
            Segment::Close => finish(&mut outlines, &mut points, true),
        }
    }
    finish(&mut outlines, &mut points, false);
    outlines
}

/// Strokes or fills `segments`. Everything the toolbar fills is convex.
pub(super) fn paint(painter: &egui::Painter, segments: &[Segment], paint: Paint, color: Color32) {
    for (points, closed) in outlines(segments) {
        match paint {
            Paint::Stroke(width) => {
                let stroke = Stroke::new(width as f32, color);
                painter.add(if closed {
                    egui::Shape::closed_line(points, stroke)
                } else {
                    egui::Shape::line(points, stroke)
                });
            }
            Paint::Fill => {
                painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
            }
        }
    }
}

fn paint_icon(painter: &egui::Painter, icon: Icon, area: toolbar::Rect, color: Color32) {
    for shape in toolbar::icon(icon) {
        let (segments, shape_paint) = toolbar::place(&shape, area);
        paint(painter, &segments, shape_paint, color);
    }
}

/// The toolbar as painted: its items with the mouse's state over each, and
/// the rest of it, which moves the window.
pub(super) struct Shown {
    pub(super) items: Vec<(Item, egui::Response)>,
    pub(super) background: egui::Response,
}

/// Paints the toolbar across the top of a window `width` points wide.
/// `open` says whether the menu an item opened is showing.
pub(super) fn show(
    ui: &egui::Ui,
    state: &toolbar::State,
    width: f32,
    open: &dyn Fn(&egui::Response) -> bool,
) -> Shown {
    let painter = ui.painter();
    let bar = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(width, toolbar::HEIGHT as f32));
    // Under the items, which take the mouse where they are.
    let background = ui.interact(bar, ui.scope_id().with("toolbar"), Sense::click_and_drag());
    painter.rect_filled(bar, 0.0, color(toolbar::BACKGROUND));
    painter.line_segment(
        [
            Pos2::new(0.0, bar.bottom() - 0.5),
            Pos2::new(width, bar.bottom() - 0.5),
        ],
        Stroke::new(1.0, color(toolbar::BORDER)),
    );

    let items = toolbar::items(state);
    let measure = |text: &str| {
        f64::from(
            painter
                .layout_no_wrap(text.to_owned(), font(), Color32::WHITE)
                .size()
                .x,
        )
    };
    let layout = toolbar::layout(&items, f64::from(width), 0.0, &measure);
    for &x in &layout.separators {
        let x = x as f32 - 0.5;
        painter.line_segment(
            [Pos2::new(x, 12.0), Pos2::new(x, 28.0)],
            Stroke::new(1.0, color(toolbar::SEPARATOR)),
        );
    }

    let mut shown = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let area = layout.rects[index];
        let label = layout.labels.get(index).cloned().flatten();
        let response = ui.interact(
            rect(area),
            ui.scope_id()
                .with(("toolbar item", format!("{:?}", item.action))),
            if item.enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        let open = open(&response);
        let style = toolbar::style(
            &item,
            response.hovered() || open,
            response.is_pointer_button_down_on() || open,
        );
        if let Some(background) = style.background {
            paint(
                painter,
                &toolbar::rounded_rect(area, style.radius),
                Paint::Fill,
                color(background),
            );
        }
        let galley = label
            .filter(|label| !label.is_empty())
            .map(|label| painter.layout_no_wrap(label, font(), color(style.label)));
        let text_width = galley
            .as_ref()
            .map_or(0.0, |galley| f64::from(galley.size().x));
        let parts = toolbar::parts(&item, area, text_width);
        paint_icon(painter, item.icon, parts.icon, color(style.foreground));
        if let (Some(area), Some(galley)) = (parts.label, galley) {
            let top = area.y as f32 + (area.height as f32 - galley.size().y) / 2.0;
            painter.galley(Pos2::new(area.x as f32, top), galley, color(style.label));
        }
        if let Some(chevron) = parts.chevron {
            paint_icon(painter, Icon::Chevron, chevron, color(style.foreground));
        }
        if let (Some((x, y)), Some(badge)) = (parts.badge, style.badge) {
            paint(
                painter,
                &toolbar::circle(x, y, style.badge_radius + 1.5),
                Paint::Fill,
                color(style.badge_ring),
            );
            paint(
                painter,
                &toolbar::circle(x, y, style.badge_radius),
                Paint::Fill,
                color(badge),
            );
        }
        let response = if open {
            response
        } else {
            response.on_hover_text(item.tooltip.clone())
        };
        shown.push((item, response));
    }
    Shown {
        items: shown,
        background,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outline_is_closed_once_and_curves_become_lines() {
        let circle = outlines(&toolbar::circle(10.0, 10.0, 4.0));
        assert_eq!(circle.len(), 1);
        let (points, closed) = &circle[0];
        assert!(closed);
        // Four quarter arcs, without the repeated first point.
        assert_eq!(points.len(), 4 * CURVE_STEPS);
        for point in points {
            let radius = point.distance(Pos2::new(10.0, 10.0));
            assert!((radius - 4.0).abs() < 0.05, "radius {radius}");
        }
    }

    #[test]
    fn each_move_starts_another_outline() {
        let lines = outlines(&[
            Segment::Move(0.0, 0.0),
            Segment::Line(4.0, 0.0),
            Segment::Move(0.0, 2.0),
            Segment::Line(4.0, 2.0),
            Segment::Line(4.0, 6.0),
            // A point alone draws nothing.
            Segment::Move(9.0, 9.0),
        ]);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].0.len(), lines[0].1), (2, false));
        assert_eq!((lines[1].0.len(), lines[1].1), (3, false));
    }
}
