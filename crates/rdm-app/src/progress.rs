//! Per-download windows: live progress, and the "download complete" dialog.
//! Each is a real OS window (an egui viewport), as in IDM.

use eframe::egui::{self, vec2, Color32, RichText};
use rdm_core::util::{eta_secs, format_duration, format_size, format_speed};
use rdm_core::{Id, Manager, Status, View};

use crate::theme::{GREEN, ORANGE, RED};
use crate::widgets::segment_bar;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Status,
    Limiter,
    OnCompletion,
}

pub struct ProgressWindow {
    pub id: Id,
    tab: Tab,
    show_details: bool,
    limit_on: bool,
    limit_kb: u64,
    /// "Show the download-complete dialog" for this download.
    pub announce: bool,
}

fn percent(v: &View) -> Option<f32> {
    v.d.size.filter(|s| *s > 0).map(|s| v.d.downloaded as f32 * 100.0 / s as f32)
}

pub fn status_text(v: &View) -> (String, Color32) {
    match v.d.status {
        Status::Running => match &v.note {
            Some(note) => (note.clone(), ORANGE),
            None => ("Receiving data…".into(), GREEN),
        },
        Status::Paused => ("Paused".into(), ORANGE),
        Status::Queued => ("Waiting to start".into(), Color32::GRAY),
        Status::Complete => ("Complete".into(), GREEN),
        Status::Failed => (format!("Error: {}", v.d.error.as_deref().unwrap_or("unknown")), RED),
    }
}

impl ProgressWindow {
    pub fn new(id: Id, view: Option<&View>, announce: bool) -> Self {
        let limit = view.map(|v| v.d.speed_limit).unwrap_or(0);
        Self { id, tab: Tab::Status, show_details: true, limit_on: limit > 0, limit_kb: (limit / 1024).max(1), announce }
    }

    /// Returns false once the window should go away.
    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager) -> bool {
        let Some(v) = manager.view(self.id) else { return false };
        let segments = manager.segments(self.id);
        let title = match percent(&v) {
            Some(p) => format!("{:.0}% {}", p.floor(), v.d.file_name),
            None => v.d.file_name.clone(),
        };
        let height = if self.show_details && self.tab == Tab::Status { 500.0 } else { 300.0 };
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([580.0, height])
            .with_min_inner_size([500.0, 280.0]);
        let mut keep = true;
        ctx.show_viewport_immediate(egui::ViewportId(egui::Id::new(("progress", self.id))), builder, |ui, _class| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Status, "Download status");
                    ui.selectable_value(&mut self.tab, Tab::Limiter, "Speed Limiter");
                    ui.selectable_value(&mut self.tab, Tab::OnCompletion, "Options on completion");
                });
                ui.separator();
                match self.tab {
                    Tab::Status => self.status_tab(ui, manager, &v, &segments, &mut keep),
                    Tab::Limiter => self.limiter_tab(ui, manager),
                    Tab::OnCompletion => {
                        ui.add_space(6.0);
                        ui.checkbox(&mut self.announce, "Show the download complete dialog");
                        ui.label(RichText::new("Applies to this download only. The default is set in Options, Downloads tab.").weak());
                    }
                }
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                // Closing the window only hides it; the transfer carries on.
                keep = false;
            }
        });
        keep
    }

    fn status_tab(&mut self, ui: &mut egui::Ui, manager: &Manager, v: &View, segments: &[rdm_core::segment::SegSnapshot], keep: &mut bool) {
        let (status, status_color) = status_text(v);
        egui::Grid::new("facts").num_columns(2).spacing([14.0, 5.0]).show(ui, |ui| {
            ui.label("Address");
            ui.add(egui::Label::new(&v.d.spec.url).truncate());
            ui.end_row();

            ui.label("Status");
            ui.add(egui::Label::new(RichText::new(status).color(status_color)).truncate());
            ui.end_row();

            ui.label("File size");
            ui.label(v.d.size.map(format_size).unwrap_or_else(|| "Unknown".into()));
            ui.end_row();

            ui.label("Downloaded");
            ui.label(match percent(v) {
                Some(p) => format!("{} ({:.2} %)", format_size(v.d.downloaded), p),
                None => format_size(v.d.downloaded),
            });
            ui.end_row();

            ui.label("Transfer rate");
            ui.label(if v.d.status == Status::Running { format_speed(v.speed) } else { "—".into() });
            ui.end_row();

            ui.label("Time left");
            ui.label(match eta_secs(v.d.size, v.d.downloaded, v.speed) {
                Some(s) if v.d.status == Status::Running => format_duration(s),
                _ => "—".into(),
            });
            ui.end_row();

            ui.label("Resume capability");
            match v.d.resumable {
                Some(true) => ui.label(RichText::new("Yes").color(GREEN)),
                Some(false) => ui.label(RichText::new("No — pausing will restart this download").color(RED)),
                None => ui.label("Unknown"),
            };
            ui.end_row();
        });

        ui.add_space(6.0);
        segment_bar(ui, v.d.size, v.d.downloaded, segments, 22.0);
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            let label = if self.show_details { "<< Hide details" } else { "Show details >>" };
            if ui.button(label).clicked() {
                self.show_details = !self.show_details;
                let h = if self.show_details { 500.0 } else { 300.0 };
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(580.0, h)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_sized([84.0, 24.0], egui::Button::new("Cancel")).on_hover_text("Stop the download and close this window").clicked() {
                    manager.pause(self.id);
                    *keep = false;
                }
                match v.d.status {
                    Status::Running => {
                        if ui.add_sized([84.0, 24.0], egui::Button::new("Pause")).clicked() {
                            manager.pause(self.id);
                        }
                    }
                    Status::Complete => {}
                    _ => {
                        if ui.add_sized([84.0, 24.0], egui::Button::new("Resume")).clicked() {
                            manager.start(self.id);
                        }
                    }
                }
            });
        });

        if self.show_details {
            ui.add_space(4.0);
            ui.label(format!("Connections: {}", v.connections));
            let active: Vec<_> = segments.iter().filter(|s| s.owned).collect();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("conns").num_columns(3).striped(true).spacing([24.0, 3.0]).min_col_width(40.0).show(ui, |ui| {
                    ui.label(RichText::new("N.").strong());
                    ui.label(RichText::new("Downloaded").strong());
                    ui.label(RichText::new("Info").strong());
                    ui.end_row();
                    for (i, s) in active.iter().enumerate() {
                        ui.label((i + 1).to_string());
                        ui.label(format_size(s.pos - s.start));
                        let left = s.end.saturating_sub(s.pos);
                        ui.label(if s.speed > 0 {
                            format!("Receiving data… {}  ({} left)", format_speed(s.speed), format_size(left))
                        } else {
                            "Connecting…".to_string()
                        });
                        ui.end_row();
                    }
                });
            });
        }
    }

    fn limiter_tab(&mut self, ui: &mut egui::Ui, manager: &Manager) {
        ui.add_space(6.0);
        let mut changed = ui.checkbox(&mut self.limit_on, "Limit the speed of this download").changed();
        ui.horizontal(|ui| {
            ui.label("Maximum download speed");
            changed |= ui.add_enabled(self.limit_on, egui::DragValue::new(&mut self.limit_kb).range(1..=1_000_000).suffix(" KB/s")).changed();
        });
        ui.label(RichText::new("The global limiter (toolbar / Options) applies on top of this.").weak());
        if changed {
            manager.set_speed_limit(self.id, if self.limit_on { self.limit_kb * 1024 } else { 0 });
        }
    }
}

pub struct CompleteDialog {
    pub id: Id,
}

impl CompleteDialog {
    /// Returns false once dismissed. `dont_show_again` is set when the user ticks the box.
    pub fn show(&mut self, ctx: &egui::Context, manager: &Manager, dont_show_again: &mut bool) -> bool {
        let Some(v) = manager.view(self.id) else { return false };
        let path = v.d.path();
        let builder = egui::ViewportBuilder::default()
            .with_title("Download complete")
            .with_inner_size([520.0, 190.0])
            .with_resizable(false);
        let mut keep = true;
        ctx.show_viewport_immediate(egui::ViewportId(egui::Id::new(("complete", self.id))), builder, |ui, _class| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label(RichText::new("Download complete").heading().color(GREEN));
                ui.add_space(4.0);
                egui::Grid::new("done").num_columns(2).spacing([14.0, 5.0]).show(ui, |ui| {
                    ui.label("Address");
                    ui.add(egui::Label::new(&v.d.spec.url).truncate());
                    ui.end_row();
                    ui.label("Size");
                    ui.label(v.d.size.map(format_size).unwrap_or_default());
                    ui.end_row();
                    ui.label("Saved to");
                    ui.add(egui::Label::new(path.display().to_string()).truncate());
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_sized([90.0, 24.0], egui::Button::new("Open")).clicked() {
                        let _ = open::that_detached(&path);
                        keep = false;
                    }
                    if ui.add_sized([110.0, 24.0], egui::Button::new("Open folder")).clicked() {
                        reveal_in_folder(&path);
                        keep = false;
                    }
                    if ui.add_sized([90.0, 24.0], egui::Button::new("Close")).clicked() {
                        keep = false;
                    }
                });
                ui.add_space(4.0);
                ui.checkbox(dont_show_again, "Don't show this dialog again");
            });
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                keep = false;
            }
        });
        keep
    }
}

/// Open the containing folder with the file selected.
pub fn reveal_in_folder(path: &std::path::Path) {
    #[cfg(windows)]
    {
        if path.exists() {
            let _ = std::process::Command::new("explorer.exe").arg("/select,").arg(path).spawn();
            return;
        }
    }
    if let Some(dir) = path.parent() {
        let _ = open::that_detached(dir);
    }
}
