//! keel ↔ git mirror (epic NEW-1113, M4).
//!
//! Holds a full git repository *inside* the keel store, losslessly, and regenerates a
//! **byte-identical** `.git` from it (same commit SHAs — `git fsck` clean, `git log` matches).
//! This is the tool-level mirror that makes migrating onto keel incremental: both stores stay
//! valid, nothing is lost.
//!
//! Storage (via the store's generic namespaced KV, so the keel core stays git-agnostic):
//! - `gobj`  git-oid → `kind_tag ++ payload`  (tree/commit/tag; small, kept verbatim)
//! - `gblob` git-oid → keel-blob-id           (blob **bytes deduped** into keel's own blob store)
//! - `gref`  ref-name → `"<40-hex>"` or `"ref: <target>"` (symbolic, e.g. HEAD)
//!
//! Blob payloads are the bulk of a repo and are identical to file content keel already stores,
//! so we keep them once (as keel blobs) and map the git-oid to them; only the small tree/commit
//! objects are held verbatim.

use crate::{hash, loose, Kind, Oid};
use keel_store::object::{Object, ObjectId};
use keel_store::store::Store;
use std::io;
use std::path::Path;

const NS_OBJ: &str = "gobj";
const NS_BLOB: &str = "gblob";
const NS_REF: &str = "gref";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MirrorStats {
    pub blobs: u64,
    pub trees: u64,
    pub commits: u64,
    pub tags: u64,
    pub refs: u64,
}

fn kind_tag(k: Kind) -> u8 {
    match k {
        Kind::Blob => 0,
        Kind::Tree => 2,
        Kind::Commit => 3,
        Kind::Tag => 4,
    }
}
fn kind_untag(b: u8) -> Option<Kind> {
    match b {
        0 => Some(Kind::Blob),
        2 => Some(Kind::Tree),
        3 => Some(Kind::Commit),
        4 => Some(Kind::Tag),
        _ => None,
    }
}

/// Ingest git's whole object database from a `git cat-file --batch-all-objects --batch --buffer`
/// byte stream. Bulk-writes in batches so 10k+ objects don't cost 10k transactions.
pub fn ingest_batch_stream(store: &Store, buf: &[u8]) -> io::Result<MirrorStats> {
    let mut stats = MirrorStats::default();
    let mut obj_batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut blob_batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    const FLUSH: usize = 2048;

    let mut i = 0usize;
    while i < buf.len() {
        let nl = match memchr(b'\n', &buf[i..]) {
            Some(p) => i + p,
            None => break,
        };
        let line = &buf[i..nl];
        i = nl + 1;
        let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        if parts.len() != 3 {
            continue; // "<name> missing" or a malformed line — skip
        }
        let goid = Oid::from_hex(parts[0]).map_err(|_| bad("oid hex"))?;
        let kind = Kind::parse(parts[1]).ok_or_else(|| bad("kind"))?;
        let size: usize = std::str::from_utf8(parts[2])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("size"))?;
        if i + size > buf.len() {
            return Err(bad("truncated payload"));
        }
        let payload = &buf[i..i + size];
        i += size + 1; // payload + trailing LF

        // integrity: the stream's oid must equal our own hash of the payload
        if hash(kind, payload) != goid {
            return Err(bad("stream oid disagrees with computed hash"));
        }

        match kind {
            Kind::Blob => {
                let id = store.put(&Object::Blob(payload.to_vec())).map_err(to_io)?;
                blob_batch.push((goid.as_bytes().to_vec(), id.0.to_vec()));
                stats.blobs += 1;
            }
            _ => {
                let mut v = Vec::with_capacity(1 + payload.len());
                v.push(kind_tag(kind));
                v.extend_from_slice(payload);
                obj_batch.push((goid.as_bytes().to_vec(), v));
                match kind {
                    Kind::Tree => stats.trees += 1,
                    Kind::Commit => stats.commits += 1,
                    Kind::Tag => stats.tags += 1,
                    Kind::Blob => unreachable!(),
                }
            }
        }
        if obj_batch.len() >= FLUSH {
            store.aux_put_many(NS_OBJ, &obj_batch).map_err(to_io)?;
            obj_batch.clear();
        }
        if blob_batch.len() >= FLUSH {
            store.aux_put_many(NS_BLOB, &blob_batch).map_err(to_io)?;
            blob_batch.clear();
        }
    }
    store.aux_put_many(NS_OBJ, &obj_batch).map_err(to_io)?;
    store.aux_put_many(NS_BLOB, &blob_batch).map_err(to_io)?;
    Ok(stats)
}

/// Fetch a git object from the mirror as `(kind, payload)` — from the verbatim table for
/// tree/commit/tag, or reconstructed from the deduped keel blob for a blob. `None` if absent.
pub fn get_object(store: &Store, oid: &Oid) -> io::Result<Option<(Kind, Vec<u8>)>> {
    let k = oid.as_bytes();
    if let Some(v) = store.aux_get(NS_OBJ, k).map_err(to_io)? {
        if let Some(kind) = v.first().copied().and_then(kind_untag) {
            return Ok(Some((kind, v[1..].to_vec())));
        }
    }
    if let Some(idb) = store.aux_get(NS_BLOB, k).map_err(to_io)? {
        if idb.len() == 32 {
            let mut id = [0u8; 32];
            id.copy_from_slice(&idb);
            if let Some(Object::Blob(b)) = store.get(&ObjectId(id)).map_err(to_io)? {
                return Ok(Some((Kind::Blob, b)));
            }
        }
    }
    Ok(None)
}

/// All refs in the mirror as `(name, value)` — direct refs carry a 40-hex oid, symbolic refs
/// (e.g. HEAD) a `"ref: <target>"` value.
pub fn refs(store: &Store) -> io::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (k, v) in store.aux_iter(NS_REF).map_err(to_io)? {
        if let (Ok(name), Ok(val)) = (String::from_utf8(k), String::from_utf8(v)) {
            out.push((name, val));
        }
    }
    out.sort();
    Ok(out)
}

/// Whether the mirror already holds a git object (in either the verbatim or deduped-blob table).
/// Lets the CLI sync only *new* objects after a git command instead of re-ingesting everything.
pub fn has_object(store: &Store, oid: &Oid) -> io::Result<bool> {
    let k = oid.as_bytes();
    Ok(store.aux_get(NS_OBJ, k).map_err(to_io)?.is_some()
        || store.aux_get(NS_BLOB, k).map_err(to_io)?.is_some())
}

/// Ingest already-resolved git objects (e.g. from a received/unpacked push) into the mirror:
/// blob bytes deduped into keel's blob store, tree/commit/tag payloads kept verbatim. Bulk-writes.
pub fn ingest_objects(store: &Store, objs: &[(Kind, Vec<u8>)]) -> io::Result<MirrorStats> {
    let mut stats = MirrorStats::default();
    let mut obj_batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut blob_batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    const FLUSH: usize = 2048;
    for (kind, payload) in objs {
        let goid = hash(*kind, payload);
        match kind {
            Kind::Blob => {
                let id = store.put(&Object::Blob(payload.clone())).map_err(to_io)?;
                blob_batch.push((goid.as_bytes().to_vec(), id.0.to_vec()));
                stats.blobs += 1;
            }
            _ => {
                let mut v = Vec::with_capacity(1 + payload.len());
                v.push(kind_tag(*kind));
                v.extend_from_slice(payload);
                obj_batch.push((goid.as_bytes().to_vec(), v));
                match kind {
                    Kind::Tree => stats.trees += 1,
                    Kind::Commit => stats.commits += 1,
                    Kind::Tag => stats.tags += 1,
                    Kind::Blob => unreachable!(),
                }
            }
        }
        if obj_batch.len() >= FLUSH {
            store.aux_put_many(NS_OBJ, &obj_batch).map_err(to_io)?;
            obj_batch.clear();
        }
        if blob_batch.len() >= FLUSH {
            store.aux_put_many(NS_BLOB, &blob_batch).map_err(to_io)?;
            blob_batch.clear();
        }
    }
    store.aux_put_many(NS_OBJ, &obj_batch).map_err(to_io)?;
    store.aux_put_many(NS_BLOB, &blob_batch).map_err(to_io)?;
    Ok(stats)
}

/// Point a ref at a target (40-hex oid or `"ref: <target>"`).
pub fn set_ref(store: &Store, name: &str, value: &str) -> io::Result<()> {
    store.aux_put(NS_REF, name.as_bytes(), value.as_bytes()).map_err(to_io)
}

/// Delete a ref.
pub fn delete_ref(store: &Store, name: &str) -> io::Result<bool> {
    store.aux_delete(NS_REF, name.as_bytes()).map_err(to_io)
}

/// Record refs (name → target). A symbolic ref (e.g. HEAD) has a `"ref: <target>"` value; a
/// direct ref has a 40-hex value.
pub fn ingest_refs(store: &Store, refs: &[(String, String)]) -> io::Result<u64> {
    let items: Vec<(Vec<u8>, Vec<u8>)> =
        refs.iter().map(|(n, v)| (n.clone().into_bytes(), v.clone().into_bytes())).collect();
    store.aux_put_many(NS_REF, &items).map_err(to_io)?;
    Ok(refs.len() as u64)
}

/// Regenerate a byte-identical `.git` at `dst` from the mirror. Creates a minimal valid repo
/// skeleton (no dependency on the `git` binary), writes every object as a loose object, and
/// writes the refs + HEAD. Every written object's recomputed oid is checked against the stored
/// git-oid, so a corrupted mirror fails loudly rather than emitting wrong history.
pub fn materialize(store: &Store, dst: &Path) -> io::Result<MirrorStats> {
    let git = dst.join(".git");
    std::fs::create_dir_all(git.join("objects"))?;
    std::fs::create_dir_all(git.join("refs").join("heads"))?;
    std::fs::create_dir_all(git.join("refs").join("tags"))?;
    std::fs::write(
        git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n",
    )?;

    let mut stats = MirrorStats::default();
    // tree/commit/tag objects
    for (goid_bytes, v) in store.aux_iter(NS_OBJ).map_err(to_io)? {
        let goid = oid_from(&goid_bytes)?;
        let kind = v.first().copied().and_then(kind_untag).ok_or_else(|| bad("gobj kind tag"))?;
        let written = loose::write(&git, kind, &v[1..])?;
        if written != goid {
            return Err(bad("materialized object oid mismatch (corrupt mirror)"));
        }
        match kind {
            Kind::Tree => stats.trees += 1,
            Kind::Commit => stats.commits += 1,
            Kind::Tag => stats.tags += 1,
            Kind::Blob => {}
        }
    }
    // blobs (bytes from keel's dedup store)
    for (goid_bytes, blobid) in store.aux_iter(NS_BLOB).map_err(to_io)? {
        let goid = oid_from(&goid_bytes)?;
        if blobid.len() != 32 {
            return Err(bad("gblob keel-id length"));
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&blobid);
        let bytes = match store.get(&ObjectId(id)).map_err(to_io)? {
            Some(Object::Blob(b)) => b,
            _ => return Err(bad("gblob target is not a blob")),
        };
        let written = loose::write(&git, Kind::Blob, &bytes)?;
        if written != goid {
            return Err(bad("materialized blob oid mismatch (corrupt mirror)"));
        }
        stats.blobs += 1;
    }
    // refs + HEAD
    let mut wrote_head = false;
    for (name, val) in store.aux_iter(NS_REF).map_err(to_io)? {
        let name = String::from_utf8(name).map_err(|_| bad("ref name utf8"))?;
        let val = String::from_utf8(val).map_err(|_| bad("ref value utf8"))?;
        write_ref(&git, &name, &val)?;
        if name == "HEAD" {
            wrote_head = true;
        }
        stats.refs += 1;
    }
    if !wrote_head {
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n")?;
    }
    Ok(stats)
}

fn write_ref(git: &Path, name: &str, val: &str) -> io::Result<()> {
    // guard against path escapes; refs are always under .git
    if name.contains("..") || name.starts_with('/') {
        return Err(bad("suspicious ref name"));
    }
    let path = git.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = val.to_string();
    line.push('\n');
    std::fs::write(path, line)
}

fn oid_from(bytes: &[u8]) -> io::Result<Oid> {
    if bytes.len() != 20 {
        return Err(bad("git-oid length"));
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(bytes);
    Ok(Oid::from_bytes(a))
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}
fn bad(msg: &'static str) -> io::Error {
    io::Error::other(format!("mirror: {msg}"))
}
fn to_io(e: keel_store::store::StoreError) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame one object as a `git cat-file --batch` record: `<hex> <type> <size>\n<payload>\n`.
    fn record(kind: Kind, payload: &[u8]) -> Vec<u8> {
        let oid = hash(kind, payload);
        let mut v = format!("{} {} {}\n", oid.to_hex(), kind.as_str(), payload.len()).into_bytes();
        v.extend_from_slice(payload);
        v.push(b'\n');
        v
    }

    #[test]
    fn ingest_then_materialize_reproduces_git_oids() {
        let dir = std::env::temp_dir().join(format!("keelgit-mirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("store")).unwrap();

        // a blob, a tree referencing it, and a commit referencing the tree — real git payloads
        let blob = b"fn main() {}\n";
        let blob_oid = hash(Kind::Blob, blob);
        let mut tree_payload = Vec::new();
        tree_payload.extend_from_slice(b"100644 main.rs\0");
        tree_payload.extend_from_slice(blob_oid.as_bytes());
        let tree_oid = hash(Kind::Tree, &tree_payload);
        let commit_payload = format!(
            "tree {}\nauthor A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n\nhi\n",
            tree_oid.to_hex()
        )
        .into_bytes();
        let commit_oid = hash(Kind::Commit, &commit_payload);

        let mut stream = Vec::new();
        stream.extend(record(Kind::Blob, blob));
        stream.extend(record(Kind::Tree, &tree_payload));
        stream.extend(record(Kind::Commit, &commit_payload));

        let stats = ingest_batch_stream(&store, &stream).unwrap();
        assert_eq!((stats.blobs, stats.trees, stats.commits), (1, 1, 1));
        ingest_refs(&store, &[
            ("refs/heads/main".into(), commit_oid.to_hex()),
            ("HEAD".into(), "ref: refs/heads/main".into()),
        ])
        .unwrap();

        // regenerate a .git and confirm every object reads back byte-identical under its git oid
        let out = dir.join("out");
        let mstats = materialize(&store, &out).unwrap();
        assert_eq!((mstats.blobs, mstats.trees, mstats.commits, mstats.refs), (1, 1, 1, 2));
        let git = out.join(".git");
        assert_eq!(loose::read(&git, &blob_oid).unwrap().unwrap(), (Kind::Blob, blob.to_vec()));
        assert_eq!(loose::read(&git, &tree_oid).unwrap().unwrap(), (Kind::Tree, tree_payload));
        assert_eq!(loose::read(&git, &commit_oid).unwrap().unwrap(), (Kind::Commit, commit_payload));
        // HEAD is symbolic, branch points at the commit
        assert_eq!(std::fs::read_to_string(git.join("HEAD")).unwrap(), "ref: refs/heads/main\n");
        assert_eq!(
            std::fs::read_to_string(git.join("refs/heads/main")).unwrap(),
            format!("{}\n", commit_oid.to_hex())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
