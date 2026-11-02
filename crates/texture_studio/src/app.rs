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
use wwhd_tex::pipeline::{self, ExportReport, UpscaleTask};
use wwhd_tex::policy::{self, Decisions, Plan, Review, ReviewStatus};
use wwhd_tex::project::{Project, ScanScope, Settings};
use wwhd_tex::resample::Filter;
use wwhd_tex::upscale::{self, BackendChoice, Tools};

use crate::images::ImageCache;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
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
    Installed(Vec<String>),
    Done(String),
    Failed(String),
}

pub struct App {
    project: Option<Project>,
    settings: Settings,
    inv: Option<Arc<Inventory>>,
    decisions: Decisions,
    plans: Vec<Plan>,
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
    mods_root: String,
    install_confirm: bool,
    status: String,
    filter_dirty: bool,
    thumb_size: f32,
}

fn default_mods_root() -> String {
    std::env::var("APPDATA").map(|a| format!("{a}\\wwhd\\ModManager")).unwrap_or_else(|_| {
        std::env::var("HOME").map(|h| format!("{h}/.config/wwhd/ModManager")).unwrap_or_default()
    })
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
            mods_root: default_mods_root(),
            install_confirm: false,
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
                    // Resolve relative to the executable's repository layout or the working dir.
                    let exe_rel = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("../../tools/upscalers")));
                    if let Some(c) = exe_rel.filter(|c| c.exists()) {
                        self.settings.tools_dir = c.canonicalize().unwrap_or(c);
                    }
                }
                self.decisions = Decisions::load(&p.review_path(), &p.classes_path());
                self.progress = Progress::with_log_file(p.log_path());
                self.log.clear();
                self.log_seen = 0;
                self.inv = Inventory::load(&p.inventory_path()).ok().map(Arc::new);
                self.last_report = std::fs::read(p.reports_dir().join("export_report.json")).ok().and_then(|d| serde_json::from_slice(&d).ok());
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

    fn save_settings(&mut self) {
        if let Some(p) = &self.project {
            let _ = p.save_settings(&self.settings);
        }
        self.recompute();
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
        let Some(inv) = &self.inv else {
            self.plans.clear();
            self.unique.clear();
            self.filtered.clear();
            return;
        };
        self.plans = policy::plan_all(inv, &self.settings, &self.decisions);
        let mut seen = std::collections::HashSet::new();
        self.unique = (0..inv.textures.len()).filter(|&i| seen.insert(inv.textures[i].key.clone())).collect();
        self.filter_dirty = true;
    }

    fn upscaled_path(&self, i: usize) -> Option<PathBuf> {
        let (p, inv) = (self.project.as_ref()?, self.inv.as_ref()?);
        let pl = &self.plans[i];
        pl.upscales().then(|| p.upscaled_path(&pl.backend.key(), &inv.textures[i].key, pl.scale))
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
                    StatusFilter::Upscaled => self.upscaled_path(i).is_some_and(|p| p.exists()),
                    StatusFilter::NotUpscaled => pl.upscales() && !self.upscaled_path(i).is_some_and(|p| p.exists()),
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
                let plans = policy::plan_all(&inv, &s, &d);
                let tasks = pipeline::upscale_tasks(&inv, &plans, |i| s.file_selected(&inv.textures[i].id.location.file));
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
                let path = p.upscaled_path(&plans[i].backend.key(), &inv.textures[i].key, plans[i].scale);
                let _ = std::fs::remove_file(&path);
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

    fn run_compare(&mut self, i: usize) {
        let (Some(p), Some(inv)) = (self.project.clone(), self.inv.clone()) else { return };
        let s = self.settings.clone();
        let scale = self.plans[i].scale.max(2).min(s.scale.max(2));
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
                Msg::Installed(ids) => self.status = format!("Installed {} package(s): {}. Restart the game.", ids.len(), ids.join(", ")),
                Msg::Done(s) => {
                    self.status = s;
                    self.filter_dirty = true;
                }
                Msg::Failed(e) => self.status = format!("Stopped: {e}"),
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

fn mib(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64)
    }
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.pump(ctx);
        if self.filter_dirty {
            self.apply_filter();
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
                        self.save_settings();
                    }
                }
                ui.label(RichText::new(self.settings.disc_root.display().to_string()).weak());
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
                }
            }
        });

        if self.view == View::Assets {
            egui::Panel::right("detail").resizable(true).default_size(560.0).min_size(380.0).show(root, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.detail(ui));
            });
        }

        egui::CentralPanel::default().show(root, |ui| match self.view {
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

impl App {
    fn dashboard(&mut self, ui: &mut egui::Ui) {
        let Some(inv) = self.inv.clone() else {
            ui.heading("Welcome");
            ui.label("1. Choose a project folder (anywhere with free space; nothing is written to the disc dump).");
            ui.label("2. Choose the Wind Waker HD disc folder (the one containing code/, content/ and meta/).");
            ui.label("3. Press ▶ Process everything, or run the steps one by one and review textures in Assets.");
            return;
        };
        let p = self.project.clone().unwrap();
        let upscaled = self.unique.iter().filter(|&&i| self.upscaled_path(i).is_some_and(|x| x.exists())).count();
        let planned = self.unique.iter().filter(|&&i| self.plans[i].upscales()).count();
        let extracted = self.unique.iter().filter(|&&i| p.master_path(&inv.textures[i].key).exists()).count();
        let problems = self.unique.iter().filter(|&&i| !inv.textures[i].gate_ok || !inv.textures[i].decodable).count();
        egui::Grid::new("dash").num_columns(2).striped(true).spacing([30.0, 6.0]).show(ui, |ui| {
            for (k, v) in [
                ("Disc folder", inv.root.clone()),
                ("Resource files scanned", inv.files.len().to_string()),
                ("Textures found (all copies)", inv.textures.len().to_string()),
                ("Unique textures", self.unique.len().to_string()),
                ("Extracted masters", extracted.to_string()),
                ("Planned for upscaling", planned.to_string()),
                ("Upscaled", format!("{upscaled} / {planned}")),
                ("Unsupported or not rewritable", problems.to_string()),
                ("Scan / archive errors", inv.errors.len().to_string()),
                ("Project disk use (cache + masters + output)", mib(dir_size(&p.root))),
            ] {
                ui.label(k);
                ui.label(RichText::new(v).strong());
                ui.end_row();
            }
        });
        ui.add_space(12.0);
        ui.heading("By class");
        let mut rows: std::collections::BTreeMap<Class, (usize, usize, u64, u64)> = Default::default();
        for &i in &self.unique {
            let t = &inv.textures[i];
            let e = rows.entry(self.decisions.classification(t).class).or_default();
            e.0 += 1;
            if self.plans[i].upscales() {
                e.1 += 1;
            }
            e.2 += policy::surface_bytes(t, 1);
            e.3 += policy::surface_bytes(t, self.plans[i].scale);
        }
        egui::Grid::new("classes").num_columns(6).striped(true).spacing([20.0, 4.0]).show(ui, |ui| {
            for h in ["Class", "Unique", "Upscaled", "Backend", "Size now", "Size after"] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            let (mut a, mut b) = (0, 0);
            for (c, (n, u, x, y)) in rows {
                ui.label(c.label());
                ui.label(n.to_string());
                ui.label(u.to_string());
                ui.label(self.settings.backend_for(c).label());
                ui.label(mib(x));
                ui.label(mib(y));
                a += x;
                b += y;
                ui.end_row();
            }
            ui.label(RichText::new("Total (unique textures)").strong());
            ui.label("");
            ui.label("");
            ui.label("");
            ui.label(mib(a));
            ui.label(mib(b));
            ui.end_row();
        });
    }

    fn assets(&mut self, ui: &mut egui::Ui) {
        let Some(inv) = self.inv.clone() else {
            ui.label("Scan the disc first.");
            return;
        };
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
                            "{}\n{}x{} {}\n{}",
                            t.id.location.display(),
                            t.surface.width,
                            t.surface.height,
                            wwhd_tex::gx2::format_name(t.surface.format),
                            self.plans[i].reason
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
                ("Plan", plan.reason.clone()),
                ("Output", if plan.upscales() { format!("{}×{} via {}", s.width * plan.scale, s.height * plan.scale, plan.backend.label()) } else { "original kept".into() }),
            ] {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            }
        });
        if let Some(up) = self.upscaled_path(i) {
            if let Some(m) = pipeline::load_meta(&up) {
                ui.label(format!(
                    "Made with {} in {:.2} s{}{}",
                    m.used.label(),
                    m.seconds,
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
            for (v, l) in [(0u32, "auto"), (1, "×1"), (2, "×2"), (4, "×4")] {
                ui.selectable_value(&mut sc, v, l);
            }
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
                    show(ui, &b, None, if up.is_some() { "Upscaled" } else { "No upscale planned" });
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
        ui.horizontal_wrapped(|ui| {
            for (b, on) in self.compare_backends.iter_mut() {
                let avail = tools.available(b);
                ui.add_enabled(avail, egui::Checkbox::new(on, b.label())).on_disabled_hover_text("not installed in the tools folder");
            }
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new("Run selected models")).clicked() {
                self.run_compare(i);
            }
            ui.add(egui::Slider::new(&mut self.zoom, 0.25..=8.0).logarithmic(true).text("zoom"));
            ui.label("Click a result to use that model for this texture.");
        });
        ui.separator();
        let scale = self.plans[i].scale.max(2).min(self.settings.scale.max(2));
        let cell = 300.0 * self.zoom;
        let mut entries: Vec<(String, PathBuf, Option<BackendChoice>)> = vec![("Original".into(), p.master_path(&t.key), None)];
        for (b, on) in &self.compare_backends {
            if *on {
                entries.push((b.label(), p.upscaled_path(&b.key(), &t.key, scale), Some(b.clone())));
            }
        }
        let per_row = ((ui.available_width() / (cell + 10.0)).floor() as usize).max(1);
        let mut choose = None;
        egui::ScrollArea::both().show(ui, |ui| {
            for row in entries.chunks(per_row) {
                ui.horizontal(|ui| {
                    for (label, path, b) in row {
                        ui.vertical(|ui| {
                            let meta = pipeline::load_meta(path);
                            let extra = meta.map(|m| format!(" ({:.1}s{})", m.seconds, if m.note.is_empty() { String::new() } else { format!(", {}", m.note) })).unwrap_or_default();
                            ui.label(format!("{label}{extra}"));
                            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(cell), Sense::click());
                            checker(ui, rect);
                            if let Some(tex) = self.images.as_mut().unwrap().get_mapped(path, pipeline::display_map(t)) {
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

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        let mut s = self.settings.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
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
                for v in [2u32, 4] {
                    ui.selectable_value(&mut s.scale, v, format!("×{v}"));
                }
            });
            ui.add(egui::Slider::new(&mut s.max_dim, 512..=8192).step_by(512.0).text("maximum output side (px)"));
            ui.add(egui::Slider::new(&mut s.min_source_dim, 1..=128).text("keep native below (px)"));
            ui.heading("Models per class");
            let tools = Tools { dir: s.tools_dir.clone() };
            egui::Grid::new("backends").num_columns(2).striped(true).show(ui, |ui| {
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
            self.settings = s;
            self.save_settings();
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
        let Some(inv) = self.inv.clone() else {
            ui.label("Scan the disc first.");
            return;
        };
        ui.heading("Targets");
        ui.label(RichText::new("✔ ZeldaWWHD recomp (desktop and PCVR): rebuilt archives as content packages").strong());
        ui.label("Each texture keeps its original GX2 format (BC1/BC3/BC4/BC5/RGBA8): the game's GPU path has no BC7. Packages are split to stay within the mod loader's limits (128 MiB per file, 512 MiB and 4096 files per package).");
        ui.label(RichText::new("✖ Quest 3 / Steam Frame (ASTC) and PCVR BC7: not loadable yet").color(Color32::from_rgb(220, 170, 90)));
        ui.label("These need a runtime texture replacement path in the recomp (see docs/runtime-adapter.md). The lossless masters and upscaled PNGs in the project are the inputs for that later.");
        ui.separator();
        ui.heading("Budget");
        let mut by_file: std::collections::BTreeMap<&str, (u64, u64)> = Default::default();
        for (t, pl) in inv.textures.iter().zip(&self.plans) {
            if pl.upscales() && self.settings.file_selected(&t.id.location.file) {
                let e = by_file.entry(t.id.location.file.as_str()).or_default();
                e.0 += policy::surface_bytes(t, 1);
                e.1 += policy::surface_bytes(t, pl.scale);
            }
        }
        let growth: u64 = by_file.values().map(|(a, b)| b - a).sum();
        let resident: u64 = by_file.iter().filter(|(f, _)| f.contains("permanent")).map(|(_, (a, b))| b - a).sum();
        ui.label(format!("{} archives will be rebuilt; texture data grows by {} in total.", by_file.len(), mib(growth)));
        ui.label(format!("Always-loaded packs (permanent*.pack) grow by {} — this memory is used all the time in game.", mib(resident)));
        if resident > 256 << 20 {
            ui.label(RichText::new("Warning: the always-loaded packs grow a lot; if the game runs out of memory, lower the per-archive budget or the scale.").color(Color32::from_rgb(230, 160, 80)));
        }
        let worst = by_file.iter().map(|(f, (a, b))| (b - a, *f)).max();
        if let Some((g, f)) = worst {
            ui.label(format!("Largest growth: {f} (+{})", mib(g)));
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new("Validate and export packages")).clicked() {
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
        ui.horizontal(|ui| {
            ui.label("ModManager folder");
            ui.text_edit_singleline(&mut self.mods_root);
            if ui.button("…").clicked() {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    self.mods_root = d.display().to_string();
                }
            }
        });
        ui.checkbox(&mut self.install_confirm, "The game is closed; replace earlier Texture Studio packages there and switch the new ones on");
        if ui.add_enabled(self.install_confirm && !self.busy() && self.project.is_some(), egui::Button::new("Install packages")).clicked() {
            let p = self.project.clone().unwrap();
            let root = PathBuf::from(&self.mods_root);
            self.install_confirm = false;
            self.spawn("Installing", move |_, tx| {
                let ids = crate::cli::install(&p, &root, true)?;
                let _ = tx.send(Msg::Installed(ids.clone()));
                Ok(format!("Installed {} package(s); start the game to use them", ids.len()))
            });
        }
        ui.label(RichText::new("The game's Mods tab lists the packages as content mods; switching them off restores the original textures.").weak());
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
