//! EffectCraft as a desk of Fossa Direct's native window: the editor runs in-process, inside a
//! rect of the host's egui window. It mirrors the web build's Fossa bridge
//! (`fossa-direct/engines/effects/fossa-bridge.js`): Fossa opens a frame or clip as the layer to
//! work on ([`Desk::open`]), renders and saves go back to Fossa instead of to disk dialogs
//! ([`Desk::take_saved`]), and [`Desk::grab_png`] is the composited frame at the playhead.
//!
//! Fossa's chat drives the editor through the control channel in-process ([`Desk::control`],
//! the methods of [`crate::control`]; [`Desk::commands`] lists the commands and every effect's
//! parameters, typed), and can draw one panel alone ([`Desk::panel_ui`]), narrowed to what the
//! chat is about ([`Desk::focus`]: one effect of one layer, a time range of the timeline).
//! The files its requests may read and write are confined to the desk's own folder and those
//! the host names ([`Desk::set_file_roots`]), as `effectcraft-cli mcp --automation-read-root`
//! confines them; what the person does by hand is not.
//!
//! While hosted ([`crate::FOSSA_HOSTED`]) the editor sends no window commands (title, close,
//! size, full screen), opens no windows of its own (torn-off panels and the video preview are
//! drawn inside the host's window), never replaces the host's fonts, scale or look, and its
//! shortcuts act only while the pointer is over the desk or the last click was inside it. Its
//! dialogs, menus, popups and tooltips stay inside the desk's rect (a modal dialog's backdrop
//! covers the desk, not the host), file dialogs open off the UI thread, and preview sound plays
//! through the desk's own output stream.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use effectcraft_automation::confine::{Access, Confinement, Lookup};
use effectcraft_engine::Services;
use effectcraft_engine::effects::EffectSpec;
use effectcraft_engine::project::{GroupKind, ParamUi};
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::control::{ControlRequest, Outcome};
use crate::dock::PanelKind;

/// How long [`Desk::render_clip`] may take before it is stopped.
// ponytail: one fixed limit; scale it with the clip's length if long clips hit it.
pub const CLIP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// A render [`Desk::start_render`] runs on its own thread.
#[derive(Default)]
struct ClipJob {
    file: String,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// (frames done, frames in all).
    progress: Arc<Mutex<(u64, u64)>>,
    result: Arc<Mutex<Option<Result<Vec<u8>, String>>>>,
    until: Option<std::time::Instant>,
}

/// The project as it stood at [`Desk::edit_mark`]; holding it keeps that project alive, so
/// [`Desk::edited_since`] compares the very same one.
pub struct EditMark(Arc<effectcraft_engine::project::Project>);

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

/// A file dialog of the desk (File > Open, Import, Settings paths...), shown off the UI thread:
/// `done` gets the picked paths (empty: cancelled) when it closes. Saves name a file in the
/// desk's folder at once; what is written there goes to Fossa ([`Desk::take_saved`]).
fn file_dialog(dir: &Path, dialog: crate::FileDialog, done: crate::DialogDone, ctx: &egui::Context) {
    use rfd::AsyncFileDialog;
    type Answer = std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send>>;
    fn paths(files: Option<Vec<rfd::FileHandle>>) -> Vec<String> {
        files.unwrap_or_default().into_iter().map(|f| f.path().to_string_lossy().to_string()).collect()
    }
    let filtered = |name: &str, exts: &[String]| if exts.is_empty() { AsyncFileDialog::new() } else { AsyncFileDialog::new().add_filter(name, exts) };
    // (The dialog is made here, on the UI thread, as macOS needs; it is awaited on a worker.)
    let answer: Answer = match dialog {
        crate::FileDialog::Open { exts, many } => {
            let d = filtered("Media", &exts);
            if many {
                let f = d.pick_files();
                Box::pin(async move { paths(f.await) })
            } else {
                let f = d.pick_file();
                Box::pin(async move { paths(f.await.map(|f| vec![f])) })
            }
        }
        crate::FileDialog::OpenProject => {
            let f = AsyncFileDialog::new().add_filter("EffectCraft Project", &["ecproj", "ecprojx"]).pick_file();
            Box::pin(async move { paths(f.await.map(|f| vec![f])) })
        }
        crate::FileDialog::Folder => {
            let f = AsyncFileDialog::new().pick_folder();
            Box::pin(async move { paths(f.await.map(|f| vec![f])) })
        }
        crate::FileDialog::Save { default, .. } => {
            let file = Path::new(&default).file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Untitled".into());
            done(vec![dir.join(file).to_string_lossy().to_string()]);
            return;
        }
    };
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("ec-desk-dialog".into()).spawn(move || {
        done(pollster::block_on(answer));
        ctx.request_repaint();
    });
    if let Err(e) = spawned {
        log::error!("file dialog: {e}");
    }
}

/// EffectCraft embedded in a host's egui window (Fossa Direct's Effects desk).
pub struct Desk {
    app: EffectcraftApp,
    ctx: egui::Context,
    saved: Saved,
    /// Where files Fossa opens are written, and where saves without a path go: made fresh for
    /// the desk, private on Unix.
    dir: PathBuf,
    /// [`Self::dir`] was made by the desk (it is removed with it, and the agent may use it).
    made: bool,
    /// The files [`Desk::control`]'s requests may read and write.
    roots: Confinement,
    /// The desk's look between frames (the host's is put back after each).
    look: Option<Look>,
    /// The last click landed in the desk: keyboard shortcuts go to it.
    focused: bool,
    /// [`Desk::focus`]: the one effect Effect Controls shows in [`Desk::panel_ui`] (group uid).
    focus_effect: Option<u64>,
    /// [`Desk::focus`]: the parameters of that effect it shows (spec ids, in order; empty: all).
    focus_params: Vec<String>,
    /// [`Desk::focus`]: the time range (seconds) the Timeline is to show, once its width is known.
    focus_range: Option<(f64, f64)>,
    /// The render [`Desk::start_render`] began.
    clip: Option<ClipJob>,
}

impl Desk {
    /// `render_state` is the host window's wgpu state (eframe::Frame::wgpu_render_state()), for
    /// the GPU compositor; without it the desk composites on the CPU.
    pub fn new(ctx: &egui::Context, render_state: Option<&eframe::egui_wgpu::RenderState>) -> Self {
        crate::FOSSA_HOSTED.store(true, std::sync::atomic::Ordering::Relaxed);
        let saved: Saved = Arc::default();
        // A new folder no one else could have made first: a name not guessed in advance, made
        // fresh (an existing one fails), private on Unix. Without it the agent gets no files.
        let tag = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), std::time::SystemTime::now());
        let dir = std::env::temp_dir().join(format!("fossa-effectcraft-{}-{tag:016x}", std::process::id()));
        let made = std::fs::create_dir(&dir).is_ok();
        #[cfg(unix)]
        let made = made && std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).is_ok();
        let own: &[PathBuf] = if made { std::slice::from_ref(&dir) } else { &[] };
        let roots = Confinement::new(own, own).unwrap_or_else(|_| no_files());
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
        app.hooks.pick_save =
            Some(Box::new(move |name: &str| Some(into.join(if name.is_empty() { "Untitled Project.ecproj" } else { name }).to_string_lossy().to_string())));
        let into = dir.clone();
        app.hooks.pick_save_file = Some(Box::new(move |name: &str, _ext: &str| {
            let file = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".into());
            Some(into.join(file).to_string_lossy().to_string())
        }));
        // Every other file dialog (File > Open, Import, folders, Effect Controls' Choose..., the
        // forms' Browse...) opens without blocking the host's frame.
        let (into, host) = (dir.clone(), ctx.clone());
        app.hooks.file_dialog_async = Some(Box::new(move |dialog, done| file_dialog(&into, dialog, done, &host)));
        // Preview sound: the desk opens its own output while it plays (one stream; it closes
        // when playback stops or the desk is dropped).
        app.hooks.audio_device = Some(Box::new(crate::audio_out::open));
        app.hooks.audio_devices = Some(Box::new(crate::audio_out::devices));
        Desk { app, ctx: ctx.clone(), saved, dir, made, roots, look: None, focused: false, focus_effect: None, focus_params: vec![], focus_range: None, clip: None }
    }

    /// The desk's own folder (files Fossa opens, saves without a path).
    pub fn folder(&self) -> &Path {
        &self.dir
    }

    /// The folders the agent's requests may read and write (the project's, say), besides the
    /// desk's own. Each must exist.
    pub fn set_file_roots(&mut self, read: &[&Path], write: &[&Path]) -> Result<(), String> {
        let own = if self.made { vec![self.dir.clone()] } else { vec![] };
        let with_own = |extra: &[&Path]| own.iter().cloned().chain(extra.iter().map(|p| p.to_path_buf())).collect::<Vec<_>>();
        self.roots = Confinement::new(&with_own(read), &with_own(write)).map_err(|e| e.to_string())?;
        Ok(())
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

    /// The active composition's work area as an H.264 MP4 (with its sound, if it has any), made
    /// by the Render Queue's exporter with the Movie defaults: (file name, bytes). `Ok(None)` for
    /// a still: use [`Desk::grab_png`]. This waits for the render; [`Desk::start_render`] and
    /// [`Desk::poll_render`] do the same without holding the host's frame.
    pub fn render_clip(&mut self) -> Result<Option<(String, Vec<u8>)>, String> {
        if !self.start_render()? {
            return Ok(None);
        }
        loop {
            if let Some(r) = self.poll_render() {
                return r.map(Some);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Start rendering the active comp's work area to an H.264 MP4 on a thread of its own (from
    /// a snapshot of the project: editing goes on meanwhile). `Ok(false)` for a still (no layer
    /// has footage or a comp with time of its own): use [`Desk::grab_png`]. The movie is encoded
    /// in memory (no file is left behind, none goes to [`Desk::take_saved`]).
    pub fn start_render(&mut self) -> Result<bool, String> {
        use effectcraft_engine::project::render_queue::{OutputFormat, OutputModule, RenderQueueItem};
        if self.clip.is_some() {
            return Err("a render is already running".into());
        }
        let s = &self.app.session;
        let cid = s.resolve_comp(None).map_err(|e| e.to_string())?;
        let comp = s.project.comp(cid).ok_or("no composition is open")?;
        let frame = comp.frame_rate.frame_duration();
        let timed = comp.layers.iter().any(|l| match &l.source {
            effectcraft_engine::project::LayerSource::Footage { item } | effectcraft_engine::project::LayerSource::Comp { item } => {
                s.project.item(*item).and_then(|i| i.duration()).is_some_and(|d| d > frame)
            }
            _ => false,
        });
        if !timed || comp.duration <= frame {
            return Ok(false);
        }
        let name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
        let name: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' }).collect();
        let file = format!("{}.mp4", if name.trim().is_empty() { "Clip" } else { name.trim() });
        let mut item = RenderQueueItem::new(0, cid);
        item.output = OutputModule::for_format(OutputFormat::H264);
        let job = ClipJob { file, until: Some(std::time::Instant::now() + CLIP_TIMEOUT), ..Default::default() };
        let (project, footage, expr, accel) = (s.project.clone(), s.footage.clone(), s.expr.clone(), s.accel.clone());
        let nested = s.prefs.general.switches_affect_nested_comps;
        let path = self.dir.join(&job.file).to_string_lossy().to_string();
        let (cancel, progress, result) = (job.cancel.clone(), job.progress.clone(), job.result.clone());
        std::thread::Builder::new()
            .name("ec-desk-render".into())
            .spawn(move || {
                let out: Saved = Arc::default();
                let sink = out.clone();
                let exporter = effectcraft_host::FileExporter { sink: Some(Arc::new(move |path: &str, data: Vec<u8>| push(&sink, path, data))) };
                let job = effectcraft_engine::ExportJob {
                    project: &project,
                    footage: footage.as_ref(),
                    expr: expr.as_deref(),
                    accel: accel.as_deref(),
                    item: &item,
                    path: &path,
                    storage: None,
                    label: format!("{name} (Fossa)"),
                    nested_switches: nested,
                };
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    effectcraft_engine::Exporter::export(&exporter, &job, &mut |done, total| {
                        *progress.lock().unwrap_or_else(|e| e.into_inner()) = (done, total);
                        !cancel.load(std::sync::atomic::Ordering::Relaxed)
                    })
                }));
                let r = match run {
                    Ok(Ok(_)) => out.lock().unwrap_or_else(|e| e.into_inner()).pop().map(|f| f.1).ok_or_else(|| "the render wrote no movie".to_string()),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err("the render failed".into()),
                };
                *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            })
            .map_err(|e| e.to_string())?;
        self.clip = Some(job);
        Ok(true)
    }

    /// The render [`Desk::start_render`] began: `None` while it runs (or none was started);
    /// `Some` once, when it is done or failed. One running past [`CLIP_TIMEOUT`] is stopped.
    pub fn poll_render(&mut self) -> Option<Result<(String, Vec<u8>), String>> {
        let job = self.clip.as_ref()?;
        let done = job.result.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = done {
            let file = self.clip.take()?.file;
            return Some(r.map(|bytes| (file, bytes)));
        }
        if job.until.is_none_or(|t| std::time::Instant::now() < t) {
            return None;
        }
        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        self.clip = None;
        Some(Err(format!("the render took longer than {} s and was stopped", CLIP_TIMEOUT.as_secs())))
    }

    /// How far the running render is, 0 to 1 (`None` when none is running).
    pub fn render_progress(&self) -> Option<f32> {
        let (done, total) = *self.clip.as_ref()?.progress.lock().unwrap_or_else(|e| e.into_inner());
        Some(if total == 0 { 0.0 } else { done as f32 / total as f32 })
    }

    /// The project as it stands now, to compare against later with [`Desk::edited_since`].
    pub fn edit_mark(&self) -> EditMark {
        EditMark(self.app.session.project.clone())
    }

    /// The project is not the one `mark` took: false again after undoing back to it (an edit
    /// makes a new project and undo puts the old one back).
    pub fn edited_since(&self, mark: &EditMark) -> bool {
        !Arc::ptr_eq(&mark.0, &self.app.session.project)
    }

    /// The selected layers alone at the playhead (with their effects, masks and transforms, on
    /// transparency), as a PNG; an error when no layer is selected.
    pub fn grab_selection_png(&mut self) -> Result<Vec<u8>, String> {
        let layers: Vec<u64> = self.app.session.state.selected_layers.iter().map(|l| l.0).collect();
        if layers.is_empty() {
            return Err("no layer is selected".into());
        }
        self.grab_layers(&layers)
    }

    /// Layer `layer` (id) of the active composition alone at the playhead, as a PNG.
    pub fn grab_layer_png(&mut self, layer: u64) -> Result<Vec<u8>, String> {
        self.grab_layers(&[layer])
    }

    /// `layers` of the active comp soloed (in a copy of the project), at the comp's size.
    fn grab_layers(&self, layers: &[u64]) -> Result<Vec<u8>, String> {
        let s = &self.app.session;
        let cid = s.resolve_comp(None).map_err(|e| e.to_string())?;
        let mut project = (*s.project).clone();
        let comp = project.comp_mut(cid).ok_or("no composition is open")?;
        if let Some(missing) = layers.iter().find(|id| !comp.layers.iter().any(|l| l.id.0 == **id)) {
            return Err(format!("no layer {missing} in the composition"));
        }
        for l in &mut comp.layers {
            l.switches.solo = layers.contains(&l.id.0);
            if l.switches.solo {
                l.switches.video = true;
            }
        }
        let opts = effectcraft_engine::render::RenderOpts { backend: effectcraft_engine::render::Backend::Auto, ..Default::default() };
        let mut r = effectcraft_engine::render::Renderer::new(&project, s.footage.as_ref(), opts);
        r.expr = s.expr.as_deref();
        r.accel = s.accel.as_deref();
        let img = r.comp_frame(cid, s.time());
        crate::control::encode_png(&img.to_rgba8(), img.width, img.height)
    }

    /// The pictures asked for with Add to Shot in a layer's context menu since the last call:
    /// (file name, PNG) per layer.
    pub fn take_shot_requests(&mut self) -> Vec<(String, Vec<u8>)> {
        let ids = std::mem::take(&mut self.app.shot_requests);
        let mut out = Vec::new();
        for id in ids {
            let name = self
                .app
                .session
                .active_comp()
                .and_then(|c| c.layers.iter().find(|l| l.id.0 == id))
                .map(|l| l.name.clone())
                .unwrap_or_else(|| format!("Layer {id}"));
            let name: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' }).collect();
            match self.grab_layer_png(id) {
                Ok(png) => out.push((format!("{}.png", name.trim()), png)),
                Err(e) => self.app.ui.status = e,
            }
        }
        out
    }

    /// Run one control-channel request in-process (the methods [`crate::control`] lists:
    /// `engine.execute`, `engine.commands`, `ui.set`, `ui.panel.show`, `ui.inspect`,
    /// `render.frame`...), between frames. Synthetic input (`ui.click`, `ui.key`...) and window
    /// screenshots need the windowed app's channel: here they are errors (use [`Desk::panel_ui`],
    /// [`Desk::grab_png`]). Files are read and written only inside the desk's roots
    /// ([`Desk::set_file_roots`]).
    pub fn control(&mut self, method: &str, mut params: Value) -> Result<Value, String> {
        self.confine(method, &mut params)?;
        let (req, _reply) = ControlRequest::new(method, params);
        let reply = match crate::control::handle(&mut self.app, &self.ctx, &req) {
            Outcome::Done(v) => v,
            Outcome::AwaitJobs(w) => w.finish(&mut self.app),
            Outcome::Retry(e) => return Err(e),
            Outcome::AfterInput => {
                drop(self.app.take_synthetic_input());
                return Err(format!("`{method}` is synthetic input, which the desk does not take in-process; run the command with engine.execute"));
            }
            Outcome::Screenshot { .. } => return Err("the desk takes no window screenshots: use grab_png, or panel_ui to draw a panel".into()),
        };
        if reply["ok"].as_bool() == Some(true) { Ok(reply["result"].clone()) } else { Err(reply["error"].as_str().unwrap_or("failed").to_string()) }
    }

    /// Check a [`Desk::control`] request against the desk's roots before it runs (the checks of
    /// `effectcraft_automation::Backend::exec`). Methods not known to touch no files are refused.
    fn confine(&mut self, method: &str, params: &mut Value) -> Result<(), String> {
        let c = &self.roots;
        match method {
            "engine.execute" | "ui.menu.invoke" => {
                let id = params.get("command").or(params.get("id")).and_then(Value::as_str).unwrap_or_default().to_string();
                let inner = params.get("params").cloned().filter(|v| !v.is_null()).unwrap_or(json!({}));
                let lookup = c.check_command(&id, &inner).map_err(|e| e.to_string())?;
                let Some(lookup) = lookup else { return Ok(()) };
                let s = &mut self.app.session;
                let found = match lookup {
                    Lookup::ProjectFile => s.execute("project.summary", json!({})),
                    Lookup::RenderOutputs => s.execute("renderQueue.list", json!({})),
                };
                c.check_lookup(&lookup, &found.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
            }
            // Without a path the frame would go to a temp file outside the roots: inline instead.
            "render.frame" => match params.get("path").and_then(Value::as_str) {
                Some(p) => c.check(p, Access::Write).map(drop).map_err(|e| e.to_string()),
                None => {
                    match params.as_object_mut() {
                        Some(o) => drop(o.insert("base64".into(), json!(true))),
                        None => *params = json!({"base64": true}),
                    }
                    Ok(())
                }
            },
            // No files (synthetic input and screenshots are refused when they come back).
            "engine.commands" | "ui.menu.list" | "ui.inspect" | "ui.elements" | "ui.set" | "ui.panel.show" | "ui.panel.close" | "ui.viewer.locate"
            | "ui.viewer.hit" | "ui.timeline.locate" | "ui.playback" | "ui.resize" | "ui.focus" | "app.quit" | "ui.click" | "ui.move" | "ui.drag"
            | "ui.scroll" | "ui.key" | "ui.type" | "ui.screenshot" => Ok(()),
            m => Err(format!("unknown method `{m}`")),
        }
    }

    /// The `engine.commands` listing for agents: every command with its parameters, typed where
    /// the code says what they take (`kind`, and `choices`, `items` for arrays), `kind: "any"`
    /// where its doc leaves a parameter free; and every effect with its parameters typed from
    /// the effect registry (kind, default, min/max, slider range, choices, file types), with
    /// the `prop.set` path of each. `typed` / `untyped` count the commands.
    pub fn commands(&self) -> Value {
        let session = &self.app.session;
        let (mut typed, mut untyped) = (0, 0);
        let commands: Vec<Value> = effectcraft_engine::command_specs()
            .iter()
            .map(|c| {
                let schema = effectcraft_engine::commands::params_schema(c);
                let required: Vec<&str> = schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let params: Vec<Value> = schema["properties"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .map(|(k, v)| {
                                let mut p = param_type(c.id, k, v["description"].as_str().unwrap_or(""));
                                p["id"] = json!(k);
                                p["optional"] = json!(!required.contains(&k.as_str()));
                                p
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if params.iter().all(|p| p["kind"] != "any") {
                    typed += 1;
                } else {
                    untyped += 1;
                }
                let enabled = (c.enabled)(session);
                json!({"id": c.id, "label": c.label, "menu": c.menu, "shortcut": c.shortcut, "doc": c.params, "params": params, "enabled": enabled.is_ok(), "why": enabled.err()})
            })
            .collect();
        let effects: Vec<Value> = effectcraft_engine::effects::all().into_iter().map(effect_json).collect();
        json!({"commands": commands, "typed": typed, "untyped": untyped, "effects": effects})
    }

    /// The panels [`Desk::panel_ui`] draws (ids `ui.panel.show` takes too).
    pub fn panels(&self) -> Vec<String> {
        PanelKind::ALL.iter().map(|p| p.id()).collect()
    }

    /// Narrow what [`Desk::panel_ui`] shows to `target` (any of): `{comp, layer | layers,
    /// effect, time, range: [start, end]}`. `comp` opens that composition; `layer(s)` selects
    /// them (id, name or `#n`); `effect` (a uid, 1-based index, or effect name or id on the
    /// first of them) makes Effect Controls show only that effect and selects it (null shows
    /// them all again), and with `params: [id]` (the effect's parameter ids, `tweaking/size`)
    /// only those rows, in that order (absent or empty: all); `time` (seconds) moves the
    /// playhead; `range` scrolls and zooms the Timeline to those seconds. `{}` clears the
    /// narrowing.
    pub fn focus(&mut self, target: Value) -> Result<(), String> {
        let s = &mut self.app.session;
        if target.as_object().is_some_and(|o| o.is_empty()) {
            self.focus_effect = None;
            self.focus_params.clear();
            self.focus_range = None;
            return Ok(());
        }
        if let Some(c) = target.get("comp").filter(|c| !c.is_null()) {
            s.execute("comp.open", json!({"comp": c})).map_err(|e| e.to_string())?;
        }
        let layers = match (target.get("layers"), target.get("layer")) {
            (Some(Value::Array(a)), _) => Some(a.clone()),
            (_, Some(l)) if !l.is_null() => Some(vec![l.clone()]),
            _ => None,
        };
        if let Some(layers) = layers {
            s.execute("layer.select", json!({"layers": layers})).map_err(|e| e.to_string())?;
        }
        if let Some(t) = target.get("time").and_then(Value::as_f64) {
            s.execute("time.set", json!({"time": t})).map_err(|e| e.to_string())?;
        }
        if let Some(e) = target.get("effect") {
            self.focus_effect = None;
            self.focus_params.clear();
            if !e.is_null() {
                let layer = s.state.selected_layers.first().copied().ok_or("select a layer for the effect (`layer`)")?;
                let comp = s.active_comp().ok_or("no composition is open")?;
                let l = comp.layers.iter().find(|l| l.id == layer).ok_or("no such layer")?;
                let fx: Vec<(u64, String, String)> = l
                    .effects()
                    .map(|g| {
                        g.groups()
                            .filter_map(|g| if let GroupKind::Effect { effect } = &g.kind { Some((g.uid, g.name.clone(), effect.clone())) } else { None })
                            .collect()
                    })
                    .unwrap_or_default();
                let found = match e {
                    Value::Number(n) => n.as_u64().and_then(|n| fx.iter().find(|f| f.0 == n).or_else(|| fx.get((n as usize).wrapping_sub(1)))),
                    Value::String(name) => fx.iter().find(|f| f.1.eq_ignore_ascii_case(name) || f.2 == *name),
                    _ => None,
                };
                let (uid, spec) = found.map(|f| (f.0, f.2.clone())).ok_or_else(|| format!("no effect {e} on layer {}", l.name))?;
                let params: Vec<String> = match target.get("params") {
                    None | Some(Value::Null) => vec![],
                    Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("`params` takes ids, not {v}"))).collect::<Result<_, _>>()?,
                    Some(v) => return Err(format!("`params` takes a list of parameter ids, not {v}")),
                };
                let known: Vec<&str> = effectcraft_engine::effects::find(&spec).map(|f| f.params.iter().map(|p| p.id).collect()).unwrap_or_default();
                let unknown: Vec<&str> = params.iter().map(String::as_str).filter(|p| !known.contains(p)).collect();
                if !unknown.is_empty() {
                    return Err(format!("{spec} has no parameter {}", unknown.join(", ")));
                }
                s.execute("prop.select", json!({"layer": layer.0, "prop": uid})).map_err(|e| e.to_string())?;
                self.focus_effect = Some(uid);
                self.focus_params = params;
            }
        }
        if let Some(Value::Array(r)) = target.get("range")
            && let (Some(a), Some(b)) = (r.first().and_then(Value::as_f64), r.get(1).and_then(Value::as_f64))
            && b > a
        {
            self.focus_range = Some((a, b));
        }
        Ok(())
    }

    /// Draw and run the editor inside `ui`'s rect of the host window, one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.run(ui, frame, None);
    }

    /// Draw and run only `panel` (one of [`Desk::panels`]) inside `ui`'s rect, one frame, as
    /// [`Desk::focus`] narrowed it: Fossa Direct's card for what the chat is about. Its
    /// dialogs and menus stay in that rect.
    pub fn panel_ui(&mut self, panel: &str, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let Some(p) = PanelKind::from_name(panel) else {
            ui.label(format!("No panel `{panel}`"));
            return;
        };
        self.run(ui, frame, Some(p));
    }

    fn run(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame, panel: Option<PanelKind>) {
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
                    matches!(
                        e,
                        egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) | egui::Event::Ime(_)
                    )
                });
                i.events = rest;
                held
            })
        };
        // The Timeline range `focus` asked for, now that the Timeline has a width.
        if let Some((a, b)) = self.focus_range
            && let Some((_, w)) = ctx.data(|d| d.get_temp::<(f32, f32)>(crate::id("timeline-graph-area")))
        {
            self.app.ui.timeline.start = a;
            self.app.ui.timeline.pps = Some(((w - 6.0).max(10.0) as f64) / (b - a));
            self.focus_range = None;
        }
        let painted = painted_layers(&ctx, ui.layer_id());
        eframe::App::logic(&mut self.app, &ctx, frame);
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).id_salt("effectcraft"), |ui| {
            ui.set_style(ctx.global_style());
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            match panel {
                Some(p) => {
                    self.app.effect_only = self.focus_effect;
                    self.app.effect_params = self.focus_params.clone();
                    self.app.panel_ui(ui, frame, p);
                    self.app.effect_only = None;
                    self.app.effect_params.clear();
                }
                None => eframe::App::ui(&mut self.app, ui, frame),
            }
        });
        keep_inside(&ctx, rect, &painted, ui.layer_id());
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
        if let Some(job) = &self.clip {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        eframe::App::on_exit(&mut self.app);
        if self.made {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Roots that let the agent read and write nothing.
fn no_files() -> Confinement {
    Confinement::new(&[], &[]).expect("no roots")
}

/// How many shapes each layer above the background holds so far this frame (what the host
/// painted before the desk).
fn painted_layers(ctx: &egui::Context, own: egui::LayerId) -> Vec<(egui::LayerId, usize)> {
    let layers = ctx.memory(|m| m.areas().visible_layer_ids());
    ctx.graphics(|g| {
        layers.into_iter().filter(|l| l.order != egui::Order::Background && *l != own).map(|l| (l, g.get(l).map_or(0, |p| p.next_idx().0))).collect()
    })
}

/// Move the desk's popups, menus, tooltips and windows into `rect`: each layer the desk painted
/// this frame (more shapes than `before`, the host's) is shifted by what sticks out of it, as
/// drawn and as clicked (egui's layer transforms).
fn keep_inside(ctx: &egui::Context, rect: egui::Rect, before: &[(egui::LayerId, usize)], own: egui::LayerId) {
    let layers = ctx.memory(|m| m.areas().visible_layer_ids());
    for l in layers {
        if l.order == egui::Order::Background || l == own {
            continue;
        }
        let had = before.iter().find(|(b, _)| *b == l).map_or(0, |b| b.1);
        if ctx.graphics(|g| g.get(l).map_or(0, |p| p.next_idx().0)) <= had {
            continue;
        }
        let Some(area) = egui::AreaState::load(ctx, l.id) else { continue };
        let shift = shift_into(area.rect(), rect);
        ctx.set_transform_layer(l, egui::emath::TSTransform::from_translation(shift));
    }
}

/// How far to move `r` so it lies within `bounds` (its top left corner when it is larger).
fn shift_into(r: egui::Rect, bounds: egui::Rect) -> egui::Vec2 {
    let axis = |min: f32, max: f32, lo: f32, hi: f32| {
        if min < lo || max - min > hi - lo {
            lo - min
        } else if max > hi {
            hi - max
        } else {
            0.0
        }
    };
    egui::vec2(axis(r.min.x, r.max.x, bounds.min.x, bounds.max.x), axis(r.min.y, r.max.y, bounds.min.y, bounds.max.y))
}

/// A command parameter's type, from the conventions of the commands' params docs and the code
/// that reads them: `{kind}` plus `choices` (`a|b|c` in the doc, or the effect registry for
/// `effect.apply`) or `items` (arrays). `kind: "any"` when the doc leaves it free.
fn param_type(command: &str, key: &str, doc: &str) -> Value {
    // The part of the doc after `key:` (or `key?:`).
    let after = doc
        .split_once(':')
        .map(|(k, v)| (k.trim().trim_end_matches('?'), v.trim()))
        .filter(|(k, _)| k.split('|').any(|k| k.trim().trim_end_matches('?') == key))
        .map(|(_, v)| v);
    if command == "effect.apply" && key == "effect" {
        let ids: Vec<&str> = effectcraft_engine::effects::all().into_iter().map(|e| e.id).collect();
        return json!({"kind": "effect", "choices": ids});
    }
    // `key: a|b|c`: one of those words.
    if let Some(v) = after {
        let words: Vec<&str> = v.split(['(', ';']).next().unwrap_or("").split('|').map(str::trim).collect();
        // (`id|name|index` names the ways to refer to something, not choices.)
        let refs = ["id", "name", "uid", "index", "path", "prop", "#n", "number", "string", "seconds"];
        if words.len() > 1 && words.iter().all(|w| !w.is_empty() && !refs.contains(w) && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))) {
            return json!({"kind": "choice", "choices": words});
        }
    }
    let seconds = doc.contains("(s)") || doc.contains("seconds") || doc.contains("comp s");
    let kind = match key {
        "layer" | "parent" | "matte" => "layer",
        "layers" => return json!({"kind": "array", "items": "layer"}),
        "comp" => "comp",
        "item" | "folder" if !command.starts_with("file.") => "item",
        "items" => return json!({"kind": "array", "items": "item"}),
        "effect" | "mask" | "tracker" | "animator" | "prop" | "group" => "property",
        "props" | "properties" | "masks" | "effects" => return json!({"kind": "array", "items": "property"}),
        "paths" | "files" => return json!({"kind": "array", "items": "file"}),
        "path" if command.starts_with("prop.") || command.starts_with("key") || doc.contains("prop") => "property",
        "path" | "file" | "folder" | "out" if !seconds => "file",
        "time" | "in" | "out" | "at" | "start" | "end" | "duration" | "seconds" | "offset" => "number",
        "frame" | "index" | "count" | "width" | "height" | "fps" | "frameRate" | "x" | "y" | "z" | "scale" | "opacity" | "amount" | "size" | "step" | "pps" => {
            "number"
        }
        "name" | "text" | "title" | "comment" | "query" | "filter" | "timecode" | "preset" | "newName" | "label" | "url" | "script" | "expression"
        | "command" | "font" => "string",
        "color" | "fill" | "stroke" => "color",
        "wait" | "add" | "toggle" | "on" | "enabled" | "force" | "background" | "edit" | "all" | "enabledOnly" | "schemas" | "selected" | "keys"
        | "selectKeys" | "relative" | "loop" | "merge" | "solo" | "visible" | "locked" | "shy" => "bool",
        _ if seconds => "number",
        _ => "any",
    };
    json!({"kind": kind})
}

/// An effect of the registry with its parameters typed: what an agent needs to apply it
/// (`effect.apply {effect: id}`) and set each parameter (`prop.set {layer, path, value}`, the
/// path with `{effect}` replaced by the uid `effect.apply` returned, or `#n` for the n-th).
fn effect_json(e: &EffectSpec) -> Value {
    let params: Vec<Value> = e
        .params
        .iter()
        .filter(|p| !matches!(p.ui, ParamUi::Hidden))
        .map(|p| {
            let mut o = json!({"id": p.id, "name": p.name, "default": p.default.to_json(), "path": format!("effects/@{{effect}}/{}", p.id)});
            let kind = match &p.ui {
                ParamUi::Number => "number",
                ParamUi::Slider { min, max, slider_min, slider_max, decimals } => {
                    o["min"] = json!(min);
                    o["max"] = json!(max);
                    o["sliderMin"] = json!(slider_min);
                    o["sliderMax"] = json!(slider_max);
                    o["decimals"] = json!(decimals);
                    "number"
                }
                ParamUi::Percent => "percent",
                ParamUi::Angle => "angle",
                ParamUi::Pixels => "pixels",
                ParamUi::Point => "point",
                ParamUi::Point3 => "point3",
                ParamUi::Color => "color",
                ParamUi::Checkbox => "bool",
                ParamUi::Popup { options } => {
                    o["choices"] = json!(options);
                    "choice"
                }
                ParamUi::Layer => "layer",
                ParamUi::Mask => "mask",
                ParamUi::Path => "path",
                ParamUi::Text => "text",
                ParamUi::File { filters } => {
                    o["fileTypes"] = json!(filters);
                    "file"
                }
                ParamUi::Gradient => "gradient",
                ParamUi::Hidden => "hidden",
            };
            o["kind"] = json!(kind);
            o
        })
        .collect();
    json!({"id": e.id, "name": e.name, "category": e.category, "params": params})
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_popup_is_moved_into_the_desk() {
        let desk = egui::Rect::from_min_max(egui::pos2(100.0, 50.0), egui::pos2(1000.0, 370.0));
        let menu = egui::Rect::from_min_size(egui::pos2(900.0, 300.0), egui::vec2(200.0, 150.0));
        assert_eq!(shift_into(menu, desk), egui::vec2(-100.0, -80.0));
        assert_eq!(shift_into(egui::Rect::from_min_size(egui::pos2(200.0, 60.0), egui::vec2(10.0, 10.0)), desk), egui::Vec2::ZERO);
        // Larger than the desk: its top left corner goes to the desk's.
        assert_eq!(shift_into(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2000.0, 10.0)), desk), egui::vec2(100.0, 50.0));
    }

    #[test]
    fn parameters_are_typed_from_their_docs() {
        assert_eq!(param_type("x", "kind", "kind: range|wiggly|expression")["choices"], json!(["range", "wiggly", "expression"]));
        assert_eq!(param_type("layer.select", "layers", "layers: [id|name|#n]")["kind"], "array");
        assert_eq!(param_type("time.set", "time", "time? (s) | frame? | timecode?")["kind"], "number");
        assert_eq!(param_type("x", "value", "value?")["kind"], "any");
        assert_eq!(param_type("effect.apply", "effect", "effect: id|name (e.g. Gaussian Blur)")["kind"], "effect");
    }
}
