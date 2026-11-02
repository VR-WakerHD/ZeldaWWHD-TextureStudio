//! Yaz0 (SZS) compression as used by Wind Waker HD archives.

use crate::bytes::{be32, slice};
use crate::error::{bail, Result};

/// Largest decompressed size accepted, to avoid uncontrolled allocations from a
/// corrupt header. The biggest WWHD archive is far below this.
pub const MAX_DECOMPRESSED: usize = 512 * 1024 * 1024;

pub fn is_yaz0(d: &[u8]) -> bool {
    d.len() >= 16 && &d[..4] == b"Yaz0"
}

/// The 8 header bytes after the size field (alignment hint and reserved), kept
/// so recompressed archives carry the same header as the original.
pub fn header_extra(d: &[u8]) -> [u8; 8] {
    let mut h = [0u8; 8];
    if d.len() >= 16 {
        h.copy_from_slice(&d[8..16]);
    }
    h
}

pub fn decompress(d: &[u8]) -> Result<Vec<u8>> {
    if !is_yaz0(d) {
        bail!("not a Yaz0 stream");
    }
    let size = be32(d, 4)? as usize;
    if size > MAX_DECOMPRESSED {
        bail!("Yaz0 size {size} exceeds limit");
    }
    let mut out = Vec::with_capacity(size);
    let mut i = 16usize;
    while out.len() < size {
        let Some(&code) = d.get(i) else { bail!("Yaz0 stream truncated") };
        i += 1;
        for bit in 0..8 {
            if out.len() >= size {
                break;
            }
            if code & (0x80 >> bit) != 0 {
                let Some(&b) = d.get(i) else { bail!("Yaz0 stream truncated") };
                out.push(b);
                i += 1;
            } else {
                let p = slice(d, i, 2)?;
                let (b1, b2) = (p[0] as usize, p[1] as usize);
                i += 2;
                let dist = ((b1 & 0xF) << 8 | b2) + 1;
                let mut n = b1 >> 4;
                if n == 0 {
                    let Some(&b3) = d.get(i) else { bail!("Yaz0 stream truncated") };
                    n = b3 as usize + 0x12;
                    i += 1;
                } else {
                    n += 2;
                }
                if dist > out.len() {
                    bail!("Yaz0 back-reference before start of output");
                }
                let n = n.min(size - out.len());
                let start = out.len() - dist;
                for k in 0..n {
                    let b = out[start + k];
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}

const WINDOW: usize = 0x1000;
const MAX_LEN: usize = 0x111;
const MIN_LEN: usize = 3;
const HASH_BITS: u32 = 15;
const CHAIN_LIMIT: usize = 96;

/// Compress with a hash-chained LZ search. Output decompresses to exactly `src`.
pub fn compress(src: &[u8], extra: [u8; 8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 2 + 64);
    out.extend_from_slice(b"Yaz0");
    out.extend_from_slice(&(src.len() as u32).to_be_bytes());
    out.extend_from_slice(&extra);

    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; WINDOW];
    let hash = |p: usize| -> usize {
        let v = (src[p] as u32) << 16 | (src[p + 1] as u32) << 8 | src[p + 2] as u32;
        (v.wrapping_mul(2654435761) >> (32 - HASH_BITS)) as usize
    };
    let insert = |p: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if p + MIN_LEN <= src.len() {
            let h = hash(p);
            prev[p % WINDOW] = head[h];
            head[h] = p;
        }
    };

    let mut pos = 0usize;
    while pos < src.len() {
        let code_at = out.len();
        out.push(0);
        let mut code = 0u8;
        for bit in 0..8 {
            if pos >= src.len() {
                break;
            }
            let (mut best_len, mut best_dist) = (0usize, 0usize);
            if pos + MIN_LEN <= src.len() {
                let max = MAX_LEN.min(src.len() - pos);
                let mut cand = head[hash(pos)];
                let mut steps = 0;
                while cand != usize::MAX && pos - cand <= WINDOW && steps < CHAIN_LIMIT {
                    let mut l = 0;
                    while l < max && src[cand + l] == src[pos + l] {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_dist = pos - cand;
                        if l == max {
                            break;
                        }
                    }
                    let next = prev[cand % WINDOW];
                    if next == usize::MAX || next >= cand {
                        break;
                    }
                    cand = next;
                    steps += 1;
                }
            }
            if best_len >= MIN_LEN {
                let d = best_dist - 1;
                if best_len >= 0x12 {
                    out.push((d >> 8) as u8);
                    out.push(d as u8);
                    out.push((best_len - 0x12) as u8);
                } else {
                    out.push((((best_len - 2) << 4) | (d >> 8)) as u8);
                    out.push(d as u8);
                }
                for p in pos..pos + best_len {
                    insert(p, &mut head, &mut prev);
                }
                pos += best_len;
            } else {
                code |= 0x80 >> bit;
                out.push(src[pos]);
                insert(pos, &mut head, &mut prev);
                pos += 1;
            }
        }
        out[code_at] = code;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut data = Vec::new();
        for i in 0..200_000u32 {
            data.push((i % 251) as u8 ^ (i / 977) as u8);
        }
        data.extend(std::iter::repeat(7u8).take(5000));
        let c = compress(&data, [0; 8]);
        assert!(c.len() < data.len());
        assert_eq!(decompress(&c).unwrap(), data);
    }

    #[test]
    fn rejects_truncated() {
        let c = compress(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 1, 2, 3, 4, 5, 6], [0; 8]);
        assert!(decompress(&c[..c.len() - 2]).is_err());
        assert!(decompress(b"Yaz0").is_err());
    }
}
