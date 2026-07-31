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

/// Upper bound on a single object's declared uncompressed size (and on a delta's target size).
/// Object headers and delta streams carry these as unbounded varints; a crafted value would make
/// us pre-allocate an enormous buffer and abort the process, so reject anything past this cap
/// before allocating. 4 GiB is well above any real source object while keeping allocations sane.
const MAX_OBJECT_SIZE: usize = 4 * 1024 * 1024 * 1024;

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

/// Assemble a **delta-compressed** v2 packfile. Objects are sorted by (type, size desc) and each
/// is encoded as a REF-delta against the best of a small window of earlier *full* objects of the
/// same type when that beats storing it whole — git's basic pack heuristic. Bases are always
/// emitted before their deltas (so a reader resolving by oid has them), and only full objects are
/// used as bases, so delta chains stay depth-1. git accepts this and repacks locally.
pub fn write_deltified(objects: &[(Kind, Vec<u8>)]) -> Vec<u8> {
    const WINDOW: usize = 16;
    let mut order: Vec<usize> = (0..objects.len()).collect();
    order.sort_by_key(|&i| (type_num(objects[i].0), std::cmp::Reverse(objects[i].1.len())));

    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());

    // sliding window of recent FULL objects, each with its delta-index built ONCE (reused across
    // every target it's probed against — that caching is what makes this tractable at repo scale).
    let mut window: std::collections::VecDeque<(Oid, Kind, usize, DeltaIndex)> = std::collections::VecDeque::new();
    for &idx in &order {
        let (kind, payload) = &objects[idx];
        let full_compressed = miniz_oxide::deflate::compress_to_vec_zlib(payload, 6);

        let mut best: Option<(Oid, Vec<u8>)> = None;
        for (boid, bkind, bidx, bindex) in window.iter().rev() {
            if bkind != kind {
                continue;
            }
            let delta = delta_with(&objects[*bidx].1, bindex, payload);
            if best.as_ref().is_none_or(|(_, d)| delta.len() < d.len()) {
                best = Some((*boid, delta));
            }
        }

        let use_delta = best
            .as_ref()
            .map(|(_, d)| miniz_oxide::deflate::compress_to_vec_zlib(d, 6).len() + 20 < full_compressed.len())
            .unwrap_or(false);
        if let (true, Some((boid, delta))) = (use_delta, best) {
            write_obj_header(&mut out, 7, delta.len()); // OBJ_REF_DELTA
            out.extend_from_slice(boid.as_bytes());
            out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec_zlib(&delta, 6));
        } else {
            write_obj_header(&mut out, type_num(*kind), payload.len());
            out.extend_from_slice(&full_compressed);
            window.push_back((hash(*kind, payload), *kind, idx, DeltaIndex::build(payload)));
            if window.len() > WINDOW {
                window.pop_front();
            }
        }
    }
    let digest = Sha1::digest(&out);
    out.extend_from_slice(&digest);
    out
}

/// Minimum match length / index window for the delta encoder.
const DELTA_W: usize = 16;

/// A reusable index of a base blob's windows (`window-hash → offsets`), so probing many targets
/// against the same base doesn't rebuild it each time.
struct DeltaIndex {
    map: HashMap<u64, Vec<usize>>,
}
impl DeltaIndex {
    fn build(base: &[u8]) -> DeltaIndex {
        let mut map: HashMap<u64, Vec<usize>> = HashMap::new();
        if base.len() >= DELTA_W {
            for i in 0..=base.len() - DELTA_W {
                let e = map.entry(hash_window(&base[i..i + DELTA_W])).or_default();
                if e.len() < 16 {
                    e.push(i);
                }
            }
        }
        DeltaIndex { map }
    }
}

/// Encode `target` as a git delta against `base` using a prebuilt index.
fn delta_with(base: &[u8], index: &DeltaIndex, target: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_size_varint(&mut out, base.len());
    write_size_varint(&mut out, target.len());
    let mut i = 0;
    let mut lit_start = 0;
    while i < target.len() {
        let mut best_len = 0;
        let mut best_off = 0;
        if i + DELTA_W <= target.len() {
            if let Some(cands) = index.map.get(&hash_window(&target[i..i + DELTA_W])) {
                for &off in cands {
                    let mut l = 0;
                    while off + l < base.len() && i + l < target.len() && base[off + l] == target[i + l] {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_off = off;
                    }
                }
            }
        }
        if best_len >= DELTA_W {
            emit_insert(&mut out, &target[lit_start..i]);
            emit_copy(&mut out, best_off, best_len);
            i += best_len;
            lit_start = i;
        } else {
            i += 1;
        }
    }
    emit_insert(&mut out, &target[lit_start..]);
    out
}

/// Encode `target` as a git delta against `base` (builds a fresh index; convenience for callers
/// that only delta once against a base).
pub fn encode_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    delta_with(base, &DeltaIndex::build(base), target)
}

fn emit_copy(out: &mut Vec<u8>, mut off: usize, mut size: usize) {
    while size > 0 {
        let chunk = size.min(0xff_ffff); // one COPY op carries up to 3 size bytes
        let mut op = 0x80u8;
        let mut bytes = Vec::new();
        for b in 0..4 {
            let v = (off >> (8 * b)) & 0xff;
            if v != 0 {
                op |= 1 << b;
                bytes.push(v as u8);
            }
        }
        for b in 0..3 {
            let v = (chunk >> (8 * b)) & 0xff;
            if v != 0 {
                op |= 0x10 << b;
                bytes.push(v as u8);
            }
        }
        out.push(op);
        out.extend_from_slice(&bytes);
        off += chunk;
        size -= chunk;
    }
}

fn emit_insert(out: &mut Vec<u8>, data: &[u8]) {
    for chunk in data.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
}

fn write_size_varint(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn hash_window(w: &[u8]) -> u64 {
    // FNV-1a over the window — good enough to bucket candidates (matches are byte-verified).
    let mut h = 0xcbf29ce484222325u64;
    for &b in w {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
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
    // The header's object count is attacker-controlled (up to 4.29B). Each object costs at least a
    // few bytes on the wire, so a legitimate `count` can't exceed the remaining pack length — cap
    // the capacity hint by it so a bogus count can't drive a huge up-front reservation.
    let mut out = Vec::with_capacity(count.min(pack.len()));

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
        // Bound the varint: past 64 bits the shift overflows (panic in debug, garbage in release).
        if shift >= usize::BITS as usize {
            return None;
        }
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
        // A valid ofs-delta offset fits in usize; reject an over-long varint rather than overflow.
        // `(off + 1) << 7 | low7` == `(off + 1) * 128 + low7` since the low 7 bits are clear.
        off = off.checked_add(1).and_then(|o| o.checked_mul(128))? | (c & 0x7f) as usize;
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

    // `expected` is an attacker-controlled object-size varint; `vec![0u8; expected]` commits pages
    // immediately, so a huge value aborts the process. Reject oversized objects before allocating.
    if expected > MAX_OBJECT_SIZE {
        return Err(bad("pack object exceeds size limit"));
    }
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
    // Bound the attacker-controlled target size before reserving (mirrors the `inflate` cap).
    if target_size > MAX_OBJECT_SIZE {
        return Err(bad("delta target exceeds size limit"));
    }
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
        if shift >= usize::BITS as usize {
            return None; // varint longer than usize — reject rather than overflow the shift
        }
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
    fn malicious_headers_error_instead_of_crashing() {
        // A tiny packfile whose object-size varint declares a ~2^60-byte object must be rejected
        // before allocating (regression for the unauthenticated receive-pack allocation abort).
        let mut p = b"PACK".to_vec();
        p.extend_from_slice(&2u32.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p.push(0x90); // continuation | type=commit(1) | low4 size=0
        p.extend_from_slice(&[0xff; 7]); // seven continuation bytes → a ~2^60 declared size
        p.push(0x7f);
        assert!(read(&p, &no_base).is_err(), "oversized object size must be a clean error");

        // A header claiming 0xFFFF_FFFF objects but carrying none must not pre-reserve or hang.
        let mut q = b"PACK".to_vec();
        q.extend_from_slice(&2u32.to_be_bytes());
        q.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        assert!(read(&q, &no_base).is_err(), "bogus object count must be a clean error");
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
    fn deltified_pack_roundtrips_and_shrinks() {
        // near-duplicate blobs → the deltified writer should encode later ones as deltas, and a
        // read must reconstruct every object exactly (order-independent: compare as a set).
        let base: Vec<u8> = (0..4000).map(|i| (i % 251) as u8).collect();
        let mut objs = Vec::new();
        for v in 0..8u8 {
            let mut b = base.clone();
            b[2000] = v; // one-byte change each → highly deltafiable
            objs.push((Kind::Blob, b));
        }
        let full = write(&objs);
        let delta = write_deltified(&objs);
        assert!(delta.len() < full.len(), "deltified pack should be smaller: {} vs {}", delta.len(), full.len());

        let got = read(&delta, &no_base).unwrap();
        use std::collections::HashSet;
        let want: HashSet<_> = objs.iter().map(|(k, p)| hash(*k, p).to_hex()).collect();
        let have: HashSet<_> = got.iter().map(|(k, p)| hash(*k, p).to_hex()).collect();
        assert_eq!(want, have, "every object must survive the delta round-trip");
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
        assert!(pack.len() >= 12 + 20); // header + at least the trailer
    }
}
