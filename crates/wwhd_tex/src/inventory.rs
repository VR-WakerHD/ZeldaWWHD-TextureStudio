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
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
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
                                    let cl = classify::classify(kind, &rel, &t.id.name, t.surface.format, t.surface.width, t.surface.height);
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
