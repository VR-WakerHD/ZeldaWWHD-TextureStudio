//! Project directory layout and persisted settings.
//!
//! ```text
//! <project>/
//!   settings.json
//!   source_inventory/inventory.json      every file and texture found on the disc
//!   masters/<aa>/<hash>.png              lossless level-0 RGBA of each unique texture
//!   cache/upscaled/<backend>/<hash>_x<scale>.png
//!   cache/encoded/<key>.bin              tiled replacement surfaces
//!   manifests/review.json                approvals, rejections, overrides
//!   manifests/classes.json               manual classifications
//!   manifests/replacement_manifest.json  what each exported texture replaced
//!   output/packages/<id>/content/...     recomp content packages
//!   output/runtime/<id>/textures/...      runtime texture packs (index.json + DDS)
//!   output/previews/{pcvr,quest3,steam_frame}/...  non-game previews (DDS)
//!   logs/  reports/
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::classify::Class;
use crate::error::Result;
use crate::upscale::BackendChoice;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScanScope {
    /// Everything below the selected directory.
    All,
    /// Only the `content` folder of a disc dump.
    ContentOnly,
    /// Selected sub-directories or files, relative to the disc root.
    Selected(Vec<String>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub disc_root: PathBuf,
    pub scope: ScanScope,
    /// Longest output side (2048, 4096, 8192 or 16384).
    pub max_dim: u32,
    /// Global scale factor for eligible textures: 1 (native) or one of `SCALES`.
    pub scale: u32,
    /// Scale per class, overriding `scale`; classes not listed follow the global scale.
    pub class_scales: Vec<(Class, ScaleChoice)>,
    /// Textures whose longest side is below this stay native (tiny ramps, dots).
    pub min_source_dim: u32,
    /// Backend per class; classes not listed use `default_backend`.
    pub backends: Vec<(Class, BackendChoice)>,
    pub default_backend: BackendChoice,
    /// GPU index for ncnn tools; -1 forces CPU.
    pub gpu: i32,
    /// ncnn tile size; 0 = automatic.
    pub tile: u32,
    /// Number of textures handed to one ncnn process.
    pub batch: usize,
    /// Directory holding the official ncnn tool releases.
    pub tools_dir: PathBuf,
    /// Content package limits enforced by the recomp mod loader.
    pub package_max_bytes: u64,
    pub file_max_bytes: u64,
    pub package_max_files: usize,
    /// Upper bound on extra guest memory per rebuilt archive (decompressed growth).
    pub archive_growth_max_bytes: u64,
    /// Export unreviewed textures that passed validation (rejected ones never).
    pub export_unreviewed: bool,
    pub package_name: String,
    /// Limit upscaling/export to disc files whose path contains one of these
    /// (empty = everything). Useful for trying one area first.
    pub only_files: Vec<String>,
    /// Diagnostics: tint replaced textures magenta so in-game loading is obvious.
    pub debug_tint: bool,
    /// A rebuilt archive may hold at most this many times its original
    /// decompressed size: the game loads archives into heaps sized for the
    /// originals, so unlimited growth overflows them (0 = no limit).
    pub max_growth_factor: f32,
    /// Archives (path fragments) that are never rebuilt: verified in game to
    /// be loaded into fixed-size heaps that cannot take larger textures.
    pub exclude_files: Vec<String>,
}

/// Scale factors a texture or class can be set to (besides native).
pub const SCALES: [u32; 5] = [2, 4, 8, 16, 32];

/// A class's scale: follow the global one, stay native, or a fixed factor from `SCALES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScaleChoice {
    Global,
    Native,
    X(u32),
}

/// Ramps keep their native resolution (their backend is `Native` too); every
/// other class follows the global scale.
pub fn default_class_scales() -> Vec<(Class, ScaleChoice)> {
    crate::classify::ALL_CLASSES
        .iter()
        .map(|&c| (c, if c == Class::Ramp { ScaleChoice::Native } else { ScaleChoice::Global }))
        .collect()
}

/// Archives verified on 2026-11-02 to crash or halt the game when enlarged
/// (fixed-size heaps in the game's code).
pub fn default_excluded() -> Vec<String> {
    [
        "Common/Misc/Misc.szs",
        "Common/Object/Tlogo.szs",
        "Common/Particle/Particle.szs",
        "Common/ProgramTexture/ProgramTexture.szs",
        // Packs: their inner archives are unpacked on demand into a resident
        // heap of fixed size (the King of Red Lions' Ship.szs in
        // szs_permanent0.pack fails to load when enlarged).
        "Common/Pack/",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            disc_root: PathBuf::new(),
            scope: ScanScope::ContentOnly,
            max_dim: 4096,
            scale: 4,
            class_scales: default_class_scales(),
            min_source_dim: 16,
            backends: crate::upscale::default_class_backends(),
            default_backend: BackendChoice::default(),
            gpu: 0,
            tile: 0,
            batch: 32,
            tools_dir: PathBuf::from("tools/upscalers"),
            package_max_bytes: 500 * 1024 * 1024,
            file_max_bytes: 120 * 1024 * 1024,
            package_max_files: 4000,
            archive_growth_max_bytes: 96 * 1024 * 1024,
            export_unreviewed: true,
            package_name: "WWHD 4K Textures".into(),
            only_files: Vec::new(),
            debug_tint: false,
            max_growth_factor: 2.0,
            exclude_files: default_excluded(),
        }
    }
}

impl Settings {
    pub fn file_excluded(&self, file: &str) -> bool {
        self.exclude_files.iter().any(|f| !f.is_empty() && file.contains(f.as_str()))
    }
    pub fn file_selected(&self, file: &str) -> bool {
        self.only_files.is_empty() || self.only_files.iter().any(|f| file.contains(f.as_str()))
    }
    /// The class's scale setting; `Global` when the class is not listed.
    pub fn scale_choice_for(&self, class: Class) -> ScaleChoice {
        self.class_scales.iter().find(|(c, _)| *c == class).map(|(_, v)| *v).unwrap_or(ScaleChoice::Global)
    }
    pub fn backend_for(&self, class: Class) -> BackendChoice {
        self.backends
            .iter()
            .find(|(c, _)| *c == class)
            .map(|(_, b)| b.clone())
            .unwrap_or_else(|| self.default_backend.clone())
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
}

impl Project {
    pub fn open(root: impl Into<PathBuf>) -> Result<Project> {
        let p = Project { root: root.into() };
        for d in ["source_inventory", "masters", "cache", "manifests", "output", "logs", "reports"] {
            std::fs::create_dir_all(p.root.join(d))?;
        }
        Ok(p)
    }
    pub fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    pub fn load_settings(&self) -> Settings {
        std::fs::read(self.settings_path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default()
    }
    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        write_atomic(&self.settings_path(), &serde_json::to_vec_pretty(s)?)
    }
    pub fn inventory_path(&self) -> PathBuf {
        self.root.join("source_inventory/inventory.json")
    }
    pub fn master_path(&self, hash: &str) -> PathBuf {
        self.root.join("masters").join(&hash[..2]).join(format!("{hash}.png"))
    }
    pub fn upscaled_path(&self, backend_key: &str, hash: &str, scale: u32) -> PathBuf {
        self.root.join("cache/upscaled").join(backend_key).join(&hash[..2]).join(format!("{hash}_x{scale}.png"))
    }
    pub fn encoded_path(&self, key: &str) -> PathBuf {
        self.root.join("cache/encoded").join(&key[..2]).join(format!("{key}.bin"))
    }
    /// Encoded runtime-pack DDS, keyed like `encoded_path` plus the output format.
    pub fn encoded_dds_path(&self, key: &str) -> PathBuf {
        self.root.join("cache/encoded").join(&key[..2]).join(format!("{key}.dds"))
    }
    pub fn review_path(&self) -> PathBuf {
        self.root.join("manifests/review.json")
    }
    pub fn classes_path(&self) -> PathBuf {
        self.root.join("manifests/classes.json")
    }
    pub fn packages_dir(&self) -> PathBuf {
        self.root.join("output/packages")
    }
    /// Runtime texture packs (`textures` mod packages), one folder per package.
    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("output/runtime")
    }
    pub fn previews_dir(&self) -> PathBuf {
        self.root.join("output/previews")
    }
    pub fn log_path(&self) -> PathBuf {
        self.root.join("logs/studio.log")
    }
    pub fn reports_dir(&self) -> PathBuf {
        self.root.join("reports")
    }
}

/// Write via a temporary file and rename, so an interrupted write never leaves
/// a truncated file in place.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let mut buf = Vec::new();
    {
        use image::ImageEncoder;
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            &mut buf,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        );
        enc.write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
            .map_err(|e| crate::Error::Format(format!("png encode: {e}")))?;
    }
    write_atomic(path, &buf)
}

pub fn load_png(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let img = image::open(path).map_err(|e| crate::Error::Format(format!("{}: {e}", path.display())))?;
    let rgba = img.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}
