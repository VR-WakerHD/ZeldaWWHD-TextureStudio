//! Post-upscale enhancement: detail recovery, sharpening and colour, run on the
//! upscaled level 0 just before mips are built and the texture is encoded.
//!
//! The upscaled PNG in `cache/upscaled` is never changed: it is the lossless
//! master every profile starts from, so changing a profile re-encodes and never
//! re-upscales.
//!
//! All work is done in Oklab (a perceptual space): sharpening and detail touch
//! lightness only, so they never create colour fringes; saturation, vibrance
//! and contrast work on lightness and chroma, then colours that leave the sRGB
//! gamut have their chroma reduced (hue and lightness kept) instead of being
//! clipped. Pixel values are read as sRGB-encoded for this, whatever the GX2
//! format says: the game's UNORM colour textures hold painted, gamma-encoded
//! colours too, and the conversion back uses the same curve, so a no-op profile
//! gives back the exact input bytes.
//!
//! Alpha is never written. Filters weight neighbours by alpha, so the colour
//! hidden under transparent pixels cannot bleed into visible edges, and pixels
//! with alpha 0 keep their colour unless `fill_transparent` is on.
//!
//! Data textures (normal maps, masks, ramps, one- and two-channel formats) and
//! textures of unknown class are never enhanced.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::classify::Class;
use crate::gx2::{self, Codec};
use crate::resample::{self, Edge, Filter};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SharpenMode {
    /// Gaussian unsharp mask on lightness.
    UnsharpMask,
    /// High-pass detail added with a mid-tone weight: highlights and shadows are left alone.
    HighPass,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sharpen {
    pub enabled: bool,
    pub mode: SharpenMode,
    /// Gaussian sigma, in source texels (`radius_in_source`, the default) or in output pixels.
    pub radius: f32,
    /// The radius counts original texels, so a ×4 upscale is sharpened at 4× the pixel radius: the scale of the
    /// painted detail, which survives mips. Off: output pixels (very fine grain; mostly lost once the texture is
    /// seen from further than arm's length).
    pub radius_in_source: bool,
    /// 0.55 = 55 %.
    pub amount: f32,
    /// Differences below this many 8-bit lightness levels are left alone (keeps flat cel shading flat).
    pub threshold: f32,
    /// Less sharpening on edges that are already strong (outlines), more on fine detail.
    pub edge_protection: bool,
    /// The result stays within the neighbourhood's range (plus a small margin): no ringing or bright/dark halos.
    pub halo_suppression: bool,
    /// Less sharpening on busy, noisy textures, a little more on clean ones (measured per texture).
    pub adaptive: bool,
}

impl Default for Sharpen {
    fn default() -> Self {
        Sharpen {
            enabled: true,
            mode: SharpenMode::UnsharpMask,
            radius: 0.6,
            radius_in_source: true,
            amount: 0.55,
            threshold: 2.0,
            edge_protection: true,
            halo_suppression: true,
            adaptive: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Detail {
    pub enabled: bool,
    /// Edge-preserving local contrast (guided filter): lifts painted texture the upscaler smoothed, leaves outlines.
    pub amount: f32,
    /// Size of the detail recovered, in source texels (the radius grows with the upscale factor).
    pub radius: f32,
    /// Below this local contrast (Oklab L) a region counts as texture to lift; above it, as an edge to keep.
    pub edge_level: f32,
    /// Re-inject the original's own texture where an AI model flattened it (0 = off). Taken from the original only:
    /// nothing is invented, but the original's compression noise comes back with it, so keep it low.
    pub restore: f32,
}

impl Default for Detail {
    fn default() -> Self {
        Detail { enabled: true, amount: 0.3, radius: 0.75, edge_level: 0.05, restore: 0.0 }
    }
}

/// Hue bands for selective colour, in Oklab hue order.
pub const HUE_BANDS: [(&str, f32); 6] = [("Red", 29.0), ("Yellow", 100.0), ("Green", 142.0), ("Cyan", 195.0), ("Blue", 264.0), ("Magenta", 328.0)];

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Colour {
    pub enabled: bool,
    /// Chroma scale: 0.07 = +7 %.
    pub saturation: f32,
    /// Chroma boost that is strongest for muted colours and fades out for colours already saturated.
    pub vibrance: f32,
    /// Lightness contrast around the texture's mean lightness: 0.03 = +3 %.
    pub contrast: f32,
    /// Lightness gamma; above 1 brightens mid-tones.
    pub gamma: f32,
    /// Lightness mapped to black and to white (0 and 1 = unchanged).
    pub black_point: f32,
    pub white_point: f32,
    /// −1..1: lift (+) or deepen (−) the bright and dark ranges.
    pub highlights: f32,
    pub shadows: f32,
    /// −1 (cooler) .. 1 (warmer).
    pub temperature: f32,
    /// Extra chroma per hue band (`HUE_BANDS`), e.g. 0.05 = +5 % for that hue.
    pub selective: [f32; 6],
    /// Brightening fades out near white, so highlights never clip.
    pub protect_highlights: bool,
    /// Darkening fades out near black, so shadow detail is not crushed.
    pub protect_shadows: bool,
}

impl Default for Colour {
    fn default() -> Self {
        Colour {
            enabled: true,
            saturation: 0.07,
            vibrance: 0.05,
            contrast: 0.03,
            gamma: 1.0,
            black_point: 0.0,
            white_point: 1.0,
            highlights: 0.0,
            shadows: 0.0,
            temperature: 0.0,
            selective: [0.0; 6],
            protect_highlights: true,
            protect_shadows: true,
        }
    }
}

/// One processing profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub sharpen: Sharpen,
    pub detail: Detail,
    pub colour: Colour,
    /// Strengths are tuned per texture class (`class_tuning`); off = the same settings for every colour texture.
    pub per_class: bool,
    /// Mips are reduced with a Catmull-Rom filter (in linear light for sRGB) instead of a 2×2 box: distant
    /// surfaces stay crisper. Colour textures only.
    pub sharp_mips: bool,
}

impl Default for Profile {
    fn default() -> Self {
        enhanced()
    }
}

pub const ORIGINAL: &str = "Wind Waker Original";
pub const ENHANCED: &str = "Wind Waker Enhanced";
pub const ULTRA: &str = "Wind Waker Ultra Sharp";

/// Minimal sharpening, colours as painted.
pub fn original() -> Profile {
    Profile {
        name: ORIGINAL.into(),
        sharpen: Sharpen { amount: 0.25, threshold: 3.0, ..Sharpen::default() },
        detail: Detail { enabled: false, ..Detail::default() },
        colour: Colour { enabled: false, saturation: 0.0, vibrance: 0.0, contrast: 0.0, ..Colour::default() },
        per_class: true,
        sharp_mips: false,
    }
}

/// The default: moderate sharpening, mild vibrance and contrast, balanced detail recovery.
pub fn enhanced() -> Profile {
    Profile { name: ENHANCED.into(), sharpen: Sharpen::default(), detail: Detail::default(), colour: Colour::default(), per_class: true, sharp_mips: true }
}

/// Stronger edges and detail, still halo-suppressed; colour kept controlled.
pub fn ultra() -> Profile {
    Profile {
        name: ULTRA.into(),
        sharpen: Sharpen { radius: 0.7, amount: 0.85, ..Sharpen::default() },
        detail: Detail { amount: 0.55, restore: 0.15, ..Detail::default() },
        colour: Colour { saturation: 0.08, vibrance: 0.08, contrast: 0.04, ..Colour::default() },
        per_class: true,
        sharp_mips: true,
    }
}

/// Stages of a profile switched off, for side-by-side comparisons of what each stage does.
pub fn only(p: &Profile, sharpen: bool, detail: bool, colour: bool) -> Profile {
    let mut q = p.clone();
    q.sharpen.enabled &= sharpen;
    q.detail.enabled &= detail;
    q.colour.enabled &= colour;
    q
}

/// The profile with sharpening (and detail) strength scaled by `k`, e.g. 0.5 = half as strong.
pub fn with_sharpen_strength(p: &Profile, k: f32) -> Profile {
    let mut q = p.clone();
    q.sharpen.amount *= k;
    q.detail.amount *= k;
    q.name = format!("{} · sharpen ×{k}", p.name);
    q
}

/// The profile with every colour change scaled by `k`.
pub fn with_colour_strength(p: &Profile, k: f32) -> Profile {
    let mut q = p.clone();
    let c = &mut q.colour;
    c.saturation *= k;
    c.vibrance *= k;
    c.contrast *= k;
    c.gamma = 1.0 + (c.gamma - 1.0) * k;
    c.temperature *= k;
    c.highlights *= k;
    c.shadows *= k;
    c.selective = c.selective.map(|v| v * k);
    q.name = format!("{} · colour ×{k}", p.name);
    q
}

pub fn builtins() -> Vec<Profile> {
    vec![original(), enhanced(), ultra()]
}

pub fn is_builtin(name: &str) -> bool {
    [ORIGINAL, ENHANCED, ULTRA].contains(&name)
}

/// Project-wide enhancement settings (in `settings.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnhanceSettings {
    pub enabled: bool,
    /// The profile used for textures without their own choice.
    pub profile: String,
    /// User profiles (names never equal a built-in's).
    pub custom: Vec<Profile>,
    /// Colour under fully transparent pixels of cut-out textures is replaced by the nearest visible colour, so
    /// filtering in the game cannot pull dark or bright fringes into edges. Off by default: in some textures the
    /// alpha channel is not coverage, and there the hidden colour is visible.
    pub fill_transparent: bool,
}

impl Default for EnhanceSettings {
    fn default() -> Self {
        EnhanceSettings { enabled: true, profile: ENHANCED.into(), custom: Vec::new(), fill_transparent: false }
    }
}

impl EnhanceSettings {
    /// A profile by name: built-in, then custom; an unknown name falls back to Enhanced.
    pub fn find(&self, name: &str) -> Profile {
        builtins().into_iter().chain(self.custom.iter().cloned()).find(|p| p.name == name).unwrap_or_else(enhanced)
    }
    pub fn names(&self) -> Vec<String> {
        builtins().into_iter().map(|p| p.name).chain(self.custom.iter().map(|p| p.name.clone())).collect()
    }
    /// The profile for a texture: its own choice, else the project's.
    pub fn profile_for(&self, own: Option<&str>) -> Profile {
        self.find(own.unwrap_or(&self.profile))
    }
    /// A name not used yet, from `base`.
    pub fn unique_name(&self, base: &str) -> String {
        let names = self.names();
        if !names.iter().any(|n| n == base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} {i}")).find(|n| !names.contains(n)).unwrap()
    }
}

/// Category of a texture for enhancement, from its class and alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    Character,
    Environment,
    Vegetation,
    Water,
    Architecture,
    Sky,
    UiIcons,
    Effects,
    Transparent,
    Unknown,
    /// Normal maps, masks, ramps, one/two-channel data: never enhanced.
    Data,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Character => "Character artwork",
            Category::Environment => "Environment artwork",
            Category::Vegetation => "Vegetation",
            Category::Water => "Water",
            Category::Architecture => "Stone & architecture",
            Category::Sky => "Sky & clouds",
            Category::UiIcons => "UI & icons",
            Category::Effects => "Effects",
            Category::Transparent => "Transparent (blended)",
            Category::Unknown => "Unknown (not enhanced)",
            Category::Data => "Data (never enhanced)",
        }
    }
}

/// Whether the format stores colour in RGB (one- and two-channel formats hold data or luminance).
pub fn stores_rgb(format: u32) -> bool {
    matches!(gx2::codec(format), Ok(Codec::Bc1 | Codec::Bc2 | Codec::Bc3 | Codec::Rgba8 | Codec::Rgb565 | Codec::Rgba4 | Codec::Rgb5a1))
}

/// Alpha that blends (many values between 0 and 255) rather than cutting out.
pub fn has_blended_alpha(rgba: &[u8]) -> bool {
    let (mut n, mut mid) = (0usize, 0usize);
    for p in rgba.chunks_exact(4).step_by(3) {
        n += 1;
        if p[3] > 16 && p[3] < 240 {
            mid += 1;
        }
    }
    n > 0 && mid as f32 / n as f32 > 0.1
}

pub fn category(class: Class, format: u32, rgba: &[u8]) -> Category {
    if !stores_rgb(format) {
        return Category::Data;
    }
    let base = match class {
        Class::NormalMap | Class::Mask | Class::Ramp => return Category::Data,
        Class::Unknown => return Category::Unknown,
        Class::CharacterAlbedo => Category::Character,
        Class::EnvironmentAlbedo | Class::Terrain => Category::Environment,
        Class::Foliage => Category::Vegetation,
        Class::Water => Category::Water,
        Class::Rock | Class::Wood => Category::Architecture,
        Class::Sky => Category::Sky,
        Class::Ui | Class::Lettering => Category::UiIcons,
        Class::Particle => Category::Effects,
    };
    if !matches!(base, Category::UiIcons | Category::Effects) && has_blended_alpha(rgba) { Category::Transparent } else { base }
}

/// Strength multipliers per category: sharpening, detail recovery, colour.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tuning {
    pub sharpen: f32,
    pub detail: f32,
    pub colour: f32,
}

/// Why: faces and cloth have soft painted gradients (less detail lift); foliage and busy ground shimmer in VR when
/// over-sharpened; stone and wood carry painted texture that benefits most; water and sky are smooth gradients
/// where sharpening shows banding and noise; UI colours must stay exact and text edges must not ring; effects
/// are soft by design; blended transparency shows halos first.
pub fn class_tuning(c: Category) -> Tuning {
    let t = |sharpen, detail, colour| Tuning { sharpen, detail, colour };
    match c {
        Category::Character => t(1.0, 0.8, 1.0),
        Category::Environment => t(1.0, 1.0, 1.0),
        Category::Vegetation => t(0.75, 0.8, 1.0),
        Category::Water => t(0.5, 0.4, 0.6),
        Category::Architecture => t(1.15, 1.25, 0.9),
        Category::Sky => t(0.3, 0.2, 0.8),
        Category::UiIcons => t(0.6, 0.0, 0.0),
        Category::Effects => t(0.4, 0.3, 0.6),
        Category::Transparent => t(0.7, 0.6, 0.9),
        Category::Unknown | Category::Data => t(0.0, 0.0, 0.0),
    }
}

/// Classifications below this confidence get conservative strengths.
pub const LOW_CONFIDENCE: f32 = 0.5;
pub const LOW_CONFIDENCE_FACTOR: f32 = 0.6;

/// What is being enhanced.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub class: Class,
    pub confidence: f32,
    pub manual: bool,
    pub format: u32,
    /// Upscale factor of the image (output texels per source texel).
    pub scale: u32,
    pub edge: Edge,
    /// Made by an AI model (texture restoration only makes sense then).
    pub ai: bool,
}

/// Everything besides the image bytes that decides what [`resolve`] + [`apply`] produce, for cache keys (the
/// image-dependent parts are a function of the image, which the caches hash anyway).
pub fn cache_tag(profile: &Profile, s: &EnhanceSettings, t: &Target) -> String {
    if !s.enabled {
        return "enhance-off".into();
    }
    format!("enhance-v1:{}:{}:{t:?}", serde_json::to_string(profile).unwrap_or_default(), s.fill_transparent)
}

/// A profile with the per-texture tuning applied: exactly what runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub profile: String,
    pub category: Category,
    pub sharpen: Option<Sharpen>,
    pub detail: Option<Detail>,
    pub colour: Option<Colour>,
    pub scale: u32,
    #[serde(skip)]
    pub edge: Option<Edge>,
    pub fill_transparent: bool,
    pub sharp_mips: bool,
}

impl Resolved {
    /// A stable text of everything that decides the output, for encode-cache keys.
    pub fn tag(&self) -> String {
        format!("enhance-v1:{}:{:?}", serde_json::to_string(self).unwrap_or_default(), self.edge)
    }
    pub fn is_noop(&self) -> bool {
        self.sharpen.is_none() && self.detail.is_none() && self.colour.is_none() && !self.fill_transparent
    }
}

fn colour_is_noop(c: &Colour) -> bool {
    c.saturation == 0.0
        && c.vibrance == 0.0
        && c.contrast == 0.0
        && c.gamma == 1.0
        && c.black_point == 0.0
        && c.white_point == 1.0
        && c.highlights == 0.0
        && c.shadows == 0.0
        && c.temperature == 0.0
        && c.selective.iter().all(|v| *v == 0.0)
}

/// Apply class tuning and confidence to a profile. `None` for textures that are never enhanced.
pub fn resolve(profile: &Profile, s: &EnhanceSettings, t: &Target, rgba: &[u8]) -> Option<Resolved> {
    if !s.enabled {
        return None;
    }
    let category = category(t.class, t.format, rgba);
    if matches!(category, Category::Data | Category::Unknown) {
        return None;
    }
    let mut tune = if profile.per_class { class_tuning(category) } else { Tuning { sharpen: 1.0, detail: 1.0, colour: if category == Category::UiIcons { 0.0 } else { 1.0 } } };
    if !t.manual && t.confidence < LOW_CONFIDENCE {
        tune.sharpen *= LOW_CONFIDENCE_FACTOR;
        tune.detail *= LOW_CONFIDENCE_FACTOR;
        tune.colour *= LOW_CONFIDENCE_FACTOR;
    }
    let sharpen = (profile.sharpen.enabled && profile.sharpen.amount * tune.sharpen > 0.0)
        .then(|| Sharpen { amount: profile.sharpen.amount * tune.sharpen, ..profile.sharpen });
    let d = &profile.detail;
    let restore = if t.ai && t.scale > 1 { d.restore * tune.detail } else { 0.0 };
    let detail = (d.enabled && t.scale > 1 && (d.amount * tune.detail > 0.0 || restore > 0.0)).then(|| Detail { amount: d.amount * tune.detail, restore, ..*d });
    let c = &profile.colour;
    let k = tune.colour;
    let colour = Colour {
        saturation: c.saturation * k,
        vibrance: c.vibrance * k,
        contrast: c.contrast * k,
        gamma: 1.0 + (c.gamma - 1.0) * k,
        black_point: c.black_point * k,
        white_point: 1.0 - (1.0 - c.white_point) * k,
        highlights: c.highlights * k,
        shadows: c.shadows * k,
        temperature: c.temperature * k,
        selective: c.selective.map(|v| v * k),
        ..*c
    };
    let colour = (c.enabled && k > 0.0 && !colour_is_noop(&colour)).then_some(colour);
    let fill = s.fill_transparent && crate::mips::is_alpha_tested(rgba);
    Some(Resolved { profile: profile.name.clone(), category, sharpen, detail, colour, scale: t.scale.max(1), edge: Some(t.edge), fill_transparent: fill, sharp_mips: profile.sharp_mips })
}

// ---- colour space ----------------------------------------------------------------------------------------------

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

fn decode_lut() -> [f32; 256] {
    let mut t = [0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        *v = srgb_to_linear(i as f32 / 255.0);
    }
    t
}

pub fn linear_to_oklab([r, g, b]: [f32; 3]) -> [f32; 3] {
    let l = 0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b;
    let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
    let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
    let (l, m, s) = (l.max(0.0).cbrt(), m.max(0.0).cbrt(), s.max(0.0).cbrt());
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

pub fn oklab_to_linear([l, a, b]: [f32; 3]) -> [f32; 3] {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

fn in_gamut(c: [f32; 3]) -> bool {
    c.iter().all(|v| (-1e-4..=1.0 + 1e-4).contains(v))
}

/// Linear RGB for an Oklab colour; out-of-gamut colours keep lightness and hue and lose chroma until they fit.
fn to_gamut(lab: [f32; 3]) -> [f32; 3] {
    let c = oklab_to_linear(lab);
    if in_gamut(c) {
        return c;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        if in_gamut(oklab_to_linear([lab[0], lab[1] * mid, lab[2] * mid])) { lo = mid } else { hi = mid }
    }
    let c = oklab_to_linear([lab[0], lab[1] * lo, lab[2] * lo]);
    [c[0].clamp(0.0, 1.0), c[1].clamp(0.0, 1.0), c[2].clamp(0.0, 1.0)]
}

// ---- filters ---------------------------------------------------------------------------------------------------

fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil().max(1.0) as i32;
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

/// Separable 1D convolution with edge addressing, over several planes at once.
fn convolve(w: usize, h: usize, planes: &[&[f32]], k: &[f32], edge: Edge) -> Vec<Vec<f32>> {
    let r = (k.len() / 2) as i64;
    let xs: Vec<Vec<usize>> = (0..w as i64).map(|x| (-r..=r).map(|d| resample::address(x + d, w, edge)).collect()).collect();
    let ys: Vec<Vec<usize>> = (0..h as i64).map(|y| (-r..=r).map(|d| resample::address(y + d, h, edge)).collect()).collect();
    planes
        .iter()
        .map(|src| {
            let mut mid = vec![0f32; w * h];
            mid.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                let s = &src[y * w..(y + 1) * w];
                for (x, o) in row.iter_mut().enumerate() {
                    *o = xs[x].iter().zip(k).map(|(&i, kv)| s[i] * kv).sum();
                }
            });
            let mut out = vec![0f32; w * h];
            out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                for (&i, kv) in ys[y].iter().zip(k) {
                    let m = &mid[i * w..(i + 1) * w];
                    for (o, v) in row.iter_mut().zip(m) {
                        *o += v * kv;
                    }
                }
            });
            out
        })
        .collect()
}

/// Alpha-weighted blur: transparent texels do not contribute.
fn weighted_blur(w: usize, h: usize, v: &[f32], wt: &[f32], k: &[f32], edge: Edge) -> Vec<f32> {
    if wt.iter().all(|&a| a >= 1.0) {
        return convolve(w, h, &[v], k, edge).remove(0);
    }
    let vw: Vec<f32> = v.iter().zip(wt).map(|(a, b)| a * b).collect();
    let r = convolve(w, h, &[&vw, wt], k, edge);
    r[0].iter().zip(&r[1]).zip(v).map(|((n, d), orig)| if *d > 1e-4 { n / d } else { *orig }).collect()
}

fn box_kernel(r: usize) -> Vec<f32> {
    vec![1.0 / (2 * r + 1) as f32; 2 * r + 1]
}

/// Local min and max over a (2r+1)² window.
fn local_range(w: usize, h: usize, v: &[f32], r: usize, edge: Edge) -> (Vec<f32>, Vec<f32>) {
    let r = r as i64;
    let pass = |src: &[f32], horizontal: bool, f: fn(f32, f32) -> f32, init: f32| -> Vec<f32> {
        let mut out = vec![0f32; w * h];
        out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            for (x, o) in row.iter_mut().enumerate() {
                let mut acc = init;
                for d in -r..=r {
                    let i = if horizontal { y * w + resample::address(x as i64 + d, w, edge) } else { resample::address(y as i64 + d, h, edge) * w + x };
                    acc = f(acc, src[i]);
                }
                *o = acc;
            }
        });
        out
    };
    let mn = pass(&pass(v, true, f32::min, f32::INFINITY), false, f32::min, f32::INFINITY);
    let mx = pass(&pass(v, true, f32::max, f32::NEG_INFINITY), false, f32::max, f32::NEG_INFINITY);
    (mn, mx)
}

/// Halo suppression window radius (output pixels) and margin (Oklab L, ~1 level).
const HALO_WINDOW: usize = 2;
const HALO_MARGIN: f32 = 0.004;

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x >= e0 { 1.0 } else { 0.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---- stages ----------------------------------------------------------------------------------------------------

/// Mean |high-pass| of opaque texels: how busy the texture already is (0.005 flat cel shading, 0.03+ noisy).
pub fn busyness(l: &[f32], blur: &[f32], wt: &[f32]) -> f32 {
    let (mut s, mut n) = (0f64, 0f64);
    for ((a, b), w) in l.iter().zip(blur).zip(wt) {
        s += ((a - b).abs() * w) as f64;
        n += *w as f64;
    }
    if n > 0.0 { (s / n) as f32 } else { 0.0 }
}

/// Adaptive factor from busyness: clean art gets up to +15 %, noisy art down to −40 %.
pub fn adaptive_factor(busy: f32) -> f32 {
    (1.25 - 25.0 * busy).clamp(0.6, 1.15)
}

fn sharpen_l(w: usize, h: usize, l: &mut [f32], wt: &[f32], p: &Sharpen, scale: u32, edge: Edge) -> f32 {
    let sigma = (if p.radius_in_source { p.radius * scale.max(1) as f32 } else { p.radius }).max(0.2);
    let blur = weighted_blur(w, h, l, wt, &gaussian_kernel(sigma), edge);
    let factor = if p.adaptive { adaptive_factor(busyness(l, &blur, wt)) } else { 1.0 };
    let amount = p.amount * factor;
    let thr = p.threshold.max(0.0) / 255.0;
    l.par_iter_mut().enumerate().for_each(|(i, v)| {
        let hp = *v - blur[i];
        let mag = hp.abs();
        let mut g = if thr > 0.0 { smoothstep(thr * 0.5, thr * 1.5, mag) } else { 1.0 };
        if p.edge_protection {
            g *= 1.0 - 0.6 * smoothstep(0.06, 0.25, mag);
        }
        if p.mode == SharpenMode::HighPass {
            let m = 2.0 * *v - 1.0;
            g *= (1.0 - m * m).max(0.0);
        }
        *v += amount * g * hp;
    });
    factor
}

/// Self-guided filter on lightness: `base` follows edges and flattens texture; texture = l - base is lifted.
fn detail_l(w: usize, h: usize, l: &mut [f32], wt: &[f32], p: &Detail, scale: u32, edge: Edge) {
    if p.amount <= 0.0 {
        return;
    }
    let r = (p.radius * scale as f32).round().max(1.0) as usize;
    let k = box_kernel(r);
    let eps = p.edge_level.max(1e-3).powi(2);
    let mean = |v: &[f32]| weighted_blur(w, h, v, wt, &k, edge);
    let ii: Vec<f32> = l.iter().map(|v| v * v).collect();
    let (m, mii) = (mean(l), mean(&ii));
    let a: Vec<f32> = m.iter().zip(&mii).map(|(m, mii)| {
        let var = (mii - m * m).max(0.0);
        var / (var + eps)
    }).collect();
    let b: Vec<f32> = a.iter().zip(&m).map(|(a, m)| m - a * m).collect();
    let (ma, mb) = (mean(&a), mean(&b));
    l.par_iter_mut().enumerate().for_each(|(i, v)| {
        let base = ma[i] * *v + mb[i];
        let d = (*v - base).clamp(-0.1, 0.1);
        *v += p.amount * d;
    });
}

/// Put back the original's texture (band one to two source texels wide) where the upscaled image has less of it.
fn restore_l(w: usize, h: usize, l: &mut [f32], wt: &[f32], strength: f32, scale: u32, orig_l: &[f32], ow: usize, oh: usize, edge: Edge) {
    if strength <= 0.0 || ow * scale as usize != w || oh * scale as usize != h {
        return;
    }
    let lz = resample::resize(ow, oh, 1, orig_l, w, h, Filter::Lanczos3, edge);
    let k = gaussian_kernel(0.5 * scale as f32);
    let (blz, bl) = (weighted_blur(w, h, &lz, wt, &k, edge), weighted_blur(w, h, l, wt, &k, edge));
    l.par_iter_mut().enumerate().for_each(|(i, v)| {
        let (hz, ha) = (lz[i] - blz[i], *v - bl[i]);
        let weight = (1.0 - ha.abs() / (hz.abs() + 1e-3)).clamp(0.0, 1.0);
        *v += strength * weight * (hz - ha).clamp(-0.08, 0.08);
    });
}

fn hue_weight(h_deg: f32, centre: f32) -> f32 {
    let d = ((h_deg - centre + 540.0) % 360.0 - 180.0).abs();
    if d >= 60.0 { 0.0 } else { 0.5 + 0.5 * (std::f32::consts::PI * d / 60.0).cos() }
}

/// Lightness and chroma adjustments for one texel; `mean_l` is the contrast pivot.
fn colour_lab(lab: [f32; 3], c: &Colour, mean_l: f32) -> [f32; 3] {
    let [mut l, mut a, mut b] = lab;
    if c.black_point != 0.0 || c.white_point != 1.0 {
        let span = (c.white_point - c.black_point).max(0.05);
        l = (l - c.black_point) / span;
    }
    if c.gamma != 1.0 && l > 0.0 {
        l = l.powf(1.0 / c.gamma.max(0.1));
    }
    if c.contrast != 0.0 {
        l = mean_l + (l - mean_l) * (1.0 + c.contrast);
    }
    if c.shadows != 0.0 || c.highlights != 0.0 {
        let ws = 1.0 - smoothstep(0.0, 0.5, l);
        let wh = smoothstep(0.5, 1.0, l);
        l += 0.15 * (c.shadows * ws + c.highlights * wh);
    }
    if c.temperature != 0.0 {
        b += 0.02 * c.temperature;
        a += 0.004 * c.temperature;
    }
    let chroma = (a * a + b * b).sqrt();
    if chroma > 1e-5 {
        let mut f = 1.0 + c.saturation;
        if c.vibrance != 0.0 {
            f *= 1.0 + c.vibrance * (1.0 - (chroma / 0.2).min(1.0));
        }
        if c.selective.iter().any(|v| *v != 0.0) {
            let hue = b.atan2(a).to_degrees().rem_euclid(360.0);
            f *= 1.0 + HUE_BANDS.iter().zip(&c.selective).map(|((_, centre), s)| s * hue_weight(hue, *centre)).sum::<f32>();
        }
        a *= f.max(0.0);
        b *= f.max(0.0);
    }
    [l, a, b]
}

/// Colour under fully transparent texels from the nearest visible texels (a few dilation passes, then the mean).
fn fill_transparent(w: usize, h: usize, rgba: &mut [u8]) {
    let mut known: Vec<bool> = rgba.chunks_exact(4).map(|p| p[3] > 0).collect();
    if known.iter().all(|k| *k) || !known.iter().any(|k| *k) {
        return;
    }
    for _ in 0..64 {
        let mut changed = false;
        let snapshot = rgba.to_vec();
        let prev = known.clone();
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if prev[i] {
                    continue;
                }
                let mut acc = [0u32; 3];
                let mut n = 0;
                for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1), (-1, -1), (1, -1), (-1, 1), (1, 1)] {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if prev[j] {
                        for c in 0..3 {
                            acc[c] += snapshot[j * 4 + c] as u32;
                        }
                        n += 1;
                    }
                }
                if n > 0 {
                    for c in 0..3 {
                        rgba[i * 4 + c] = ((acc[c] + n / 2) / n) as u8;
                    }
                    known[i] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Texels further than the passes reach: the mean visible colour.
    let vis: Vec<&[u8]> = rgba.chunks_exact(4).filter(|p| p[3] > 0).collect();
    let n = vis.len().max(1) as u32;
    let mean: Vec<u8> = (0..3).map(|c| (vis.iter().map(|p| p[c] as u32).sum::<u32>() / n) as u8).collect();
    for (p, k) in rgba.chunks_exact_mut(4).zip(&known) {
        if !k {
            p[..3].copy_from_slice(&mean);
        }
    }
}

/// Stats from one run, for logs and benchmarks.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub busyness_factor: f32,
    pub millis: f32,
}

/// Enhance an RGBA8 image (straight alpha). `original` is the source the upscale was made from, for restoration.
/// Alpha bytes are returned unchanged.
pub fn apply(w: u32, h: u32, rgba: &[u8], r: &Resolved, original: Option<(u32, u32, &[u8])>) -> (Vec<u8>, Stats) {
    let started = std::time::Instant::now();
    let mut stats = Stats { busyness_factor: 1.0, millis: 0.0 };
    if r.is_noop() {
        return (rgba.to_vec(), stats);
    }
    let (w, h) = (w as usize, h as usize);
    let edge = r.edge.unwrap_or(Edge::Clamp);
    let mut out = rgba.to_vec();
    if r.fill_transparent {
        fill_transparent(w, h, &mut out);
    }
    if r.sharpen.is_none() && r.detail.is_none() && r.colour.is_none() {
        stats.millis = started.elapsed().as_secs_f32() * 1000.0;
        return (out, stats);
    }
    let lut = decode_lut();
    let lab: Vec<[f32; 3]> = out.par_chunks_exact(4).map(|p| linear_to_oklab([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]])).collect();
    let wt: Vec<f32> = out.chunks_exact(4).map(|p| p[3] as f32 / 255.0).collect();
    let l0: Vec<f32> = lab.iter().map(|v| v[0]).collect();
    let mut l = l0.clone();

    if let Some(d) = &r.detail {
        if d.restore > 0.0 {
            if let Some((ow, oh, orig)) = original {
                let ol: Vec<f32> = orig.chunks_exact(4).map(|p| linear_to_oklab([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]])[0]).collect();
                restore_l(w, h, &mut l, &wt, d.restore, r.scale, &ol, ow as usize, oh as usize, edge);
            }
        }
        detail_l(w, h, &mut l, &wt, d, r.scale, edge);
    }
    if let Some(s) = &r.sharpen {
        stats.busyness_factor = sharpen_l(w, h, &mut l, &wt, s, r.scale, edge);
    }
    // Halo suppression for detail and sharpening together: every texel stays within the range of the unenhanced
    // image's 5×5 neighbourhood plus about one 8-bit level, so edges get steeper but no rim appears on either side.
    let suppress = r.sharpen.map_or(r.detail.is_some(), |s| s.halo_suppression);
    if suppress && (r.sharpen.is_some() || r.detail.is_some()) {
        let (mn, mx) = local_range(w, h, &l0, HALO_WINDOW, edge);
        l.par_iter_mut().enumerate().for_each(|(i, v)| *v = v.clamp(mn[i] - HALO_MARGIN, mx[i] + HALO_MARGIN));
    }
    let mean_l = {
        let (s, n) = l0.iter().zip(&wt).fold((0f64, 0f64), |(s, n), (v, a)| (s + (v * a) as f64, n + *a as f64));
        if n > 0.0 { (s / n) as f32 } else { 0.5 }
    };
    let (ph, ps) = r.colour.map(|c| (c.protect_highlights, c.protect_shadows)).unwrap_or((true, true));
    out.par_chunks_exact_mut(4).enumerate().for_each(|(i, p)| {
        if p[3] == 0 && !r.fill_transparent {
            return; // hidden colour stays as it was
        }
        let mut v = [l[i], lab[i][1], lab[i][2]];
        if let Some(c) = &r.colour {
            v = colour_lab(v, c, mean_l);
        }
        // Protection on the total lightness change: brightening fades out near white, darkening near black.
        let (lo, mut d) = (l0[i], v[0] - l0[i]);
        if d > 0.0 && ph {
            d *= ((1.0 - lo) / 0.1).clamp(0.0, 1.0);
        } else if d < 0.0 && ps {
            d *= (lo / 0.1).clamp(0.0, 1.0);
        }
        v[0] = (lo + d).clamp(0.0, 1.0);
        let rgb = to_gamut(v);
        for c in 0..3 {
            p[c] = (linear_to_srgb(rgb[c].clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
        }
    });
    stats.millis = started.elapsed().as_secs_f32() * 1000.0;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(class: Class) -> Target {
        Target { class, confidence: 0.9, manual: false, format: 0x431, scale: 4, edge: Edge::Wrap, ai: true }
    }

    /// Soft painted pattern with a hard outline and some transparency.
    fn art(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let t = ((x as f32 * 0.3).sin() * 0.5 + 0.5) * 120.0 + 60.0;
                let outline = (x % 32) < 2;
                let a = if alpha && y < h / 4 { 0 } else if alpha && y < h / 3 { 128 } else { 255 };
                let px = if outline { [20, 20, 30, a] } else { [t as u8, (t * 0.8) as u8, 90, a] };
                v.extend_from_slice(&px);
            }
        }
        v
    }

    #[test]
    fn oklab_round_trips() {
        let lut = decode_lut();
        for &(r, g, b) in &[(0u8, 0u8, 0u8), (255, 255, 255), (200, 30, 40), (12, 140, 230), (128, 128, 128)] {
            let lab = linear_to_oklab([lut[r as usize], lut[g as usize], lut[b as usize]]);
            let back = oklab_to_linear(lab);
            for (c, orig) in back.iter().zip([r, g, b]) {
                let v = (linear_to_srgb(c.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
                assert!((v as i32 - orig as i32).abs() <= 1, "{r},{g},{b}: {back:?}");
            }
        }
    }

    #[test]
    fn noop_profile_returns_the_exact_bytes() {
        let img = art(64, 64, true);
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour.enabled = false;
        let r = resolve(&p, &EnhanceSettings::default(), &target(Class::Rock), &img).unwrap();
        assert!(r.is_noop());
        assert_eq!(apply(64, 64, &img, &r, None).0, img);
    }

    #[test]
    fn neutral_colour_values_change_nothing_but_rounding() {
        let img = art(64, 64, false);
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour = Colour { saturation: 0.0, vibrance: 0.0, contrast: 0.0, ..Colour::default() };
        let r = resolve(&p, &EnhanceSettings::default(), &target(Class::Rock), &img).unwrap();
        assert!(r.colour.is_none(), "neutral colour is skipped");
    }

    #[test]
    fn alpha_is_never_changed_and_hidden_colour_stays() {
        let img = art(64, 64, true);
        for prof in builtins() {
            let r = resolve(&prof, &EnhanceSettings::default(), &target(Class::Foliage), &img).unwrap();
            let (out, _) = apply(64, 64, &img, &r, Some((16, 16, &art(16, 16, true))));
            for (a, b) in out.chunks_exact(4).zip(img.chunks_exact(4)) {
                assert_eq!(a[3], b[3], "{}", prof.name);
                if b[3] == 0 {
                    assert_eq!(a, b, "{}: colour under alpha 0 kept", prof.name);
                }
            }
        }
    }

    #[test]
    fn data_and_unknown_textures_are_never_enhanced() {
        let img = art(32, 32, false);
        let s = EnhanceSettings::default();
        for c in [Class::NormalMap, Class::Mask, Class::Ramp, Class::Unknown] {
            assert!(resolve(&enhanced(), &s, &target(c), &img).is_none(), "{c:?}");
        }
        // A colour class in a one/two-channel format is data too.
        for f in [0x34, 0x35, 0x234, 0x235, 0x1, 0x7] {
            let t = Target { format: f, ..target(Class::Rock) };
            assert!(resolve(&enhanced(), &s, &t, &img).is_none(), "format {f:#x}");
        }
        assert!(resolve(&enhanced(), &EnhanceSettings { enabled: false, ..s }, &target(Class::Rock), &img).is_none());
    }

    #[test]
    fn sharpening_raises_edge_contrast_without_halos() {
        // A soft step, like a smoothed AI edge.
        let (w, h) = (64u32, 8u32);
        let mut img = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                let t = 1.0 / (1.0 + (-(x as f32 - 32.0) / 1.5).exp());
                let v = (60.0 + t * 120.0) as u8;
                img.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut p = enhanced();
        p.detail.enabled = false;
        p.colour.enabled = false;
        p.sharpen = Sharpen { radius: 1.0, radius_in_source: false, threshold: 0.0, adaptive: false, ..Sharpen::default() };
        let t = Target { edge: Edge::Clamp, ..target(Class::Rock) };
        let r = resolve(&p, &EnhanceSettings::default(), &t, &img).unwrap();
        let (out, _) = apply(w, h, &img, &r, None);
        let row = |d: &[u8]| (0..w as usize).map(|x| d[x * 4] as i32).collect::<Vec<_>>();
        let (a, b) = (row(&img), row(&out));
        let grad = |r: &[i32]| (30..34).map(|x| (r[x + 1] - r[x]).abs()).max().unwrap();
        assert!(grad(&b) > grad(&a), "steeper edge: {} vs {}
{a:?}
{b:?}", grad(&b), grad(&a));
        // No overshoot beyond the input's range (halo suppression), allowing the 1 % margin and rounding.
        let (mn, mx) = (*a.iter().min().unwrap(), *a.iter().max().unwrap());
        assert!(b.iter().all(|v| *v >= mn - 4 && *v <= mx + 4), "{b:?}");
    }

    #[test]
    fn halo_suppression_limits_overshoot() {
        let (w, h) = (48u32, 4u32);
        let mut img = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < 24 { 50 } else { 200 };
                img.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut p = enhanced();
        p.detail.enabled = false;
        p.colour.enabled = false;
        p.sharpen = Sharpen { amount: 2.0, radius: 1.5, radius_in_source: false, edge_protection: false, adaptive: false, threshold: 0.0, ..Sharpen::default() };
        let t = Target { edge: Edge::Clamp, ..target(Class::Rock) };
        let mut r = resolve(&p, &EnhanceSettings::default(), &t, &img).unwrap();
        let over = |r: &Resolved| {
            let (o, _) = apply(w, h, &img, r, None);
            o.chunks_exact(4).map(|p| (p[0] as i32 - 200).max(50 - p[0] as i32).max(0)).max().unwrap()
        };
        let with = over(&r);
        r.sharpen.as_mut().unwrap().halo_suppression = false;
        let without = over(&r);
        assert!(without > 20 && with <= 4, "overshoot with {with}, without {without}");
    }

    #[test]
    fn threshold_keeps_flat_areas_flat() {
        // Noise of ±1 level must not be amplified with threshold 2.
        let (w, h) = (32u32, 32u32);
        let img: Vec<u8> = (0..w * h).flat_map(|i| {
            let v = 120 + (i * 7 % 3) as u8;
            [v, v, v, 255]
        }).collect();
        let mut p = enhanced();
        p.detail.enabled = false;
        p.colour.enabled = false;
        let r = resolve(&p, &EnhanceSettings::default(), &target(Class::Rock), &img).unwrap();
        let (out, _) = apply(w, h, &img, &r, None);
        let spread = |d: &[u8]| d.chunks_exact(4).map(|p| p[0]).max().unwrap() - d.chunks_exact(4).map(|p| p[0]).min().unwrap();
        assert!(spread(&out) <= spread(&img) + 1);
    }

    #[test]
    fn vibrance_boosts_muted_colours_more_than_saturated_ones() {
        let img: Vec<u8> = [[150u8, 130, 120, 255], [230, 30, 30, 255]].concat();
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour = Colour { saturation: 0.0, vibrance: 0.3, contrast: 0.0, ..Colour::default() };
        let r = resolve(&p, &EnhanceSettings::default(), &Target { scale: 1, ..target(Class::Rock) }, &img).unwrap();
        let (out, _) = apply(2, 1, &img, &r, None);
        let chroma = |p: &[u8]| {
            let lut = decode_lut();
            let l = linear_to_oklab([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]]);
            (l[1] * l[1] + l[2] * l[2]).sqrt()
        };
        let gain = |i: usize| chroma(&out[i * 4..i * 4 + 4]) / chroma(&img[i * 4..i * 4 + 4]);
        assert!(gain(0) > gain(1) && gain(0) > 1.1, "{} vs {}", gain(0), gain(1));
    }

    #[test]
    fn saturation_never_clips_or_shifts_hue_out_of_gamut() {
        let img: Vec<u8> = [[250u8, 10, 10, 255], [10, 250, 10, 255], [255, 255, 255, 255], [0, 0, 0, 255]].concat();
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour = Colour { saturation: 0.5, contrast: 0.2, ..Colour::default() };
        let r = resolve(&p, &EnhanceSettings::default(), &Target { scale: 1, ..target(Class::Rock) }, &img).unwrap();
        let (out, _) = apply(4, 1, &img, &r, None);
        // White and black stay white and black (highlight/shadow protection).
        assert_eq!(&out[8..12], &[255, 255, 255, 255]);
        assert_eq!(&out[12..16], &[0, 0, 0, 255]);
        // Saturated red stays red: dominant channel unchanged in order.
        assert!(out[0] > 240 && out[1] < 20 && out[2] < 20, "{:?}", &out[..4]);
    }

    #[test]
    fn edges_wrap_for_tiling_textures() {
        // A texture that tiles: a column at x = 0 and x = w-1 differ; sharpening must treat them as neighbours.
        let (w, h) = (16u32, 4u32);
        let img: Vec<u8> = (0..w * h).flat_map(|i| {
            let x = i % w;
            let v = if x == 0 { 200 } else { 80 + x as u8 };
            [v, v, v, 255]
        }).collect();
        let mut p = enhanced();
        p.detail.enabled = false;
        p.colour.enabled = false;
        let wrap = resolve(&p, &EnhanceSettings::default(), &target(Class::Rock), &img).unwrap();
        let clamp = resolve(&p, &EnhanceSettings::default(), &Target { edge: Edge::Clamp, ..target(Class::Rock) }, &img).unwrap();
        assert_ne!(apply(w, h, &img, &wrap, None).0, apply(w, h, &img, &clamp, None).0);
    }

    #[test]
    fn low_confidence_is_conservative_and_classes_differ() {
        let img = art(32, 32, false);
        let s = EnhanceSettings::default();
        let hi = resolve(&enhanced(), &s, &target(Class::Rock), &img).unwrap();
        let lo = resolve(&enhanced(), &s, &Target { confidence: 0.3, ..target(Class::Rock) }, &img).unwrap();
        assert!(lo.sharpen.unwrap().amount < hi.sharpen.unwrap().amount);
        let sky = resolve(&enhanced(), &s, &target(Class::Sky), &img).unwrap();
        assert!(sky.sharpen.unwrap().amount < hi.sharpen.unwrap().amount);
        let ui = resolve(&enhanced(), &s, &target(Class::Ui), &img).unwrap();
        assert!(ui.colour.is_none() && ui.detail.is_none(), "UI colours stay exact");
        // Manual classes are trusted.
        let manual = resolve(&enhanced(), &s, &Target { confidence: 0.3, manual: true, ..target(Class::Rock) }, &img).unwrap();
        assert_eq!(manual.sharpen.unwrap().amount, hi.sharpen.unwrap().amount);
    }

    #[test]
    fn detail_and_restore_need_an_upscale() {
        let img = art(32, 32, false);
        let r = resolve(&ultra(), &EnhanceSettings::default(), &Target { scale: 1, ..target(Class::Rock) }, &img).unwrap();
        assert!(r.detail.is_none());
        let r = resolve(&ultra(), &EnhanceSettings::default(), &Target { ai: false, ..target(Class::Rock) }, &img).unwrap();
        assert_eq!(r.detail.unwrap().restore, 0.0, "nothing to restore from a conventional filter");
    }

    #[test]
    fn fill_transparent_only_touches_hidden_colour_of_cutouts() {
        let (w, h) = (8u32, 8u32);
        let img: Vec<u8> = (0..w * h).flat_map(|i| if i % w < 4 { [200, 100, 50, 255] } else { [0, 0, 0, 0] }).collect();
        let s = EnhanceSettings { fill_transparent: true, ..EnhanceSettings::default() };
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour.enabled = false;
        let r = resolve(&p, &s, &Target { edge: Edge::Clamp, ..target(Class::Foliage) }, &img).unwrap();
        let (out, _) = apply(w, h, &img, &r, None);
        for (a, b) in out.chunks_exact(4).zip(img.chunks_exact(4)) {
            assert_eq!(a[3], b[3]);
            if b[3] == 0 {
                assert_eq!(&a[..3], &[200, 100, 50]);
            } else {
                assert_eq!(a, b);
            }
        }
    }

    #[test]
    fn profiles_resolve_by_name_and_unknown_names_fall_back() {
        let mut s = EnhanceSettings::default();
        assert_eq!(s.find(ULTRA).name, ULTRA);
        assert_eq!(s.find("nope").name, ENHANCED);
        let mut c = ultra();
        c.name = s.unique_name("Mine");
        s.custom.push(c);
        assert_eq!(s.find("Mine").sharpen.amount, ultra().sharpen.amount);
        assert_eq!(s.unique_name("Mine"), "Mine 2");
        assert_eq!(s.unique_name(ENHANCED), format!("{ENHANCED} 2"));
        // The defaults the spec asks for.
        let e = enhanced();
        assert_eq!((e.sharpen.radius, e.sharpen.amount, e.sharpen.threshold), (0.6, 0.55, 2.0));
        assert!(e.sharpen.edge_protection && e.sharpen.halo_suppression);
        assert_eq!((e.colour.saturation, e.colour.vibrance, e.colour.contrast, e.colour.gamma), (0.07, 0.05, 0.03, 1.0));
        assert!(e.colour.protect_highlights && e.colour.protect_shadows);
        assert_eq!(EnhanceSettings::default().profile, ENHANCED);
    }

    #[test]
    fn settings_without_the_new_fields_still_load() {
        let s: EnhanceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, EnhanceSettings::default());
        let p: Profile = serde_json::from_str(r#"{"name":"x","sharpen":{"amount":0.2}}"#).unwrap();
        assert_eq!(p.sharpen.amount, 0.2);
        assert_eq!(p.sharpen.radius, 0.6);
    }
}
