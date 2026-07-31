//! LMDB-backed content-addressed object store.
//!
//! Tables:
//! - `objects`        id → canonical bytes (all objects except large blobs)
//! - `blob_manifests` id → chunk manifest (large blobs only)
//! - `chunks`         chunk-id → chunk bytes (FastCDC, deduped across all blobs)
//! - `refs`           name → id (mutable named pointers)
//! - `stat_cache`     repo-relative path → (mtime_ns, size, blob-id); a git-style index
//!   that lets a snapshot skip re-hashing files whose mtime+size are unchanged
//! - `verifications`  change-id → verification tag; a POST-HOC annotation (CI runs after the
//!   commit), kept out of the immutable change so recording green/red doesn't change its id
//! - `pins`           symbol → invariant lesson; a curated rule auto-served in every brief
//!   that touches the symbol (cold-start lever: pays off from one pin, no history needed)
//! - `deltas`         blob-id → prefix/suffix byte-delta against a base blob (a `repack`-only
//!   storage form that narrows the gap to git's cross-object delta compression). The id is still
//!   BLAKE3 over the logical content; a read reconstructs and **re-verifies the id**, so a bad
//!   delta fails loudly and can never be served as wrong bytes.
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

use crate::object::{DecodeError, Object, ObjectId, Verification};
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

/// Outcome of a compare-and-swap commit ([`Store::apply_cas`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Committed,
    /// The ref had moved (its current value is carried back for the caller to retry against).
    Conflict(Option<ObjectId>),
}

/// Reserved stat_cache key holding the cache epoch (a NUL byte can't appear in a repo path).
const EPOCH_KEY: &[u8] = b"\0epoch";

fn read_id(b: &[u8]) -> Option<ObjectId> {
    (b.len() == 32).then(|| {
        let mut a = [0u8; 32];
        a.copy_from_slice(b);
        ObjectId(a)
    })
}

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
    stat_cache: Database<Bytes, Bytes>,
    verifications: Database<Bytes, Bytes>,
    pins: Database<Bytes, Bytes>,
    deltas: Database<Bytes, Bytes>,
    /// Generic namespaced KV for adapters that need to ride alongside the object store without
    /// the core knowing about them (e.g. the git-mirror's oid↔object and ref maps). Keys are
    /// `namespace ++ 0x00 ++ key`; the core stays adapter-agnostic.
    aux: Database<Bytes, Bytes>,
}

/// A cached stat entry: `(mtime_ns, size, blob-id)`. A snapshot may reuse `id` (skipping the
/// read+hash) when a file's mtime and size both still match.
pub type StatEntry = (u64, u64, ObjectId);

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        Self::open_with_map_size(path, DEFAULT_MAP_SIZE)
    }

    pub fn open_with_map_size(path: &Path, map_size: usize) -> Result<Store> {
        std::fs::create_dir_all(path)?;
        // SAFETY: standard LMDB open; the mapped file is only accessed through this env.
        let env = unsafe { EnvOpenOptions::new().map_size(map_size).max_dbs(9).open(path)? };
        let mut w = env.write_txn()?;
        let objects = env.create_database(&mut w, Some("objects"))?;
        let blob_manifests = env.create_database(&mut w, Some("blob_manifests"))?;
        let chunks = env.create_database(&mut w, Some("chunks"))?;
        let refs = env.create_database(&mut w, Some("refs"))?;
        let stat_cache = env.create_database(&mut w, Some("stat_cache"))?;
        let verifications = env.create_database(&mut w, Some("verifications"))?;
        let pins = env.create_database(&mut w, Some("pins"))?;
        let deltas = env.create_database(&mut w, Some("deltas"))?;
        let aux = env.create_database(&mut w, Some("aux"))?;
        w.commit()?;
        Ok(Store {
            env,
            objects,
            blob_manifests,
            chunks,
            refs,
            stat_cache,
            verifications,
            pins,
            deltas,
            aux,
        })
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
                            self.chunks.put(w, cidb, &pack(chunk))?;
                        }
                        manifest.extend_from_slice(cidb);
                    }
                    self.blob_manifests.put(w, &id.0, &manifest)?;
                }
                return Ok(id);
            }
        }
        // Skip if already present in either full (`objects`) or delta form — re-writing a
        // deltified blob back into `objects` would double-store it until the next repack.
        if self.objects.get(&*w, &id.0)?.is_none() && self.deltas.get(&*w, &id.0)?.is_none() {
            self.objects.put(w, &id.0, &pack(&obj.encode()))?;
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

    /// Compare-and-swap commit: write `objs` and advance `ref_name` to `new_ref` **only if**
    /// the ref still equals `expected`. Because LMDB permits one writer at a time, the read
    /// of the current ref and its update happen with no interleaving, so two un-coordinated
    /// committers linearize (the loser gets `Conflict` and retries against the new head)
    /// instead of silently clobbering each other's commit. On conflict nothing is written.
    pub fn apply_cas(
        &self,
        objs: &[Object],
        ref_name: &str,
        expected: Option<ObjectId>,
        new_ref: ObjectId,
    ) -> Result<Applied> {
        let mut w = self.env.write_txn()?;
        let cur = self.refs.get(&w, ref_name.as_bytes())?.and_then(read_id);
        if cur != expected {
            return Ok(Applied::Conflict(cur)); // dropping `w` discards any pending writes
        }
        for obj in objs {
            self.write_object(&mut w, obj)?;
        }
        self.refs.put(&mut w, ref_name.as_bytes(), &new_ref.0)?;
        w.commit()?;
        Ok(Applied::Committed)
    }

    /// Fetch and decode an object by address.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Object>> {
        let r = self.env.read_txn()?;
        self.read_object(&r, id)
    }

    /// Resolve an object within one read txn, following the delta form recursively (a delta's
    /// base may itself be a delta — `repack` bounds the chain with periodic full "keyframes", so
    /// the recursion is shallow). Reusing a single txn avoids nested-read-txn slot issues.
    fn read_object(&self, r: &RoTxn, id: &ObjectId) -> Result<Option<Object>> {
        self.read_object_depth(r, id, 0)
    }

    /// Hard cap on delta-chain reconstruct depth. `repack` bounds real chains far below this
    /// (a keyframe every 16 versions); the cap only exists so a *bug* that produced a delta
    /// cycle would surface as `Corrupt` instead of a stack overflow. Chosen well above any
    /// legitimate chain length.
    const MAX_DELTA_DEPTH: u32 = 4096;

    fn read_object_depth(&self, r: &RoTxn, id: &ObjectId, depth: u32) -> Result<Option<Object>> {
        if depth > Self::MAX_DELTA_DEPTH {
            return Err(StoreError::Corrupt(*id)); // runaway delta chain (should be impossible)
        }
        if let Some(bytes) = self.objects.get(r, &id.0)? {
            return Ok(Some(Object::decode(&unpack(bytes)?)?));
        }
        if let Some(stored) = self.deltas.get(r, &id.0)? {
            return Ok(Some(self.reconstruct_delta(r, id, stored, depth)?));
        }
        if let Some(manifest) = self.blob_manifests.get(r, &id.0)? {
            return Ok(Some(self.reassemble_blob(r, id, manifest)?));
        }
        Ok(None)
    }

    /// Rebuild a delta-stored blob: read its base, apply the copy/insert op stream, then
    /// re-verify the id. A wrong delta (or a base that no longer matches) fails as `Corrupt`
    /// rather than returning altered bytes — the same integrity guarantee chunked blobs get.
    fn reconstruct_delta(&self, r: &RoTxn, id: &ObjectId, stored: &[u8], depth: u32) -> Result<Object> {
        let raw = unpack(stored)?;
        let (base_id, ops) = split_delta(&raw).ok_or(StoreError::Corrupt(*id))?;
        let base = match self.read_object_depth(r, &base_id, depth + 1)? {
            Some(Object::Blob(b)) => b,
            _ => return Err(StoreError::Corrupt(*id)), // base missing or not a blob
        };
        let content = crate::delta::expand(&base, ops).ok_or(StoreError::Corrupt(*id))?;
        let obj = Object::Blob(content);
        if obj.id() != *id {
            return Err(StoreError::Corrupt(*id));
        }
        Ok(obj)
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
            content.extend_from_slice(&unpack(chunk)?);
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
            || self.deltas.get(&r, &id.0)?.is_some()
            || self.blob_manifests.get(&r, &id.0)?.is_some())
    }

    /// Number of stored logical objects (inline + chunked blobs).
    pub fn object_count(&self) -> Result<u64> {
        let r = self.env.read_txn()?;
        Ok(self.objects.len(&r)? + self.deltas.len(&r)? + self.blob_manifests.len(&r)?)
    }

    /// Number of distinct stored chunks (dedup denominator).
    pub fn chunk_count(&self) -> Result<u64> {
        let r = self.env.read_txn()?;
        Ok(self.chunks.len(&r)?)
    }

    /// Number of blobs currently stored in delta form.
    pub fn delta_count(&self) -> Result<u64> {
        let r = self.env.read_txn()?;
        Ok(self.deltas.len(&r)?)
    }

    /// The content of `id` iff it is currently a **full inline blob** (in `objects`, decodes to
    /// a Blob) — `None` for a delta, a chunked blob, a tree/change/session, or a miss. `repack`
    /// uses this to pick cross-path delta bases that are full (so the delta chain stays depth 1).
    pub fn blob_bytes_if_full(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let r = self.env.read_txn()?;
        match self.objects.get(&r, &id.0)? {
            Some(b) => match Object::decode(&unpack(b)?)? {
                Object::Blob(c) => Ok(Some(c)),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// All content-table keys (objects + deltas + chunked-blob manifests). For benchmarks and
    /// integrity sweeps — lets a caller `get` every stored object to time/verify reconstruction.
    pub fn content_ids(&self) -> Result<Vec<ObjectId>> {
        let r = self.env.read_txn()?;
        let mut out = Vec::new();
        for db in [&self.objects, &self.deltas, &self.blob_manifests] {
            for kv in db.iter(&r)? {
                let (k, _) = kv?;
                if let Some(id) = read_id(k) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Total logical content bytes held (sum of stored value lengths across the content tables).
    /// This is the honest "size on disk" for comparison — unlike the LMDB file length it isn't
    /// inflated by map-size reservation or by freed-but-not-reclaimed pages after a GC.
    pub fn stored_bytes(&self) -> Result<u64> {
        let r = self.env.read_txn()?;
        let mut total = 0u64;
        for db in [&self.objects, &self.deltas, &self.chunks, &self.blob_manifests] {
            for kv in db.iter(&r)? {
                let (k, v) = kv?;
                total += (k.len() + v.len()) as u64;
            }
        }
        Ok(total)
    }

    /// Re-store the inline blob `target` as a copy/insert byte-delta against `base`, **iff**
    /// that is smaller than its current on-disk (already-compressed) form. Returns the bytes
    /// saved (0 if it wasn't worth it, or `target` isn't an inline blob).
    ///
    /// Caller contract: `base` must be reachable and must NOT (transitively) delta against
    /// `target` — `repack` guarantees this by only ever pointing a higher-rank blob at a
    /// strictly lower-rank one, so delta chains are acyclic and bounded by keyframes. The
    /// reconstruct is verified against the address before the swap, so a bad delta is never
    /// committed.
    pub fn deltify(&self, target: &ObjectId, base: &ObjectId) -> Result<u64> {
        if target == base {
            return Ok(0);
        }
        let r = self.env.read_txn()?;
        // target must currently be an inline blob (chunked/absent/already-delta → skip)
        let Some(stored) = self.objects.get(&r, &target.0)? else {
            return Ok(0);
        };
        let current_size = stored.len();
        let content = match Object::decode(&unpack(stored)?)? {
            Object::Blob(b) => b,
            _ => return Ok(0), // only blobs deltify
        };
        let base_content = match self.read_object(&r, base)? {
            Some(Object::Blob(b)) => b,
            _ => return Ok(0),
        };
        drop(r);

        let ops = crate::delta::compress(&base_content, &content);
        let mut raw = Vec::with_capacity(32 + ops.len());
        raw.extend_from_slice(&base.0);
        raw.extend_from_slice(&ops);
        let packed = pack(&raw);
        if packed.len() >= current_size {
            return Ok(0); // delta didn't beat the full compressed form — leave it whole
        }

        // Verify the round-trip IN MEMORY before deleting the full copy: rebuild from
        // base_content + this delta and confirm it hashes back to `target`. A codec bug bails
        // here (returns 0, blob left whole) instead of destroying the only full copy.
        let verified = match crate::delta::expand(&base_content, &ops) {
            Some(rebuilt) => Object::Blob(rebuilt).id() == *target,
            None => false,
        };
        if !verified {
            return Ok(0);
        }

        let mut w = self.env.write_txn()?;
        self.deltas.put(&mut w, &target.0, &packed)?;
        self.objects.delete(&mut w, &target.0)?;
        w.commit()?;
        Ok((current_size - packed.len()) as u64)
    }

    // ── verifications (post-hoc green/red on an immutable change) ────────────

    /// Record a change's verification result (CI/tests run after the commit). Stored beside
    /// the change, not in it, so the change's content address never moves.
    pub fn set_verification(&self, change: &ObjectId, v: Verification) -> Result<()> {
        let mut w = self.env.write_txn()?;
        self.verifications.put(&mut w, &change.0, &[v.tag()])?;
        w.commit()?;
        Ok(())
    }

    /// A change's recorded verification, or `Unverified` if none was set.
    pub fn verification(&self, change: &ObjectId) -> Result<Verification> {
        let r = self.env.read_txn()?;
        Ok(self
            .verifications
            .get(&r, &change.0)?
            .and_then(|b| b.first().copied())
            .and_then(Verification::from_u8)
            .unwrap_or(Verification::Unverified))
    }

    // ── pinned invariants (symbol → curated lesson, auto-served) ─────────────

    /// Pin an invariant lesson to a symbol (overwrites any existing pin for it).
    pub fn set_pin(&self, symbol: &str, lesson: &str) -> Result<()> {
        let mut w = self.env.write_txn()?;
        self.pins.put(&mut w, symbol.as_bytes(), lesson.as_bytes())?;
        w.commit()?;
        Ok(())
    }

    /// The invariant pinned to `symbol`, if any.
    pub fn pin(&self, symbol: &str) -> Result<Option<String>> {
        let r = self.env.read_txn()?;
        Ok(self.pins.get(&r, symbol.as_bytes())?.map(|b| String::from_utf8_lossy(b).into_owned()))
    }

    /// All pins (symbol, lesson), sorted by symbol — for `keel pins`.
    pub fn pins(&self) -> Result<Vec<(String, String)>> {
        let r = self.env.read_txn()?;
        let mut out = Vec::new();
        for kv in self.pins.iter(&r)? {
            let (k, v) = kv?;
            if let Ok(sym) = std::str::from_utf8(k) {
                out.push((sym.to_string(), String::from_utf8_lossy(v).into_owned()));
            }
        }
        out.sort();
        Ok(out)
    }

    // ── lessons (change → the non-obvious thing learned; the flywheel's fuel) ─────────────────
    //
    // A lesson is a POST-HOC annotation on an (immutable) change — like a verification — so it
    // works on git-driven history too: `keel commit` (git) makes the change, `keel learn` attaches
    // what was learned, and a later brief retrieves it for related work. Stored in the `aux` KV so
    // it never moves the change's address. Value is `task ++ 0x00 ++ lesson` (task has no NUL).

    /// Attach a `(task, lesson)` to a change.
    pub fn set_lesson(&self, change: &ObjectId, task: &str, lesson: &str) -> Result<()> {
        let mut v = Vec::with_capacity(task.len() + 1 + lesson.len());
        v.extend_from_slice(task.as_bytes());
        v.push(0);
        v.extend_from_slice(lesson.as_bytes());
        self.aux_put("lesson", &change.0, &v)
    }

    /// The `(task, lesson)` recorded for a change, if any.
    pub fn lesson(&self, change: &ObjectId) -> Result<Option<(String, String)>> {
        Ok(self.aux_get("lesson", &change.0)?.map(|v| split_lesson(&v)))
    }

    /// Every recorded lesson as `(change, task, lesson)`.
    pub fn lessons(&self) -> Result<Vec<(ObjectId, String, String)>> {
        let mut out = Vec::new();
        for (k, v) in self.aux_iter("lesson")? {
            if let Some(id) = read_id(&k) {
                let (task, lesson) = split_lesson(&v);
                out.push((id, task, lesson));
            }
        }
        Ok(out)
    }

    // ── generic namespaced KV (adapter side-tables; core stays adapter-agnostic) ─────────────

    fn aux_key(ns: &str, key: &[u8]) -> Vec<u8> {
        let mut k = Vec::with_capacity(ns.len() + 1 + key.len());
        k.extend_from_slice(ns.as_bytes());
        k.push(0);
        k.extend_from_slice(key);
        k
    }

    /// Put `key → val` in namespace `ns`.
    pub fn aux_put(&self, ns: &str, key: &[u8], val: &[u8]) -> Result<()> {
        let mut w = self.env.write_txn()?;
        self.aux.put(&mut w, &Self::aux_key(ns, key), val)?;
        w.commit()?;
        Ok(())
    }

    /// Put many `(key, val)` in namespace `ns` in ONE transaction (bulk ingest).
    pub fn aux_put_many(&self, ns: &str, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut w = self.env.write_txn()?;
        for (k, v) in items {
            self.aux.put(&mut w, &Self::aux_key(ns, k), v)?;
        }
        w.commit()?;
        Ok(())
    }

    /// Get `key` from namespace `ns`.
    pub fn aux_get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let r = self.env.read_txn()?;
        Ok(self.aux.get(&r, &Self::aux_key(ns, key))?.map(|v| v.to_vec()))
    }

    /// Delete `key` from namespace `ns` (returns whether it existed). Used e.g. for a ref
    /// deletion pushed by a client.
    pub fn aux_delete(&self, ns: &str, key: &[u8]) -> Result<bool> {
        let mut w = self.env.write_txn()?;
        let existed = self.aux.delete(&mut w, &Self::aux_key(ns, key))?;
        w.commit()?;
        Ok(existed)
    }

    /// All `(key, val)` in namespace `ns` (key without the namespace prefix).
    pub fn aux_iter(&self, ns: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let r = self.env.read_txn()?;
        let prefix = Self::aux_key(ns, b"");
        let mut out = Vec::new();
        for kv in self.aux.iter(&r)? {
            let (k, v) = kv?;
            if let Some(rest) = k.strip_prefix(prefix.as_slice()) {
                out.push((rest.to_vec(), v.to_vec()));
            }
        }
        Ok(out)
    }

    /// Number of entries in namespace `ns`.
    pub fn aux_count(&self, ns: &str) -> Result<usize> {
        Ok(self.aux_iter(ns)?.len())
    }

    // ── stat cache (snapshot fast-path) ──────────────────────────────────────

    /// Load the whole stat cache into memory (one read txn). Snapshot consults this to skip
    /// re-hashing files whose mtime+size are unchanged.
    /// Returns `(entries, epoch)`. `epoch` is the time the cache was last written (ns since the
    /// epoch); an entry is only trustworthy if its mtime is strictly older than it (see the
    /// snapshot fast path) — the "racy git" guard against a same-size edit within one mtime
    /// tick of a snapshot. The epoch lives under a reserved NUL key (never a repo path).
    pub fn load_stat_cache(&self) -> Result<(std::collections::HashMap<String, StatEntry>, u64)> {
        let r = self.env.read_txn()?;
        let mut map = std::collections::HashMap::new();
        let mut epoch = 0u64;
        for kv in self.stat_cache.iter(&r)? {
            let (k, v) = kv?;
            if k == EPOCH_KEY {
                if v.len() == 8 {
                    epoch = u64::from_le_bytes(v.try_into().unwrap());
                }
            } else if let (Ok(path), Some(entry)) = (std::str::from_utf8(k), decode_stat(v)) {
                map.insert(path.to_string(), entry);
            }
        }
        Ok((map, epoch))
    }

    /// Upsert stat-cache entries and stamp `epoch` (the snapshot's time). No-op on an empty
    /// slice, so a snapshot that changed nothing writes nothing (the prior epoch stays valid).
    pub fn put_stat_cache(&self, updates: &[(String, StatEntry)], epoch: u64) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut w = self.env.write_txn()?;
        for (path, entry) in updates {
            self.stat_cache.put(&mut w, path.as_bytes(), &encode_stat(entry))?;
        }
        self.stat_cache.put(&mut w, EPOCH_KEY, &epoch.to_le_bytes())?;
        w.commit()?;
        Ok(())
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
                match Object::decode(&unpack(bytes)?)? {
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
            } else if let Some(stored) = self.deltas.get(&w, &idb)? {
                // a delta blob is kept alive, and its base must be kept too (the delta can't be
                // reconstructed without it) — push the base so reachability follows the edge.
                if let Some((base, _)) = split_delta(&unpack(stored)?) {
                    stack.push(base.0);
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
        let del_delta = unreached(self.deltas.iter(&w)?, &reach_obj)?;
        let del_man = unreached(self.blob_manifests.iter(&w)?, &reach_obj)?;
        let del_chunk = unreached(self.chunks.iter(&w)?, &reach_chunk)?;

        for k in del_obj.iter().chain(del_delta.iter()).chain(del_man.iter()) {
            // an id is in exactly one of objects/deltas/blob_manifests; deleting a miss is a no-op
            self.objects.delete(&mut w, k.as_slice())?;
            self.deltas.delete(&mut w, k.as_slice())?;
            self.blob_manifests.delete(&mut w, k.as_slice())?;
        }
        for k in &del_chunk {
            self.chunks.delete(&mut w, k.as_slice())?;
        }
        w.commit()?;

        Ok(GcStats {
            objects_removed: (del_obj.len() + del_delta.len() + del_man.len()) as u64,
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

// ── value compression (store-internal) ───────────────────────────────────────
//
// Stored `objects` and `chunks` values are DEFLATE-compressed with a 1-byte tag, so a
// source tree (mostly small text files, inlined below the chunk threshold) doesn't cost
// more on disk than the raw bytes. Addressing is unaffected — the id is BLAKE3 over the
// *logical* content; compression is a pure storage-layer transform below the address.
// Incompressible values keep their raw form (tag 0) so we never pay to "compress" noise.
// (Manifests and refs are tiny/incompressible and stay raw — only objects/chunks go through
// here.)

const PACK_RAW: u8 = 0;
const PACK_DEFLATE: u8 = 1;

fn pack(bytes: &[u8]) -> Vec<u8> {
    // Level 6: measured byte-identical to level 9 on real source trees, so the higher level only
    // costs ingest CPU for no size win. The store's gap to git is NOT the codec — it's git's
    // cross-object delta compression; `repack`'s prefix/suffix byte-deltas narrow that gap for
    // the localized-edit case (see `deltify` / NEW-1111).
    let comp = miniz_oxide::deflate::compress_to_vec(bytes, 6);
    if comp.len() + 1 < bytes.len() {
        let mut out = Vec::with_capacity(comp.len() + 1);
        out.push(PACK_DEFLATE);
        out.extend_from_slice(&comp);
        out
    } else {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(PACK_RAW);
        out.extend_from_slice(bytes);
        out
    }
}

fn unpack(stored: &[u8]) -> Result<Vec<u8>> {
    match stored.split_first() {
        Some((&PACK_RAW, rest)) => Ok(rest.to_vec()),
        Some((&PACK_DEFLATE, rest)) => miniz_oxide::inflate::decompress_to_vec(rest)
            .map_err(|_| StoreError::Io(std::io::Error::other("stored value: deflate failed"))),
        _ => Err(StoreError::Io(std::io::Error::other("stored value: bad/empty pack tag"))),
    }
}

// ── blob delta form (base-id ++ copy/insert op stream; see the `delta` module) ──
//
// A delta value is `base_id(32) ++ ops`, where `ops` is a copy/insert stream (module `delta`)
// that reconstructs the blob from a base. `repack` builds these; `deltify` adopts one only when
// it beats the full compressed blob, so a blob with no good base simply stays whole (never a
// regression). The op codec finds shared regions anywhere in the base, so a blob can delta
// against a merely *similar* one, not just its same-path predecessor.

/// Split a stored lesson (`task ++ 0x00 ++ lesson`) into `(task, lesson)`.
fn split_lesson(v: &[u8]) -> (String, String) {
    match v.iter().position(|&b| b == 0) {
        Some(i) => (
            String::from_utf8_lossy(&v[..i]).into_owned(),
            String::from_utf8_lossy(&v[i + 1..]).into_owned(),
        ),
        None => (String::new(), String::from_utf8_lossy(v).into_owned()),
    }
}

/// Split a delta value (already `unpack`ed) into `(base_id, ops)`.
fn split_delta(raw: &[u8]) -> Option<(ObjectId, &[u8])> {
    let base = ObjectId(raw.get(0..32)?.try_into().ok()?);
    Some((base, raw.get(32..)?))
}

// ── stat-cache entry codec (fixed 48 bytes: mtime_ns | size | blob-id) ────────

fn encode_stat((mtime, size, id): &StatEntry) -> [u8; 48] {
    let mut out = [0u8; 48];
    out[0..8].copy_from_slice(&mtime.to_le_bytes());
    out[8..16].copy_from_slice(&size.to_le_bytes());
    out[16..48].copy_from_slice(&id.0);
    out
}

fn decode_stat(v: &[u8]) -> Option<StatEntry> {
    if v.len() != 48 {
        return None;
    }
    let mtime = u64::from_le_bytes(v[0..8].try_into().ok()?);
    let size = u64::from_le_bytes(v[8..16].try_into().ok()?);
    let mut id = [0u8; 32];
    id.copy_from_slice(&v[16..48]);
    Some((mtime, size, ObjectId(id)))
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
    fn deltify_roundtrips_and_gc_keeps_the_base() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        // two versions of a file: v2 tweaks the middle → long shared prefix + suffix
        let v1: Vec<u8> = (0..3000).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let mut v2 = v1.clone();
        let at = v1.windows(9).position(|w| w == b"line 1500").unwrap();
        v2.splice(at..at + 9, b"line 1500-EDITED".iter().copied());

        let base = s.put(&Object::Blob(v1.clone())).unwrap();
        let target = s.put(&Object::Blob(v2.clone())).unwrap();
        let saved = s.deltify(&target, &base).unwrap();
        assert!(saved > 0, "localized edit should delta-compress");
        assert_eq!(s.delta_count().unwrap(), 1);

        // the delta still reads back byte-identical
        assert_eq!(s.get(&target).unwrap(), Some(Object::Blob(v2.clone())));

        // reference ONLY the delta (not its base); GC must keep the base alive via the delta edge
        s.set_ref("r", &target).unwrap();
        s.gc().unwrap();
        assert!(s.has(&base).unwrap(), "GC must keep a delta's base");
        assert_eq!(s.get(&target).unwrap(), Some(Object::Blob(v2)), "delta survives GC intact");

        // dropping the reference lets BOTH the delta and its base go
        s.delete_ref("r").unwrap();
        s.gc().unwrap();
        assert!(!s.has(&target).unwrap() && !s.has(&base).unwrap(), "unreferenced delta+base swept");
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
    fn pins_roundtrip_and_list() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        assert_eq!(s.pin("charge").unwrap(), None);
        s.set_pin("charge", "settle before charge").unwrap();
        s.set_pin("login", "rate-limit").unwrap();
        assert_eq!(s.pin("charge").unwrap().as_deref(), Some("settle before charge"));
        s.set_pin("charge", "settle FIRST").unwrap(); // overwrite
        assert_eq!(s.pin("charge").unwrap().as_deref(), Some("settle FIRST"));
        assert_eq!(
            s.pins().unwrap(),
            vec![("charge".to_string(), "settle FIRST".to_string()), ("login".to_string(), "rate-limit".to_string())],
            "sorted by symbol"
        );
    }

    #[test]
    fn verification_side_table_roundtrips() {
        let d = TmpDir::new();
        let s = Store::open(&d.0).unwrap();
        let id = s.put(&Object::Blob(b"x".to_vec())).unwrap();
        assert_eq!(s.verification(&id).unwrap(), Verification::Unverified, "default is unverified");
        s.set_verification(&id, Verification::Green).unwrap();
        assert_eq!(s.verification(&id).unwrap(), Verification::Green);
        s.set_verification(&id, Verification::Red).unwrap();
        assert_eq!(s.verification(&id).unwrap(), Verification::Red, "later result overwrites");
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
