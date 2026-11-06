//! Texture classification from metadata first (container, format, name
//! fragments used by the game's artists), then simple image statistics.
//! Every result carries a confidence; uncertain textures land in classes whose
//! processing is non-destructive (conventional resizing or native size).

use serde::{Deserialize, Serialize};

use crate::container::LeafKind;
use crate::gx2::{self, Codec};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Class {
    CharacterAlbedo,
    /// Character and creature eyes (irises, pupils, eye whites, highlights). Own class so they can get their own
    /// model and scale; unset, they follow Character / object colour.
    Eyes,
    EnvironmentAlbedo,
    Terrain,
    Foliage,
    Rock,
    Wood,
    Water,
    Sky,
    Ui,
    Lettering,
    Particle,
    NormalMap,
    Mask,
    /// Lighting ramps / lookup tables (e.g. toon shading). Never resized.
    Ramp,
    Unknown,
}

pub const ALL_CLASSES: [Class; 16] = [
    Class::CharacterAlbedo,
    Class::Eyes,
    Class::EnvironmentAlbedo,
    Class::Terrain,
    Class::Foliage,
    Class::Rock,
    Class::Wood,
    Class::Water,
    Class::Sky,
    Class::Ui,
    Class::Lettering,
    Class::Particle,
    Class::NormalMap,
    Class::Mask,
    Class::Ramp,
    Class::Unknown,
];

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::CharacterAlbedo => "Character / object colour",
            Class::Eyes => "Eyes",
            Class::EnvironmentAlbedo => "Environment colour",
            Class::Terrain => "Terrain",
            Class::Foliage => "Grass & foliage",
            Class::Rock => "Rock & stone",
            Class::Wood => "Wood & architecture",
            Class::Water => "Water",
            Class::Sky => "Sky & clouds",
            Class::Ui => "UI",
            Class::Lettering => "Icons & lettering",
            Class::Particle => "Particle effect",
            Class::NormalMap => "Normal map",
            Class::Mask => "Mask / material data",
            Class::Ramp => "Lighting ramp / lookup",
            Class::Unknown => "Unknown",
        }
    }
    /// Colour classes that AI models may process.
    pub fn is_colour(self) -> bool {
        matches!(
            self,
            Class::CharacterAlbedo
                | Class::Eyes
                | Class::EnvironmentAlbedo
                | Class::Terrain
                | Class::Foliage
                | Class::Rock
                | Class::Wood
                | Class::Water
                | Class::Sky
                | Class::Particle
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Classification {
    pub class: Class,
    pub confidence: f32,
    pub reason: String,
    pub manual: bool,
}

fn has(name: &str, words: &[&str]) -> bool {
    words.iter().any(|w| name.contains(w))
}

/// Whether a texture is a two-channel image the game shows as luminance + alpha: a BC5/RG8 surface whose component
/// selector sends R to the three colour channels and G to alpha (BFRES `comp_sel` [0, 0, 0, 1]; BFLIM one/two-channel
/// images are always luminance-based). These hold artwork (eyes, eyebrows, mouths, cursors, numbers, cracks), not
/// normal vectors: 635 of the game's 636 BFRES BC5 textures are like this, the one real normal map (`himo_nml`)
/// has `comp_sel` [0, 1, 4, 5].
pub fn is_luminance_alpha(kind: LeafKind, format: u32, comp_sel: [u8; 4]) -> bool {
    matches!(gx2::codec(format), Ok(Codec::Bc5) | Ok(Codec::Rg8)) && (kind != LeafKind::Bfres || comp_sel == [0, 0, 0, 1])
}

/// Name fragments that mark a normal map.
const NORMAL_NAMES: [&str; 5] = ["_nml", "nml", "_nrm", "normal", "_n."];

fn named_like_a_normal_map(n: &str) -> bool {
    has(n, &NORMAL_NAMES) || n.ends_with("_n")
}

/// Whether earlier builds classed this texture as a normal map although it is not one: two-channel luminance +
/// alpha BFRES textures (any BC5 was a normal map) and colour (sRGB) textures with "normal" in the name, such as
/// `Ganonjo_NormalDoor_cl`. Their cached upscales went through the normal-map path and are made again.
pub fn misread_as_normal_map_before(kind: LeafKind, name: &str, format: u32, comp_sel: [u8; 4]) -> bool {
    (kind == LeafKind::Bfres && is_luminance_alpha(kind, format, comp_sel)) || (gx2::is_srgb(format) && named_like_a_normal_map(&name.to_ascii_lowercase()))
}

/// Classify from container kind, location, name and format, for a surface whose component selector is the identity.
pub fn classify(kind: LeafKind, file: &str, name: &str, format: u32, width: u32, height: u32) -> Classification {
    classify_texture(kind, file, name, format, width, height, [0, 1, 2, 3])
}

/// Classify from container kind, location, name, format and component selector.
pub fn classify_texture(kind: LeafKind, file: &str, name: &str, format: u32, width: u32, height: u32, comp_sel: [u8; 4]) -> Classification {
    let mut cl = classify_inner(kind, file, name, format, width, height, comp_sel);
    if is_luminance_alpha(kind, format, comp_sel) && kind == LeafKind::Bfres {
        cl.reason = format!("{}; two-channel luminance + alpha", cl.reason);
    }
    cl
}

fn classify_inner(kind: LeafKind, file: &str, name: &str, format: u32, width: u32, height: u32, comp_sel: [u8; 4]) -> Classification {
    let lum_alpha = is_luminance_alpha(kind, format, comp_sel);
    let n = name.to_ascii_lowercase();
    let f = file.to_ascii_lowercase();
    let codec = gx2::codec(format).ok();
    let c = |class, confidence: f32, reason: &str| Classification { class, confidence, reason: reason.into(), manual: false };

    if has(&n, &["toon", "ramp", "lut", "gradation"]) || (height <= 4 && width >= 16) || (width <= 4 && height >= 16) {
        return c(Class::Ramp, 0.9, "name or shape marks a lighting ramp / lookup table");
    }
    if kind == LeafKind::Bflim || f.contains("/layout/") || f.contains("permanent_2d") {
        if has(&n, &["font", "txt", "text", "msg", "logo", "num", "btn", "icon", "mark", "sign"]) {
            return c(Class::Lettering, 0.75, "layout image named like text, a button or an icon");
        }
        return c(Class::Ui, 0.85, "layout (BFLIM) image");
    }
    // Normal maps are never sRGB: "Ganonjo_NormalDoor_cl" is a door's colour ("normal door").
    if named_like_a_normal_map(&n) && !gx2::is_srgb(format) {
        return c(Class::NormalMap, 0.9, "name marks a normal map");
    }
    // Two-channel luminance + alpha is artwork: classified by name and location below, like colour textures.
    if codec == Some(Codec::Bc5) && !lum_alpha {
        return c(Class::NormalMap, 0.6, "BC5 two-channel texture (normal maps use BC5)");
    }
    if matches!(codec, Some(Codec::Bc4) | Some(Codec::R8)) || (codec == Some(Codec::Rg8) && !lum_alpha) {
        return c(Class::Mask, 0.7, "single/dual-channel format holds mask or material data");
    }
    if has(&n, &["mask", "spec", "rough", "metal", "ao_", "_ao", "shadow", "kage", "alpha_", "_a."]) {
        return c(Class::Mask, 0.7, "name marks material data");
    }
    if f.contains("/particle/") || has(&n, &["ak_", "fx_", "effect", "smoke", "fire", "spark", "hinoko", "kemuri"]) {
        return c(Class::Particle, 0.8, "particle archive or effect name");
    }
    if has(&n, &["eye", "hitomi", "pupil"]) && !has(&n, &["leaf", "happa"]) {
        return c(Class::Eyes, 0.8, "eye name");
    }
    if has(&n, &["sky", "cloud", "kumo", "_vr", "vr_", "sora", "star", "hoshi", "moon", "sun"]) {
        return c(Class::Sky, 0.75, "sky/cloud name");
    }
    if has(&n, &["water", "umi", "sea", "nami", "mizu", "wave", "taki", "kawa", "nagare", "awa"]) {
        return c(Class::Water, 0.75, "water/wave name");
    }
    if has(&n, &["kusa", "grass", "leaf", "happa", "ha_", "tree", "ki_", "flower", "hana", "tuta", "ivy", "shiba"]) {
        return c(Class::Foliage, 0.65, "plant name");
    }
    if has(&n, &["iwa", "rock", "ishi", "stone", "gake", "cliff", "renga", "brick"]) {
        return c(Class::Rock, 0.65, "rock/stone name");
    }
    if has(&n, &["wood", "ita", "mokuzai", "kabe", "wall", "yane", "roof", "house", "ie_", "hashi", "bridge"]) {
        return c(Class::Wood, 0.55, "wood/architecture name");
    }
    if has(&n, &["tsuchi", "jimen", "ground", "suna", "sand", "tile", "yuka", "floor", "michi", "road", "dirt"]) {
        return c(Class::Terrain, 0.6, "ground/floor name");
    }
    if f.contains("/stage/") {
        return c(Class::EnvironmentAlbedo, 0.6, "stage or room archive");
    }
    if f.contains("/object/") || f.contains("permanent") || f.contains("/programtexture/") {
        return c(Class::CharacterAlbedo, 0.55, "object/actor archive");
    }
    c(Class::Unknown, 0.2, "no rule matched")
}

/// Refine with image statistics: a texture whose pixels are all grey is mask
/// data even when its name looked like colour.
pub fn refine_with_pixels(mut cl: Classification, rgba: &[u8]) -> Classification {
    if cl.manual || !cl.class.is_colour() || rgba.len() < 16 {
        return cl;
    }
    let mut grey = true;
    for p in rgba.chunks_exact(4).step_by(7) {
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        if (r - g).abs() > 6 || (g - b).abs() > 6 {
            grey = false;
            break;
        }
    }
    if grey {
        cl.reason = format!("{}; but all pixels are grey", cl.reason);
        cl.confidence = (cl.confidence * 0.8).min(0.6);
        // Grey colour textures (stone, metal) are still colour; only very low
        // confidence results are moved to the non-destructive class.
        if cl.confidence < 0.45 {
            cl.class = Class::Mask;
        }
    }
    cl
}

#[cfg(test)]
mod lum_alpha_tests {
    use super::*;

    #[test]
    fn luminance_alpha_bc5_is_artwork_and_real_normal_maps_stay_normal_maps() {
        let eye = classify_texture(LeafKind::Bfres, "content/Common/Pack/permanent_3d.pack", "eyeh.2", 0x35, 384, 384, [0, 0, 0, 1]);
        assert_eq!(eye.class, Class::Eyes, "{}", eye.reason);
        assert!(eye.reason.contains("luminance + alpha"));
        let brow = classify_texture(LeafKind::Bfres, "content/Common/Pack/permanent_3d.pack", "mayuh.1", 0x35, 256, 256, [0, 0, 0, 1]);
        assert_eq!(brow.class, Class::CharacterAlbedo, "{}", brow.reason);
        let stage = classify_texture(LeafKind::Bfres, "content/Common/Stage/M_NewD2_Room2.szs", "Txm_DDR02deco01", 0x35, 512, 512, [0, 0, 0, 1]);
        assert_eq!(stage.class, Class::EnvironmentAlbedo);
        // The game's real normal map: R, G = X, Y; B, A constant.
        assert_eq!(classify_texture(LeafKind::Bfres, "content/Common/ProgramTexture/x.szs", "himo_nml", 0x235, 32, 32, [0, 1, 4, 5]).class, Class::NormalMap);
        assert_eq!(classify_texture(LeafKind::Bfres, "content/Common/Object/x.szs", "plain_bc5", 0x35, 32, 32, [0, 1, 4, 5]).class, Class::NormalMap);
        // A lighting ramp in the same layout is still a ramp.
        assert_eq!(classify_texture(LeafKind::Bfres, "content/Common/Object/x.szs", "toon_bin", 0x35, 256, 4, [0, 0, 0, 1]).class, Class::Ramp);
        assert!(is_luminance_alpha(LeafKind::Bflim, 0x35, [0, 1, 2, 3]));
        assert!(!is_luminance_alpha(LeafKind::Bfres, 0x35, [0, 1, 4, 5]));
        assert!(!is_luminance_alpha(LeafKind::Bfres, 0x433, [0, 0, 0, 1]), "only two-channel formats");
    }

    #[test]
    fn eyes_get_their_own_class_but_not_leaves_effects_or_single_channel_masks() {
        let cl = |name: &str, file: &str, format: u32| classify_texture(LeafKind::Bfres, file, name, format, 128, 128, [0, 1, 2, 3]).class;
        assert_eq!(cl("link_eye", "content/Common/Object/Figure2.szs", 0x433), Class::Eyes);
        assert_eq!(cl("hitomi", "content/Common/Object/Demo07.szs", 0x431), Class::Eyes);
        assert_eq!(cl("by_eye.1", "content/Common/Object/Bms.szs", 0x433), Class::Eyes);
        assert_eq!(cl("dk_eyeleaf", "content/Common/Object/Demo14.szs", 0x431), Class::Foliage, "a leaf");
        assert_eq!(cl("AK_jabuEye", "content/Common/Object/Demo18.szs", 0x433), Class::Particle, "an effect");
        assert_eq!(classify_texture(LeafKind::Bfres, "content/Common/Pack/permanent_3d.pack", "eyeh.8", 0x34, 32, 32, [0, 0, 0, 0]).class, Class::Mask, "single-channel data");
        assert_eq!(classify_texture(LeafKind::Bflim, "content/Common/Layout/x.szs", "SquidEye_00^t.bflim", 0x35, 16, 16, [0, 1, 2, 3]).class, Class::Ui);
    }

    #[test]
    fn a_colour_texture_named_normal_is_not_a_normal_map() {
        let door = classify_texture(LeafKind::Bfres, "content/Common/Stage/GanonJ_Room1.szs", "Ganonjo_NormalDoor_cl", 0x431, 1024, 1024, [0, 1, 2, 3]);
        assert_eq!(door.class, Class::EnvironmentAlbedo, "{}", door.reason);
        assert!(misread_as_normal_map_before(LeafKind::Bfres, "Ganonjo_NormalDoor_cl", 0x431, [0, 1, 2, 3]));
        // Real normal maps (never sRGB) keep their class and are not "misread".
        assert_eq!(classify_texture(LeafKind::Bfres, "content/Common/Object/x.szs", "wall_nrm", 0x31, 64, 64, [0, 1, 2, 3]).class, Class::NormalMap);
        assert!(!misread_as_normal_map_before(LeafKind::Bfres, "himo_nml", 0x235, [0, 1, 4, 5]));
        assert!(misread_as_normal_map_before(LeafKind::Bfres, "eyeh.2", 0x35, [0, 0, 0, 1]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert_eq!(classify(LeafKind::Bfres, "Common/Object/Bdk.szs", "toon", 0x34, 1024, 32).class, Class::Ramp);
        assert_eq!(classify(LeafKind::Bflim, "Common/Layout/Title_00.szs", "TitleLogoIn_00^s", 0x34, 160, 50).class, Class::Lettering);
        assert_eq!(classify(LeafKind::Bfres, "Common/ProgramTexture/x.szs", "himo_nml", 0x235, 32, 32).class, Class::NormalMap);
        assert_eq!(classify(LeafKind::Bfres, "Common/Stage/A.szs", "cloud_01", 0x1a, 256, 256).class, Class::Sky);
        assert_eq!(classify(LeafKind::Bfres, "Common/Stage/A.szs", "zz_q", 0x431, 256, 256).class, Class::EnvironmentAlbedo);
    }
}
