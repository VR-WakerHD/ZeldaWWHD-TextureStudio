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
//! Alpha is written only for cut-out textures with *crisp cut-out edges* on: the
//! edge is rebuilt from the original along the same contour, so coverage stays the
//! same. Blended alpha is never written. Filters weight neighbours by alpha, so the
//! colour hidden under transparent pixels cannot bleed into visible edges, and
//! pixels with alpha 0 keep their colour unless `fill_transparent` is on.
//!
//! Halo removal keeps the upscale's lightness within the original's local range,
//! which removes the rims upscalers (AI models and Lanczos alike) add along edges.
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
    /// How far (8-bit lightness levels) a texel may go past its neighbourhood's range with halo suppression on.
    /// About 1 keeps edges rim-free; a few levels add crispness (acutance) once an edge is fully steepened.
    pub halo_margin: f32,
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
            halo_margin: 1.0,
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

/// Fixes for upscaling artefacts, applied before sharpening and colour. Both work from the original texture.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cleanup {
    /// Remove the bright/dark rims upscalers add along strong edges (0 = off, 1 = full): lightness is kept within
    /// the original's local range (3x3 source texels, interpolated) plus about 2 levels. Detail the upscaler made
    /// inside that range stays.
    pub dering: f32,
    /// Cut-out textures (alpha mostly 0 or 255) get their alpha rebuilt from the original: the same smooth contour,
    /// re-sharpened to a narrow anti-aliased edge instead of the soft enlarged one. Blended alpha is left alone.
    pub crisp_cutouts: bool,
    /// Width of the anti-aliased cut-out edge in output pixels (0.8 = razor sharp but can shimmer in VR; 1.5, the
    /// default, chosen 2026-11-04 after comparing six settings on ten cut-out textures; 2-3 = softer).
    pub edge_width: f32,
    /// Smooths the cut-out outline before the edge is sharpened, in source texels (0 = off, default 0.5): irons out
    /// the original's pixel staircase on diagonals; too much rounds corners and thins strands.
    pub edge_smoothing: f32,
    /// Snaps the cut-out edge to the nearest edge in the upscaled colour (guided filter), so silhouettes line up
    /// with the painted outline.
    pub edge_follow_colour: bool,
}

impl Default for Cleanup {
    fn default() -> Self {
        Cleanup { dering: 1.0, crisp_cutouts: true, edge_width: 1.5, edge_smoothing: 0.5, edge_follow_colour: false }
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
    /// Upscaling artefact fixes (halo removal, crisp cut-out edges).
    pub cleanup: Cleanup,
}

impl Default for Profile {
    fn default() -> Self {
        enhanced()
    }
}

pub const ORIGINAL: &str = "Wind Waker Original";
pub const ENHANCED: &str = "Wind Waker Enhanced";
pub const ULTRA: &str = "Wind Waker Ultra Sharp";
pub const ULTRA_PLUS: &str = "Wind Waker Ultra Sharp +";
pub const ULTRA_VIBRANT: &str = "Wind Waker Ultra Sharp & Ultra Vibrant";
pub const ENHANCED_VIBRANT: &str = "Wind Waker Enhanced & Extra Vibrant";
/// The profile new projects use, and the fallback for a name that no longer exists.
pub const DEFAULT_PROFILE: &str = ENHANCED_VIBRANT;

/// Minimal sharpening, colours as painted.
pub fn original() -> Profile {
    Profile {
        name: ORIGINAL.into(),
        sharpen: Sharpen { amount: 0.25, threshold: 3.0, ..Sharpen::default() },
        detail: Detail { enabled: false, ..Detail::default() },
        colour: Colour { enabled: false, saturation: 0.0, vibrance: 0.0, contrast: 0.0, ..Colour::default() },
        per_class: true,
        sharp_mips: false,
        cleanup: Cleanup::default(),
    }
}

/// Moderate sharpening, mild vibrance and contrast, balanced detail recovery: the specification's defaults (the
/// project default is [`enhanced_vibrant`], which adds colour on top of this).
pub fn enhanced() -> Profile {
    Profile { name: ENHANCED.into(), sharpen: Sharpen::default(), detail: Detail::default(), colour: Colour::default(), per_class: true, sharp_mips: true, cleanup: Cleanup::default() }
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
        cleanup: Cleanup::default(),
    }
}

/// Stronger than Ultra Sharp: bolder radius, more sharpening and detail, lower threshold, and a 3-level halo
/// margin (a little overshoot for crispness). Edge protection stays on; colour as Ultra Sharp.
pub fn ultra_plus() -> Profile {
    Profile {
        name: ULTRA_PLUS.into(),
        sharpen: Sharpen { radius: 0.8, amount: 1.2, threshold: 1.5, halo_margin: 3.0, ..Sharpen::default() },
        detail: Detail { amount: 0.8, restore: 0.2, ..Detail::default() },
        colour: Colour { saturation: 0.08, vibrance: 0.08, contrast: 0.04, ..Colour::default() },
        per_class: true,
        sharp_mips: true,
        cleanup: Cleanup::default(),
    }
}

/// Ultra Sharp's sharpening and detail with much stronger colour: vibrance does most of the work (muted colours gain
/// most, saturated ones little), plus saturation and contrast. Highlight/shadow protection and gamut mapping keep
/// whites, blacks and hues; per-class tuning still holds water and sky back and leaves UI colours alone.
pub fn ultra_vibrant() -> Profile {
    Profile {
        name: ULTRA_VIBRANT.into(),
        colour: Colour { saturation: 0.12, vibrance: 0.25, contrast: 0.06, ..Colour::default() },
        ..ultra()
    }
}

/// Enhanced's sharpening and detail with stronger colour, between Ultra Sharp's and Ultra Vibrant's: for richer
/// colour without the stronger sharpening. Same protections and per-class tuning.
pub fn enhanced_vibrant() -> Profile {
    Profile {
        name: ENHANCED_VIBRANT.into(),
        colour: Colour { saturation: 0.10, vibrance: 0.18, contrast: 0.05, ..Colour::default() },
        ..enhanced()
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
    vec![original(), enhanced(), ultra(), ultra_plus(), ultra_vibrant(), enhanced_vibrant()]
}

pub fn is_builtin(name: &str) -> bool {
    [ORIGINAL, ENHANCED, ULTRA, ULTRA_PLUS, ULTRA_VIBRANT, ENHANCED_VIBRANT].contains(&name)
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
    /// Cut-out edge choice for every texture and every profile (default 1.5 px + smoothing).
    pub cutout_edges: CutoutChoice,
}

impl Default for EnhanceSettings {
    fn default() -> Self {
        EnhanceSettings { enabled: true, profile: DEFAULT_PROFILE.into(), custom: Vec::new(), fill_transparent: false, cutout_edges: CutoutChoice::Smooth15 }
    }
}

/// The cut-out edge options the user can pick (the variants compared on 2026-11-04). It decides the cut-out edges for
/// every profile; the profiles' own edge fields are no longer used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CutoutChoice {
    /// 1.5 px + smoothing, the default (a saved "FromProfile" from older builds reads as this).
    #[serde(alias = "FromProfile")]
    Smooth15,
    /// The soft enlarged edge, as before crisp edges existed.
    Soft,
    Crisp08,
    Crisp15,
    Crisp25,
    Follow15,
    SmoothFollow15,
}

impl Default for CutoutChoice {
    fn default() -> Self {
        CutoutChoice::Smooth15
    }
}

impl CutoutChoice {
    /// In menu order: the default first.
    pub const ALL: [CutoutChoice; 7] = [
        CutoutChoice::Smooth15,
        CutoutChoice::Soft,
        CutoutChoice::Crisp08,
        CutoutChoice::Crisp15,
        CutoutChoice::Crisp25,
        CutoutChoice::Follow15,
        CutoutChoice::SmoothFollow15,
    ];
    pub fn label(self) -> &'static str {
        match self {
            CutoutChoice::Soft => "Soft (no crisp edges)",
            CutoutChoice::Crisp08 => "Crisp 0.8 px (razor sharp)",
            CutoutChoice::Crisp15 => "Crisp 1.5 px",
            CutoutChoice::Crisp25 => "Crisp 2.5 px (softest crisp)",
            CutoutChoice::Smooth15 => "1.5 px + smoothing (default)",
            CutoutChoice::Follow15 => "1.5 px + follow colour",
            CutoutChoice::SmoothFollow15 => "1.5 px + smoothing + follow colour",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            CutoutChoice::Soft => "The enlarged edge, about 8 px of blur at x4: no stair-steps, but soft up close",
            CutoutChoice::Crisp08 => "Sharpest; keeps the original's pixel staircase and can shimmer in VR as you move",
            CutoutChoice::Crisp15 => "Crisp and steadier than 0.8 px; the staircase is still visible on curves",
            CutoutChoice::Crisp25 => "Still far crisper than soft, the steadiest in motion",
            CutoutChoice::Smooth15 => "Clean, smooth outlines without stair-steps; mild rounding of the finest tips",
            CutoutChoice::Follow15 => "Edges snap to the painted outline in the upscaled colour; keeps tips and corners",
            CutoutChoice::SmoothFollow15 => "Smoothed outline that then snaps to the painted colour edge",
        }
    }
    /// The clean-up settings this choice stands for: `base`'s halo removal, with this choice's cut-out edges.
    pub fn apply(self, base: &Cleanup) -> Cleanup {
        let c = |crisp: bool, width: f32, smoothing: f32, follow: bool| Cleanup { crisp_cutouts: crisp, edge_width: width, edge_smoothing: smoothing, edge_follow_colour: follow, ..*base };
        match self {
            CutoutChoice::Soft => c(false, 1.5, 0.0, false),
            CutoutChoice::Crisp08 => c(true, 0.8, 0.0, false),
            CutoutChoice::Crisp15 => c(true, 1.5, 0.0, false),
            CutoutChoice::Crisp25 => c(true, 2.5, 0.0, false),
            CutoutChoice::Smooth15 => c(true, 1.5, 0.5, false),
            CutoutChoice::Follow15 => c(true, 1.5, 0.0, true),
            CutoutChoice::SmoothFollow15 => c(true, 1.5, 0.5, true),
        }
    }
}

impl EnhanceSettings {
    /// A profile by name: built-in, then custom; an unknown name (e.g. a removed profile) falls back to the default.
    pub fn find(&self, name: &str) -> Profile {
        builtins().into_iter().chain(self.custom.iter().cloned()).find(|p| p.name == name).unwrap_or_else(enhanced_vibrant)
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
    Eyes,
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
            Category::Eyes => "Eyes",
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
        Class::Eyes => Category::Eyes,
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
        // Eyes: small, high-contrast, exact colours matter (an iris must keep its hue): gentle sharpening, no
        // invented detail, colours untouched.
        Category::Eyes => t(0.6, 0.0, 0.0),
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
    format!("enhance-v2:{}:{}:{:?}:{t:?}", serde_json::to_string(profile).unwrap_or_default(), s.fill_transparent, s.cutout_edges)
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
    /// Halo removal strength (0 = off); needs the original.
    #[serde(default)]
    pub dering: f32,
    /// Rebuild the alpha edge from the original when the original is a cut-out (alpha mostly 0 or 255).
    #[serde(default)]
    pub crisp_cutouts: bool,
    /// How the cut-out edge is rebuilt (width, smoothing, colour guidance).
    #[serde(default)]
    pub cutout: CutoutEdge,
}

/// The cut-out edge settings that apply to one texture.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CutoutEdge {
    pub width: f32,
    pub smoothing: f32,
    pub follow_colour: bool,
}

impl Default for CutoutEdge {
    fn default() -> Self {
        let c = Cleanup::default();
        CutoutEdge { width: c.edge_width, smoothing: c.edge_smoothing, follow_colour: c.edge_follow_colour }
    }
}

impl Resolved {
    /// A stable text of everything that decides the output, for encode-cache keys.
    pub fn tag(&self) -> String {
        format!("enhance-v1:{}:{:?}", serde_json::to_string(self).unwrap_or_default(), self.edge)
    }
    pub fn is_noop(&self) -> bool {
        self.sharpen.is_none() && self.detail.is_none() && self.colour.is_none() && !self.fill_transparent && self.dering <= 0.0 && !self.crisp_cutouts
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
    let upscaled = t.scale > 1;
    let dering = if upscaled { profile.cleanup.dering.clamp(0.0, 1.0) } else { 0.0 };
    // Whether the texture is a cut-out is decided from the original in `apply` (an enlarged cut-out has many soft
    // edge texels and no longer looks like one).
    let cleanup = s.cutout_edges.apply(&profile.cleanup);
    let crisp = upscaled && cleanup.crisp_cutouts;
    Some(Resolved {
        profile: profile.name.clone(),
        category,
        sharpen,
        detail,
        colour,
        scale: t.scale.max(1),
        edge: Some(t.edge),
        fill_transparent: fill,
        sharp_mips: profile.sharp_mips,
        dering,
        crisp_cutouts: crisp,
        cutout: CutoutEdge { width: cleanup.edge_width, smoothing: cleanup.edge_smoothing, follow_colour: cleanup.edge_follow_colour },
    })
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

/// Halo suppression window radius (output pixels), and the Oklab L of one 8-bit level for the margin.
const HALO_WINDOW: usize = 2;
const LEVEL: f32 = 0.004;

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

/// The cut-out alpha rebuilt from the original's alpha (`ow` x `oh`) at `w` x `h`: a smooth enlargement keeps the
/// contour (and so the coverage at the 0.5 test), then a steep ramp makes the edge `e.width` pixels wide.
///
/// `e.smoothing` blurs the enlarged alpha first (Gaussian, in source texels); `guide` (the upscale's lightness at the
/// output size) with `e.follow_colour` moves the outline onto the colour edge with a guided filter; `e.width` is the
/// width of the final ramp in output pixels. A hard step enlarged with Catmull-Rom rises over about one source texel
/// (`scale` pixels), so a ramp of `width` pixels spans `width / scale` of alpha.
pub fn crisp_alpha(ow: usize, oh: usize, orig: &[u8], w: usize, h: usize, scale: u32, edge: Edge, e: &CutoutEdge, guide: Option<&[f32]>) -> Vec<u8> {
    let a: Vec<f32> = orig.chunks_exact(4).map(|p| p[3] as f32 / 255.0).collect();
    let sc = scale.max(1) as f32;
    let mut up = resample::resize(ow, oh, 1, &a, w, h, Filter::CatmullRom, edge);
    if e.smoothing > 0.0 {
        let k = gaussian_kernel((e.smoothing * sc).max(0.3));
        up = convolve(w, h, &[&up], &k, edge).remove(0);
    }
    if e.follow_colour {
        if let Some(g) = guide {
            up = guided_filter(w, h, g, &up, (sc * 0.75).round().max(1.0) as usize, 0.02f32.powi(2), edge);
        }
    }
    let half = (e.width.max(0.25) / (2.0 * sc)).clamp(0.02, 0.5);
    up.par_iter().map(|v| (smoothstep(0.5 - half, 0.5 + half, *v) * 255.0 + 0.5) as u8).collect()
}

/// He et al.'s guided filter: `p` filtered so its edges follow those of `guide` (both `w` x `h`), window radius `r`.
fn guided_filter(w: usize, h: usize, guide: &[f32], p: &[f32], r: usize, eps: f32, edge: Edge) -> Vec<f32> {
    let k = box_kernel(r);
    let mean = |v: &[f32]| convolve(w, h, &[v], &k, edge).remove(0);
    let ip: Vec<f32> = guide.iter().zip(p).map(|(a, b)| a * b).collect();
    let ii: Vec<f32> = guide.iter().map(|a| a * a).collect();
    let (mi, mp, mip, mii) = (mean(guide), mean(p), mean(&ip), mean(&ii));
    let a: Vec<f32> = (0..w * h).map(|i| (mip[i] - mi[i] * mp[i]) / ((mii[i] - mi[i] * mi[i]).max(0.0) + eps)).collect();
    let b: Vec<f32> = (0..w * h).map(|i| mp[i] - a[i] * mi[i]).collect();
    let (ma, mb) = (mean(&a), mean(&b));
    (0..w * h).map(|i| ma[i] * guide[i] + mb[i]).collect()
}

/// Lightness bounds from the original: the 3x3-texel min and max around each source texel, enlarged smoothly to the
/// output size. An upscale's lightness outside these bounds is ringing or a halo.
fn source_bounds(ol: &[f32], ow: usize, oh: usize, w: usize, h: usize, edge: Edge) -> (Vec<f32>, Vec<f32>) {
    let (mn, mx) = local_range(ow, oh, ol, 1, edge);
    (resample::resize(ow, oh, 1, &mn, w, h, Filter::Bilinear, edge), resample::resize(ow, oh, 1, &mx, w, h, Filter::Bilinear, edge))
}

/// Halo removal margin (Oklab L, about 2 levels).
const DERING_MARGIN: f32 = 2.0 * LEVEL;

/// Stats from one run, for logs and benchmarks.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub busyness_factor: f32,
    pub millis: f32,
    /// Share of texels whose lightness halo removal pulled back.
    #[serde(default)]
    pub dering_fraction: f32,
    /// The alpha edge was rebuilt (the original is a cut-out).
    #[serde(default)]
    pub crisp_cutout: bool,
}

/// Enhance an RGBA8 image (straight alpha). `original` is the source the upscale was made from, for restoration.
/// Alpha bytes are returned unchanged.
pub fn apply(w: u32, h: u32, rgba: &[u8], r: &Resolved, original: Option<(u32, u32, &[u8])>) -> (Vec<u8>, Stats) {
    let started = std::time::Instant::now();
    let mut stats = Stats { busyness_factor: 1.0, millis: 0.0, dering_fraction: 0.0, crisp_cutout: false };
    if r.is_noop() {
        return (rgba.to_vec(), stats);
    }
    let (w, h) = (w as usize, h as usize);
    let edge = r.edge.unwrap_or(Edge::Clamp);
    let mut out = rgba.to_vec();
    // The original at the matching size, if given (crisp edges, halo removal and restoration need it).
    let original = original.filter(|(ow, oh, _)| *ow as usize * r.scale as usize == w && *oh as usize * r.scale as usize == h);
    if r.crisp_cutouts {
        if let Some((ow, oh, orig)) = original.filter(|(_, _, o)| crate::mips::is_alpha_tested(o) && !has_blended_alpha(o)) {
            stats.crisp_cutout = true;
            let guide: Option<Vec<f32>> = r.cutout.follow_colour.then(|| {
                let lut = decode_lut();
                out.par_chunks_exact(4).map(|p| linear_to_oklab([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]])[0]).collect()
            });
            let a = crisp_alpha(ow as usize, oh as usize, orig, w, h, r.scale, edge, &r.cutout, guide.as_deref());
            for (p, v) in out.chunks_exact_mut(4).zip(a) {
                p[3] = v;
            }
        }
    }
    if r.fill_transparent {
        fill_transparent(w, h, &mut out);
    }
    let dering = if original.is_some() { r.dering } else { 0.0 };
    if r.sharpen.is_none() && r.detail.is_none() && r.colour.is_none() && dering <= 0.0 {
        stats.millis = started.elapsed().as_secs_f32() * 1000.0;
        return (out, stats);
    }
    let lut = decode_lut();
    let lab: Vec<[f32; 3]> = out.par_chunks_exact(4).map(|p| linear_to_oklab([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]])).collect();
    let wt: Vec<f32> = out.chunks_exact(4).map(|p| p[3] as f32 / 255.0).collect();
    let l0: Vec<f32> = lab.iter().map(|v| v[0]).collect();
    let mut l = l0.clone();
    let orig_l: Option<(Vec<f32>, usize, usize)> = original.map(|(ow, oh, orig)| {
        (orig.par_chunks_exact(4).map(|p| linear_to_oklab([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]])[0]).collect(), ow as usize, oh as usize)
    });
    if dering > 0.0 {
        if let Some((ol, ow, oh)) = &orig_l {
            let (mn, mx) = source_bounds(ol, *ow, *oh, w, h, edge);
            let mut n = 0usize;
            for (i, v) in l.iter_mut().enumerate() {
                let c = v.clamp(mn[i] - DERING_MARGIN, mx[i] + DERING_MARGIN);
                if c != *v {
                    n += 1;
                }
                *v += dering * (c - *v);
            }
            stats.dering_fraction = n as f32 / l.len().max(1) as f32;
        }
    }
    // Later stages are held to the range of this cleaned image, so they cannot bring a removed halo back.
    let base = l.clone();

    if let Some(d) = &r.detail {
        if d.restore > 0.0 {
            if let Some((ol, ow, oh)) = &orig_l {
                restore_l(w, h, &mut l, &wt, d.restore, r.scale, ol, *ow, *oh, edge);
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
        let margin = r.sharpen.map_or(1.0, |s| s.halo_margin).clamp(0.0, 20.0) * LEVEL;
        let (mn, mx) = local_range(w, h, &base, HALO_WINDOW, edge);
        l.par_iter_mut().enumerate().for_each(|(i, v)| *v = v.clamp(mn[i] - margin, mx[i] + margin));
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
        p.cleanup = Cleanup { dering: 0.0, ..Cleanup::default() };
        let soft = EnhanceSettings { cutout_edges: CutoutChoice::Soft, ..EnhanceSettings::default() };
        let r = resolve(&p, &soft, &target(Class::Rock), &img).unwrap();
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
            let soft = EnhanceSettings { cutout_edges: CutoutChoice::Soft, ..EnhanceSettings::default() };
            let r = resolve(&prof, &soft, &target(Class::Foliage), &img).unwrap();
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
        assert_eq!(s.find("nope").name, DEFAULT_PROFILE);
        assert_eq!(s.find("Wind Waker Ultra Sharp ++").name, ENHANCED_VIBRANT, "the removed profile falls back to the default");
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
        assert_eq!(EnhanceSettings::default().profile, ENHANCED_VIBRANT);
        assert_eq!(DEFAULT_PROFILE, ENHANCED_VIBRANT);
    }

    #[test]
    fn ultra_sharp_plus_is_stronger_than_ultra_sharp_and_keeps_its_safeguards() {
        let (a, b) = (ultra(), ultra_plus());
        assert!(b.sharpen.amount > a.sharpen.amount && b.sharpen.radius > a.sharpen.radius && b.detail.amount > a.detail.amount);
        assert!(b.sharpen.halo_margin > a.sharpen.halo_margin);
        let img = art(64, 64, true);
        let measure = |p: &Profile| {
            let r = resolve(p, &EnhanceSettings::default(), &target(Class::Rock), &img).unwrap();
            let (out, _) = apply(64, 64, &img, &r, None);
            assert!(out.chunks_exact(4).zip(img.chunks_exact(4)).all(|(x, y)| x[3] == y[3]), "{}", p.name);
            (crate::quality::sharpness(64, 64, &out), crate::quality::mean_chroma(&out))
        };
        let (sa, _) = measure(&a);
        let (sb, _) = measure(&b);
        assert!(sb > sa, "{sa} vs {sb}");
        for p in [&a, &b, &ultra_vibrant()] {
            assert!(p.sharpen.halo_suppression && p.colour.protect_highlights && p.colour.protect_shadows && is_builtin(&p.name));
        }
    }

    #[test]
    fn ultra_vibrant_is_ultra_sharp_with_stronger_colour() {
        let (u, v) = (ultra(), ultra_vibrant());
        assert_eq!((v.sharpen, v.detail, v.sharp_mips, v.per_class), (u.sharpen, u.detail, u.sharp_mips, u.per_class));
        assert!(v.colour.vibrance > u.colour.vibrance && v.colour.saturation > u.colour.saturation && v.colour.contrast > u.colour.contrast);
        let img = art(64, 64, false);
        let chroma = |p: &Profile, c: Class| {
            let r = resolve(p, &EnhanceSettings::default(), &target(c), &img).unwrap();
            crate::quality::mean_chroma(&apply(64, 64, &img, &r, None).0)
        };
        assert!(chroma(&v, Class::Rock) > chroma(&u, Class::Rock));
        // UI colours stay exact even here.
        assert!(resolve(&v, &EnhanceSettings::default(), &target(Class::Ui), &img).unwrap().colour.is_none());
        assert!(!is_builtin("Wind Waker Ultra Sharp ++"));
    }

    #[test]
    fn enhanced_extra_vibrant_is_enhanced_with_colour_between_ultra_and_ultra_vibrant() {
        let (e, x, u, v) = (enhanced(), enhanced_vibrant(), ultra(), ultra_vibrant());
        assert_eq!((x.sharpen, x.detail, x.sharp_mips, x.per_class), (e.sharpen, e.detail, e.sharp_mips, e.per_class));
        for (a, b, c) in [(u.colour.vibrance, x.colour.vibrance, v.colour.vibrance), (u.colour.saturation, x.colour.saturation, v.colour.saturation), (u.colour.contrast, x.colour.contrast, v.colour.contrast)] {
            assert!(a < b && b < c, "{a} < {b} < {c}");
        }
        assert!(is_builtin(&x.name) && x.colour.protect_highlights && x.colour.protect_shadows);
    }

    /// A cut-out disc: alpha 255 inside radius `r` (in source texels), 0 outside.
    fn disc(n: u32, r: f32) -> Vec<u8> {
        (0..n * n)
            .flat_map(|i| {
                let (x, y) = ((i % n) as f32 + 0.5 - n as f32 / 2.0, (i / n) as f32 + 0.5 - n as f32 / 2.0);
                [90, 160, 60, if (x * x + y * y).sqrt() < r { 255 } else { 0 }]
            })
            .collect()
    }

    fn soft_upscale(n: u32, src: &[u8], scale: u32) -> Vec<u8> {
        resample::resize_rgba8(n, n, src, n * scale, n * scale, Filter::CatmullRom, Edge::Clamp)
    }

    #[test]
    fn crisp_cutouts_narrow_the_edge_and_keep_the_coverage() {
        let (n, sc) = (16u32, 4u32);
        let src = disc(n, 5.0);
        let soft = soft_upscale(n, &src, sc);
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour.enabled = false;
        p.cleanup.dering = 0.0;
        let t = Target { edge: Edge::Clamp, ..target(Class::Foliage) };
        let r = resolve(&p, &EnhanceSettings::default(), &t, &soft).unwrap();
        assert!(r.crisp_cutouts, "a cut-out texture");
        let (out, st) = apply(n * sc, n * sc, &soft, &r, Some((n, n, &src)));
        assert!(st.crisp_cutout);
        let partial = |d: &[u8]| d.chunks_exact(4).filter(|p| p[3] > 16 && p[3] < 240).count();
        let cover = |d: &[u8]| d.chunks_exact(4).filter(|p| p[3] >= 128).count() as f32;
        assert!(partial(&out) * 2 < partial(&soft), "edge pixels {} vs {}", partial(&out), partial(&soft));
        assert!((cover(&out) / cover(&soft) - 1.0).abs() < 0.03, "coverage {} vs {}", cover(&out), cover(&soft));
        // Colour is not touched by this step.
        assert!(out.chunks_exact(4).zip(soft.chunks_exact(4)).all(|(a, b)| a[..3] == b[..3]));
        // Without the original nothing can be rebuilt: alpha stays.
        assert_eq!(apply(n * sc, n * sc, &soft, &r, None).0, soft);
    }

    #[test]
    fn the_project_cutout_choice_overrides_any_profile() {
        let (n, sc) = (16u32, 4u32);
        let src = disc(n, 5.0);
        let soft = soft_upscale(n, &src, sc);
        let t = Target { edge: Edge::Clamp, ..target(Class::Foliage) };
        let resolve_with = |c: CutoutChoice, p: &Profile| resolve(p, &EnhanceSettings { cutout_edges: c, ..EnhanceSettings::default() }, &t, &soft).unwrap();
        for p in builtins() {
            let r = resolve_with(CutoutChoice::Crisp25, &p);
            assert!(r.crisp_cutouts && r.cutout.width == 2.5 && r.cutout.smoothing == 0.0, "{}", p.name);
            assert!(!resolve_with(CutoutChoice::Soft, &p).crisp_cutouts, "{}", p.name);
            let mut odd = p.clone();
            odd.cleanup = Cleanup { crisp_cutouts: false, edge_width: 3.5, edge_smoothing: 0.0, edge_follow_colour: true, ..p.cleanup };
            let picked = resolve_with(CutoutChoice::Smooth15, &odd);
            assert!(picked.crisp_cutouts && picked.cutout == CutoutEdge { width: 1.5, smoothing: 0.5, follow_colour: false }, "the profile's own edge fields are ignored");
            let both = resolve_with(CutoutChoice::SmoothFollow15, &p);
            assert!(both.cutout.follow_colour && both.cutout.smoothing == 0.5);
        }
        // Each choice gives its own cache tag, so a change re-encodes.
        let tags: std::collections::HashSet<String> = CutoutChoice::ALL.iter().map(|c| cache_tag(&enhanced(), &EnhanceSettings { cutout_edges: *c, ..EnhanceSettings::default() }, &t)).collect();
        assert_eq!(tags.len(), CutoutChoice::ALL.len());
        // Halo removal is not touched by the choice.
        assert_eq!(CutoutChoice::Soft.apply(&Cleanup::default()).dering, Cleanup::default().dering);
    }

    #[test]
    fn the_default_cutout_edge_is_1_5_px_with_mild_smoothing() {
        assert_eq!(EnhanceSettings::default().cutout_edges, CutoutChoice::Smooth15);
        assert_eq!(CutoutChoice::ALL[0], CutoutChoice::Smooth15, "first in the menu");
        let old: EnhanceSettings = serde_json::from_str(r#"{"cutout_edges":"FromProfile"}"#).unwrap();
        assert_eq!(old.cutout_edges, CutoutChoice::Smooth15);
        let c = Cleanup::default();
        assert_eq!((c.edge_width, c.edge_smoothing, c.edge_follow_colour), (1.5, 0.5, false));
        assert!(c.crisp_cutouts);
    }

    #[test]
    fn a_wider_cutout_edge_has_more_soft_pixels_and_the_same_coverage() {
        let (n, sc) = (16u32, 4u32);
        let src = disc(n, 5.0);
        let soft = soft_upscale(n, &src, sc);
        let run = |width: f32, smoothing: f32| {
            let mut p = enhanced();
            p.sharpen.enabled = false;
            p.detail.enabled = false;
            p.colour.enabled = false;
            p.cleanup.dering = 0.0;
            let mut r = resolve(&p, &EnhanceSettings::default(), &Target { edge: Edge::Clamp, ..target(Class::Foliage) }, &soft).unwrap();
            r.cutout = CutoutEdge { width, smoothing, follow_colour: false };
            apply(n * sc, n * sc, &soft, &r, Some((n, n, &src))).0
        };
        let partial = |d: &[u8]| d.chunks_exact(4).filter(|p| p[3] > 16 && p[3] < 240).count();
        let cover = |d: &[u8]| d.chunks_exact(4).filter(|p| p[3] >= 128).count() as f32;
        let (a, b, c) = (run(0.8, 0.0), run(1.5, 0.0), run(3.0, 0.0));
        assert!(partial(&a) < partial(&b) && partial(&b) < partial(&c), "{} {} {}", partial(&a), partial(&b), partial(&c));
        for x in [&b, &c, &run(1.5, 0.5)] {
            assert!((cover(x) / cover(&a) - 1.0).abs() < 0.04, "coverage {} vs {}", cover(x), cover(&a));
        }
    }

    #[test]
    fn following_colour_puts_the_cutout_edge_on_the_colour_edge() {
        // Original 16x16: a cut-out half plane x < 8. The upscale's colour edge sits 2 px right of where the alpha
        // contour falls (the AI drew the outline slightly differently); with colour guidance the alpha moves to it.
        let (n, sc) = (16u32, 4u32);
        let src: Vec<u8> = (0..n * n).flat_map(|i| if i % n < 8 { [200, 60, 40, 255] } else { [0, 0, 0, 0] }).collect();
        let w = n * sc;
        let up: Vec<u8> = (0..w * w).flat_map(|i| if i % w < 34 { [200, 60, 40, 255] } else { [0, 0, 0, 255] }).collect();
        let contour = |d: &[u8]| (0..w as usize).find(|&x| d[x * 4 + 3] < 128).unwrap();
        let run = |follow: bool| {
            let mut p = enhanced();
            p.sharpen.enabled = false;
            p.detail.enabled = false;
            p.colour.enabled = false;
            p.cleanup.dering = 0.0;
            let mut r = resolve(&p, &EnhanceSettings::default(), &Target { edge: Edge::Clamp, ..target(Class::Foliage) }, &up).unwrap();
            r.cutout = CutoutEdge { width: 0.8, smoothing: 0.0, follow_colour: follow };
            apply(w, w, &up, &r, Some((n, n, &src))).0
        };
        let (plain, guided) = (contour(&run(false)), contour(&run(true)));
        assert_eq!(plain, 32, "the original's contour");
        assert!(guided > plain && guided <= 35, "moved towards the colour edge at 34: {guided}");
    }

    #[test]
    fn blended_alpha_is_never_rebuilt() {
        let n = 16u32;
        let src: Vec<u8> = (0..n * n).flat_map(|i| [120, 120, 120, (i * 255 / (n * n)) as u8]).collect();
        let up = soft_upscale(n, &src, 4);
        let r = resolve(&enhanced(), &EnhanceSettings::default(), &Target { edge: Edge::Clamp, ..target(Class::Particle) }, &up).unwrap();
        let (out, st) = apply(n * 4, n * 4, &up, &r, Some((n, n, &src)));
        assert!(!st.crisp_cutout, "the original's alpha blends: not a cut-out");
        assert!(out.chunks_exact(4).zip(up.chunks_exact(4)).all(|(a, b)| a[3] == b[3]));
    }

    #[test]
    fn halo_removal_pulls_upscaler_rims_back_to_the_originals_range() {
        // Original: a hard step 60 | 200. "Upscale": the step x4 with a bright rim (245) and a dark rim (15) beside it.
        let (n, sc) = (16u32, 4u32);
        let src: Vec<u8> = (0..n * n).flat_map(|i| { let v = if i % n < 8 { 60 } else { 200 }; [v, v, v, 255] }).collect();
        let w = n * sc;
        let up: Vec<u8> = (0..w * w)
            .flat_map(|i| {
                let x = i % w;
                let v = match x { 30 | 31 => 15, 32 | 33 => 245, x if x < 32 => 60, _ => 200 };
                [v, v, v, 255]
            })
            .collect();
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour.enabled = false;
        let r = resolve(&p, &EnhanceSettings::default(), &Target { edge: Edge::Clamp, ..target(Class::Rock) }, &up).unwrap();
        assert!(r.dering > 0.0);
        let (out, st) = apply(w, w, &up, &r, Some((n, n, &src)));
        let row: Vec<i32> = (0..w as usize).map(|x| out[x * 4] as i32).collect();
        assert!(row.iter().all(|v| (55..=206).contains(v)), "rims removed: {:?}", &row[26..38]);
        assert!(st.dering_fraction > 0.0);
        // Flat areas far from the edge are untouched.
        assert_eq!((row[5], row[60]), (60, 200));
        // Off: the rims stay.
        p.cleanup.dering = 0.0;
        let r = resolve(&p, &EnhanceSettings::default(), &Target { edge: Edge::Clamp, ..target(Class::Rock) }, &up).unwrap();
        assert_eq!(apply(w, w, &up, &r, Some((n, n, &src))).0, up);
    }

    #[test]
    fn halo_removal_keeps_detail_inside_the_originals_range() {
        // Texture whose upscale adds in-range detail (values between the original's local min and max).
        let (n, sc) = (16u32, 4u32);
        let src: Vec<u8> = (0..n * n).flat_map(|i| { let v = if (i % n + i / n) % 2 == 0 { 80 } else { 180 }; [v, v, v, 255] }).collect();
        let w = n * sc;
        let up: Vec<u8> = (0..w * w).flat_map(|i| { let v = 100 + ((i * 37) % 60) as u8; [v, v, v, 255] }).collect();
        let mut p = enhanced();
        p.sharpen.enabled = false;
        p.detail.enabled = false;
        p.colour.enabled = false;
        let r = resolve(&p, &EnhanceSettings::default(), &Target { edge: Edge::Wrap, ..target(Class::Rock) }, &up).unwrap();
        let (out, _) = apply(w, w, &up, &r, Some((n, n, &src)));
        assert_eq!(out, up, "everything is inside 80..180, so nothing changes");
    }

    #[test]
    fn settings_without_the_new_fields_still_load() {
        let s: EnhanceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, EnhanceSettings::default());
        let p: Profile = serde_json::from_str(r#"{"name":"x","sharpen":{"amount":0.2}}"#).unwrap();
        assert_eq!(p.sharpen.amount, 0.2);
        assert_eq!(p.cleanup, Cleanup::default(), "profiles saved before clean-up existed get it on");
        assert_eq!(p.sharpen.radius, 0.6);
    }
}
