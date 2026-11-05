//! Image measurements for comparing upscales and enhancement profiles. Numbers help spot regressions; they are no
//! substitute for looking at the pictures.

use rayon::prelude::*;
use serde::Serialize;

use crate::enhance::linear_to_oklab;
use crate::resample::{self, Edge, Filter};

fn lut() -> Vec<f32> {
    (0..256)
        .map(|i| {
            let v = i as f32 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        })
        .collect()
}

/// Oklab of every texel.
pub fn oklab(rgba: &[u8]) -> Vec<[f32; 3]> {
    let t = lut();
    rgba.par_chunks_exact(4).map(|p| linear_to_oklab([t[p[0] as usize], t[p[1] as usize], t[p[2] as usize]])).collect()
}

fn weights(rgba: &[u8]) -> Vec<f32> {
    rgba.chunks_exact(4).map(|p| if p[3] > 0 { 1.0 } else { 0.0 }).collect()
}

fn wmean(v: impl Iterator<Item = (f32, f32)>) -> f32 {
    let (s, n) = v.fold((0f64, 0f64), |(s, n), (x, w)| (s + (x * w) as f64, n + w as f64));
    if n > 0.0 { (s / n) as f32 } else { 0.0 }
}

/// Mean Sobel gradient magnitude of lightness over visible texels (higher = crisper edges and detail).
pub fn sharpness(w: u32, h: u32, rgba: &[u8]) -> f32 {
    let l: Vec<f32> = oklab(rgba).iter().map(|v| v[0]).collect();
    let wt = weights(rgba);
    let (w, h) = (w as usize, h as usize);
    let at = |x: i64, y: i64| l[y.clamp(0, h as i64 - 1) as usize * w + x.clamp(0, w as i64 - 1) as usize];
    let g: Vec<f32> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = ((i % w) as i64, (i / w) as i64);
            let gx = at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1) - at(x - 1, y - 1) - 2.0 * at(x - 1, y) - at(x - 1, y + 1);
            let gy = at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1) - at(x - 1, y - 1) - 2.0 * at(x, y - 1) - at(x + 1, y - 1);
            (gx * gx + gy * gy).sqrt() / 8.0
        })
        .collect();
    wmean(g.into_iter().zip(wt))
}

/// Hasler & Süsstrunk colourfulness of the visible texels (sRGB values, 0..255 scale).
pub fn colourfulness(rgba: &[u8]) -> f32 {
    let vis: Vec<(f32, f32)> = rgba
        .chunks_exact(4)
        .filter(|p| p[3] > 0)
        .map(|p| {
            let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
            (r - g, 0.5 * (r + g) - b)
        })
        .collect();
    let n = vis.len().max(1) as f32;
    let (mrg, myb) = (vis.iter().map(|v| v.0).sum::<f32>() / n, vis.iter().map(|v| v.1).sum::<f32>() / n);
    let (vrg, vyb) = (vis.iter().map(|v| (v.0 - mrg).powi(2)).sum::<f32>() / n, vis.iter().map(|v| (v.1 - myb).powi(2)).sum::<f32>() / n);
    (vrg + vyb).sqrt() + 0.3 * (mrg * mrg + myb * myb).sqrt()
}

/// Mean Oklab chroma of visible texels.
pub fn mean_chroma(rgba: &[u8]) -> f32 {
    let lab = oklab(rgba);
    wmean(lab.iter().zip(weights(rgba)).map(|(v, w)| ((v[1] * v[1] + v[2] * v[2]).sqrt(), w)))
}

/// Mean Oklab ΔE (×100, ~1 = just noticeable) between two images of the same size, over texels visible in `a`.
pub fn delta_e(a: &[u8], b: &[u8]) -> f32 {
    let (la, lb) = (oklab(a), oklab(b));
    wmean(la.iter().zip(&lb).zip(weights(a)).map(|((x, y), w)| {
        let d = ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt() * 100.0;
        (d, w)
    }))
}

/// Mean lightness change (Oklab L ×100) from `a` to `b`.
pub fn lightness_shift(a: &[u8], b: &[u8]) -> f32 {
    let (la, lb) = (oklab(a), oklab(b));
    wmean(la.iter().zip(&lb).zip(weights(a)).map(|((x, y), w)| ((y[0] - x[0]) * 100.0, w)))
}

/// Fraction of visible texels of `b` whose lightness leaves the range of `reference` in their 5×5 neighbourhood by
/// more than `margin` (Oklab L): ringing and halos the enhancement added.
pub fn halo_fraction(w: u32, h: u32, reference: &[u8], b: &[u8], margin: f32) -> f32 {
    let (w, h) = (w as usize, h as usize);
    let lr: Vec<f32> = oklab(reference).iter().map(|v| v[0]).collect();
    let lb: Vec<f32> = oklab(b).iter().map(|v| v[0]).collect();
    let wt = weights(reference);
    let (over, n) = (0..h)
        .into_par_iter()
        .map(|y| {
            let (mut over, mut n) = (0usize, 0usize);
            for x in 0..w {
                let i = y * w + x;
                if wt[i] == 0.0 {
                    continue;
                }
                n += 1;
                let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
                for dy in -2i64..=2 {
                    for dx in -2i64..=2 {
                        let (xx, yy) = ((x as i64 + dx).clamp(0, w as i64 - 1) as usize, (y as i64 + dy).clamp(0, h as i64 - 1) as usize);
                        let v = lr[yy * w + xx];
                        mn = mn.min(v);
                        mx = mx.max(v);
                    }
                }
                if lb[i] > mx + margin || lb[i] < mn - margin {
                    over += 1;
                }
            }
            (over, n)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    over as f32 / n.max(1) as f32
}

/// PSNR of RGB over texels of `a` at least half opaque (all texels when none is).
pub fn psnr_rgb(a: &[u8], b: &[u8]) -> f32 {
    let (mut se, mut n) = (0f64, 0f64);
    let any = a.chunks_exact(4).any(|p| p[3] >= 128);
    for (x, y) in a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(x, _)| !any || x[3] >= 128) {
        for c in 0..3 {
            se += (x[c] as f64 - y[c] as f64).powi(2);
        }
        n += 3.0;
    }
    let mse = se / n.max(1.0);
    if mse <= 1e-9 { 99.0 } else { (10.0 * (255.0f64 * 255.0 / mse).log10()) as f32 }
}

/// Mean SSIM of lightness (8×8 windows, stride 4) over texels at least half opaque, in windows where those are the
/// majority. Faint edge texels are left out: their colour barely shows, and upscalers fill it differently.
pub fn ssim(w: u32, h: u32, a: &[u8], b: &[u8]) -> f32 {
    let (w, h) = (w as usize, h as usize);
    let la: Vec<f32> = oklab(a).iter().map(|v| v[0]).collect();
    let lb: Vec<f32> = oklab(b).iter().map(|v| v[0]).collect();
    let (c1, c2) = (0.01f32.powi(2), 0.03f32.powi(2));
    let win = 8.min(w).min(h).max(1);
    let (mut s, mut n) = (0f64, 0f64);
    let mut y = 0;
    while y + win <= h {
        let mut x = 0;
        while x + win <= w {
            let idx: Vec<usize> = (0..win * win).map(|k| (y + k / win) * w + x + k % win).filter(|&i| a[i * 4 + 3] >= 128).collect();
            if idx.len() > win * win / 2 {
                let m = idx.len() as f32;
                let (ma, mb) = (idx.iter().map(|&i| la[i]).sum::<f32>() / m, idx.iter().map(|&i| lb[i]).sum::<f32>() / m);
                let va = idx.iter().map(|&i| (la[i] - ma).powi(2)).sum::<f32>() / m;
                let vb = idx.iter().map(|&i| (lb[i] - mb).powi(2)).sum::<f32>() / m;
                let cov = idx.iter().map(|&i| (la[i] - ma) * (lb[i] - mb)).sum::<f32>() / m;
                s += (((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))) as f64;
                n += 1.0;
            }
            x += 4;
        }
        y += 4;
    }
    if n > 0.0 { (s / n) as f32 } else { 1.0 }
}

/// An upscaled image reduced back to the source size (Mitchell, in stored values) for comparing with the source.
pub fn reduce(uw: u32, uh: u32, up: &[u8], sw: u32, sh: u32) -> Vec<u8> {
    resample::resize_rgba8(uw, uh, up, sw, sh, Filter::Mitchell, Edge::Clamp)
}

/// Whether alpha is byte-identical, and colour under alpha 0 too.
pub fn alpha_intact(a: &[u8], b: &[u8]) -> (bool, bool) {
    let alpha = a.chunks_exact(4).zip(b.chunks_exact(4)).all(|(x, y)| x[3] == y[3]);
    let hidden = a.chunks_exact(4).zip(b.chunks_exact(4)).all(|(x, y)| x[3] != 0 || x == y);
    (alpha, hidden)
}

/// Every measurement of one variant.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub sharpness: f32,
    pub colourfulness: f32,
    pub chroma: f32,
    /// Against the unenhanced upscale.
    pub delta_e: f32,
    pub lightness_shift: f32,
    pub halo_fraction: f32,
    /// The variant reduced to the source size against the source: faithfulness to the original art.
    pub fidelity_ssim: f32,
    pub fidelity_psnr: f32,
    pub alpha_intact: bool,
    pub hidden_colour_intact: bool,
    pub millis: f32,
}

/// Measure `v` (upscaled size) against the unenhanced upscale `raw` and the source.
pub fn report(uw: u32, uh: u32, raw: &[u8], v: &[u8], sw: u32, sh: u32, src: &[u8], millis: f32) -> Report {
    let small = reduce(uw, uh, v, sw, sh);
    let (alpha, hidden) = alpha_intact(raw, v);
    Report {
        sharpness: sharpness(uw, uh, v),
        colourfulness: colourfulness(v),
        chroma: mean_chroma(v),
        delta_e: delta_e(raw, v),
        lightness_shift: lightness_shift(raw, v),
        halo_fraction: halo_fraction(uw, uh, raw, v, 3.0 / 255.0),
        fidelity_ssim: ssim(sw, sh, src, &small),
        fidelity_psnr: psnr_rgb(src, &small),
        alpha_intact: alpha,
        hidden_colour_intact: hidden,
        millis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_images_measure_as_identical() {
        let img: Vec<u8> = (0..64u32).flat_map(|i| [(i * 4) as u8, 100, (255 - i * 4) as u8, 255]).collect();
        assert!((ssim(8, 8, &img, &img) - 1.0).abs() < 1e-4);
        assert_eq!(psnr_rgb(&img, &img), 99.0);
        assert!(delta_e(&img, &img) < 1e-4);
        assert_eq!(halo_fraction(8, 8, &img, &img, 0.01), 0.0);
        assert_eq!(alpha_intact(&img, &img), (true, true));
    }

    #[test]
    fn fidelity_ignores_colour_under_faint_alpha() {
        // Same visible picture; only the colour under alpha 10 differs.
        let a: Vec<u8> = (0..256u32).flat_map(|i| if i % 16 < 8 { [200, 100, 50, 255] } else { [0, 0, 0, 10] }).collect();
        let mut b = a.clone();
        for p in b.chunks_exact_mut(4).filter(|p| p[3] == 10) {
            p[..3].copy_from_slice(&[255, 255, 255]);
        }
        assert!((ssim(16, 16, &a, &b) - 1.0).abs() < 1e-4);
        assert_eq!(psnr_rgb(&a, &b), 99.0);
    }

    #[test]
    fn sharper_and_more_colourful_measure_higher() {
        let soft: Vec<u8> = (0..256u32).flat_map(|i| { let v = 100 + (i % 16) as u8; [v, v, v, 255] }).collect();
        let hard: Vec<u8> = (0..256u32).flat_map(|i| { let v = if i % 16 < 8 { 60 } else { 200 }; [v, v, v, 255] }).collect();
        assert!(sharpness(16, 16, &hard) > sharpness(16, 16, &soft));
        let grey = vec![128u8; 64];
        let red: Vec<u8> = (0..16).flat_map(|_| [200, 40, 40, 255]).collect();
        assert!(colourfulness(&red) > colourfulness(&grey));
    }
}
