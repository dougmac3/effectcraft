//! EffectCraft as a desk of Fossa Direct's native window: the editor runs in-process, inside a
//! rect of the host's egui window. It mirrors the web build's Fossa bridge
//! (`fossa-direct/engines/effects/fossa-bridge.js`): Fossa opens a frame or clip as the layer to
//! work on ([`Desk::open`]), renders and saves go back to Fossa instead of to disk dialogs
//! ([`Desk::take_saved`]), and [`Desk::grab_png`] is the composited frame at the playhead.
//!
//! While hosted ([`crate::FOSSA_HOSTED`]) the editor sends no window commands (title, close,
//! size, full screen), opens no windows of its own (torn-off panels and the video preview are
//! drawn inside the host's window), never replaces the host's fonts, scale or look, and its
//! shortcuts act only while the pointer is over the desk or the last click was inside it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use effectcraft_engine::Services;
use serde_json::json;

use crate::EffectcraftApp;

/// Files saved or rendered since the last [`Desk::take_saved`]: (file name, bytes).
type Saved = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

fn push(saved: &Saved, path: &str, data: Vec<u8>) {
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
    saved.lock().unwrap_or_else(|e| e.into_inner()).push((name, data));
}

/// The engine's file access inside Fossa: what the user saves (projects, frames, exports) is
/// written as on the desktop and also handed to Fossa, as the web build downloads it; files the
/// app keeps for itself ([`Services::store_file`]) are only written.
struct FossaServices {
    saved: Saved,
}

impl Services for FossaServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if let Some(d) = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(d)?;
        }
        effectcraft_engine::config::atomic_write(Path::new(path), data)?;
        push(&self.saved, path, data.to_vec());
        Ok(())
    }
    fn store_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if let Some(d) = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(d);
        }
        effectcraft_engine::config::atomic_write(Path::new(path), data)
    }
}

/// The host's look, or the desk's, as egui keeps it (both themes' styles and the preference).
struct Look {
    dark: Arc<egui::Style>,
    light: Arc<egui::Style>,
    theme: egui::ThemePreference,
    zoom_keys: bool,
}

impl Look {
    fn read(ctx: &egui::Context) -> Self {
        ctx.options(|o| Look { dark: o.dark_style.clone(), light: o.light_style.clone(), theme: o.theme_preference, zoom_keys: o.zoom_with_keyboard })
    }
    fn apply(&self, ctx: &egui::Context) {
        ctx.options_mut(|o| {
            o.dark_style = self.dark.clone();
            o.light_style = self.light.clone();
            o.theme_preference = self.theme;
            o.zoom_with_keyboard = self.zoom_keys;
        });
    }
}

/// EffectCraft embedded in a host's egui window (Fossa Direct's Effects desk).
pub struct Desk {
    app: EffectcraftApp,
    saved: Saved,
    /// Where files Fossa opens are written, and where saves without a path go.
    dir: PathBuf,
    /// The desk's look between frames (the host's is put back after each).
    look: Option<Look>,
    /// The last click landed in the desk: keyboard shortcuts go to it.
    focused: bool,
}

impl Desk {
    /// `render_state` is the host window's wgpu state (eframe::Frame::wgpu_render_state()), for
    /// the GPU compositor; without it the desk composites on the CPU.
    pub fn new(ctx: &egui::Context, render_state: Option<&eframe::egui_wgpu::RenderState>) -> Self {
        crate::FOSSA_HOSTED.store(true, std::sync::atomic::Ordering::Relaxed);
        let saved: Saved = Arc::default();
        let dir = std::env::temp_dir().join(format!("fossa-effectcraft-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut session = effectcraft_host::session();
        session.check_footage_on_open = true;
        session.autosave.background = true;
        session.services = Arc::new(FossaServices { saved: saved.clone() });
        // Render Queue output goes to Fossa (the web build downloads it).
        let sink = saved.clone();
        session.exporter = Some(Arc::new(effectcraft_host::FileExporter { sink: Some(Arc::new(move |path: &str, data: Vec<u8>| push(&sink, path, data))) }));
        if let Some(config) = effectcraft_host::config_dir() {
            let plugins = config.join("Plug-ins");
            if plugins.is_dir() {
                let _ = session.execute("effect.plugins.load", json!({"folder": plugins.to_string_lossy()}));
            }
            // Its own settings and workspaces: a desk's layout is not the desktop app's.
            let desk = config.join("Fossa Desk");
            if std::fs::create_dir_all(&desk).is_ok() {
                session.config = Some(Arc::new(effectcraft_engine::config::DirConfig::new(desk)));
            }
        }
        session.load_settings();
        let mut app = EffectcraftApp::new(session);
        app.ui.start_screen = false;
        // Health reports of the compositor; the host owns the device and its handlers.
        app.set_gpu_failure_bridge(crate::gpu_failure::GpuFailureBridge::new(ctx));
        app.init_gpu_on(render_state);
        // Save dialogs name a file in the desk's folder; the file goes to Fossa when written.
        let into = dir.clone();
        app.hooks.pick_save = Some(Box::new(move |name: &str| Some(into.join(if name.is_empty() { "Untitled Project.ecproj" } else { name }).to_string_lossy().to_string())));
        let into = dir.clone();
        app.hooks.pick_save_file = Some(Box::new(move |name: &str, _ext: &str| {
            let file = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".into());
            Some(into.join(file).to_string_lossy().to_string())
        }));
        Desk { app, saved, dir, look: None, focused: false }
    }

    /// The editor (for hosts and tests that drive it further).
    pub fn app(&mut self) -> &mut EffectcraftApp {
        &mut self.app
    }

    /// Open a file Fossa hands in, as a drop would: a project opens; a frame or clip is imported
    /// and a composition made from it, so there is a layer to put effects on straight away.
    pub fn open(&mut self, name: &str, bytes: Vec<u8>) {
        let file = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "opened".into());
        let folder = self.dir.join("opened");
        let path = unique(&folder, &file);
        if let Err(e) = std::fs::create_dir_all(&folder).and_then(|()| std::fs::write(&path, &bytes)) {
            self.app.ui.status = format!("{file}: {e}");
            return;
        }
        let p = path.to_string_lossy().to_string();
        let lower = file.to_ascii_lowercase();
        let s = &mut self.app.session;
        let r = if lower.ends_with(".ecproj") || lower.ends_with(".ecprojx") {
            s.execute("file.open", json!({"path": p})).map(|_| ())
        } else {
            s.execute("file.import", json!({"paths": [p]})).and_then(|r| {
                if let Some(e) = r["errors"].as_array().and_then(|e| e.first()).and_then(|e| e.as_str()) {
                    return Err(effectcraft_engine::EngineError::Other(e.to_string()));
                }
                if r["comps"].as_array().is_none_or(Vec::is_empty) && r["items"].as_array().is_some_and(|i| !i.is_empty()) {
                    s.execute("file.newCompFromSelection", json!({}))?;
                }
                Ok(())
            })
        };
        if let Err(e) = r {
            self.app.ui.status = e.to_string();
        }
    }

    /// Files the editor saved or rendered since the last call, (file name, bytes), for Fossa to
    /// add to the shot.
    pub fn take_saved(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut *self.saved.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The active composition's frame at the playhead, composited and encoded as a PNG.
    pub fn grab_png(&mut self) -> Result<Vec<u8>, String> {
        let s = &self.app.session;
        let comp = s.resolve_comp(None).map_err(|e| e.to_string())?;
        let (w, h, rgba) = s.render_rgba8_alpha(comp, s.time(), 0, false).map_err(|e| e.to_string())?;
        crate::control::encode_png(&rgba, w, h)
    }

    /// Draw and run the editor inside `ui`'s rect of the host window, one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        ctx.data_mut(|d| d.insert_temp(crate::fossa_native_bounds(), rect));
        let host = Look::read(&ctx);
        if let Some(look) = &self.look {
            look.apply(&ctx);
        }
        // Panel windows and the video preview become windows inside the host's.
        let embed = ctx.embed_viewports();
        ctx.set_embed_viewports(true);
        // Keys belong to the desk while the pointer is over it (and the host isn't typing) or its
        // last click was inside it; otherwise it doesn't see them.
        let (pressed, hover) = ctx.input(|i| (i.pointer.press_origin().filter(|_| i.pointer.any_pressed()), i.pointer.hover_pos()));
        let typing = ctx.egui_wants_keyboard_input();
        if let Some(at) = pressed {
            self.focused = rect.contains(at);
        }
        let keys = self.focused || (hover.is_some_and(|p| rect.contains(p)) && !typing);
        let held = if keys {
            Vec::new()
        } else {
            ctx.input_mut(|i| {
                let (held, rest) = std::mem::take(&mut i.events).into_iter().partition(|e| {
                    matches!(e, egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) | egui::Event::Ime(_))
                });
                i.events = rest;
                held
            })
        };
        eframe::App::logic(&mut self.app, &ctx, frame);
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).id_salt("effectcraft"), |ui| {
            ui.set_style(ctx.global_style());
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            eframe::App::ui(&mut self.app, ui, frame);
        });
        self.look = Some(Look::read(&ctx));
        host.apply(&ctx);
        ctx.set_embed_viewports(embed);
        if !held.is_empty() {
            ctx.input_mut(|i| i.events.extend(held));
        }
    }
}

impl Drop for Desk {
    fn drop(&mut self) {
        eframe::App::on_exit(&mut self.app);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `file` in `folder`, numbered when a file of that name is already there.
fn unique(folder: &Path, file: &str) -> PathBuf {
    let first = folder.join(file);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (file, String::new()),
    };
    (2..).map(|n| folder.join(format!("{stem} {n}{ext}"))).find(|p| !p.exists()).unwrap_or(first)
}
