//! `enhance-bench`: the enhancement QA set. For each picked texture: a contact sheet of a detailed crop (original,
//! AI upscale, AI + each stage, each profile, other cached models) and measurements of every variant, plus the time
//! the full export path (enhance, mips, BC encode) takes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use wwhd_tex::classify::{Class, ALL_CLASSES};
use wwhd_tex::enhance::{self, Profile};
use wwhd_tex::pipeline;
use wwhd_tex::policy::Decisions;
use wwhd_tex::project::{load_png, save_png, Project, Settings};
use wwhd_tex::quality::{self, Report};
use wwhd_tex::resample::{self, Edge, Filter};
use wwhd_tex::runtime_pack;
use wwhd_tex::upscale::{self, BackendChoice};
use wwhd_tex::{mips, Result};

use crate::cli::{blit, load_inventory};

/// Peak working set of this process in bytes (0 where unknown).
pub fn peak_memory() -> u64 {
    #[cfg(windows)]
    unsafe {
        #[repr(C)]
        struct Counters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set: usize,
            working_set: usize,
            quota_peak_paged: usize,
            quota_paged: usize,
            quota_peak_nonpaged: usize,
            quota_nonpaged: usize,
            pagefile: usize,
            peak_pagefile: usize,
        }
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(p: isize, c: *mut Counters, cb: u32) -> i32;
        }
        let mut c: Counters = std::mem::zeroed();
        c.cb = std::mem::size_of::<Counters>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
            return c.peak_working_set as u64;
        }
    }
    0
}

/// The crop (`n`×`n` source texels) with the most edge energy, on a half-crop grid.
fn busiest_crop(w: u32, h: u32, rgba: &[u8], n: u32) -> (u32, u32) {
    let (n, step) = (n.min(w).min(h), (n / 2).max(1));
    let l: Vec<f32> = quality::oklab(rgba).iter().zip(rgba.chunks_exact(4)).map(|(v, p)| if p[3] > 0 { v[0] } else { 0.0 }).collect();
    let mut best = (0, 0, -1.0f32);
    let mut y = 0;
    while y + n <= h {
        let mut x = 0;
        while x + n <= w {
            let mut e = 0.0;
            for yy in y..y + n - 1 {
                for xx in x..x + n - 1 {
                    let i = (yy * w + xx) as usize;
                    e += (l[i + 1] - l[i]).abs() + (l[i + w as usize] - l[i]).abs();
                }
            }
            if e > best.2 {
                best = (x, y, e);
            }
            x += step;
        }
        y += step;
    }
    (best.0, best.1)
}

struct Variant {
    label: String,
    rgba: Vec<u8>,
    report: Report,
}

#[allow(clippy::too_many_arguments)]
pub fn run(p: &Project, s: &Settings, d: &Decisions, names: Option<Vec<String>>, per_class: usize, backend: Option<String>, scale: u32, crop: u32, max_side: u32) -> Result<()> {
    let inv = load_inventory(p)?;
    let backend_key = backend.unwrap_or_else(|| s.default_backend.key());
    let main_backend = upscale::catalogue().into_iter().chain(std::iter::once(s.default_backend.clone())).find(|b| b.key() == backend_key).unwrap_or_else(|| s.default_backend.clone());
    let out_dir = p.reports_dir().join("enhance_bench");
    std::fs::create_dir_all(&out_dir)?;

    // Pick: by name, or the largest N textures per class that have an upscale from the backend.
    let mut picked: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    match &names {
        Some(names) => {
            for (i, t) in inv.textures.iter().enumerate() {
                if names.iter().any(|n| t.id.name.contains(n.as_str())) && t.decodable && pipeline::find_upscaled(p, &backend_key, &t.key, scale).is_some() && seen.insert(t.key.clone()) {
                    picked.push(i);
                }
            }
        }
        None => {
            let mut by_class: BTreeMap<Class, Vec<usize>> = BTreeMap::new();
            for (i, t) in inv.textures.iter().enumerate() {
                if t.decodable && enhance::stores_rgb(t.surface.format) && t.surface.width.max(t.surface.height) <= max_side && seen.insert(t.key.clone()) && pipeline::find_upscaled(p, &backend_key, &t.key, scale).is_some() {
                    by_class.entry(d.classification(t).class).or_default().push(i);
                }
            }
            for c in ALL_CLASSES {
                let mut v = by_class.remove(&c).unwrap_or_default();
                v.sort_by_key(|&i| (std::cmp::Reverse(inv.textures[i].surface.width * inv.textures[i].surface.height), inv.textures[i].key.clone()));
                picked.extend(v.into_iter().take(per_class));
            }
        }
    }
    if picked.is_empty() {
        return Err(wwhd_tex::Error::Format(format!("no texture with a {backend_key} ×{scale} upscale matched")));
    }

    let profiles = enhance::builtins();
    let enh = enhance::enhanced();
    let stage_variants: Vec<(String, Profile)> = vec![
        ("AI + sharpen".into(), enhance::only(&enh, true, false, false)),
        ("AI + colour".into(), enhance::only(&enh, false, false, true)),
        ("AI + sharpen + colour".into(), enhance::only(&enh, true, false, true)),
        ("Enhanced (sharpen + detail + colour)".into(), enh.clone()),
        ("Original profile".into(), profiles[0].clone()),
        ("Ultra Sharp profile".into(), profiles[2].clone()),
        ("Ultra Sharp + profile".into(), profiles[3].clone()),
        ("Ultra Sharp & Ultra Vibrant profile".into(), profiles[4].clone()),
        ("Enhanced & Extra Vibrant profile".into(), profiles[5].clone()),
    ];
    let other_models: Vec<BackendChoice> = upscale::catalogue().into_iter().filter(|b| b.is_ai() && b.key() != backend_key).collect();

    let mut md = String::new();
    let _ = writeln!(md, "# Enhancement benchmark\n\nBackend `{}` ×{scale}; crops of {crop}×{crop} source texels (shown ×{scale}).\n", main_backend.label());
    let _ = writeln!(md, "Sharpness = mean Sobel gradient of Oklab L (higher is crisper). Colourfulness = Hasler–Süsstrunk. ΔE and L shift are Oklab ×100 against the AI upscale. Halo = share of texels leaving the AI upscale's 5×5 range by more than 3 levels. Fidelity = the variant reduced to the source size against the source (SSIM of L, PSNR of RGB).\n");
    let mut json = Vec::new();
    let mut total_enhance_ms = 0f32;
    let mut total_encode_ms = 0f32;
    let mut total_mpx = 0f64;

    for &i in &picked {
        let t = &inv.textures[i];
        let cl = d.classification(t);
        let (sw, sh, src) = load_png(&p.master_path(&t.key))?;
        let (png, achieved) = pipeline::find_upscaled(p, &backend_key, &t.key, scale).unwrap();
        let (uw, uh, raw) = load_png(&png)?;
        let target = enhance::Target { class: cl.class, confidence: cl.confidence, manual: cl.manual, format: t.surface.format, scale: achieved, edge: pipeline::edge_of(t), ai: main_backend.is_ai() };
        let category = enhance::category(cl.class, t.surface.format, &raw);
        let mut variants = vec![Variant { label: format!("AI upscale ({})", main_backend.label()), report: quality::report(uw, uh, &raw, &raw, sw, sh, &src, 0.0), rgba: raw.clone() }];
        let mut enhanced_img = None;
        for (label, prof) in &stage_variants {
            let r = enhance::resolve(prof, &s.enhance, &target, &raw);
            let (img, ms, note) = match &r {
                Some(r) => {
                    let (img, st) = enhance::apply(uw, uh, &raw, r, Some((sw, sh, &src)));
                    (img, st.millis, format!(" (adaptive ×{:.2})", st.busyness_factor))
                }
                None => (raw.clone(), 0.0, " (not enhanced)".into()),
            };
            if prof == &enh {
                total_enhance_ms += ms;
                enhanced_img = Some((img.clone(), r.as_ref().is_some_and(|r| r.sharp_mips)));
            }
            variants.push(Variant { label: format!("{label}{note}"), report: quality::report(uw, uh, &raw, &img, sw, sh, &src, ms), rgba: img });
        }
        for b in &other_models {
            if let Some((path, a)) = pipeline::find_upscaled(p, &b.key(), &t.key, scale) {
                if a == achieved {
                    if let Ok((w2, h2, img)) = load_png(&path) {
                        if (w2, h2) == (uw, uh) {
                            variants.push(Variant { label: format!("model: {}", b.label()), report: quality::report(uw, uh, &raw, &img, sw, sh, &src, 0.0), rgba: img });
                        }
                    }
                }
            }
        }

        // The export path for the Enhanced result: mips + the runtime pack's format.
        let mut encode_ms = 0.0;
        if let (Some((img, sharp)), Some(fmt)) = (enhanced_img, runtime_pack::pack_format(t.surface.format, cl.class)) {
            let started = std::time::Instant::now();
            let count = mips::upscaled_mip_count(t.surface.mip_count, achieved).min(runtime_pack::max_levels(uw, uh));
            let mode = pipeline::mip_mode(cl.class, t.surface.format, &img); // colour textures only (stores_rgb)
            let levels: Vec<Vec<u8>> = mips::chain_with(uw, uh, img, count, mode, sharp.then_some(target.edge)).into_iter().map(|l| l.2).collect();
            let _ = runtime_pack::write_dds(uw, uh, fmt, &levels)?;
            encode_ms = started.elapsed().as_secs_f32() * 1000.0;
            total_encode_ms += encode_ms;
        }
        total_mpx += (uw * uh) as f64 / 1e6;

        // Sheet: original (nearest) + every variant, one detailed crop, each cell crop×scale.
        let (cx, cy) = busiest_crop(sw, sh, &src, crop);
        let (cw, ch) = (crop.min(sw), crop.min(sh));
        let cell = (cw * achieved, ch * achieved);
        // Grid of 4 columns: Original (nearest), then the variants in table order.
        let per_row = 4u32;
        let n = variants.len() as u32 + 1;
        let rows = n.div_ceil(per_row);
        let (shw, shh) = (per_row * (cell.0 + 4), rows * (cell.1 + 4));
        let mut sheet = vec![30u8; (shw * shh * 4) as usize];
        let pos = |k: u32| ((k % per_row) * (cell.0 + 4), (k / per_row) * (cell.1 + 4));
        let orig = resample::crop_rgba8(sw, &src, cx, cy, cw, ch);
        let big = resample::resize_rgba8(cw, ch, &orig, cell.0, cell.1, Filter::Nearest, Edge::Clamp);
        blit(&mut sheet, shw, 0, 0, cell.0, cell.1, &big);
        for (k, v) in variants.iter().enumerate() {
            let c = resample::crop_rgba8(uw, &v.rgba, cx * achieved, cy * achieved, cell.0, cell.1);
            let (x, y) = pos(k as u32 + 1);
            blit(&mut sheet, shw, x, y, cell.0, cell.1, &c);
        }
        let file = format!("{}_{}.png", t.id.name.replace(['^', '/', '\\', ' ', '.'], "_"), &t.key[..8]);
        save_png(&out_dir.join(&file), shw, shh, &sheet)?;

        let _ = writeln!(md, "## {} — {} ({:.0}%), category {}\n", t.id.name, cl.class.label(), cl.confidence * 100.0, category.label());
        let _ = writeln!(md, "{}×{} → {}×{}, `{}`; sheet `{file}` (4 per row: Original (nearest), then the rows below in order). Full export path (Enhanced + mips + {}): {encode_ms:.0} ms.\n", sw, sh, uw, uh, wwhd_tex::gx2::format_name(t.surface.format), runtime_pack::pack_format(t.surface.format, cl.class).map(|f| format!("{f:?}")).unwrap_or_default());
        let _ = writeln!(md, "| Variant | Sharpness | Colourful | Chroma | ΔE | L shift | Halo % | Fid. SSIM | Fid. PSNR | Alpha ok | ms |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|:-:|---:|");
        for v in &variants {
            let r = &v.report;
            let _ = writeln!(
                md,
                "| {} | {:.4} | {:.1} | {:.4} | {:.2} | {:+.2} | {:.2} | {:.4} | {:.2} | {} | {:.0} |",
                v.label,
                r.sharpness,
                r.colourfulness,
                r.chroma,
                r.delta_e,
                r.lightness_shift,
                r.halo_fraction * 100.0,
                r.fidelity_ssim,
                r.fidelity_psnr,
                if r.alpha_intact && r.hidden_colour_intact { "yes" } else if r.alpha_intact { "alpha" } else { "NO" },
                r.millis
            );
        }
        let _ = writeln!(md);
        json.push(serde_json::json!({
            "texture": t.id.key(), "class": cl.class, "category": category, "size": [sw, sh], "upscaled": [uw, uh], "encode_ms": encode_ms,
            "variants": variants.iter().map(|v| serde_json::json!({"label": v.label, "report": v.report})).collect::<Vec<_>>(),
        }));
        println!("{}: {} variants, enhance {:.0} ms, encode {encode_ms:.0} ms", t.id.name, variants.len(), variants.iter().map(|v| v.report.millis).fold(0.0, f32::max));
    }
    let _ = writeln!(
        md,
        "## Throughput\n\n{} textures, {:.1} Mpx upscaled. Enhanced profile: {:.0} ms total ({:.1} ms/Mpx). Mips + BC encode: {:.0} ms total ({:.1} ms/Mpx). Peak memory of the benchmark process: {} MiB. Threads: {}.\n",
        picked.len(),
        total_mpx,
        total_enhance_ms,
        total_enhance_ms as f64 / total_mpx.max(1e-9),
        total_encode_ms,
        total_encode_ms as f64 / total_mpx.max(1e-9),
        peak_memory() >> 20,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
    );
    std::fs::write(out_dir.join("report.md"), &md)?;
    std::fs::write(out_dir.join("report.json"), serde_json::to_vec_pretty(&json)?)?;
    println!("report: {}", out_dir.join("report.md").display());
    Ok(())
}
