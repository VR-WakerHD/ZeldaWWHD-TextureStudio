//! Mip chain generation with per-class rules:
//! - colour: 2×2 box filter in linear light for sRGB formats, gamma space otherwise;
//! - alpha-tested textures: alpha rescaled per level so the coverage at the
//!   0.5 test threshold matches level 0 (foliage does not thin out with distance);
//! - normal maps: vectors averaged then renormalised;
//! - masks and data: plain averaging, no colour conversion.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MipMode {
    Colour { srgb: bool, alpha_test: bool },
    Normal { two_channel: bool, snorm: bool },
    Data,
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Whether alpha looks like a cut-out mask (mostly 0 or 255) rather than blending.
pub fn is_alpha_tested(rgba: &[u8]) -> bool {
    let (mut n, mut hard, mut transparent) = (0usize, 0usize, 0usize);
    for p in rgba.chunks_exact(4).step_by(3) {
        n += 1;
        if p[3] < 16 || p[3] > 239 {
            hard += 1;
        }
        if p[3] < 128 {
            transparent += 1;
        }
    }
    n > 0 && transparent > 0 && hard as f32 / n as f32 > 0.9
}

fn coverage(rgba: &[u8], threshold: f32, scale: f32) -> f32 {
    let t = (threshold * 255.0) as f32;
    let n = rgba.len() / 4;
    rgba.chunks_exact(4).filter(|p| p[3] as f32 * scale >= t).count() as f32 / n.max(1) as f32
}

fn downsample(w: u32, h: u32, src: &[u8], mode: MipMode) -> (u32, u32, Vec<u8>) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    let lut: Vec<f32> = (0..256).map(|i| srgb_to_linear(i as f32 / 255.0)).collect();
    for y in 0..nh {
        for x in 0..nw {
            let xs = [(2 * x).min(w - 1), (2 * x + 1).min(w - 1)];
            let ys = [(2 * y).min(h - 1), (2 * y + 1).min(h - 1)];
            let mut acc = [0f32; 4];
            for &sy in &ys {
                for &sx in &xs {
                    let p = &src[((sy * w + sx) * 4) as usize..][..4];
                    match mode {
                        MipMode::Colour { srgb: true, .. } => {
                            for c in 0..3 {
                                acc[c] += lut[p[c] as usize];
                            }
                            acc[3] += p[3] as f32 / 255.0;
                        }
                        MipMode::Normal { two_channel, snorm } => {
                            let v = |b: u8| if snorm { (b as i8 as f32 / 127.0).max(-1.0) } else { b as f32 / 255.0 * 2.0 - 1.0 };
                            let (nx, ny) = (v(p[0]), v(p[1]));
                            let nz = if two_channel { (1.0 - nx * nx - ny * ny).max(0.0).sqrt() } else { v(p[2]) };
                            acc[0] += nx;
                            acc[1] += ny;
                            acc[2] += nz;
                            acc[3] += p[3] as f32 / 255.0;
                        }
                        _ => {
                            for c in 0..4 {
                                acc[c] += p[c] as f32 / 255.0;
                            }
                        }
                    }
                }
            }
            let d = &mut out[((y * nw + x) * 4) as usize..][..4];
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            match mode {
                MipMode::Colour { srgb: true, .. } => {
                    for c in 0..3 {
                        d[c] = q(linear_to_srgb(acc[c] / 4.0));
                    }
                    d[3] = q(acc[3] / 4.0);
                }
                MipMode::Normal { two_channel, snorm } => {
                    let len = (acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]).sqrt().max(1e-6);
                    let e = |v: f32| if snorm { ((v.clamp(-1.0, 1.0) * 127.0).round() as i8) as u8 } else { q(v * 0.5 + 0.5) };
                    d[0] = e(acc[0] / len);
                    d[1] = e(acc[1] / len);
                    d[2] = if two_channel { 0 } else { e(acc[2] / len) };
                    d[3] = q(acc[3] / 4.0);
                }
                _ => {
                    for c in 0..4 {
                        d[c] = q(acc[c] / 4.0);
                    }
                }
            }
        }
    }
    (nw, nh, out)
}

/// Build `count` levels starting from level 0.
pub fn chain(w: u32, h: u32, base: Vec<u8>, count: u32, mode: MipMode) -> Vec<(u32, u32, Vec<u8>)> {
    let target = match mode {
        MipMode::Colour { alpha_test: true, .. } => Some(coverage(&base, 0.5, 1.0)),
        _ => None,
    };
    let mut levels = vec![(w, h, base)];
    for _ in 1..count.max(1) {
        let (pw, ph, prev) = levels.last().unwrap();
        let (nw, nh, mut img) = downsample(*pw, *ph, prev, mode);
        if let Some(t) = target {
            // Binary search an alpha scale that restores the coverage.
            let (mut lo, mut hi) = (0.25f32, 4.0f32);
            for _ in 0..12 {
                let mid = (lo + hi) / 2.0;
                if coverage(&img, 0.5, mid) < t { lo = mid } else { hi = mid }
            }
            let s = (lo + hi) / 2.0;
            for p in img.chunks_exact_mut(4) {
                p[3] = (p[3] as f32 * s).min(255.0) as u8;
            }
        }
        levels.push((nw, nh, img));
    }
    levels
}

/// Mip count for an upscaled texture: one more level per doubling, so the
/// smallest level keeps the original's smallest size. GX2 allows 13 levels.
pub fn upscaled_mip_count(original: u32, scale: u32) -> u32 {
    if original <= 1 {
        return 1;
    }
    (original + scale.trailing_zeros()).min(13)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_dims() {
        let l = chain(16, 4, vec![128; 16 * 4 * 4], 5, MipMode::Data);
        let dims: Vec<_> = l.iter().map(|(w, h, _)| (*w, *h)).collect();
        assert_eq!(dims, vec![(16, 4), (8, 2), (4, 1), (2, 1), (1, 1)]);
    }

    #[test]
    fn srgb_average_is_brighter_than_gamma_average() {
        let mut img = vec![0u8; 2 * 2 * 4];
        for (i, p) in img.chunks_exact_mut(4).enumerate() {
            let v = if i % 2 == 0 { 0 } else { 255 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let l = chain(2, 2, img, 2, MipMode::Colour { srgb: true, alpha_test: false });
        assert!(l[1].2[0] > 180);
    }

    #[test]
    fn alpha_coverage_preserved() {
        // Scattered cut-out pixels, like leaves in a foliage card.
        let mut img = vec![0u8; 64 * 64 * 4];
        for y in 0..64u32 {
            for x in 0..64u32 {
                let hsh = (x.wrapping_mul(73856093) ^ y.wrapping_mul(19349663)).wrapping_mul(2654435761) >> 16;
                let a = if hsh % 3 == 0 { 255 } else { 0 };
                img[((y * 64 + x) * 4) as usize..][..4].copy_from_slice(&[0, 200, 0, a]);
            }
        }
        let l = chain(64, 64, img.clone(), 3, MipMode::Colour { srgb: true, alpha_test: true });
        let c0 = coverage(&l[0].2, 0.5, 1.0);
        let c2 = coverage(&l[2].2, 0.5, 1.0);
        assert!((c0 - c2).abs() < 0.1, "{c0} vs {c2}");
    }

    #[test]
    fn mip_count() {
        assert_eq!(upscaled_mip_count(9, 4), 11);
        assert_eq!(upscaled_mip_count(1, 4), 1);
        assert_eq!(upscaled_mip_count(13, 2), 13);
    }
}
