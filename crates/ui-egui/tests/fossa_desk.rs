//! EffectCraft as Fossa Direct's Effects desk, offscreen (egui_kittest + wgpu): a host strip on
//! top, the desk in the rest of a 1600 x 1000 window, a generated 1280 x 720 frame opened in it.
//! Ignored by default (needs a GPU adapter); run with
//! `FOSSA_DESK_SHOT=/abs/desk.png cargo test --release -p effectcraft-ui-egui --test fossa_desk -- --ignored`.

use effectcraft_ui_egui::fossa_native::Desk;
use serde_json::json;

const HOST_FILL: egui::Color32 = egui::Color32::from_rgb(0x14, 0x12, 0x1c);

struct Host {
    desk: Desk,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let strip = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 40.0));
        ui.painter().rect_filled(strip, 0.0, HOST_FILL);
        ui.scope_builder(egui::UiBuilder::new().max_rect(strip.shrink2(egui::vec2(12.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.label("Fossa host");
            let _ = ui.button("Add to shot");
        });
        let desk = egui::Rect::from_min_max(egui::pos2(full.min.x, strip.max.y), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(desk), |ui| self.desk.ui(ui, frame));
    }
}

/// A 1280 x 720 test frame: a gradient with a light disc in the middle.
fn frame_png() -> Vec<u8> {
    let (w, h) = (1280u32, 720u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (x as f32 - 640.0, y as f32 - 360.0);
            let disc = dx * dx + dy * dy < 200.0 * 200.0;
            rgba.extend_from_slice(&if disc { [240, 220, 160, 255] } else { [(x * 255 / w) as u8, 60, (y * 255 / h) as u8, 255] });
        }
    }
    effectcraft_ui_egui::control::encode_png(&rgba, w, h).unwrap()
}

#[test]
#[ignore]
fn effects_desk_in_a_host_window() {
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|cc| {
        // The host's look: its own panel fill, which the desk must leave as it found it.
        cc.egui_ctx.all_styles_mut(|s| s.visuals.panel_fill = HOST_FILL);
        let mut desk = Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref());
        desk.open("frame.png", frame_png());
        Host { desk }
    });
    let host_fonts = h.ctx.fonts(|f| f.definitions().families.clone());
    h.run_steps(8);
    // Let the viewer's frame render, then draw it.
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.run_steps(1);
    }

    // The host's look and fonts survive: its style comes back after the desk's frame, and the
    // desk's faces were added to the families, ahead of none of the host's.
    assert_eq!(h.ctx.global_style().visuals.panel_fill, HOST_FILL);
    let fonts = h.ctx.fonts(|f| f.definitions().families.clone());
    for (family, names) in &host_fonts {
        assert_eq!(&fonts[family][..names.len()], &names[..], "{family:?} keeps the host's faces first");
    }
    assert!(fonts.contains_key(&egui::FontFamily::Name("semibold".into())));

    if let Ok(out) = std::env::var("FOSSA_DESK_SHOT") {
        let img = h.render().expect("render");
        if let Some(d) = std::path::Path::new(&out).parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        img.save(&out).unwrap();
    }

    // "Add to shot": the composited frame at the playhead, at the document's size.
    let desk = &mut h.state_mut().desk;
    let png = desk.grab_png().expect("a frame");
    let img = image::load_from_memory(&png).expect("a PNG");
    assert_eq!((img.width(), img.height()), (1280, 720));

    // Saving hands the file to Fossa.
    let dir = std::env::temp_dir().join(format!("fossa-desk-test-{}", std::process::id()));
    let path = dir.join("Shot.ecproj");
    desk.app().session.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    let saved = desk.take_saved();
    assert!(saved.iter().any(|(name, bytes)| name == "Shot.ecproj" && !bytes.is_empty()), "{:?}", saved.iter().map(|s| &s.0).collect::<Vec<_>>());
    assert!(desk.take_saved().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}
