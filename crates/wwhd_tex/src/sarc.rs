//! SARC archives (big- or little-endian). Rebuilding keeps the original header,
//! node table, name table, file order and each file's data alignment, and only
//! moves file data, so name hashes and the hash key are untouched.

use crate::bytes::{align_up, natural_alignment, slice};
use crate::error::{bail, Result};

#[derive(Clone, Debug)]
pub struct SarcFile {
    pub name: String,
    pub data: Vec<u8>,
    /// Alignment of this file's data in the original archive.
    pub alignment: usize,
}

#[derive(Clone, Debug)]
pub struct Sarc {
    big_endian: bool,
    /// Bytes from the start of the archive up to the data region (header, SFAT, SFNT).
    prefix: Vec<u8>,
    nodes_off: usize,
    pub files: Vec<SarcFile>,
}

/// Cap for per-file alignment inference. GX2 surfaces need at most 0x2000.
const MAX_ALIGN: usize = 0x2000;

pub fn is_sarc(d: &[u8]) -> bool {
    d.len() >= 0x14 && &d[..4] == b"SARC"
}

impl Sarc {
    pub fn parse(d: &[u8]) -> Result<Sarc> {
        if !is_sarc(d) {
            bail!("not a SARC archive");
        }
        let big_endian = match slice(d, 6, 2)? {
            [0xFE, 0xFF] => true,
            [0xFF, 0xFE] => false,
            _ => bail!("SARC has an invalid byte-order mark"),
        };
        let r16 = |o: usize| -> Result<u16> {
            let b: [u8; 2] = slice(d, o, 2)?.try_into().unwrap();
            Ok(if big_endian { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
        };
        let r32 = |o: usize| -> Result<u32> {
            let b: [u8; 4] = slice(d, o, 4)?.try_into().unwrap();
            Ok(if big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
        };
        let header_len = r16(4)? as usize;
        let data_off = r32(0xC)? as usize;
        let sfat = header_len;
        if slice(d, sfat, 4)? != b"SFAT" {
            bail!("SARC is missing its SFAT section");
        }
        let count = r16(sfat + 6)? as usize;
        let nodes_off = sfat + r16(sfat + 4)? as usize;
        let sfnt = nodes_off + count * 16;
        if slice(d, sfnt, 4)? != b"SFNT" {
            bail!("SARC is missing its SFNT section");
        }
        let names = sfnt + r16(sfnt + 4)? as usize;
        if data_off > d.len() || data_off < names {
            bail!("SARC data offset 0x{data_off:x} is invalid");
        }
        let mut files = Vec::with_capacity(count);
        for k in 0..count {
            let n = nodes_off + k * 16;
            let attr = r32(n + 4)?;
            let start = r32(n + 8)? as usize;
            let end = r32(n + 12)? as usize;
            if end < start {
                bail!("SARC node {k} has end before start");
            }
            let name = if attr & 0x0100_0000 != 0 {
                let o = names + (attr & 0xFFFF) as usize * 4;
                crate::bytes::cstr(d, o)?
            } else {
                format!("unnamed_{k:04}")
            };
            if !safe_relative_name(&name) {
                bail!("SARC entry name {name:?} is not a safe relative path");
            }
            let data = slice(d, data_off + start, end - start)?.to_vec();
            let alignment = natural_alignment(data_off + start, MAX_ALIGN).max(4);
            files.push(SarcFile { name, data, alignment });
        }
        Ok(Sarc { big_endian, prefix: d[..data_off].to_vec(), nodes_off, files })
    }

    pub fn file(&self, name: &str) -> Option<&SarcFile> {
        self.files.iter().find(|f| f.name == name)
    }

    /// Serialise the archive. File data alignment is never reduced.
    pub fn build(&self) -> Vec<u8> {
        let mut out = self.prefix.clone();
        let data_off = out.len();
        let mut ranges = Vec::with_capacity(self.files.len());
        for f in &self.files {
            let start = align_up(out.len(), f.alignment);
            out.resize(start, 0);
            out.extend_from_slice(&f.data);
            ranges.push((start - data_off, out.len() - data_off));
        }
        let w32 = |out: &mut Vec<u8>, o: usize, v: u32| {
            let b = if self.big_endian { v.to_be_bytes() } else { v.to_le_bytes() };
            out[o..o + 4].copy_from_slice(&b);
        };
        let total = out.len() as u32;
        w32(&mut out, 8, total);
        for (k, (s, e)) in ranges.into_iter().enumerate() {
            let n = self.nodes_off + k * 16;
            w32(&mut out, n + 8, s as u32);
            w32(&mut out, n + 12, e as u32);
        }
        out
    }
}

/// Reject absolute paths, drive letters and `..` components so extracted
/// names can never escape the output directory.
pub fn safe_relative_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.starts_with('\\')
        && !name.contains(':')
        && name.split(['/', '\\']).all(|c| !c.is_empty() && c != "." && c != "..")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal big-endian SARC writer for tests.
    pub fn make_sarc(entries: &[(&str, &[u8], usize)]) -> Vec<u8> {
        let mut names = Vec::new();
        let mut name_offs = Vec::new();
        for (n, _, _) in entries {
            name_offs.push(names.len() / 4);
            names.extend_from_slice(n.as_bytes());
            names.push(0);
            while names.len() % 4 != 0 {
                names.push(0);
            }
        }
        let nodes_len = entries.len() * 16;
        let header_end = 0x14 + 0xC + nodes_len + 8 + names.len();
        let data_off = align_up(header_end, 0x100);
        let mut d = vec![0u8; data_off];
        d[..4].copy_from_slice(b"SARC");
        d[4..6].copy_from_slice(&0x14u16.to_be_bytes());
        d[6..8].copy_from_slice(&[0xFE, 0xFF]);
        d[0xC..0x10].copy_from_slice(&(data_off as u32).to_be_bytes());
        d[0x10..0x12].copy_from_slice(&0x100u16.to_be_bytes());
        d[0x14..0x18].copy_from_slice(b"SFAT");
        d[0x18..0x1A].copy_from_slice(&0xCu16.to_be_bytes());
        d[0x1A..0x1C].copy_from_slice(&(entries.len() as u16).to_be_bytes());
        d[0x1C..0x20].copy_from_slice(&0x65u32.to_be_bytes());
        let sfnt = 0x20 + nodes_len;
        d[sfnt..sfnt + 4].copy_from_slice(b"SFNT");
        d[sfnt + 4..sfnt + 6].copy_from_slice(&8u16.to_be_bytes());
        d[sfnt + 8..sfnt + 8 + names.len()].copy_from_slice(&names);
        for (k, (_, data, align)) in entries.iter().enumerate() {
            let start = align_up(d.len(), *align);
            d.resize(start, 0);
            d.extend_from_slice(data);
            let n = 0x20 + k * 16;
            let attr = 0x0100_0000u32 | name_offs[k] as u32;
            d[n + 4..n + 8].copy_from_slice(&attr.to_be_bytes());
            d[n + 8..n + 12].copy_from_slice(&((start - data_off) as u32).to_be_bytes());
            let end = (d.len() - data_off) as u32;
            d[n + 12..n + 16].copy_from_slice(&end.to_be_bytes());
        }
        let len = d.len() as u32;
        d[8..12].copy_from_slice(&len.to_be_bytes());
        d
    }

    #[test]
    fn parse_and_rebuild_identity() {
        let a = make_sarc(&[("a/model.bfres", &[1u8; 300], 0x2000), ("b.txt", b"hello", 4)]);
        let s = Sarc::parse(&a).unwrap();
        assert_eq!(s.files.len(), 2);
        assert_eq!(s.files[0].name, "a/model.bfres");
        assert_eq!(s.build(), a);
    }

    #[test]
    fn rebuild_with_bigger_file_keeps_alignment() {
        let a = make_sarc(&[("x.bin", &[1u8; 10], 0x100), ("y.bfres", &[2u8; 10], 0x2000)]);
        let mut s = Sarc::parse(&a).unwrap();
        s.files[0].data = vec![9u8; 0x3000];
        let b = s.build();
        let s2 = Sarc::parse(&b).unwrap();
        assert_eq!(s2.files[0].data, vec![9u8; 0x3000]);
        assert_eq!(s2.files[1].data, vec![2u8; 10]);
        assert!(s2.files[1].alignment >= 0x2000);
    }

    #[test]
    fn rejects_traversal_and_garbage() {
        assert!(!safe_relative_name("../x"));
        assert!(!safe_relative_name("C:/x"));
        assert!(!safe_relative_name("/x"));
        assert!(safe_relative_name("timg/a^o.bflim"));
        let a = make_sarc(&[("../evil", b"x", 4)]);
        assert!(Sarc::parse(&a).is_err());
        let mut bad = make_sarc(&[("ok", b"x", 4)]);
        bad[0x2C..0x30].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        assert!(Sarc::parse(&bad).is_err());
    }
}
