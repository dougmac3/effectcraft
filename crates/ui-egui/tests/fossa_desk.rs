//! EffectCraft as Fossa Direct's Effects desk, offscreen (egui_kittest + wgpu): a host strip on
//! top, the desk in the rest of a 1600 x 1000 window, a generated 1280 x 720 frame opened in it;
//! then driven in-process (`Desk::control`), and one panel drawn alone (`Desk::panel_ui`) in a
//! 900 x 320 card, with a context menu and a dialog kept inside it.
//! Ignored by default (needs a GPU adapter); run with
//! `FOSSA_DESK_SHOT=/abs/desk.png FOSSA_PANEL_SHOT=/abs/panel.png cargo test --release -p effectcraft-ui-egui --test fossa_desk -- --ignored`.

use effectcraft_ui_egui::fossa_native::Desk;
use egui_kittest::kittest::Queryable;
use serde_json::json;

const HOST_FILL: egui::Color32 = egui::Color32::from_rgb(0x14, 0x12, 0x1c);

/// Where the host draws the panel card.
fn card() -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(40.0, 60.0), egui::vec2(900.0, 320.0))
}

struct Host {
    desk: Desk,
    /// Draw only this panel, in [`card`].
    panel: Option<&'static str>,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let strip = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 40.0));
        ui.painter().rect_filled(strip, 0.0, HOST_FILL);
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(strip.shrink2(egui::vec2(12.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.label("Fossa host");
                let _ = ui.button("Add to shot");
            },
        );
        if let Some(p) = self.panel {
            ui.scope_builder(egui::UiBuilder::new().max_rect(card()), |ui| self.desk.panel_ui(p, ui, frame));
            return;
        }
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

/// Let the viewer's frames render, drawing as it goes.
fn settle(h: &mut egui_kittest::Harness<'_, Host>) {
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        h.run_steps(1);
    }
}

fn save(h: &mut egui_kittest::Harness<'_, Host>, var: &str, crop: Option<egui::Rect>) {
    let Ok(out) = std::env::var(var) else { return };
    let mut img = h.render().expect("render");
    if let Some(r) = crop {
        let ppp = h.ctx.pixels_per_point();
        img = image::imageops::crop_imm(&img, (r.min.x * ppp) as u32, (r.min.y * ppp) as u32, (r.width() * ppp) as u32, (r.height() * ppp) as u32).to_image();
    }
    if let Some(d) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    img.save(&out).unwrap();
}

/// The layers above the host's that are on screen and not inside `rect` (as drawn: with their
/// layer transforms).
fn outside(ctx: &egui::Context, rect: egui::Rect) -> Vec<String> {
    let layers = ctx.memory(|m| m.areas().visible_layer_ids());
    layers
        .into_iter()
        .filter(|l| l.order != egui::Order::Background)
        .filter_map(|l| {
            let r = egui::AreaState::load(ctx, l.id)?.rect();
            let r = ctx.layer_transform_to_global(l).map_or(r, |t| t * r);
            (r.area() > 0.0 && !rect.expand(1.0).contains_rect(r)).then(|| format!("{l:?} at {r:?}"))
        })
        .collect()
}

/// The top Foreground layer's rect as drawn and its transform (an open menu or dialog).
fn top_popup(ctx: &egui::Context) -> Option<(egui::Rect, egui::emath::TSTransform)> {
    let layer = ctx.memory(|m| m.areas().top_layer_id(egui::Order::Foreground))?;
    let r = egui::AreaState::load(ctx, layer.id)?.rect();
    let t = ctx.layer_transform_to_global(layer).unwrap_or_default();
    Some((t * r, t))
}

fn click(h: &mut egui_kittest::Harness<'_, Host>, at: egui::Pos2, button: egui::PointerButton) {
    h.event(egui::Event::PointerMoved(at));
    h.run_steps(1);
    h.event(egui::Event::PointerButton { pos: at, button, pressed: true, modifiers: Default::default() });
    h.run_steps(1);
    h.event(egui::Event::PointerButton { pos: at, button, pressed: false, modifiers: Default::default() });
    h.run_steps(3);
}

/// The effects on `layer` (their registry ids).
fn effects(desk: &mut Desk, layer: u64) -> Vec<String> {
    let comp = desk.app().session.active_comp().expect("a comp");
    let l = comp.layers.iter().find(|l| l.id.0 == layer).expect("the layer");
    l.effects()
        .map(|g| {
            g.groups()
                .filter_map(|g| match &g.kind {
                    effectcraft_engine::project::GroupKind::Effect { effect } => Some(effect.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore]
fn effects_desk_in_a_host_window() {
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|cc| {
        // The host's look: its own panel fill, which the desk must leave as it found it.
        cc.egui_ctx.all_styles_mut(|s| s.visuals.panel_fill = HOST_FILL);
        let mut desk = Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref());
        desk.open("frame.png", frame_png());
        Host { desk, panel: None }
    });
    let host_fonts = h.ctx.fonts(|f| f.definitions().families.clone());
    h.run_steps(8);
    settle(&mut h);

    // The host's look and fonts survive: its style comes back after the desk's frame, and the
    // desk's faces were added to the families, ahead of none of the host's.
    assert_eq!(h.ctx.global_style().visuals.panel_fill, HOST_FILL);
    let fonts = h.ctx.fonts(|f| f.definitions().families.clone());
    for (family, names) in &host_fonts {
        assert_eq!(&fonts[family][..names.len()], &names[..], "{family:?} keeps the host's faces first");
    }
    assert!(fonts.contains_key(&egui::FontFamily::Name("semibold".into())));
    save(&mut h, "FOSSA_DESK_SHOT", None);

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

    // In-process control: the chat's agent adds a blur to the layer and sets its amount.
    let info = desk.control("engine.execute", json!({"command": "comp.info", "params": {}})).expect("comp.info");
    let layer = info["layers"][0]["id"].as_u64().unwrap_or_else(|| panic!("a layer: {info}"));
    let applied =
        desk.control("engine.execute", json!({"command": "effect.apply", "params": {"effect": "ec.blur.gaussian", "layers": [layer]}})).expect("effect.apply");
    let blur = applied["effects"][0].as_u64().unwrap_or_else(|| panic!("the effect's uid: {applied}"));
    desk.control("engine.execute", json!({"command": "prop.set", "params": {"layer": layer, "path": format!("effects/@{blur}/blurriness"), "value": 24.0}}))
        .expect("prop.set");
    assert!(desk.control("ui.click", json!({"x": 1, "y": 1})).is_err(), "synthetic input is not taken in-process");
    assert!(desk.control("no.such.method", json!({})).is_err());
    assert_eq!(effects(desk, layer), ["ec.blur.gaussian"]);

    // The listing: commands with their parameters typed, every effect's parameters typed.
    let list = desk.commands();
    let (typed, untyped) = (list["typed"].as_u64().unwrap(), list["untyped"].as_u64().unwrap());
    assert_eq!(typed + untyped, list["commands"].as_array().unwrap().len() as u64);
    let params: Vec<&serde_json::Value> = list["commands"].as_array().unwrap().iter().flat_map(|c| c["params"].as_array().unwrap()).collect();
    let free = params.iter().filter(|p| p["kind"] == "any").count();
    println!(
        "commands: {typed} fully typed, {untyped} with an untyped parameter; parameters: {} typed, {free} untyped; effects: {}",
        params.len() - free,
        list["effects"].as_array().unwrap().len()
    );
    let gauss = list["effects"].as_array().unwrap().iter().find(|e| e["id"] == "ec.blur.gaussian").expect("Gaussian Blur");
    let amount = gauss["params"].as_array().unwrap().iter().find(|p| p["id"] == "blurriness").expect("Blurriness");
    assert_eq!(amount["kind"], "number");
    assert!(amount["max"].as_f64().is_some() && amount["default"].is_number(), "{amount}");
    let apply = list["commands"].as_array().unwrap().iter().find(|c| c["id"] == "effect.apply").unwrap();
    assert!(apply["params"].as_array().unwrap().iter().any(|p| p["id"] == "effect" && p["choices"].as_array().is_some_and(|c| c.len() > 100)), "{apply}");
    assert!(desk.panels().iter().any(|p| p == "EffectControls"));

    // A layer's context menu offers Add to Shot, which queues that layer's picture.
    h.run_steps(3);
    let at = h.state_mut().desk.control("ui.timeline.locate", json!({"layer": layer})).expect("the layer's bar");
    let bar = egui::pos2(at["x"].as_f64().unwrap() as f32, at["y"].as_f64().unwrap() as f32);
    click(&mut h, bar, egui::PointerButton::Secondary);
    h.get_by_label("Add to Shot").click();
    h.run_steps(3);
    let shots = h.state_mut().desk.take_shot_requests();
    assert_eq!(shots.len(), 1, "{:?}", shots.iter().map(|s| &s.0).collect::<Vec<_>>());
    assert!(shots[0].0.ends_with(".png"));
    assert_eq!(image::load_from_memory(&shots[0].1).expect("a PNG").width(), 1280);

    // The grabs: the selected layer, a layer by id; nothing selected is an error.
    let desk = &mut h.state_mut().desk;
    desk.control("engine.execute", json!({"command": "layer.select", "params": {"layers": [layer]}})).unwrap();
    let sel = image::load_from_memory(&desk.grab_selection_png().expect("the selection")).expect("a PNG");
    assert_eq!((sel.width(), sel.height()), (1280, 720));
    let one = image::load_from_memory(&desk.grab_layer_png(layer).expect("the layer")).expect("a PNG");
    assert_eq!((one.width(), one.height()), (1280, 720));
    assert!(desk.grab_layer_png(999_999).is_err());
    desk.control("engine.execute", json!({"command": "layer.select", "params": {"layers": []}})).unwrap();
    assert!(desk.grab_selection_png().is_err());

    // One panel alone: Effect Controls narrowed to the blur on that layer, in a 900 x 320 card.
    desk.focus(json!({"layer": layer, "effect": blur})).expect("focus");
    h.state_mut().panel = Some("EffectControls");
    h.run_steps(4);
    settle(&mut h);
    save(&mut h, "FOSSA_PANEL_SHOT", Some(card()));
    assert!(outside(&h.ctx, card()).is_empty(), "{:?}", outside(&h.ctx, card()));

    // Its context menu opens inside the card (moved up: the card is short), takes a click and
    // closes.
    let name = h.state_mut().desk.app().auto.find(&format!("effectControls.effect.{blur}.name")).map(|e| e.rect).expect("the effect's name");
    click(&mut h, egui::pos2(name[0] + 10.0, name[1] + name[3] * 0.5), egui::PointerButton::Secondary);
    let (menu, shift) = top_popup(&h.ctx).expect("a context menu");
    assert!(card().expand(1.0).contains_rect(menu), "menu at {menu:?}");
    assert!(outside(&h.ctx, card()).is_empty(), "{:?}", outside(&h.ctx, card()));
    save(&mut h, "FOSSA_PANEL_MENU_SHOT", Some(card()));
    let dup = h.get_by_label("Duplicate").rect().center();
    // (The accessibility tree has the menu where egui laid it out; it is drawn and clicked moved.)
    let dup = if card().contains(dup) && shift == Default::default() { dup } else { shift * dup };
    click(&mut h, dup, egui::PointerButton::Primary);
    assert_eq!(effects(&mut h.state_mut().desk, layer), ["ec.blur.gaussian", "ec.blur.gaussian"], "Duplicate ran from the moved menu");

    // A dialog opens over the card only, takes input and closes.
    h.state_mut().desk.control("ui.menu.invoke", json!({"id": "comp.settings"})).expect("Composition Settings");
    h.run_steps(6);
    assert!(h.state_mut().desk.app().dialog.is_some());
    let (dialog, _) = top_popup(&h.ctx).expect("a dialog");
    assert!(card().expand(1.0).contains_rect(dialog), "dialog at {dialog:?}");
    assert!(outside(&h.ctx, card()).is_empty(), "{:?}", outside(&h.ctx, card()));
    // The card is short: the dialog scrolls to its buttons.
    h.event(egui::Event::PointerMoved(dialog.center()));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -2000.0),
        modifiers: Default::default(),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
    });
    h.run_steps(6);
    save(&mut h, "FOSSA_PANEL_DIALOG_SHOT", Some(card()));
    h.get_by_label("Cancel").click();
    h.run_steps(4);
    assert!(h.state_mut().desk.app().dialog.is_none());
}
