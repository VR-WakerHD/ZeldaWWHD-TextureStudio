//! Bounds-checked big-endian reads and writes. Every read of untrusted game
//! data goes through these helpers so malformed offsets become errors, not panics.

use crate::error::{bail, Result};

pub fn slice(d: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    match off.checked_add(len) {
        Some(end) if end <= d.len() => Ok(&d[off..end]),
        _ => bail!("range 0x{off:x}+0x{len:x} outside data of 0x{:x} bytes", d.len()),
    }
}

pub fn u8_at(d: &[u8], off: usize) -> Result<u8> {
    Ok(slice(d, off, 1)?[0])
}

pub fn be16(d: &[u8], off: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(slice(d, off, 2)?.try_into().unwrap()))
}

pub fn be32(d: &[u8], off: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(slice(d, off, 4)?.try_into().unwrap()))
}

pub fn put_be16(d: &mut [u8], off: usize, v: u16) {
    d[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

pub fn put_be32(d: &mut [u8], off: usize, v: u32) {
    d[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/// Resolve a self-relative 32-bit pointer (BFRES v3 style): the stored value is
/// the distance from the field's own address. Zero means null.
pub fn rel_ptr(d: &[u8], field: usize) -> Result<Option<usize>> {
    let v = be32(d, field)? as i32;
    if v == 0 {
        return Ok(None);
    }
    let target = field as i64 + v as i64;
    if target < 0 || target as usize >= d.len() {
        bail!("relative pointer at 0x{field:x} points outside the file (0x{target:x})");
    }
    Ok(Some(target as usize))
}

/// Store a self-relative pointer to `target` in `field`.
pub fn put_rel_ptr(d: &mut [u8], field: usize, target: usize) {
    put_be32(d, field, (target as i64 - field as i64) as i32 as u32);
}

/// Null-terminated string starting at `off`.
pub fn cstr(d: &[u8], off: usize) -> Result<String> {
    let rest = slice(d, off, d.len().saturating_sub(off))?;
    let Some(end) = rest.iter().position(|&b| b == 0) else {
        bail!("unterminated string at 0x{off:x}");
    };
    Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
}

pub fn align_up(v: usize, a: usize) -> usize {
    if a <= 1 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

/// Largest power of two dividing `v`, capped (0 counts as the cap).
pub fn natural_alignment(v: usize, cap: usize) -> usize {
    if v == 0 {
        return cap;
    }
    (1usize << v.trailing_zeros()).min(cap)
}
