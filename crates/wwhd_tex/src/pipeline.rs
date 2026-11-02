//! The stages: extract masters, upscale, export rebuilt archives as recomp
//! content packages. Every stage is resumable: finished outputs are found by
//! content-addressed paths that include the backend key and scale, and are
//! written atomically.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::bfres;
use crate::bflim::Bflim;
use crate::classify::{self, Class};
use crate::container::{self, LeafKind};
use crate::error::{bail, Error, Result};
use crate::gx2;
use crate::inventory::{Inventory, TextureRecord};
use crate::jobs::Progress;
use crate::mips::{self, MipMode};
use crate::policy::{self, Decisions, Plan, ReviewStatus};
use crate::project::{load_png, save_png, write_atomic, Project, Settings};
use crate::resample::{self, Edge, Filter};
use crate::texture::{self, TextureInfo};
use crate::upscale::{self, BackendChoice, Tools};

pub const THUMB: u32 = 96;

pub(crate) fn info_of(t: &TextureRecord) -> TextureInfo {
    TextureInfo {
        id: t.id.clone(),
        kind: t.kind,
        surface: t.surface.clone(),
        comp_sel: t.comp_sel,
        regs: [0; 5],
        data_hash: t.data_hash.clone(),
    }
}

pub fn thumb_path(p: &Project, key: &str) -> PathBuf {
    p.root.join("masters/thumbs2").join(&key[..2]).join(format!("{key}.png"))
}

/// How to show a texture's stored channels: per output channel, the source
/// channel 0-3, or 4 = zero, 5 = one. BFRES carries the GX2 component
/// selector; BFLIM one/two-channel images are luminance (+ alpha).
pub fn display_map(t: &TextureRecord) -> [u8; 4] {
    let c = gx2::codec(t.surface.format).ok();
    match t.kind {
        LeafKind::Bfres if t.comp_sel.iter().all(|&v| v <= 5) => t.comp_sel,
        _ => match c {
            Some(gx2::Codec::Bc4) | Some(gx2::Codec::R8) => [0, 0, 0, 5],
            Some(gx2::Codec::Bc5) | Some(gx2::Codec::Rg8) => [0, 0, 0, 1],
            _ => [0, 1, 2, 3],
        },
    }
}

pub fn apply_display_map(rgba: &mut [u8], map: [u8; 4]) {
    if map == [0, 1, 2, 3] {
        return;
    }
    for p in rgba.chunks_exact_mut(4) {
        let src = [p[0], p[1], p[2], p[3]];
        for c in 0..4 {
            p[c] = match map[c] {
                v @ 0..=3 => src[v as usize],
                4 => 0,
                _ => 255,
            };
        }
    }
}

/// Read every needed leaf once per disc file and hand it to `f`.
pub(crate) fn for_each_leaf<F>(inv: &Inventory, records: &[usize], progress: &Progress, f: F) -> Result<()>
where
    F: Fn(&[u8], &[usize]) + Sync,
{
    let mut by_leaf: BTreeMap<(&str, &Vec<String>), Vec<usize>> = BTreeMap::new();
    for &i in records {
        let t = &inv.textures[i];
        by_leaf.entry((t.id.location.file.as_str(), &t.id.location.inner)).or_default().push(i);
    }
    let mut by_file: BTreeMap<&str, Vec<(&Vec<String>, Vec<usize>)>> = BTreeMap::new();
    for ((file, inner), idx) in by_leaf {
        by_file.entry(file).or_default().push((inner, idx));
    }
    let root = Path::new(&inv.root);
    by_file.par_iter().try_for_each(|(file, leaves)| -> Result<()> {
        if progress.cancelled() {
            return Err(Error::Cancelled);
        }
        progress.wait_if_paused();
        progress.item(file);
        let data = match std::fs::read(root.join(file)) {
            Ok(d) => d,
            Err(e) => {
                progress.fail(format!("{file}: {e}"));
                return Ok(());
            }
        };
        for (inner, idx) in leaves {
            match container::extract_leaf(&data, inner) {
                Ok(leaf) => f(&leaf, idx),
                Err(e) => progress.fail(format!("{file} {inner:?}: {e}")),
            }
        }
        Ok(())
    })
}

/// Stage 1: lossless level-0 PNG master and thumbnail per unique texture,
/// plus pixel-based classification refinement.
pub fn extract(p: &Project, inv: &mut Inventory, progress: &Progress) -> Result<()> {
    let mut first: HashMap<String, usize> = HashMap::new();
    for (i, t) in inv.textures.iter().enumerate() {
        if t.decodable {
            first.entry(t.key.clone()).or_insert(i);
        }
    }
    let todo: Vec<usize> = first.values().copied().filter(|&i| !p.master_path(&inv.textures[i].key).exists() || !thumb_path(p, &inv.textures[i].key).exists()).collect();
    let already = first.len() - todo.len();
    progress.begin("Extracting", first.len());
    progress.steps(already);
    let refined: Mutex<HashMap<String, classify::Classification>> = Mutex::new(HashMap::new());
    let inv_ref: &Inventory = inv;
    for_each_leaf(inv_ref, &todo, progress, |leaf, idx| {
        for &i in idx {
            let t = &inv_ref.textures[i];
            match texture::decode(&info_of(t), leaf) {
                Ok(d) => {
                    let (w, h, rgba) = &d.levels[0];
                    let r = save_png(&p.master_path(&t.key), *w, *h, rgba).and_then(|_| {
                        let s = THUMB as f32 / (*w).max(*h) as f32;
                        let (tw, th) = (((*w as f32 * s) as u32).max(1), ((*h as f32 * s) as u32).max(1));
                        let mut small = if s < 1.0 { resample::resize_rgba8(*w, *h, rgba, tw, th, Filter::Bilinear, Edge::Clamp) } else { rgba.clone() };
                        apply_display_map(&mut small, display_map(t));
                        let (tw, th) = if s < 1.0 { (tw, th) } else { (*w, *h) };
                        save_png(&thumb_path(p, &t.key), tw, th, &small)
                    });
                    match r {
                        Ok(()) => {
                            refined.lock().unwrap().insert(t.key.clone(), classify::refine_with_pixels(t.classification.clone(), rgba));
                        }
                        Err(e) => progress.fail(format!("{}: {e}", t.id.key())),
                    }
                }
                Err(e) => progress.fail(format!("{}: {e}", t.id.key())),
            }
            progress.step();
        }
    })?;
    let refined = refined.into_inner().unwrap();
    for t in inv.textures.iter_mut() {
        if let Some(c) = refined.get(&t.key) {
            t.classification = c.clone();
        }
    }
    progress.log(format!("Extraction finished: {} masters ({} were already present)", first.len(), already));
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpscaleMeta {
    pub backend: BackendChoice,
    pub used: BackendChoice,
    /// The scale of the file this record sits next to (the achieved scale).
    pub scale: u32,
    /// The scale the plan asked for; 0 in records written before step-down.
    #[serde(default)]
    pub wanted: u32,
    /// The scale actually reached; 0 in records written before step-down
    /// (use [`UpscaleMeta::achieved_scale`]).
    #[serde(default)]
    pub achieved: u32,
    /// One entry per scale that failed, e.g. "x16 failed consistency (19.3 dB) → x8".
    #[serde(default)]
    pub steps: Vec<String>,
    pub seconds: f32,
    pub note: String,
    /// PSNR of the result shrunk back to the source size against the source.
    #[serde(default)]
    pub consistency_db: f32,
}

impl UpscaleMeta {
    /// The scale reached; an old record without the field achieved its file's scale.
    pub fn achieved_scale(&self) -> u32 {
        if self.achieved == 0 { self.scale } else { self.achieved }
    }
    /// The scale that was asked for.
    pub fn wanted_scale(&self) -> u32 {
        if self.wanted == 0 { self.achieved_scale() } else { self.wanted }
    }
}

/// AI results that do not shrink back to roughly the source are broken
/// (garbage output, wrong colours, shifted image) and are replaced.
pub const MIN_CONSISTENCY_DB: f32 = 22.0;

/// Shrink `up` to the source size and compare colour with the source.
pub fn consistency(sw: u32, sh: u32, src: &[u8], uw: u32, uh: u32, up: &[u8]) -> f32 {
    let small = resample::resize_rgba8(uw, uh, up, sw, sh, Filter::Bilinear, Edge::Clamp);
    let rgb = |d: &[u8]| -> Vec<u8> { d.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect() };
    psnr(&rgb(&small), &rgb(src))
}

fn meta_path(png: &Path) -> PathBuf {
    png.with_extension("json")
}

pub fn load_meta(png: &Path) -> Option<UpscaleMeta> {
    serde_json::from_slice(&std::fs::read(meta_path(png)).ok()?).ok()
}

/// The cached upscale for a planned (backend, texture, scale): the file at the
/// planned scale, or the step-down result a run for that scale recorded in a
/// lower-scale file's `.json`. Returns the PNG and the scale it really has.
pub fn find_upscaled(p: &Project, backend_key: &str, hash: &str, scale: u32) -> Option<(PathBuf, u32)> {
    let planned = p.upscaled_path(backend_key, hash, scale);
    if planned.exists() {
        return Some((planned, scale));
    }
    let mut n = scale / 2;
    while n >= 2 {
        let path = p.upscaled_path(backend_key, hash, n);
        if path.exists() && load_meta(&path).is_some_and(|m| m.wanted == scale && m.achieved_scale() == n) {
            return Some((path, n));
        }
        n /= 2;
    }
    None
}

/// Drop the cached upscale for a planned scale (the stepped-down result
/// included, PNG and record) so the next run starts over. Returns the PNG removed.
pub fn remove_upscaled(p: &Project, backend_key: &str, hash: &str, scale: u32) -> Option<PathBuf> {
    let (png, _) = find_upscaled(p, backend_key, hash, scale)?;
    let _ = std::fs::remove_file(meta_path(&png));
    let _ = std::fs::remove_file(&png);
    Some(png)
}

/// Effective backend for a class: normal maps and ramps never go to AI models.
fn effective(class: Class, b: &BackendChoice) -> BackendChoice {
    match class {
        Class::NormalMap if b.is_ai() => BackendChoice::Filter(Filter::CatmullRom),
        Class::Ramp => BackendChoice::Native,
        _ => b.clone(),
    }
}

#[derive(Clone)]
pub struct UpscaleTask {
    pub key: String,
    pub record: usize,
    pub scale: u32,
    pub backend: BackendChoice,
}

/// Unique (key, backend, scale) tasks for the given plans.
pub fn upscale_tasks(inv: &Inventory, plans: &[Plan], filter: impl Fn(usize) -> bool) -> Vec<UpscaleTask> {
    upscale_tasks_for(inv, &[plans], filter)
}

/// Unique (key, backend, scale) tasks over several plan sets, e.g. the rebuilt archives' and the runtime pack's,
/// which differ where the archive heap budget or exclusions hold a texture back.
pub fn upscale_tasks_for(inv: &Inventory, plan_sets: &[&[Plan]], filter: impl Fn(usize) -> bool) -> Vec<UpscaleTask> {
    let mut seen = std::collections::HashSet::new();
    let mut v = Vec::new();
    for plans in plan_sets {
        for (i, (t, pl)) in inv.textures.iter().zip(plans.iter()).enumerate() {
            if pl.upscales() && filter(i) && seen.insert((t.key.clone(), pl.backend.key(), pl.scale)) {
                v.push(UpscaleTask { key: t.key.clone(), record: i, scale: pl.scale, backend: pl.backend.clone() });
            }
        }
    }
    v
}

/// Everything either export needs: the rebuilt archives' plans and the runtime pack's.
pub fn all_upscale_tasks(inv: &Inventory, s: &Settings, d: &Decisions) -> Vec<UpscaleTask> {
    let (archives, runtime) = (crate::policy::plan_all(inv, s, d), crate::policy::plan_all_runtime(inv, s, d));
    upscale_tasks_for(inv, &[&archives, &runtime], |i| s.file_selected(&inv.textures[i].id.location.file))
}

/// Stage 2: upscale tasks whose results are not cached yet.
pub fn upscale(p: &Project, s: &Settings, inv: &Inventory, d: &Decisions, tasks: &[UpscaleTask], progress: &Progress) -> Result<()> {
    let tools = Tools { dir: s.tools_dir.clone() };
    let todo: Vec<&UpscaleTask> = tasks.iter().filter(|t| find_upscaled(p, &t.backend.key(), &t.key, t.scale).is_none()).collect();
    progress.begin("Upscaling", tasks.len());
    progress.steps(tasks.len() - todo.len());
    let scratch = p.root.join("cache/tmp");
    let mut groups: BTreeMap<String, Vec<&UpscaleTask>> = BTreeMap::new();
    for t in todo {
        let class = d.classification(&inv.textures[t.record]).class;
        groups.entry(effective(class, &t.backend).key()).or_default().push(t);
    }
    for (_, group) in groups {
        let class0 = d.classification(&inv.textures[group[0].record]).class;
        let backend = effective(class0, &group[0].backend);
        if !tools.available(&backend) {
            progress.log(format!("{} is not installed; using Lanczos-3 for {} textures", backend.label(), group.len()));
        }
        let batch = if backend.is_ai() { s.batch.max(1) } else { 64 };
        for chunk in group.chunks(batch) {
            if progress.cancelled() {
                return Err(Error::Cancelled);
            }
            progress.wait_if_paused();
            progress.item(&format!("{} ×{} ({})", backend.label(), chunk.len(), inv.textures[chunk[0].record].id.name));
            let started = std::time::Instant::now();
            let mut jobs = Vec::new();
            let mut tasks_ok = Vec::new();
            for t in chunk {
                let rec = &inv.textures[t.record];
                match load_png(&p.master_path(&t.key)) {
                    Ok((w, h, rgba)) => {
                        jobs.push(upscale::Job {
                            name: rec.id.name.clone(),
                            w,
                            h,
                            rgba,
                            scale: t.scale,
                            edge: if rec.kind == LeafKind::Bflim { Edge::Clamp } else { Edge::Wrap },
                        });
                        tasks_ok.push(*t);
                    }
                    Err(e) => progress.fail(format!("{}: master missing ({e}); run Extract first", rec.id.key())),
                }
            }
            let results = run_with_recovery(&backend, &tools, s, &scratch, &jobs, &tasks_ok, inv, d, progress)?;
            let secs = started.elapsed().as_secs_f32() / tasks_ok.len().max(1) as f32;
            for (t, r) in tasks_ok.iter().zip(results) {
                debug_assert_eq!(r.out.w % r.achieved, 0);
                let path = p.upscaled_path(&t.backend.key(), &t.key, r.achieved);
                let meta = UpscaleMeta {
                    backend: t.backend.clone(),
                    used: r.used,
                    scale: r.achieved,
                    wanted: t.scale,
                    achieved: r.achieved,
                    steps: r.steps,
                    seconds: secs,
                    note: r.note,
                    consistency_db: r.consistency_db,
                };
                match save_png(&path, r.out.w, r.out.h, &r.out.rgba).and_then(|_| write_atomic(&meta_path(&path), &serde_json::to_vec(&meta)?)) {
                    Ok(()) => progress.step(),
                    Err(e) => progress.fail(format!("{}: {e}", t.key)),
                }
            }
        }
    }
    progress.log("Upscaling finished".into());
    Ok(())
}

/// One upscaled texture and how it came about.
struct Recovered {
    out: upscale::Output,
    used: BackendChoice,
    /// The scale `out` has (the planned scale unless the run stepped down).
    achieved: u32,
    consistency_db: f32,
    steps: Vec<String>,
    note: String,
}

/// Runs a backend over a batch with a tile size; the seam that lets tests
/// replace the ncnn tools.
type RunFn<'a> = &'a dyn Fn(&BackendChoice, u32, &[upscale::Job]) -> Result<Vec<upscale::Output>>;

/// Run a batch; on failure retry with a small tile size (VRAM), then one by
/// one. A texture whose model run fails, or whose result does not shrink back
/// to the source (< 22 dB), steps down: the same model at half the scale, until
/// x2; only then Lanczos-3 at the planned scale. Every step is recorded.
#[allow(clippy::too_many_arguments)]
fn run_with_recovery(
    backend: &BackendChoice,
    tools: &Tools,
    s: &Settings,
    scratch: &Path,
    jobs: &[upscale::Job],
    tasks: &[&UpscaleTask],
    inv: &Inventory,
    d: &Decisions,
    progress: &Progress,
) -> Result<Vec<Recovered>> {
    let finish = |i: usize, o: upscale::Output| -> upscale::Output {
        let t = tasks[i];
        if d.classification(&inv.textures[t.record]).class != Class::NormalMap {
            return o;
        }
        let f = inv.textures[t.record].surface.format;
        let two = gx2::codec(f).map(|c| c == gx2::Codec::Bc5).unwrap_or(false);
        let filter = match backend {
            BackendChoice::Filter(f) => *f,
            _ => Filter::CatmullRom,
        };
        upscale::Output {
            w: o.w,
            h: o.h,
            // image_dds hands SNORM data over unsigned-mapped (0..255), like UNORM.
            rgba: upscale::upscale_normal_map(jobs[i].w, jobs[i].h, &jobs[i].rgba, jobs[i].scale, two, false, filter),
        }
    };
    let run = |b: &BackendChoice, tile: u32, js: &[upscale::Job]| upscale::run(b, tools, s.gpu, tile, scratch, js, progress.cancel_flag());
    recover(backend, tools.available(backend), s.tile, &run, &finish, jobs, &|m| progress.log(m))
}

fn recover(
    backend: &BackendChoice,
    available: bool,
    tile: u32,
    run: RunFn,
    finish: &dyn Fn(usize, upscale::Output) -> upscale::Output,
    jobs: &[upscale::Job],
    log: &dyn Fn(String),
) -> Result<Vec<Recovered>> {
    let lanczos = BackendChoice::Filter(Filter::Lanczos3);
    let q_of = |j: &upscale::Job, o: &upscale::Output| consistency(j.w, j.h, &j.rgba, o.w, o.h, &o.rgba);
    if !available {
        let outs = run(&lanczos, 0, jobs)?;
        return Ok(outs
            .into_iter()
            .enumerate()
            .zip(jobs)
            .map(|((i, o), j)| {
                let o = finish(i, o);
                let q = q_of(j, &o);
                Recovered { out: o, used: lanczos.clone(), achieved: j.scale, consistency_db: q, steps: vec![], note: format!("{} not installed", backend.label()) }
            })
            .collect());
    }
    // Whole batch first (tile as set, then 128); failing that, image by image below.
    let mut note = String::new();
    let mut batch = match run(backend, tile, jobs) {
        Ok(o) => Some(o),
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(e) => {
            log(format!("{} batch failed ({e}); retrying with tile 128", backend.label()));
            None
        }
    };
    if batch.is_none() {
        if let Ok(o) = run(backend, 128, jobs) {
            batch = Some(o);
            note = "retried with tile 128".into();
        }
    }
    let mut batch = batch.map(|b| b.into_iter());
    let mut results = Vec::with_capacity(jobs.len());
    for (i, j) in jobs.iter().enumerate() {
        let first = match batch.as_mut() {
            Some(it) => Ok(it.next().expect("one output per job")),
            None => {
                note = "processed one by one".into();
                run(backend, 64, std::slice::from_ref(j)).map(|mut o| o.remove(0))
            }
        };
        let mut why = match first {
            Ok(o) => {
                let o = finish(i, o);
                let q = q_of(j, &o);
                if q >= MIN_CONSISTENCY_DB || !backend.is_ai() {
                    results.push(Recovered { out: o, used: backend.clone(), achieved: j.scale, consistency_db: q, steps: vec![], note: note.clone() });
                    continue;
                }
                format!("consistency ({q:.1} dB)")
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            // A conventional filter resizes in one step: there is no lower scale to step down to.
            Err(e) if !backend.is_ai() => return Err(e),
            Err(e) => format!("({e})"),
        };
        // Step down: same model, half the scale, down to x2; then Lanczos-3 at the planned scale.
        let mut steps = Vec::new();
        let mut n = j.scale;
        loop {
            if n / 2 < 2 {
                steps.push(format!("x{n} failed {why} → Lanczos-3 x{}", j.scale));
                log(format!("{}: {} {}", j.name, backend.label(), steps.last().unwrap()));
                let o = run(&lanczos, 0, std::slice::from_ref(j))?.remove(0);
                let q = q_of(j, &o);
                let note = format!("{} failed at every scale; Lanczos-3 used", backend.label());
                results.push(Recovered { out: o, used: lanczos.clone(), achieved: j.scale, consistency_db: q, steps, note });
                break;
            }
            steps.push(format!("x{n} failed {why} → x{}", n / 2));
            log(format!("{}: {} {}", j.name, backend.label(), steps.last().unwrap()));
            n /= 2;
            // Only models step down, and models never get normal-map treatment, so no `finish`.
            let lower = upscale::Job { name: j.name.clone(), w: j.w, h: j.h, rgba: j.rgba.clone(), scale: n, edge: j.edge };
            match run(backend, 64, std::slice::from_ref(&lower)) {
                Ok(mut o) => {
                    let o = o.remove(0);
                    let q = q_of(j, &o);
                    if q >= MIN_CONSISTENCY_DB {
                        results.push(Recovered { out: o, used: backend.clone(), achieved: n, consistency_db: q, steps, note: note.clone() });
                        break;
                    }
                    why = format!("consistency ({q:.1} dB)");
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => why = format!("({e})"),
            }
        }
    }
    Ok(results)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplacementEntry {
    pub source_file: String,
    pub archive_path: Vec<String>,
    pub texture_name: String,
    pub texture_index: usize,
    pub original_hash: String,
    pub key: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub mip_count: u32,
    pub replacement_width: u32,
    pub replacement_height: u32,
    pub replacement_mip_count: u32,
    pub backend: String,
    pub backend_used: String,
    pub class: Class,
    pub psnr_db: f32,
    pub package: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportReport {
    pub packages: Vec<(String, u64, usize)>,
    pub files_rebuilt: usize,
    pub textures_replaced: usize,
    pub textures_skipped: Vec<String>,
    pub file_errors: Vec<String>,
    pub bytes_original: u64,
    pub bytes_rebuilt: u64,
    pub guest_growth_bytes: u64,
}

pub(crate) fn mip_mode(class: Class, format: u32, rgba: &[u8]) -> MipMode {
    let c = gx2::codec(format).ok();
    match class {
        // Decoded SNORM data is unsigned-mapped, see `run_with_recovery`.
        Class::NormalMap => MipMode::Normal { two_channel: c == Some(gx2::Codec::Bc5), snorm: false },
        Class::Mask | Class::Ramp => MipMode::Data,
        _ => MipMode::Colour { srgb: gx2::is_srgb(format), alpha_test: c.map(|c| c.has_alpha()).unwrap_or(false) && mips::is_alpha_tested(rgba) },
    }
}

fn psnr(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len()).max(1);
    let mse: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / n as f64;
    if mse <= 1e-9 { 99.0 } else { (10.0 * (255.0f64 * 255.0 / mse).log10()) as f32 }
}

/// Channels that carry data for a format, for validation (BC4/BC5/R8/RG8 ignore the rest).
fn data_channels(format: u32) -> [bool; 4] {
    match gx2::codec(format) {
        Ok(gx2::Codec::Bc4) | Ok(gx2::Codec::R8) => [true, false, false, false],
        Ok(gx2::Codec::Bc5) | Ok(gx2::Codec::Rg8) => [true, true, false, false],
        Ok(c) if !c.has_alpha() => [true, true, true, false],
        _ => [true, true, true, true],
    }
}

fn masked(rgba: &[u8], ch: [bool; 4]) -> Vec<u8> {
    rgba.chunks_exact(4).flat_map(|p| (0..4).filter(|&c| ch[c]).map(move |c| p[c])).collect()
}

/// Encoded (tiled) replacement for one texture record from its upscaled image.
fn build_one(p: &Project, s: &Settings, inv: &Inventory, d: &Decisions, i: usize, plan: &Plan) -> Result<(bfres::TextureReplacement, f32)> {
    let t = &inv.textures[i];
    let class = d.classification(t).class;
    // The scale the upscale really reached (it may have stepped down from the plan).
    let (png, scale) = find_upscaled(p, &plan.backend.key(), &t.key, plan.scale).ok_or_else(|| Error::Format("upscaled image missing".into()))?;
    let png_bytes = std::fs::read(&png).map_err(|e| Error::Format(format!("upscaled image missing: {e}")))?;
    // Encoded surfaces are cached by everything that determines them, including
    // the upscaled image's bytes, so shared textures encode once.
    let mut h = blake3::Hasher::new();
    h.update(b"encode-v1");
    h.update(&png_bytes);
    h.update(t.key.as_bytes());
    h.update(format!("{class:?}{}{}{:?}", scale, s.debug_tint, t.surface).as_bytes());
    let cache_key = h.finalize().to_hex().to_string();
    let cache_path = p.encoded_path(&cache_key);
    if let Some(hit) = load_encoded(&cache_path, t) {
        return Ok(hit);
    }
    let img = image::load_from_memory(&png_bytes).map_err(|e| Error::Format(format!("{}: {e}", png.display())))?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    let mut rgba = img.into_raw();
    if s.debug_tint {
        for px in rgba.chunks_exact_mut(4) {
            px[1] /= 3;
        }
    }
    if (w, h) != (t.surface.width * scale, t.surface.height * scale) {
        bail!("upscaled image is {w}x{h}, expected x{scale}");
    }
    let count = mips::upscaled_mip_count(t.surface.mip_count, scale);
    let mode = mip_mode(class, t.surface.format, &rgba);
    let levels = mips::chain(w, h, rgba, count, mode);
    let rep = texture::build_replacement(&info_of(t), &levels)?;
    // Validate: decode what will be written and compare with the intended image.
    let mut check = info_of(t);
    check.surface = rep.surface.clone();
    let lin = gx2::detile_levels(&rep.surface, &rep.image, &rep.mips)?;
    let (_, _, dec) = gx2::level_to_rgba(&check.surface, 0, &lin[0])?;
    let ch = data_channels(t.surface.format);
    let q = psnr(&masked(&dec, ch), &masked(&levels[0].2, ch));
    store_encoded(&cache_path, &rep, q);
    Ok((rep, q))
}

#[derive(Serialize, Deserialize)]
struct EncodedHeader {
    surface: gx2::Gx2Surface,
    regs: [u32; 5],
    image_len: usize,
    mips_len: usize,
    psnr: f32,
}

fn store_encoded(path: &Path, rep: &bfres::TextureReplacement, q: f32) {
    let head = EncodedHeader { surface: rep.surface.clone(), regs: rep.regs, image_len: rep.image.len(), mips_len: rep.mips.len(), psnr: q };
    let Ok(json) = serde_json::to_vec(&head) else { return };
    let mut out = Vec::with_capacity(4 + json.len() + rep.image.len() + rep.mips.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&rep.image);
    out.extend_from_slice(&rep.mips);
    let _ = write_atomic(path, &out);
}

fn load_encoded(path: &Path, t: &TextureRecord) -> Option<(bfres::TextureReplacement, f32)> {
    let d = std::fs::read(path).ok()?;
    let n = u32::from_le_bytes(d.get(..4)?.try_into().ok()?) as usize;
    let head: EncodedHeader = serde_json::from_slice(d.get(4..4 + n)?).ok()?;
    let body = &d[4 + n..];
    if body.len() != head.image_len + head.mips_len || head.surface.format != t.surface.format {
        return None;
    }
    let rep = bfres::TextureReplacement {
        surface: head.surface,
        regs: head.regs,
        image: body[..head.image_len].to_vec(),
        mips: body[head.image_len..].to_vec(),
    };
    Some((rep, head.psnr))
}

/// Minimum quality of the encoded result against the upscaled image. BC1 of
/// detailed art usually lands at 32–40 dB; much lower means a broken encode.
pub const MIN_PSNR: f32 = 24.0;

/// The pool that encodes and writes textures. Large archives hold many 4K textures: half the cores (2 to 12) bound
/// the memory in flight.
pub(crate) fn worker_pool() -> Result<rayon::ThreadPool> {
    let threads = std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(4).clamp(2, 12);
    rayon::ThreadPoolBuilder::new().num_threads(threads).build().map_err(|e| Error::Format(e.to_string()))
}

/// Stage 3: rebuild every affected disc file and write content packages.
pub fn export(p: &Project, s: &Settings, inv: &Inventory, d: &Decisions, progress: &Progress) -> Result<ExportReport> {
    let plans = policy::plan_all(inv, s, d);
    let mut report = ExportReport::default();
    let mut chosen: Vec<usize> = Vec::new();
    for (i, (t, pl)) in inv.textures.iter().zip(&plans).enumerate() {
        if !pl.upscales() || !s.file_selected(&t.id.location.file) {
            continue;
        }
        let r = d.review(&t.key);
        if r.status == ReviewStatus::Rejected || (r.status == ReviewStatus::Pending && !s.export_unreviewed) {
            continue;
        }
        if find_upscaled(p, &pl.backend.key(), &t.key, pl.scale).is_none() {
            report.textures_skipped.push(format!("{}: not upscaled yet", t.id.key()));
            continue;
        }
        chosen.push(i);
    }
    let mut by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &i in &chosen {
        let f = &inv.textures[i].id.location.file;
        if game_path(&inv.root, f).is_none() {
            report.textures_skipped.push(format!("{}: not below the disc's content folder", inv.textures[i].id.key()));
            continue;
        }
        by_file.entry(f.clone()).or_default().push(i);
    }
    let staging = p.packages_dir().join("_staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    progress.begin("Exporting", by_file.len());
    let root = Path::new(&inv.root);
    let entries: Mutex<Vec<ReplacementEntry>> = Mutex::new(Vec::new());
    let file_results: Mutex<Vec<(String, u64, u64, u64)>> = Mutex::new(Vec::new());
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let pool = worker_pool()?;
    pool.install(|| {
        by_file.par_iter().for_each(|(file, idx)| {
            if progress.cancelled() {
                return;
            }
            progress.wait_if_paused();
            progress.item(file);
            let result = (|| -> Result<(u64, u64, u64, Vec<ReplacementEntry>)> {
                let data = std::fs::read(root.join(file))?;
                let mut by_leaf: BTreeMap<Vec<String>, Vec<usize>> = BTreeMap::new();
                for &i in idx {
                    by_leaf.entry(inv.textures[i].id.location.inner.clone()).or_default().push(i);
                }
                let mut replace = BTreeMap::new();
                let mut file_entries = Vec::new();
                let mut growth = 0u64;
                for (inner, list) in &by_leaf {
                    let leaf = container::extract_leaf(&data, inner)?;
                    let kind = container::leaf_kind(&leaf).ok_or_else(|| Error::Format("leaf is no longer a texture container".into()))?;
                    let mut reps = Vec::new();
                    for &i in list {
                        let t = &inv.textures[i];
                        match build_one(p, s, inv, d, i, &plans[i]) {
                            Ok((rep, q)) if q >= MIN_PSNR => {
                                let meta = find_upscaled(p, &plans[i].backend.key(), &t.key, plans[i].scale).and_then(|(png, _)| load_meta(&png));
                                file_entries.push(ReplacementEntry {
                                    source_file: file.clone(),
                                    archive_path: inner.clone(),
                                    texture_name: t.id.name.clone(),
                                    texture_index: t.id.index,
                                    original_hash: t.data_hash.clone(),
                                    key: t.key.clone(),
                                    format: gx2::format_name(t.surface.format),
                                    width: t.surface.width,
                                    height: t.surface.height,
                                    mip_count: t.surface.mip_count,
                                    replacement_width: rep.surface.width,
                                    replacement_height: rep.surface.height,
                                    replacement_mip_count: rep.surface.mip_count,
                                    backend: plans[i].backend.label(),
                                    backend_used: meta.map(|m| m.used.label()).unwrap_or_default(),
                                    class: d.classification(t).class,
                                    psnr_db: q,
                                    package: String::new(),
                                });
                                growth += (rep.image.len() + rep.mips.len()) as u64 - (t.surface.image_size + t.surface.mip_size) as u64;
                                reps.push((t.id.index, rep));
                            }
                            Ok((_, q)) => errors.lock().unwrap().push(format!("{}: encoded quality {q:.1} dB below {MIN_PSNR} dB; kept original", t.id.key())),
                            Err(e) => errors.lock().unwrap().push(format!("{}: {e}; kept original", t.id.key())),
                        }
                    }
                    if reps.is_empty() {
                        continue;
                    }
                    let new_leaf = match kind {
                        LeafKind::Bfres => bfres::replace_textures(&leaf, &reps)?,
                        LeafKind::Bflim => {
                            let f = Bflim::parse(&leaf)?;
                            let (_, rep) = &reps[0];
                            f.rebuild(&rep.surface, &rep.image)?
                        }
                    };
                    validate_leaf(&leaf, &new_leaf, kind, &reps)?;
                    replace.insert(inner.clone(), new_leaf);
                }
                if replace.is_empty() {
                    return Ok((data.len() as u64, 0, 0, Vec::new()));
                }
                let rebuilt = container::rebuild_file(&data, &replace)?;
                validate_file(file, &data, &rebuilt, &replace)?;
                if rebuilt.len() as u64 > s.file_max_bytes {
                    bail!("rebuilt size {} MiB exceeds the {} MiB per-file limit", rebuilt.len() >> 20, s.file_max_bytes >> 20);
                }
                write_atomic(&staging.join(file), &rebuilt)?;
                Ok((data.len() as u64, rebuilt.len() as u64, growth, file_entries))
            })();
            match result {
                Ok((orig, new, growth, es)) => {
                    if new > 0 {
                        file_results.lock().unwrap().push((file.clone(), orig, new, growth));
                        entries.lock().unwrap().extend(es);
                    }
                    progress.step();
                }
                Err(e) => {
                    progress.fail(format!("{file}: {e}"));
                    errors.lock().unwrap().push(format!("{file}: {e}; file not exported"));
                }
            }
        })
    });
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    let mut files = file_results.into_inner().unwrap();
    let mut entries = entries.into_inner().unwrap();
    report.file_errors = errors.into_inner().unwrap();

    // Split into packages within the loader limits (first-fit decreasing).
    files.sort_by(|a, b| b.2.cmp(&a.2));
    let mut bins: Vec<(u64, Vec<String>)> = Vec::new();
    for (f, _, new, _) in &files {
        match bins.iter_mut().find(|(sz, list)| sz + new <= s.package_max_bytes && list.len() < s.package_max_files) {
            Some(b) => {
                b.0 += new;
                b.1.push(f.clone());
            }
            None => bins.push((*new, vec![f.clone()])),
        }
    }
    // Remove previous packages of this tool, then move staged files into place.
    if let Ok(rd) = std::fs::read_dir(p.packages_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(PACKAGE_PREFIX) {
                std::fs::remove_dir_all(e.path())?;
            }
        }
    }
    let mut file_pkg: HashMap<String, String> = HashMap::new();
    for (k, (size, list)) in bins.iter().enumerate() {
        let id = format!("{PACKAGE_PREFIX}{}", k + 1);
        let dir = p.packages_dir().join(&id);
        for f in list {
            let dst = dir.join("content").join(game_path(&inv.root, f).unwrap());
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::rename(staging.join(f), &dst)?;
            file_pkg.insert(f.clone(), id.clone());
        }
        let manifest = serde_json::json!({
            "format_version": 1,
            "id": id,
            "name": format!("{} ({}/{})", s.package_name, k + 1, bins.len()),
            "version": "1.0.0",
            "game_id": "wwhd",
            "minimum_manager_version": "1.5.0",
            "kind": "content",
            "content_dir": "content",
            "description": format!("Upscaled textures made with ZeldaWWHD Texture Studio. Part {} of {}: {} rebuilt archives. Requires restart.", k + 1, bins.len(), list.len()),
        });
        write_atomic(&dir.join("manifest.json"), &serde_json::to_vec_pretty(&manifest)?)?;
        report.packages.push((id, *size, list.len()));
    }
    let _ = std::fs::remove_dir_all(&staging);
    for e in entries.iter_mut() {
        e.package = file_pkg.get(&e.source_file).cloned().unwrap_or_default();
    }
    report.files_rebuilt = files.len();
    report.textures_replaced = entries.len();
    report.bytes_original = files.iter().map(|f| f.1).sum();
    report.bytes_rebuilt = files.iter().map(|f| f.2).sum();
    report.guest_growth_bytes = files.iter().map(|f| f.3).sum();
    write_atomic(&p.root.join("manifests/replacement_manifest.json"), &serde_json::to_vec_pretty(&serde_json::json!({
        "version": 1,
        "game": "ZeldaWWHD",
        "integration": "recomp content packages (rebuilt archives)",
        "textures": entries,
    }))?)?;
    write_atomic(&p.reports_dir().join("export_report.json"), &serde_json::to_vec_pretty(&report)?)?;
    progress.log(format!(
        "Export finished: {} textures in {} archives, {} packages, {} errors",
        report.textures_replaced,
        report.files_rebuilt,
        report.packages.len(),
        report.file_errors.len()
    ));
    Ok(report)
}

pub const PACKAGE_PREFIX: &str = "wwhd-texture-studio-";

/// Path the game opens below `/vol/content/` for a scanned file, or None when
/// the file is not game content (code, meta) and cannot be replaced.
pub fn game_path(inv_root: &str, file: &str) -> Option<String> {
    let root_is_content = Path::new(inv_root).file_name().is_some_and(|n| n.eq_ignore_ascii_case("content"));
    if root_is_content {
        return Some(file.to_string());
    }
    file.strip_prefix("content/").map(String::from)
}

/// The rebuilt leaf must list the same textures; replaced ones must have the
/// new surface and untouched ones identical data.
fn validate_leaf(old: &[u8], new: &[u8], kind: LeafKind, reps: &[(usize, bfres::TextureReplacement)]) -> Result<()> {
    let loc = container::AssetLocation { file: String::new(), inner: Vec::new() };
    let a = texture::list(&loc, kind, old)?;
    let b = texture::list(&loc, kind, new)?;
    if a.len() != b.len() {
        bail!("rebuilt leaf lists {} textures instead of {}", b.len(), a.len());
    }
    for (x, y) in a.iter().zip(&b) {
        if x.id.name != y.id.name {
            bail!("texture order/name changed: {} → {}", x.id.name, y.id.name);
        }
        match reps.iter().find(|(i, _)| *i == x.id.index) {
            Some((_, r)) => {
                if y.surface.width != r.surface.width || y.surface.height != r.surface.height || y.surface.mip_count != r.surface.mip_count || y.surface.format != x.surface.format {
                    bail!("{}: rebuilt surface does not match the replacement", x.id.name);
                }
                let g = texture::gate(&y, new);
                if !g.passed() {
                    bail!("{}: rebuilt surface fails the layout gate: {}", x.id.name, g.detail);
                }
            }
            None => {
                if x.data_hash != y.data_hash || x.surface != y.surface {
                    bail!("{}: untouched texture changed", x.id.name);
                }
            }
        }
    }
    if kind == LeafKind::Bfres {
        let fo = bfres::Bfres::parse(old)?;
        let fn_ = bfres::Bfres::parse(new)?;
        if fo.version != fn_.version {
            bail!("BFRES version changed");
        }
        // Everything before the appended data, except the replaced FTEX headers and the header size/alignment, is unchanged.
        let mut diff_ok = vec![false; old.len()];
        for (i, _) in reps {
            let t = &fo.textures[*i];
            for k in t.offset..(t.offset + 0xC0).min(old.len()) {
                diff_ok[k] = true;
            }
        }
        for k in 0x0C..0x14 {
            diff_ok[k] = true;
        }
        if let Some(k) = (0..old.len()).find(|&k| !diff_ok[k] && old[k] != new[k]) {
            bail!("rebuilt BFRES differs outside the replaced textures at 0x{k:x}");
        }
    }
    Ok(())
}

/// Re-walk the rebuilt disc file: same leaves in the same order, and each
/// replaced leaf decompresses to exactly the bytes we built.
fn validate_file(file: &str, old: &[u8], new: &[u8], replace: &BTreeMap<Vec<String>, Vec<u8>>) -> Result<()> {
    let list = |d: &[u8]| {
        let v = std::cell::RefCell::new(Vec::new());
        container::walk(
            file,
            d,
            &mut |loc, _, _| v.borrow_mut().push(loc.inner),
            &mut |loc, _, _| v.borrow_mut().push(loc.inner.clone()),
            &mut |loc, e| v.borrow_mut().push(vec![format!("ERROR {e} {:?}", loc.inner)]),
        );
        v.into_inner()
    };
    let (a, b) = (list(old), list(new));
    if a != b {
        bail!("rebuilt archive entry list differs from the original");
    }
    for (inner, bytes) in replace {
        if &container::extract_leaf(new, inner)? != bytes {
            bail!("{inner:?} does not read back identically");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn job(scale: u32) -> upscale::Job {
        let rgba: Vec<u8> = (0..64).flat_map(|i| [(i * 4) as u8, (255 - i * 4) as u8, (i * 2) as u8, 255]).collect();
        upscale::Job { name: "t".into(), w: 8, h: 8, rgba, scale, edge: Edge::Wrap }
    }

    /// Stand-in for the ncnn tools: the model fails or returns garbage at the
    /// scales listed, and is a faithful nearest-neighbour upscale otherwise.
    /// Conventional filters run for real. Records the scales it was asked for.
    struct Stub {
        fail: Vec<u32>,
        garbage: Vec<u32>,
        asked: RefCell<Vec<u32>>,
    }
    impl Stub {
        fn new(fail: &[u32], garbage: &[u32]) -> Self {
            Stub { fail: fail.to_vec(), garbage: garbage.to_vec(), asked: RefCell::new(vec![]) }
        }
        fn run(&self, b: &BackendChoice, _tile: u32, js: &[upscale::Job]) -> Result<Vec<upscale::Output>> {
            if !b.is_ai() {
                let tools = Tools { dir: PathBuf::new() };
                return upscale::run(b, &tools, 0, 0, Path::new("."), js, &std::sync::atomic::AtomicBool::new(false));
            }
            let sc = js.iter().map(|j| j.scale).max().unwrap();
            self.asked.borrow_mut().push(sc);
            if self.fail.contains(&sc) {
                return Err(Error::Format(format!("out of memory at x{sc}")));
            }
            Ok(js
                .iter()
                .map(|j| {
                    let (w, h) = (j.w * j.scale, j.h * j.scale);
                    let mut rgba = Vec::new();
                    for y in 0..h {
                        for x in 0..w {
                            let px = if self.garbage.contains(&j.scale) {
                                [255, 0, 255, 255]
                            } else {
                                let k = (((y / j.scale) * j.w + x / j.scale) * 4) as usize;
                                [j.rgba[k], j.rgba[k + 1], j.rgba[k + 2], j.rgba[k + 3]]
                            };
                            rgba.extend_from_slice(&px);
                        }
                    }
                    upscale::Output { w, h, rgba }
                })
                .collect())
        }
    }

    fn go(stub: &Stub, jobs: &[upscale::Job]) -> Vec<Recovered> {
        let backend = BackendChoice::Custom("stub".into());
        let run = |b: &BackendChoice, t: u32, js: &[upscale::Job]| stub.run(b, t, js);
        recover(&backend, true, 0, &run, &|_, o| o, jobs, &|_| {}).unwrap()
    }

    #[test]
    fn model_failure_steps_down_instead_of_using_lanczos() {
        let stub = Stub::new(&[32, 16], &[]);
        let r = go(&stub, &[job(32)]).remove(0);
        assert_eq!(r.achieved, 8);
        assert_eq!((r.out.w, r.out.h), (64, 64));
        assert!(matches!(r.used, BackendChoice::Custom(_)), "the model, not Lanczos");
        assert_eq!(r.steps.len(), 2);
        assert!(r.steps[0].starts_with("x32 failed (") && r.steps[0].ends_with("→ x16"), "{:?}", r.steps);
        assert!(r.steps[1].starts_with("x16 failed (") && r.steps[1].ends_with("→ x8"), "{:?}", r.steps);
        assert_eq!(*stub.asked.borrow(), [32, 32, 32, 16, 8], "batch, tile 128, one by one, then down");
    }

    #[test]
    fn consistency_failure_steps_down() {
        let stub = Stub::new(&[], &[16]);
        let r = go(&stub, &[job(16)]).remove(0);
        assert_eq!(r.achieved, 8);
        assert_eq!(r.steps.len(), 1);
        assert!(r.steps[0].starts_with("x16 failed consistency (") && r.steps[0].ends_with(" dB) → x8"), "{:?}", r.steps);
        assert!(r.consistency_db >= MIN_CONSISTENCY_DB);
    }

    #[test]
    fn model_failing_everywhere_ends_in_lanczos_at_the_planned_scale() {
        let stub = Stub::new(&[2, 4, 8, 16, 32], &[]);
        let r = go(&stub, &[job(32)]).remove(0);
        assert_eq!(r.achieved, 32);
        assert_eq!((r.out.w, r.out.h), (256, 256));
        assert_eq!(r.used, BackendChoice::Filter(Filter::Lanczos3));
        assert_eq!(r.steps.len(), 5);
        assert!(r.steps[4].ends_with("→ Lanczos-3 x32"), "{:?}", r.steps);
    }

    #[test]
    fn success_records_no_steps_and_each_texture_steps_down_on_its_own() {
        let stub = Stub::new(&[], &[]);
        let r = go(&stub, &[job(4)]).remove(0);
        assert_eq!((r.achieved, r.steps.len()), (4, 0));
        // x8 is garbage, x4 fine: only the x8 texture steps down.
        let stub = Stub::new(&[], &[8]);
        let r = go(&stub, &[job(4), job(8)]);
        assert_eq!((r[0].achieved, r[1].achieved), (4, 4));
        assert_eq!((r[0].steps.len(), r[1].steps.len()), (0, 1));
    }

    /// A conventional filter resizes in one step: when it fails there is no lower scale to try and no
    /// other filter to fall back on; the error is the result.
    #[test]
    fn a_failing_filter_does_not_step_down() {
        let asked = RefCell::new(Vec::new());
        let run = |_: &BackendChoice, _: u32, js: &[upscale::Job]| -> Result<Vec<upscale::Output>> {
            asked.borrow_mut().push(js[0].scale);
            Err(Error::Format("filter failed".into()))
        };
        let backend = BackendChoice::Filter(Filter::Mitchell);
        let r = recover(&backend, true, 0, &run, &|_, o| o, &[job(4)], &|_| {});
        assert!(matches!(r, Err(Error::Format(ref m)) if m == "filter failed"));
        assert_eq!(*asked.borrow(), [4, 4, 4], "batch, tile 128, one by one; no x2 and no Lanczos");
    }

    #[test]
    fn a_stepped_down_result_is_found_for_the_scale_that_was_planned() {
        let root = std::env::temp_dir().join(format!("wwhd-find-upscaled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let p = Project { root: root.clone() };
        let hash = "ab".repeat(16);
        let (be, png) = ("custom-x", p.upscaled_path("custom-x", &hash, 8));
        std::fs::create_dir_all(png.parent().unwrap()).unwrap();
        std::fs::write(&png, b"png").unwrap();
        // No record: a lone x8 file is an x8 result, not an answer for x32.
        assert_eq!(find_upscaled(&p, be, &hash, 8), Some((png.clone(), 8)));
        assert_eq!(find_upscaled(&p, be, &hash, 32), None);
        let meta = UpscaleMeta {
            backend: BackendChoice::Custom("x".into()),
            used: BackendChoice::Custom("x".into()),
            scale: 8,
            wanted: 32,
            achieved: 8,
            steps: vec!["x32 failed (oom) → x16".into(), "x16 failed (oom) → x8".into()],
            seconds: 0.0,
            note: String::new(),
            consistency_db: 40.0,
        };
        std::fs::write(meta_path(&png), serde_json::to_vec(&meta).unwrap()).unwrap();
        assert_eq!(find_upscaled(&p, be, &hash, 32), Some((png.clone(), 8)));
        assert_eq!(find_upscaled(&p, be, &hash, 16), None, "recorded for x32 only");
        assert_eq!(remove_upscaled(&p, be, &hash, 32), Some(png.clone()));
        assert!(!png.exists() && !meta_path(&png).exists());
        assert_eq!(find_upscaled(&p, be, &hash, 32), None, "regenerate really starts over");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_records_without_step_fields_read_as_achieved_at_their_scale() {
        let old = r#"{"backend":{"Filter":"Lanczos3"},"used":{"Filter":"Lanczos3"},"scale":4,"seconds":1.0,"note":"","consistency_db":30.0}"#;
        let m: UpscaleMeta = serde_json::from_str(old).unwrap();
        assert_eq!((m.achieved_scale(), m.wanted_scale()), (4, 4));
    }
}
