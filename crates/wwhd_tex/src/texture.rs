//! Textures independent of their container (BFRES FTEX or BFLIM): listing,
//! decoding to RGBA8, the rewrite safety gate and building replacements.

use serde::{Deserialize, Serialize};

use crate::bfres::{Bfres, TextureReplacement};
use crate::bflim::Bflim;
use crate::container::{AssetLocation, LeafKind};
use crate::error::{bail, Result};
use crate::gx2::{self, Codec, Gx2Surface};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TextureId {
    pub location: AssetLocation,
    /// FTEX index inside the BFRES; 0 for BFLIM.
    pub index: usize,
    pub name: String,
}

impl TextureId {
    pub fn key(&self) -> String {
        format!("{}#{}:{}", self.location.display(), self.index, self.name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextureInfo {
    pub id: TextureId,
    pub kind: LeafKind,
    pub surface: Gx2Surface,
    pub comp_sel: [u8; 4],
    pub regs: [u32; 5],
    /// blake3 of the tiled image + mip bytes.
    pub data_hash: String,
}

pub fn list(location: &AssetLocation, kind: LeafKind, leaf: &[u8]) -> Result<Vec<TextureInfo>> {
    match kind {
        LeafKind::Bfres => {
            let b = Bfres::parse(leaf)?;
            Ok(b.textures
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut h = blake3::Hasher::new();
                    h.update(t.image_data(leaf));
                    h.update(t.mip_data(leaf));
                    TextureInfo {
                        id: TextureId { location: location.clone(), index: i, name: t.name.clone() },
                        kind,
                        surface: t.surface.clone(),
                        comp_sel: t.comp_sel,
                        regs: t.regs,
                        data_hash: h.finalize().to_hex().to_string(),
                    }
                })
                .collect())
        }
        LeafKind::Bflim => {
            let f = Bflim::parse(leaf)?;
            let name = location.leaf_name().rsplit('/').next().unwrap_or("").to_string();
            Ok(vec![TextureInfo {
                id: TextureId { location: location.clone(), index: 0, name },
                kind,
                surface: f.surface.clone(),
                comp_sel: [0, 1, 2, 3],
                regs: [0; 5],
                data_hash: blake3::hash(f.image(leaf)).to_hex().to_string(),
            }])
        }
    }
}

/// Tiled (image, mips) bytes of a texture in its leaf.
pub fn raw_data<'a>(info: &TextureInfo, leaf: &'a [u8]) -> Result<(&'a [u8], &'a [u8])> {
    match info.kind {
        LeafKind::Bfres => {
            let b = Bfres::parse(leaf)?;
            let Some(t) = b.textures.get(info.id.index) else { bail!("texture index out of range") };
            Ok((t.image_data(leaf), t.mip_data(leaf)))
        }
        LeafKind::Bflim => {
            let f = Bflim::parse(leaf)?;
            Ok((&leaf[..f.surface.image_size as usize], &[][..]))
        }
    }
}

/// Decoded mip chain, RGBA8 per level (first slice only).
pub struct Decoded {
    pub levels: Vec<(u32, u32, Vec<u8>)>,
}

pub fn decode(info: &TextureInfo, leaf: &[u8]) -> Result<Decoded> {
    let s = &info.surface;
    if s.depth > 1 {
        crate::error::unsupported!("textures with {} slices (cube maps/arrays)", s.depth);
    }
    let (image, mips) = raw_data(info, leaf)?;
    let linear = gx2::detile_levels(s, image, mips)?;
    let mut levels = Vec::with_capacity(linear.len());
    for (l, data) in linear.iter().enumerate() {
        levels.push(gx2::level_to_rgba(s, l as u32, data)?);
    }
    Ok(Decoded { levels })
}

/// Result of the rewrite gate for one original texture.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GateResult {
    pub layout_matches: bool,
    pub regs_match: bool,
    pub roundtrip_exact: bool,
    pub detail: String,
}

impl GateResult {
    pub fn passed(&self) -> bool {
        self.layout_matches && self.regs_match && self.roundtrip_exact
    }
}

/// Recompute the original surface's layout and registers and tile its own
/// detiled data back; all three must match the stored bytes exactly.
pub fn gate(info: &TextureInfo, leaf: &[u8]) -> GateResult {
    let mut r = GateResult::default();
    let s = &info.surface;
    let mut calc = s.clone();
    match gx2::calc_layout(&mut calc) {
        Ok(_) => {
            let mut diffs = Vec::new();
            if calc.image_size != s.image_size {
                diffs.push(format!("imageSize {} vs {}", calc.image_size, s.image_size));
            }
            if calc.pitch != s.pitch {
                diffs.push(format!("pitch {} vs {}", calc.pitch, s.pitch));
            }
            if info.kind == LeafKind::Bfres {
                if calc.alignment != s.alignment {
                    diffs.push(format!("alignment {} vs {}", calc.alignment, s.alignment));
                }
                if s.mip_count > 1 {
                    if calc.mip_size != s.mip_size {
                        diffs.push(format!("mipSize {} vs {}", calc.mip_size, s.mip_size));
                    }
                    if calc.mip_offsets != s.mip_offsets {
                        diffs.push(format!("mipOffsets {:?} vs {:?}", calc.mip_offsets, s.mip_offsets));
                    }
                }
            }
            r.layout_matches = diffs.is_empty();
            r.detail = diffs.join("; ");
        }
        Err(e) => r.detail = e.to_string(),
    }
    // All-zero registers are computed by the game at load time (GX2InitTextureRegs).
    r.regs_match = info.kind == LeafKind::Bflim || info.regs == [0; 5] || gx2::regs_consistent(info.regs, s);
    if !r.regs_match {
        r.detail.push_str(&format!(" regs {:08x?} inconsistent", info.regs));
    }
    if r.layout_matches {
        let check = || -> Result<bool> {
            let (image, mips) = raw_data(info, leaf)?;
            let linear = gx2::detile_levels(s, image, mips)?;
            let (ti, tm) = gx2::tile_levels(&calc, &linear)?;
            // Compare only the bytes addrlib defines; padding is unspecified.
            let ok_image = tiled_equal(s, &calc, 0, image, &ti)?;
            let ok_mips = s.mip_count <= 1 || mips_equal(&calc, mips, &tm)?;
            Ok(ok_image && ok_mips)
        };
        match check() {
            Ok(ok) => {
                r.roundtrip_exact = ok;
                if !ok {
                    r.detail.push_str(" tile roundtrip differs");
                }
            }
            Err(e) => r.detail.push_str(&format!(" roundtrip error: {e}")),
        }
    }
    r
}

fn tiled_equal(_s: &Gx2Surface, _calc: &Gx2Surface, _level: u32, a: &[u8], b: &[u8]) -> Result<bool> {
    let n = a.len().min(b.len());
    Ok(a[..n] == b[..n])
}

fn mips_equal(calc: &Gx2Surface, a: &[u8], b: &[u8]) -> Result<bool> {
    let n = (calc.mip_size as usize).min(a.len()).min(b.len());
    Ok(a[..n] == b[..n])
}

/// Build a tiled replacement surface from new RGBA levels. Keeps format, tile
/// mode, swizzle, usage and dim of the original; recomputes layout and registers.
pub fn build_replacement(info: &TextureInfo, levels: &[(u32, u32, Vec<u8>)]) -> Result<TextureReplacement> {
    let c: Codec = gx2::codec(info.surface.format)?;
    let snorm = gx2::is_snorm(info.surface.format);
    let Some((w, h, _)) = levels.first() else { bail!("no levels") };
    let mut s = info.surface.clone();
    s.width = *w;
    s.height = *h;
    s.mip_count = levels.len() as u32;
    gx2::calc_layout(&mut s)?;
    let mut linear = Vec::with_capacity(levels.len());
    for (l, (lw, lh, rgba)) in levels.iter().enumerate() {
        let (ew, eh) = ((*w >> l).max(1), (*h >> l).max(1));
        if (*lw, *lh) != (ew, eh) || rgba.len() != (ew * eh * 4) as usize {
            bail!("level {l} is {lw}x{lh}, expected {ew}x{eh}");
        }
        linear.push(gx2::rgba_to_level(c, snorm, *lw, *lh, rgba)?);
    }
    let (image, mips) = gx2::tile_levels(&s, &linear)?;
    let regs = if info.regs == [0; 5] { info.regs } else { gx2::patch_regs(info.regs, &s)? };
    Ok(TextureReplacement { surface: s, regs, image, mips })
}
