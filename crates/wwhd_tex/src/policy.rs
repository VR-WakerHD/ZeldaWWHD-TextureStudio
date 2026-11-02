//! Per-texture decisions: target resolution and backend, review state, and the
//! per-archive budget that keeps rebuilt files inside the mod loader limits.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::classify::{Class, Classification};
use crate::error::Result;
use crate::gx2;
use crate::inventory::{Inventory, TextureRecord};
use crate::project::{write_atomic, Settings};
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
        Plan { scale: 1, backend: BackendChoice::Native, reason: reason.into() }
    }
    pub fn upscales(&self) -> bool {
        self.scale > 1 && self.backend != BackendChoice::Native
    }
}

pub fn plan(t: &TextureRecord, s: &Settings, d: &Decisions) -> Plan {
    if !t.decodable {
        return Plan::native(format!("format {} / {} slices not supported", gx2::format_name(t.surface.format), t.surface.depth));
    }
    if !t.gate_ok {
        return Plan::native(format!("failed the rewrite gate: {}", t.gate_detail));
    }
    if s.file_excluded(&t.id.location.file) {
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
    if review.scale.is_none() {
        if side < s.min_source_dim {
            return Plan::native(format!("smaller than {} px", s.min_source_dim));
        }
        if side >= s.native_from_dim {
            return Plan::native(format!("already {side} px"));
        }
    }
    let want = review.scale.unwrap_or(s.scale).max(1);
    let mut scale = want;
    while scale > 1 && side * scale > s.max_dim {
        scale /= 2;
    }
    if scale <= 1 {
        return Plan::native(format!("{side} px cannot grow within {} px", s.max_dim));
    }
    Plan { scale, backend, reason: format!("{} → x{scale}", class.label()) }
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
            let p = &mut plans[big];
            p.scale /= 2;
            p.reason = format!("{} (reduced by archive budget)", p.reason);
            if p.scale <= 1 {
                *p = Plan::native("archive memory budget exhausted");
            }
        }
    }
    plans
}
