//! Tests against the user's own disc dump. They run when WWHD_DISC points at
//! the disc folder (containing content/); otherwise they pass without doing
//! anything, because game files are not part of the repository.

use std::path::PathBuf;

use wwhd_tex::bfres::{self, Bfres};
use wwhd_tex::container::{self, LeafKind};
use wwhd_tex::resample::{self, Edge, Filter};
use wwhd_tex::{gx2, mips, sarc, texture, yaz0};

fn disc() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("WWHD_DISC").ok()?);
    p.join("content").is_dir().then_some(p)
}

const SAMPLES: [&str; 5] = [
    "content/Common/Object/Bgn.szs",
    "content/Common/Layout/Title_00.szs",
    "content/Common/Stage/sea_T_Stage.szs",
    "content/Common/Particle/Particle.szs",
    "content/Common/ProgramTexture/ProgramTexture.szs",
];

#[test]
fn sarc_rebuild_is_byte_identical() {
    let Some(d) = disc() else { return };
    for f in SAMPLES {
        let raw = std::fs::read(d.join(f)).unwrap();
        let plain = yaz0::decompress(&raw).unwrap();
        let s = sarc::Sarc::parse(&plain).unwrap();
        assert_eq!(s.build(), plain, "{f}");
    }
}

#[test]
fn yaz0_recompression_roundtrips() {
    let Some(d) = disc() else { return };
    let raw = std::fs::read(d.join(SAMPLES[1])).unwrap();
    let plain = yaz0::decompress(&raw).unwrap();
    let again = yaz0::compress(&plain, yaz0::header_extra(&raw));
    assert_eq!(&again[..16], &raw[..16]);
    assert_eq!(yaz0::decompress(&again).unwrap(), plain);
}

#[test]
fn every_texture_in_samples_passes_gate_and_decodes() {
    let Some(d) = disc() else { return };
    for f in SAMPLES {
        let raw = std::fs::read(d.join(f)).unwrap();
        container::walk(
            f,
            &raw,
            &mut |loc, kind, leaf| {
                for t in texture::list(&loc, kind, leaf).unwrap() {
                    if t.surface.depth > 1 {
                        continue;
                    }
                    assert!(texture::gate(&t, leaf).passed(), "{}", t.id.key());
                    let dec = texture::decode(&t, leaf).unwrap();
                    assert_eq!(dec.levels.len() as u32, t.surface.mip_count.max(1));
                    assert_eq!(dec.levels[0].0, t.surface.width);
                }
            },
            &mut |_, _, _| {},
            &mut |loc, e| panic!("{}: {e}", loc.display()),
        );
    }
}

#[test]
fn bfres_replacement_reads_back() {
    let Some(d) = disc() else { return };
    let raw = std::fs::read(d.join(SAMPLES[0])).unwrap();
    let inner = vec!["Bgn.bfres".to_string()];
    let leaf = container::extract_leaf(&raw, &inner).unwrap();
    let loc = container::AssetLocation { file: SAMPLES[0].into(), inner: inner.clone() };
    let list = texture::list(&loc, LeafKind::Bfres, &leaf).unwrap();
    // First BC1 texture with mips.
    let t = list.iter().find(|t| gx2::codec(t.surface.format).ok() == Some(gx2::Codec::Bc1) && t.surface.mip_count > 1).unwrap();
    let dec = texture::decode(t, &leaf).unwrap();
    let (w, h, base) = &dec.levels[0];
    let (nw, nh) = (w * 2, h * 2);
    let up = resample::resize_rgba8(*w, *h, base, nw, nh, Filter::Lanczos3, Edge::Wrap);
    let levels = mips::chain(nw, nh, up.clone(), mips::upscaled_mip_count(t.surface.mip_count, 2), mips::MipMode::Colour { srgb: true, alpha_test: false });
    let rep = texture::build_replacement(t, &levels).unwrap();
    let new_leaf = bfres::replace_textures(&leaf, &[(t.id.index, rep)]).unwrap();
    let b = Bfres::parse(&new_leaf).unwrap();
    assert_eq!(b.textures.len(), list.len());
    let list2 = texture::list(&loc, LeafKind::Bfres, &new_leaf).unwrap();
    let t2 = &list2[t.id.index];
    assert_eq!((t2.surface.width, t2.surface.height), (nw, nh));
    assert!(texture::gate(t2, &new_leaf).passed());
    let dec2 = texture::decode(t2, &new_leaf).unwrap();
    let diff: f64 = dec2.levels[0].2.iter().zip(&up).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum::<f64>() / up.len() as f64;
    let psnr = 10.0 * (255.0f64 * 255.0 / diff.max(1e-9)).log10();
    assert!(psnr > 28.0, "BC1 re-encode PSNR {psnr}");
    for (a, b) in list.iter().zip(&list2) {
        if a.id.index != t.id.index {
            assert_eq!(a.data_hash, b.data_hash, "{} changed", a.id.name);
        }
    }
    // Whole-file rebuild keeps every other entry.
    let mut map = std::collections::BTreeMap::new();
    map.insert(inner.clone(), new_leaf.clone());
    let rebuilt = container::rebuild_file(&raw, &map).unwrap();
    assert_eq!(container::extract_leaf(&rebuilt, &inner).unwrap(), new_leaf);
    let s1 = sarc::Sarc::parse(&yaz0::decompress(&raw).unwrap()).unwrap();
    let s2 = sarc::Sarc::parse(&yaz0::decompress(&rebuilt).unwrap()).unwrap();
    for (x, y) in s1.files.iter().zip(&s2.files) {
        assert_eq!(x.name, y.name);
        if x.name != "Bgn.bfres" {
            assert_eq!(x.data, y.data);
        }
    }
}
