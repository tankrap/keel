//! Packfile *writer* (v2), undeltified. A pack is `PACK` + version(2) + object-count, then each
//! object as a (type,size) varint header followed by its zlib-compressed payload, then a 20-byte
//! SHA-1 trailer over everything before it.
//!
//! We emit every object in full (no OFS/REF deltas). git accepts that — a delta is purely a size
//! optimization — so `git clone`/`fetch` from keel work immediately; delta-encoding the pack is a
//! later size win (M5+). keel already holds every object individually in its mirror, so building
//! a pack is just "frame these objects".

use crate::{hash, Kind, Oid};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io;

/// git pack object type numbers (non-delta): commit=1, tree=2, blob=3, tag=4.
fn type_num(k: Kind) -> u8 {
    match k {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    }
}

/// Assemble a v2 packfile from `objects` (each `(type, full payload)`), returning the pack bytes
/// including the trailing SHA-1.
pub fn write(objects: &[(Kind, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for (kind, payload) in objects {
        write_obj_header(&mut out, type_num(*kind), payload.len());
        out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec_zlib(payload, 6));
    }
    let digest = Sha1::digest(&out);
    out.extend_from_slice(&digest);
    out
}

/// The variable-length object header: first byte carries `type` in bits 6-4 and the low 4 bits of
/// the (uncompressed) size in bits 3-0; each continuation byte carries 7 more size bits. Bit 7 is
/// the "more bytes follow" flag. Size is little-endian across the bytes.
fn write_obj_header(out: &mut Vec<u8>, ty: u8, size: usize) {
    let mut size = size;
    let mut byte = (ty << 4) | ((size & 0x0f) as u8);
    size >>= 4;
    loop {
        if size > 0 {
            out.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        } else {
            out.push(byte);
            break;
        }
    }
}

// ── packfile reader (parse + resolve deltas) ─────────────────────────────────────────────────
//
// This is what lets keel RECEIVE a push: a client sends a (possibly delta-compressed, possibly
// thin) packfile; we parse every object, resolve OFS/REF deltas, and hand back full
// `(kind, payload)` objects to store in the mirror. Base objects for REF deltas may live outside
// the pack (a thin pack references objects the server already has) — `base_by_oid` supplies those.

const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// Parse and fully resolve every object in a packfile. `base_by_oid` resolves REF-delta bases the
/// pack doesn't contain (thin-pack bases already in the store); it may return `None` if absent.
pub fn read(
    pack: &[u8],
    base_by_oid: &impl Fn(&Oid) -> Option<(Kind, Vec<u8>)>,
) -> io::Result<Vec<(Kind, Vec<u8>)>> {
    if pack.len() < 12 || &pack[0..4] != b"PACK" {
        return Err(bad("not a packfile"));
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    let mut pos = 12;
    let mut by_offset: HashMap<usize, (Kind, Vec<u8>)> = HashMap::new();
    let mut by_oid: HashMap<[u8; 20], (Kind, Vec<u8>)> = HashMap::new();
    let mut out = Vec::with_capacity(count);

    for _ in 0..count {
        let obj_offset = pos;
        let (ty, size, hdr) = parse_obj_header(&pack[pos..]).ok_or_else(|| bad("bad object header"))?;
        pos += hdr;
        let resolved = match ty {
            1..=4 => {
                let (payload, used) = inflate(&pack[pos..], size)?;
                pos += used;
                (kind_from_num(ty).ok_or_else(|| bad("bad type"))?, payload)
            }
            OBJ_OFS_DELTA => {
                let (back, n) = parse_ofs(&pack[pos..]).ok_or_else(|| bad("bad ofs-delta offset"))?;
                pos += n;
                let (delta, used) = inflate(&pack[pos..], size)?;
                pos += used;
                let base_off = obj_offset.checked_sub(back).ok_or_else(|| bad("ofs-delta underflow"))?;
                let (bkind, bbytes) = by_offset.get(&base_off).cloned().ok_or_else(|| bad("ofs-delta base missing"))?;
                (bkind, apply_delta(&bbytes, &delta)?)
            }
            OBJ_REF_DELTA => {
                let base_oid = Oid::from_bytes(pack.get(pos..pos + 20).ok_or_else(|| bad("ref-delta oid"))?.try_into().unwrap());
                pos += 20;
                let (delta, used) = inflate(&pack[pos..], size)?;
                pos += used;
                let (bkind, bbytes) = by_oid
                    .get(base_oid.as_bytes())
                    .cloned()
                    .or_else(|| base_by_oid(&base_oid))
                    .ok_or_else(|| bad("ref-delta base missing"))?;
                (bkind, apply_delta(&bbytes, &delta)?)
            }
            _ => return Err(bad("unknown pack object type")),
        };
        by_offset.insert(obj_offset, resolved.clone());
        by_oid.insert(*hash(resolved.0, &resolved.1).as_bytes(), resolved.clone());
        out.push(resolved);
    }
    Ok(out)
}

fn kind_from_num(n: u8) -> Option<Kind> {
    match n {
        1 => Some(Kind::Commit),
        2 => Some(Kind::Tree),
        3 => Some(Kind::Blob),
        4 => Some(Kind::Tag),
        _ => None,
    }
}

/// Parse a pack object header: type in bits 6-4 of the first byte, size as a little-endian varint
/// (low 4 bits in the first byte, 7 bits per continuation). Returns `(type, size, bytes_read)`.
fn parse_obj_header(buf: &[u8]) -> Option<(u8, usize, usize)> {
    let b0 = *buf.first()?;
    let ty = (b0 >> 4) & 0x07;
    let mut size = (b0 & 0x0f) as usize;
    let mut shift = 4;
    let mut i = 1;
    let mut b = b0;
    while b & 0x80 != 0 {
        b = *buf.get(i)?;
        size |= ((b & 0x7f) as usize) << shift;
        shift += 7;
        i += 1;
    }
    Some((ty, size, i))
}

/// Parse an OFS-delta negative offset (git's own varint): `(distance_back, bytes_read)`.
fn parse_ofs(buf: &[u8]) -> Option<(usize, usize)> {
    let mut c = *buf.first()?;
    let mut off = (c & 0x7f) as usize;
    let mut i = 1;
    while c & 0x80 != 0 {
        c = *buf.get(i)?;
        i += 1;
        off = ((off + 1) << 7) | (c & 0x7f) as usize;
    }
    Some((off, i))
}

/// Inflate a zlib stream whose *uncompressed* length is known (`expected`). Returns the bytes and
/// how many *input* bytes the stream consumed (so the caller can advance to the next object).
fn inflate(input: &[u8], expected: usize) -> io::Result<(Vec<u8>, usize)> {
    use miniz_oxide::inflate::core::inflate_flags::{
        TINFL_FLAG_PARSE_ZLIB_HEADER, TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    };
    use miniz_oxide::inflate::core::{decompress, DecompressorOxide};
    use miniz_oxide::inflate::TINFLStatus;

    let mut out = vec![0u8; expected];
    let mut d = DecompressorOxide::new();
    let flags = TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    let (status, in_used, out_used) = decompress(&mut d, input, &mut out, 0, flags);
    if status != TINFLStatus::Done || out_used != expected {
        return Err(bad("pack object inflate failed"));
    }
    Ok((out, in_used))
}

/// Apply a git delta (`base_size` varint, `target_size` varint, then COPY/INSERT ops) to `base`.
fn apply_delta(base: &[u8], delta: &[u8]) -> io::Result<Vec<u8>> {
    let mut i = 0;
    let _base_size = read_delta_varint(delta, &mut i).ok_or_else(|| bad("delta base size"))?;
    let target_size = read_delta_varint(delta, &mut i).ok_or_else(|| bad("delta target size"))?;
    let mut out = Vec::with_capacity(target_size);
    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            // COPY: the low bits select which offset/size bytes are present (little-endian).
            let mut off = 0usize;
            let mut sz = 0usize;
            for (bit, shift) in [(0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)] {
                if op & bit != 0 {
                    off |= (*delta.get(i).ok_or_else(|| bad("delta copy off"))? as usize) << shift;
                    i += 1;
                }
            }
            for (bit, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                if op & bit != 0 {
                    sz |= (*delta.get(i).ok_or_else(|| bad("delta copy size"))? as usize) << shift;
                    i += 1;
                }
            }
            if sz == 0 {
                sz = 0x10000;
            }
            let end = off.checked_add(sz).filter(|&e| e <= base.len()).ok_or_else(|| bad("delta copy range"))?;
            out.extend_from_slice(&base[off..end]);
        } else if op != 0 {
            // INSERT: `op` literal bytes follow.
            let n = op as usize;
            let end = i.checked_add(n).filter(|&e| e <= delta.len()).ok_or_else(|| bad("delta insert range"))?;
            out.extend_from_slice(&delta[i..end]);
            i = end;
        } else {
            return Err(bad("delta op 0 is reserved"));
        }
    }
    if out.len() != target_size {
        return Err(bad("delta produced wrong length"));
    }
    Ok(out)
}

/// git delta size varint: 7 bits per byte, LSB first, high bit = continue.
fn read_delta_varint(buf: &[u8], i: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    let mut shift = 0;
    loop {
        let b = *buf.get(*i)?;
        *i += 1;
        v |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::other(format!("pack: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_base(_: &Oid) -> Option<(Kind, Vec<u8>)> {
        None
    }

    #[test]
    fn write_then_read_roundtrips() {
        let objs = vec![
            (Kind::Blob, b"hello world\n".to_vec()),
            (Kind::Blob, b"another blob with more bytes in it".to_vec()),
            (Kind::Commit, b"tree 0000000000000000000000000000000000000000\n\nmsg\n".to_vec()),
        ];
        let pack = write(&objs);
        let got = read(&pack, &no_base).unwrap();
        assert_eq!(got, objs, "pack write->read must round-trip every object");
    }

    #[test]
    fn header_encodes_type_and_size() {
        // blob (3), size 12 → fits in the low nibble: one byte 0b0011_1100 = 0x3c
        let mut out = Vec::new();
        write_obj_header(&mut out, 3, 12);
        assert_eq!(out, vec![0x3c]);
        // size 200 → low nibble 8 (200 & 0xf), continuation carries 200>>4 = 12
        let mut out2 = Vec::new();
        write_obj_header(&mut out2, 3, 200);
        assert_eq!(out2, vec![0x38 | 0x80, 12]);
    }

    #[test]
    fn pack_has_signature_and_count() {
        let pack = write(&[(Kind::Blob, b"hello\n".to_vec())]);
        assert_eq!(&pack[0..4], b"PACK");
        assert_eq!(&pack[4..8], &2u32.to_be_bytes());
        assert_eq!(&pack[8..12], &1u32.to_be_bytes());
        assert_eq!(pack.len() >= 12 + 20, true); // header + at least the trailer
    }
}
