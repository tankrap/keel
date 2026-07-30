//! Packfile *writer* (v2), undeltified. A pack is `PACK` + version(2) + object-count, then each
//! object as a (type,size) varint header followed by its zlib-compressed payload, then a 20-byte
//! SHA-1 trailer over everything before it.
//!
//! We emit every object in full (no OFS/REF deltas). git accepts that — a delta is purely a size
//! optimization — so `git clone`/`fetch` from keel work immediately; delta-encoding the pack is a
//! later size win (M5+). keel already holds every object individually in its mirror, so building
//! a pack is just "frame these objects".

use crate::Kind;
use sha1::{Digest, Sha1};

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

#[cfg(test)]
mod tests {
    use super::*;

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
