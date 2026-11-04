//! The egui application: project/disc selection, one-click processing,
//! asset browser with review, backend comparison, settings, queue, export
//! and diagnostics. All heavy work runs on a worker thread.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, Sense, Vec2};

use wwhd_tex::classify::{Class, ALL_CLASSES};
use wwhd_tex::inventory::{self, Inventory, TextureRecord};
use wwhd_tex::jobs::Progress;
use wwhd_tex::mods_dir::Env;
use wwhd_tex::pipeline::{self, ExportReport, UpscaleTask};
use wwhd_tex::policy::{self, Decisions, Plan, Review, ReviewStatus};
use wwhd_tex::project::{Project, ScaleChoice, ScanScope, Settings, SCALES};
use wwhd_tex::resample::Filter;
use wwhd_tex::tools_install;
use wwhd_tex::runtime_pack::{self, RuntimeReport};
use wwhd_tex::upscale::{self, BackendChoice, Tools};

use crate::cli::{self, InstallKind};
use crate::derived::{self, EnvInfo, Estimate, Found, FoundCache};
use crate::images::ImageCache;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Quickstart,
    Dashboard,
    Assets,
    Compare,
    Settings,
    Queue,
    Export,
    Diagnostics,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompareMode {
    SideBySide,
    Slider,
    Alpha,
    Normals,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    WillUpscale,
    Native,
    Upscaled,
    NotUpscaled,
    Pending,
    Approved,
    Rejected,
    Problems,
}

impl StatusFilter {
    const ALL: [StatusFilter; 9] = [
        StatusFilter::All,
        StatusFilter::WillUpscale,
        StatusFilter::Native,
        StatusFilter::Upscaled,
        StatusFilter::NotUpscaled,
        StatusFilter::Pending,
        StatusFilter::Approved,
        StatusFilter::Rejected,
        StatusFilter::Problems,
    ];
    fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::WillUpscale => "Will be upscaled",
            StatusFilter::Native => "Stays native",
            StatusFilter::Upscaled => "Upscaled",
            StatusFilter::NotUpscaled => "Waiting for upscale",
            StatusFilter::Pending => "Not reviewed",
            StatusFilter::Approved => "Approved",
            StatusFilter::Rejected => "Rejected",
            StatusFilter::Problems => "Unsupported / failed gate",
        }
    }
}

enum Msg {
    Inventory(Inventory),
    Exported(ExportReport),
    RuntimeExported(RuntimeReport),
    /// Package ids installed and the install's warnings.
    Installed(Vec<String>, Vec<String>),
    /// The upscaled-file scan finished.
    Found(derived::ScanResult),
    /// Upscaling models installed: the tools folder changed.
    ToolsInstalled,
    Done(String),
    Failed(String),
}

pub struct App {
    project: Option<Project>,
    settings: Settings,
    inv: Option<Arc<Inventory>>,
    decisions: Decisions,
    plans: Vec<Plan>,
    /// The plans the runtime pack uses (no per-archive budget, no excluded archives).
    rt_plans: Vec<Plan>,
    estimate: Estimate,
    /// The cached upscale of each planned texture (see `derived::FoundCache`).
    found: FoundCache,
    /// Settings changed since the plans and estimates were last worked out; the user presses Refresh estimates.
    settings_stale: bool,
    /// Index into `inv.textures` of one record per unique key, in display order.
    unique: Vec<usize>,
    filtered: Vec<usize>,
    progress: Progress,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    images: Option<ImageCache>,
    view: View,
    search: String,
    class_filter: Option<Class>,
    status_filter: StatusFilter,
    selected: Option<usize>,
    compare_mode: CompareMode,
    zoom: f32,
    slider: f32,
    compare_backends: Vec<(BackendChoice, bool)>,
    reprocess_backend: BackendChoice,
    log_seen: usize,
    log: Vec<String>,
    last_report: Option<ExportReport>,
    runtime_report: Option<RuntimeReport>,
    /// Mods folder, game executable and disc folder checks; re-read when `env_dirty`.
    env_info: Option<EnvInfo>,
    env_dirty: bool,
    /// Which supported AI upscalers are in the tools folder; re-read when `tools_dirty`.
    tools_status: Vec<tools_install::Status>,
    tools_dirty: bool,
    install_confirm: bool,
    /// An install waits for the user to agree to create the missing mods folder.
    create_confirm: Option<InstallKind>,
    install_warnings: Vec<String>,
    compare_scale: u32,
    /// Per selected model: its cached result for the comparison scale, and the run's time/notes for the label.
    compare_found: Vec<(BackendChoice, Found, String)>,
    compare_for: Option<(usize, u32)>,
    compare_dirty: bool,
    status: String,
    filter_dirty: bool,
    thumb_size: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> App {
        let (tx, rx) = channel();
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(40, 110, 160);
        cc.egui_ctx.set_visuals(visuals);
        let tools = Tools { dir: Settings::default().tools_dir };
        let compare_backends = upscale::catalogue()
            .into_iter()
            .filter(|b| *b != BackendChoice::Native)
            .map(|b| {
                let on = matches!(&b, BackendChoice::Filter(Filter::Lanczos3))
                    || matches!(&b, BackendChoice::RealEsrgan(m) if m == "realesrgan-x4plus-anime" || m == "realesr-animevideov3")
                    || matches!(&b, BackendChoice::RealCugan { model, noise: -1 } if model == "models-se")
                    || matches!(&b, BackendChoice::Waifu2x { model, noise: -1 } if model == "models-cunet");
                (b, on)
            })
            .collect();
        let _ = tools;
        let mut app = App {
            project: None,
            settings: Settings::default(),
            inv: None,
            decisions: Decisions::default(),
            plans: Vec::new(),
            rt_plans: Vec::new(),
            estimate: Estimate::default(),
            found: FoundCache::default(),
            settings_stale: false,
            unique: Vec::new(),
            filtered: Vec::new(),
            progress: Progress::default(),
            tx,
            rx,
            images: Some(ImageCache::new(&cc.egui_ctx, 1500)),
            view: View::Dashboard,
            search: String::new(),
            class_filter: None,
            status_filter: StatusFilter::All,
            selected: None,
            compare_mode: CompareMode::SideBySide,
            zoom: 1.0,
            slider: 0.5,
            compare_backends,
            reprocess_backend: BackendChoice::default(),
            log_seen: 0,
            log: Vec::new(),
            last_report: None,
            runtime_report: None,
            env_info: None,
            env_dirty: true,
            tools_status: Vec::new(),
            tools_dirty: true,
            install_confirm: false,
            create_confirm: None,
            install_warnings: Vec::new(),
            compare_scale: 4,
            compare_found: Vec::new(),
            compare_for: None,
            compare_dirty: true,
            status: "Choose a project folder to start.".into(),
            filter_dirty: true,
            thumb_size: 96.0,
        };
        // Reopen the last project.
        if let Some(last) = std::fs::read_to_string(last_project_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            app.open_project(PathBuf::from(last));
        }
        app
    }

    fn open_project(&mut self, dir: PathBuf) {
        match Project::open(&dir) {
            Ok(p) => {
                self.settings = p.load_settings();
                if self.settings.tools_dir.is_relative() {
                    // Resolve next to the executable (the release zip), in its repository layout, or the working dir.
                    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf()));
                    let exe_rel = exe_dir.and_then(|d| {
                        [d.join(&self.settings.tools_dir), d.join("../..").join(&self.settings.tools_dir)].into_iter().find(|c| c.exists())
                    });
                    if let Some(c) = exe_rel {
                        // absolute, not canonicalize: on Windows that gives a \\?\ path, which the ncnn tools take as
                        // relative and join to their own folder (the model files were not found)
                        self.settings.tools_dir = std::path::absolute(&c).unwrap_or(c);
                    }
                }
                self.decisions = Decisions::load(&p.review_path(), &p.classes_path());
                self.progress = Progress::with_log_file(p.log_path());
                self.log.clear();
                self.log_seen = 0;
                self.inv = Inventory::load(&p.inventory_path()).ok().map(Arc::new);
                self.last_report = std::fs::read(p.reports_dir().join("export_report.json")).ok().and_then(|d| serde_json::from_slice(&d).ok());
                self.runtime_report = std::fs::read(p.reports_dir().join("runtime_export_report.json")).ok().and_then(|d| serde_json::from_slice(&d).ok());
                self.env_dirty = true;
                self.install_warnings.clear();
                let _ = std::fs::write(last_project_file(), dir.to_string_lossy().as_bytes());
                self.status = match &self.inv {
                    Some(i) => format!("Project loaded: {} textures from {}", i.textures.len(), i.root),
                    None => "Project opened. Choose the game disc folder and press Process everything (or Scan).".into(),
                };
                self.project = Some(p);
                self.recompute();
            }
            Err(e) => self.status = format!("Cannot open project: {e}"),
        }
    }

    /// Save the settings. The plans and estimates are not worked out again (a slider drag would redo them every
    /// frame): they are marked out of date until the user presses Refresh estimates.
    /// `affects_plans` false for settings that decide no plan (the disc folder only makes a re-scan necessary).
    fn save_settings_with(&mut self, affects_plans: bool) {
        if let Some(p) = &self.project {
            let _ = p.save_settings(&self.settings);
        }
        self.settings_stale |= affects_plans;
        self.env_dirty = true;
    }

    /// The disc folder chosen is not the one the inventory was scanned from.
    fn rescan_needed(&self) -> bool {
        self.inv.as_ref().is_some_and(|i| i.root != self.settings.disc_root.to_string_lossy())
    }

    fn save_decisions(&mut self) {
        if let Some(p) = &self.project {
            if let Err(e) = self.decisions.save(&p.review_path(), &p.classes_path()) {
                self.status = format!("Could not save review: {e}");
            }
        }
        self.recompute();
    }

    fn recompute(&mut self) {
        self.settings_stale = false;
        let Some(inv) = self.inv.clone() else {
            self.plans.clear();
            self.rt_plans.clear();
            self.unique.clear();
            self.filtered.clear();
            self.estimate = Estimate::default();
            self.found.clear();
            return;
        };
        self.plans = policy::plan_all(&inv, &self.settings, &self.decisions);
        self.rt_plans = policy::plan_all_runtime(&inv, &self.settings, &self.decisions);
        let mut seen = std::collections::HashSet::new();
        self.unique = (0..inv.textures.len()).filter(|&i| seen.insert(inv.textures[i].key.clone())).collect();
        self.estimate = derived::estimate(&inv, &self.unique, &self.plans, &self.rt_plans, &self.decisions, &self.settings);
        self.filter_dirty = true;
        self.compare_dirty = true;
        self.rescan();
    }

    /// Look for the upscaled files again on a worker thread (the check touches the disk for every planned texture).
    fn rescan(&mut self) {
        if self.project.is_none() || self.inv.is_none() {
            self.found.clear();
        } else if self.found.invalidate() {
            self.start_scan();
        }
    }

    fn start_scan(&self) {
        let (Some(p), Some(inv)) = (self.project.clone(), self.inv.clone()) else { return };
        let (plans, unique, tx) = (self.plans.clone(), self.unique.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Found(derived::scan_upscaled(&p, &inv, &unique, &plans)));
        });
    }

    /// The cached upscaled file of texture `i`.
    fn upscaled_path(&self, i: usize) -> Option<PathBuf> {
        self.found.get(i).map(|(p, _)| p.clone())
    }

    /// The scale the cached upscale of `i` really has, when it stepped down from the plan.
    fn stepped_down(&self, i: usize) -> Option<u32> {
        self.found.get(i).map(|f| f.1).filter(|&a| a != self.plans[i].scale)
    }

    fn apply_filter(&mut self) {
        let Some(inv) = &self.inv else { return };
        let q = self.search.to_lowercase();
        let filtered: Vec<usize> = self
            .unique
            .iter()
            .copied()
            .filter(|&i| {
                let t = &inv.textures[i];
                if !q.is_empty() && !t.id.name.to_lowercase().contains(&q) && !t.id.location.display().to_lowercase().contains(&q) {
                    return false;
                }
                if let Some(c) = self.class_filter {
                    if self.decisions.classification(t).class != c {
                        return false;
                    }
                }
                let pl = &self.plans[i];
                let review = self.decisions.review(&t.key).status;
                match self.status_filter {
                    StatusFilter::All => true,
                    StatusFilter::WillUpscale => pl.upscales(),
                    StatusFilter::Native => !pl.upscales(),
                    StatusFilter::Upscaled => self.found.get(i).is_some(),
                    StatusFilter::NotUpscaled => pl.upscales() && self.found.get(i).is_none(),
                    StatusFilter::Pending => review == ReviewStatus::Pending,
                    StatusFilter::Approved => review == ReviewStatus::Approved,
                    StatusFilter::Rejected => review == ReviewStatus::Rejected,
                    StatusFilter::Problems => !t.gate_ok || !t.decodable,
                }
            })
            .collect();
        self.filtered = filtered;
        self.filter_dirty = false;
    }

    fn busy(&self) -> bool {
        self.progress.is_running()
    }

    /// Run `f` on a worker thread with the shared progress.
    fn spawn<F>(&mut self, label: &str, f: F)
    where
        F: FnOnce(&Progress, &Sender<Msg>) -> wwhd_tex::Result<String> + Send + 'static,
    {
        if self.busy() {
            return;
        }
        let prog = self.progress.clone();
        let tx = self.tx.clone();
        prog.set_running(true);
        prog.log(format!("— {label} —"));
        self.status = format!("{label}…");
        std::thread::spawn(move || {
            let r = f(&prog, &tx);
            prog.set_running(false);
            let _ = tx.send(match r {
                Ok(s) => Msg::Done(s),
                Err(e) => Msg::Failed(e.to_string()),
            });
        });
    }

    fn start_pipeline(&mut self, scan: bool, extract: bool, upscale_all: bool, export: bool) {
        let Some(p) = self.project.clone() else { return };
        let s = self.settings.clone();
        let d = self.decisions.clone();
        let inv0 = self.inv.as_ref().map(|i| (**i).clone());
        self.spawn("Processing", move |prog, tx| {
            let mut inv = match inv0 {
                Some(i) if !scan && i.root == s.disc_root.to_string_lossy() => i,
                _ => {
                    if s.disc_root.as_os_str().is_empty() {
                        return Err(wwhd_tex::Error::Format("choose the game disc folder first".into()));
                    }
                    let i = inventory::scan(&s.disc_root, &s.scope, prog)?;
                    i.save(&p.inventory_path())?;
                    let _ = tx.send(Msg::Inventory(i.clone()));
                    i
                }
            };
            if extract {
                pipeline::extract(&p, &mut inv, prog)?;
                inv.save(&p.inventory_path())?;
                let _ = tx.send(Msg::Inventory(inv.clone()));
            }
            if upscale_all {
                let tasks = pipeline::all_upscale_tasks(&inv, &s, &d);
                pipeline::upscale(&p, &s, &inv, &d, &tasks, prog)?;
            }
            if export {
                let r = pipeline::export(&p, &s, &inv, &d, prog)?;
                let msg = format!("Exported {} textures in {} archives into {} package(s)", r.textures_replaced, r.files_rebuilt, r.packages.len());
                let _ = tx.send(Msg::Exported(r));
                return Ok(msg);
            }
            Ok("Finished".into())
        });
    }

    fn reprocess(&mut self, records: Vec<usize>, backend: BackendChoice) {
        let (Some(p), Some(inv)) = (self.project.clone(), self.inv.clone()) else { return };
        let s = self.settings.clone();
        let mut d = self.decisions.clone();
        for &i in &records {
            let key = inv.textures[i].key.clone();
            let mut r = d.review(&key);
            r.backend = Some(backend.clone());
            d.reviews.insert(key, r);
        }
        self.decisions = d.clone();
        self.save_decisions();
        let plans = policy::plan_all(&inv, &self.settings, &self.decisions);
        // Force a fresh run: drop cached results of this model for these textures.
        for &i in &records {
            if plans[i].upscales() {
                // Also removes a stepped-down result (e.g. the x16 file of a x32 plan).
                let path = pipeline::remove_upscaled(&p, &plans[i].backend.key(), &inv.textures[i].key, plans[i].scale)
                    .unwrap_or_else(|| p.upscaled_path(&plans[i].backend.key(), &inv.textures[i].key, plans[i].scale));
                if let Some(c) = self.images.as_mut() {
                    c.invalidate(&path);
                }
            }
        }
        let tasks: Vec<UpscaleTask> = records
            .iter()
            .filter(|&&i| plans[i].upscales())
            .map(|&i| UpscaleTask { key: inv.textures[i].key.clone(), record: i, scale: plans[i].scale, backend: plans[i].backend.clone() })
            .collect();
        self.spawn("Reprocessing", move |prog, _| {
            pipeline::upscale(&p, &s, &inv, &d, &tasks, prog)?;
            Ok(format!("Reprocessed {} texture(s)", tasks.len()))
        });
    }

    fn start_runtime_export(&mut self) {
        let (Some(p), Some(inv)) = (self.project.clone(), self.inv.clone()) else { return };
        let (s, d) = (self.settings.clone(), self.decisions.clone());
        self.spawn("Exporting runtime pack", move |prog, tx| {
            let r = runtime_pack::export_runtime(&p, &s, &inv, &d, prog)?;
            let msg = format!("Runtime pack: {} entries in {} package(s), {} MiB", r.entries, r.packages.len(), r.bytes >> 20);
            let _ = tx.send(Msg::RuntimeExported(r));
            Ok(msg)
        });
    }

    /// Install the packages of `kind` into the detected (or chosen) mods folder.
    fn start_tools_install(&mut self) {
        let dir = self.settings.tools_dir.clone();
        self.spawn("Installing upscaling models", move |prog, tx| {
            let got = tools_install::install_missing(&dir, prog)?;
            let _ = tx.send(Msg::ToolsInstalled);
            Ok(if got.is_empty() {
                "All supported upscaling models were already installed".into()
            } else {
                format!("Installed {}", got.join(", "))
            })
        });
    }

    fn start_install(&mut self, kind: InstallKind) {
        let (Some(p), Some(info)) = (self.project.clone(), self.env_info.clone()) else { return };
        let root = info.mods_path;
        self.install_confirm = false;
        self.create_confirm = None;
        self.install_warnings.clear();
        self.spawn("Installing", move |_, tx| {
            let done = cli::install(&p, &root, true, kind)?;
            let _ = tx.send(Msg::Installed(done.ids.clone(), done.warnings.clone()));
            Ok(format!("Installed {} package(s); start the game to use them. {}", done.ids.len(), done.warnings.join(" ")))
        });
    }

    /// Something a job wrote may have changed what exists on disk.
    fn files_changed(&mut self) {
        self.filter_dirty = true;
        self.compare_dirty = true;
        self.env_dirty = true;
        self.rescan();
    }

    fn run_compare(&mut self, i: usize) {
        let (Some(p), Some(inv)) = (self.project.clone(), self.inv.clone()) else { return };
        let s = self.settings.clone();
        let scale = self.compare_scale;
        let tasks: Vec<UpscaleTask> = self
            .compare_backends
            .iter()
            .filter(|(_, on)| *on)
            .map(|(b, _)| UpscaleTask { key: inv.textures[i].key.clone(), record: i, scale, backend: b.clone() })
            .collect();
        self.spawn("Comparing backends", move |prog, _| {
            pipeline::upscale(&p, &s, &inv, &Decisions::default(), &tasks, prog)?;
            Ok(format!("Compared {} backends", tasks.len()))
        });
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Inventory(i) => {
                    self.inv = Some(Arc::new(i));
                    self.recompute();
                }
                Msg::Exported(r) => self.last_report = Some(r),
                Msg::RuntimeExported(r) => self.runtime_report = Some(r),
                Msg::Installed(ids, warnings) => {
                    self.status = format!("Installed {} package(s): {}. Restart the game.", ids.len(), ids.join(", "));
                    self.install_warnings = warnings;
                }
                Msg::Found(r) => {
                    if self.found.finish(r) {
                        self.start_scan();
                    }
                    self.filter_dirty = true;
                    self.compare_dirty = true;
                }
                Msg::ToolsInstalled => self.tools_dirty = true,
                Msg::Done(s) => {
                    self.status = s;
                    self.files_changed();
                    self.tools_dirty = true;
                }
                Msg::Failed(e) => {
                    self.status = format!("Stopped: {e}");
                    self.files_changed();
                    self.tools_dirty = true;
                }
            }
        }
        let (n, lines) = self.progress.log_lines(self.log_seen);
        self.log_seen = n;
        self.log.extend(lines);
        if self.log.len() > 4000 {
            self.log.drain(..1000);
        }
        if let Some(c) = &mut self.images {
            c.pump(ctx);
        }
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

fn last_project_file() -> PathBuf {
    std::env::temp_dir().join("wwhd_texture_studio_last_project.txt")
}

fn checker(ui: &egui::Ui, rect: egui::Rect) {
    let p = ui.painter_at(rect);
    let s = 8.0;
    let (nx, ny) = (((rect.width() / s).ceil()) as i32, ((rect.height() / s).ceil()) as i32);
    p.rect_filled(rect, 0.0, Color32::from_gray(70));
    for y in 0..ny.min(400) {
        for x in 0..nx.min(400) {
            if (x + y) % 2 == 0 {
                let r = egui::Rect::from_min_size(rect.min + Vec2::new(x as f32 * s, y as f32 * s), Vec2::splat(s)).intersect(rect);
                p.rect_filled(r, 0.0, Color32::from_gray(95));
            }
        }
    }
}

/// One line on whether the chosen disc folder looks like a dump (it has `content/`); used by the top bar and Settings.
fn disc_note(ui: &mut egui::Ui, disc: &std::path::Path, info: Option<&EnvInfo>) {
    if disc.as_os_str().is_empty() {
        ui.label(RichText::new("Not chosen yet.").weak());
    } else if info.is_some_and(|i| !i.disc_ok) {
        ui.label(RichText::new("No content folder here: this does not look like a disc dump (code/, content/, meta/).").color(Color32::from_rgb(230, 160, 80)));
    } else {
        ui.label(RichText::new("Looks like a disc dump (has content/).").weak());
    }
}

fn scale_text(v: u32) -> String {
    if v <= 1 { "Native".into() } else { format!("×{v}") }
}

fn choice_text(c: ScaleChoice) -> String {
    match c {
        ScaleChoice::Global => "Global".into(),
        ScaleChoice::Native => "Native".into(),
        ScaleChoice::X(v) => format!("×{v}"),
    }
}

fn texel_text(n: u64) -> String {
    format!("{:.1} M", n as f64 / 1e6)
}

/// Per class and in total: replacements, texels, disk and texture memory of the runtime pack at the current policy.
fn estimate_table(ui: &mut egui::Ui, id: &str, est: &Estimate) {
    egui::Grid::new(id).num_columns(5).striped(true).spacing([20.0, 4.0]).show(ui, |ui| {
        for h in ["Class", "Replaced", "Texels", "Disk", "Texture memory"] {
            ui.label(RichText::new(h).strong());
        }
        ui.end_row();
        for (c, e) in &est.classes {
            ui.label(c.label());
            ui.label(e.replaced.to_string());
            ui.label(texel_text(e.texels));
            ui.label(mib(e.disk));
            ui.label(mib(e.vram));
            ui.end_row();
        }
        let t = &est.total;
        ui.label(RichText::new("Total").strong());
        ui.label(t.replaced.to_string());
        ui.label(texel_text(t.texels));
        ui.label(mib(t.disk));
        ui.label(mib(t.vram));
        ui.end_row();
    });
    if let Some(w) = runtime_pack::vram_warning(est.total.vram) {
        ui.label(RichText::new(format!("Warning: {w}")).color(Color32::from_rgb(230, 160, 80)));
    }
}

fn mib(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64)
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.pump(ctx);
        if self.filter_dirty {
            self.apply_filter();
        }
        if self.env_dirty {
            self.env_info = Some(EnvInfo::detect(&self.settings, &Env::from_process()));
            self.env_dirty = false;
        }
        if self.tools_dirty {
            self.tools_status = tools_install::status(&self.settings.tools_dir);
            self.tools_dirty = false;
        }

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ZeldaWWHD Texture Studio");
                ui.separator();
                if ui.button("Project folder…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("Project folder (output, cache, packages)").pick_folder() {
                        self.open_project(d);
                    }
                }
                let proj = self.project.as_ref().map(|p| p.root.display().to_string()).unwrap_or_else(|| "no project".into());
                ui.label(RichText::new(proj).weak());
                ui.separator();
                let enabled = self.project.is_some() && !self.busy();
                if ui.add_enabled(enabled, egui::Button::new("Game disc folder…")).clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("Wind Waker HD disc folder (code/content/meta)").pick_folder() {
                        self.settings.disc_root = d;
                        self.save_settings_with(false);
                        if self.rescan_needed() {
                            self.status = "Disc folder changed: press Scan to read it (the earlier scan is of another folder).".into();
                        }
                    }
                }
                ui.label(RichText::new(self.settings.disc_root.display().to_string()).weak());
                disc_note(ui, &self.settings.disc_root, self.env_info.as_ref());
            });
            ui.horizontal(|ui| {
                let ready = self.project.is_some() && !self.busy() && !self.settings.disc_root.as_os_str().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new(RichText::new("▶ Process everything").strong()).fill(Color32::from_rgb(30, 100, 60)))
                    .on_hover_text("Scan (if needed), extract, upscale every eligible texture, validate and export content packages")
                    .clicked()
                {
                    self.start_pipeline(false, true, true, true);
                    self.view = View::Queue;
                }
                ui.add_enabled_ui(ready, |ui| {
                    if ui.button("Scan").clicked() {
                        self.start_pipeline(true, false, false, false);
                    }
                    if ui.button("Extract").clicked() {
                        self.start_pipeline(false, true, false, false);
                    }
                    if ui.button("Upscale").clicked() {
                        self.start_pipeline(false, false, true, false);
                        self.view = View::Queue;
                    }
                    if ui.button("Export").clicked() {
                        self.start_pipeline(false, false, false, true);
                    }
                });
                ui.separator();
                if self.busy() {
                    let snap = self.progress.snapshot();
                    if ui.button(if snap.paused { "Resume" } else { "Pause" }).clicked() {
                        self.progress.set_paused(!snap.paused);
                    }
                    if ui.button("Cancel").clicked() {
                        self.progress.cancel();
                    }
                    let frac = if snap.total > 0 { snap.done as f32 / snap.total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).desired_width(260.0).text(format!("{} {}/{}", snap.stage, snap.done, snap.total)));
                }
            });
            ui.label(RichText::new(&self.status).color(Color32::from_rgb(170, 200, 230)));
            ui.add_space(2.0);
        });

        egui::Panel::left("nav").resizable(false).exact_size(130.0).show(root, |ui| {
            ui.add_space(6.0);
            for (v, name) in [
                (View::Quickstart, "Quickstart"),
                (View::Dashboard, "Dashboard"),
                (View::Assets, "Assets"),
                (View::Compare, "Compare models"),
                (View::Settings, "Settings"),
                (View::Queue, "Queue & log"),
                (View::Export, "Export / install"),
                (View::Diagnostics, "Diagnostics"),
            ] {
                if ui.selectable_label(self.view == v, name).clicked() {
                    self.view = v;
                    // The folders may have changed outside the program (the game creates its mods folder on first start).
                    self.env_dirty = true;
                }
            }
        });

        if self.view == View::Assets {
            egui::Panel::right("detail").resizable(true).default_size(560.0).min_size(380.0).show(root, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.detail(ui));
            });
        }

        egui::CentralPanel::default().show(root, |ui| match self.view {
            View::Quickstart => quickstart(ui),
            View::Dashboard => self.dashboard(ui),
            View::Assets => self.assets(ui),
            View::Compare => self.compare(ui),
            View::Settings => self.settings_view(ui),
            View::Queue => self.queue(ui),
            View::Export => self.export_view(ui),
            View::Diagnostics => self.diagnostics(ui),
        });
    }
}

/// The Quickstart tab: QUICKSTART.txt (the same file the release zip carries), as written.
fn quickstart(ui: &mut egui::Ui) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.label(RichText::new(include_str!("../../../QUICKSTART.txt").replace('\r', "")).monospace());
    });
}

impl App {
    /// The Refresh estimates button and whether what is shown matches the settings.
    fn estimate_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.inv.is_some(), egui::Button::new("Refresh estimates"))
                .on_hover_text("Work out the plans and the estimates again with the current settings")
                .clicked()
            {
                self.recompute();
            }
            if self.settings_stale {
                ui.label(RichText::new("Settings changed: the plans and estimates are out of date. Press Refresh estimates.").color(Color32::from_rgb(230, 160, 80)));
            } else {
                ui.label(RichText::new("Plans and estimates match the current settings.").weak());
            }
        });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        let Some(inv) = self.inv.clone() else {
            ui.heading("Welcome");
            ui.label("1. Choose a project folder (anywhere with free space; nothing is written to the disc dump).");
            ui.label("2. Choose the Wind Waker HD disc folder (the one containing code/, content/ and meta/).");
            ui.label("3. Press ▶ Process everything, or run the steps one by one and review textures in Assets.");
            return;
        };
        self.estimate_controls(ui);
        ui.add_space(6.0);
        let upscaled = self.unique.iter().filter(|&&i| self.found.get(i).is_some()).count();
        let planned = self.unique.iter().filter(|&&i| self.plans[i].upscales()).count();
        let problems = self.unique.iter().filter(|&&i| !inv.textures[i].gate_ok || !inv.textures[i].decodable).count();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("dash").num_columns(2).striped(true).spacing([30.0, 6.0]).show(ui, |ui| {
                for (k, v) in [
                    ("Disc folder", inv.root.clone()),
                    ("Resource files scanned", inv.files.len().to_string()),
                    ("Textures found (all copies)", inv.textures.len().to_string()),
                    ("Unique textures", self.unique.len().to_string()),
                    ("Extracted masters", self.found.extracted.to_string()),
                    ("Planned for upscaling", planned.to_string()),
                    ("Upscaled", format!("{upscaled} / {planned}")),
                    ("Unsupported or not rewritable", problems.to_string()),
                    ("Scan / archive errors", inv.errors.len().to_string()),
                    ("Project disk use (cache + masters + output)", mib(self.found.disk_bytes)),
                ] {
                    ui.label(k);
                    ui.label(RichText::new(v).strong());
                    ui.end_row();
                }
            });
            ui.add_space(12.0);
            ui.heading("By class (rebuilt archives)");
            egui::Grid::new("classes").num_columns(6).striped(true).spacing([20.0, 4.0]).show(ui, |ui| {
                for h in ["Class", "Unique", "Upscaled", "Backend", "Size now", "Size after"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (c, e) in &self.estimate.classes {
                    ui.label(c.label());
                    ui.label(e.unique.to_string());
                    ui.label(e.upscaled.to_string());
                    ui.label(self.settings.backend_for(*c).label());
                    ui.label(mib(e.bytes_now));
                    ui.label(mib(e.bytes_after));
                    ui.end_row();
                }
                let t = &self.estimate.total;
                ui.label(RichText::new("Total (unique textures)").strong());
                ui.label(t.unique.to_string());
                ui.label(t.upscaled.to_string());
                ui.label("");
                ui.label(mib(t.bytes_now));
                ui.label(mib(t.bytes_after));
                ui.end_row();
            });
            ui.add_space(12.0);
            ui.heading("Runtime pack at the current policy");
            estimate_table(ui, "dash_rt", &self.estimate);
        });
    }

    fn assets(&mut self, ui: &mut egui::Ui) {
        let Some(inv) = self.inv.clone() else {
            ui.label("Scan the disc first.");
            return;
        };
        self.estimate_controls(ui);
        ui.horizontal_wrapped(|ui| {
            ui.label("Search");
            if ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(160.0)).changed() {
                self.filter_dirty = true;
            }
            egui::ComboBox::from_id_salt("classf")
                .selected_text(self.class_filter.map(|c| c.label()).unwrap_or("All classes"))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.class_filter.is_none(), "All classes").clicked() {
                        self.class_filter = None;
                        self.filter_dirty = true;
                    }
                    for c in ALL_CLASSES {
                        if ui.selectable_label(self.class_filter == Some(c), c.label()).clicked() {
                            self.class_filter = Some(c);
                            self.filter_dirty = true;
                        }
                    }
                });
            egui::ComboBox::from_id_salt("statusf").selected_text(self.status_filter.label()).show_ui(ui, |ui| {
                for f in StatusFilter::ALL {
                    if ui.selectable_label(self.status_filter == f, f.label()).clicked() {
                        self.status_filter = f;
                        self.filter_dirty = true;
                    }
                }
            });
            ui.label(format!("{} shown", self.filtered.len()));
            ui.add(egui::Slider::new(&mut self.thumb_size, 48.0..=192.0).text("size"));
        });
        ui.horizontal(|ui| {
            if ui.button("Approve all shown").clicked() {
                for &i in &self.filtered.clone() {
                    let k = inv.textures[i].key.clone();
                    self.decisions.reviews.entry(k).or_default().status = ReviewStatus::Approved;
                }
                self.save_decisions();
            }
            if ui.button("Reject all shown").clicked() {
                for &i in &self.filtered.clone() {
                    let k = inv.textures[i].key.clone();
                    self.decisions.reviews.entry(k).or_default().status = ReviewStatus::Rejected;
                }
                self.save_decisions();
            }
            if ui.button("Reset review of shown").clicked() {
                for &i in &self.filtered.clone() {
                    self.decisions.reviews.remove(&inv.textures[i].key);
                }
                self.save_decisions();
            }
            ui.separator();
            egui::ComboBox::from_id_salt("batchbackend").selected_text(self.reprocess_backend.label()).show_ui(ui, |ui| {
                for b in upscale::catalogue() {
                    let l = b.label();
                    ui.selectable_value(&mut self.reprocess_backend, b, l);
                }
            });
            if ui.add_enabled(!self.busy(), egui::Button::new("Reprocess shown with this")).clicked() {
                let recs = self.filtered.clone();
                let b = self.reprocess_backend.clone();
                self.reprocess(recs, b);
            }
        });
        ui.separator();
        let p = self.project.clone().unwrap();
        let size = self.thumb_size;
        let per_row = ((ui.available_width() / (size + 8.0)).floor() as usize).max(1);
        let rows = self.filtered.len().div_ceil(per_row);
        let filtered = self.filtered.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, size + 22.0, rows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    for &i in filtered.iter().skip(r * per_row).take(per_row) {
                        let t = &inv.textures[i];
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(size, size + 16.0), Sense::click());
                        let img_rect = egui::Rect::from_min_size(rect.min, Vec2::splat(size));
                        checker(ui, img_rect);
                        if let Some(tex) = self.images.as_mut().unwrap().get(&pipeline::thumb_path(&p, &t.key)) {
                            let ts = tex.size_vec2();
                            let s = (size / ts.x.max(ts.y)).min(size / 8.0f32.max(1.0) * 8.0);
                            let fit = egui::Rect::from_center_size(img_rect.center(), ts * s.min(size / ts.x.max(ts.y)));
                            ui.painter().image(tex.id(), fit, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                        }
                        let review = self.decisions.review(&t.key).status;
                        let col = match review {
                            ReviewStatus::Approved => Color32::from_rgb(60, 180, 90),
                            ReviewStatus::Rejected => Color32::from_rgb(200, 60, 60),
                            ReviewStatus::Pending => {
                                if self.plans[i].upscales() {
                                    Color32::from_rgb(90, 140, 200)
                                } else {
                                    Color32::from_gray(90)
                                }
                            }
                        };
                        let stroke = if self.selected == Some(i) { egui::Stroke::new(3.0, Color32::YELLOW) } else { egui::Stroke::new(1.5, col) };
                        ui.painter().rect_stroke(img_rect, 2.0, stroke, egui::StrokeKind::Inside);
                        let mut name = t.id.name.clone();
                        if name.len() > 16 {
                            name.truncate(15);
                            name.push('…');
                        }
                        ui.painter().text(egui::pos2(rect.min.x + 2.0, rect.max.y - 14.0), egui::Align2::LEFT_TOP, name, egui::FontId::proportional(11.0), Color32::LIGHT_GRAY);
                        if resp.clicked() {
                            self.selected = Some(i);
                        }
                        resp.on_hover_text(format!(
                            "{}\n{}x{} {}\n{}{}",
                            t.id.location.display(),
                            t.surface.width,
                            t.surface.height,
                            wwhd_tex::gx2::format_name(t.surface.format),
                            self.plans[i].reason,
                            self.stepped_down(i).map(|a| format!(" (reached x{a})")).unwrap_or_default()
                        ));
                    }
                });
            }
        });
    }

    fn detail(&mut self, ui: &mut egui::Ui) {
        let (Some(i), Some(inv), Some(p)) = (self.selected, self.inv.clone(), self.project.clone()) else {
            ui.label("Select a texture.");
            return;
        };
        let t: &TextureRecord = &inv.textures[i];
        let plan = self.plans[i].clone();
        let cl = self.decisions.classification(t);
        ui.heading(&t.id.name);
        ui.label(RichText::new(t.id.location.display()).weak());
        let reached = self.stepped_down(i);
        let copies = inv.textures.iter().filter(|x| x.key == t.key).count();
        egui::Grid::new("meta").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
            let s = &t.surface;
            for (k, v) in [
                ("Size", format!("{}×{}, {} mips", s.width, s.height, s.mip_count)),
                ("Format", format!("{} (tile mode {}, swizzle 0x{:x})", wwhd_tex::gx2::format_name(s.format), s.tile_mode, s.swizzle)),
                ("Colour space", if wwhd_tex::gx2::is_srgb(s.format) { "sRGB" } else { "linear / data" }.to_string()),
                ("Container", format!("{:?}", t.kind)),
                ("Copies on disc", copies.to_string()),
                ("Original hash", t.data_hash[..16].to_string()),
                ("Rewrite gate", if t.gate_ok { "passed".to_string() } else { format!("failed: {}", t.gate_detail) }),
                ("Class", format!("{} ({:.0}% — {})", cl.class.label(), cl.confidence * 100.0, cl.reason)),
                ("Plan", format!("{}{}", plan.reason, reached.map(|a| format!(" → x{a} reached: the larger scale did not work for this texture")).unwrap_or_default())),
                (
                    "Output",
                    if plan.upscales() {
                        let a = reached.unwrap_or(plan.scale);
                        format!("{}×{} via {}", s.width * a, s.height * a, plan.backend.label())
                    } else {
                        "original kept".into()
                    },
                ),
            ] {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            }
        });
        if let Some(up) = self.upscaled_path(i) {
            if let Some(m) = pipeline::load_meta(&up) {
                ui.label(format!(
                    "Made with {} in {:.2} s{}{}{}",
                    m.used.label(),
                    m.seconds,
                    if m.steps.is_empty() { String::new() } else { format!(" — {}", m.steps.join("; ")) },
                    if m.consistency_db > 0.0 { format!(", consistency {:.1} dB", m.consistency_db) } else { String::new() },
                    if m.note.is_empty() { String::new() } else { format!(" — {}", m.note) }
                ));
            }
        }
        ui.horizontal(|ui| {
            ui.label("Class:");
            let mut c = cl.class;
            egui::ComboBox::from_id_salt("setclass").selected_text(c.label()).show_ui(ui, |ui| {
                for x in ALL_CLASSES {
                    ui.selectable_value(&mut c, x, x.label());
                }
            });
            if c != cl.class {
                self.decisions.classes.insert(t.key.clone(), c);
                self.save_decisions();
            }
            if cl.manual && ui.small_button("auto").clicked() {
                self.decisions.classes.remove(&t.key);
                self.save_decisions();
            }
        });
        let mut review: Review = self.decisions.review(&t.key);
        ui.horizontal(|ui| {
            let mut changed = false;
            if ui.selectable_label(review.status == ReviewStatus::Approved, "✔ Approve").clicked() {
                review.status = ReviewStatus::Approved;
                changed = true;
            }
            if ui.selectable_label(review.status == ReviewStatus::Rejected, "✖ Reject (keep original)").clicked() {
                review.status = ReviewStatus::Rejected;
                changed = true;
            }
            if ui.selectable_label(review.status == ReviewStatus::Pending, "Undecided").clicked() {
                review.status = ReviewStatus::Pending;
                changed = true;
            }
            ui.separator();
            if ui.add(egui::Slider::new(&mut review.rating, 0..=5).text("rating")).changed() {
                changed = true;
            }
            if changed {
                self.decisions.reviews.insert(t.key.clone(), review.clone());
                self.save_decisions();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Scale:");
            let mut sc = review.scale.unwrap_or(0);
            let label = |v: u32| match v {
                0 => "auto (class / global)".to_string(),
                1 => "Native".to_string(),
                v => format!("×{v}"),
            };
            egui::ComboBox::from_id_salt("texscale").selected_text(label(sc)).show_ui(ui, |ui| {
                for v in [0u32, 1].into_iter().chain(SCALES) {
                    ui.selectable_value(&mut sc, v, label(v));
                }
            });
            if sc != review.scale.unwrap_or(0) {
                review.scale = (sc != 0).then_some(sc);
                self.decisions.reviews.insert(t.key.clone(), review.clone());
                self.save_decisions();
            }
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("rb").selected_text(self.reprocess_backend.label()).show_ui(ui, |ui| {
                for b in upscale::catalogue() {
                    let l = b.label();
                    ui.selectable_value(&mut self.reprocess_backend, b, l);
                }
            });
            if ui.add_enabled(!self.busy(), egui::Button::new("Reprocess with this model")).clicked() {
                let b = self.reprocess_backend.clone();
                self.reprocess(vec![i], b);
            }
            if review.backend.is_some() && ui.button("Use class default").clicked() {
                review.backend = None;
                self.decisions.reviews.insert(t.key.clone(), review.clone());
                self.save_decisions();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            for (m, l) in [(CompareMode::SideBySide, "Side by side"), (CompareMode::Slider, "Slider"), (CompareMode::Alpha, "Alpha"), (CompareMode::Normals, "Normal map")] {
                ui.selectable_value(&mut self.compare_mode, m, l);
            }
            ui.add(egui::Slider::new(&mut self.zoom, 0.25..=8.0).logarithmic(true).text("zoom"));
        });
        let master = p.master_path(&t.key);
        let up = self.upscaled_path(i);
        let up_label = match self.found.get(i) {
            Some((_, a)) => format!("Upscaled ×{a}"),
            None if plan.upscales() => "Not upscaled yet".to_string(),
            None => "No upscale planned".to_string(),
        };
        let map = if self.compare_mode == CompareMode::Normals { [0, 1, 2, 5] } else { pipeline::display_map(t) };
        let images = self.images.as_mut().unwrap();
        let a = images.get_mapped(&master, map);
        let b = up.as_ref().and_then(|u| images.get_mapped(u, map));
        let base = (t.surface.width.max(t.surface.height) as f32).max(1.0);
        let avail = ui.available_width();
        let fit = match self.compare_mode {
            CompareMode::SideBySide => (avail / 2.0 - 8.0) / base,
            _ => (avail - 8.0) / base,
        } * self.zoom;
        let view = Vec2::new(t.surface.width as f32, t.surface.height as f32) * fit;
        let show = |ui: &mut egui::Ui, tex: &Option<egui::TextureHandle>, tint: Option<&str>, label: &str| {
            ui.vertical(|ui| {
                ui.label(label);
                let (rect, _) = ui.allocate_exact_size(view, Sense::hover());
                checker(ui, rect);
                if let Some(tex) = tex {
                    ui.painter().image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                    if tint == Some("alpha") {
                        // Alpha preview: dim colour so coverage stands out against the checkerboard.
                        ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(120));
                    }
                }
            });
        };
        egui::ScrollArea::both().max_height(720.0).show(ui, |ui| match self.compare_mode {
            CompareMode::SideBySide => {
                ui.horizontal(|ui| {
                    show(ui, &a, None, "Original");
                    show(ui, &b, None, &up_label);
                });
            }
            CompareMode::Slider => {
                ui.add(egui::Slider::new(&mut self.slider, 0.0..=1.0).text("original ◀ ▶ upscaled"));
                let (rect, _) = ui.allocate_exact_size(view, Sense::hover());
                checker(ui, rect);
                let split = rect.min.x + rect.width() * self.slider;
                if let Some(ta) = &a {
                    let r = egui::Rect::from_min_max(rect.min, egui::pos2(split, rect.max.y));
                    ui.painter().image(ta.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(self.slider, 1.0)), Color32::WHITE);
                }
                if let Some(tb) = &b {
                    let r = egui::Rect::from_min_max(egui::pos2(split, rect.min.y), rect.max);
                    ui.painter().image(tb.id(), r, egui::Rect::from_min_max(egui::pos2(self.slider, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                ui.painter().vline(split, rect.y_range(), egui::Stroke::new(2.0, Color32::YELLOW));
            }
            CompareMode::Alpha => {
                ui.label("Transparent areas show the checkerboard; dimmed so edges and halos are visible.");
                ui.horizontal(|ui| {
                    show(ui, &a, Some("alpha"), "Original");
                    show(ui, &b, Some("alpha"), "Upscaled");
                });
            }
            CompareMode::Normals => {
                ui.label("Normal maps are stored as X/Y in red/green; upscaling resizes the vectors and renormalises them.");
                ui.horizontal(|ui| {
                    show(ui, &a, None, "Original");
                    show(ui, &b, None, "Upscaled");
                });
            }
        });
    }

    /// Look up the comparison results for texture `i` at the comparison scale (a few files; done when the selection,
    /// scale, model choice or files change, not every frame).
    fn refresh_compare(&mut self, i: usize) {
        let (Some(p), Some(inv)) = (self.project.clone(), self.inv.clone()) else { return };
        let (key, scale) = (inv.textures[i].key.clone(), self.compare_scale);
        self.compare_found = self
            .compare_backends
            .iter()
            .filter(|(_, on)| *on)
            .map(|(b, _)| {
                let found = pipeline::find_upscaled(&p, &b.key(), &key, scale);
                let extra = found
                    .as_ref()
                    .and_then(|(path, _)| pipeline::load_meta(path))
                    .map(|m| format!(" ({:.1}s{})", m.seconds, if m.note.is_empty() { String::new() } else { format!(", {}", m.note) }))
                    .unwrap_or_default();
                (b.clone(), found, extra)
            })
            .collect();
        self.compare_for = Some((i, scale));
        self.compare_dirty = false;
    }

    fn compare(&mut self, ui: &mut egui::Ui) {
        let (Some(inv), Some(p)) = (self.inv.clone(), self.project.clone()) else {
            ui.label("Scan the disc first.");
            return;
        };
        let Some(i) = self.selected else {
            ui.label("Select a texture in Assets, then come back here to run several models on it.");
            return;
        };
        let t = &inv.textures[i];
        ui.heading(format!("Compare models: {}", t.id.name));
        let tools = Tools { dir: self.settings.tools_dir.clone() };
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            for (b, on) in self.compare_backends.iter_mut() {
                let avail = tools.available(b);
                changed |= ui.add_enabled(avail, egui::Checkbox::new(on, b.label())).on_disabled_hover_text("not installed in the tools folder").changed();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Scale");
            egui::ComboBox::from_id_salt("cmpscale").selected_text(format!("×{}", self.compare_scale)).show_ui(ui, |ui| {
                for v in SCALES {
                    ui.selectable_value(&mut self.compare_scale, v, format!("×{v}"));
                }
            });
            if ui.add_enabled(!self.busy(), egui::Button::new("Run selected models")).clicked() {
                self.run_compare(i);
            }
            ui.add(egui::Slider::new(&mut self.zoom, 0.25..=8.0).logarithmic(true).text("zoom"));
            ui.label("Click a result to use that model for this texture.");
        });
        ui.separator();
        let scale = self.compare_scale;
        if changed || self.compare_dirty || self.compare_for != Some((i, scale)) {
            self.refresh_compare(i);
        }
        let cell = 300.0 * self.zoom;
        // (label, image to show, model to choose on click)
        let mut entries: Vec<(String, Option<PathBuf>, Option<BackendChoice>)> = vec![("Original".into(), Some(p.master_path(&t.key)), None)];
        for (b, found, extra) in &self.compare_found {
            let label = match found {
                Some((_, a)) if *a != scale => format!("{} ×{a} (×{scale} did not work){extra}", b.label()),
                _ => format!("{}{extra}", b.label()),
            };
            entries.push((label, found.as_ref().map(|f| f.0.clone()), Some(b.clone())));
        }
        let per_row = ((ui.available_width() / (cell + 10.0)).floor() as usize).max(1);
        let mut choose = None;
        egui::ScrollArea::both().show(ui, |ui| {
            for row in entries.chunks(per_row) {
                ui.horizontal(|ui| {
                    for (label, path, b) in row {
                        ui.vertical(|ui| {
                            ui.label(label);
                            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(cell), Sense::click());
                            checker(ui, rect);
                            let tex = path.as_ref().and_then(|path| self.images.as_mut().unwrap().get_mapped(path, pipeline::display_map(t)));
                            if let Some(tex) = tex {
                                ui.painter().image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                            } else if b.is_some() {
                                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "not run yet", egui::FontId::proportional(13.0), Color32::GRAY);
                            }
                            if resp.clicked() {
                                if let Some(b) = b {
                                    choose = Some(b.clone());
                                }
                            }
                        });
                    }
                });
            }
        });
        if let Some(b) = choose {
            let mut r = self.decisions.review(&t.key);
            r.backend = Some(b.clone());
            self.decisions.reviews.insert(t.key.clone(), r);
            self.save_decisions();
            self.status = format!("{} will use {}", t.id.name, b.label());
        }
    }

    /// Settings' first section: which AI upscalers are installed, and the button that downloads the missing ones.
    fn models_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Upscaling models");
        let list = self.tools_status.clone();
        let missing: Vec<&tools_install::Status> = list.iter().filter(|t| !t.installed).collect();
        let mib: u64 = missing.iter().map(|t| t.release.size).sum::<u64>() >> 20;
        ui.horizontal(|ui| {
            let can = !missing.is_empty() && !self.busy();
            if ui
                .add_enabled(can, egui::Button::new("Install Upscaling Models"))
                .on_hover_text("Downloads every supported AI upscaler that is missing from the tools folder, from its official GitHub release")
                .clicked()
            {
                self.start_tools_install();
            }
            let note = if list.is_empty() {
                "no official upscaler builds for this operating system".to_string()
            } else if missing.is_empty() {
                "all supported AI upscalers are installed".to_string()
            } else {
                format!("downloads {} missing tool(s), about {mib} MiB", missing.len())
            };
            ui.label(RichText::new(note).weak());
        });
        egui::Grid::new("models_status").num_columns(3).show(ui, |ui| {
            for t in &list {
                ui.label(&t.release.tool);
                if t.installed {
                    ui.label(RichText::new("installed").color(Color32::from_rgb(120, 190, 120)));
                } else {
                    ui.label(RichText::new(format!("missing ({} MiB)", t.release.size >> 20)).color(Color32::from_rgb(230, 160, 80)));
                }
                ui.label(RichText::new(format!("models: {}", t.release.licence)).weak());
                ui.end_row();
            }
        });
        ui.label(
            RichText::new(format!(
                "Into {}. Each download is checked against the official release's SHA-256 before it is unpacked.",
                self.settings.tools_dir.display()
            ))
            .weak(),
        );
        ui.separator();
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        let mut s = self.settings.clone();
        let info = self.env_info.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.models_section(ui);
            self.estimate_controls(ui);
            ui.heading("Folders");
            ui.horizontal(|ui| {
                ui.label("Game disc folder");
                let mut t = s.disc_root.display().to_string();
                if ui.add(egui::TextEdit::singleline(&mut t).desired_width(440.0)).changed() {
                    s.disc_root = PathBuf::from(t);
                }
                if ui.button("…").on_hover_text("Choose the folder that contains code/, content/ and meta/").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("Wind Waker HD disc folder (code/content/meta)").pick_folder() {
                        s.disc_root = d;
                    }
                }
            });
            disc_note(ui, &s.disc_root, info.as_ref());
            if self.rescan_needed() {
                ui.label(RichText::new("The textures shown come from a scan of another folder. Press Scan (top bar) to read this one; it is not started automatically.").color(Color32::from_rgb(230, 160, 80)));
            }
            ui.horizontal(|ui| {
                ui.label("Mods folder");
                let mut t = s.mods_root.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                let hint = info.as_ref().map(|i| i.mods_path.display().to_string()).unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut t).hint_text(hint).desired_width(440.0)).changed() {
                    s.mods_root = (!t.trim().is_empty()).then(|| PathBuf::from(t.trim()));
                }
                if ui.button("…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_title("The recomp's ModManager folder").pick_folder() {
                        s.mods_root = Some(d);
                    }
                }
                if s.mods_root.is_some() && ui.button("Use detection").clicked() {
                    s.mods_root = None;
                }
            });
            if let Some(i) = &info {
                ui.label(
                    RichText::new(format!(
                        "In use: {} (found by: {}); {}{}. Leave the field empty to use this.",
                        i.mods_path.display(),
                        i.mods_rule.label(),
                        if i.mods_exists { "the folder exists" } else { "the folder does not exist yet" },
                        if i.has_profiles { ", profiles.json present" } else { "" }
                    ))
                    .weak(),
                );
            }
            ui.horizontal(|ui| {
                ui.label("Game executable");
                let mut t = s.game_exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                let hint = info.as_ref().and_then(|i| i.game_exe.as_ref()).map(|p| p.display().to_string()).unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut t).hint_text(hint).desired_width(440.0)).changed() {
                    s.game_exe = (!t.trim().is_empty()).then(|| PathBuf::from(t.trim()));
                }
                if ui.button("…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().set_title("wwhd.exe").pick_file() {
                        s.game_exe = Some(f);
                    }
                }
                if s.game_exe.is_some() && ui.button("Use detection").clicked() {
                    s.game_exe = None;
                }
            });
            ui.label(
                RichText::new(match info.as_ref().and_then(|i| i.game_exe.as_ref()) {
                    Some(p) => format!("In use: {}. Its folder decides portable mode (portable.txt) when looking for the mods folder.", p.display()),
                    None => "No game executable known (none chosen, none in build/game beside the disc folder's checkout): portable mode is not checked.".into(),
                })
                .weak(),
            );
            ui.heading("Scanning");
            ui.horizontal(|ui| {
                ui.radio_value(&mut s.scope, ScanScope::ContentOnly, "Content folder only");
                ui.radio_value(&mut s.scope, ScanScope::All, "Entire disc folder");
                let sel = matches!(s.scope, ScanScope::Selected(_));
                if ui.radio(sel, "Selected folders / archives").clicked() && !sel {
                    s.scope = ScanScope::Selected(vec!["content/Common/Stage".into()]);
                }
            });
            if let ScanScope::Selected(list) = &mut s.scope {
                let mut text = list.join("\n");
                ui.label("One path per line, relative to the disc folder:");
                if ui.add(egui::TextEdit::multiline(&mut text).desired_rows(3)).changed() {
                    *list = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                }
            }
            ui.heading("Resolution policy");
            ui.horizontal(|ui| {
                ui.label("Scale");
                egui::ComboBox::from_id_salt("globalscale").selected_text(scale_text(s.scale)).show_ui(ui, |ui| {
                    for v in std::iter::once(1).chain(SCALES) {
                        ui.selectable_value(&mut s.scale, v, scale_text(v));
                    }
                });
                ui.label("Max side");
                egui::ComboBox::from_id_salt("maxside").selected_text(format!("{} px", s.max_dim)).show_ui(ui, |ui| {
                    for v in [2048u32, 4096, 8192, 16384] {
                        ui.selectable_value(&mut s.max_dim, v, format!("{v} px"));
                    }
                });
            });
            if let Some(w) = runtime_pack::max_side_warning(s.max_dim) {
                ui.label(RichText::new(format!("Runtime pack: {w}.")).color(Color32::from_rgb(230, 160, 80)));
            }
            ui.label(
                RichText::new("A texture whose scale would pass the max side steps down (×32 → ×16 → … → ×2); only a texture that cannot even double stays native. A class below, or a texture in Assets, can set its own scale.")
                    .weak(),
            );
            ui.add(egui::Slider::new(&mut s.min_source_dim, 1..=128).text("keep native below (px)"));
            ui.label("Runtime pack at the current policy:");
            estimate_table(ui, "settings_rt", &self.estimate);
            ui.heading("Models per class");
            let tools = Tools { dir: s.tools_dir.clone() };
            egui::Grid::new("backends").num_columns(3).striped(true).show(ui, |ui| {
                ui.label(RichText::new("Class").strong());
                ui.label(RichText::new("Model").strong());
                ui.label(RichText::new("Scale").strong());
                ui.end_row();
                for c in ALL_CLASSES {
                    ui.label(c.label());
                    let cur = s.backend_for(c);
                    let mut chosen = cur.clone();
                    egui::ComboBox::from_id_salt(format!("cb{c:?}")).width(280.0).selected_text(cur.label()).show_ui(ui, |ui| {
                        for b in upscale::catalogue() {
                            let ok = tools.available(&b);
                            let l = if ok { b.label() } else { format!("{} (not installed)", b.label()) };
                            ui.selectable_value(&mut chosen, b, l);
                        }
                    });
                    if chosen != cur {
                        s.backends.retain(|(x, _)| *x != c);
                        s.backends.push((c, chosen));
                    }
                    let cur_scale = s.scale_choice_for(c);
                    let mut sc = cur_scale;
                    egui::ComboBox::from_id_salt(format!("cs{c:?}")).width(110.0).selected_text(choice_text(cur_scale)).show_ui(ui, |ui| {
                        for ch in [ScaleChoice::Global, ScaleChoice::Native].into_iter().chain(SCALES.map(ScaleChoice::X)) {
                            ui.selectable_value(&mut sc, ch, choice_text(ch));
                        }
                    });
                    if sc != cur_scale {
                        derived::set_class_scale(&mut s.class_scales, c, sc);
                    }
                    ui.end_row();
                }
            });
            ui.label(RichText::new("Normal maps are always resized as vectors and ramps are never resized, whatever is chosen here.").weak());
            ui.heading("GPU");
            ui.horizontal(|ui| {
                ui.label("Device");
                ui.add(egui::DragValue::new(&mut s.gpu).range(-1..=8));
                ui.label("(-1 = CPU; Real-ESRGAN needs a Vulkan GPU and falls back to Lanczos)");
            });
            ui.add(egui::Slider::new(&mut s.tile, 0..=1024).text("tile size (0 = automatic from VRAM)"));
            ui.add(egui::Slider::new(&mut s.batch, 1..=256).text("textures per model run"));
            ui.horizontal(|ui| {
                ui.label("Upscaler tools folder");
                let mut t = s.tools_dir.display().to_string();
                if ui.text_edit_singleline(&mut t).changed() {
                    s.tools_dir = PathBuf::from(t);
                }
                if ui.button("…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        s.tools_dir = d;
                    }
                }
            });
            if s.tools_dir != self.settings.tools_dir {
                self.tools_dirty = true;  // (Upscaling models: another folder's tools)
            }
            for b in [
                BackendChoice::RealEsrgan("realesrgan-x4plus".into()),
                BackendChoice::Waifu2x { model: "models-cunet".into(), noise: 0 },
                BackendChoice::RealCugan { model: "models-se".into(), noise: 0 },
                BackendChoice::RealSr("models-DF2K".into()),
            ] {
                let path = tools.exe(&b);
                ui.label(format!("{}: {}", b.label().split(' ').next().unwrap_or(""), path.map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into())));
            }
            ui.heading("Packages and memory budget");
            let mut mb = (s.archive_growth_max_bytes >> 20) as u32;
            if ui.add(egui::Slider::new(&mut mb, 8..=512).text("max extra texture memory per archive (MiB)")).changed() {
                s.archive_growth_max_bytes = (mb as u64) << 20;
            }
            let mut fm = (s.file_max_bytes >> 20) as u32;
            if ui.add(egui::Slider::new(&mut fm, 16..=128).text("max rebuilt file size (MiB, loader limit 128)")).changed() {
                s.file_max_bytes = (fm as u64) << 20;
            }
            let mut pm = (s.package_max_bytes >> 20) as u32;
            if ui.add(egui::Slider::new(&mut pm, 64..=512).text("max package size (MiB, loader limit 512)")).changed() {
                s.package_max_bytes = (pm as u64) << 20;
            }
            ui.add(egui::Slider::new(&mut s.max_growth_factor, 1.0..=8.0).step_by(0.25).text("max archive size vs original (game heap safety)"));
            ui.label(RichText::new("The game loads archives into heaps sized for the originals. 2× was tested to boot and play; larger values may crash some areas.").weak());
            ui.label("Never rebuild archives containing (one per line; verified fixed-size heaps):");
            let mut ex = s.exclude_files.join("
");
            if ui.add(egui::TextEdit::multiline(&mut ex).desired_rows(4)).changed() {
                s.exclude_files = ex.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            }
            ui.checkbox(&mut s.export_unreviewed, "Export textures that were not reviewed (rejected ones are never exported)");
            ui.horizontal(|ui| {
                ui.label("Package name");
                ui.text_edit_singleline(&mut s.package_name);
            });
            ui.horizontal(|ui| {
                ui.label("Only archives containing (comma separated, empty = all)");
                let mut t = s.only_files.join(",");
                if ui.text_edit_singleline(&mut t).changed() {
                    s.only_files = t.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                }
            });
            ui.checkbox(&mut s.debug_tint, "Diagnostics: tint exported textures magenta (to see what loads in game)");
            if ui.button("Restore defaults").clicked() {
                let keep = (s.disc_root.clone(), s.tools_dir.clone());
                s = Settings::default();
                s.disc_root = keep.0;
                s.tools_dir = keep.1;
            }
        });
        if serde_json::to_string(&s).ok() != serde_json::to_string(&self.settings).ok() {
            // Folders decide no plan: changing only those must not mark the estimates out of date.
            let mut same = s.clone();
            (same.disc_root, same.mods_root, same.game_exe) = (self.settings.disc_root.clone(), self.settings.mods_root.clone(), self.settings.game_exe.clone());
            let affects_plans = serde_json::to_string(&same).ok() != serde_json::to_string(&self.settings).ok();
            self.settings = s;
            self.save_settings_with(affects_plans);
        }
    }

    fn queue(&mut self, ui: &mut egui::Ui) {
        let snap = self.progress.snapshot();
        ui.heading(if snap.running { format!("{} — {}", snap.stage, if snap.paused { "paused" } else { "running" }) } else { "Idle".into() });
        if snap.total > 0 {
            let frac = snap.done as f32 / snap.total as f32;
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            let rate = if snap.elapsed_s > 1.0 { snap.done as f32 / snap.elapsed_s } else { 0.0 };
            let eta = if rate > 0.0 && snap.running { format!(", about {:.0} min left", (snap.total - snap.done) as f32 / rate / 60.0) } else { String::new() };
            ui.label(format!("{}/{} done, {} failed, {:.1} per second{}", snap.done, snap.total, snap.failed, rate, eta));
            ui.label(RichText::new(format!("Current: {}", snap.current)).weak());
        }
        ui.horizontal(|ui| {
            if snap.running {
                if ui.button(if snap.paused { "Resume" } else { "Pause" }).clicked() {
                    self.progress.set_paused(!snap.paused);
                }
                if ui.button("Cancel").clicked() {
                    self.progress.cancel();
                }
            }
            if ui.button("Copy log").clicked() {
                ui.ctx().copy_text(self.log.join("\n"));
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in &self.log {
                let c = if l.contains("FAILED") || l.contains("failed") { Color32::from_rgb(230, 120, 110) } else { Color32::LIGHT_GRAY };
                ui.label(RichText::new(l).monospace().size(12.0).color(c));
            }
        });
    }

    fn export_view(&mut self, ui: &mut egui::Ui) {
        if self.inv.is_none() {
            ui.label("Scan the disc first.");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Two ways to deliver the upscales");
            ui.label(RichText::new("Runtime pack: the recomp swaps textures at draw time").strong());
            ui.label("BC7/BC4/BC5 DDS files keyed by the original texture's contents. No game-heap limit, so the policy's full scale is used. Needs the game's vr.texturePacks setting (on by default).");
            ui.label(RichText::new("Rebuilt archives: content packages with the game's own archives rewritten").strong());
            ui.label("Each texture keeps its original GX2 format (BC1/BC3/BC4/BC5/RGBA8). Packages are split to stay within the mod loader's limits (128 MiB per file, 512 MiB and 4096 files per package).");
            ui.label(RichText::new("Use one or the other. Both replace the same textures in different ways; with both switched on one of them does nothing and its memory is wasted. Installing one kind leaves the other kind's packages alone and warns when both are on.").color(Color32::from_rgb(230, 160, 80)));
            ui.separator();
            self.estimate_controls(ui);
            ui.separator();

            ui.heading("Runtime pack");
            let t = &self.estimate.total;
            ui.label(format!(
                "At the current policy: {} textures get a replacement ({} texels), about {} on disk and {} of texture memory if all were resident.",
                t.replaced,
                texel_text(t.texels),
                mib(t.disk),
                mib(t.vram)
            ));
            if let Some(w) = runtime_pack::vram_warning(t.vram) {
                ui.label(RichText::new(format!("Warning: {w}. The export still runs.")).color(Color32::from_rgb(230, 160, 80)));
            }
            if let Some(w) = runtime_pack::max_side_warning(self.settings.max_dim) {
                ui.label(RichText::new(format!("Warning: {w} (Settings → Max side). The export still runs.")).color(Color32::from_rgb(230, 160, 80)));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("Export runtime pack")).clicked() {
                    self.start_runtime_export();
                }
                if let Some(p) = &self.project {
                    if ui.button("Open runtime folder").clicked() {
                        let _ = open_folder(&p.runtime_dir());
                    }
                }
            });
            if let Some(r) = &self.runtime_report {
                ui.label(format!(
                    "Last runtime export: {} entries in {} package(s), {} on disc, ~{} texture memory; {} native by the policy, {} shared a key, {} skipped, {} failed.",
                    r.entries,
                    r.packages.len(),
                    mib(r.bytes),
                    mib(r.vram_estimate_bytes),
                    r.native,
                    r.shared,
                    r.skipped.len(),
                    r.errors.len()
                ));
                for (id, size, n) in &r.packages {
                    ui.label(format!("  {id}: {n} entries, {}", mib(*size)));
                }
                let mut by_reason: std::collections::BTreeMap<&str, usize> = Default::default();
                for sk in &r.skipped {
                    *by_reason.entry(sk.reason.as_str()).or_default() += 1;
                }
                for (reason, n) in by_reason {
                    ui.label(RichText::new(format!("  {n} skipped: {reason}")).weak());
                }
                for w in &r.warnings {
                    ui.label(RichText::new(w).color(Color32::from_rgb(230, 160, 80)));
                }
                for e in r.errors.iter().take(5) {
                    ui.label(RichText::new(e).color(Color32::from_rgb(230, 160, 80)));
                }
            }
            ui.separator();

            ui.heading("Rebuilt archives");
            let b = self.estimate.archives.clone();
            ui.label(format!("{} archives will be rebuilt; texture data grows by {} in total.", b.archives, mib(b.growth)));
            ui.label(format!("Always-loaded packs (permanent*.pack) grow by {} — this memory is used all the time in game.", mib(b.resident)));
            if b.resident > 256 << 20 {
                ui.label(RichText::new("Warning: the always-loaded packs grow a lot; if the game runs out of memory, lower the per-archive budget or the scale.").color(Color32::from_rgb(230, 160, 80)));
            }
            if let Some((g, f)) = &b.worst {
                ui.label(format!("Largest growth: {f} (+{})", mib(*g)));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("Export rebuilt archives")).clicked() {
                    self.start_pipeline(false, false, false, true);
                }
                if let Some(p) = &self.project {
                    if ui.button("Open packages folder").clicked() {
                        let _ = open_folder(&p.packages_dir());
                    }
                }
            });
            if let Some(r) = &self.last_report {
                ui.label(format!(
                    "Last export: {} textures in {} archives; {} → {} on disc; +{} texture data.",
                    r.textures_replaced,
                    r.files_rebuilt,
                    mib(r.bytes_original),
                    mib(r.bytes_rebuilt),
                    mib(r.guest_growth_bytes)
                ));
                for (id, size, n) in &r.packages {
                    ui.label(format!("  {id}: {n} files, {}", mib(*size)));
                }
                if !r.file_errors.is_empty() {
                    ui.label(RichText::new(format!("{} textures/files were kept original (see Diagnostics)", r.file_errors.len())).color(Color32::from_rgb(230, 160, 80)));
                }
            }
            ui.separator();

            ui.heading("Install into the game");
            match self.env_info.clone() {
                Some(i) => {
                    ui.label(format!("Mods folder: {}", i.mods_path.display()));
                    ui.label(format!(
                        "Found by: {}; {}{}.",
                        i.mods_rule.label(),
                        if i.mods_exists { "the folder exists" } else { "the folder does not exist yet" },
                        if i.has_profiles { ", profiles.json present" } else { ", no profiles.json" }
                    ));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Change the folder or the game executable in Settings.").weak());
                        if ui.small_button("Open Settings").clicked() {
                            self.view = View::Settings;
                        }
                    });
                }
                None => {
                    ui.label("Looking for the mods folder…");
                }
            }
            ui.checkbox(&mut self.install_confirm, "The game is closed; replace earlier Texture Studio packages of that kind there and switch the new ones on");
            let can = self.install_confirm && !self.busy() && self.project.is_some() && self.env_info.is_some();
            let exists = self.env_info.as_ref().is_some_and(|i| i.mods_exists);
            // one row per kind, with what it trades off
            for (kind, label, note) in [
                (InstallKind::Archives, "Install rebuilt archives", "BC1 textures, faster loading, better performance"),
                (InstallKind::Runtime, "Install runtime pack", "BC7 textures, sharper images, higher performance cost"),
            ] {
                ui.horizontal(|ui| {
                    if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                        if exists {
                            self.start_install(kind);
                        } else {
                            self.create_confirm = Some(kind);
                        }
                    }
                    ui.label(RichText::new(note).weak());
                });
            }
            if let (Some(kind), Some(i)) = (self.create_confirm, self.env_info.clone()) {
                ui.label(RichText::new(format!("{} does not exist yet (the game creates it on its first start). Create it and install?", i.mods_path.display())).color(Color32::from_rgb(230, 160, 80)));
                ui.horizontal(|ui| {
                    if ui.add_enabled(can, egui::Button::new("Create the folder and install")).clicked() {
                        self.start_install(kind);
                    }
                    if ui.button("Cancel").clicked() {
                        self.create_confirm = None;
                    }
                });
            }
            for w in &self.install_warnings {
                ui.label(RichText::new(format!("Warning: {w}")).color(Color32::from_rgb(230, 160, 80)));
            }
            ui.label(RichText::new("The game's Mods tab lists the packages; switching them off restores the original textures.").weak());
        });
    }

    fn diagnostics(&mut self, ui: &mut egui::Ui) {
        let Some(inv) = self.inv.clone() else {
            ui.label("Scan the disc first.");
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Formats");
            let mut fm: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
            for &i in &self.unique {
                let t = &inv.textures[i];
                let e = fm.entry(format!("{:?} {}", t.kind, wwhd_tex::gx2::format_name(t.surface.format))).or_default();
                e.0 += 1;
                if t.gate_ok && t.decodable {
                    e.1 += 1;
                }
            }
            egui::Grid::new("fmts").striped(true).show(ui, |ui| {
                ui.label(RichText::new("Container / format").strong());
                ui.label(RichText::new("Unique").strong());
                ui.label(RichText::new("Rewritable").strong());
                ui.end_row();
                for (k, (n, ok)) in fm {
                    ui.label(k);
                    ui.label(n.to_string());
                    ui.label(ok.to_string());
                    ui.end_row();
                }
            });
            ui.heading("Not rewritable (kept original)");
            for &i in &self.unique {
                let t = &inv.textures[i];
                if !t.gate_ok || !t.decodable {
                    ui.label(format!("{} — {}", t.id.key(), self.plans[i].reason));
                }
            }
            ui.heading("Other resources found (not textures)");
            for (k, n) in &inv.other_leaves {
                ui.label(format!("{k}: {n}"));
            }
            ui.heading("Scan errors");
            if inv.errors.is_empty() {
                ui.label("none");
            }
            for e in &inv.errors {
                ui.label(e);
            }
            if let Some(r) = &self.last_report {
                ui.heading("Last export problems");
                if r.file_errors.is_empty() {
                    ui.label("none");
                }
                for e in &r.file_errors {
                    ui.label(e);
                }
                for e in r.textures_skipped.iter().take(200) {
                    ui.label(RichText::new(e).weak());
                }
            }
            if let Some(p) = &self.project {
                if ui.button("Open log folder").clicked() {
                    let _ = open_folder(&p.root.join("logs"));
                }
            }
        });
    }
}

fn open_folder(p: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p)?;
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(not(windows))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd).arg(p).spawn().map(|_| ())
}
