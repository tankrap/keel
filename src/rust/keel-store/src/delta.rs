//! Copy/insert byte-delta codec (an LZ-against-a-dictionary, where the dictionary is a base
//! blob). A `target` blob is expressed as a sequence of COPY(base region) and INSERT(literal)
//! ops. Unlike a prefix/suffix delta it finds shared regions *anywhere* in the base — an
//! interior line insertion still keeps everything after it as a COPY — which is what lets a
//! blob delta usefully against a merely *similar* base, not just its same-path predecessor.
//!
//! This sits strictly below the content-address layer: the store re-verifies BLAKE3 after an
//! `expand`, so a wrong delta surfaces as corruption rather than wrong bytes (see `store`).
//!
//! Ops (a flat byte stream, itself DEFLATE-packed by the caller):
//!   COPY   = 0x00 ++ uvarint(base_offset) ++ uvarint(len)
//!   INSERT = 0x01 ++ uvarint(len) ++ len literal bytes
//! Blobs here are bounded by the chunk threshold (64 KiB), so the O(n) index + scan is cheap
//! and reconstruct is a handful of memcpies even through a full delta chain.

use std::collections::HashMap;

/// Minimum match length (also the rolling-hash window). Below this, a COPY's overhead (tag +
/// two varints) isn't worth it, so short coincidental matches stay literal.
const WINDOW: usize = 16;
/// Cap on candidate offsets kept per hash bucket — bounds worst-case work on highly repetitive
/// input (e.g. long runs of identical bytes) without materially hurting match quality.
const MAX_CANDIDATES: usize = 32;

const OP_COPY: u8 = 0;
const OP_INSERT: u8 = 1;

/// Rolling polynomial hash over a `WINDOW`-byte window (wrapping u64). Collisions are harmless:
/// every candidate is byte-verified before use, so a collision only costs a failed extend.
struct Roller {
    hash: u64,
    top: u64, // BASE^(WINDOW-1)
}
const BASE: u64 = 1_000_000_007;

impl Roller {
    fn new(win: &[u8]) -> Roller {
        let mut hash = 0u64;
        for &b in win {
            hash = hash.wrapping_mul(BASE).wrapping_add(b as u64);
        }
        let mut top = 1u64;
        for _ in 0..WINDOW - 1 {
            top = top.wrapping_mul(BASE);
        }
        Roller { hash, top }
    }
    /// Slide the window one byte right: drop `out`, append `inp`.
    fn roll(&mut self, out: u8, inp: u8) {
        self.hash = self
            .hash
            .wrapping_sub((out as u64).wrapping_mul(self.top))
            .wrapping_mul(BASE)
            .wrapping_add(inp as u64);
    }
}

/// Encode `target` as an op stream against `base`. Always valid input to [`expand`]; returns an
/// all-literal stream when `base` shares nothing (the caller decides if it's worth keeping).
pub fn compress(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
    if base.len() >= WINDOW {
        let mut r = Roller::new(&base[..WINDOW]);
        for off in 0..=base.len() - WINDOW {
            let e = index.entry(r.hash).or_default();
            if e.len() < MAX_CANDIDATES {
                e.push(off);
            }
            if off + WINDOW < base.len() {
                r.roll(base[off], base[off + WINDOW]);
            }
        }
    }

    let mut ops = Vec::new();
    let mut i = 0usize;
    let mut lit_start = 0usize;
    let mut roller = (target.len() >= WINDOW).then(|| Roller::new(&target[..WINDOW]));

    while i < target.len() {
        let mut best_off = 0usize;
        let mut best_len = 0usize;
        if let Some(r) = &roller {
            if let Some(cands) = index.get(&r.hash) {
                for &off in cands {
                    // verify (guards hash collisions) then extend the match as far as it goes
                    let mut l = 0usize;
                    while off + l < base.len()
                        && i + l < target.len()
                        && base[off + l] == target[i + l]
                    {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_off = off;
                    }
                }
            }
        }

        if best_len >= WINDOW {
            if lit_start < i {
                emit_insert(&mut ops, &target[lit_start..i]);
            }
            emit_copy(&mut ops, best_off, best_len);
            // advance i by the match, rolling the window along the way
            let target_new = i + best_len;
            advance(&mut roller, target, i, target_new);
            i = target_new;
            lit_start = i;
        } else {
            advance(&mut roller, target, i, i + 1);
            i += 1;
        }
    }
    if lit_start < target.len() {
        emit_insert(&mut ops, &target[lit_start..]);
    }
    ops
}

/// Roll `roller` forward from position `from` to `to` (keeping its window aligned to the current
/// scan position). No-op once fewer than `WINDOW` bytes remain — matches can't start there.
fn advance(roller: &mut Option<Roller>, target: &[u8], from: usize, to: usize) {
    let Some(r) = roller else { return };
    for p in from..to {
        if p + WINDOW < target.len() {
            r.roll(target[p], target[p + WINDOW]);
        } else {
            *roller = None; // window ran off the end
            return;
        }
    }
}

/// A MinHash sketch: the `k` smallest distinct window-hashes of `bytes`. Two blobs that share
/// much content share many of their smallest window-hashes, so overlap in these sketches
/// estimates similarity (Jaccard) cheaply — used by `repack` to pick a cross-path delta base
/// without an O(n²) all-pairs comparison. Empty for inputs shorter than one window.
pub fn sketch(bytes: &[u8], k: usize) -> Vec<u64> {
    if bytes.len() < WINDOW || k == 0 {
        return Vec::new();
    }
    let mut r = Roller::new(&bytes[..WINDOW]);
    let mut hashes = Vec::with_capacity(bytes.len() - WINDOW + 1);
    for off in 0..=bytes.len() - WINDOW {
        hashes.push(r.hash);
        if off + WINDOW < bytes.len() {
            r.roll(bytes[off], bytes[off + WINDOW]);
        }
    }
    hashes.sort_unstable();
    hashes.dedup();
    hashes.truncate(k);
    hashes
}

/// Reconstruct the target from `base` and an op stream produced by [`compress`]. Returns `None`
/// on any malformed op or out-of-range COPY (surfaced as store corruption by the caller).
pub fn expand(base: &[u8], ops: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < ops.len() {
        let tag = ops[i];
        i += 1;
        match tag {
            OP_COPY => {
                let off = read_uvarint(ops, &mut i)? as usize;
                let len = read_uvarint(ops, &mut i)? as usize;
                let end = off.checked_add(len)?;
                out.extend_from_slice(base.get(off..end)?);
            }
            OP_INSERT => {
                let len = read_uvarint(ops, &mut i)? as usize;
                let end = i.checked_add(len)?;
                out.extend_from_slice(ops.get(i..end)?);
                i = end;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn emit_copy(ops: &mut Vec<u8>, off: usize, len: usize) {
    ops.push(OP_COPY);
    put_uvarint(ops, off as u64);
    put_uvarint(ops, len as u64);
}

fn emit_insert(ops: &mut Vec<u8>, bytes: &[u8]) {
    ops.push(OP_INSERT);
    put_uvarint(ops, bytes.len() as u64);
    ops.extend_from_slice(bytes);
}

fn put_uvarint(o: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        o.push(b);
        if v == 0 {
            return;
        }
    }
}

fn read_uvarint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*i)?;
        *i += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(base: &[u8], target: &[u8]) -> Vec<u8> {
        let ops = compress(base, target);
        expand(base, &ops).expect("valid ops")
    }

    #[test]
    fn identical_is_one_big_copy() {
        let b: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        assert_eq!(roundtrip(&b, &b), b);
        // an identical target should compress to essentially a single COPY, far under its size
        assert!(compress(&b, &b).len() < b.len() / 10);
    }

    #[test]
    fn interior_edit_keeps_tail_as_copy() {
        // prefix/suffix deltas give up everything after an interior insertion; copy/insert
        // must still COPY the long unchanged tail.
        let base: Vec<u8> = (0..4000).map(|i| (i % 97) as u8).collect();
        let mut target = base.clone();
        target.splice(2000..2000, b"XXXXXXXXXXXXXXXXXXXX".iter().copied()); // insert in the middle
        assert_eq!(roundtrip(&base, &target), target);
        assert!(compress(&base, &target).len() < target.len() / 4, "interior edit should compress well");
    }

    #[test]
    fn shared_middle_across_reorder() {
        // a big shared region that has moved — findable anywhere, not just at the ends
        let chunk: Vec<u8> = (0..1000).map(|i| (i % 253) as u8).collect();
        let base = [b"HEADER-A".as_slice(), &chunk, b"TAIL-A"].concat();
        let target = [b"DIFFERENT-HEADER".as_slice(), &chunk, b"OTHER-TAIL"].concat();
        assert_eq!(roundtrip(&base, &target), target);
        assert!(compress(&base, &target).len() < target.len() / 2);
    }

    #[test]
    fn empty_and_unrelated_are_lossless() {
        assert_eq!(roundtrip(b"", b""), b"");
        assert_eq!(roundtrip(b"", b"hello"), b"hello");
        assert_eq!(roundtrip(b"abcdef", b""), b"");
        assert_eq!(roundtrip(b"completely", b"unrelated-xyz-9999"), b"unrelated-xyz-9999");
    }

    #[test]
    fn malformed_ops_rejected() {
        assert_eq!(expand(b"base", &[OP_COPY, 200, 200]), None); // COPY out of range
        assert_eq!(expand(b"base", &[9]), None); // unknown tag
    }
}
