//! git loose objects: `zlib("<type> <len>\0" ++ payload)` stored at
//! `.git/objects/<oid[0:2]>/<oid[2:40]>`.
//!
//! Note: the object id is over the *inflated* wrapped bytes, so keel's zlib output need not be
//! byte-identical to git's — git re-inflates and re-hashes, and any valid zlib stream that
//! inflates to the same bytes yields the same object. (Packed objects live in packfiles and are
//! handled separately; see the pack module — a later milestone.)

use crate::{header, hash, Kind, Oid};
use std::io;
use std::path::{Path, PathBuf};

/// Inflate a loose object and split off its type + payload.
pub fn decode(bytes: &[u8]) -> io::Result<(Kind, Vec<u8>)> {
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(bytes)
        .map_err(|_| io::Error::other("loose object: zlib inflate failed"))?;
    let sp = raw.iter().position(|&b| b == b' ').ok_or_else(bad)?;
    let kind = Kind::parse(&raw[..sp]).ok_or_else(bad)?;
    let nul = raw.iter().position(|&b| b == 0).ok_or_else(bad)?;
    let len: usize = std::str::from_utf8(&raw[sp + 1..nul])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    let payload = raw[nul + 1..].to_vec();
    if payload.len() != len {
        return Err(io::Error::other("loose object: declared length mismatch"));
    }
    Ok((kind, payload))
}

/// zlib-compress a wrapped object for on-disk storage.
pub fn encode(kind: Kind, payload: &[u8]) -> Vec<u8> {
    let mut wrapped = header(kind, payload.len());
    wrapped.extend_from_slice(payload);
    miniz_oxide::deflate::compress_to_vec_zlib(&wrapped, 6)
}

/// `<git_dir>/objects/<xx>/<38 hex>` for an oid.
pub fn object_path(git_dir: &Path, oid: &Oid) -> PathBuf {
    let hex = oid.to_hex();
    git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

/// Read a loose object by id, or `None` if it isn't stored loose (may be packed).
pub fn read(git_dir: &Path, oid: &Oid) -> io::Result<Option<(Kind, Vec<u8>)>> {
    let path = object_path(git_dir, oid);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(decode(&bytes)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write a payload as a loose object (content-addressed; idempotent — a rewrite is a no-op).
/// Returns the object id git will see.
pub fn write(git_dir: &Path, kind: Kind, payload: &[u8]) -> io::Result<Oid> {
    let oid = hash(kind, payload);
    let path = object_path(git_dir, &oid);
    if path.exists() {
        return Ok(oid); // immutable + content-addressed → already correct
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // write to a temp then rename, so a reader never sees a half-written object
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, encode(kind, payload))?;
    std::fs::rename(&tmp, &path)?;
    Ok(oid)
}

fn bad() -> io::Error {
    io::Error::other("loose object: malformed header")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let payload = b"hello git object";
        let enc = encode(Kind::Blob, payload);
        let (kind, got) = decode(&enc).unwrap();
        assert_eq!(kind, Kind::Blob);
        assert_eq!(got, payload);
    }

    #[test]
    fn write_read_roundtrip_and_id() {
        let dir = std::env::temp_dir().join(format!("keelgit-loose-{}", std::process::id()));
        let git = dir.join(".git");
        let payload = b"what is up, doc?";
        let oid = write(&git, Kind::Blob, payload).unwrap();
        // matches git's well-known SHA-1 for this content
        assert_eq!(oid.to_hex(), "bd9dbf5aae1a3862dd1526723246b20206e5fc37");
        let (kind, got) = read(&git, &oid).unwrap().unwrap();
        assert_eq!((kind, got.as_slice()), (Kind::Blob, payload.as_slice()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
