//! Runtime texture packs: the key the recomp looks replacements up by, and the DDS files it loads.
//!
//! A texture is identified by (GX2 format, width, height, XXH3-64 of its level-0 guest bytes). The recomp hashes
//! the same bytes at run time, so `level0_bytes` must slice exactly what the game uploads.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use image_dds::{ImageFormat, Mipmaps, Quality, SurfaceRgba8};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use crate::classify::Class;
use crate::container::LeafKind;
use crate::error::{bail, Error, Result};
use crate::gx2::{self, Gx2Surface};
use crate::inventory::{Inventory, TextureRecord};
use crate::jobs::Progress;
use crate::mips;
use crate::pipeline;
use crate::policy::{self, Decisions, ReviewStatus};
use crate::project::{write_atomic, Project, Settings};
use crate::texture;

/// XXH3-64 (seed 0) of a texture's level-0 guest bytes: the hash part of the key.
pub fn key_hash(level0: &[u8]) -> u64 {
    xxh3_64(level0)
}

/// The level-0 guest bytes of a texture: the first `image_size` bytes of its image data (never longer than `data`).
pub fn level0_bytes<'a>(data: &'a [u8], surface: &Gx2Surface) -> &'a [u8] {
    &data[..(surface.image_size as usize).min(data.len())]
}

/// The DDS format of a replacement; it keeps the original's sRGB-ness or signedness so the guest's sampling applies
/// unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackFormat {
    Bc7 { srgb: bool },
    Bc4 { snorm: bool },
    Bc5 { snorm: bool },
    Rgba8 { srgb: bool },
}

impl PackFormat {
    /// The DXGI_FORMAT value written into the DX10 header.
    pub fn dxgi(&self) -> u32 {
        match *self {
            PackFormat::Bc7 { srgb } => 98 + srgb as u32,
            PackFormat::Bc4 { snorm } => 80 + snorm as u32,
            PackFormat::Bc5 { snorm } => 83 + snorm as u32,
            PackFormat::Rgba8 { srgb } => 28 + srgb as u32,
        }
    }

    /// Bytes of the level data of a `width` x `height` image with `levels` levels in this format (no DDS header):
    /// what `write_dds` appends after `DDS_HEADER_BYTES`.
    pub fn data_bytes(&self, width: u32, height: u32, levels: u32) -> u64 {
        (0..levels.max(1))
            .map(|l| {
                let (w, h) = ((width >> l).max(1) as u64, (height >> l).max(1) as u64);
                match self.block_bytes() {
                    Some(b) => w.div_ceil(4) * h.div_ceil(4) * b as u64,
                    None => w * h * 4,
                }
            })
            .sum()
    }

    fn block_bytes(&self) -> Option<usize> {
        match self {
            PackFormat::Bc4 { .. } => Some(8),
            PackFormat::Bc7 { .. } | PackFormat::Bc5 { .. } => Some(16),
            PackFormat::Rgba8 { .. } => None,
        }
    }
}

/// The replacement format for a GX2 surface format, or `None` for a format that gets no entry. BC1/BC2/BC3 become
/// BC7 of the same colour space, BC4/BC5 stay as they are, RGBA8 becomes BC7 (RGBA8 itself for UI and lettering,
/// which stay sharp).
pub fn pack_format(gx2_format: u32, class: Class) -> Option<PackFormat> {
    let plain_rgba = matches!(class, Class::Ui | Class::Lettering);
    Some(match gx2_format {
        0x31..=0x33 => PackFormat::Bc7 { srgb: false },
        0x431..=0x433 => PackFormat::Bc7 { srgb: true },
        0x34 => PackFormat::Bc4 { snorm: false },
        0x234 => PackFormat::Bc4 { snorm: true },
        0x35 => PackFormat::Bc5 { snorm: false },
        0x235 => PackFormat::Bc5 { snorm: true },
        0x1A | 0x41A => {
            let srgb = gx2_format == 0x41A;
            if plain_rgba { PackFormat::Rgba8 { srgb } } else { PackFormat::Bc7 { srgb } }
        }
        _ => return None,
    })
}

/// Encodes one level (RGBA8, `w`x`h`) to the format's blocks, padding to whole 4x4 blocks by edge replication.
fn encode_level(fmt: PackFormat, w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let f = match fmt {
        PackFormat::Rgba8 { .. } => return Ok(rgba.to_vec()),
        PackFormat::Bc7 { srgb: false } => ImageFormat::BC7RgbaUnorm,
        PackFormat::Bc7 { srgb: true } => ImageFormat::BC7RgbaUnormSrgb,
        PackFormat::Bc4 { snorm: false } => ImageFormat::BC4RUnorm,
        PackFormat::Bc4 { snorm: true } => ImageFormat::BC4RSnorm,
        PackFormat::Bc5 { snorm: false } => ImageFormat::BC5RgUnorm,
        PackFormat::Bc5 { snorm: true } => ImageFormat::BC5RgSnorm,
    };
    let (pw, ph) = (w.div_ceil(4) * 4, h.div_ceil(4) * 4);
    let padded;
    let data = if (pw, ph) == (w, h) {
        rgba
    } else {
        let mut p = vec![0u8; (pw * ph * 4) as usize];
        for y in 0..ph {
            for x in 0..pw {
                let si = ((y.min(h - 1) * w + x.min(w - 1)) * 4) as usize;
                let di = ((y * pw + x) * 4) as usize;
                p[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
            }
        }
        padded = p;
        &padded[..]
    };
    let surf = SurfaceRgba8 { width: pw, height: ph, depth: 1, layers: 1, mipmaps: 1, data };
    let enc = surf
        .encode(f, Quality::Slow, Mipmaps::Disabled)
        .map_err(|e| Error::Format(format!("encode: {e}")))?;
    Ok(enc.data)
}

/// The most levels a `width` x `height` image can have: 1 + floor(log2(max side)), the bound the recomp's loader enforces.
pub fn max_levels(width: u32, height: u32) -> u32 {
    32 - width.max(height).max(1).leading_zeros()
}

/// A DDS file (DX10 header, one 2D image) holding `levels_rgba` encoded as `fmt`, largest level first. Level `i` is
/// `max(1, width >> i)` x `max(1, height >> i)` RGBA8; the level sizes are those the recomp's loader checks.
pub fn write_dds(width: u32, height: u32, fmt: PackFormat, levels_rgba: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mips = levels_rgba.len();
    if width == 0 || height == 0 || mips == 0 || mips > 15 || mips as u32 > max_levels(width, height) {
        bail!(
            "a DDS needs a non-empty size and 1..={} levels (got {width}x{height}, {mips} levels)",
            max_levels(width, height).min(15)
        );
    }
    let dims = |i: usize| ((width >> i).max(1), (height >> i).max(1));
    let mut body = Vec::new();
    for (i, rgba) in levels_rgba.iter().enumerate() {
        let (w, h) = dims(i);
        if rgba.len() as u64 != w as u64 * h as u64 * 4 {
            bail!("level {i} has {} bytes, expected {w}x{h} RGBA8", rgba.len());
        }
        body.extend_from_slice(&encode_level(fmt, w, h, rgba)?);
    }
    let (w0, h0) = dims(0);
    let linear = match fmt.block_bytes() {
        Some(b) => w0.div_ceil(4) as u64 * h0.div_ceil(4) as u64 * b as u64,
        None => w0 as u64 * h0 as u64 * 4,
    };
    let mut out = Vec::with_capacity(148 + body.len());
    let mut put = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    put(0x2053_4444); // "DDS "
    put(124);
    put(0x1 | 0x2 | 0x4 | 0x1000 | 0x2_0000 | 0x8_0000); // CAPS HEIGHT WIDTH PIXELFORMAT MIPMAPCOUNT LINEARSIZE
    put(height);
    put(width);
    put(linear as u32);
    put(0); // depth
    put(mips as u32);
    for _ in 0..11 {
        put(0);
    }
    put(32); // DDS_PIXELFORMAT: size, FOURCC, "DX10"
    put(4);
    put(0x3031_5844);
    for _ in 0..5 {
        put(0);
    }
    put(0x1000 | if mips > 1 { 0x8 | 0x40_0000 } else { 0 }); // caps: TEXTURE (+ COMPLEX, MIPMAP)
    for _ in 0..4 {
        put(0); // caps2-4, reserved
    }
    put(fmt.dxgi());
    put(3); // D3D10_RESOURCE_DIMENSION_TEXTURE2D
    put(0); // misc flags
    put(1); // array size
    put(0); // misc flags 2
    out.extend_from_slice(&body);
    Ok(out)
}

/// Package ids written by the runtime export start with this (the rebuilt-archive packages' ids end in a number
/// straight after `pipeline::PACKAGE_PREFIX`).
pub const RUNTIME_PREFIX: &str = "wwhd-texture-studio-tex-";

/// The largest a runtime package may grow.
pub const PACKAGE_MAX_BYTES: u64 = 4 << 30;

/// The largest index.json a runtime package may have: the recomp reads it with its JSON reader, which took at most
/// 1 MiB before 2026-11-03 (a larger index made the whole package fail to load). Packages are split to stay under it,
/// so they load in every build.
pub const INDEX_MAX_BYTES: u64 = 1 << 20;

/// Room kept in each index for its header (format_version, generator) and the brackets around the entries.
const INDEX_HEADER_BYTES: u64 = 4 << 10;

/// Above this estimate of resident texture memory the export warns.
pub const VRAM_WARN_BYTES: u64 = 8 << 30;

/// The largest max side the runtime pack is safe at in VR: the recomp makes a replacement in one frame when the game
/// first draws it, and one larger than this can cost that frame.
pub const VR_MAX_SIDE: u32 = 4096;

/// The recomp's mod manager version that knows `kind: "textures"` packages.
pub const MINIMUM_MANAGER_VERSION: &str = "1.7.0";

/// Bytes before the level data of a DDS written by `write_dds`.
pub const DDS_HEADER_BYTES: u64 = 148;

/// Whether `id` names a package of the runtime export.
pub fn is_runtime_id(id: &str) -> bool {
    id.starts_with(RUNTIME_PREFIX)
}

/// A texture the export left out, and why.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeReport {
    /// Entries written: one per unique key.
    pub entries: usize,
    /// Textures whose key was already taken by an earlier one with the same level-0 bytes (they share its entry).
    pub shared: usize,
    /// Unique textures the policy keeps at native resolution (nothing to replace).
    pub native: usize,
    /// (package id, bytes, entries)
    pub packages: Vec<(String, u64, usize)>,
    /// DDS bytes written.
    pub bytes: u64,
    /// Texture memory if every entry were resident: the level data of all DDS files.
    pub vram_estimate_bytes: u64,
    pub skipped: Vec<Skipped>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// The warning for an estimated resident size, or `None` while it is within 8 GiB.
pub fn vram_warning(bytes: u64) -> Option<String> {
    (bytes > VRAM_WARN_BYTES).then(|| {
        format!(
            "estimated texture memory is {:.1} GiB, above 8 GiB; the game keeps only what its budget allows resident",
            bytes as f64 / (1u64 << 30) as f64
        )
    })
}

/// The warning for a runtime pack made at `max_dim` (Settings → Max side), or `None` up to `VR_MAX_SIDE`.
pub fn max_side_warning(max_dim: u32) -> Option<String> {
    (max_dim > VR_MAX_SIDE).then(|| {
        format!(
            "max side is {max_dim} px: a replacement larger than {VR_MAX_SIDE} px is made in one frame when the game first draws it and can cost a VR frame; use max side {VR_MAX_SIDE} for VR until the recomp spreads large uploads over frames"
        )
    })
}

/// A texture chosen for the pack and the upscaled image that goes into it.
struct Candidate {
    record: usize,
    fmt: PackFormat,
    png: PathBuf,
    /// The scale the upscale really reached (it may have stepped down from the plan).
    scale: u32,
}

/// The identity the recomp looks a replacement up by: (GX2 format, width, height, level-0 hash).
type Key = (u32, u32, u32, u64);

/// A DDS in the staging folder.
struct Staged {
    key: Key,
    name: String,
    scale: u32,
    dxgi: u32,
    bytes: u64,
    path: PathBuf,
}

/// The DDS path below the package's `textures` folder: named by the whole key, because textures of different
/// format or size can share level-0 bytes (and so the hash).
fn dds_file(key: &Key) -> String {
    let hash = format!("{:016x}", key.3);
    format!("{}/{hash}_{:x}_{}x{}.dds", &hash[..2], key.0, key.1, key.2)
}

/// "file/inner/leaf/texture" as the index shows it; a BFLIM leaf is its own texture name.
fn entry_name(t: &TextureRecord) -> String {
    let mut parts = vec![t.id.location.file.clone()];
    parts.extend(t.id.location.inner.iter().cloned());
    if t.kind == LeafKind::Bfres {
        parts.push(t.id.name.clone());
    }
    parts.join("/")
}

/// Content key -> the top-level disc files (game paths below `content/`, forward slashes) that hold a texture with
/// it. A texture inside a pack or archive belongs to the file the game opens, which is `location.file`; the nested
/// names in `location.inner` are not files of their own. The recomp prefetches a key when the game opens one of these.
fn archives_by_content_key(inv: &Inventory, s: &Settings) -> HashMap<String, BTreeSet<String>> {
    let mut map: HashMap<String, BTreeSet<String>> = HashMap::new();
    for t in &inv.textures {
        if !s.file_selected(&t.id.location.file) {
            continue;
        }
        if let Some(path) = pipeline::game_path(&inv.root, &t.id.location.file) {
            map.entry(t.key.clone()).or_default().insert(path.replace('\\', "/"));
        }
    }
    map
}

/// Stage "Runtime pack": writes `textures` packages (index.json + DDS per key) from the upscales the pipeline made,
/// with the rules of the archive export for review state and `debug_tint` but none of its guest-heap limits.
pub fn export_runtime(p: &Project, s: &Settings, inv: &Inventory, d: &Decisions, progress: &Progress) -> Result<RuntimeReport> {
    export_runtime_with(p, s, inv, d, progress, PACKAGE_MAX_BYTES, INDEX_MAX_BYTES)
}

/// One index.json entry for `st`.
fn index_entry(st: &Staged, archives: &BTreeMap<Key, BTreeSet<String>>) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "format": st.key.0, "width": st.key.1, "height": st.key.2, "hash": format!("{:016x}", st.key.3),
        "scale": st.scale, "file": dds_file(&st.key), "dxgi": st.dxgi, "name": st.name,
    });
    // Optional: lets the recomp load the replacement when the game opens one of these files.
    if let Some(a) = archives.get(&st.key).filter(|a| !a.is_empty()) {
        entry["archives"] = serde_json::json!(a);
    }
    entry
}

/// Bytes `entry` takes in an index written by `to_vec_pretty`: its own pretty text indented two levels (4 spaces a
/// line) plus the comma and line break after it.
fn index_entry_bytes(entry: &serde_json::Value) -> u64 {
    let text = serde_json::to_vec_pretty(entry).unwrap_or_default();
    let lines = text.iter().filter(|&&b| b == b'\n').count() as u64 + 1;
    text.len() as u64 + 4 * lines + 2
}

/// `export_runtime` with the package size limit as a parameter, so tests can split small exports.
fn export_runtime_with(
    p: &Project,
    s: &Settings,
    inv: &Inventory,
    d: &Decisions,
    progress: &Progress,
    package_max: u64,
    index_max: u64,
) -> Result<RuntimeReport> {
    let plans = policy::plan_all_runtime(inv, s, d);
    let mut report = RuntimeReport::default();

    // Which unique textures get an entry.
    let mut seen = HashSet::new();
    let mut cands: Vec<Candidate> = Vec::new();
    for (i, (t, plan)) in inv.textures.iter().zip(&plans).enumerate() {
        if !s.file_selected(&t.id.location.file) || !seen.insert(t.key.clone()) {
            continue;
        }
        let skip = |report: &mut RuntimeReport, reason: String| report.skipped.push(Skipped { name: entry_name(t), reason });
        if t.surface.dim != gx2::DIM_2D || t.surface.depth > 1 {
            skip(&mut report, format!("not a 2D single-slice texture (dim {}, {} slices)", t.surface.dim, t.surface.depth));
            continue;
        }
        let class = d.classification(t).class;
        let Some(fmt) = pack_format(t.surface.format, class) else {
            skip(&mut report, format!("format {} has no runtime replacement", gx2::format_name(t.surface.format)));
            continue;
        };
        if !plan.upscales() {
            report.native += 1;
            continue;
        }
        if d.review(&t.key).status == ReviewStatus::Pending && !s.export_unreviewed {
            skip(&mut report, "not reviewed".into());
            continue;
        }
        match pipeline::find_upscaled(p, &plan.backend.key(), &t.key, plan.scale) {
            Some((png, scale)) => cands.push(Candidate { record: i, fmt, png, scale }),
            None => skip(&mut report, "not upscaled yet".into()),
        }
    }

    // Keys: the hash of each texture's level-0 guest bytes, read from the disc.
    progress.log(format!("Runtime pack: keying {} textures", cands.len()));
    let hashes: Mutex<HashMap<usize, u64>> = Mutex::new(HashMap::new());
    let records: Vec<usize> = cands.iter().map(|c| c.record).collect();
    pipeline::for_each_leaf(inv, &records, progress, |leaf, idx| {
        for &i in idx {
            let t = &inv.textures[i];
            match texture::raw_data(&pipeline::info_of(t), leaf) {
                Ok((image, _)) => {
                    hashes.lock().unwrap().insert(i, key_hash(level0_bytes(image, &t.surface)));
                }
                Err(e) => progress.fail(format!("{}: {e}", t.id.key())),
            }
        }
    })?;
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    let hashes = hashes.into_inner().unwrap();
    let by_content = archives_by_content_key(inv, s);
    let mut archives: BTreeMap<Key, BTreeSet<String>> = BTreeMap::new();
    let mut unique: BTreeMap<Key, &Candidate> = BTreeMap::new();
    for c in &cands {
        let t = &inv.textures[c.record];
        let Some(&h) = hashes.get(&c.record) else {
            report.skipped.push(Skipped { name: entry_name(t), reason: "could not be read from the disc".into() });
            continue;
        };
        let key = (t.surface.format, t.surface.width, t.surface.height, h);
        if let Some(files) = by_content.get(&t.key) {
            archives.entry(key).or_default().extend(files.iter().cloned());
        }
        match unique.entry(key) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(c);
            }
            std::collections::btree_map::Entry::Occupied(_) => report.shared += 1,
        }
    }

    // Encode every unique texture into the staging folder (cached by content, so re-exports are cheap).
    let root = p.runtime_dir();
    let staging = root.join("_staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    progress.begin("Exporting runtime pack", unique.len());
    let staged: Mutex<Vec<Staged>> = Mutex::new(Vec::new());
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let pool = pipeline::worker_pool()?;
    pool.install(|| {
        unique.par_iter().for_each(|(key, c)| {
            if progress.cancelled() {
                return;
            }
            progress.wait_if_paused();
            let t = &inv.textures[c.record];
            progress.item(&t.id.name);
            match encode_one(p, s, inv, d, c, *key, &staging) {
                Ok(st) => {
                    staged.lock().unwrap().push(st);
                    progress.step();
                }
                Err(e) => {
                    progress.fail(format!("{}: {e}", t.id.key()));
                    errors.lock().unwrap().push(format!("{}: {e}; kept original", t.id.key()));
                }
            }
        })
    });
    if progress.cancelled() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::Cancelled);
    }
    let mut staged = staged.into_inner().unwrap();
    report.errors = errors.into_inner().unwrap();
    report.errors.sort();

    // Split into packages within the size limit and the index limit (first-fit decreasing).
    staged.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.key.cmp(&b.key)));
    let index_room = index_max.saturating_sub(INDEX_HEADER_BYTES);
    let mut bins: Vec<(u64, u64, Vec<Staged>)> = Vec::new();
    for st in staged {
        if st.bytes > package_max {
            report.errors.push(format!("{}: {} bytes exceed the package limit of {package_max} bytes; kept original", st.name, st.bytes));
            continue;
        }
        let ib = index_entry_bytes(&index_entry(&st, &archives));
        match bins.iter_mut().find(|(size, index, _)| size + st.bytes <= package_max && index + ib <= index_room) {
            Some(b) => {
                b.0 += st.bytes;
                b.1 += ib;
                b.2.push(st);
            }
            None => bins.push((st.bytes, ib, vec![st])),
        }
    }
    if bins.is_empty() {
        report.warnings.push("nothing to export; earlier runtime packages were left untouched".into());
    } else if let Ok(rd) = std::fs::read_dir(&root) {
        // Replace the earlier packages of this tool.
        for e in rd.flatten() {
            if is_runtime_id(&e.file_name().to_string_lossy()) {
                std::fs::remove_dir_all(e.path())?;
            }
        }
    }
    let (generator, count) = (format!("ZeldaWWHD Texture Studio {}", env!("CARGO_PKG_VERSION")), bins.len());
    for (k, (size, _, list)) in bins.iter_mut().enumerate() {
        let id = format!("{RUNTIME_PREFIX}{}", k + 1);
        let dir = root.join(&id);
        let mut entries = Vec::new();
        list.sort_by_key(|st| st.key.3);
        for st in list.iter() {
            let dst = dir.join("textures").join(dds_file(&st.key));
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::rename(&st.path, &dst)?;
            entries.push(index_entry(st, &archives));
        }
        write_atomic(
            &dir.join("textures/index.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({"format_version": 1, "generator": generator, "entries": entries}))?,
        )?;
        write_atomic(
            &dir.join("manifest.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "format_version": 1,
                "id": id,
                "name": format!("{} Runtime ({}/{count})", s.package_name, k + 1),
                "version": "1.0.0",
                "game_id": "wwhd-usa",
                "kind": "textures",
                "textures_dir": "textures",
                "minimum_manager_version": MINIMUM_MANAGER_VERSION,
            }))?,
        )?;
        report.packages.push((id, *size, list.len()));
    }
    let _ = std::fs::remove_dir_all(&staging);

    report.entries = report.packages.iter().map(|p| p.2).sum();
    report.bytes = report.packages.iter().map(|p| p.1).sum();
    report.vram_estimate_bytes = report.bytes.saturating_sub(report.entries as u64 * DDS_HEADER_BYTES);
    report.warnings.extend(vram_warning(report.vram_estimate_bytes));
    report.warnings.extend(max_side_warning(s.max_dim));
    write_atomic(&p.reports_dir().join("runtime_export_report.json"), &serde_json::to_vec_pretty(&report)?)?;
    progress.log(format!(
        "Runtime pack finished: {} entries in {} packages ({} MiB, ~{} MiB resident), {} skipped, {} errors",
        report.entries,
        report.packages.len(),
        report.bytes >> 20,
        report.vram_estimate_bytes >> 20,
        report.skipped.len(),
        report.errors.len()
    ));
    Ok(report)
}

/// Builds (or finds in the cache) the DDS of one texture and links it into `staging`.
fn encode_one(p: &Project, s: &Settings, inv: &Inventory, d: &Decisions, c: &Candidate, key: Key, staging: &Path) -> Result<Staged> {
    let t = &inv.textures[c.record];
    let class = d.classification(t).class;
    let png_bytes = std::fs::read(&c.png).map_err(|e| Error::Format(format!("upscaled image missing: {e}")))?;
    // Cached by everything that determines the file, like the archive export's encode cache, plus the output format.
    let mut h = blake3::Hasher::new();
    h.update(b"dds-v1");
    h.update(&png_bytes);
    h.update(t.key.as_bytes());
    h.update(format!("{class:?}{}{}{:?}{:?}", c.scale, s.debug_tint, t.surface, c.fmt).as_bytes());
    let cache = p.encoded_dds_path(&h.finalize().to_hex());
    if !cache.exists() {
        let img = image::load_from_memory(&png_bytes).map_err(|e| Error::Format(format!("{}: {e}", c.png.display())))?.to_rgba8();
        let (w, hh) = (img.width(), img.height());
        let mut rgba = img.into_raw();
        if s.debug_tint {
            for px in rgba.chunks_exact_mut(4) {
                px[1] /= 3;
            }
        }
        if (w, hh) != (t.surface.width * c.scale, t.surface.height * c.scale) {
            bail!("upscaled image is {w}x{hh}, expected x{}", c.scale);
        }
        // Original levels plus one per doubling, never more than the size allows.
        let count = mips::upscaled_mip_count(t.surface.mip_count, c.scale).min(max_levels(w, hh));
        let mode = pipeline::mip_mode(class, t.surface.format, &rgba);
        let levels: Vec<Vec<u8>> = mips::chain(w, hh, rgba, count, mode).into_iter().map(|(_, _, l)| l).collect();
        write_atomic(&cache, &write_dds(w, hh, c.fmt, &levels)?)?;
    }
    let bytes = std::fs::metadata(&cache)?.len();
    let path = staging.join(dds_file(&key));
    std::fs::create_dir_all(path.parent().unwrap())?;
    if std::fs::hard_link(&cache, &path).is_err() {
        // Different volume or no hard-link support: copy instead.
        std::fs::copy(&cache, &path)?;
    }
    Ok(Staged { key, name: entry_name(t), scale: c.scale, dxgi: c.fmt.dxgi(), bytes, path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Class;

    // XXH3-64 (seed 0) of the empty input, from the xxHash reference.
    #[test]
    fn data_bytes_matches_what_write_dds_writes() {
        let levels: Vec<Vec<u8>> = [(8u32, 8u32), (4, 4), (2, 2)].iter().map(|&(w, h)| vec![128u8; (w * h * 4) as usize]).collect();
        for fmt in [PackFormat::Bc7 { srgb: true }, PackFormat::Bc4 { snorm: false }, PackFormat::Bc5 { snorm: false }, PackFormat::Rgba8 { srgb: false }] {
            let dds = write_dds(8, 8, fmt, &levels).unwrap();
            assert_eq!(dds.len() as u64, DDS_HEADER_BYTES + fmt.data_bytes(8, 8, 3), "{fmt:?}");
        }
    }

    #[test]
    fn empty_hash_is_the_reference_value() {
        assert_eq!(key_hash(&[]), 0x2D06800538D394C2);
    }

    // Pinned on both sides: the recomp's texture_pack test asserts XXH3_64bits("wwhd", 4) equals this value.
    #[test]
    fn wwhd_hash_is_pinned() {
        assert_eq!(key_hash(b"wwhd"), 0x390cfa6e3c090774);
    }

    #[test]
    fn level0_is_the_first_image_size_bytes() {
        let s = crate::gx2::Gx2Surface { image_size: 3, ..Default::default() };
        assert_eq!(level0_bytes(&[1, 2, 3, 4, 5], &s), &[1, 2, 3]);
        // data shorter than the surface: everything there is, never a panic
        assert_eq!(level0_bytes(&[1, 2], &s), &[1, 2]);
    }

    #[test]
    fn dxgi_codes() {
        assert_eq!(PackFormat::Bc7 { srgb: false }.dxgi(), 98);
        assert_eq!(PackFormat::Bc7 { srgb: true }.dxgi(), 99);
        assert_eq!(PackFormat::Bc4 { snorm: false }.dxgi(), 80);
        assert_eq!(PackFormat::Bc4 { snorm: true }.dxgi(), 81);
        assert_eq!(PackFormat::Bc5 { snorm: false }.dxgi(), 83);
        assert_eq!(PackFormat::Bc5 { snorm: true }.dxgi(), 84);
        assert_eq!(PackFormat::Rgba8 { srgb: false }.dxgi(), 28);
        assert_eq!(PackFormat::Rgba8 { srgb: true }.dxgi(), 29);
    }

    #[test]
    fn pack_format_table() {
        use PackFormat::*;
        let c = Class::EnvironmentAlbedo;
        for f in [0x31, 0x32, 0x33] {
            assert_eq!(pack_format(f, c), Some(Bc7 { srgb: false }));
        }
        for f in [0x431, 0x432, 0x433] {
            assert_eq!(pack_format(f, c), Some(Bc7 { srgb: true }));
        }
        assert_eq!(pack_format(0x34, c), Some(Bc4 { snorm: false }));
        assert_eq!(pack_format(0x234, c), Some(Bc4 { snorm: true }));
        assert_eq!(pack_format(0x35, c), Some(Bc5 { snorm: false }));
        assert_eq!(pack_format(0x235, c), Some(Bc5 { snorm: true }));
        assert_eq!(pack_format(0x1A, Class::Ui), Some(Rgba8 { srgb: false }));
        assert_eq!(pack_format(0x1A, Class::Lettering), Some(Rgba8 { srgb: false }));
        assert_eq!(pack_format(0x41A, Class::Ui), Some(Rgba8 { srgb: true }));
        assert_eq!(pack_format(0x1A, c), Some(Bc7 { srgb: false }));
        assert_eq!(pack_format(0x41A, c), Some(Bc7 { srgb: true }));
        // R8, RG8, RGB565, RGBA4, RGB5A1: no entry
        for f in [0x01, 0x19, 0x07, 0x08, 0x0A, 0x0B] {
            assert_eq!(pack_format(f, c), None, "{f:#x}");
        }
    }

    fn rd(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 90, 255]).collect()
    }

    /// Parses the header by hand the way the recomp's check_dds does and returns (w, h, dxgi, mips, data offset).
    fn header(f: &[u8]) -> (u32, u32, u32, u32, usize) {
        assert_eq!(rd(f, 0), 0x20534444);
        assert_eq!((rd(f, 4), rd(f, 76), rd(f, 84)), (124, 32, 0x30315844));
        assert_eq!((rd(f, 132), rd(f, 136), rd(f, 140)), (3, 0, 1));
        (rd(f, 16), rd(f, 12), rd(f, 128), rd(f, 28), 148)
    }

    fn level_bytes(dxgi: u32, w: u32, h: u32) -> usize {
        match dxgi {
            80 | 81 => (w as usize).div_ceil(4) * (h as usize).div_ceil(4) * 8,
            83 | 84 | 98 | 99 => (w as usize).div_ceil(4) * (h as usize).div_ceil(4) * 16,
            28 | 29 => (w * h * 4) as usize,
            _ => panic!(),
        }
    }

    #[test]
    fn dds_headers_and_sizes_match_the_recomp_check() {
        let cases = [
            PackFormat::Bc7 { srgb: false },
            PackFormat::Bc7 { srgb: true },
            PackFormat::Bc4 { snorm: false },
            PackFormat::Bc5 { snorm: true },
            PackFormat::Rgba8 { srgb: true },
        ];
        for fmt in cases {
            let (w, h) = (64u32, 64u32);
            let levels: Vec<Vec<u8>> = (0..7).map(|i| gradient((w >> i).max(1), (h >> i).max(1))).collect();
            let dds = write_dds(w, h, fmt, &levels).unwrap();
            let (rw, rh, dxgi, mips, at) = header(&dds);
            assert_eq!((rw, rh, dxgi, mips), (w, h, fmt.dxgi(), 7), "{fmt:?}");
            let total: usize = (0..7).map(|i| level_bytes(dxgi, (w >> i).max(1), (h >> i).max(1))).sum();
            assert_eq!(dds.len(), at + total, "{fmt:?}");
        }
    }

    #[test]
    fn dds_non_square_non_multiple_of_four() {
        let levels: Vec<Vec<u8>> = [(10u32, 6u32), (5, 3), (2, 1), (1, 1)].iter().map(|&(w, h)| gradient(w, h)).collect();
        let dds = write_dds(10, 6, PackFormat::Bc7 { srgb: false }, &levels).unwrap();
        let (w, h, dxgi, mips, at) = header(&dds);
        assert_eq!((w, h, dxgi, mips), (10, 6, 98, 4));
        let total: usize = [(10, 6), (5, 3), (2, 1), (1, 1)].iter().map(|&(w, h)| level_bytes(98, w, h)).sum();
        assert_eq!(dds.len(), at + total);
    }

    #[test]
    fn dds_levels_decode_back_close_to_the_input() {
        let rgba = gradient(32, 32);
        let dds = write_dds(32, 32, PackFormat::Bc7 { srgb: false }, std::slice::from_ref(&rgba)).unwrap();
        let blocks = &dds[148..];
        let surf = image_dds::Surface {
            width: 32, height: 32, depth: 1, layers: 1, mipmaps: 1,
            image_format: image_dds::ImageFormat::BC7RgbaUnorm, data: blocks,
        };
        let dec = surf.decode_rgba8().unwrap().data;
        let worst = rgba.iter().zip(&dec).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= 6, "worst channel error {worst}");
    }

    #[test]
    fn rgba8_level_is_stored_verbatim() {
        let rgba = gradient(8, 8);
        let dds = write_dds(8, 8, PackFormat::Rgba8 { srgb: false }, std::slice::from_ref(&rgba)).unwrap();
        assert_eq!(&dds[148..], &rgba[..]);
    }

    #[test]
    fn bad_level_lists_are_errors() {
        let f = PackFormat::Bc7 { srgb: false };
        assert!(write_dds(8, 8, f, &[]).is_err());
        assert!(write_dds(8, 8, f, &[vec![0; 10]]).is_err());
        assert!(write_dds(0, 8, f, &[vec![]]).is_err());
        let sixteen: Vec<Vec<u8>> = (0..16).map(|_| vec![0; 4]).collect();
        assert!(write_dds(1, 1, f, &sixteen).is_err());
    }

    // The recomp's check_dds allows at most 1 + floor(log2(max(w, h))) levels.
    #[test]
    fn level_count_is_bounded_by_the_size() {
        let f = PackFormat::Bc7 { srgb: false };
        let levels = |n: usize| -> Vec<Vec<u8>> { (0..n).map(|i| gradient((4u32 >> i).max(1), (2u32 >> i).max(1))).collect() };
        assert!(write_dds(4, 2, f, &levels(3)).is_ok(), "4x2 has 3 levels: 4x2, 2x1, 1x1");
        assert!(write_dds(4, 2, f, &levels(4)).is_err(), "a fourth level is more than the recomp accepts");
        let one: Vec<Vec<u8>> = vec![gradient(1, 1); 2];
        assert!(write_dds(1, 1, f, &one).is_err());
        assert_eq!(max_levels(1, 1), 1);
        assert_eq!(max_levels(4096, 512), 13);
        assert_eq!(max_levels(16384, 16384), 15);
    }

    // ---- export_runtime on a small synthetic disc of BFLIM files ----

    use crate::classify::Classification;
    use crate::container::{AssetLocation, LeafKind};
    use crate::inventory::{Inventory, TextureRecord};
    use crate::jobs::Progress;
    use crate::policy::Decisions;
    use crate::project::{save_png, Project, Settings};
    use crate::resample::Filter;
    use crate::texture::TextureId;
    use crate::upscale::BackendChoice;

    /// A BFLIM leaf: `image` followed by the 0x28-byte FLIM footer with its `imag` block.
    fn bflim(w: u16, h: u16, flim_format: u8, image: &[u8]) -> Vec<u8> {
        let mut foot = vec![0u8; 0x28];
        foot[0..4].copy_from_slice(b"FLIM");
        foot[4..6].copy_from_slice(&0xFEFFu16.to_be_bytes());
        foot[0x0C..0x10].copy_from_slice(&((image.len() + 0x28) as u32).to_be_bytes());
        foot[0x14..0x18].copy_from_slice(b"imag");
        foot[0x1C..0x1E].copy_from_slice(&w.to_be_bytes());
        foot[0x1E..0x20].copy_from_slice(&h.to_be_bytes());
        foot[0x22] = flim_format;
        foot[0x24..0x28].copy_from_slice(&(image.len() as u32).to_be_bytes());
        [image, &foot].concat()
    }

    struct Rig {
        root: std::path::PathBuf,
        p: Project,
        s: Settings,
        inv: Inventory,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn backend_key() -> String {
        BackendChoice::Filter(Filter::Lanczos3).key()
    }

    impl Rig {
        fn new(name: &str) -> Rig {
            let root = std::env::temp_dir().join(format!("wwhd-runtime-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("content")).unwrap();
            let p = Project::open(root.join("project")).unwrap();
            let s = Settings { scale: 2, default_backend: BackendChoice::Filter(Filter::Lanczos3), backends: vec![], ..Settings::default() };
            let inv = Inventory { root: root.join("content").to_string_lossy().into_owned(), ..Default::default() };
            Rig { root, p, s, inv }
        }
        /// Writes `<file>` to the disc, adds its record under content key `key` and (when `upscaled`) its x2 PNG.
        fn add(&mut self, file: &str, key: &str, class: Class, w: u16, flim_format: u8, image: &[u8], upscaled: bool) {
            self.add_shaped(file, key, class, (w, w), flim_format, image, upscaled)
        }
        #[allow(clippy::too_many_arguments)]
        fn add_shaped(&mut self, file: &str, key: &str, class: Class, (w, h): (u16, u16), flim_format: u8, image: &[u8], upscaled: bool) {
            let leaf = bflim(w, h, flim_format, image);
            let path = self.root.join("content").join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &leaf).unwrap();
            let f = crate::bflim::Bflim::parse(&leaf).unwrap();
            let key = key.repeat(8);
            self.inv.textures.push(TextureRecord {
                id: TextureId { location: AssetLocation { file: file.into(), inner: vec![] }, index: 0, name: file.into() },
                kind: LeafKind::Bflim,
                surface: f.surface,
                comp_sel: [0, 1, 2, 3],
                data_hash: key.clone(),
                key: key.clone(),
                gate_ok: true,
                gate_detail: String::new(),
                decodable: true,
                classification: Classification { class, confidence: 1.0, reason: String::new(), manual: false },
            });
            if upscaled {
                let (nw, nh) = (w as u32 * 2, h as u32 * 2);
                save_png(&self.p.upscaled_path(&backend_key(), &key, 2), nw, nh, &gradient(nw, nh)).unwrap();
            }
        }
        fn runtime_dir(&self) -> std::path::PathBuf {
            self.p.root.join("output/runtime")
        }
    }

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn index_of(dir: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.join("textures/index.json")).unwrap()).unwrap()
    }

    #[test]
    fn export_writes_one_entry_per_key_and_lists_what_it_skipped() {
        let mut rig = Rig::new("export");
        let bc1 = pattern(128, 1); // 16x16 BC1
        let rgba = pattern(16 * 16 * 4, 7); // 16x16 RGBA8, a UI texture
        let r8 = pattern(16 * 16, 3); // 16x16 R8: no pack format
        rig.add("a.bflim", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        // the same texture in a second archive: same content key, one entry
        rig.add("b.bflim", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        // another content key with identical level-0 bytes: the runtime key is the same, still one entry
        rig.add("c.bflim", "cc", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        rig.add("ui.bflim", "dd", Class::Ui, 16, 0x09, &rgba, true);
        rig.add("r8.bflim", "ee", Class::Unknown, 16, 0x00, &r8, true);

        let r = export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        assert_eq!(r.entries, 2, "{r:?}");
        assert_eq!(r.shared, 1, "c.bflim collapses onto a.bflim's key");
        assert_eq!(r.skipped.len(), 1, "{:?}", r.skipped);
        assert_eq!(r.skipped[0].name, "r8.bflim");
        assert!(r.skipped[0].reason.contains("R8"), "{:?}", r.skipped[0]);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.packages.len(), 1);
        let id = "wwhd-texture-studio-tex-1";
        assert_eq!(r.packages[0].0, id);

        let dir = rig.runtime_dir().join(id);
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest,
            serde_json::json!({"format_version": 1, "id": id, "name": "WWHD 4K Textures Runtime (1/1)", "version": "1.0.0",
                "game_id": "wwhd-usa", "kind": "textures", "textures_dir": "textures", "minimum_manager_version": "1.7.0"})
        );
        let index = index_of(&dir);
        assert_eq!(index["format_version"], 1);
        assert_eq!(index["generator"], format!("ZeldaWWHD Texture Studio {}", env!("CARGO_PKG_VERSION")));
        let entries = index["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let mut total = 0u64;
        for e in entries {
            let hash = e["hash"].as_str().unwrap();
            assert!(hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "{hash}");
            let file = e["file"].as_str().unwrap();
            assert_eq!(file, format!("{}/{hash}_{:x}_16x16.dds", &hash[..2], e["format"].as_u64().unwrap()));
            assert!(!file.contains('\\') && !file.contains(".."));
            let bytes = std::fs::read(dir.join("textures").join(file)).unwrap();
            total += bytes.len() as u64;
            assert_eq!((e["width"].as_u64(), e["height"].as_u64(), e["scale"].as_u64()), (Some(16), Some(16), Some(2)));
            // DDS: 32x32, one level (a BFLIM has no stored mips), the entry's dxgi
            assert_eq!((rd(&bytes, 16), rd(&bytes, 12), rd(&bytes, 28)), (32, 32, 1));
            assert_eq!(rd(&bytes, 128) as u64, e["dxgi"].as_u64().unwrap());
        }
        let by_name = |n: &str| entries.iter().find(|e| e["name"] == n).unwrap_or_else(|| panic!("no entry named {n}"));
        let (bc, ui) = (by_name("a.bflim"), by_name("ui.bflim"));
        assert_eq!((bc["format"].as_u64(), bc["dxgi"].as_u64()), (Some(0x31), Some(98)));
        assert_eq!((ui["format"].as_u64(), ui["dxgi"].as_u64()), (Some(0x1A), Some(28)), "UI stays RGBA8");
        // the key is the hash of the guest's level-0 bytes
        assert_eq!(bc["hash"], format!("{:016x}", key_hash(&bc1)));
        assert_eq!(ui["hash"], format!("{:016x}", key_hash(&rgba)));
        assert_eq!(r.bytes, total);
        assert_eq!(r.vram_estimate_bytes, total - 2 * 148, "the estimate is the level data");
        assert!(r.warnings.is_empty());
        assert!(!rig.runtime_dir().join("_staging").exists(), "staging is cleaned up");
        // a second run reuses the cache and gives the same result
        let again = export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        assert_eq!((again.entries, again.bytes), (r.entries, r.bytes));
        assert_eq!(index_of(&dir), index);
        // at a max side above 4096 the export warns (a large first-sight replacement can cost a VR frame)
        let mut wide = rig.s.clone();
        wide.max_dim = 8192;
        let w = export_runtime(&rig.p, &wide, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        assert!(w.warnings.iter().any(|w| w.contains("max side 4096 for VR")), "{:?}", w.warnings);
    }

    #[test]
    fn index_entries_list_every_archive_the_key_comes_from_sorted_and_deduplicated() {
        let mut rig = Rig::new("archives");
        let bc1 = pattern(128, 1);
        rig.add("Common/Stage/sea_Room13.szs", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        rig.add("Common/Stage/A_Room1.szs", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        // a second record of the first archive and a different content key with the same level-0 bytes
        rig.add("Common/Stage/sea_Room13.szs", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        rig.add("Common/Stage/B_Room2.szs", "cc", Class::EnvironmentAlbedo, 16, 0x0C, &bc1, true);
        export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        let index = index_of(&rig.runtime_dir().join("wwhd-texture-studio-tex-1"));
        let entries = index["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["archives"],
            serde_json::json!(["Common/Stage/A_Room1.szs", "Common/Stage/B_Room2.szs", "Common/Stage/sea_Room13.szs"])
        );
    }

    #[test]
    fn a_texture_inside_a_pack_lists_the_pack_and_content_prefixes_are_stripped() {
        let rec = |file: &str, inner: &[&str], key: &str| TextureRecord {
            id: TextureId { location: AssetLocation { file: file.into(), inner: inner.iter().map(|s| s.to_string()).collect() }, index: 0, name: "t".into() },
            kind: LeafKind::Bflim,
            surface: Default::default(),
            comp_sel: [0, 1, 2, 3],
            data_hash: key.into(),
            key: key.into(),
            gate_ok: true,
            gate_detail: String::new(),
            decodable: true,
            classification: Classification { class: Class::Ui, confidence: 1.0, reason: String::new(), manual: false },
        };
        let inv = Inventory {
            root: "D:/game".into(),
            textures: vec![
                rec("content/Common/Pack/szs_permanent1.pack", &["sea_Room13.szs", "x.bflim"], "k1"),
                rec("content/Common/Stage/sea_Room13.szs", &[], "k1"),
                rec("Other/not_under_content.szs", &[], "k1"),
            ],
            ..Default::default()
        };
        let map = archives_by_content_key(&inv, &Settings::default());
        let got: Vec<&str> = map["k1"].iter().map(|s| s.as_str()).collect();
        assert_eq!(got, ["Common/Pack/szs_permanent1.pack", "Common/Stage/sea_Room13.szs"]);
    }

    #[test]
    fn debug_tint_changes_the_written_texture() {
        let mut rig = Rig::new("tint");
        rig.add("a.bflim", "aa", Class::Ui, 16, 0x09, &pattern(1024, 5), true);
        let read = |rig: &Rig| {
            let dir = rig.runtime_dir().join("wwhd-texture-studio-tex-1");
            let e = &index_of(&dir)["entries"][0];
            std::fs::read(dir.join("textures").join(e["file"].as_str().unwrap())).unwrap()
        };
        export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        let plain = read(&rig);
        rig.s.debug_tint = true;
        export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        let tinted = read(&rig);
        assert_eq!(plain.len(), tinted.len());
        // the last row of the gradient has a large green value
        let at = 148 + (31 * 32) * 4 + 1;
        assert!(plain[at] > 200, "{}", plain[at]);
        assert_eq!(tinted[at], plain[at] / 3, "green is divided by three like the archive export");
    }

    #[test]
    fn packages_split_at_the_limit_and_no_key_is_in_two() {
        let mut rig = Rig::new("split");
        rig.add("a.bflim", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &pattern(128, 1), true);
        rig.add("ui.bflim", "dd", Class::Ui, 16, 0x09, &pattern(1024, 7), true);
        // BC7 32x32 = 1024 + 148 bytes, RGBA8 32x32 = 4096 + 148: together they exceed 4300
        let r = export_runtime_with(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default(), 4300, INDEX_MAX_BYTES).unwrap();
        assert_eq!((r.entries, r.packages.len()), (2, 2), "{r:?}");
        let mut hashes = Vec::new();
        for (k, (id, size, n)) in r.packages.iter().enumerate() {
            assert_eq!((id.clone(), *n), (format!("wwhd-texture-studio-tex-{}", k + 1), 1));
            assert!(*size <= 4300);
            let dir = rig.runtime_dir().join(id);
            let index = index_of(&dir);
            for e in index["entries"].as_array().unwrap() {
                assert!(dir.join("textures").join(e["file"].as_str().unwrap()).exists());
                hashes.push(e["hash"].as_str().unwrap().to_string());
            }
            let m: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(m["name"], format!("WWHD 4K Textures Runtime ({}/2)", k + 1));
        }
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), 2, "no key in both packages");
        // exporting again with a roomier limit replaces the old packages instead of leaving the second behind
        let r = export_runtime_with(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default(), 1 << 20, INDEX_MAX_BYTES).unwrap();
        assert_eq!(r.packages.len(), 1);
        assert!(!rig.runtime_dir().join("wwhd-texture-studio-tex-2").exists());
    }

    #[test]
    fn packages_split_to_keep_index_json_under_the_index_limit() {
        let mut rig = Rig::new("indexsplit");
        rig.add("a.bflim", "aa", Class::EnvironmentAlbedo, 16, 0x0C, &pattern(128, 1), true);
        rig.add("ui.bflim", "dd", Class::Ui, 16, 0x09, &pattern(1024, 7), true);
        // one index: its size is the header plus the estimates of its entries
        let r = export_runtime_with(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default(), 1 << 20, INDEX_MAX_BYTES).unwrap();
        assert_eq!(r.packages.len(), 1);
        let path = rig.runtime_dir().join(&r.packages[0].0).join("textures/index.json");
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let estimate: u64 = index["entries"].as_array().unwrap().iter().map(index_entry_bytes).sum();
        let written = std::fs::metadata(&path).unwrap().len();
        assert!(written >= estimate - 2 && written <= estimate + INDEX_HEADER_BYTES, "{written} vs {estimate}");
        // room for the header and less than both entries: one entry a package, each index under the limit
        let index_max = INDEX_HEADER_BYTES + estimate - 1;
        let r = export_runtime_with(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default(), 1 << 20, index_max).unwrap();
        assert_eq!((r.entries, r.packages.len()), (2, 2), "{r:?}");
        for (id, _, n) in &r.packages {
            assert_eq!(*n, 1);
            assert!(std::fs::metadata(rig.runtime_dir().join(id).join("textures/index.json")).unwrap().len() <= index_max);
        }
    }

    #[test]
    fn a_texture_bigger_than_the_package_limit_is_an_error_not_a_package() {
        let mut rig = Rig::new("toobig");
        rig.add("ui.bflim", "dd", Class::Ui, 16, 0x09, &pattern(1024, 7), true);
        let r = export_runtime_with(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default(), 1000, INDEX_MAX_BYTES).unwrap();
        assert_eq!((r.entries, r.packages.len()), (0, 0));
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert!(r.errors[0].contains("package limit"), "{:?}", r.errors);
    }

    #[test]
    fn textures_not_upscaled_or_not_2d_are_reported_and_the_rest_still_export() {
        let mut rig = Rig::new("skips");
        rig.add("a.bflim", "aa", Class::Ui, 16, 0x09, &pattern(1024, 1), true);
        rig.add("todo.bflim", "bb", Class::Ui, 16, 0x09, &pattern(1024, 2), false);
        rig.add("cube.bflim", "cc", Class::Ui, 16, 0x09, &pattern(1024, 3), true);
        rig.inv.textures[2].surface.dim = 3;
        rig.inv.textures[2].surface.depth = 6;
        let r = export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        assert_eq!(r.entries, 1);
        let reason = |n: &str| r.skipped.iter().find(|s| s.name == n).unwrap_or_else(|| panic!("{n} not skipped")).reason.clone();
        assert!(reason("todo.bflim").contains("not upscaled"), "{}", reason("todo.bflim"));
        assert!(reason("cube.bflim").contains("2D"), "{}", reason("cube.bflim"));
    }

    #[test]
    fn keys_sharing_level0_bytes_but_not_shape_get_their_own_files() {
        let mut rig = Rig::new("alias");
        let bc1 = pattern(128, 9); // 16x16 and 32x8 BC1 are both 128 bytes
        rig.add_shaped("a.bflim", "aa", Class::EnvironmentAlbedo, (16, 16), 0x0C, &bc1, true);
        rig.add_shaped("b.bflim", "bb", Class::EnvironmentAlbedo, (32, 8), 0x0C, &bc1, true);
        let r = export_runtime(&rig.p, &rig.s, &rig.inv, &Decisions::default(), &Progress::default()).unwrap();
        assert_eq!((r.entries, r.shared), (2, 0), "{r:?}");
        let dir = rig.runtime_dir().join("wwhd-texture-studio-tex-1");
        let index = index_of(&dir);
        let entries = index["entries"].as_array().unwrap();
        let files: HashSet<&str> = entries.iter().map(|e| e["file"].as_str().unwrap()).collect();
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!(entries[0]["hash"], entries[1]["hash"]);
        for e in entries {
            let (w, h) = (e["width"].as_u64().unwrap() as u32, e["height"].as_u64().unwrap() as u32);
            let bytes = std::fs::read(dir.join("textures").join(e["file"].as_str().unwrap())).unwrap();
            assert_eq!((rd(&bytes, 16), rd(&bytes, 12)), (w * 2, h * 2), "{e}");
        }
    }

    #[test]
    fn the_max_side_warning_starts_above_4096() {
        assert_eq!(max_side_warning(2048), None);
        assert_eq!(max_side_warning(4096), None);
        for side in [8192, 16384] {
            let w = max_side_warning(side).unwrap();
            assert!(w.contains(&format!("{side} px")) && w.contains("max side 4096 for VR"), "{w}");
        }
    }

    #[test]
    fn the_vram_warning_starts_above_eight_gib() {
        assert_eq!(vram_warning(8 << 30), None);
        let w = vram_warning((8 << 30) + 1).unwrap();
        assert!(w.contains("8 GiB"), "{w}");
    }
}
