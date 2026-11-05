//! Separable resampling of float images with wrap or clamp edge addressing.
//! Used for conventional upscaling, alpha, masks, normal vectors and padding.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Filter {
    Nearest,
    Bilinear,
    CatmullRom,
    Mitchell,
    Lanczos3,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::Nearest, Filter::Bilinear, Filter::CatmullRom, Filter::Mitchell, Filter::Lanczos3];
    pub fn label(self) -> &'static str {
        match self {
            Filter::Nearest => "Nearest (pixel art)",
            Filter::Bilinear => "Bilinear",
            Filter::CatmullRom => "Catmull-Rom",
            Filter::Mitchell => "Mitchell",
            Filter::Lanczos3 => "Lanczos-3",
        }
    }
    fn support(self) -> f32 {
        match self {
            Filter::Nearest => 0.5,
            Filter::Bilinear => 1.0,
            Filter::CatmullRom | Filter::Mitchell => 2.0,
            Filter::Lanczos3 => 3.0,
        }
    }
    fn weight(self, x: f32) -> f32 {
        let x = x.abs();
        let cubic = |b: f32, c: f32| {
            if x < 1.0 {
                ((12.0 - 9.0 * b - 6.0 * c) * x * x * x + (-18.0 + 12.0 * b + 6.0 * c) * x * x + (6.0 - 2.0 * b)) / 6.0
            } else if x < 2.0 {
                ((-b - 6.0 * c) * x * x * x + (6.0 * b + 30.0 * c) * x * x + (-12.0 * b - 48.0 * c) * x + (8.0 * b + 24.0 * c)) / 6.0
            } else {
                0.0
            }
        };
        match self {
            Filter::Nearest => {
                if x <= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            Filter::Bilinear => (1.0 - x).max(0.0),
            Filter::CatmullRom => cubic(0.0, 0.5),
            Filter::Mitchell => cubic(1.0 / 3.0, 1.0 / 3.0),
            Filter::Lanczos3 => {
                if x < 1e-6 {
                    1.0
                } else if x < 3.0 {
                    let px = std::f32::consts::PI * x;
                    3.0 * px.sin() * (px / 3.0).sin() / (px * px)
                } else {
                    0.0
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Wrap,
    Clamp,
}

pub(crate) fn address(i: i64, n: usize, edge: Edge) -> usize {
    match edge {
        Edge::Wrap => i.rem_euclid(n as i64) as usize,
        Edge::Clamp => i.clamp(0, n as i64 - 1) as usize,
    }
}

/// Precomputed taps for one output coordinate.
fn taps(src: usize, dst: usize, filter: Filter, edge: Edge) -> Vec<Vec<(usize, f32)>> {
    let scale = dst as f32 / src as f32;
    let fscale = if scale < 1.0 { 1.0 / scale } else { 1.0 };
    let support = filter.support() * fscale;
    (0..dst)
        .map(|o| {
            let center = (o as f32 + 0.5) / scale - 0.5;
            if filter == Filter::Nearest {
                let i = (center + 0.5).floor() as i64;
                return vec![(address(i, src, edge), 1.0)];
            }
            let lo = (center - support).floor() as i64;
            let hi = (center + support).ceil() as i64;
            let mut t: Vec<(usize, f32)> = (lo..=hi)
                .map(|i| (address(i, src, edge), filter.weight((i as f32 - center) / fscale)))
                .filter(|(_, w)| w.abs() > 1e-7)
                .collect();
            let sum: f32 = t.iter().map(|(_, w)| w).sum();
            if sum.abs() > 1e-7 {
                for x in t.iter_mut() {
                    x.1 /= sum;
                }
            }
            t
        })
        .collect()
}

/// Resize an interleaved float image with `ch` channels.
pub fn resize(w: usize, h: usize, ch: usize, src: &[f32], nw: usize, nh: usize, filter: Filter, edge: Edge) -> Vec<f32> {
    assert_eq!(src.len(), w * h * ch);
    let tx = taps(w, nw, filter, edge);
    let ty = taps(h, nh, filter, edge);
    // Horizontal pass: h rows of nw.
    let mut mid = vec![0f32; nw * h * ch];
    mid.par_chunks_mut(nw * ch).enumerate().for_each(|(y, row)| {
        let s = &src[y * w * ch..(y + 1) * w * ch];
        for (x, t) in tx.iter().enumerate() {
            for c in 0..ch {
                let mut acc = 0.0;
                for &(i, wgt) in t {
                    acc += s[i * ch + c] * wgt;
                }
                row[x * ch + c] = acc;
            }
        }
    });
    let mut out = vec![0f32; nw * nh * ch];
    out.par_chunks_mut(nw * ch).enumerate().for_each(|(y, row)| {
        for &(i, wgt) in &ty[y] {
            let m = &mid[i * nw * ch..(i + 1) * nw * ch];
            for (o, v) in row.iter_mut().zip(m) {
                *o += v * wgt;
            }
        }
    });
    out
}

pub fn u8_to_f32(d: &[u8]) -> Vec<f32> {
    d.iter().map(|&v| v as f32 / 255.0).collect()
}

pub fn f32_to_u8(d: &[f32]) -> Vec<u8> {
    d.iter().map(|&v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect()
}

/// Resize an RGBA8 image.
pub fn resize_rgba8(w: u32, h: u32, rgba: &[u8], nw: u32, nh: u32, filter: Filter, edge: Edge) -> Vec<u8> {
    let f = u8_to_f32(rgba);
    f32_to_u8(&resize(w as usize, h as usize, 4, &f, nw as usize, nh as usize, filter, edge))
}

/// Pad an RGBA8 image by `m` pixels on every side using the edge mode.
pub fn pad_rgba8(w: u32, h: u32, rgba: &[u8], m: u32, edge: Edge) -> (u32, u32, Vec<u8>) {
    let (pw, ph) = (w + 2 * m, h + 2 * m);
    let mut out = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph {
        let sy = address(y as i64 - m as i64, h as usize, edge);
        for x in 0..pw {
            let sx = address(x as i64 - m as i64, w as usize, edge);
            let s = (sy * w as usize + sx) * 4;
            let d = ((y * pw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (pw, ph, out)
}

/// Crop a rectangle out of an RGBA8 image.
pub fn crop_rgba8(w: u32, rgba: &[u8], x0: u32, y0: u32, cw: u32, ch: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y0..y0 + ch {
        let s = ((y * w + x0) * 4) as usize;
        out.extend_from_slice(&rgba[s..s + (cw * 4) as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_stays_constant_and_dims() {
        let src = vec![0.25f32; 8 * 4 * 2];
        for f in Filter::ALL {
            let out = resize(8, 4, 2, &src, 32, 16, f, Edge::Wrap);
            assert_eq!(out.len(), 32 * 16 * 2);
            assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-4), "{f:?}");
        }
    }

    #[test]
    fn nearest_upscale_is_exact_repeat() {
        let src: Vec<u8> = (0..16u8).flat_map(|i| [i * 10, i, 255 - i, 255]).collect();
        let out = resize_rgba8(4, 4, &src, 16, 16, Filter::Nearest, Edge::Clamp);
        assert_eq!(&out[..4], &src[..4]);
        // Output pixel 3 of row 0 still maps to source pixel 0; pixel 4 to source pixel 1.
        assert_eq!(&out[3 * 4..3 * 4 + 4], &src[..4]);
        assert_eq!(&out[4 * 4..4 * 4 + 4], &src[4..8]);
    }

    #[test]
    fn pad_and_crop_roundtrip() {
        let src: Vec<u8> = (0..(5 * 3 * 4)).map(|i| i as u8).collect();
        let (pw, _, p) = pad_rgba8(5, 3, &src, 2, Edge::Wrap);
        assert_eq!(crop_rgba8(pw, &p, 2, 2, 5, 3), src);
    }
}
