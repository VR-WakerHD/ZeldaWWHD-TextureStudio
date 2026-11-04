//! Upscaling backends. Conventional filters run in-process; AI models run
//! through the official ncnn/Vulkan executables of each project in folder
//! batches (the model is loaded once per batch). Colour goes to the model with
//! a wrapped or clamped border so tiling textures stay seamless; alpha is
//! always resized separately with a conventional filter, so AI models never
//! create halos in transparency.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::classify::Class;
use crate::error::{bail, Error, Result};
use crate::resample::{self, Edge, Filter};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendChoice {
    /// Keep the original resolution.
    Native,
    Filter(Filter),
    /// Real-ESRGAN (xinntao): realesrgan-x4plus, realesrgan-x4plus-anime, realesr-animevideov3.
    RealEsrgan(String),
    /// waifu2x (nihui ncnn port): models-cunet, models-upconv_7_anime_style_art_rgb, models-upconv_7_photo.
    Waifu2x { model: String, noise: i32 },
    /// Real-CUGAN (bilibili, nihui ncnn port): models-se, models-pro, models-nose.
    RealCugan { model: String, noise: i32 },
    /// RealSR (Tencent, nihui ncnn port): models-DF2K, models-DF2K_JPEG.
    RealSr(String),
    /// A user-supplied ESRGAN-family ncnn model (`<name>.param/.bin` in `custom-models`), fixed 4x.
    Custom(String),
}

impl Default for BackendChoice {
    fn default() -> Self {
        BackendChoice::RealEsrgan("realesrgan-x4plus-anime".into())
    }
}

pub fn default_class_backends() -> Vec<(Class, BackendChoice)> {
    vec![
        (Class::Ui, BackendChoice::Filter(Filter::Lanczos3)),
        (Class::Lettering, BackendChoice::Filter(Filter::Lanczos3)),
        (Class::NormalMap, BackendChoice::Filter(Filter::CatmullRom)),
        (Class::Mask, BackendChoice::Filter(Filter::Mitchell)),
        (Class::Ramp, BackendChoice::Native),
        (Class::Particle, BackendChoice::Filter(Filter::Lanczos3)),
        (Class::Unknown, BackendChoice::Filter(Filter::Lanczos3)),
    ]
}

/// Every backend the tool offers, for menus and comparisons.
pub fn catalogue() -> Vec<BackendChoice> {
    let mut v = vec![BackendChoice::Native];
    v.extend(Filter::ALL.iter().map(|f| BackendChoice::Filter(*f)));
    for m in ["realesrgan-x4plus-anime", "realesrgan-x4plus", "realesr-animevideov3"] {
        v.push(BackendChoice::RealEsrgan(m.into()));
    }
    for (m, n) in [("models-cunet", 0), ("models-cunet", -1), ("models-upconv_7_anime_style_art_rgb", 0), ("models-upconv_7_photo", 0)] {
        v.push(BackendChoice::Waifu2x { model: m.into(), noise: n });
    }
    for (m, n) in [("models-se", -1), ("models-se", 0), ("models-pro", -1), ("models-nose", 0)] {
        v.push(BackendChoice::RealCugan { model: m.into(), noise: n });
    }
    for m in ["models-DF2K", "models-DF2K_JPEG"] {
        v.push(BackendChoice::RealSr(m.into()));
    }
    v
}

impl BackendChoice {
    pub fn label(&self) -> String {
        match self {
            BackendChoice::Native => "Native (no upscale)".into(),
            BackendChoice::Filter(f) => f.label().into(),
            BackendChoice::RealEsrgan(m) => format!("Real-ESRGAN {m}"),
            BackendChoice::Waifu2x { model, noise } => format!("waifu2x {} noise {noise}", model.trim_start_matches("models-")),
            BackendChoice::RealCugan { model, noise } => format!("Real-CUGAN {} noise {noise}", model.trim_start_matches("models-")),
            BackendChoice::RealSr(m) => format!("RealSR {}", m.trim_start_matches("models-")),
            BackendChoice::Custom(m) => format!("Custom ncnn {m}"),
        }
    }
    /// Stable identifier used for cache paths. Includes the tool release so a
    /// changed model never reuses old results.
    pub fn key(&self) -> String {
        let s = match self {
            BackendChoice::Native => "native".into(),
            BackendChoice::Filter(f) => format!("filter-{f:?}-v1"),
            BackendChoice::RealEsrgan(m) => format!("realesrgan-20220424-{m}"),
            BackendChoice::Waifu2x { model, noise } => format!("waifu2x-20250915-{model}-n{noise}"),
            BackendChoice::RealCugan { model, noise } => format!("realcugan-20220728-{model}-n{noise}"),
            BackendChoice::RealSr(m) => format!("realsr-20220728-{m}"),
            BackendChoice::Custom(m) => format!("custom-{m}"),
        };
        s.to_ascii_lowercase().replace(['/', '\\', ' '], "_")
    }
    pub fn is_ai(&self) -> bool {
        !matches!(self, BackendChoice::Native | BackendChoice::Filter(_))
    }
    /// Scale factors the model produces natively, ascending.
    fn native_scales(&self) -> Vec<u32> {
        match self {
            BackendChoice::Native => vec![1],
            BackendChoice::Filter(_) => vec![2, 3, 4, 8],
            BackendChoice::RealEsrgan(m) if m.contains("animevideov3") => vec![2, 3, 4],
            BackendChoice::RealEsrgan(_) | BackendChoice::RealSr(_) | BackendChoice::Custom(_) => vec![4],
            BackendChoice::Waifu2x { .. } => vec![2, 4, 8],
            BackendChoice::RealCugan { model, .. } if model.contains("nose") => vec![2],
            BackendChoice::RealCugan { model, .. } if model.contains("pro") => vec![2, 3],
            BackendChoice::RealCugan { .. } => vec![2, 3, 4],
        }
    }
    /// The passes (run scale, kept scale) that produce `want`. Conventional
    /// filters do it in one step at any scale; so does a wanted scale that is
    /// not a power of two (one run at the nearest native scale).
    pub fn passes(&self, want: u32) -> Vec<(u32, u32)> {
        if !self.is_ai() || !want.is_power_of_two() {
            return vec![(if self.is_ai() { self.run_scale(want) } else { want }, want)];
        }
        let plan = pass_plan(&self.native_scales(), want);
        if plan.is_empty() { vec![(self.run_scale(want), want)] } else { plan }
    }
    /// The scale to run the model at for a wanted scale; the result is then
    /// reduced with Lanczos if larger.
    fn run_scale(&self, want: u32) -> u32 {
        let s = self.native_scales();
        s.iter().copied().find(|&x| x >= want).unwrap_or(*s.last().unwrap())
    }
}

/// Passes that reach `want` (a power of two) with a model whose native scales
/// are `native`: each entry is (run scale, kept scale). Greedy: the largest
/// native power-of-two scale not above what is left, with x4 preferred when the
/// model has it; when the model has nothing that small (a x4-only model for the
/// last x2) it runs the smallest larger scale and keeps a Lanczos-reduced
/// result, e.g. `(4, 2)`.
pub fn pass_plan(native: &[u32], want: u32) -> Vec<(u32, u32)> {
    let pow2: Vec<u32> = native.iter().copied().filter(|n| *n >= 2 && n.is_power_of_two()).collect();
    let mut left = want;
    let mut plan = Vec::new();
    while left >= 2 && !pow2.is_empty() {
        let fits: Vec<u32> = pow2.iter().copied().filter(|&n| n <= left).collect();
        let run = if fits.contains(&4) {
            4
        } else if let Some(&m) = fits.iter().max() {
            m
        } else {
            *pow2.iter().min().unwrap()
        };
        let kept = run.min(left);
        plan.push((run, kept));
        left /= kept;
    }
    plan
}

/// Locations of the ncnn executables and models.
#[derive(Clone, Debug)]
pub struct Tools {
    pub dir: PathBuf,
}

pub(crate) fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            subdirs.push(p);
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Some(p);
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs.sort();
    subdirs.iter().find_map(|d| find_file(d, name, depth - 1))
}

impl Tools {
    pub fn exe(&self, b: &BackendChoice) -> Option<PathBuf> {
        let base = match b {
            BackendChoice::RealEsrgan(_) | BackendChoice::Custom(_) => "realesrgan-ncnn-vulkan",
            BackendChoice::Waifu2x { .. } => "waifu2x-ncnn-vulkan",
            BackendChoice::RealCugan { .. } => "realcugan-ncnn-vulkan",
            BackendChoice::RealSr(_) => "realsr-ncnn-vulkan",
            _ => return None,
        };
        let name = if cfg!(windows) { format!("{base}.exe") } else { base.to_string() };
        find_file(&self.dir, &name, 3)
    }
    pub fn available(&self, b: &BackendChoice) -> bool {
        !b.is_ai() || self.exe(b).is_some()
    }
    pub fn custom_models(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dir.join("custom-models"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? == "param" { Some(p.file_stem()?.to_string_lossy().into_owned()) } else { None }
            })
            .collect();
        v.sort();
        v
    }
}

/// The Vulkan devices the ncnn tools see, as `(-g number, name)` in their order: the device lines an installed tool
/// prints at start (`[0 NVIDIA GeForce RTX 4090]  queueC=...`), read from a run with an input that does not exist (it
/// stops after listing them). Empty without an installed tool or a Vulkan device. Blocks for about a second.
pub fn list_gpus(tools: &Tools) -> Vec<(i32, String)> {
    let probes = [
        BackendChoice::default(),
        BackendChoice::Waifu2x { model: "models-cunet".into(), noise: 0 },
        BackendChoice::RealCugan { model: "models-se".into(), noise: -1 },
        BackendChoice::RealSr("models-DF2K".into()),
    ];
    let Some(exe) = probes.iter().find_map(|b| tools.exe(b)) else { return Vec::new() };
    let missing = std::env::temp_dir().join(format!("wwhd-gpu-probe-{}-missing.png", std::process::id()));
    let mut cmd = Command::new(&exe);
    cmd.current_dir(exe.parent().unwrap_or(Path::new("."))).arg("-i").arg(&missing).arg("-o").arg(missing.with_extension("out.png"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match cmd.stdin(std::process::Stdio::null()).output() {
        Ok(o) => parse_gpu_lines(&String::from_utf8_lossy(&o.stderr)),
        Err(_) => Vec::new(),
    }
}

/// The devices in an ncnn tool's start-up lines (`[<n> <name>]  <fields>`), each once, in order.
fn parse_gpu_lines(text: &str) -> Vec<(i32, String)> {
    let mut out: Vec<(i32, String)> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('[') else { continue };
        let Some((inside, _)) = rest.split_once(']') else { continue };
        let Some((n, name)) = inside.split_once(' ') else { continue };
        let (Ok(n), name) = (n.parse::<i32>(), name.trim()) else { continue };
        if !name.is_empty() && !out.iter().any(|(m, _)| *m == n) {
            out.push((n, name.to_string()));
        }
    }
    out
}

/// One image to upscale.
pub struct Job {
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
    pub scale: u32,
    /// Wrap for repeating 3D textures, Clamp for UI images.
    pub edge: Edge,
}

pub struct Output {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

const PAD: u32 = 16;

/// Upscale a batch. Alpha is resized with Catmull-Rom; colour with the backend.
/// `Settings::gpu`: no `-g`, so each ncnn tool picks its default device, a discrete GPU before an integrated one
/// (ncnn's `get_default_gpu_index`); Vulkan's device order alone can put the on-board GPU first.
pub const GPU_AUTO: i32 = -2;

/// The `-g` value for a tool run, or `None` to leave the choice to the tool: automatic, and the CPU for the tools
/// that have no CPU path (Real-ESRGAN and custom ESRGAN models: they fail and fall back to Lanczos-3).
fn gpu_arg(b: &BackendChoice, gpu: i32) -> Option<String> {
    let cpu_capable = !matches!(b, BackendChoice::RealEsrgan(_) | BackendChoice::Custom(_));
    (gpu != GPU_AUTO && (gpu >= 0 || cpu_capable)).then(|| gpu.to_string())
}

pub fn run(b: &BackendChoice, tools: &Tools, gpu: i32, tile: u32, scratch: &Path, jobs: &[Job], cancel: &AtomicBool) -> Result<Vec<Output>> {
    let colour: Vec<Output> = match b {
        BackendChoice::Native => jobs.iter().map(|j| Output { w: j.w, h: j.h, rgba: j.rgba.clone() }).collect(),
        BackendChoice::Filter(f) => jobs
            .iter()
            .map(|j| {
                let (nw, nh) = (j.w * j.scale, j.h * j.scale);
                Output { w: nw, h: nh, rgba: resample::resize_rgba8(j.w, j.h, &j.rgba, nw, nh, *f, j.edge) }
            })
            .collect(),
        _ => run_ncnn_passes(b, tools, gpu, tile, scratch, jobs, cancel)?,
    };
    // Recombine with conventionally resized alpha.
    Ok(jobs
        .iter()
        .zip(colour)
        .map(|(j, mut o)| {
            if matches!(b, BackendChoice::Native) || !j.rgba.chunks_exact(4).any(|p| p[3] != 255) {
                if b.is_ai() {
                    for p in o.rgba.chunks_exact_mut(4) {
                        p[3] = 255;
                    }
                }
                return o;
            }
            let alpha: Vec<f32> = j.rgba.chunks_exact(4).map(|p| p[3] as f32 / 255.0).collect();
            let a = resample::resize(j.w as usize, j.h as usize, 1, &alpha, o.w as usize, o.h as usize, Filter::CatmullRom, j.edge);
            for (p, v) in o.rgba.chunks_exact_mut(4).zip(a) {
                p[3] = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
            o
        })
        .collect())
}

/// Run a model as many passes as each job's wanted scale needs (x32 with a
/// x4-only model is x4, x4, then x4 reduced to x2). Jobs sharing a wanted scale
/// go through each pass together; every pass pads and crops on its own.
fn run_ncnn_passes(b: &BackendChoice, tools: &Tools, gpu: i32, tile: u32, scratch: &Path, jobs: &[Job], cancel: &AtomicBool) -> Result<Vec<Output>> {
    run_passes(b, jobs, &mut |pass, run| run_ncnn(b, tools, gpu, tile, scratch, pass, cancel, run))
}

/// The pass loop behind [`run_ncnn_passes`]; `single` runs one pass over a
/// batch (jobs carry the kept scale, the second argument is the run scale) and
/// returns outputs of the kept size.
fn run_passes(b: &BackendChoice, jobs: &[Job], single: &mut dyn FnMut(&[Job], u32) -> Result<Vec<Output>>) -> Result<Vec<Output>> {
    let mut results: Vec<Option<Output>> = (0..jobs.len()).map(|_| None).collect();
    let mut scales: Vec<u32> = jobs.iter().map(|j| j.scale).collect();
    scales.sort_unstable();
    scales.dedup();
    for want in scales {
        let idx: Vec<usize> = (0..jobs.len()).filter(|&i| jobs[i].scale == want).collect();
        let mut cur: Vec<Output> = idx.iter().map(|&i| Output { w: jobs[i].w, h: jobs[i].h, rgba: jobs[i].rgba.clone() }).collect();
        for (run, kept) in b.passes(want) {
            let pass: Vec<Job> = idx
                .iter()
                .zip(cur)
                .map(|(&i, o)| Job { name: jobs[i].name.clone(), w: o.w, h: o.h, rgba: o.rgba, scale: kept, edge: jobs[i].edge })
                .collect();
            cur = single(&pass, run)?;
        }
        for (&i, o) in idx.iter().zip(cur) {
            results[i] = Some(o);
        }
    }
    Ok(results.into_iter().map(|o| o.expect("every job belongs to a scale group")).collect())
}

#[allow(clippy::too_many_arguments)]
fn run_ncnn(b: &BackendChoice, tools: &Tools, gpu: i32, tile: u32, scratch: &Path, jobs: &[Job], cancel: &AtomicBool, run_scale: u32) -> Result<Vec<Output>> {
    let Some(exe) = tools.exe(b) else {
        bail!("{} is not installed in {}", b.label(), tools.dir.display());
    };
    let tool_dir = exe.parent().unwrap().to_path_buf();
    let batch_dir = scratch.join(format!("batch-{}-{}", std::process::id(), b.key()));
    let (in_dir, out_dir) = (batch_dir.join("in"), batch_dir.join("out"));
    let _ = std::fs::remove_dir_all(&batch_dir);
    std::fs::create_dir_all(&in_dir)?;
    std::fs::create_dir_all(&out_dir)?;

    let mut padded = Vec::with_capacity(jobs.len());
    for (i, j) in jobs.iter().enumerate() {
        let m = PAD.min(j.w).min(j.h);
        let opaque: Vec<u8> = j.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        let (pw, ph, p) = resample::pad_rgba8(j.w, j.h, &opaque, m, j.edge);
        crate::project::save_png(&in_dir.join(format!("{i:05}.png")), pw, ph, &p)?;
        padded.push((m, pw, ph));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    let mut cmd = Command::new(&exe);
    cmd.current_dir(&tool_dir)
        .arg("-i")
        .arg(&in_dir)
        .arg("-o")
        .arg(&out_dir)
        .arg("-s")
        .arg(run_scale.to_string())
        .arg("-f")
        .arg("png")
        .arg("-t")
        .arg(tile.to_string());
    if let Some(g) = gpu_arg(b, gpu) {
        cmd.arg("-g").arg(g);
    }
    match b {
        BackendChoice::RealEsrgan(m) => {
            cmd.arg("-n").arg(m).arg("-m").arg(tool_dir.join("models"));
        }
        BackendChoice::Custom(m) => {
            cmd.arg("-n").arg(m).arg("-m").arg(tools.dir.join("custom-models"));
        }
        BackendChoice::Waifu2x { model, noise } | BackendChoice::RealCugan { model, noise } => {
            cmd.arg("-n").arg(noise.to_string()).arg("-m").arg(tool_dir.join(model));
        }
        BackendChoice::RealSr(m) => {
            cmd.arg("-m").arg(tool_dir.join(m));
        }
        _ => unreachable!(),
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            use std::io::Read;
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Cancelled);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let log = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("{} failed ({status}): {}", b.label(), last_lines(&log, 6));
    }

    let mut outs = Vec::with_capacity(jobs.len());
    for (i, (j, (m, pw, ph))) in jobs.iter().zip(padded).enumerate() {
        let p = out_dir.join(format!("{i:05}.png"));
        let (ow, oh, rgba) = crate::project::load_png(&p).map_err(|e| {
            Error::Format(format!("{} produced no output for {} ({e}); tool log: {}", b.label(), j.name, last_lines(&log, 4)))
        })?;
        if ow != pw * run_scale || oh != ph * run_scale {
            bail!("{} returned {ow}x{oh} for {pw}x{ph} at x{run_scale}", b.label());
        }
        let (cw, ch) = (j.w * run_scale, j.h * run_scale);
        let mut rgba = resample::crop_rgba8(ow, &rgba, m * run_scale, m * run_scale, cw, ch);
        let (nw, nh) = (j.w * j.scale, j.h * j.scale);
        if run_scale != j.scale {
            rgba = resample::resize_rgba8(cw, ch, &rgba, nw, nh, Filter::Lanczos3, j.edge);
        }
        outs.push(Output { w: nw, h: nh, rgba });
    }
    let _ = std::fs::remove_dir_all(&batch_dir);
    Ok(outs)
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

/// Normal-map aware upscale for two-channel (BC5) or RGB normal maps: decode
/// vectors, resize the vector field, renormalise, encode back.
pub fn upscale_normal_map(w: u32, h: u32, rgba: &[u8], scale: u32, two_channel: bool, snorm: bool, filter: Filter) -> Vec<u8> {
    let (nw, nh) = (w * scale, h * scale);
    let to_v = |b: u8| if snorm { (b as i8 as f32 / 127.0).max(-1.0) } else { b as f32 / 255.0 * 2.0 - 1.0 };
    let from_v = |v: f32| if snorm { ((v.clamp(-1.0, 1.0) * 127.0).round() as i8) as u8 } else { ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0 + 0.5) as u8 };
    let vec3: Vec<f32> = rgba
        .chunks_exact(4)
        .flat_map(|p| {
            let x = to_v(p[0]);
            let y = to_v(p[1]);
            let z = if two_channel { (1.0 - x * x - y * y).max(0.0).sqrt() } else { to_v(p[2]) };
            [x, y, z]
        })
        .collect();
    let up = resample::resize(w as usize, h as usize, 3, &vec3, nw as usize, nh as usize, filter, Edge::Wrap);
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for (i, v) in up.chunks_exact(3).enumerate() {
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
        let src_a = rgba[((i as u32 / nw / scale * w + (i as u32 % nw) / scale) * 4 + 3) as usize];
        out.extend_from_slice(&[from_v(v[0] / len), from_v(v[1] / len), if two_channel { 0 } else { from_v(v[2] / len) }, src_a]);
    }
    out
}

#[cfg(test)]
mod tests {
    /// The installed tools' cards on this machine (`cargo test -p wwhd_tex list_gpus_here -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn list_gpus_here() {
        let tools = Tools { dir: std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/upscalers") };
        let gpus = list_gpus(&tools);
        println!("{gpus:?}");
        assert!(!gpus.is_empty());
    }

    #[test]
    fn gpu_names_come_from_the_tools_device_lines_in_their_order() {
        let text = "[0 NVIDIA GeForce RTX 4090]  queueC=2[8]  queueG=0[16]  queueT=1[2]\n\
                    [0 NVIDIA GeForce RTX 4090]  subgroup=32  basic=1\n\
                    [1 AMD Radeon(TM) Graphics]  queueC=1[4]  queueG=0[8]\n\
                    decode image C:/x/none.png failed\n";
        assert_eq!(
            parse_gpu_lines(text),
            vec![(0, "NVIDIA GeForce RTX 4090".to_string()), (1, "AMD Radeon(TM) Graphics".to_string())]
        );
        assert!(parse_gpu_lines("Usage: realesrgan-ncnn-vulkan -i infile\n  -h  show this help\n").is_empty());
    }

    #[test]
    fn gpu_automatic_passes_no_device_and_cpu_only_where_the_tool_has_one() {
        let esrgan = BackendChoice::default();
        assert!(matches!(esrgan, BackendChoice::RealEsrgan(_)));
        let waifu = BackendChoice::Waifu2x { model: "models-cunet".into(), noise: 0 };
        assert_eq!(gpu_arg(&esrgan, GPU_AUTO), None);
        assert_eq!(gpu_arg(&waifu, GPU_AUTO), None);
        assert_eq!(gpu_arg(&esrgan, 1).as_deref(), Some("1"));
        assert_eq!(gpu_arg(&esrgan, -1), None);
        assert_eq!(gpu_arg(&waifu, -1).as_deref(), Some("-1"));
        assert_eq!(crate::project::Settings::default().gpu, GPU_AUTO);
    }

    use super::*;

    #[test]
    fn normal_map_stays_unit_length() {
        let src: Vec<u8> = (0..64).flat_map(|i| [(i * 4) as u8, 128, 0, 255]).collect();
        let out = upscale_normal_map(8, 8, &src, 4, true, false, Filter::CatmullRom);
        assert_eq!(out.len(), 32 * 32 * 4);
        for p in out.chunks_exact(4) {
            let x = p[0] as f32 / 255.0 * 2.0 - 1.0;
            let y = p[1] as f32 / 255.0 * 2.0 - 1.0;
            assert!(x * x + y * y <= 1.02);
        }
    }

    #[test]
    fn pass_plans() {
        assert_eq!(pass_plan(&[4], 32), [(4, 4), (4, 4), (4, 2)]);
        assert_eq!(pass_plan(&[2, 4], 8), [(4, 4), (2, 2)]);
        assert_eq!(pass_plan(&[2], 8), [(2, 2), (2, 2), (2, 2)]);
        assert_eq!(pass_plan(&[4], 2), [(4, 2)]);
        assert_eq!(pass_plan(&[4], 16), [(4, 4), (4, 4)]);
        assert_eq!(pass_plan(&[2, 3, 4], 4), [(4, 4)]);
        // Powers of two only; a x3 native scale is never chosen.
        assert_eq!(pass_plan(&[2, 3], 4), [(2, 2), (2, 2)]);
        assert!(pass_plan(&[4], 1).is_empty());
    }

    #[test]
    fn filters_run_in_one_step() {
        assert_eq!(BackendChoice::Filter(Filter::Lanczos3).passes(32), [(32, 32)]);
        assert_eq!(BackendChoice::RealEsrgan("realesrgan-x4plus".into()).passes(32), [(4, 4), (4, 4), (4, 2)]);
    }

    #[test]
    fn passes_chain_and_groups_keep_their_jobs() {
        // A x4-only model on a x32 job (4,4,4->2) and a x8 job (4,4->2), in one batch.
        let b = BackendChoice::Custom("fake".into());
        let mk = |name: &str, scale: u32, v: u8| Job { name: name.into(), w: 2, h: 2, rgba: vec![v; 16], scale, edge: Edge::Wrap };
        let jobs = [mk("a32", 32, 10), mk("b8", 8, 20), mk("c32", 32, 30)];
        let mut calls: Vec<(u32, Vec<(String, u32, u32, u8)>)> = Vec::new();
        let out = run_passes(&b, &jobs, &mut |pass, run| {
            calls.push((run, pass.iter().map(|j| (j.name.clone(), j.w, j.scale, j.rgba[0])).collect()));
            // Fake model: every pixel +1 per pass so chaining is visible; result at the kept size.
            Ok(pass
                .iter()
                .map(|j| Output { w: j.w * j.scale, h: j.h * j.scale, rgba: vec![j.rgba[0] + 1; (j.w * j.scale * j.h * j.scale * 4) as usize] })
                .collect())
        })
        .unwrap();
        let n = |s: &str, w: u32, sc: u32, v: u8| (s.to_string(), w, sc, v);
        assert_eq!(
            calls,
            vec![
                // x8 group first (scales ascending): 2 -> 8 -> 16 ... (4,4) then (4,2)
                (4, vec![n("b8", 2, 4, 20)]),
                (4, vec![n("b8", 8, 2, 21)]),
                (4, vec![n("a32", 2, 4, 10), n("c32", 2, 4, 30)]),
                (4, vec![n("a32", 8, 4, 11), n("c32", 8, 4, 31)]),
                (4, vec![n("a32", 32, 2, 12), n("c32", 32, 2, 32)]),
            ]
        );
        assert_eq!((out[0].w, out[1].w, out[2].w), (64, 16, 64));
        assert_eq!((out[0].rgba[0], out[1].rgba[0], out[2].rgba[0]), (13, 22, 33), "each pass fed the next, results back in job order");
    }

    #[test]
    fn run_scale_selection() {
        assert_eq!(BackendChoice::RealEsrgan("realesrgan-x4plus".into()).run_scale(2), 4);
        assert_eq!(BackendChoice::RealEsrgan("realesr-animevideov3".into()).run_scale(2), 2);
        assert_eq!(BackendChoice::RealCugan { model: "models-pro".into(), noise: -1 }.run_scale(4), 3);
    }
}
