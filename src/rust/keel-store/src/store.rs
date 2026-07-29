//! LMDB-backed content-addressed object store.
//!
//! Tables:
//! - `objects`        id → canonical bytes (all objects except large blobs)
//! - `blob_manifests` id → chunk manifest (large blobs only)
//! - `chunks`         chunk-id → chunk bytes (FastCDC, deduped across all blobs)
//! - `refs`           name → id (mutable named pointers)
//!
//! Content-addressing is stable regardless of storage form: a blob's id is always
//! `BLAKE3([KIND_BLOB] ++ content)`, computed over the *logical content*. Whether it
//! is stored inline or split into FastCDC chunks is a storage decision that never
//! touches the address. `get` reassembles chunked blobs and **re-verifies the id**,
//! so a corrupt or missing chunk surfaces as an error instead of wrong bytes.
//!
//! LMDB was chosen by benchmark (`results-storage-engine.md`): mmap B+tree + MVCC
//! single-writer + copy-on-write (no WAL) matches keel's read-heavy, concurrent,
//! write-once, crash-only pattern, with zero-recovery restart.

use crate::object::{DecodeError, Object, ObjectId};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn};
use std::collections::HashSet;
use std::fmt;
use std::path::Path;

/// Default LMDB map size (address-space reservation; the file is sparse).
const DEFAULT_MAP_SIZE: usize = 64 * 1024 * 1024 * 1024; // 64 GiB

/// Blobs larger than this are stored as FastCDC chunk manifests (deduped);
/// smaller blobs are inlined. `keel_core::MAX` is the FastCDC max chunk size, so
/// below it a blob is at most one chunk and inlining is strictly cheaper.
const CHUNK_THRESHOLD: usize = crate::chunk::MAX;

#[derive(Debug)]
pub enum StoreError {
    Db(heed::Error),
    Io(std::io::Error),
    Decode(DecodeError),
    /// a stored object failed integrity verification (missing/altered chunk)
    Corrupt(ObjectId),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Db(e) => write!(f, "store db error: {e}"),
            StoreError::Io(e) => write!(f, "store io error: {e}"),
            StoreError::Decode(e) => write!(f, "corrupt object in store: {e}"),
            StoreError::Corrupt(id) => write!(f, "integrity failure for object {id}"),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<heed::Error> for StoreError {
    fn from(e: heed::Error) -> Self {
        StoreError::Db(e)
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}
impl From<DecodeError> for StoreError {
    fn from(e: DecodeError) -> Self {
        StoreError::Decode(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// What a garbage-collection sweep reclaimed / kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStats {
    pub objects_removed: u64,
    pub chunks_removed: u64,
    pub objects_kept: u64,
    pub chunks_kept: u64,
}

/// A handle to a keel object store. Cheap to clone (the LMDB env is refcounted);
/// clones share one env, so N reader threads hit one mapping.
#[derive(Clone)]
pub struct Store {
    env: Env,
    objects: Database<Bytes, Bytes>,
    blob_manifests: Database<Bytes, Bytes>,
    chunks: Database<Bytes, Bytes>,
    refs: Database<Bytes, Bytes>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        Self::open_with_map_size(path, DEFAULT_MAP_SIZE)
    }

    pub fn open_with_map_size(path: &Path, map_size: usize) -> Result<Store> {
        std::fs::create_dir_all(path)?;
        // SAFETY: standard LMDB open; the mapped file is only accessed through this env.
        let env = unsafe { EnvOpenOptions::new().map_size(map_size).max_dbs(4).open(path)? };
        let mut w = env.write_txn()?;
        let objects = env.create_database(&mut w, Some("objects"))?;
        let blob_manifests = env.create_database(&mut w, Some("blob_manifests"))?;
        let chunks = env.create_database(&mut w, Some("chunks"))?;
        let refs = env.create_database(&mut w, Some("refs"))?;
        w.commit()?;
        Ok(Store { env, objects, blob_manifests, chunks, refs })
    }

    /// Store an object, returning its content address. Idempotent.
    pub fn put(&self, obj: &Object) -> Result<ObjectId> {
        let mut w = self.env.write_txn()?;
        let id = self.write_object(&mut w, obj)?;
        w.commit()?;
        Ok(id)
    }

    /// Store many objects in ONE atomic transaction (all-or-nothing). This is the
    /// primitive the transactional-fusion model needs: a change + its session +
    /// verification commit together or not at all.
    pub fn put_many(&self, objs: &[Object]) -> Result<Vec<ObjectId>> {
        let mut w = self.env.write_txn()?;
        let mut ids = Vec::with_capacity(objs.len());
        for obj in objs {
            ids.push(self.write_object(&mut w, obj)?);
        }
        w.commit()?;
        Ok(ids)
    }

    /// Write one object within an existing transaction. Idempotent per address.
    fn write_object(&self, w: &mut RwTxn, obj: &Object) -> Result<ObjectId> {
        let id = obj.id();
        if let Object::Blob(content) = obj {
            if content.len() > CHUNK_THRESHOLD {
                if self.blob_manifests.get(&*w, &id.0)?.is_none() {
                    let ranges = crate::chunk::chunk_ranges(content);
                    let mut manifest = Vec::with_capacity(4 + ranges.len() * 32);
                    put_uvarint(&mut manifest, ranges.len() as u64);
                    for (a, b) in ranges {
                        let chunk = &content[a..b];
                        let cid = blake3::hash(chunk);
                        let cidb = cid.as_bytes();
                        if self.chunks.get(&*w, cidb)?.is_none() {
                            self.chunks.put(w, cidb, chunk)?;
                        }
                        manifest.extend_from_slice(cidb);
                    }
                    self.blob_manifests.put(w, &id.0, &manifest)?;
                }
                return Ok(id);
            }
        }
        if self.objects.get(&*w, &id.0)?.is_none() {
            self.objects.put(w, &id.0, &obj.encode())?;
        }
        Ok(id)
    }

    /// Atomically store objects AND update refs in one transaction — the fused
    /// commit primitive: a change (with its session + verification) lands together
    /// with the ref advance, or not at all. Nothing else can observe a half-commit.
    pub fn apply(&self, objs: &[Object], set_refs: &[(&str, ObjectId)]) -> Result<Vec<ObjectId>> {
        let mut w = self.env.write_txn()?;
        let mut ids = Vec::with_capacity(objs.len());
        for obj in objs {
            ids.push(self.write_object(&mut w, obj)?);
        }
        for (name, id) in set_refs {
            self.refs.put(&mut w, name.as_bytes(), &id.0)?;
        }
        w.commit()?;
        Ok(ids)
    }

    /// Fetch and decode an object by address.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Object>> {
        let r = self.env.read_txn()?;
        if let Some(bytes) = self.objects.get(&r, &id.0)? {
            return Ok(Some(Object::decode(bytes)?));
        }
        if let Some(manifest) = self.blob_manifests.get(&r, &id.0)? {
            return Ok(Some(self.reassemble_blob(&r, id, manifest)?));
        }
        Ok(None)
    }

    fn reassemble_blob(&self, r: &RoTxn, id: &ObjectId, manifest: &[u8]) -> Result<Object> {
        let mut i = 0usize;
        let n = read_uvarint(manifest, &mut i).ok_or(StoreError::Corrupt(*id))?;
        let mut content = Vec::new();
        for _ in 0..n {
            let end = i.checked_add(32).ok_or(StoreError::Corrupt(*id))?;
            let cidb = manifest.get(i..end).ok_or(StoreError::Corrupt(*id))?;
            i = end;
            let chunk = self.chunks.get(r, cidb)?.ok_or(StoreError::Corrupt(*id))?;
            content.extend_from_slice(chunk);
        }
        if i != manifest.len() {
            return Err(StoreError::Corrupt(*id));
        }
        let obj = Object::Blob(content);
        if obj.id() != *id {
            return Err(StoreError::Corrupt(*id)); // reassembled bytes don't match the address
        }
        Ok(obj)
    }

    /// Whether an address is present, without decoding/reassembling.
    pub fn has(&self, id: &ObjectId) -> Result<bool> {
        let r = self.env.read_txn()?;
        Ok(self.objects.get(&r, &id.0)?.is_some()
            || self.blob_manifests.get(&r, &id.0)?.is_some())
    }

    /// Number of stored logical objects (inline + chunked blobs).
    pub fn object_count(&self) -> Result<u64> {
        let r = self.env.read_txn()?;
        Ok(self.objects.len(&r)? + self.blob_manifests.len(&r)?)
    }

    /// Number of distinct stored chunks (dedup denominator).
    pub fn chunk_count(&self) -> Result<u64> {
        let r = self.env.read_txn()?;
        Ok(self.chunks.len(&r)?)
    }

    // ── refs (mutable named pointers) ────────────────────────────────────────

    pub fn set_ref(&self, name: &str, id: &ObjectId) -> Result<()> {
        let mut w = self.env.write_txn()?;
        self.refs.put(&mut w, name.as_bytes(), &id.0)?;
        w.commit()?;
        Ok(())
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<ObjectId>> {
        let r = self.env.read_txn()?;
        match self.refs.get(&r, name.as_bytes())? {
            Some(b) if b.len() == 32 => {
                let mut a = [0u8; 32];
                a.copy_from_slice(b);
                Ok(Some(ObjectId(a)))
            }
            _ => Ok(None),
        }
    }

    pub fn delete_ref(&self, name: &str) -> Result<bool> {
        let mut w = self.env.write_txn()?;
        let existed = self.refs.delete(&mut w, name.as_bytes())?;
        w.commit()?;
        Ok(existed)
    }

    pub fn list_refs(&self) -> Result<Vec<(String, ObjectId)>> {
        let r = self.env.read_txn()?;
        let mut out = Vec::new();
        for kv in self.refs.iter(&r)? {
            let (name, idb) = kv?;
            if idb.len() == 32 {
                let mut a = [0u8; 32];
                a.copy_from_slice(idb);
                out.push((String::from_utf8_lossy(name).into_owned(), ObjectId(a)));
            }
        }
        Ok(out)
    }

    /// Reachability GC: mark everything reachable from refs (the only roots) —
    /// change→parents/tree/session, tree→entries, session→referenced blobs, and
    /// chunked-blob→chunks — then sweep the rest. Reclaims objects orphaned by a
    /// crash mid-snapshot or by ref rewrites. Safe because content is immutable and refs
    /// are the roots. The whole mark+sweep runs in ONE write transaction, so a concurrent
    /// commit cannot interleave between mark and sweep and re-reference (e.g. via dedup) an
    /// object we're about to delete; other writers simply queue for GC's duration.
    pub fn gc(&self) -> Result<GcStats> {
        let mut w = self.env.write_txn()?;
        let mut reach_obj: HashSet<[u8; 32]> = HashSet::new();
        let mut reach_chunk: HashSet<[u8; 32]> = HashSet::new();
        let mut stack: Vec<[u8; 32]> = Vec::new();
        for kv in self.refs.iter(&w)? {
            let (_, v) = kv?;
            if v.len() == 32 {
                let mut a = [0u8; 32];
                a.copy_from_slice(v);
                stack.push(a);
            }
        }
        while let Some(idb) = stack.pop() {
            if !reach_obj.insert(idb) {
                continue;
            }
            if let Some(bytes) = self.objects.get(&w, &idb)? {
                match Object::decode(bytes)? {
                    Object::Blob(_) => {}
                    Object::Tree(t) => {
                        for e in &t.entries {
                            stack.push(e.id.0);
                        }
                    }
                    Object::Change(c) => {
                        for p in &c.parents {
                            stack.push(p.0);
                        }
                        stack.push(c.tree.0);
                        if let Some(s) = c.session {
                            stack.push(s.0);
                        }
                    }
                    Object::Session(s) => {
                        for o in [s.prompts, s.context_served].into_iter().flatten() {
                            stack.push(o.0);
                        }
                        for x in s.tool_calls.iter().chain(s.tool_results.iter()) {
                            stack.push(x.0);
                        }
                    }
                }
            } else if let Some(manifest) = self.blob_manifests.get(&w, &idb)? {
                let mut i = 0usize;
                if let Some(n) = read_uvarint(manifest, &mut i) {
                    for _ in 0..n {
                        match manifest.get(i..i + 32) {
                            Some(cid) => {
                                let mut a = [0u8; 32];
                                a.copy_from_slice(cid);
                                reach_chunk.insert(a);
                                i += 32;
                            }
                            None => break,
                        }
                    }
                }
            }
        }

        // collect unreachable keys (mutating during iteration is unsafe), then sweep — all
        // still inside the same write txn `w`.
        let del_obj = unreached(self.objects.iter(&w)?, &reach_obj)?;
        let del_man = unreached(self.blob_manifests.iter(&w)?, &reach_obj)?;
        let del_chunk = unreached(self.chunks.iter(&w)?, &reach_chunk)?;

        for k in del_obj.iter().chain(del_man.iter()) {
            // an id is in exactly one of objects/blob_manifests; deleting a miss is a no-op
            self.objects.delete(&mut w, k.as_slice())?;
            self.blob_manifests.delete(&mut w, k.as_slice())?;
        }
        for k in &del_chunk {
            self.chunks.delete(&mut w, k.as_slice())?;
        }
        w.commit()?;

        Ok(GcStats {
            objects_removed: (del_obj.len() + del_man.len()) as u64,
            chunks_removed: del_chunk.len() as u64,
            objects_kept: self.object_count()?,
            chunks_kept: self.chunk_count()?,
        })
    }
}

/// Keys present in `iter` but not in `reachable`.
fn unreached<'a, I>(iter: I, reachable: &HashSet<[u8; 32]>) -> Result<Vec<[u8; 32]>>
where
    I: Iterator<Item = std::result::Result<(&'a [u8], &'a [u8]), heed::Error>>,
{
    let mut out = Vec::new();
    for kv in iter {
        let (k, _) = kv?;
        if k.len() == 32 {
            let mut a = [0u8; 32];
            a.copy_from_slice(k);
            if !reachable.contains(&a) {
                out.push(a);
            }
        }
    }
    Ok(out)
}

// ── manifest varint (store-internal) ─────────────────────────────────────────

fn put_uvarint(o: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        o.push(b);
        if v == 0 {
            break;
        }
    }
}

fn read_uvarint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut x: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *b.get(*i)?;
        *i += 1;
        if shift >= 64 {
            return None;
        }
        x |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Change, Session, Tree, TreeEntry, Verification};
    use crate::testutil::TmpDir;

    fn oid(b: u8) -> ObjectId {
        ObjectId([b; 32])
    }

    /// Deterministic pseudo-random bytes (LCG) so tests are reproducible.
    fn fill(n: usize, seed: u32) -> Vec<u8> {
        let mut v = vec![0u8; n];
        let mut s = seed | 1;
        for b in &mut v {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (s >> 24) as u8;
        }
        v
    }

    #[test]
    fn put_get_round_trip_all_kinds() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let objs = vec![
            Object::Blob(b"file contents\n".to_vec()),
            Object::Tree(Tree {
                entries: vec![TreeEntry { name: "a".into(), mode: 0o100644, id: oid(1) }],
            }),
            Object::Change(Change {
                parents: vec![oid(2)],
                tree: oid(3),
                session: None,
                intent: "do the thing".into(),
                author: "acct:x".into(),
                timestamp: 100,
                verification: Verification::Unverified,
            }),
            Object::Session(Session {
                task: "t".into(),
                model: "claude-opus-4-8".into(),
                lesson: String::new(),
                prompts: None,
                context_served: None,
                tool_calls: vec![],
                tool_results: vec![],
                verification: Verification::Green,
                tokens_in: 1,
                tokens_out: 2,
            }),
        ];
        for obj in &objs {
            let id = s.put(obj).unwrap();
            assert_eq!(id, obj.id());
            assert!(s.has(&id).unwrap());
            assert_eq!(s.get(&id).unwrap().as_ref(), Some(obj));
        }
    }

    #[test]
    fn put_is_idempotent() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let obj = Object::Blob(b"dedup me".to_vec());
        let id1 = s.put(&obj).unwrap();
        let id2 = s.put(&obj).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(s.object_count().unwrap(), 1);
    }

    #[test]
    fn put_many_batches_atomically() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let objs = vec![
            Object::Blob(b"one".to_vec()),
            Object::Blob(b"two".to_vec()),
            Object::Change(Change {
                parents: vec![],
                tree: oid(9),
                session: None,
                intent: "batch".into(),
                author: "acct:x".into(),
                timestamp: 7,
                verification: Verification::Green,
            }),
        ];
        let ids = s.put_many(&objs).unwrap();
        assert_eq!(ids.len(), 3);
        for (obj, id) in objs.iter().zip(&ids) {
            assert_eq!(*id, obj.id());
            assert_eq!(s.get(id).unwrap().as_ref(), Some(obj));
        }
        assert_eq!(s.object_count().unwrap(), 3);
    }

    #[test]
    fn missing_object_is_none() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        assert_eq!(s.get(&oid(0xff)).unwrap(), None);
        assert!(!s.has(&oid(0xff)).unwrap());
    }

    #[test]
    fn large_blob_is_chunked_and_reassembles() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let content = fill(300 * 1024, 7); // > CHUNK_THRESHOLD → chunked
        let obj = Object::Blob(content.clone());
        let id = s.put(&obj).unwrap();
        assert_eq!(id, obj.id(), "chunking must not change the address");
        assert!(s.chunk_count().unwrap() > 1, "large blob split into chunks");
        assert_eq!(s.get(&id).unwrap(), Some(Object::Blob(content)));
    }

    #[test]
    fn shared_content_deduplicates_chunks() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        // Two large blobs sharing a big identical prefix → shared FastCDC chunks.
        let common = fill(256 * 1024, 42);
        let mut a = common.clone();
        a.extend_from_slice(&fill(6 * 1024, 1));
        let mut b = common.clone();
        b.extend_from_slice(&fill(6 * 1024, 2));

        let ida = s.put(&Object::Blob(a.clone())).unwrap();
        let idb = s.put(&Object::Blob(b.clone())).unwrap();

        // both reassemble exactly
        assert_eq!(s.get(&ida).unwrap(), Some(Object::Blob(a.clone())));
        assert_eq!(s.get(&idb).unwrap(), Some(Object::Blob(b.clone())));

        // dedup: stored chunks strictly fewer than storing each blob's chunks naively
        let naive = crate::chunk::chunk_ranges(&a).len() + crate::chunk::chunk_ranges(&b).len();
        let stored = s.chunk_count().unwrap() as usize;
        assert!(
            stored < naive,
            "expected chunk dedup: stored {stored} should be < naive {naive}"
        );
    }

    #[test]
    fn tampered_chunk_is_detected_not_returned() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let content = fill(200 * 1024, 3);
        let id = s.put(&Object::Blob(content)).unwrap();
        assert!(s.get(&id).unwrap().is_some());

        // delete one chunk directly — reassembly must FAIL, not return wrong bytes
        let victim = {
            let r = s.env.read_txn().unwrap();
            let (k, _) = s.chunks.iter(&r).unwrap().next().unwrap().unwrap();
            k.to_vec()
        };
        {
            let mut w = s.env.write_txn().unwrap();
            s.chunks.delete(&mut w, victim.as_slice()).unwrap();
            w.commit().unwrap();
        }
        match s.get(&id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn gc_removes_unreachable_keeps_reachable() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        // reachable graph: ref main → change → tree → blob
        let bid = s.put(&Object::Blob(b"kept".to_vec())).unwrap();
        let tid = s
            .put(&Object::Tree(Tree {
                entries: vec![TreeEntry { name: "f".into(), mode: 0o100644, id: bid }],
            }))
            .unwrap();
        let cid = s
            .put(&Object::Change(Change {
                parents: vec![],
                tree: tid,
                session: None,
                intent: "c".into(),
                author: "a".into(),
                timestamp: 1,
                verification: Verification::Green,
            }))
            .unwrap();
        s.set_ref("main", &cid).unwrap();

        // orphans: a small blob and a chunked blob, referenced by nothing
        let orphan = s.put(&Object::Blob(b"orphan".to_vec())).unwrap();
        let orphan_big = s.put(&Object::Blob(fill(200 * 1024, 9))).unwrap();

        let stats = s.gc().unwrap();
        assert!(stats.objects_removed >= 2, "both orphans removed ({stats:?})");
        assert!(stats.chunks_removed > 0, "orphan chunked-blob chunks removed");
        assert_eq!(s.get(&orphan).unwrap(), None);
        assert_eq!(s.get(&orphan_big).unwrap(), None);
        // reachable survive
        assert!(s.get(&cid).unwrap().is_some());
        assert!(s.get(&tid).unwrap().is_some());
        assert_eq!(s.get(&bid).unwrap(), Some(Object::Blob(b"kept".to_vec())));
    }

    #[test]
    fn gc_keeps_reachable_chunked_blob() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let big = fill(200 * 1024, 4);
        let bid = s.put(&Object::Blob(big.clone())).unwrap();
        s.set_ref("keep", &bid).unwrap();
        let chunks_before = s.chunk_count().unwrap();

        let stats = s.gc().unwrap();
        assert_eq!(stats.objects_removed, 0);
        assert_eq!(s.get(&bid).unwrap(), Some(Object::Blob(big)));
        assert_eq!(s.chunk_count().unwrap(), chunks_before, "reachable chunks kept");
    }

    #[test]
    fn refs_set_get_list_delete() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let id = s.put(&Object::Blob(b"main tip".to_vec())).unwrap();
        assert_eq!(s.get_ref("main").unwrap(), None);
        s.set_ref("main", &id).unwrap();
        assert_eq!(s.get_ref("main").unwrap(), Some(id));

        let other = s.put(&Object::Blob(b"feature tip".to_vec())).unwrap();
        s.set_ref("feature/x", &other).unwrap();
        let mut refs = s.list_refs().unwrap();
        refs.sort();
        assert_eq!(refs, vec![("feature/x".into(), other), ("main".into(), id)]);

        s.set_ref("main", &other).unwrap();
        assert_eq!(s.get_ref("main").unwrap(), Some(other));
        assert!(s.delete_ref("main").unwrap());
        assert!(!s.delete_ref("main").unwrap());
        assert_eq!(s.get_ref("main").unwrap(), None);
    }

    #[test]
    fn persists_across_reopen() {
        let d = TmpDir::new();
        let (id, big_id);
        let big = fill(200 * 1024, 5);
        {
            let s = Store::open(&d.0).unwrap();
            id = s.put(&Object::Blob(b"durable".to_vec())).unwrap();
            big_id = s.put(&Object::Blob(big.clone())).unwrap();
            s.set_ref("head", &id).unwrap();
        }
        let s = Store::open(&d.0).unwrap();
        assert_eq!(s.get(&id).unwrap(), Some(Object::Blob(b"durable".to_vec())));
        assert_eq!(s.get(&big_id).unwrap(), Some(Object::Blob(big)));
        assert_eq!(s.get_ref("head").unwrap(), Some(id));
    }
}
