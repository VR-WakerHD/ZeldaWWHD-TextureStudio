//! Nested containers: a disc file may be Yaz0-compressed, a SARC, and contain
//! further Yaz0/SARC files down to BFRES/BFLIM leaves. An `AssetLocation`
//! names a leaf by its disc path plus the chain of archive entry names.

use serde::{Deserialize, Serialize};

use crate::error::{bail, Result};
use crate::{bflim, bfres, sarc, yaz0};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetLocation {
    /// Path relative to the scanned root, with `/` separators.
    pub file: String,
    /// Archive entry names from the outer archive inwards; empty for a loose file.
    pub inner: Vec<String>,
}

impl AssetLocation {
    pub fn display(&self) -> String {
        let mut s = self.file.clone();
        for i in &self.inner {
            s.push_str(" › ");
            s.push_str(i);
        }
        s
    }
    pub fn leaf_name(&self) -> &str {
        self.inner.last().map(|s| s.as_str()).unwrap_or(&self.file)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeafKind {
    Bfres,
    Bflim,
}

pub fn leaf_kind(d: &[u8]) -> Option<LeafKind> {
    if bfres::is_bfres(d) {
        Some(LeafKind::Bfres)
    } else if bflim::is_bflim(d) {
        Some(LeafKind::Bflim)
    } else {
        None
    }
}

/// Describes what a byte blob is, for the inventory.
pub fn describe(d: &[u8]) -> &'static str {
    if yaz0::is_yaz0(d) {
        "Yaz0"
    } else if sarc::is_sarc(d) {
        "SARC"
    } else if bfres::is_bfres(d) {
        "BFRES"
    } else if bflim::is_bflim(d) {
        "BFLIM"
    } else if d.len() >= 4 && &d[..4] == b"BAHS" {
        "SHARCFB (shader)"
    } else if d.len() >= 4 && &d[..4] == b"FLYT" {
        "BFLYT (layout)"
    } else if d.len() >= 4 && &d[..4] == b"FLAN" {
        "BFLAN (animation)"
    } else if d.len() >= 3 && d[..3] == [0xFF, 0xD8, 0xFF] {
        "JPEG"
    } else {
        "other"
    }
}

const MAX_DEPTH: usize = 6;

/// Visit every leaf texture container below `data`. Errors inside one archive
/// entry are reported through `on_error` and do not stop the walk.
pub fn walk(
    file: &str,
    data: &[u8],
    on_leaf: &mut dyn FnMut(AssetLocation, LeafKind, &[u8]),
    on_other: &mut dyn FnMut(&AssetLocation, &'static str, usize),
    on_error: &mut dyn FnMut(&AssetLocation, String),
) {
    let loc = AssetLocation { file: file.to_string(), inner: Vec::new() };
    walk_inner(loc, data, 0, on_leaf, on_other, on_error);
}

fn walk_inner(
    loc: AssetLocation,
    data: &[u8],
    depth: usize,
    on_leaf: &mut dyn FnMut(AssetLocation, LeafKind, &[u8]),
    on_other: &mut dyn FnMut(&AssetLocation, &'static str, usize),
    on_error: &mut dyn FnMut(&AssetLocation, String),
) {
    if depth > MAX_DEPTH {
        on_error(&loc, "archive nesting too deep".into());
        return;
    }
    let owned;
    let data = if yaz0::is_yaz0(data) {
        match yaz0::decompress(data) {
            Ok(d) => {
                owned = d;
                &owned[..]
            }
            Err(e) => return on_error(&loc, e.to_string()),
        }
    } else {
        data
    };
    if sarc::is_sarc(data) {
        match sarc::Sarc::parse(data) {
            Ok(s) => {
                for f in &s.files {
                    let mut child = loc.clone();
                    child.inner.push(f.name.clone());
                    walk_inner(child, &f.data, depth + 1, on_leaf, on_other, on_error);
                }
            }
            Err(e) => on_error(&loc, e.to_string()),
        }
        return;
    }
    match leaf_kind(data) {
        Some(k) => on_leaf(loc, k, data),
        None => on_other(&loc, describe(data), data.len()),
    }
}

/// Fetch the decompressed bytes of the leaf at `loc` from the bytes of its disc file.
pub fn extract_leaf(file_data: &[u8], inner: &[String]) -> Result<Vec<u8>> {
    let mut cur = if yaz0::is_yaz0(file_data) { yaz0::decompress(file_data)? } else { file_data.to_vec() };
    for name in inner {
        if !sarc::is_sarc(&cur) {
            bail!("expected an archive before {name:?}");
        }
        let s = sarc::Sarc::parse(&cur)?;
        let Some(f) = s.file(name) else { bail!("archive entry {name:?} not found") };
        cur = if yaz0::is_yaz0(&f.data) { yaz0::decompress(&f.data)? } else { f.data.clone() };
    }
    Ok(cur)
}

/// Rebuild a disc file with some leaves replaced. `replace` maps an inner
/// chain to the new (decompressed) leaf bytes. Every level keeps its original
/// compression (Yaz0 header extra bytes preserved) and SARC layout rules.
pub fn rebuild_file(file_data: &[u8], replace: &std::collections::BTreeMap<Vec<String>, Vec<u8>>) -> Result<Vec<u8>> {
    rebuild_level(file_data, &[], replace)
}

fn rebuild_level(
    data: &[u8],
    prefix: &[String],
    replace: &std::collections::BTreeMap<Vec<String>, Vec<u8>>,
) -> Result<Vec<u8>> {
    if let Some(new) = replace.get(prefix) {
        return Ok(if yaz0::is_yaz0(data) { yaz0::compress(new, yaz0::header_extra(data)) } else { new.clone() });
    }
    let compressed = yaz0::is_yaz0(data);
    let plain = if compressed { yaz0::decompress(data)? } else { data.to_vec() };
    if !sarc::is_sarc(&plain) {
        bail!("no replacement reaches {:?}", prefix);
    }
    let mut s = sarc::Sarc::parse(&plain)?;
    let mut changed = false;
    for f in s.files.iter_mut() {
        let mut p = prefix.to_vec();
        p.push(f.name.clone());
        if replace.keys().any(|k| k.starts_with(&p)) {
            f.data = rebuild_level(&f.data, &p, replace)?;
            changed = true;
        }
    }
    if !changed {
        bail!("no replacement matched inside {:?}", prefix);
    }
    let built = s.build();
    Ok(if compressed { yaz0::compress(&built, yaz0::header_extra(data)) } else { built })
}
