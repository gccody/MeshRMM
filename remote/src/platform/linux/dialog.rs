//! The viewer's small windows: what the launch is waiting on, a notice to
//! read, and a finished toolbox run's output.

use std::sync::mpsc::SyncSender;
use std::time::Instant;

use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::window::{WindowAttributes, WindowId};

use super::gpu::{Canvas, Gpu};
use super::style::WEAK_TEXT;

pub(super) enum Kind {
    /// Shown from launch until the remote desktop appears.
    Connecting { message: String, cancellable: bool },
    /// A message the technician dismisses. `closed` hears of it.
    Notice {
        title: String,
        message: String,
        closed: Option<SyncSender<()>>,
    },
    /// Text to read, select and copy.
    Output { title: String, text: String },
}

impl Kind {
    pub(super) fn attributes(&self) -> WindowAttributes {
        match self {
            Self::Connecting { .. } => super::app::window_attributes("MeshRMM Remote")
                .with_inner_size(LogicalSize::new(420.0, 150.0))
                .with_resizable(false),
            Self::Notice { title, message, .. } => {
                // Tall enough for the message, up to where it scrolls.
                let lines = message
                    .lines()
                    .map(|line| 1 + line.chars().count() / 60)
                    .sum::<usize>()
                    .max(1);
                let height = (120.0 + 19.0 * lines as f64).clamp(150.0, 420.0);
                super::app::window_attributes(title)
                    .with_inner_size(LogicalSize::new(460.0, height))
                    .with_resizable(false)
            }
            Self::Output { title, .. } => super::app::window_attributes(title)
                .with_inner_size(LogicalSize::new(720.0, 460.0))
                .with_min_inner_size(LogicalSize::new(360.0, 200.0)),
        }
    }
}

pub(super) struct Dialog {
    canvas: Canvas,
    kind: Kind,
}

impl Dialog {
    pub(super) fn new(canvas: Canvas, kind: Kind) -> Self {
        canvas.window.focus_window();
        Self { canvas, kind }
    }

    pub(super) fn window_id(&self) -> WindowId {
        self.canvas.window.id()
    }

    pub(super) fn kind(&self) -> &Kind {
        &self.kind
    }

    /// Changes what a connecting window says and whether it can be
    /// cancelled.
    pub(super) fn set_status(&mut self, new_message: String, new_cancellable: bool) {
        if let Kind::Connecting {
            message,
            cancellable,
        } = &mut self.kind
        {
            *message = new_message;
            *cancellable = new_cancellable;
            self.canvas.invalidate();
        }
    }

    /// Asks for a redraw when one is due. Returns when to ask again.
    pub(super) fn poll(&mut self, now: Instant) -> Instant {
        self.canvas.poll(now)
    }

    /// Returns whether the technician dismissed the window: closed it,
    /// chose its button, or pressed Esc.
    pub(super) fn window_event(&mut self, gpu: &Gpu, event: WindowEvent) -> bool {
        match event {
            WindowEvent::RedrawRequested => return self.paint(gpu),
            WindowEvent::CloseRequested => return true,
            WindowEvent::Resized(_) => self.canvas.resized(gpu),
            event => {
                let window = std::sync::Arc::clone(&self.canvas.window);
                let _ = self.canvas.input.on_window_event(&window, &event);
            }
        }
        self.canvas.invalidate();
        false
    }

    fn paint(&mut self, gpu: &Gpu) -> bool {
        let kind = &self.kind;
        let mut dismissed = false;
        let background = super::style::color(crate::toolbar::BACKGROUND);
        let [red, green, blue, _] = background.to_normalized_gamma_f32();
        self.canvas.paint(
            gpu,
            wgpu::Color {
                r: f64::from(red),
                g: f64::from(green),
                b: f64::from(blue),
                a: 1.0,
            },
            |ui| {
                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    dismissed = true;
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(background).inner_margin(16))
                    .show(ui, |ui| dismissed |= contents(ui, kind));
            },
            |_, _| {},
        );
        dismissed
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Kind::Notice {
            closed: Some(closed),
            ..
        } = &self.kind
        {
            let _ = closed.try_send(());
        }
    }
}

/// Draws the window. Returns whether its button was chosen.
fn contents(ui: &mut egui::Ui, kind: &Kind) -> bool {
    let mut chosen = false;
    match kind {
        Kind::Connecting {
            message,
            cancellable,
        } => {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.spinner();
                ui.add_space(8.0);
                ui.label(message);
                ui.add_space(10.0);
                chosen = ui
                    .add_enabled(*cancellable, egui::Button::new("Cancel"))
                    .clicked();
            });
        }
        Kind::Notice { message, .. } => {
            // The window's title says what the message is about, and the
            // button keeps its place under a long one.
            egui::Panel::bottom("buttons")
                .frame(egui::Frame::new())
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ok = ui.button("OK");
                        chosen =
                            ok.clicked() || ui.input(|input| input.key_pressed(egui::Key::Enter));
                    });
                });
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                });
        }
        Kind::Output { text, .. } => {
            egui::Panel::bottom("buttons")
                .frame(egui::Frame::new())
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                        ui.label(
                            egui::RichText::new("Select text to copy part of it.").color(WEAK_TEXT),
                        );
                    });
                });
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).monospace())
                            .extend()
                            .selectable(true),
                    );
                });
        }
    }
    chosen
}
