//! Content-defined chunking (FastCDC) for large-blob dedup.
//!
//! keel-native: there is no cross-implementation byte-parity constraint (the store is
//! all-Rust), so boundaries only need to be deterministic across keel peers — which they
//! are, being the same code everywhere. Chunk *addresses* are BLAKE3 of the chunk bytes
//! (see `store.rs`); this module only decides where the cuts fall.

pub const MIN: usize = 2048;
pub const AVG: usize = 8192;
pub const MAX: usize = 65536;
const MASK_S: u32 = 0x0003_5903;
const MASK_L: u32 = 0x0000_0d90;

/// Deterministic 256-entry gear table (splitmix32, seed 0x9e3779b9).
fn gear() -> [u32; 256] {
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
fn cutpoint(buf: &[u8], g: &[u32; 256]) -> usize {
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

/// Split `buf` into content-defined chunk ranges `[start, end)`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_reassemble_and_are_bounded() {
        // deterministic pseudo-random data
        let mut buf = vec![0u8; 2 * 1024 * 1024];
        let mut s: u32 = 1;
        for b in buf.iter_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (s >> 24) as u8;
        }
        let ranges = chunk_ranges(&buf);
        assert!(ranges.len() > 20, "large buffer splits into many chunks");
        // exact reassembly
        let reassembled: Vec<u8> =
            ranges.iter().flat_map(|(a, b)| buf[*a..*b].iter().copied()).collect();
        assert_eq!(reassembled, buf);
        // bounded chunk sizes (last chunk may be short)
        for (a, b) in &ranges[..ranges.len() - 1] {
            assert!(b - a <= MAX && b - a >= 1);
        }
    }

    #[test]
    fn short_input_is_one_chunk() {
        assert_eq!(chunk_ranges(b"hello"), vec![(0, 5)]);
        assert_eq!(chunk_ranges(&[]), vec![]);
    }
}
