//! Runtime texture packs: the key the recomp looks replacements up by, and the DDS files it loads.
//!
//! A texture is identified by (GX2 format, width, height, XXH3-64 of its level-0 guest bytes). The recomp hashes
//! the same bytes at run time, so `level0_bytes` must slice exactly what the game uploads.

use image_dds::{ImageFormat, Mipmaps, Quality, SurfaceRgba8};
use xxhash_rust::xxh3::xxh3_64;

use crate::classify::Class;
use crate::error::{bail, Error, Result};
use crate::gx2::Gx2Surface;

/// XXH3-64 (seed 0) of a texture's level-0 guest bytes: the hash part of the key.
pub fn key_hash(level0: &[u8]) -> u64 {
    xxh3_64(level0)
}

/// The level-0 guest bytes of a texture: the first `image_size` bytes of its image data (never longer than `data`).
pub fn level0_bytes<'a>(data: &'a [u8], surface: &Gx2Surface) -> &'a [u8] {
    &data[..(surface.image_size as usize).min(data.len())]
}

/// The DDS format of a replacement; it keeps the original's sRGB-ness or signedness so the guest's sampling applies
/// unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackFormat {
    Bc7 { srgb: bool },
    Bc4 { snorm: bool },
    Bc5 { snorm: bool },
    Rgba8 { srgb: bool },
}

impl PackFormat {
    /// The DXGI_FORMAT value written into the DX10 header.
    pub fn dxgi(&self) -> u32 {
        match *self {
            PackFormat::Bc7 { srgb } => 98 + srgb as u32,
            PackFormat::Bc4 { snorm } => 80 + snorm as u32,
            PackFormat::Bc5 { snorm } => 83 + snorm as u32,
            PackFormat::Rgba8 { srgb } => 28 + srgb as u32,
        }
    }

    fn block_bytes(&self) -> Option<usize> {
        match self {
            PackFormat::Bc4 { .. } => Some(8),
            PackFormat::Bc7 { .. } | PackFormat::Bc5 { .. } => Some(16),
            PackFormat::Rgba8 { .. } => None,
        }
    }
}

/// The replacement format for a GX2 surface format, or `None` for a format that gets no entry. BC1/BC2/BC3 become
/// BC7 of the same colour space, BC4/BC5 stay as they are, RGBA8 becomes BC7 (RGBA8 itself for UI and lettering,
/// which stay sharp).
pub fn pack_format(gx2_format: u32, class: Class) -> Option<PackFormat> {
    let plain_rgba = matches!(class, Class::Ui | Class::Lettering);
    Some(match gx2_format {
        0x31..=0x33 => PackFormat::Bc7 { srgb: false },
        0x431..=0x433 => PackFormat::Bc7 { srgb: true },
        0x34 => PackFormat::Bc4 { snorm: false },
        0x234 => PackFormat::Bc4 { snorm: true },
        0x35 => PackFormat::Bc5 { snorm: false },
        0x235 => PackFormat::Bc5 { snorm: true },
        0x1A | 0x41A => {
            let srgb = gx2_format == 0x41A;
            if plain_rgba { PackFormat::Rgba8 { srgb } } else { PackFormat::Bc7 { srgb } }
        }
        _ => return None,
    })
}

/// Encodes one level (RGBA8, `w`x`h`) to the format's blocks, padding to whole 4x4 blocks by edge replication.
fn encode_level(fmt: PackFormat, w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let f = match fmt {
        PackFormat::Rgba8 { .. } => return Ok(rgba.to_vec()),
        PackFormat::Bc7 { srgb: false } => ImageFormat::BC7RgbaUnorm,
        PackFormat::Bc7 { srgb: true } => ImageFormat::BC7RgbaUnormSrgb,
        PackFormat::Bc4 { snorm: false } => ImageFormat::BC4RUnorm,
        PackFormat::Bc4 { snorm: true } => ImageFormat::BC4RSnorm,
        PackFormat::Bc5 { snorm: false } => ImageFormat::BC5RgUnorm,
        PackFormat::Bc5 { snorm: true } => ImageFormat::BC5RgSnorm,
    };
    let (pw, ph) = (w.div_ceil(4) * 4, h.div_ceil(4) * 4);
    let padded;
    let data = if (pw, ph) == (w, h) {
        rgba
    } else {
        let mut p = vec![0u8; (pw * ph * 4) as usize];
        for y in 0..ph {
            for x in 0..pw {
                let si = ((y.min(h - 1) * w + x.min(w - 1)) * 4) as usize;
                let di = ((y * pw + x) * 4) as usize;
                p[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
            }
        }
        padded = p;
        &padded[..]
    };
    let surf = SurfaceRgba8 { width: pw, height: ph, depth: 1, layers: 1, mipmaps: 1, data };
    let enc = surf
        .encode(f, Quality::Slow, Mipmaps::Disabled)
        .map_err(|e| Error::Format(format!("encode: {e}")))?;
    Ok(enc.data)
}

/// A DDS file (DX10 header, one 2D image) holding `levels_rgba` encoded as `fmt`, largest level first. Level `i` is
/// `max(1, width >> i)` x `max(1, height >> i)` RGBA8; the level sizes are those the recomp's loader checks.
pub fn write_dds(width: u32, height: u32, fmt: PackFormat, levels_rgba: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mips = levels_rgba.len();
    if width == 0 || height == 0 || mips == 0 || mips > 15 {
        bail!("a DDS needs a non-empty size and 1..=15 levels (got {width}x{height}, {mips} levels)");
    }
    let dims = |i: usize| ((width >> i).max(1), (height >> i).max(1));
    let mut body = Vec::new();
    for (i, rgba) in levels_rgba.iter().enumerate() {
        let (w, h) = dims(i);
        if rgba.len() as u64 != w as u64 * h as u64 * 4 {
            bail!("level {i} has {} bytes, expected {w}x{h} RGBA8", rgba.len());
        }
        body.extend_from_slice(&encode_level(fmt, w, h, rgba)?);
    }
    let (w0, h0) = dims(0);
    let linear = match fmt.block_bytes() {
        Some(b) => w0.div_ceil(4) as u64 * h0.div_ceil(4) as u64 * b as u64,
        None => w0 as u64 * h0 as u64 * 4,
    };
    let mut out = Vec::with_capacity(148 + body.len());
    let mut put = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    put(0x2053_4444); // "DDS "
    put(124);
    put(0x1 | 0x2 | 0x4 | 0x1000 | 0x2_0000 | 0x8_0000); // CAPS HEIGHT WIDTH PIXELFORMAT MIPMAPCOUNT LINEARSIZE
    put(height);
    put(width);
    put(linear as u32);
    put(0); // depth
    put(mips as u32);
    for _ in 0..11 {
        put(0);
    }
    put(32); // DDS_PIXELFORMAT: size, FOURCC, "DX10"
    put(4);
    put(0x3031_5844);
    for _ in 0..5 {
        put(0);
    }
    put(0x1000 | if mips > 1 { 0x8 | 0x40_0000 } else { 0 }); // caps: TEXTURE (+ COMPLEX, MIPMAP)
    for _ in 0..4 {
        put(0); // caps2-4, reserved
    }
    put(fmt.dxgi());
    put(3); // D3D10_RESOURCE_DIMENSION_TEXTURE2D
    put(0); // misc flags
    put(1); // array size
    put(0); // misc flags 2
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Class;

    // XXH3-64 (seed 0) of the empty input, from the xxHash reference.
    #[test]
    fn empty_hash_is_the_reference_value() {
        assert_eq!(key_hash(&[]), 0x2D06800538D394C2);
    }

    // Pinned on both sides: the recomp's texture_pack test asserts XXH3_64bits("wwhd", 4) equals this value.
    #[test]
    fn wwhd_hash_is_pinned() {
        assert_eq!(key_hash(b"wwhd"), 0x390cfa6e3c090774);
    }

    #[test]
    fn level0_is_the_first_image_size_bytes() {
        let s = crate::gx2::Gx2Surface { image_size: 3, ..Default::default() };
        assert_eq!(level0_bytes(&[1, 2, 3, 4, 5], &s), &[1, 2, 3]);
        // data shorter than the surface: everything there is, never a panic
        assert_eq!(level0_bytes(&[1, 2], &s), &[1, 2]);
    }

    #[test]
    fn dxgi_codes() {
        assert_eq!(PackFormat::Bc7 { srgb: false }.dxgi(), 98);
        assert_eq!(PackFormat::Bc7 { srgb: true }.dxgi(), 99);
        assert_eq!(PackFormat::Bc4 { snorm: false }.dxgi(), 80);
        assert_eq!(PackFormat::Bc4 { snorm: true }.dxgi(), 81);
        assert_eq!(PackFormat::Bc5 { snorm: false }.dxgi(), 83);
        assert_eq!(PackFormat::Bc5 { snorm: true }.dxgi(), 84);
        assert_eq!(PackFormat::Rgba8 { srgb: false }.dxgi(), 28);
        assert_eq!(PackFormat::Rgba8 { srgb: true }.dxgi(), 29);
    }

    #[test]
    fn pack_format_table() {
        use PackFormat::*;
        let c = Class::EnvironmentAlbedo;
        for f in [0x31, 0x32, 0x33] {
            assert_eq!(pack_format(f, c), Some(Bc7 { srgb: false }));
        }
        for f in [0x431, 0x432, 0x433] {
            assert_eq!(pack_format(f, c), Some(Bc7 { srgb: true }));
        }
        assert_eq!(pack_format(0x34, c), Some(Bc4 { snorm: false }));
        assert_eq!(pack_format(0x234, c), Some(Bc4 { snorm: true }));
        assert_eq!(pack_format(0x35, c), Some(Bc5 { snorm: false }));
        assert_eq!(pack_format(0x235, c), Some(Bc5 { snorm: true }));
        assert_eq!(pack_format(0x1A, Class::Ui), Some(Rgba8 { srgb: false }));
        assert_eq!(pack_format(0x1A, Class::Lettering), Some(Rgba8 { srgb: false }));
        assert_eq!(pack_format(0x41A, Class::Ui), Some(Rgba8 { srgb: true }));
        assert_eq!(pack_format(0x1A, c), Some(Bc7 { srgb: false }));
        assert_eq!(pack_format(0x41A, c), Some(Bc7 { srgb: true }));
        // R8, RG8, RGB565, RGBA4, RGB5A1: no entry
        for f in [0x01, 0x19, 0x07, 0x08, 0x0A, 0x0B] {
            assert_eq!(pack_format(f, c), None, "{f:#x}");
        }
    }

    fn rd(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 90, 255]).collect()
    }

    /// Parses the header by hand the way the recomp's check_dds does and returns (w, h, dxgi, mips, data offset).
    fn header(f: &[u8]) -> (u32, u32, u32, u32, usize) {
        assert_eq!(rd(f, 0), 0x20534444);
        assert_eq!((rd(f, 4), rd(f, 76), rd(f, 84)), (124, 32, 0x30315844));
        assert_eq!((rd(f, 132), rd(f, 136), rd(f, 140)), (3, 0, 1));
        (rd(f, 16), rd(f, 12), rd(f, 128), rd(f, 28), 148)
    }

    fn level_bytes(dxgi: u32, w: u32, h: u32) -> usize {
        match dxgi {
            80 | 81 => (w as usize).div_ceil(4) * (h as usize).div_ceil(4) * 8,
            83 | 84 | 98 | 99 => (w as usize).div_ceil(4) * (h as usize).div_ceil(4) * 16,
            28 | 29 => (w * h * 4) as usize,
            _ => panic!(),
        }
    }

    #[test]
    fn dds_headers_and_sizes_match_the_recomp_check() {
        let cases = [
            PackFormat::Bc7 { srgb: false },
            PackFormat::Bc7 { srgb: true },
            PackFormat::Bc4 { snorm: false },
            PackFormat::Bc5 { snorm: true },
            PackFormat::Rgba8 { srgb: true },
        ];
        for fmt in cases {
            let (w, h) = (64u32, 64u32);
            let levels: Vec<Vec<u8>> = (0..7).map(|i| gradient((w >> i).max(1), (h >> i).max(1))).collect();
            let dds = write_dds(w, h, fmt, &levels).unwrap();
            let (rw, rh, dxgi, mips, at) = header(&dds);
            assert_eq!((rw, rh, dxgi, mips), (w, h, fmt.dxgi(), 7), "{fmt:?}");
            let total: usize = (0..7).map(|i| level_bytes(dxgi, (w >> i).max(1), (h >> i).max(1))).sum();
            assert_eq!(dds.len(), at + total, "{fmt:?}");
        }
    }

    #[test]
    fn dds_non_square_non_multiple_of_four() {
        let levels: Vec<Vec<u8>> = [(10u32, 6u32), (5, 3), (2, 1), (1, 1)].iter().map(|&(w, h)| gradient(w, h)).collect();
        let dds = write_dds(10, 6, PackFormat::Bc7 { srgb: false }, &levels).unwrap();
        let (w, h, dxgi, mips, at) = header(&dds);
        assert_eq!((w, h, dxgi, mips), (10, 6, 98, 4));
        let total: usize = [(10, 6), (5, 3), (2, 1), (1, 1)].iter().map(|&(w, h)| level_bytes(98, w, h)).sum();
        assert_eq!(dds.len(), at + total);
    }

    #[test]
    fn dds_levels_decode_back_close_to_the_input() {
        let rgba = gradient(32, 32);
        let dds = write_dds(32, 32, PackFormat::Bc7 { srgb: false }, std::slice::from_ref(&rgba)).unwrap();
        let blocks = &dds[148..];
        let surf = image_dds::Surface {
            width: 32, height: 32, depth: 1, layers: 1, mipmaps: 1,
            image_format: image_dds::ImageFormat::BC7RgbaUnorm, data: blocks,
        };
        let dec = surf.decode_rgba8().unwrap().data;
        let worst = rgba.iter().zip(&dec).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= 6, "worst channel error {worst}");
    }

    #[test]
    fn rgba8_level_is_stored_verbatim() {
        let rgba = gradient(8, 8);
        let dds = write_dds(8, 8, PackFormat::Rgba8 { srgb: false }, std::slice::from_ref(&rgba)).unwrap();
        assert_eq!(&dds[148..], &rgba[..]);
    }

    #[test]
    fn bad_level_lists_are_errors() {
        let f = PackFormat::Bc7 { srgb: false };
        assert!(write_dds(8, 8, f, &[]).is_err());
        assert!(write_dds(8, 8, f, &[vec![0; 10]]).is_err());
        assert!(write_dds(0, 8, f, &[vec![]]).is_err());
        let sixteen: Vec<Vec<u8>> = (0..16).map(|_| vec![0; 4]).collect();
        assert!(write_dds(1, 1, f, &sixteen).is_err());
    }
}
