//! Headless commands. The GUI drives the same library calls.

use std::path::{Path, PathBuf};

use wwhd_tex::inventory::{self, Inventory};
use wwhd_tex::jobs::Progress;
use wwhd_tex::pipeline;
use wwhd_tex::policy::{self, Decisions};
use wwhd_tex::project::{Project, ScanScope, Settings};
use wwhd_tex::resample::{self, Edge, Filter};
use wwhd_tex::upscale::{self, BackendChoice};
use wwhd_tex::Result;

const USAGE: &str = "\
texture_studio [--headless <command> [options]]

Without arguments the GUI starts. Commands:
  scan     --project DIR --disc DIR [--scope content|all|PATH,PATH]
  extract  --project DIR
  upscale  --project DIR [--only TEXT,TEXT]
  export   --project DIR [--only TEXT,TEXT]
  run      --project DIR [--disc DIR] [--only TEXT,TEXT]   scan (if needed), extract, upscale, export
  compare  --project DIR --names TEXT,TEXT [--backends all|KEY,KEY] [--crop N]
  install  --project DIR --mods-root DIR [--no-enable]      copy packages into a ModManager folder
  stats    --project DIR
";

struct Args(Vec<String>);

impl Args {
    fn get(&self, name: &str) -> Option<String> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).cloned()
    }
    fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
    fn list(&self, name: &str) -> Vec<String> {
        self.get(name).map(|v| v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()
    }
}

pub fn main(args: Vec<String>) -> i32 {
    let Some(cmd) = args.first().cloned() else {
        eprint!("{USAGE}");
        return 2;
    };
    let a = Args(args[1..].to_vec());
    match run(&cmd, &a) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn console_progress(p: &Project) -> Progress {
    let prog = Progress::with_log_file(p.log_path());
    let q = prog.clone();
    std::thread::spawn(move || {
        let mut seen = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let (n, lines) = q.log_lines(seen);
            seen = n;
            for l in lines {
                println!("{l}");
            }
            let s = q.snapshot();
            if s.total > 0 && s.running {
                println!("  {} {}/{} ({} failed) {}", s.stage, s.done, s.total, s.failed, s.current);
            }
        }
    });
    prog
}

fn load_inventory(p: &Project) -> Result<Inventory> {
    Inventory::load(&p.inventory_path())
        .map_err(|e| wwhd_tex::Error::Format(format!("no inventory yet ({e}); run scan first")))
}

fn run(cmd: &str, a: &Args) -> Result<()> {
    let project = Project::open(a.get("--project").unwrap_or_else(|| "studio_project".into()))?;
    let mut s = project.load_settings();
    if let Some(d) = a.get("--disc") {
        s.disc_root = PathBuf::from(d);
    }
    if let Some(sc) = a.get("--scope") {
        s.scope = match sc.as_str() {
            "content" => ScanScope::ContentOnly,
            "all" => ScanScope::All,
            other => ScanScope::Selected(other.split(',').map(String::from).collect()),
        };
    }
    if a.get("--only").is_some() {
        s.only_files = a.list("--only");
    }
    if a.flag("--debug-tint") {
        s.debug_tint = true;
    }
    if a.flag("--no-debug-tint") {
        s.debug_tint = false;
    }
    if let Some(t) = a.get("--tools") {
        s.tools_dir = PathBuf::from(t);
    }
    project.save_settings(&s)?;
    let prog = console_progress(&project);
    prog.set_running(true);
    let d = Decisions::load(&project.review_path(), &project.classes_path());
    match cmd {
        "scan" => {
            let inv = inventory::scan(&s.disc_root, &s.scope, &prog)?;
            inv.save(&project.inventory_path())?;
        }
        "extract" => {
            let mut inv = load_inventory(&project)?;
            pipeline::extract(&project, &mut inv, &prog)?;
            inv.save(&project.inventory_path())?;
        }
        "upscale" => {
            let inv = load_inventory(&project)?;
            let plans = policy::plan_all(&inv, &s, &d);
            let tasks = pipeline::upscale_tasks(&inv, &plans, |i| s.file_selected(&inv.textures[i].id.location.file));
            pipeline::upscale(&project, &s, &inv, &d, &tasks, &prog)?;
        }
        "export" => {
            let inv = load_inventory(&project)?;
            let r = pipeline::export(&project, &s, &inv, &d, &prog)?;
            print_report(&r);
        }
        "run" => {
            let mut inv = match Inventory::load(&project.inventory_path()) {
                Ok(i) if i.root == s.disc_root.to_string_lossy() => i,
                _ => {
                    let i = inventory::scan(&s.disc_root, &s.scope, &prog)?;
                    i.save(&project.inventory_path())?;
                    i
                }
            };
            pipeline::extract(&project, &mut inv, &prog)?;
            inv.save(&project.inventory_path())?;
            let plans = policy::plan_all(&inv, &s, &d);
            let tasks = pipeline::upscale_tasks(&inv, &plans, |i| s.file_selected(&inv.textures[i].id.location.file));
            pipeline::upscale(&project, &s, &inv, &d, &tasks, &prog)?;
            let r = pipeline::export(&project, &s, &inv, &d, &prog)?;
            print_report(&r);
        }
        "compare" => compare(&project, &s, a, &prog)?,
        "install" => {
            let root = a.get("--mods-root").ok_or_else(|| wwhd_tex::Error::Format("--mods-root is required".into()))?;
            let ids = install(&project, Path::new(&root), !a.flag("--no-enable"))?;
            println!("installed: {ids:?}");
        }
        "stats" => stats(&project, &s, &d)?,
        _ => {
            eprint!("{USAGE}");
            return Err(wwhd_tex::Error::Format(format!("unknown command {cmd}")));
        }
    }
    prog.set_running(false);
    std::thread::sleep(std::time::Duration::from_millis(1200));
    Ok(())
}

fn print_report(r: &pipeline::ExportReport) {
    println!(
        "Replaced {} textures in {} archives ({} MiB → {} MiB on disc, +{} MiB texture data)",
        r.textures_replaced,
        r.files_rebuilt,
        r.bytes_original >> 20,
        r.bytes_rebuilt >> 20,
        r.guest_growth_bytes >> 20
    );
    for (id, size, n) in &r.packages {
        println!("  package {id}: {n} files, {} MiB", size >> 20);
    }
    for e in r.file_errors.iter().take(30) {
        println!("  ! {e}");
    }
    if r.file_errors.len() > 30 {
        println!("  ... {} more problems in reports/export_report.json", r.file_errors.len() - 30);
    }
}

fn stats(p: &Project, s: &Settings, d: &Decisions) -> Result<()> {
    let inv = load_inventory(p)?;
    let plans = policy::plan_all(&inv, s, d);
    let mut by_class = std::collections::BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for (t, pl) in inv.textures.iter().zip(&plans) {
        if !seen.insert(&t.key) {
            continue;
        }
        let e = by_class.entry(d.classification(t).class).or_insert((0usize, 0usize, 0u64, 0u64));
        e.0 += 1;
        if pl.upscales() {
            e.1 += 1;
        }
        e.2 += policy::surface_bytes(t, 1);
        e.3 += policy::surface_bytes(t, pl.scale);
    }
    println!("{} files, {} texture records, {} unique", inv.files.len(), inv.textures.len(), seen.len());
    println!("class | unique | upscaled | MiB now | MiB after");
    for (c, (n, u, a, b)) in by_class {
        println!("{} | {n} | {u} | {} | {}", c.label(), a >> 20, b >> 20);
    }
    Ok(())
}

/// Upscale chosen textures with many backends and lay the results out as a
/// contact sheet: one row per texture, one column per backend (plus the
/// original, nearest-neighbour enlarged), each a centred crop.
fn compare(p: &Project, s: &Settings, a: &Args, prog: &Progress) -> Result<()> {
    let inv = load_inventory(p)?;
    let names = a.list("--names");
    let crop: u32 = a.get("--crop").and_then(|v| v.parse().ok()).unwrap_or(64);
    let tools = upscale::Tools { dir: s.tools_dir.clone() };
    let backends: Vec<BackendChoice> = match a.get("--backends").as_deref() {
        None | Some("all") => upscale::catalogue().into_iter().filter(|b| *b != BackendChoice::Native && tools.available(b)).collect(),
        Some(list) => upscale::catalogue().into_iter().filter(|b| list.split(',').any(|k| b.key().contains(k))).collect(),
    };
    let mut picked: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, t) in inv.textures.iter().enumerate() {
        if names.iter().any(|n| t.id.name.contains(n.as_str()) || t.id.location.file.contains(n.as_str())) && t.decodable && seen.insert(t.key.clone()) {
            picked.push(i);
        }
    }
    if picked.is_empty() {
        return Err(wwhd_tex::Error::Format("no texture matched --names".into()));
    }
    let scale = s.scale;
    let d = Decisions::default();
    let mut tasks = Vec::new();
    for b in &backends {
        for &i in &picked {
            tasks.push(pipeline::UpscaleTask { key: inv.textures[i].key.clone(), record: i, scale, backend: b.clone() });
        }
    }
    pipeline::upscale(p, s, &inv, &d, &tasks, prog)?;

    let cell = crop * scale;
    let cols = backends.len() as u32 + 1;
    let (sw, sh) = (cols * cell, picked.len() as u32 * cell);
    let mut sheet = vec![40u8; (sw * sh * 4) as usize];
    for (row, &i) in picked.iter().enumerate() {
        let t = &inv.textures[i];
        let (w, h, master) = wwhd_tex::project::load_png(&p.master_path(&t.key))?;
        let (cw, ch) = (crop.min(w), crop.min(h));
        let (x0, y0) = ((w - cw) / 2, (h - ch) / 2);
        let orig = resample::crop_rgba8(w, &master, x0, y0, cw, ch);
        let big = resample::resize_rgba8(cw, ch, &orig, cw * scale, ch * scale, Filter::Nearest, Edge::Clamp);
        blit(&mut sheet, sw, 0, row as u32 * cell, cw * scale, ch * scale, &big);
        for (col, b) in backends.iter().enumerate() {
            let path = p.upscaled_path(&b.key(), &t.key, scale);
            if let Ok((uw, _, up)) = wwhd_tex::project::load_png(&path) {
                let c = resample::crop_rgba8(uw, &up, x0 * scale, y0 * scale, cw * scale, ch * scale);
                blit(&mut sheet, sw, (col as u32 + 1) * cell, row as u32 * cell, cw * scale, ch * scale, &c);
            }
        }
    }
    let out = p.reports_dir().join(format!("compare_x{scale}.png"));
    wwhd_tex::project::save_png(&out, sw, sh, &sheet)?;
    let legend: Vec<String> = std::iter::once("Original (nearest)".to_string()).chain(backends.iter().map(|b| b.label())).collect();
    let rows: Vec<String> = picked.iter().map(|&i| inv.textures[i].id.key()).collect();
    std::fs::write(out.with_extension("txt"), format!("columns:\n{}\n\nrows:\n{}\n", legend.join("\n"), rows.join("\n")))?;
    println!("contact sheet: {}", out.display());
    Ok(())
}

fn blit(dst: &mut [u8], dw: u32, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
    for r in 0..h {
        for c in 0..w {
            let s = ((r * w + c) * 4) as usize;
            let d = (((y + r) * dw + x + c) * 4) as usize;
            // Composite over a checkerboard so transparency is visible.
            let a = src[s + 3] as u32;
            let bg = if ((x + c) / 8 + (y + r) / 8) % 2 == 0 { 90 } else { 60 };
            for k in 0..3 {
                dst[d + k] = ((src[s + k] as u32 * a + bg * (255 - a)) / 255) as u8;
            }
            dst[d + 3] = 255;
        }
    }
}

/// Copy this project's packages into `<mods_root>/Mods/<id>` and, when
/// `enable`, switch them on in the active profile of `<mods_root>/profiles.json`.
pub fn install(p: &Project, mods_root: &Path, enable: bool) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mods = mods_root.join("Mods");
    std::fs::create_dir_all(&mods)?;
    // Remove older installs of this tool's packages first (they may be split differently).
    for e in std::fs::read_dir(&mods)?.flatten() {
        if e.file_name().to_string_lossy().starts_with(pipeline::PACKAGE_PREFIX) {
            std::fs::remove_dir_all(e.path())?;
        }
    }
    for e in std::fs::read_dir(p.packages_dir())?.flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        if !id.starts_with(pipeline::PACKAGE_PREFIX) {
            continue;
        }
        copy_dir(&e.path(), &mods.join(&id))?;
        ids.push(id);
    }
    ids.sort();
    if enable {
        let path = mods_root.join("profiles.json");
        let mut db: serde_json::Value = std::fs::read(&path)
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_else(|| serde_json::json!({"format_version": 1, "active": "Default", "profiles": {"Default": {"enabled": {}}}}));
        let active = db["active"].as_str().unwrap_or("Default").to_string();
        let profile = &mut db["profiles"][&active];
        if !profile["enabled"].is_object() {
            profile["enabled"] = serde_json::json!({});
        }
        let enabled = profile["enabled"].as_object_mut().unwrap();
        enabled.retain(|k, _| !k.starts_with(pipeline::PACKAGE_PREFIX) || ids.contains(k));
        for id in &ids {
            enabled.insert(id.clone(), serde_json::Value::Bool(true));
        }
        wwhd_tex::project::write_atomic(&path, &serde_json::to_vec_pretty(&db)?)?;
    }
    Ok(ids)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else if std::fs::hard_link(&src, &dst).is_err() {
            // Different drive or no hard-link support: copy instead.
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}
