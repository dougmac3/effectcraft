//! How long Fossa Direct's Effects desk takes to come up, phase by phase, on a real clip and a
//! still. Ignored by default (needs a GPU adapter and the files); run with
//! `FOSSA_TIMING_CLIP=D:/AI/fossa-demo/clip.mp4 FOSSA_TIMING_STILL=D:/AI/fossa-demo/still.png
//! cargo test --release -p effectcraft-ui-egui --test fossa_desk_timing -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use effectcraft_ui_egui::fossa_native::Desk;

struct Host {
    desk: Desk,
    /// The longest single frame so far.
    worst: Duration,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let t = Instant::now();
        self.desk.ui(ui, frame);
        self.worst = self.worst.max(t.elapsed());
    }
}

fn ms(d: Duration) -> String {
    format!("{:.0} ms", d.as_secs_f64() * 1000.0)
}

fn time(var: &str) {
    let Ok(path) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy().to_string();
    let mut made = Duration::ZERO;
    let up = Instant::now();
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|cc| {
        let t = Instant::now();
        let desk = Desk::new(&cc.egui_ctx, cc.wgpu_render_state.as_ref());
        made = t.elapsed();
        Host { desk, worst: Duration::ZERO }
    });
    eprintln!("[{name}] Desk::new: {}; with the first frame: {}", ms(made), ms(up.elapsed()));
    eprintln!("[{name}] desk's first frame: {}", ms(h.state().worst));

    let t = Instant::now();
    let mut frames = 0;
    while !h.ctx.fonts(|f| f.definitions().families.contains_key(&egui::FontFamily::Name("semibold".into()))) && frames < 20 {
        h.run_steps(1);
        frames += 1;
    }
    eprintln!("[{name}] frames until fonts: {frames} in {}", ms(t.elapsed()));

    let t = Instant::now();
    h.state_mut().desk.open(&name, bytes);
    eprintln!("[{name}] open: {}", ms(t.elapsed()));
    let t = Instant::now();
    h.state_mut().worst = Duration::ZERO;
    h.run_steps(1);
    eprintln!("[{name}] first frame after open: {}", ms(t.elapsed()));

    // Until the desk has a composited frame to give (open may finish in the background).
    let t = Instant::now();
    let png = loop {
        let g = Instant::now();
        let r = h.state_mut().desk.grab_png();
        eprintln!("[{name}]   grab_png: {} {}", ms(g.elapsed()), r.as_ref().err().map_or("ok", |e| e.as_str()));
        if let Ok(p) = r {
            break p;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "no composited frame after 60 s");
        std::thread::sleep(Duration::from_millis(10));
        h.run_steps(1);
    };
    eprintln!("[{name}] first composited frame: {} ({} bytes)", ms(t.elapsed()), png.len());
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(20));
        h.run_steps(1);
    }
    eprintln!("[{name}] worst frame since open: {}", ms(h.state().worst));
    let g = Instant::now();
    let _ = h.state_mut().desk.grab_png();
    eprintln!("[{name}] grab_png again: {}", ms(g.elapsed()));
}

#[test]
#[ignore]
fn effects_desk_open_timing() {
    time("FOSSA_TIMING_CLIP");
    time("FOSSA_TIMING_STILL");
}
