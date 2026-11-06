//! Disc scan: every file, every archive entry, every texture, with hashes,
//! rewrite-gate results and an initial classification.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::classify::{self, Classification};
use crate::container::{self, LeafKind};
use crate::error::{Error, Result};
use crate::gx2::Gx2Surface;
use crate::jobs::Progress;
use crate::project::ScanScope;
use crate::texture::{self, TextureId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
    /// Size after Yaz0 decompression (what the game holds in memory); equals
    /// `size` for uncompressed files.
    #[serde(default)]
    pub plain_size: u64,
    pub hash: String,
    pub kind: String,
    pub leaves: usize,
    pub textures: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextureRecord {
    pub id: TextureId,
    pub kind: LeafKind,
    pub surface: Gx2Surface,
    pub comp_sel: [u8; 4],
    /// blake3 of the original tiled data.
    pub data_hash: String,
    /// Content key: identical textures in different archives share it.
    pub key: String,
    pub gate_ok: bool,
    pub gate_detail: String,
    pub decodable: bool,
    pub classification: Classification,
}

impl TextureRecord {
    pub fn max_side(&self) -> u32 {
        self.surface.width.max(self.surface.height)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub root: String,
    pub created_unix: u64,
    pub tool_version: String,
    pub files: Vec<FileRecord>,
    pub textures: Vec<TextureRecord>,
    /// Non-texture leaves by kind (shaders, layouts, animations, ...).
    pub other_leaves: BTreeMap<String, usize>,
    pub errors: Vec<String>,
}

impl Inventory {
    pub fn load(path: &Path) -> Result<Inventory> {
        let mut inv: Inventory = serde_json::from_slice(&std::fs::read(path)?)?;
        inv.reclassify_stale();
        Ok(inv)
    }
    /// Classify again what older scans got wrong or did not know: textures classed as normal maps that are not
    /// (two-channel luminance + alpha; colour textures named "normal"), and eyes (a class added later). Other
    /// records keep their class (it may carry a pixel-based refinement); manual choices are always kept.
    /// Returns how many records changed.
    pub fn reclassify_stale(&mut self) -> usize {
        let mut n = 0;
        for t in &mut self.textures {
            if t.classification.manual {
                continue;
            }
            let new = classify::classify_texture(t.kind, &t.id.location.file, &t.id.name, t.surface.format, t.surface.width, t.surface.height, t.comp_sel);
            let misread = t.classification.class == classify::Class::NormalMap && new.class != classify::Class::NormalMap;
            let eyes = new.class == classify::Class::Eyes && t.classification.class != classify::Class::Eyes;
            if misread || eyes {
                t.classification = new;
                n += 1;
            }
        }
        n
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::project::write_atomic(path, &serde_json::to_vec(self)?)
    }
    /// One representative record per content key.
    pub fn unique(&self) -> Vec<&TextureRecord> {
        let mut seen = std::collections::HashSet::new();
        self.textures.iter().filter(|t| seen.insert(t.key.clone())).collect()
    }
}

pub fn content_key(data_hash: &str, s: &Gx2Surface) -> String {
    let mut h = blake3::Hasher::new();
    h.update(data_hash.as_bytes());
    for v in [s.format, s.width, s.height, s.depth, s.mip_count, s.tile_mode, s.swizzle, s.dim] {
        h.update(&v.to_le_bytes());
    }
    h.finalize().to_hex()[..32].to_string()
}

/// Files selected by the scope, relative to `root`, sorted.
pub fn select_files(root: &Path, scope: &ScanScope) -> Result<Vec<PathBuf>> {
    let starts: Vec<PathBuf> = match scope {
        ScanScope::All => vec![root.to_path_buf()],
        ScanScope::ContentOnly => {
            let c = root.join("content");
            vec![if c.is_dir() { c } else { root.to_path_buf() }]
        }
        ScanScope::Selected(list) => list.iter().map(|p| root.join(p)).collect(),
    };
    let mut out = Vec::new();
    for s in starts {
        collect(&s, &mut out)?;
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect(p: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_file() {
        out.push(p.to_path_buf());
        return Ok(());
    }
    for e in std::fs::read_dir(p)? {
        collect(&e?.path(), out)?;
    }
    Ok(())
}

pub fn relative(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Scan the selected files. Only files that may contain textures are opened
/// as archives; executables and other files are inventoried by hash only.
pub fn scan(root: &Path, scope: &ScanScope, progress: &Progress) -> Result<Inventory> {
    let files = select_files(root, scope)?;
    progress.begin("Scanning", files.len());
    let others = Mutex::new(BTreeMap::<String, usize>::new());
    let results: Vec<(FileRecord, Vec<TextureRecord>, Vec<String>)> = files
        .par_iter()
        .map(|f| {
            if progress.cancelled() {
                return Err(Error::Cancelled);
            }
            progress.wait_if_paused();
            let rel = relative(root, f);
            progress.item(&rel);
            let data = std::fs::read(f)?;
            let mut rec = FileRecord {
                path: rel.clone(),
                size: data.len() as u64,
                plain_size: if crate::yaz0::is_yaz0(&data) { crate::bytes::be32(&data, 4).unwrap_or(0) as u64 } else { data.len() as u64 },
                hash: blake3::hash(&data).to_hex().to_string(),
                kind: container::describe(&data).to_string(),
                leaves: 0,
                textures: 0,
                error: None,
            };
            let mut textures = Vec::new();
            let errors = std::cell::RefCell::new(Vec::new());
            let archive_like = matches!(rec.kind.as_str(), "Yaz0" | "SARC" | "BFRES" | "BFLIM");
            if archive_like {
                container::walk(
                    &rel,
                    &data,
                    &mut |loc, kind, leaf| {
                        rec.leaves += 1;
                        match texture::list(&loc, kind, leaf) {
                            Ok(list) => {
                                for t in list {
                                    let g = texture::gate(&t, leaf);
                                    let decodable = t.surface.depth <= 1 && crate::gx2::codec(t.surface.format).is_ok();
                                    let cl = classify::classify_texture(kind, &rel, &t.id.name, t.surface.format, t.surface.width, t.surface.height, t.comp_sel);
                                    textures.push(TextureRecord {
                                        key: content_key(&t.data_hash, &t.surface),
                                        id: t.id,
                                        kind,
                                        surface: t.surface,
                                        comp_sel: t.comp_sel,
                                        data_hash: t.data_hash,
                                        gate_ok: g.passed(),
                                        gate_detail: g.detail.trim().to_string(),
                                        decodable,
                                        classification: cl,
                                    });
                                }
                            }
                            Err(e) => errors.borrow_mut().push(format!("{}: {e}", loc.display())),
                        }
                    },
                    &mut |_, kind, _| *others.lock().unwrap().entry(kind.to_string()).or_default() += 1,
                    &mut |loc, e| errors.borrow_mut().push(format!("{}: {e}", loc.display())),
                );
            }
            let errors = errors.into_inner();
            rec.textures = textures.len();
            if !errors.is_empty() {
                rec.error = Some(errors.join("; "));
            }
            progress.step();
            Ok((rec, textures, errors))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut inv = Inventory {
        root: root.to_string_lossy().into_owned(),
        created_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        other_leaves: others.into_inner().unwrap(),
        ..Default::default()
    };
    for (f, t, e) in results {
        inv.files.push(f);
        inv.textures.extend(t);
        inv.errors.extend(e);
    }
    progress.log(format!(
        "Scan finished: {} files, {} textures ({} unique), {} errors",
        inv.files.len(),
        inv.textures.len(),
        inv.unique().len(),
        inv.errors.len()
    ));
    Ok(inv)
}

#[cfg(test)]
mod lum_alpha_migration_tests {
    use super::*;
    use crate::classify::Class;

    fn rec(name: &str, comp_sel: [u8; 4], class: Class, manual: bool) -> TextureRecord {
        TextureRecord {
            id: TextureId { location: container::AssetLocation { file: "content/Common/Object/Link.szs".into(), inner: vec![] }, index: 0, name: name.into() },
            kind: LeafKind::Bfres,
            surface: Gx2Surface { format: 0x35, width: 64, height: 64, depth: 1, mip_count: 1, ..Default::default() },
            comp_sel,
            data_hash: String::new(),
            key: name.into(),
            gate_ok: true,
            gate_detail: String::new(),
            decodable: true,
            classification: Classification { class, confidence: 0.6, reason: "BC5 two-channel texture (normal maps use BC5)".into(), manual },
        }
    }

    #[test]
    fn old_inventories_reclassify_misread_normal_maps_and_eyes_but_keep_normal_maps_and_manual_choices() {
        let mut inv = Inventory {
            textures: vec![rec("eyeh.2", [0, 0, 0, 1], Class::NormalMap, false), rec("himo_nml", [0, 1, 4, 5], Class::NormalMap, false), rec("mayuh.1", [0, 0, 0, 1], Class::NormalMap, true)],
            ..Default::default()
        };
        assert_eq!(inv.reclassify_stale(), 1);
        assert_eq!(inv.textures[0].classification.class, Class::Eyes);
        assert_eq!(inv.textures[1].classification.class, Class::NormalMap, "a real normal map");
        assert_eq!(inv.textures[2].classification.class, Class::NormalMap, "the user's choice");
        assert_eq!(inv.reclassify_stale(), 0, "once");
    }
}
