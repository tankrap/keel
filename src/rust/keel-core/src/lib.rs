//! keel-core — the byte-identical primitives shared by the CLI and the server.
//!
//! Content addressing and chunk boundaries MUST be computed identically on
//! every peer or dedup and signature verification break. This crate is the one
//! source of truth; it is differential-tested against the Node reference
//! (`keel-server/src/store.mjs`) to guarantee the two agree byte-for-byte.

use blake2::{Blake2b512, Digest};

/// BLAKE2b-256 content address as 64 hex chars (first 32 bytes of Blake2b512),
/// matching the Node store's `createHash("blake2b512").digest("hex").slice(0,64)`.
pub fn hash_hex(buf: &[u8]) -> String {
    let full = Blake2b512::digest(buf);
    let mut s = String::with_capacity(64);
    for b in &full[..32] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ── FastCDC ─────────────────────────────────────────────────────────────────
// Ported to match store.mjs exactly: same splitmix32 gear table from the same
// seed, same masks, same min/avg/max, same `fp = (fp << 1) + gear[b]` as u32.

pub const MIN: usize = 2048;
pub const AVG: usize = 8192;
pub const MAX: usize = 65536;
const MASK_S: u32 = 0x0003_5903;
const MASK_L: u32 = 0x0000_0d90;

/// Deterministic 256-entry gear table (splitmix32, seed 0x9e3779b9) — identical
/// to the Node generator so chunk boundaries are reproducible across peers.
pub fn gear() -> [u32; 256] {
    let mut g = [0u32; 256];
    let mut s: u32 = 0x9e37_79b9;
    for slot in g.iter_mut() {
        s = s.wrapping_add(0x9e37_79b9);
        let mut z = s;
        z = (z ^ (z >> 16)).wrapping_mul(0x21f0_aaad);
        z = (z ^ (z >> 15)).wrapping_mul(0x735a_2d97);
        *slot = z ^ (z >> 15);
    }
    g
}

/// Cut point (chunk length) for `buf` starting at index 0.
pub fn cutpoint(buf: &[u8], g: &[u32; 256]) -> usize {
    let n = buf.len();
    if n <= MIN {
        return n;
    }
    let end = n.min(MAX);
    let normal = AVG.min(end);
    let mut fp: u32 = 0;
    let mut i = MIN;
    while i < normal {
        fp = (fp << 1).wrapping_add(g[buf[i] as usize]);
        if fp & MASK_S == 0 {
            return i;
        }
        i += 1;
    }
    while i < end {
        fp = (fp << 1).wrapping_add(g[buf[i] as usize]);
        if fp & MASK_L == 0 {
            return i;
        }
        i += 1;
    }
    end
}

/// Split into content-defined chunks (as index ranges into `buf`).
pub fn chunk_ranges(buf: &[u8]) -> Vec<(usize, usize)> {
    let g = gear();
    let mut out = Vec::new();
    let mut off = 0;
    while off < buf.len() {
        let len = cutpoint(&buf[off..], &g);
        out.push((off, off + len));
        off += len;
    }
    out
}

// ── canonical JSON (for signatures) ──────────────────────────────────────────
/// Deep key-sorted JSON with no incidental whitespace, matching the Node
/// `canonical()` used for cert/manifest signing. Minimal serializer over the
/// value types keel actually signs (objects, arrays, strings, numbers, bools).
pub enum J {
    S(String),
    N(f64),
    B(bool),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

pub fn canonical(v: &J) -> String {
    match v {
        J::S(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        J::N(n) => {
            if n.fract() == 0.0 {
                format!("{}", *n as i64)
            } else {
                format!("{}", n)
            }
        }
        J::B(b) => b.to_string(),
        J::A(a) => {
            let items: Vec<String> = a.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        J::O(o) => {
            let mut keys: Vec<&(String, J)> = o.iter().collect();
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            let items: Vec<String> =
                keys.iter().map(|(k, val)| format!("\"{}\":{}", k, canonical(val))).collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_256bit_hex() {
        let h = hash_hex(b"hello");
        assert_eq!(h.len(), 64);
        // identical input -> identical address
        assert_eq!(h, hash_hex(b"hello"));
        assert_ne!(h, hash_hex(b"world"));
    }

    #[test]
    fn chunks_reassemble_and_are_bounded() {
        // deterministic pseudo-random (same LCG shape as the JS test helper)
        let mut buf = vec![0u8; 2 * 1024 * 1024];
        let mut s: u32 = 1;
        for b in buf.iter_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (s & 0xff) as u8;
        }
        let ranges = chunk_ranges(&buf);
        assert!(ranges.len() > 20);
        let reassembled: Vec<u8> =
            ranges.iter().flat_map(|(a, b)| buf[*a..*b].iter().copied()).collect();
        assert_eq!(reassembled, buf);
        for (a, b) in &ranges[..ranges.len() - 1] {
            assert!(b - a <= MAX);
        }
    }

    #[test]
    fn canonical_sorts_keys() {
        let v = J::O(vec![
            ("b".into(), J::N(2.0)),
            ("a".into(), J::S("x".into())),
        ]);
        assert_eq!(canonical(&v), "{\"a\":\"x\",\"b\":2}");
    }
}
