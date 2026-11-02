//! Per-texture decisions: target resolution and backend, review state, and the
//! per-archive budget that keeps rebuilt files inside the mod loader limits.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::classify::{Class, Classification};
use crate::error::Result;
use crate::gx2;
use crate::inventory::{Inventory, TextureRecord};
use crate::project::{write_atomic, ScaleChoice, Settings, SCALES};
use crate::upscale::BackendChoice;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    #[default]
    Pending,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Review {
    pub status: ReviewStatus,
    pub backend: Option<BackendChoice>,
    pub scale: Option<u32>,
    pub rating: u8,
    pub note: String,
}

/// Reviews and manual classifications, keyed by texture content key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Decisions {
    pub reviews: HashMap<String, Review>,
    pub classes: HashMap<String, Class>,
}

impl Decisions {
    pub fn load(review: &Path, classes: &Path) -> Decisions {
        let reviews = std::fs::read(review).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        let classes = std::fs::read(classes).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        Decisions { reviews, classes }
    }
    pub fn save(&self, review: &Path, classes: &Path) -> Result<()> {
        write_atomic(review, &serde_json::to_vec_pretty(&self.reviews)?)?;
        write_atomic(classes, &serde_json::to_vec_pretty(&self.classes)?)
    }
    pub fn review(&self, key: &str) -> Review {
        self.reviews.get(key).cloned().unwrap_or_default()
    }
    pub fn classification(&self, t: &TextureRecord) -> Classification {
        match self.classes.get(&t.key) {
            Some(c) => Classification { class: *c, confidence: 1.0, reason: "set manually".into(), manual: true },
            None => t.classification.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub scale: u32,
    pub backend: BackendChoice,
    pub reason: String,
}

impl Plan {
    fn native(reason: impl Into<String>) -> Plan {
        Plan { scale: 1, backend: BackendChoice::Native, reason: format!("native: {}", reason.into()) }
    }
    pub fn upscales(&self) -> bool {
        self.scale > 1 && self.backend != BackendChoice::Native
    }
}

/// The scale a texture asks for and where it comes from ("texture", "class" or
/// "global"): a review's scale beats the class setting, which beats the global
/// scale. 1 means native.
pub fn wanted_scale(_t: &TextureRecord, class: Class, s: &Settings, review: Option<&Review>) -> (u32, &'static str) {
    let valid = |v: u32| v == 1 || SCALES.contains(&v);
    if let Some(v) = review.and_then(|r| r.scale).filter(|&v| valid(v)) {
        return (v, "texture");
    }
    match s.scale_choice_for(class) {
        ScaleChoice::Native => (1, "class"),
        ScaleChoice::X(v) if SCALES.contains(&v) => (v, "class"),
        _ => (if valid(s.scale) { s.scale } else { 1 }, "global"),
    }
}

/// The largest texture side the device can hold.
const MAX_DEVICE_DIM: u64 = 16384;

/// Largest scale at or below `want` (halving, never skipping a step) whose
/// output side stays within `max_dim` (itself capped at the device limit of
/// 16384). Returns 1 only when even x2 does not fit.
pub fn fit_scale(want: u32, max_side: u32, max_dim: u32) -> u32 {
    let cap = (max_dim as u64).min(MAX_DEVICE_DIM);
    let mut scale = want;
    while scale > 1 && max_side as u64 * scale as u64 > cap {
        scale /= 2;
    }
    scale.max(1)
}

pub fn plan(t: &TextureRecord, s: &Settings, d: &Decisions) -> Plan {
    plan_with(t, s, d, true)
}

/// `plan`, optionally ignoring `exclude_files` (the runtime pack has no guest heaps).
fn plan_with(t: &TextureRecord, s: &Settings, d: &Decisions, honour_excluded: bool) -> Plan {
    if !t.decodable {
        return Plan::native(format!("format {} / {} slices not supported", gx2::format_name(t.surface.format), t.surface.depth));
    }
    if !t.gate_ok {
        return Plan::native(format!("failed the rewrite gate: {}", t.gate_detail));
    }
    if honour_excluded && s.file_excluded(&t.id.location.file) {
        return Plan::native("archive is loaded into a fixed-size game heap (excluded)");
    }
    let review = d.review(&t.key);
    if review.status == ReviewStatus::Rejected {
        return Plan::native("rejected in review");
    }
    let class = d.classification(t).class;
    let backend = review.backend.clone().unwrap_or_else(|| s.backend_for(class));
    if backend == BackendChoice::Native {
        return Plan::native(format!("{} keeps native resolution", class.label()));
    }
    let side = t.max_side();
    if review.scale.is_none() && side < s.min_source_dim {
        return Plan::native(format!("smaller than {} px", s.min_source_dim));
    }
    let (want, source) = wanted_scale(t, class, s, Some(&review));
    if want <= 1 {
        return Plan::native(format!("{} is set to native ({source})", class.label()));
    }
    let scale = fit_scale(want, side, s.max_dim);
    if scale <= 1 {
        return Plan::native(format!("{}: x{want} ({source}), but even x2 of {side} px exceeds max side {}", class.label(), s.max_dim));
    }
    let mut reason = format!("{}: x{want} ({source})", class.label());
    if scale != want {
        reason.push_str(&format!(" → x{scale}: max side {}", s.max_dim));
    }
    Plan { scale, backend, reason }
}

/// Bytes of a full mip chain at the given base size for a texture's format.
pub fn surface_bytes(t: &TextureRecord, scale: u32) -> u64 {
    let Ok(c) = gx2::codec(t.surface.format) else { return 0 };
    let (bd, bpe) = (c.block_dim() as u64, c.bytes() as u64);
    let levels = if scale == 1 { t.surface.mip_count } else { crate::mips::upscaled_mip_count(t.surface.mip_count, scale) };
    let (w, h) = (t.surface.width as u64 * scale as u64, t.surface.height as u64 * scale as u64);
    (0..levels.max(1) as u64)
        .map(|l| ((w >> l).max(1)).div_ceil(bd) * ((h >> l).max(1)).div_ceil(bd) * bpe)
        .sum()
}

/// Plans for every texture record (index-aligned with `inv.textures`), with
/// per-file budgets applied: while a file's growth exceeds the limits, the
/// largest upscaled texture in it is halved.
pub fn plan_all(inv: &Inventory, s: &Settings, d: &Decisions) -> Vec<Plan> {
    let mut plans: Vec<Plan> = inv.textures.iter().map(|t| plan(t, s, d)).collect();
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, t) in inv.textures.iter().enumerate() {
        by_file.entry(t.id.location.file.as_str()).or_default().push(i);
    }
    let sizes: HashMap<&str, (u64, u64)> = inv.files.iter().map(|f| (f.path.as_str(), (f.size, f.plain_size))).collect();
    for (file, idx) in by_file {
        // Compressed size is unknown before export; assume BC data stays
        // ~uncompressible, so growth adds to the on-disc size.
        let (base, plain) = *sizes.get(file).unwrap_or(&(0, 0));
        let plain = if plain == 0 { base } else { plain };
        let growth_cap = if s.max_growth_factor > 0.0 {
            ((plain as f64) * (s.max_growth_factor as f64 - 1.0)).max(0.0) as u64
        } else {
            u64::MAX
        };
        loop {
            let growth: u64 = idx
                .iter()
                .map(|&i| surface_bytes(&inv.textures[i], plans[i].scale).saturating_sub(surface_bytes(&inv.textures[i], 1)))
                .sum();
            if growth <= s.archive_growth_max_bytes && growth <= growth_cap && base + growth <= s.file_max_bytes {
                break;
            }
            let Some(&big) = idx
                .iter()
                .filter(|&&i| plans[i].scale > 1)
                .max_by_key(|&&i| surface_bytes(&inv.textures[i], plans[i].scale))
            else {
                break;
            };
            // The documented exception to "step down, never skip": the budget may reach native.
            let p = &mut plans[big];
            p.scale /= 2;
            if p.scale <= 1 {
                *p = Plan { scale: 1, backend: BackendChoice::Native, reason: format!("{} → native: archive heap budget", p.reason) };
            } else {
                p.reason = format!("{} → x{}: archive heap budget", p.reason, p.scale);
            }
        }
    }
    plans
}

/// Plans for the runtime pack: the same per-texture plans as `plan_all`, but
/// without the per-archive budget and without `exclude_files`, because the
/// runtime pack replaces textures at draw time and has no guest-heap limits.
pub fn plan_all_runtime(inv: &Inventory, s: &Settings, d: &Decisions) -> Vec<Plan> {
    inv.textures.iter().map(|t| plan_with(t, s, d, false)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Classification;
    use crate::container::AssetLocation;
    use crate::gx2::Gx2Surface;
    use crate::inventory::FileRecord;
    use crate::project::{default_class_scales, ScaleChoice};
    use crate::texture::TextureId;

    fn tex(file: &str, key: &str, side: u32, class: Class) -> TextureRecord {
        TextureRecord {
            id: TextureId { location: AssetLocation { file: file.into(), inner: vec![] }, index: 0, name: key.into() },
            kind: crate::container::LeafKind::Bfres,
            surface: Gx2Surface { dim: 1, width: side, height: side, depth: 1, mip_count: 1, format: 0x31, ..Default::default() },
            comp_sel: [0, 1, 2, 3],
            data_hash: key.into(),
            key: key.into(),
            gate_ok: true,
            gate_detail: String::new(),
            decodable: true,
            classification: Classification { class, confidence: 1.0, reason: String::new(), manual: false },
        }
    }

    fn settings(global: u32, class: Class, choice: ScaleChoice) -> Settings {
        let mut s = Settings { scale: global, exclude_files: vec![], ..Settings::default() };
        s.class_scales.retain(|(c, _)| *c != class);
        s.class_scales.push((class, choice));
        s
    }

    fn review(scale: Option<u32>) -> Review {
        Review { scale, ..Review::default() }
    }

    #[test]
    fn precedence_texture_class_global() {
        let t = tex("a.szs", "k1", 128, Class::Rock);
        let s = settings(4, Class::Rock, ScaleChoice::X(8));
        assert_eq!(wanted_scale(&t, Class::Rock, &s, Some(&review(Some(16)))), (16, "texture"));
        assert_eq!(wanted_scale(&t, Class::Rock, &s, None), (8, "class"));
        assert_eq!(wanted_scale(&t, Class::Rock, &s, Some(&review(None))), (8, "class"));
        let s = settings(4, Class::Rock, ScaleChoice::Global);
        assert_eq!(wanted_scale(&t, Class::Rock, &s, None), (4, "global"));
        let s = settings(4, Class::Rock, ScaleChoice::Native);
        assert_eq!(wanted_scale(&t, Class::Rock, &s, None), (1, "class"));
        assert_eq!(wanted_scale(&t, Class::Rock, &s, Some(&review(Some(1)))), (1, "texture"));
        let p = plan(&t, &s, &Decisions::default());
        assert!(!p.upscales() && p.scale == 1, "{p:?}");
        assert!(p.reason.starts_with("native: "), "{p:?}");
    }

    #[test]
    fn plan_reasons_follow_the_source() {
        let t = tex("a.szs", "k1", 128, Class::Rock);
        let s = settings(4, Class::Rock, ScaleChoice::X(8));
        let p = plan(&t, &s, &Decisions::default());
        assert_eq!(p.scale, 8);
        assert_eq!(p.reason, format!("{}: x8 (class)", Class::Rock.label()));
    }

    #[test]
    fn fit_scale_steps_down() {
        assert_eq!(fit_scale(32, 128, 4096), 32);
        assert_eq!(fit_scale(32, 512, 4096), 8);
        assert_eq!(fit_scale(4, 2048, 4096), 2);
        assert_eq!(fit_scale(4, 4096, 4096), 1);
        assert_eq!(fit_scale(32, 64, 16384), 32);
        assert_eq!(fit_scale(32, 1024, 65536), 16);
        assert_eq!(fit_scale(16, 320, 4096), 8);
    }

    #[test]
    fn a_2048_px_texture_is_no_longer_native() {
        let t = tex("a.szs", "k1", 2048, Class::Rock);
        let p = plan(&t, &Settings { exclude_files: vec![], ..Settings::default() }, &Decisions::default());
        assert_eq!(p.scale, 2, "{p:?}");
        assert!(p.reason.contains("max side 4096"), "{p:?}");
        assert!(p.reason.contains("x4 (global)"), "{p:?}");
    }

    #[test]
    fn a_texture_that_cannot_double_stays_native_with_a_reason() {
        let t = tex("a.szs", "k1", 4096, Class::Rock);
        let p = plan(&t, &Settings { exclude_files: vec![], ..Settings::default() }, &Decisions::default());
        assert_eq!(p.scale, 1);
        assert!(p.reason.contains("max side 4096"), "{p:?}");
    }

    #[test]
    fn old_settings_without_class_scales_load_defaults() {
        let s: Settings = serde_json::from_str(r#"{"scale": 2, "native_from_dim": 2048}"#).unwrap();
        assert_eq!(s.scale, 2);
        assert_eq!(s.class_scales, default_class_scales());
        assert_eq!(s.scale_choice_for(Class::Ramp), ScaleChoice::Native);
        assert_eq!(s.scale_choice_for(Class::Rock), ScaleChoice::Global);
    }

    fn inventory(t: TextureRecord) -> Inventory {
        let file = FileRecord {
            path: t.id.location.file.clone(),
            size: 1000,
            plain_size: 1000,
            hash: String::new(),
            kind: String::new(),
            leaves: 1,
            textures: 1,
            error: None,
        };
        Inventory { files: vec![file], textures: vec![t], ..Inventory::default() }
    }

    #[test]
    fn archive_budget_may_leave_a_texture_native() {
        let inv = inventory(tex("a.szs", "k1", 256, Class::Rock));
        let s = Settings { exclude_files: vec![], archive_growth_max_bytes: 1, ..Settings::default() };
        let plans = plan_all(&inv, &s, &Decisions::default());
        assert_eq!(plans[0].scale, 1);
        assert!(plans[0].reason.contains("archive heap budget"), "{:?}", plans[0]);
        // The runtime pack has no archive budget.
        let rt = plan_all_runtime(&inv, &s, &Decisions::default());
        assert_eq!(rt[0].scale, 4, "{:?}", rt[0]);
    }

    #[test]
    fn runtime_plans_ignore_excluded_archives() {
        let inv = inventory(tex("Common/Misc/Misc.szs", "k1", 256, Class::Rock));
        let s = Settings::default();
        assert!(!plan_all(&inv, &s, &Decisions::default())[0].upscales());
        assert_eq!(plan_all_runtime(&inv, &s, &Decisions::default())[0].scale, 4);
    }
}
