//! A stand-in for Fossa Direct's window: a host strip on top and EffectCraft's desk
//! ([`effectcraft_ui_egui::fossa_native::Desk`]) in the rest, run in-process as Fossa runs it.
//!
//! ```text
//! cargo run --release -p effectcraft-ui-egui --example fossa_host -- [frame.png | clip.mp4 | project.ecproj]
//! ```
//!
//! "Add to shot" grabs the frame at the playhead; what the desk saves or renders is listed in
//! the strip as it arrives.

use effectcraft_ui_egui::fossa_native::Desk;

struct Host {
    desk: Desk,
    /// What came back from the desk: (file name, bytes).
    got: Vec<(String, usize)>,
    note: String,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let strip = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 40.0));
        ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(0x14, 0x12, 0x1c));
        ui.scope_builder(egui::UiBuilder::new().max_rect(strip.shrink2(egui::vec2(12.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.label(egui::RichText::new("Fossa host").strong());
            if ui.button("Add to shot").clicked() {
                self.note = match self.desk.grab_png() {
                    Ok(png) => format!("grabbed a frame ({} bytes)", png.len()),
                    Err(e) => format!("no frame: {e}"),
                };
            }
            ui.label(&self.note);
            for (name, n) in &self.got {
                ui.label(format!("{name} ({n} bytes)"));
            }
        });
        let desk = egui::Rect::from_min_max(egui::pos2(full.min.x, strip.max.y), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(desk).id_salt("desk_effects"), |ui| self.desk.ui(ui, frame));
        for (name, bytes) in self.desk.take_saved() {
            self.got.push((name, bytes.len()));
        }
    }
}

fn main() -> eframe::Result {
    let file = std::env::args().nth(1);
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_title("Fossa host").with_inner_size([1600.0, 1000.0]), ..Default::default() };
    eframe::run_native(
        "fossa_host",
        options,
        Box::new(move |cc| {
            let mut desk = Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref());
            if let Some(path) = file {
                match std::fs::read(&path) {
                    Ok(bytes) => desk.open(&path, bytes),
                    Err(e) => eprintln!("{path}: {e}"),
                }
            }
            Ok(Box::new(Host { desk, got: Vec::new(), note: String::new() }))
        }),
    )
}
