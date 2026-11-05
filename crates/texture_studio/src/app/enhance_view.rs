//! The Enhance tab: processing profiles (choose, edit, duplicate, save, load, reset), per-texture profile, and a
//! live preview of the selected texture that is computed on a worker thread from the cached upscale. Nothing is
//! re-upscaled and nothing on disk changes until an export.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions};

use wwhd_tex::enhance::{self, EnhanceSettings, Profile, SharpenMode, Target, HUE_BANDS};
use wwhd_tex::pipeline;
use wwhd_tex::project::load_png;
use wwhd_tex::runtime_pack::{self, PackFormat};
use wwhd_tex::upscale::{self, BackendChoice, Tools};

use super::App;
use crate::viewer::{self, Layout, Pane, ViewState};

/// What one preview image is.
#[derive(Clone)]
struct Req {
    id: String,
    png: PathBuf,
    master: PathBuf,
    /// `None`: the upscale as it is.
    profile: Option<Profile>,
    settings: EnhanceSettings,
    target: Target,
    /// Also encode + decode in the export format.
    encode: Option<PackFormat>,
}

type Done = (String, Result<(ColorImage, String), String>);

/// Worker + texture cache for previews. The worker skips requests no longer on screen (a slider drag queues many).
pub(super) struct Previews {
    tx: Sender<Req>,
    rx: Receiver<Done>,
    wanted: Arc<Mutex<HashSet<String>>>,
    tex: HashMap<String, (TextureHandle, String)>,
    failed: HashMap<String, String>,
    pending: HashSet<String>,
    order: VecDeque<String>,
}

impl Previews {
    pub(super) fn new(ctx: &egui::Context) -> Previews {
        let (tx, req_rx) = channel::<Req>();
        let (res_tx, rx) = channel::<Done>();
        let wanted: Arc<Mutex<HashSet<String>>> = Arc::default();
        let w = wanted.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(first) = req_rx.recv() {
                // Newest first: drain what is queued, keep only what is still on screen.
                let mut batch = vec![first];
                batch.extend(req_rx.try_iter());
                batch.reverse();
                for r in batch {
                    if !w.lock().unwrap().contains(&r.id) {
                        let _ = res_tx.send((r.id.clone(), Err(String::new())));
                        continue;
                    }
                    let out = render(&r);
                    if res_tx.send((r.id, out)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        });
        Previews { tx, rx, wanted, tex: HashMap::new(), failed: HashMap::new(), pending: HashSet::new(), order: VecDeque::new() }
    }

    pub(super) fn pump(&mut self, ctx: &egui::Context) {
        while let Ok((id, r)) = self.rx.try_recv() {
            self.pending.remove(&id);
            match r {
                Ok((img, info)) => {
                    let opts = TextureOptions { magnification: egui::TextureFilter::Nearest, minification: egui::TextureFilter::Linear, mipmap_mode: Some(egui::TextureFilter::Linear), ..TextureOptions::LINEAR };
                    let h = ctx.load_texture(&id, img, opts);
                    self.tex.insert(id.clone(), (h, info));
                    self.order.push_back(id);
                    while self.order.len() > 24 {
                        if let Some(old) = self.order.pop_front() {
                            self.tex.remove(&old);
                        }
                    }
                }
                Err(e) if e.is_empty() => {} // skipped: no longer wanted
                Err(e) => {
                    self.failed.insert(id, e);
                }
            }
        }
    }

    /// Start a frame: the set of wanted ids is rebuilt by the `get` calls that follow.
    fn begin(&mut self) {
        self.wanted.lock().unwrap().clear();
    }

    fn get(&mut self, r: Req) -> Result<Option<(egui::TextureId, String)>, String> {
        self.wanted.lock().unwrap().insert(r.id.clone());
        if let Some((t, info)) = self.tex.get(&r.id) {
            return Ok(Some((t.id(), info.clone())));
        }
        if let Some(e) = self.failed.get(&r.id) {
            return Err(e.clone());
        }
        if self.pending.insert(r.id.clone()) {
            let _ = self.tx.send(r);
        }
        Ok(None)
    }
}

fn render(r: &Req) -> Result<(ColorImage, String), String> {
    let (w, h, raw) = load_png(&r.png).map_err(|e| e.to_string())?;
    let (mut img, mut info) = (raw.clone(), String::new());
    if let Some(p) = &r.profile {
        match enhance::resolve(p, &r.settings, &r.target, &raw) {
            Some(res) => {
                let original = res.detail.filter(|d| d.restore > 0.0).and_then(|_| load_png(&r.master).ok());
                let (out, st) = enhance::apply(w, h, &raw, &res, original.as_ref().map(|(a, b, c)| (*a, *b, &c[..])));
                img = out;
                info = format!("{} · {} · {:.0} ms · adaptive ×{:.2}", p.name, res.category.label(), st.millis, st.busyness_factor);
            }
            None => info = format!("{}: not enhanced ({})", p.name, enhance::category(r.target.class, r.target.format, &raw).label()),
        }
    }
    if let Some(fmt) = r.encode {
        let started = std::time::Instant::now();
        img = runtime_pack::encode_preview(fmt, w, h, &img).map_err(|e| e.to_string())?;
        info = format!("{info} · encoded {fmt:?} in {:.0} ms", started.elapsed().as_secs_f32() * 1000.0);
    }
    Ok((ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &img), info))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Split,
    SideBySide,
    Variants,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Source {
    Original,
    Upscaled,
    Enhanced,
    Encoded,
}

impl Source {
    const ALL: [Source; 4] = [Source::Original, Source::Upscaled, Source::Enhanced, Source::Encoded];
    fn label(self) -> &'static str {
        match self {
            Source::Original => "Original (source texels)",
            Source::Upscaled => "AI upscale (no enhancement)",
            Source::Enhanced => "Enhanced (active profile)",
            Source::Encoded => "Enhanced, as exported (encoded)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum VariantSet {
    Stages,
    Profiles,
    Sharpen,
    Colour,
    Models,
}

impl VariantSet {
    const ALL: [VariantSet; 5] = [VariantSet::Stages, VariantSet::Profiles, VariantSet::Sharpen, VariantSet::Colour, VariantSet::Models];
    fn label(self) -> &'static str {
        match self {
            VariantSet::Stages => "Stages (upscale / +sharpen / +colour / all)",
            VariantSet::Profiles => "Profiles",
            VariantSet::Sharpen => "Sharpening strength",
            VariantSet::Colour => "Colour strength",
            VariantSet::Models => "Models (active profile on each)",
        }
    }
}

pub(super) struct EnhanceUi {
    pub(super) previews: Previews,
    view: ViewState,
    mode: Mode,
    left: Source,
    right: Source,
    variants: VariantSet,
    /// The profile shown in the editor.
    edit: String,
    message: String,
}

impl EnhanceUi {
    pub(super) fn new(ctx: &egui::Context) -> EnhanceUi {
        EnhanceUi {
            previews: Previews::new(ctx),
            view: ViewState::default(),
            mode: Mode::Split,
            left: Source::Upscaled,
            right: Source::Enhanced,
            variants: VariantSet::Stages,
            edit: enhance::DEFAULT_PROFILE.into(),
            message: String::new(),
        }
    }
}

impl EnhanceUi {
    /// Diagnostics (`WWHD_STUDIO_ENH`): `split`, `side`, or `variants:<stages|profiles|sharpen|colour|models>`,
    /// optionally followed by `,100` for 1:1 pixels and `,left=<original|upscaled|enhanced|encoded>` etc.
    pub(super) fn automate(&mut self, spec: &str, ppp: f32) {
        for part in spec.split(',') {
            let src = |v: &str| match v {
                "original" => Source::Original,
                "enhanced" => Source::Enhanced,
                "encoded" => Source::Encoded,
                _ => Source::Upscaled,
            };
            match part {
                "split" => self.mode = Mode::Split,
                "side" => self.mode = Mode::SideBySide,
                "100" => self.view.actual_pixels(ppp),
                p if p.starts_with("zoom=") => {
                    self.view.fit = false;
                    self.view.zoom = p[5..].parse().unwrap_or(1.0);
                }
                p if p.starts_with("center=") => {
                    let v: Vec<f32> = p[7..].split(':').filter_map(|x| x.parse().ok()).collect();
                    if v.len() == 2 {
                        self.view.center = egui::vec2(v[0], v[1]);
                    }
                }
                p if p.starts_with("left=") => self.left = src(&p[5..]),
                p if p.starts_with("right=") => self.right = src(&p[6..]),
                p if p.starts_with("edit=") => self.edit = p[5..].to_string(),
                p if p.starts_with("variants") => {
                    self.mode = Mode::Variants;
                    self.variants = match p.split(':').nth(1).unwrap_or("") {
                        "profiles" => VariantSet::Profiles,
                        "sharpen" => VariantSet::Sharpen,
                        "colour" => VariantSet::Colour,
                        "models" => VariantSet::Models,
                        _ => VariantSet::Stages,
                    };
                }
                _ => {}
            }
        }
    }
}

fn slider(ui: &mut egui::Ui, v: &mut f32, range: std::ops::RangeInclusive<f32>, text: &str, hover: &str) -> bool {
    ui.add(egui::Slider::new(v, range).text(text)).on_hover_text(hover).changed()
}

fn pct(ui: &mut egui::Ui, v: &mut f32, range: std::ops::RangeInclusive<f32>, text: &str, hover: &str) -> bool {
    let mut p = *v * 100.0;
    let r = ui.add(egui::Slider::new(&mut p, (*range.start() * 100.0)..=(*range.end() * 100.0)).suffix(" %").text(text)).on_hover_text(hover).changed();
    if r {
        *v = p / 100.0;
    }
    r
}

/// Every parameter of a profile; returns whether something changed.
fn profile_editor(ui: &mut egui::Ui, p: &mut Profile) -> bool {
    let mut ch = false;
    ui.label(RichText::new("Sharpening").strong());
    ch |= ui.checkbox(&mut p.sharpen.enabled, "Sharpen").changed();
    ui.add_enabled_ui(p.sharpen.enabled, |ui| {
        ui.horizontal(|ui| {
            ch |= ui.selectable_value(&mut p.sharpen.mode, SharpenMode::UnsharpMask, "Unsharp mask").changed();
            ch |= ui.selectable_value(&mut p.sharpen.mode, SharpenMode::HighPass, "High-pass (mid-tones)").changed();
        });
        ch |= slider(ui, &mut p.sharpen.radius, 0.2..=3.0, "radius", "Gaussian radius; see the unit below");
        ch |= ui.checkbox(&mut p.sharpen.radius_in_source, "radius in source texels").on_hover_text("On: the radius counts original texels (×4 upscale → 4× the pixels), the size of the painted detail, which survives mips. Off: output pixels (very fine)").changed();
        ch |= pct(ui, &mut p.sharpen.amount, 0.0..=2.0, "amount", "Strength");
        ch |= slider(ui, &mut p.sharpen.threshold, 0.0..=10.0, "threshold", "Differences smaller than this many 8-bit levels are left alone, so flat cel shading stays flat");
        ch |= ui.checkbox(&mut p.sharpen.edge_protection, "Edge protection").on_hover_text("Less sharpening on edges that are already strong (outlines)").changed();
        ch |= ui.checkbox(&mut p.sharpen.halo_suppression, "Halo suppression").on_hover_text("Every texel stays within its neighbourhood's original range: no rings or rims").changed();
        ui.add_enabled_ui(p.sharpen.halo_suppression, |ui| {
            ch |= slider(ui, &mut p.sharpen.halo_margin, 0.0..=12.0, "halo margin (levels)", "How far a texel may go past its neighbourhood's range. About 1 = no rims; a few levels add crispness once edges are fully steepened; much more brings rims back");
        });
        ch |= ui.checkbox(&mut p.sharpen.adaptive, "Adaptive to texture").on_hover_text("Measured per texture: busy, noisy art is sharpened less, clean art a little more").changed();
    });
    ui.separator();
    ui.label(RichText::new("Detail recovery").strong());
    ch |= ui.checkbox(&mut p.detail.enabled, "Recover detail").changed();
    ui.add_enabled_ui(p.detail.enabled, |ui| {
        ch |= pct(ui, &mut p.detail.amount, 0.0..=1.5, "local contrast", "Edge-preserving lift of painted texture the upscaler smoothed; outlines are left alone");
        ch |= slider(ui, &mut p.detail.radius, 0.25..=3.0, "size (source texels)", "Size of the detail lifted");
        ch |= slider(ui, &mut p.detail.edge_level, 0.01..=0.2, "edge level", "Local contrast above this counts as an edge and is not lifted");
        ch |= pct(ui, &mut p.detail.restore, 0.0..=1.0, "texture restoration", "Re-inject the original's own texture where the AI flattened it. Brings back the original's compression noise too: keep low");
    });
    ui.separator();
    ui.label(RichText::new("Colour").strong());
    ch |= ui.checkbox(&mut p.colour.enabled, "Adjust colour").changed();
    ui.add_enabled_ui(p.colour.enabled, |ui| {
        let c = &mut p.colour;
        ch |= pct(ui, &mut c.vibrance, -0.5..=0.5, "vibrance", "Boosts muted colours, leaves saturated ones");
        ch |= pct(ui, &mut c.saturation, -0.5..=0.5, "saturation", "All colours");
        ch |= pct(ui, &mut c.contrast, -0.3..=0.3, "contrast", "Around the texture's mean lightness");
        ch |= slider(ui, &mut c.gamma, 0.5..=2.0, "gamma", "Above 1 brightens mid-tones");
        ch |= slider(ui, &mut c.black_point, 0.0..=0.3, "black point", "Lightness mapped to black");
        ch |= slider(ui, &mut c.white_point, 0.7..=1.0, "white point", "Lightness mapped to white");
        ch |= slider(ui, &mut c.highlights, -1.0..=1.0, "highlights", "Lift (+) or deepen (−) the bright range");
        ch |= slider(ui, &mut c.shadows, -1.0..=1.0, "shadows", "Lift (+) or deepen (−) the dark range");
        ch |= slider(ui, &mut c.temperature, -1.0..=1.0, "temperature", "Cooler (−) / warmer (+)");
        egui::CollapsingHeader::new("Selective colour").show(ui, |ui| {
            for ((name, _), v) in HUE_BANDS.iter().zip(c.selective.iter_mut()) {
                ch |= pct(ui, v, -0.5..=0.5, name, "Extra chroma for this hue");
            }
        });
        ch |= ui.checkbox(&mut c.protect_highlights, "Highlight clipping protection").changed();
        ch |= ui.checkbox(&mut c.protect_shadows, "Shadow detail protection").changed();
    });
    ui.separator();
    ch |= ui.checkbox(&mut p.per_class, "Tune per texture class").on_hover_text("Strengths scaled per class: e.g. water and sky sharpened less, stone more, UI colours never changed. Low-confidence classes get 60 %").changed();
    ch |= ui.checkbox(&mut p.sharp_mips, "Sharper mips (Catmull-Rom)").on_hover_text("Distant mip levels reduced with Catmull-Rom instead of a 2×2 box: crisper at a distance").changed();
    ch
}

impl App {
    pub(super) fn enhance_view(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.enh.previews.pump(&ctx);
        self.enh.previews.begin();
        egui::Panel::left("enh_settings").resizable(true).default_size(380.0).min_size(320.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.enhance_settings(ui));
        });
        egui::CentralPanel::default().show(ui, |ui| self.enhance_preview(ui));
    }

    fn enhance_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Enhancement");
        ui.label(RichText::new("Runs on the cached upscale when textures are exported: changing it re-encodes, it never re-upscales. Normal maps, masks, ramps, one/two-channel formats and Unknown-class textures are never touched; alpha is never changed.").weak());
        let mut s = self.settings.enhance.clone();
        let mut changed = ui.checkbox(&mut s.enabled, "Enhance textures on export").changed();
        ui.horizontal(|ui| {
            ui.label("Project profile:");
            egui::ComboBox::from_id_salt("enh_profile").selected_text(&s.profile).show_ui(ui, |ui| {
                for n in s.names() {
                    changed |= ui.selectable_value(&mut s.profile, n.clone(), n).changed();
                }
            });
        });
        changed |= ui
            .checkbox(&mut s.fill_transparent, "Fill colour under transparent texels (cut-outs)")
            .on_hover_text("Replaces the hidden colour of fully transparent texels in cut-out textures with the nearest visible colour, so filtering cannot pull dark or bright fringes into edges. Off by default: where alpha is not coverage, the hidden colour is visible.")
            .changed();
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Edit:");
            egui::ComboBox::from_id_salt("enh_edit").selected_text(&self.enh.edit).show_ui(ui, |ui| {
                for n in s.names() {
                    ui.selectable_value(&mut self.enh.edit, n.clone(), n);
                }
            });
        });
        let builtin = enhance::is_builtin(&self.enh.edit);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Duplicate").on_hover_text("Copy this profile into a new custom one you can edit").clicked() {
                let mut p = s.find(&self.enh.edit);
                p.name = s.unique_name(&format!("{} copy", p.name.replace("Wind Waker ", "")));
                self.enh.edit = p.name.clone();
                s.custom.push(p);
                changed = true;
            }
            if ui.add_enabled(!builtin, egui::Button::new("Reset to Enhanced")).on_hover_text("Every value back to the Wind Waker Enhanced defaults (keeps the name)").clicked() {
                if let Some(p) = s.custom.iter_mut().find(|p| p.name == self.enh.edit) {
                    *p = Profile { name: p.name.clone(), ..enhance::enhanced() };
                    changed = true;
                }
            }
            if ui.add_enabled(!builtin, egui::Button::new("Delete")).clicked() {
                s.custom.retain(|p| p.name != self.enh.edit);
                if s.profile == self.enh.edit {
                    s.profile = enhance::DEFAULT_PROFILE.into();
                }
                self.enh.edit = s.profile.clone();
                changed = true;
            }
            if ui.button("Save to file…").clicked() {
                let p = s.find(&self.enh.edit);
                if let Some(f) = rfd::FileDialog::new().set_file_name(format!("{}.json", p.name)).add_filter("profile", &["json"]).save_file() {
                    self.enh.message = match serde_json::to_vec_pretty(&p).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&f, b).map_err(|e| e.to_string())) {
                        Ok(()) => format!("Saved {}", f.display()),
                        Err(e) => format!("Could not save: {e}"),
                    };
                }
            }
            if ui.button("Load from file…").clicked() {
                if let Some(f) = rfd::FileDialog::new().add_filter("profile", &["json"]).pick_file() {
                    match std::fs::read(&f).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Profile>(&b).map_err(|e| e.to_string())) {
                        Ok(mut p) => {
                            p.name = s.unique_name(if enhance::is_builtin(&p.name) { "Loaded" } else { &p.name });
                            self.enh.edit = p.name.clone();
                            self.enh.message = format!("Loaded as \"{}\"", p.name);
                            s.custom.push(p);
                            changed = true;
                        }
                        Err(e) => self.enh.message = format!("Not a profile: {e}"),
                    }
                }
            }
        });
        if !self.enh.message.is_empty() {
            ui.label(RichText::new(&self.enh.message).weak());
        }
        if builtin {
            ui.label(RichText::new("Built-in profiles are fixed: Duplicate to make an editable copy (Custom).").color(Color32::from_rgb(220, 190, 120)));
            let mut p = s.find(&self.enh.edit);
            ui.add_enabled_ui(false, |ui| profile_editor(ui, &mut p));
        } else if let Some(p) = s.custom.iter_mut().find(|p| p.name == self.enh.edit) {
            let mut name = p.name.clone();
            ui.horizontal(|ui| {
                ui.label("Name:");
                if ui.text_edit_singleline(&mut name).lost_focus() && name.trim() != p.name && !name.trim().is_empty() && !enhance::is_builtin(name.trim()) {
                    let old = std::mem::replace(&mut p.name, name.trim().to_string());
                    self.enh.edit = p.name.clone();
                    if s.profile == old {
                        s.profile = p.name.clone();
                    }
                    changed = true;
                }
            });
            changed |= profile_editor(ui, p);
        }
        ui.separator();
        egui::CollapsingHeader::new("Per-class tuning (strength multipliers)").show(ui, |ui| {
            egui::Grid::new("tuning").striped(true).show(ui, |ui| {
                ui.label("Category");
                ui.label("Sharpen");
                ui.label("Detail");
                ui.label("Colour");
                ui.end_row();
                for c in [
                    enhance::Category::Character,
                    enhance::Category::Environment,
                    enhance::Category::Vegetation,
                    enhance::Category::Water,
                    enhance::Category::Architecture,
                    enhance::Category::Sky,
                    enhance::Category::UiIcons,
                    enhance::Category::Effects,
                    enhance::Category::Transparent,
                    enhance::Category::Unknown,
                    enhance::Category::Data,
                ] {
                    let t = enhance::class_tuning(c);
                    ui.label(c.label());
                    ui.label(format!("×{:.2}", t.sharpen));
                    ui.label(format!("×{:.2}", t.detail));
                    ui.label(format!("×{:.2}", t.colour));
                    ui.end_row();
                }
            });
            ui.label(RichText::new(format!("Classes below {:.0} % confidence get ×{} on top (not when set by hand).", enhance::LOW_CONFIDENCE * 100.0, enhance::LOW_CONFIDENCE_FACTOR)).weak());
        });
        if changed {
            self.settings.enhance = s;
            self.save_settings_with(false);
        }
    }

    fn enhance_preview(&mut self, ui: &mut egui::Ui) {
        let (Some(inv), Some(p)) = (self.inv.clone(), self.project.clone()) else {
            ui.label("Open a project and scan the disc first.");
            return;
        };
        let Some(i) = self.selected else {
            ui.label("Select a texture in Assets to preview the enhancement on it.");
            return;
        };
        let t = &inv.textures[i];
        let cl = self.decisions.classification(t);
        let plan = self.plans.get(i).cloned();
        ui.horizontal(|ui| {
            ui.heading(&t.id.name);
            ui.label(RichText::new(format!("{} ({:.0} %)", cl.class.label(), cl.confidence * 100.0)).weak());
        });
        // The upscale to preview: the planned one, else any cached ×4 / ×2.
        let backend = plan.as_ref().map(|pl| pl.backend.clone()).filter(|b| *b != BackendChoice::Native).unwrap_or_else(|| self.settings.default_backend.clone());
        let scale = plan.as_ref().map(|pl| pl.scale).filter(|s| *s > 1).unwrap_or(self.settings.scale.max(2));
        let found = pipeline::find_upscaled(&p, &backend.key(), &t.key, scale);
        let Some((png, achieved)) = found else {
            ui.label(format!("No {} ×{scale} upscale of this texture yet: run Upscale (or Reprocess in Assets) first.", backend.label()));
            return;
        };
        let master = p.master_path(&t.key);

        // Per-texture profile.
        let mut review = self.decisions.review(&t.key);
        let mut own = review.profile.clone();
        ui.horizontal(|ui| {
            ui.label("Profile for this texture:");
            let shown = own.clone().unwrap_or_else(|| format!("project ({})", self.settings.enhance.profile));
            egui::ComboBox::from_id_salt("tex_profile").selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut own, None, "follow the project");
                for n in self.settings.enhance.names() {
                    ui.selectable_value(&mut own, Some(n.clone()), n);
                }
            });
            if own.is_some() && ui.small_button("reset").on_hover_text("Follow the project's profile again").clicked() {
                own = None;
            }
        });
        if own != review.profile {
            review.profile = own;
            self.decisions.reviews.insert(t.key.clone(), review.clone());
            self.save_decisions();
        }
        let tools = Tools { dir: self.settings.tools_dir.clone() };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Suggested models for this class:").weak());
            for b in upscale::recommended(cl.class) {
                let avail = tools.available(&b);
                let text = if avail { b.label() } else { format!("{} (not installed)", b.label()) };
                if ui.add_enabled(avail && !self.busy(), egui::Button::new(text).small()).on_hover_text("Reprocess this texture with that model").clicked() {
                    self.reprocess(vec![i], b);
                }
            }
        });

        let profile = self.settings.enhance.profile_for(review.profile.as_deref());
        let ai = pipeline::load_meta(&png).map(|m| m.used.is_ai()).unwrap_or(false);
        let target = Target { class: cl.class, confidence: cl.confidence, manual: cl.manual, format: t.surface.format, scale: achieved, edge: pipeline::edge_of(t), ai };
        let fmt = runtime_pack::pack_format(t.surface.format, cl.class);
        let settings = self.settings.enhance.clone();
        let mk = |prof: Option<&Profile>, encode: bool, png: &PathBuf| -> Req {
            let tag = prof.map(|pr| enhance::cache_tag(pr, &settings, &target)).unwrap_or_default();
            let id = format!("{}|{}|{}|{}", png.display(), achieved, {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                tag.hash(&mut h);
                h.finish()
            }, if encode { "enc" } else { "" });
            Req { id, png: png.clone(), master: master.clone(), profile: prof.cloned(), settings: settings.clone(), target, encode: if encode { fmt } else { None } }
        };

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.enh.mode, Mode::Split, "Split");
            ui.selectable_value(&mut self.enh.mode, Mode::SideBySide, "Side by side");
            ui.selectable_value(&mut self.enh.mode, Mode::Variants, "Variants");
            ui.separator();
            if ui.button("Fit").on_hover_text("Fit to window (or double-click the image)").clicked() {
                self.enh.view.fit = true;
            }
            if ui.button("100 %").on_hover_text("1 texel of the target resolution = 1 screen pixel").clicked() {
                self.enh.view.actual_pixels(ui.ctx().pixels_per_point());
            }
            if ui.button("Reset view").clicked() {
                self.enh.view.reset();
            }
            ui.label(RichText::new(format!("target {}×{} (×{achieved}) · wheel zooms, drag pans, drag the yellow divider", t.surface.width * achieved, t.surface.height * achieved)).weak());
        });
        let get_source = |this: &mut Self, s: Source| -> (Option<egui::TextureId>, String, String) {
            match s {
                Source::Original => (this.images.as_mut().unwrap().get_mapped(&master, pipeline::display_map(t)).map(|h| h.id()), String::new(), "loading…".into()),
                Source::Upscaled => match this.enh.previews.get(mk(None, false, &png)) {
                    Ok(v) => (v.map(|x| x.0), String::new(), "loading…".into()),
                    Err(e) => (None, String::new(), e),
                },
                Source::Enhanced | Source::Encoded => match this.enh.previews.get(mk(Some(&profile), s == Source::Encoded, &png)) {
                    Ok(Some((id, info))) => (Some(id), info, String::new()),
                    Ok(None) => (None, String::new(), if s == Source::Encoded { "encoding (BC7 takes a few seconds)…".into() } else { "enhancing…".into() }),
                    Err(e) => (None, String::new(), e),
                },
            }
        };
        let dims = [t.surface.width * achieved, t.surface.height * achieved];
        let height = (ui.available_height() - 40.0).max(240.0);
        match self.enh.mode {
            Mode::Split | Mode::SideBySide => {
                ui.horizontal(|ui| {
                    for (k, side) in [(0, "Left"), (1, "Right")] {
                        let cur = if k == 0 { &mut self.enh.left } else { &mut self.enh.right };
                        ui.label(side);
                        egui::ComboBox::from_id_salt(("enh_src", k)).selected_text(cur.label()).show_ui(ui, |ui| {
                            for s in Source::ALL {
                                ui.selectable_value(cur, s, s.label());
                            }
                        });
                    }
                    if ui.button("⇄").on_hover_text("Swap").clicked() {
                        std::mem::swap(&mut self.enh.left, &mut self.enh.right);
                    }
                });
                let (l, r) = (self.enh.left, self.enh.right);
                let a = get_source(self, l);
                let b = get_source(self, r);
                let info = [a.1.clone(), b.1.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  |  ");
                let panes = [Pane { label: l.label(), tex: a.0, waiting: &a.2 }, Pane { label: r.label(), tex: b.0, waiting: &b.2 }];
                let layout = if self.enh.mode == Mode::Split { Layout::Split } else { Layout::Grid(2) };
                viewer::show(ui, &mut self.enh.view, dims, &panes, layout, height);
                ui.label(RichText::new(info).weak());
            }
            Mode::Variants => {
                ui.horizontal(|ui| {
                    ui.label("Compare:");
                    egui::ComboBox::from_id_salt("enh_var").selected_text(self.enh.variants.label()).show_ui(ui, |ui| {
                        for v in VariantSet::ALL {
                            ui.selectable_value(&mut self.enh.variants, v, v.label());
                        }
                    });
                });
                let base = profile.clone();
                let list: Vec<(String, Req)> = match self.enh.variants {
                    VariantSet::Stages => vec![
                        ("AI upscale".into(), mk(None, false, &png)),
                        ("+ sharpen".into(), mk(Some(&enhance::only(&base, true, false, false)), false, &png)),
                        ("+ colour".into(), mk(Some(&enhance::only(&base, false, false, true)), false, &png)),
                        ("+ sharpen + colour".into(), mk(Some(&enhance::only(&base, true, false, true)), false, &png)),
                        (format!("all ({})", base.name), mk(Some(&base), false, &png)),
                        ("as exported".into(), mk(Some(&base), true, &png)),
                    ],
                    VariantSet::Profiles => std::iter::once(("AI upscale".to_string(), mk(None, false, &png)))
                        .chain(self.settings.enhance.names().into_iter().map(|n| (n.clone(), mk(Some(&self.settings.enhance.find(&n)), false, &png))))
                        .collect(),
                    VariantSet::Sharpen => [0.0, 0.5, 1.0, 1.5, 2.0].iter().map(|k| (format!("sharpen ×{k}"), mk(Some(&enhance::with_sharpen_strength(&base, *k)), false, &png))).collect(),
                    VariantSet::Colour => [0.0, 0.5, 1.0, 1.5, 2.0].iter().map(|k| (format!("colour ×{k}"), mk(Some(&enhance::with_colour_strength(&base, *k)), false, &png))).collect(),
                    VariantSet::Models => upscale::catalogue_with(&tools)
                        .into_iter()
                        .filter(|b| b.is_ai())
                        .filter_map(|b| pipeline::find_upscaled(&p, &b.key(), &t.key, scale).filter(|(_, a)| *a == achieved).map(|(path, _)| (b.label(), mk(Some(&base), false, &path))))
                        .collect(),
                };
                if list.is_empty() {
                    ui.label("No other model has an upscale of this texture at this scale: run them in Compare models first.");
                    return;
                }
                let mut texs = Vec::new();
                for (label, req) in &list {
                    let (tex, wait) = match self.enh.previews.get(req.clone()) {
                        Ok(Some((id, _))) => (Some(id), String::new()),
                        Ok(None) => (None, "working…".to_string()),
                        Err(e) => (None, e),
                    };
                    texs.push((label.clone(), tex, wait));
                }
                let panes: Vec<Pane> = texs.iter().map(|(l, t, w)| Pane { label: l, tex: *t, waiting: w }).collect();
                let cols = if panes.len() <= 4 { 2 } else { 3 };
                viewer::show(ui, &mut self.enh.view, dims, &panes, Layout::Grid(cols), height);
            }
        }
    }
}
