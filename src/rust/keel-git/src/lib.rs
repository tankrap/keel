//! git object format — the keystone of keel's git compatibility.
//!
//! keel keeps its own BLAKE3 content-addressed core; this crate is the **edge adapter** that
//! reads and writes git's *native* object format so keel can mirror to a real `.git`, reproduce
//! byte-identical objects (same SHA-1s — git can verify keel altered nothing), and later serve
//! the git wire protocol.
//!
//! Design rule that everything above depends on: **parse → serialize is byte-identical for every
//! real git object.** Commits/tags keep their header block and message *verbatim* (typed
//! accessors parse on demand), so gpg signatures, `encoding`/`mergetag` headers, unusual author
//! formatting, and merge parents all reproduce exactly. If this invariant ever breaks, nothing
//! built on top (mirror, server, client) can be trusted — so it is fuzzed against real repos.

pub mod bridge;
pub mod gitdir;
pub mod loose;
pub mod mirror;
pub mod oid;
pub mod pack;
pub mod pktline;
pub mod server;
pub mod smart_http;

pub use oid::Oid;

use std::fmt;

/// The four git object types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Tag => "tag",
        }
    }
    pub fn parse(s: &[u8]) -> Option<Kind> {
        match s {
            b"blob" => Some(Kind::Blob),
            b"tree" => Some(Kind::Tree),
            b"commit" => Some(Kind::Commit),
            b"tag" => Some(Kind::Tag),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum GitError {
    Malformed(&'static str),
    BadHex,
}
impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Malformed(w) => write!(f, "malformed git object: {w}"),
            GitError::BadHex => write!(f, "bad hex oid"),
        }
    }
}
impl std::error::Error for GitError {}
pub type Result<T> = std::result::Result<T, GitError>;

/// The wrapped object header git prepends before hashing: `"<type> <len>\0"`.
pub fn header(kind: Kind, payload_len: usize) -> Vec<u8> {
    format!("{} {}\0", kind.as_str(), payload_len).into_bytes()
}

/// The git object id of a payload: `SHA-1("<type> <len>\0" ++ payload)`. This is the identity
/// git uses everywhere; reproducing it exactly is the whole point of this crate.
pub fn hash(kind: Kind, payload: &[u8]) -> Oid {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(header(kind, payload.len()));
    h.update(payload);
    Oid::from_bytes(h.finalize().into())
}

// ── typed views over a payload (parse on demand; never used for serialization) ───────────────

/// One `tree` entry: an octal mode string exactly as git stores it (`"100644"`, `"40000"`,
/// `"120000"` symlink, `"160000"` gitlink), a raw (possibly non-UTF-8) name, and a child oid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: Vec<u8>,
    pub name: Vec<u8>,
    pub oid: Oid,
}

/// Parse a `tree` payload into entries. Serialization is just [`serialize_tree`] of the same
/// entries — byte-identical, because we keep mode bytes and name bytes verbatim and preserve
/// order (git trees are already stored in canonical order).
pub fn parse_tree(payload: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < payload.len() {
        let sp = memchr(b' ', &payload[i..]).ok_or(GitError::Malformed("tree: no mode sep"))?;
        let mode = payload[i..i + sp].to_vec();
        i += sp + 1;
        let nul = memchr(0, &payload[i..]).ok_or(GitError::Malformed("tree: no name NUL"))?;
        let name = payload[i..i + nul].to_vec();
        i += nul + 1;
        if i + 20 > payload.len() {
            return Err(GitError::Malformed("tree: truncated oid"));
        }
        let mut raw = [0u8; 20];
        raw.copy_from_slice(&payload[i..i + 20]);
        i += 20;
        out.push(TreeEntry { mode, name, oid: Oid::from_bytes(raw) });
    }
    Ok(out)
}

/// Serialize tree entries back to a `tree` payload. Entries must already be in git's canonical
/// order (name-sorted, directories compared as if their name had a trailing `/`); use
/// [`sort_tree`] when constructing a keel-native tree.
pub fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(&e.mode);
        out.push(b' ');
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(e.oid.as_bytes());
    }
    out
}

/// git's tree entry ordering: byte compare on the name, but a directory (mode `40000`) sorts as
/// though its name ended in `/`. Needed only when *building* a tree in keel; parsed trees are
/// already ordered.
pub fn sort_tree(entries: &mut [TreeEntry]) {
    fn key(e: &TreeEntry) -> Vec<u8> {
        let mut k = e.name.clone();
        if e.mode == b"40000" {
            k.push(b'/');
        }
        k
    }
    entries.sort_by_key(key);
}

/// A parsed but fidelity-preserving commit/tag: the header block and message are kept as raw
/// bytes so [`serialize_headed`] reproduces the original payload exactly. Typed accessors parse
/// the header block on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headed {
    /// Every header line, each including its trailing `\n` (continuation lines — leading space —
    /// included), up to but not including the blank separator line.
    pub headers: Vec<u8>,
    /// Everything after the blank separator line, verbatim.
    pub message: Vec<u8>,
}

impl Headed {
    /// Split a commit/tag payload into (headers, message) at the first blank line. Round-trips
    /// byte-for-byte via [`serialize_headed`].
    pub fn parse(payload: &[u8]) -> Headed {
        match find(payload, b"\n\n") {
            Some(idx) => Headed {
                headers: payload[..=idx].to_vec(),      // through the first '\n'
                message: payload[idx + 2..].to_vec(),   // after the blank line
            },
            None => Headed { headers: payload.to_vec(), message: Vec::new() },
        }
    }

    /// The value of the first header named `key` (top-level lines only, not continuations),
    /// trimmed of the leading `"key "` and trailing `\n`.
    pub fn header_value(&self, key: &str) -> Option<&[u8]> {
        let mut i = 0;
        while i < self.headers.len() {
            let line_end = i + memchr(b'\n', &self.headers[i..])?;
            let line = &self.headers[i..line_end];
            if line.first() != Some(&b' ') {
                if let Some(rest) = line.strip_prefix(key.as_bytes()) {
                    if rest.first() == Some(&b' ') {
                        return Some(&rest[1..]);
                    }
                }
            }
            i = line_end + 1;
        }
        None
    }

    /// All values for a repeated header (e.g. `parent`).
    pub fn header_values(&self, key: &str) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.headers.len() {
            let Some(off) = memchr(b'\n', &self.headers[i..]) else { break };
            let line = &self.headers[i..i + off];
            if line.first() != Some(&b' ') {
                if let Some(rest) = line.strip_prefix(key.as_bytes()) {
                    if rest.first() == Some(&b' ') {
                        out.push(&rest[1..]);
                    }
                }
            }
            i += off + 1;
        }
        out
    }

    /// `tree` oid of a commit.
    pub fn tree(&self) -> Result<Oid> {
        Oid::from_hex(self.header_value("tree").ok_or(GitError::Malformed("commit: no tree"))?)
    }
    /// `parent` oids of a commit (0 for a root, 2+ for a merge).
    pub fn parents(&self) -> Result<Vec<Oid>> {
        self.header_values("parent").iter().map(|h| Oid::from_hex(h)).collect()
    }
}

/// Serialize a [`Headed`] (commit or tag) back to its payload — byte-identical to what
/// [`Headed::parse`] consumed.
pub fn serialize_headed(h: &Headed) -> Vec<u8> {
    let mut out = Vec::with_capacity(h.headers.len() + 1 + h.message.len());
    out.extend_from_slice(&h.headers);
    out.push(b'\n'); // the blank separator line
    out.extend_from_slice(&h.message);
    out
}

// ── tiny byte helpers (no memchr dependency) ─────────────────────────────────────────────────

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_hash_matches_git() {
        // git hash-object of "what is up, doc?" is a well-known fixture SHA-1.
        let oid = hash(Kind::Blob, b"what is up, doc?");
        assert_eq!(oid.to_hex(), "bd9dbf5aae1a3862dd1526723246b20206e5fc37");
    }

    #[test]
    fn empty_blob_hash_matches_git() {
        // the empty blob's SHA-1 is a git constant
        assert_eq!(hash(Kind::Blob, b"").to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn empty_tree_hash_matches_git() {
        // the empty tree's SHA-1 is the famous git constant
        assert_eq!(hash(Kind::Tree, b"").to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn tree_roundtrips_byte_identical() {
        let mut entries = vec![
            TreeEntry { mode: b"100644".to_vec(), name: b"README".to_vec(), oid: Oid::from_bytes([1; 20]) },
            TreeEntry { mode: b"40000".to_vec(), name: b"src".to_vec(), oid: Oid::from_bytes([2; 20]) },
            TreeEntry { mode: b"120000".to_vec(), name: b"link".to_vec(), oid: Oid::from_bytes([3; 20]) },
        ];
        sort_tree(&mut entries);
        let payload = serialize_tree(&entries);
        assert_eq!(parse_tree(&payload).unwrap(), entries);
        assert_eq!(serialize_tree(&parse_tree(&payload).unwrap()), payload);
    }

    #[test]
    fn commit_with_gpgsig_and_merge_roundtrips() {
        // a gnarly payload: two parents, a multi-line gpgsig (continuation lines), body with a
        // blank line. serialize∘parse must be the identity.
        let payload = b"tree 1111111111111111111111111111111111111111\n\
            parent 2222222222222222222222222222222222222222\n\
            parent 3333333333333333333333333333333333333333\n\
            author A U Thor <a@ex.com> 1700000000 +0100\n\
            committer C O Mitter <c@ex.com> 1700000005 -0800\n\
            gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQIzBAABCgAd\n -----END PGP SIGNATURE-----\n\
            \n\
            Subject line\n\nBody paragraph with a blank line above.\n"
            .to_vec();
        let h = Headed::parse(&payload);
        assert_eq!(serialize_headed(&h), payload, "commit must round-trip byte-identical");
        assert_eq!(h.tree().unwrap().to_hex(), "1111111111111111111111111111111111111111");
        assert_eq!(h.parents().unwrap().len(), 2, "merge parents both parsed");
        assert_eq!(h.header_value("author").unwrap(), b"A U Thor <a@ex.com> 1700000000 +0100");
        assert_eq!(h.message, b"Subject line\n\nBody paragraph with a blank line above.\n");
    }
}
