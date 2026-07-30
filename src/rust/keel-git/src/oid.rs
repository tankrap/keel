//! git object id — a 20-byte SHA-1. (SHA-256 repositories are a later addition; the format is
//! otherwise identical with a 32-byte id.)

use crate::{GitError, Result};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid([u8; 20]);

impl Oid {
    pub fn from_bytes(b: [u8; 20]) -> Oid {
        Oid(b)
    }
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(40);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }
    /// Parse 40 hex bytes (ASCII) into an oid.
    pub fn from_hex(hex: &[u8]) -> Result<Oid> {
        if hex.len() != 40 {
            return Err(GitError::BadHex);
        }
        let mut out = [0u8; 20];
        for (i, chunk) in hex.chunks_exact(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16).ok_or(GitError::BadHex)?;
            let lo = (chunk[1] as char).to_digit(16).ok_or(GitError::BadHex)?;
            out[i] = ((hi << 4) | lo) as u8;
        }
        Ok(Oid(out))
    }
    pub fn from_hex_str(s: &str) -> Result<Oid> {
        Oid::from_hex(s.as_bytes())
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let s = "bd9dbf5aae1a3862dd1526723246b20206e5fc37";
        let oid = Oid::from_hex_str(s).unwrap();
        assert_eq!(oid.to_hex(), s);
    }
    #[test]
    fn rejects_bad_hex() {
        assert!(Oid::from_hex_str("zz").is_err());
        assert!(Oid::from_hex_str("abcd").is_err()); // wrong length
    }
}
