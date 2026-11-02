//! GX2 surfaces: format table, surface layout (equivalent of
//! GX2CalcSurfaceSizeAndAlignment), tiling, decoding to RGBA8 and encoding back.
//!
//! The layout code is checked against every texture of the disc by
//! `validate::surface_gate`: recomputing an original surface's own parameters
//! must reproduce the stored ones before the tool will rewrite that format/tile mode.

use serde::{Deserialize, Serialize};
use wiiu_swizzle::addrlib::{
    hwl_compute_surface_info, ComputeSurfaceInfoInput, ComputeSurfaceInfoOutput, SurfaceFlags,
    TileType,
};
use wiiu_swizzle::{AaMode, SurfaceFormat, TileMode};

use crate::error::{bail, unsupported, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gx2Surface {
    pub dim: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_count: u32,
    pub format: u32,
    pub aa: u32,
    pub usage: u32,
    pub image_size: u32,
    pub mip_size: u32,
    pub tile_mode: u32,
    pub swizzle: u32,
    pub alignment: u32,
    pub pitch: u32,
    pub mip_offsets: [u32; 13],
}

pub const DIM_2D: u32 = 1;
pub const DIM_CUBE: u32 = 3;

/// Formats this tool can decode and re-encode, with their meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    Bc1,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Rgba8,
    R8,
    Rg8,
    Rgb565,
    Rgba4,
    Rgb5a1,
}

pub fn format_name(format: u32) -> String {
    match SurfaceFormat::from_repr(format) {
        Some(f) => format!("{f:?}"),
        None => format!("0x{format:x}"),
    }
}

pub fn is_srgb(format: u32) -> bool {
    format & 0x400 != 0
}

pub fn is_snorm(format: u32) -> bool {
    format & 0x200 != 0
}

pub fn codec(format: u32) -> Result<Codec> {
    Ok(match format & 0x3F {
        0x31 => Codec::Bc1,
        0x32 => Codec::Bc2,
        0x33 => Codec::Bc3,
        0x34 => Codec::Bc4,
        0x35 => Codec::Bc5,
        0x1A => Codec::Rgba8,
        0x01 => Codec::R8,
        0x07 => Codec::Rg8,
        0x08 => Codec::Rgb565,
        0x0B => Codec::Rgba4,
        0x0A => Codec::Rgb5a1,
        _ => unsupported!("GX2 format {}", format_name(format)),
    })
}

impl Codec {
    pub fn is_block(self) -> bool {
        matches!(self, Codec::Bc1 | Codec::Bc2 | Codec::Bc3 | Codec::Bc4 | Codec::Bc5)
    }
    pub fn block_dim(self) -> u32 {
        if self.is_block() {
            4
        } else {
            1
        }
    }
    /// Bytes per element (block or pixel).
    pub fn bytes(self) -> u32 {
        match self {
            Codec::Bc1 | Codec::Bc4 => 8,
            Codec::Bc2 | Codec::Bc3 | Codec::Bc5 => 16,
            Codec::Rgba8 => 4,
            Codec::R8 => 1,
            Codec::Rg8 | Codec::Rgb565 | Codec::Rgba4 | Codec::Rgb5a1 => 2,
        }
    }
    pub fn has_alpha(self) -> bool {
        matches!(self, Codec::Bc2 | Codec::Bc3 | Codec::Rgba8 | Codec::Rgba4 | Codec::Rgb5a1)
    }
}

fn tile_mode(v: u32) -> Result<TileMode> {
    match TileMode::from_repr(v) {
        Some(t) => Ok(t),
        None => unsupported!("GX2 tile mode {v}"),
    }
}

fn aa_mode(v: u32) -> Result<AaMode> {
    match AaMode::from_repr(v) {
        Some(a) => Ok(a),
        None => unsupported!("GX2 AA mode {v}"),
    }
}

/// Per-mip layout produced by addrlib.
#[derive(Clone, Debug)]
pub struct LevelInfo {
    pub width_e: u32,
    pub height_e: u32,
    pub pitch: u32,
    pub tile_mode: TileMode,
    pub size: u32,
    pub base_align: u32,
}

fn surface_info(s: &Gx2Surface, level: u32) -> Result<LevelInfo> {
    let c = codec(s.format)?;
    let bd = c.block_dim();
    let width_e = (s.width >> level).max(1).div_ceil(bd);
    let height_e = (s.height >> level).max(1).div_ceil(bd);
    let Some(format) = SurfaceFormat::from_repr(s.format) else { unsupported!("GX2 format {:x}", s.format) };
    let slices = if s.dim == DIM_CUBE || s.dim == 2 { s.depth.max(1) } else { 1 };
    let input = ComputeSurfaceInfoInput {
        size: 0,
        tile_mode: tile_mode(s.tile_mode)?,
        format,
        bpp: c.bytes() * 8,
        num_samples: 1 << s.aa,
        width: width_e,
        height: height_e,
        num_slices: slices,
        slice: 0,
        mip_level: level,
        flags: SurfaceFlags::new(
            false, false, false, false, s.dim == DIM_CUBE, s.dim == 2, false, false, false, false,
            false, false, false, false, false, false, false, false, 0u8.into(),
        ),
        tile_info: Default::default(),
        tile_type: TileType::Displayable,
        tile_index: 0,
    };
    let mut out = ComputeSurfaceInfoOutput {
        size: 0,
        pitch: 0,
        height: 0,
        depth: 0,
        surf_size: 0,
        tile_mode: input.tile_mode,
        base_align: 0,
        pitch_align: 0,
        height_align: 0,
        depth_align: 0,
        bpp: 0,
        pixel_pitch: 0,
        pixel_height: 0,
        pixel_bits: 0,
        slice_size: 0,
        pitch_tile_max: 0,
        height_tile_max: 0,
        slice_tile_max: 0,
        tile_info: Default::default(),
        tile_type: TileType::Displayable,
        tile_index: 0,
    };
    hwl_compute_surface_info(&input, &mut out);
    if out.surf_size == 0 || out.surf_size > u32::MAX as u64 {
        bail!("addrlib returned an invalid size for level {level}");
    }
    Ok(LevelInfo {
        width_e,
        height_e,
        pitch: out.pitch,
        tile_mode: out.tile_mode,
        size: out.surf_size as u32,
        base_align: out.base_align,
    })
}

/// Fill in size, alignment, pitch and mip offsets for a surface whose
/// dimensions, format, tile mode and mip count are set.
pub fn calc_layout(s: &mut Gx2Surface) -> Result<Vec<LevelInfo>> {
    let mut levels = Vec::with_capacity(s.mip_count as usize);
    let mut mip_size = 0u32;
    s.mip_offsets = [0; 13];
    for level in 0..s.mip_count.max(1) {
        let info = surface_info(s, level)?;
        if level == 0 {
            s.image_size = info.size;
            s.alignment = info.base_align;
            s.pitch = info.pitch;
        } else {
            mip_size = mip_size.next_multiple_of(info.base_align.max(1));
            if level == 1 {
                s.mip_offsets[0] = s.image_size.next_multiple_of(info.base_align.max(1));
            } else {
                s.mip_offsets[level as usize - 1] = mip_size;
            }
            mip_size += info.size;
        }
        levels.push(info);
    }
    s.mip_size = mip_size;
    Ok(levels)
}

fn level_slice<'a>(s: &Gx2Surface, image: &'a [u8], mips: &'a [u8], level: u32) -> Result<&'a [u8]> {
    if level == 0 {
        return Ok(image);
    }
    let start = if level == 1 { 0 } else { s.mip_offsets[level as usize - 1] as usize };
    let end = if (level as usize) < 13 && level + 1 < s.mip_count {
        s.mip_offsets[level as usize] as usize
    } else {
        mips.len()
    };
    match mips.get(start..end.max(start)) {
        Some(x) => Ok(x),
        None => bail!("mip {level} range {start}..{end} outside mip data of {} bytes", mips.len()),
    }
}

/// Swizzle value used for a level: macro-tiled levels use the pipe/bank bits.
fn level_swizzle(s: &Gx2Surface) -> u32 {
    s.swizzle & 0x700
}

/// Untile all levels of the first slice into tightly packed element rows,
/// using the stored per-level pitch semantics of addrlib.
pub fn detile_levels(s: &Gx2Surface, image: &[u8], mips: &[u8]) -> Result<Vec<Vec<u8>>> {
    let c = codec(s.format)?;
    let aa = aa_mode(s.aa)?;
    let mut out = Vec::new();
    for level in 0..s.mip_count.max(1) {
        let info = surface_info(s, level)?;
        let src = level_slice(s, image, mips, level)?;
        let pitch = if level == 0 { s.pitch.min(info.pitch).max(1) } else { info.pitch };
        let data = wiiu_swizzle::deswizzle_mipmap(
            info.width_e,
            info.height_e,
            1,
            src,
            level_swizzle(s),
            pitch,
            info.tile_mode,
            c.bytes(),
            aa,
        )
        .map_err(|e| crate::error::Error::Format(format!("detile level {level}: {e}")))?;
        out.push(data);
    }
    Ok(out)
}

/// Tile linear levels into image and mip blobs laid out per `calc_layout`.
pub fn tile_levels(s: &Gx2Surface, levels: &[Vec<u8>]) -> Result<(Vec<u8>, Vec<u8>)> {
    let c = codec(s.format)?;
    let aa = aa_mode(s.aa)?;
    let mut image = Vec::new();
    let mut mips = Vec::new();
    for (level, data) in levels.iter().enumerate() {
        let level = level as u32;
        let info = surface_info(s, level)?;
        let tiled = wiiu_swizzle::swizzle_mipmap_sized(
            info.width_e,
            info.height_e,
            1,
            data,
            level_swizzle(s),
            info.pitch,
            info.tile_mode,
            c.bytes(),
            aa,
            info.size as usize,
        )
        .map_err(|e| crate::error::Error::Format(format!("tile level {level}: {e}")))?;
        if level == 0 {
            image = tiled;
            image.resize(s.image_size as usize, 0);
        } else {
            let at = if level == 1 { 0 } else { s.mip_offsets[level as usize - 1] as usize };
            if mips.len() < at {
                mips.resize(at, 0);
            }
            mips.truncate(at);
            mips.extend_from_slice(&tiled);
            mips.resize(at + info.size as usize, 0);
        }
    }
    if s.mip_count > 1 {
        mips.resize(s.mip_size as usize, 0);
    }
    Ok((image, mips))
}

/// Decode linear element data of one level to RGBA8 (channels as stored; the
/// texture's component selector is metadata, not applied here).
pub fn decode_rgba(c: Codec, snorm: bool, w: u32, h: u32, data: &[u8]) -> Result<Vec<u8>> {
    use image_dds::{ImageFormat, Surface};
    if c.is_block() {
        let f = match (c, snorm) {
            (Codec::Bc1, _) => ImageFormat::BC1RgbaUnorm,
            (Codec::Bc2, _) => ImageFormat::BC2RgbaUnorm,
            (Codec::Bc3, _) => ImageFormat::BC3RgbaUnorm,
            (Codec::Bc4, false) => ImageFormat::BC4RUnorm,
            (Codec::Bc4, true) => ImageFormat::BC4RSnorm,
            (Codec::Bc5, false) => ImageFormat::BC5RgUnorm,
            (Codec::Bc5, true) => ImageFormat::BC5RgSnorm,
            _ => unreachable!(),
        };
        let surf = Surface { width: w, height: h, depth: 1, layers: 1, mipmaps: 1, image_format: f, data };
        let mut rgba = surf.decode_rgba8().map_err(|e| crate::error::Error::Format(format!("decode: {e}")))?.data;
        if snorm && matches!(c, Codec::Bc4 | Codec::Bc5) {
            // image_dds decodes SNORM to raw signed bytes but encodes from
            // unsigned-mapped values (0..255 = -1..1); keep everything unsigned-mapped.
            let n = if c == Codec::Bc4 { 1 } else { 2 };
            for p in rgba.chunks_exact_mut(4) {
                for v in p.iter_mut().take(n) {
                    let f = (*v as i8 as f32 / 127.0).max(-1.0);
                    *v = ((f + 1.0) * 127.5).round() as u8;
                }
            }
        }
        return Ok(rgba);
    }
    let n = (w * h) as usize;
    if data.len() < n * c.bytes() as usize {
        bail!("pixel data too short");
    }
    let mut out = vec![0u8; n * 4];
    let x5 = |v: u16| ((v as u32 * 255 + 15) / 31) as u8;
    let x6 = |v: u16| ((v as u32 * 255 + 31) / 63) as u8;
    for i in 0..n {
        let p = &mut out[i * 4..i * 4 + 4];
        match c {
            Codec::Rgba8 => p.copy_from_slice(&data[i * 4..i * 4 + 4]),
            Codec::R8 => p.copy_from_slice(&[data[i], 0, 0, 255]),
            Codec::Rg8 => p.copy_from_slice(&[data[i * 2], data[i * 2 + 1], 0, 255]),
            // 16-bit GX2 formats: little-endian words on Latte (as in Cemu).
            Codec::Rgb565 => {
                let v = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                p.copy_from_slice(&[x5(v & 31), x6((v >> 5) & 63), x5(v >> 11), 255]);
            }
            Codec::Rgb5a1 => {
                let v = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                p.copy_from_slice(&[x5(v & 31), x5((v >> 5) & 31), x5((v >> 10) & 31), if v >> 15 != 0 { 255 } else { 0 }]);
            }
            Codec::Rgba4 => {
                let v = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                let x4 = |s: u16| (((v >> s) & 15) * 17) as u8;
                p.copy_from_slice(&[x4(0), x4(4), x4(8), x4(12)]);
            }
            _ => unreachable!(),
        }
    }
    Ok(out)
}

/// Encode RGBA8 to linear element data of one level.
pub fn encode_rgba(c: Codec, snorm: bool, w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    use image_dds::{ImageFormat, Mipmaps, Quality, SurfaceRgba8};
    if c.is_block() {
        let f = match (c, snorm) {
            (Codec::Bc1, _) => ImageFormat::BC1RgbaUnorm,
            (Codec::Bc2, _) => ImageFormat::BC2RgbaUnorm,
            (Codec::Bc3, _) => ImageFormat::BC3RgbaUnorm,
            (Codec::Bc4, false) => ImageFormat::BC4RUnorm,
            (Codec::Bc4, true) => ImageFormat::BC4RSnorm,
            (Codec::Bc5, false) => ImageFormat::BC5RgUnorm,
            (Codec::Bc5, true) => ImageFormat::BC5RgSnorm,
            _ => unreachable!(),
        };
        let surf = SurfaceRgba8 { width: w, height: h, depth: 1, layers: 1, mipmaps: 1, data: rgba };
        let enc = surf
            .encode(f, Quality::Slow, Mipmaps::Disabled)
            .map_err(|e| crate::error::Error::Format(format!("encode: {e}")))?;
        return Ok(enc.data);
    }
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * c.bytes() as usize);
    let q = |v: u8, bits: u32| ((v as u32 * ((1 << bits) - 1) + 127) / 255) as u16;
    for i in 0..n {
        let p = &rgba[i * 4..i * 4 + 4];
        match c {
            Codec::Rgba8 => out.extend_from_slice(p),
            Codec::R8 => out.push(p[0]),
            Codec::Rg8 => out.extend_from_slice(&p[..2]),
            Codec::Rgb565 => out.extend_from_slice(&(q(p[0], 5) | q(p[1], 6) << 5 | q(p[2], 5) << 11).to_le_bytes()),
            Codec::Rgb5a1 => out.extend_from_slice(
                &(q(p[0], 5) | q(p[1], 5) << 5 | q(p[2], 5) << 10 | ((p[3] >= 128) as u16) << 15).to_le_bytes(),
            ),
            Codec::Rgba4 => out.extend_from_slice(&(q(p[0], 4) | q(p[1], 4) << 4 | q(p[2], 4) << 8 | q(p[3], 4) << 12).to_le_bytes()),
            _ => unreachable!(),
        }
    }
    Ok(out)
}

/// Linear element data → RGBA8 for a level of size `w`×`h` pixels. Block
/// formats are decoded on padded block dimensions and cropped.
pub fn level_to_rgba(s: &Gx2Surface, level: u32, linear: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let c = codec(s.format)?;
    let w = (s.width >> level).max(1);
    let h = (s.height >> level).max(1);
    let bd = c.block_dim();
    let (pw, ph) = (w.div_ceil(bd) * bd, h.div_ceil(bd) * bd);
    let rgba = decode_rgba(c, is_snorm(s.format), pw, ph, linear)?;
    if pw == w && ph == h {
        return Ok((w, h, rgba));
    }
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        out.extend_from_slice(&rgba[y * pw as usize * 4..(y * pw as usize + w as usize) * 4]);
    }
    Ok((w, h, out))
}

/// RGBA8 image → linear element data, padding to whole blocks by edge replication.
pub fn rgba_to_level(c: Codec, snorm: bool, w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let bd = c.block_dim();
    let (pw, ph) = (w.div_ceil(bd) * bd, h.div_ceil(bd) * bd);
    if pw == w && ph == h {
        return encode_rgba(c, snorm, w, h, rgba);
    }
    let mut padded = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph {
        for x in 0..pw {
            let (sx, sy) = (x.min(w - 1), y.min(h - 1));
            let si = ((sy * w + sx) * 4) as usize;
            let di = ((y * pw + x) * 4) as usize;
            padded[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
        }
    }
    encode_rgba(c, snorm, pw, ph, &padded)
}

/// Patch the five GX2 texture registers for new dimensions/pitch/mip count,
/// keeping every other field (format, selectors, tile type) from the original.
/// Bit positions follow SQ_TEX_RESOURCE_WORD0/1/5 and are verified on the disc.
pub fn patch_regs(regs: [u32; 5], s: &Gx2Surface) -> Result<[u32; 5]> {
    let c = codec(s.format)?;
    let mut r = regs;
    let pixel_pitch = if c.is_block() { s.pitch * 4 } else { s.pitch };
    let pitch_field = (pixel_pitch >> 3).saturating_sub(1) & 0x7FF;
    r[0] = (r[0] & !(0x7FF << 8) & !(0x1FFF << 19)) | pitch_field << 8 | ((s.width - 1) & 0x1FFF) << 19;
    r[1] = (r[1] & !0x1FFF) | ((s.height - 1) & 0x1FFF);
    r[3] = (r[3] & !0xF) | ((s.mip_count.max(1) - 1) & 0xF);
    Ok(r)
}

/// Check that the stored registers agree with the stored surface under the bit
/// layout used by `patch_regs`.
pub fn regs_consistent(regs: [u32; 5], s: &Gx2Surface) -> bool {
    patch_regs(regs, s).map(|p| p == regs).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(c: Codec, snorm: bool) -> Vec<(u8, u8)> {
        let rgba: Vec<u8> = (0..16 * 16u32).flat_map(|i| [(i % 16 * 16) as u8, (i / 16 * 16) as u8, 0, 255]).collect();
        let enc = encode_rgba(c, snorm, 16, 16, &rgba).unwrap();
        let dec = decode_rgba(c, snorm, 16, 16, &enc).unwrap();
        rgba.chunks(4).zip(dec.chunks(4)).map(|(a, b)| (a[0], b[0])).collect()
    }

    #[test]
    fn block_formats_roundtrip_channels() {
        for (c, s) in [(Codec::Bc1, false), (Codec::Bc3, false), (Codec::Bc4, false), (Codec::Bc5, false), (Codec::Bc4, true), (Codec::Bc5, true)] {
            let pairs = roundtrip(c, s);
            let worst = pairs.iter().map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            let tol = if matches!(c, Codec::Bc1 | Codec::Bc3) { 40 } else { 12 };
            assert!(worst <= tol, "{c:?} snorm={s}: worst channel error {worst}; samples {:?}", &pairs[..20]);
        }
    }
}
