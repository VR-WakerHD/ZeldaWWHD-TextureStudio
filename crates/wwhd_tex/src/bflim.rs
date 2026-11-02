//! BFLIM layout images (Wii U): raw tiled GX2 data followed by a FLIM header
//! and an `imag` block at the end of the file.

use crate::bytes::{be16, be32, put_be16, put_be32, slice, u8_at};
use crate::error::{bail, unsupported, Result};
use crate::gx2::Gx2Surface;

#[derive(Clone, Debug)]
pub struct Bflim {
    pub width: u32,
    pub height: u32,
    pub flim_format: u8,
    pub surface: Gx2Surface,
    /// Byte offset of the footer (FLIM header) in the file.
    pub footer: usize,
    pub footer_bytes: Vec<u8>,
}

const FOOTER_LEN: usize = 0x28;

pub fn is_bflim(d: &[u8]) -> bool {
    d.len() >= FOOTER_LEN && &d[d.len() - FOOTER_LEN..d.len() - FOOTER_LEN + 4] == b"FLIM"
}

/// FLIM format byte → GX2 surface format. Formats this tool cannot verify are rejected.
fn gx2_format(f: u8) -> Result<u32> {
    Ok(match f {
        0x00 | 0x01 => 0x01, // L8 / A8 -> R8 (selector decides the channel)
        0x02 => 0x02,        // LA4 -> R4G4
        0x03 | 0x04 => 0x07, // LA8 / HILO8 -> R8G8
        0x05 => 0x08,        // RGB565
        0x06 | 0x09 => 0x1A, // RGBX8 / RGBA8
        0x07 => 0x0A,        // RGB5A1
        0x08 => 0x0B,        // RGBA4
        0x0C => 0x31,        // BC1
        0x0D => 0x32,        // BC2
        0x0E => 0x33,        // BC3
        0x0F | 0x10 => 0x34, // BC4 (L / A)
        0x11 => 0x35,        // BC5
        0x14 => 0x41A,       // RGBA8 sRGB
        0x15 => 0x431,       // BC1 sRGB
        0x16 => 0x432,       // BC2 sRGB
        0x17 => 0x433,       // BC3 sRGB
        _ => unsupported!("BFLIM format 0x{f:02x}"),
    })
}

impl Bflim {
    pub fn parse(d: &[u8]) -> Result<Bflim> {
        if !is_bflim(d) {
            bail!("not a BFLIM file");
        }
        let footer = d.len() - FOOTER_LEN;
        if be16(d, footer + 4)? != 0xFEFF {
            unsupported!("little-endian BFLIM");
        }
        if be32(d, footer + 0x0C)? as usize != d.len() {
            bail!("BFLIM size field does not match the file size");
        }
        let imag = footer + 0x14;
        if slice(d, imag, 4)? != b"imag" {
            bail!("BFLIM is missing its imag block");
        }
        let width = be16(d, imag + 8)? as u32;
        let height = be16(d, imag + 10)? as u32;
        let alignment = be16(d, imag + 12)? as u32;
        let flim_format = u8_at(d, imag + 14)?;
        let tile_swizzle = u8_at(d, imag + 15)?;
        let image_size = be32(d, imag + 16)?;
        if image_size as usize > footer {
            bail!("BFLIM image size exceeds the file");
        }
        let mut surface = Gx2Surface {
            dim: crate::gx2::DIM_2D,
            width,
            height,
            depth: 1,
            mip_count: 1,
            format: gx2_format(flim_format)?,
            aa: 0,
            usage: 1,
            image_size,
            mip_size: 0,
            tile_mode: (tile_swizzle & 0x1F) as u32,
            swizzle: ((tile_swizzle >> 5) as u32) << 8,
            alignment,
            pitch: 0,
            mip_offsets: [0; 13],
        };
        // Pitch is not stored; derive it from addrlib for the stored tile mode.
        let mut probe = surface.clone();
        crate::gx2::calc_layout(&mut probe)?;
        surface.pitch = probe.pitch;
        Ok(Bflim { width, height, flim_format, surface, footer, footer_bytes: d[footer..].to_vec() })
    }

    pub fn image<'a>(&self, d: &'a [u8]) -> &'a [u8] {
        &d[..self.surface.image_size as usize]
    }

    /// Build a BFLIM with new tiled data and dimensions, keeping the footer otherwise.
    pub fn rebuild(&self, s: &Gx2Surface, image: &[u8]) -> Result<Vec<u8>> {
        if s.width > 0xFFFF || s.height > 0xFFFF || s.alignment > 0xFFFF {
            bail!("BFLIM cannot store {}x{} with alignment {}", s.width, s.height, s.alignment);
        }
        let mut out = image.to_vec();
        out.extend_from_slice(&self.footer_bytes);
        let f = out.len() - FOOTER_LEN;
        let total = out.len() as u32;
        put_be32(&mut out, f + 0x0C, total);
        let imag = f + 0x14;
        put_be16(&mut out, imag + 8, s.width as u16);
        put_be16(&mut out, imag + 10, s.height as u16);
        put_be16(&mut out, imag + 12, s.alignment as u16);
        out[imag + 15] = (s.tile_mode as u8 & 0x1F) | (((s.swizzle >> 8) & 7) as u8) << 5;
        put_be32(&mut out, imag + 16, image.len() as u32);
        Ok(out)
    }
}
