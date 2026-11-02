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

pub const ALL_CLASSES: [Class; 15] = [
    Class::CharacterAlbedo,
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

/// Classify from container kind, location, name, format and component selector.
pub fn classify(kind: LeafKind, file: &str, name: &str, format: u32, width: u32, height: u32) -> Classification {
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
    if has(&n, &["_nml", "nml", "_nrm", "normal", "_n."]) || n.ends_with("_n") {
        return c(Class::NormalMap, 0.9, "name marks a normal map");
    }
    if codec == Some(Codec::Bc5) {
        return c(Class::NormalMap, 0.6, "BC5 two-channel texture (normal maps use BC5)");
    }
    if matches!(codec, Some(Codec::Bc4) | Some(Codec::R8) | Some(Codec::Rg8)) {
        return c(Class::Mask, 0.7, "single/dual-channel format holds mask or material data");
    }
    if has(&n, &["mask", "spec", "rough", "metal", "ao_", "_ao", "shadow", "kage", "alpha_", "_a."]) {
        return c(Class::Mask, 0.7, "name marks material data");
    }
    if f.contains("/particle/") || has(&n, &["ak_", "fx_", "effect", "smoke", "fire", "spark", "hinoko", "kemuri"]) {
        return c(Class::Particle, 0.8, "particle archive or effect name");
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
