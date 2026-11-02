//! Wii U BFRES (version 3.x, big-endian) texture access.
//!
//! Only the texture index group and FTEX entries are interpreted; the rest of
//! the file is treated as opaque bytes. Replacing a texture appends the new
//! image and mip data at the end of the file and repoints that one FTEX, so
//! every other offset in the file stays valid.

use crate::bytes::{align_up, be16, be32, cstr, put_be32, put_rel_ptr, rel_ptr, slice, u8_at};
use crate::error::{bail, unsupported, Result};
use crate::gx2::Gx2Surface;

pub fn is_bfres(d: &[u8]) -> bool {
    d.len() >= 0x6C && &d[..4] == b"FRES"
}

const TEXTURE_GROUP: usize = 1;

#[derive(Clone, Debug)]
pub struct FtexEntry {
    pub name: String,
    /// Offset of the FTEX block in the file.
    pub offset: usize,
    pub surface: Gx2Surface,
    pub comp_sel: [u8; 4],
    pub regs: [u32; 5],
    pub image_offset: usize,
    pub mip_offset: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Bfres {
    pub version: u32,
    pub alignment: u32,
    pub textures: Vec<FtexEntry>,
}

fn name_at(d: &[u8], field: usize) -> Result<String> {
    match rel_ptr(d, field)? {
        Some(o) => cstr(d, o),
        None => Ok(String::new()),
    }
}

impl Bfres {
    pub fn parse(d: &[u8]) -> Result<Bfres> {
        if !is_bfres(d) {
            bail!("not a BFRES file");
        }
        let version = be32(d, 4)?;
        if version >> 24 != 3 {
            unsupported!("BFRES version {version:08x} (only Wii U version 3 is supported)");
        }
        if be16(d, 8)? != 0xFEFF {
            unsupported!("little-endian BFRES");
        }
        let alignment = be32(d, 0x10)?;
        let mut textures = Vec::new();
        let group_field = 0x20 + TEXTURE_GROUP * 4;
        if let Some(group) = rel_ptr(d, group_field)? {
            let count = be32(d, group + 4)? as usize;
            if count > 4096 {
                bail!("texture group claims {count} entries");
            }
            // Entry 0 is the root node of the patricia tree.
            for k in 1..=count {
                let e = group + 8 + k * 16;
                let name = name_at(d, e + 8)?;
                let Some(ftex) = rel_ptr(d, e + 12)? else { bail!("texture entry {k} has no data") };
                textures.push(parse_ftex(d, ftex, name)?);
            }
        }
        Ok(Bfres { version, alignment, textures })
    }
}

fn parse_ftex(d: &[u8], o: usize, group_name: String) -> Result<FtexEntry> {
    if slice(d, o, 4)? != b"FTEX" {
        bail!("texture {group_name:?} does not start with FTEX");
    }
    let s = o + 4;
    let mut mip_offsets = [0u32; 13];
    for (i, m) in mip_offsets.iter_mut().enumerate() {
        *m = be32(d, s + 0x40 + i * 4)?;
    }
    let surface = Gx2Surface {
        dim: be32(d, s)?,
        width: be32(d, s + 0x04)?,
        height: be32(d, s + 0x08)?,
        depth: be32(d, s + 0x0C)?,
        mip_count: be32(d, s + 0x10)?,
        format: be32(d, s + 0x14)?,
        aa: be32(d, s + 0x18)?,
        usage: be32(d, s + 0x1C)?,
        image_size: be32(d, s + 0x20)?,
        mip_size: be32(d, s + 0x28)?,
        tile_mode: be32(d, s + 0x30)?,
        swizzle: be32(d, s + 0x34)?,
        alignment: be32(d, s + 0x38)?,
        pitch: be32(d, s + 0x3C)?,
        mip_offsets,
    };
    let comp_sel = [u8_at(d, o + 0x88)?, u8_at(d, o + 0x89)?, u8_at(d, o + 0x8A)?, u8_at(d, o + 0x8B)?];
    let mut regs = [0u32; 5];
    for (i, r) in regs.iter_mut().enumerate() {
        *r = be32(d, o + 0x8C + i * 4)?;
    }
    let own_name = name_at(d, o + 0xA8)?;
    let name = if own_name.is_empty() { group_name } else { own_name };
    let Some(image_offset) = rel_ptr(d, o + 0xB0)? else { bail!("texture {name:?} has no image data") };
    let mip_offset = rel_ptr(d, o + 0xB4)?;
    slice(d, image_offset, surface.image_size as usize)?;
    if let Some(m) = mip_offset {
        slice(d, m, surface.mip_size as usize)?;
    }
    Ok(FtexEntry { name, offset: o, surface, comp_sel, regs, image_offset, mip_offset })
}

impl FtexEntry {
    pub fn image_data<'a>(&self, d: &'a [u8]) -> &'a [u8] {
        &d[self.image_offset..self.image_offset + self.surface.image_size as usize]
    }
    pub fn mip_data<'a>(&self, d: &'a [u8]) -> &'a [u8] {
        match self.mip_offset {
            Some(m) => &d[m..m + self.surface.mip_size as usize],
            None => &[],
        }
    }
}

/// New contents for one texture: a complete tiled surface and its registers.
pub struct TextureReplacement {
    pub surface: Gx2Surface,
    pub regs: [u32; 5],
    pub image: Vec<u8>,
    pub mips: Vec<u8>,
}

/// Replace textures in place. Data is appended; the FTEX header fields,
/// registers and data pointers of each replaced texture are rewritten.
pub fn replace_textures(src: &[u8], items: &[(usize, TextureReplacement)]) -> Result<Vec<u8>> {
    let parsed = Bfres::parse(src)?;
    let mut d = src.to_vec();
    let mut file_align = parsed.alignment.max(1) as usize;
    for (index, rep) in items {
        let Some(t) = parsed.textures.get(*index) else { bail!("texture index {index} out of range") };
        let align = (rep.surface.alignment as usize).max(file_align).max(0x100);
        file_align = file_align.max(rep.surface.alignment as usize);
        let image_at = align_up(d.len(), align);
        d.resize(image_at, 0);
        d.extend_from_slice(&rep.image);
        let mip_at = if rep.mips.is_empty() {
            None
        } else {
            let m = align_up(d.len(), align);
            d.resize(m, 0);
            d.extend_from_slice(&rep.mips);
            Some(m)
        };
        let s = t.offset + 4;
        let su = &rep.surface;
        put_be32(&mut d, s, su.dim);
        put_be32(&mut d, s + 0x04, su.width);
        put_be32(&mut d, s + 0x08, su.height);
        put_be32(&mut d, s + 0x0C, su.depth);
        put_be32(&mut d, s + 0x10, su.mip_count);
        put_be32(&mut d, s + 0x14, su.format);
        put_be32(&mut d, s + 0x18, su.aa);
        put_be32(&mut d, s + 0x1C, su.usage);
        put_be32(&mut d, s + 0x20, rep.image.len() as u32);
        put_be32(&mut d, s + 0x28, rep.mips.len() as u32);
        put_be32(&mut d, s + 0x30, su.tile_mode);
        put_be32(&mut d, s + 0x34, su.swizzle);
        put_be32(&mut d, s + 0x38, su.alignment);
        put_be32(&mut d, s + 0x3C, su.pitch);
        for (i, m) in su.mip_offsets.iter().enumerate() {
            put_be32(&mut d, s + 0x40 + i * 4, *m);
        }
        // Texture view: all mips, same slices.
        put_be32(&mut d, t.offset + 0x7C, su.mip_count);
        for (i, r) in rep.regs.iter().enumerate() {
            put_be32(&mut d, t.offset + 0x8C + i * 4, *r);
        }
        put_rel_ptr(&mut d, t.offset + 0xB0, image_at);
        match mip_at {
            Some(m) => put_rel_ptr(&mut d, t.offset + 0xB4, m),
            None => put_be32(&mut d, t.offset + 0xB4, 0),
        }
    }
    let total = align_up(d.len(), 4);
    d.resize(total, 0);
    let len = d.len() as u32;
    put_be32(&mut d, 0x0C, len);
    put_be32(&mut d, 0x10, file_align as u32);
    Ok(d)
}
