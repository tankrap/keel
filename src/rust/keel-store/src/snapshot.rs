//! Working-tree ⇄ object-store bridge: snapshot a directory into content-addressed
//! trees + blobs, and materialize a tree back onto disk.
//!
//! Snapshot is deterministic — the same directory content always yields the same root
//! tree address, regardless of filesystem enumeration order (tree entries sort by name
//! on encode) — so `snapshot ∘ checkout ∘ snapshot` is the identity on the address.
//! Identical file content deduplicates to one blob.

use crate::object::{Object, ObjectId, Tree, TreeEntry};
use crate::store::{Result, StatEntry, Store, StoreError};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub const MODE_FILE: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
pub const MODE_DIR: u32 = 0o040000;

/// Flush the in-memory object batch once it holds this much blob content, so ingesting a
/// huge tree bounds memory instead of building every object up front (NEW-1109 #3).
const FLUSH_BYTES: usize = 256 * 1024 * 1024;

/// Names skipped during snapshot — build artifacts + VCS internals. `.keel` is keel's own
/// metadata/store dir (the analogue of `.git`); it MUST be skipped or a repo whose store
/// lives under its own working tree would version its own database. A real ignore policy
/// (.gitignore / .keelignore) is a follow-up; this is a sane default so snapshotting a repo
/// root doesn't ingest `node_modules`, `.git`, `.keel`, etc.
const IGNORED: &[&str] = &[".keel", "node_modules", ".git", "target", "dist", "build"];

/// Snapshot `dir` recursively, returning the root tree's address. Symlinks and other
/// non-regular files are skipped (a real ignore policy comes later).
///
/// Two properties make this cheap at scale (NEW-1109):
///  - **stat cache**: a file whose mtime+size match the store's cache reuses its blob id
///    without re-reading or re-hashing — so a clean `status`/`commit` stats the tree but
///    hashes nothing. (The cached blob's presence is verified, so a GC'd blob is re-read.)
///  - **streaming**: objects flush to the store in bounded batches during the walk, so a
///    giant tree doesn't accumulate every blob in memory. Blobs become durable before the
///    root; the commit's ref advance (in `Repo`) is the atomic step, so a crash mid-flush
///    only leaves unreachable objects for GC — never a dangling HEAD.
pub fn snapshot(store: &Store, dir: &Path) -> Result<ObjectId> {
    snapshot_impl(store, dir, true)
}

/// Like [`snapshot`] but ignores the stat cache — always reads + hashes every file. Use when
/// committing DISTINCT trees in rapid succession at the same paths (e.g. `keel import`
/// replaying git history): the cache keys by (path, mtime, size), so two different same-size
/// files written within one mtime tick at the same path would false-hit ("racy git"). Slower,
/// but always correct.
pub fn snapshot_uncached(store: &Store, dir: &Path) -> Result<ObjectId> {
    snapshot_impl(store, dir, false)
}

/// Read-only snapshot for `status`: compute the work-tree root id and every tree object it
/// contains WITHOUT writing anything to the store — no blobs, no trees, no stat-cache update.
/// Unchanged files reuse their cached blob id (stat only); changed files are hashed in memory
/// and discarded. Returns `(root_id, trees)` mapping each built tree's id → its `Tree`, so the
/// caller can diff the (unpersisted) work tree against head.
///
/// `status` must not mutate the store. The persisting [`snapshot`] re-serialized ~thousands of
/// unchanged trees and opened a write transaction on every call (each tree a skip-lookup) — that
/// write-txn overhead, not the walk, is what made a clean `status` slower than git's index scan.
pub fn snapshot_ro(store: &Store, dir: &Path) -> Result<(ObjectId, HashMap<ObjectId, Tree>)> {
    let (cache, epoch) = store.load_stat_cache()?;
    let mut trees = HashMap::new();
    let mut updates = Vec::new();
    let root = build_ro(dir, "", &cache, epoch, &mut trees, &mut updates)?;
    // Refresh the stat cache with any files we had to (re)hash — just as `git status`
    // opportunistically rewrites its index's stat info. On a fully clean, warm tree `updates`
    // is empty and `put_stat_cache` is a no-op, so a repeat `status` writes nothing at all.
    store.put_stat_cache(&updates, now_ns())?;
    Ok((root, trees))
}

fn build_ro(
    dir: &Path,
    rel: &str,
    cache: &HashMap<String, StatEntry>,
    epoch: u64,
    trees: &mut HashMap<ObjectId, Tree>,
    updates: &mut Vec<(String, StatEntry)>,
) -> Result<ObjectId> {
    let join = |name: &str| if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
    let mut entries = Vec::new();
    for de in fs::read_dir(dir)? {
        let de = de?;
        let ft = de.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let name = de.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        if ft.is_dir() {
            let id = build_ro(&de.path(), &join(&name), cache, epoch, trees, updates)?;
            entries.push(TreeEntry { name, mode: MODE_DIR, id });
        } else if ft.is_file() {
            let md = de.metadata()?;
            let (mode, size, mtime) = (mode_from_meta(&md), md.len(), mtime_ns(&md));
            let path = join(&name);
            // Same stat-cache fast path + racy-git guard as `build`, but a miss only hashes the
            // file in memory to learn its id — status never needs to persist the blob, only to
            // remember its stat entry so the next `status` is a stat-only walk.
            // No `store.has` probe here (unlike the persisting `build`): status only needs a
            // stable id to compare against head, and trusting mtime+size — with the racy-git
            // epoch guard — is exactly how git's index avoids re-reading unchanged files. This
            // also lets the cache hit on a mirror-built repo whose native blobs were never
            // materialized as objects.
            let id = match cache.get(&path).copied() {
                Some((cm, cs, cid)) if cm == mtime && cs == size && cm < epoch => cid,
                _ => {
                    let id = Object::Blob(fs::read(de.path())?).id();
                    updates.push((path, (mtime, size, id)));
                    id
                }
            };
            entries.push(TreeEntry { name, mode, id });
        }
    }
    let tree = Tree { entries };
    let id = Object::Tree(tree.clone()).id();
    trees.insert(id, tree);
    Ok(id)
}

fn snapshot_impl(store: &Store, dir: &Path, use_cache: bool) -> Result<ObjectId> {
    let (cache, epoch) = if use_cache { store.load_stat_cache()? } else { (HashMap::new(), 0) };
    let mut ctx =
        SnapCtx { store, cache, epoch, use_cache, objs: Vec::new(), pending_bytes: 0, updates: Vec::new() };
    let root = build(&mut ctx, dir, "")?;
    if !ctx.objs.is_empty() {
        store.put_many(&ctx.objs)?; // flush the tail
    }
    if use_cache {
        store.put_stat_cache(&ctx.updates, now_ns())?;
    }
    Ok(root)
}

/// Wall-clock now in nanoseconds — the stat-cache epoch (compared against file mtimes).
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

struct SnapCtx<'a> {
    store: &'a Store,
    cache: HashMap<String, StatEntry>,
    /// time the loaded cache was written; an entry is trusted only if its mtime < epoch.
    epoch: u64,
    use_cache: bool,
    objs: Vec<Object>,
    pending_bytes: usize,
    updates: Vec<(String, StatEntry)>,
}

impl SnapCtx<'_> {
    /// Queue an object; flush the batch once it holds enough blob content to bound memory.
    fn push(&mut self, obj: Object, approx_bytes: usize) -> Result<()> {
        self.objs.push(obj);
        self.pending_bytes += approx_bytes;
        if self.pending_bytes >= FLUSH_BYTES {
            self.store.put_many(&self.objs)?;
            self.objs.clear();
            self.pending_bytes = 0;
        }
        Ok(())
    }
}

fn build(ctx: &mut SnapCtx, dir: &Path, rel: &str) -> Result<ObjectId> {
    let join = |name: &str| if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
    let mut entries = Vec::new();
    for de in fs::read_dir(dir)? {
        let de = de?;
        let ft = de.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let name = de.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        if ft.is_dir() {
            let id = build(ctx, &de.path(), &join(&name))?;
            entries.push(TreeEntry { name, mode: MODE_DIR, id });
        } else if ft.is_file() {
            let md = de.metadata()?;
            let (mode, size, mtime) = (mode_from_meta(&md), md.len(), mtime_ns(&md));
            let path = join(&name);
            // stat-cache fast path: unchanged mtime+size AND the blob is still present →
            // reuse its id, skipping read+hash+store entirely.
            let cached =
                ctx.use_cache.then(|| ctx.cache.get(&path).copied()).flatten();
            let id = match cached {
                // trust only if mtime+size match AND the entry is strictly older than the cache
                // epoch (racy-git guard: a file touched within the snapshot's own tick is not
                // trusted, so a same-size same-tick edit can't slip through) AND the blob exists.
                Some((cm, cs, cid)) if cm == mtime && cs == size && cm < ctx.epoch && ctx.store.has(&cid)? => cid,
                _ => {
                    let bytes = fs::read(de.path())?;
                    let blob = Object::Blob(bytes);
                    let id = blob.id();
                    let len = match &blob {
                        Object::Blob(b) => b.len(),
                        _ => 0,
                    };
                    ctx.push(blob, len)?;
                    if ctx.use_cache {
                        ctx.updates.push((path, (mtime, size, id)));
                    }
                    id
                }
            };
            entries.push(TreeEntry { name, mode, id });
        }
    }
    // encode() sorts entries by name, so id is order-independent.
    let tree = Object::Tree(Tree { entries });
    let id = tree.id();
    ctx.push(tree, 0)?; // trees are tiny; don't count them toward the flush threshold
    Ok(id)
}

/// Materialize `tree_id` onto `dir` (created if needed).
pub fn checkout(store: &Store, tree_id: ObjectId, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let tree = match store.get(&tree_id)?.ok_or(StoreError::Corrupt(tree_id))? {
        Object::Tree(t) => t,
        _ => return Err(StoreError::Corrupt(tree_id)),
    };
    for e in &tree.entries {
        let p = dir.join(&e.name);
        if e.mode == MODE_DIR {
            checkout(store, e.id, &p)?;
        } else {
            let content = match store.get(&e.id)?.ok_or(StoreError::Corrupt(e.id))? {
                Object::Blob(b) => b,
                _ => return Err(StoreError::Corrupt(e.id)),
            };
            fs::write(&p, &content)?;
            set_exec(&p, e.mode)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_from_meta(m: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if m.permissions().mode() & 0o111 != 0 { MODE_EXEC } else { MODE_FILE }
}
#[cfg(not(unix))]
fn mode_from_meta(_m: &fs::Metadata) -> u32 {
    MODE_FILE
}

/// File mtime as nanoseconds since the epoch, or 0 if unavailable (which simply forces a
/// cache miss — safe, just no fast-path for that file).
fn mtime_ns(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn set_exec(path: &Path, mode: u32) -> Result<()> {
    if mode == MODE_EXEC {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm)?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn set_exec(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    fn write(dir: &Path, rel: &str, content: &[u8]) {
        let p = dir.join(rel);
        if let Some(par) = p.parent() {
            fs::create_dir_all(par).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    #[test]
    fn snapshot_checkout_roundtrip_is_stable() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let out = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        write(work.path(), "readme.md", b"hello\n");
        write(work.path(), "src/lib.rs", b"fn main() {}\n");
        write(work.path(), "src/nested/deep.txt", b"deep\n");

        let id1 = snapshot(&s, work.path()).unwrap();
        checkout(&s, id1, out.path()).unwrap();
        let id2 = snapshot(&s, out.path()).unwrap();
        assert_eq!(id1, id2, "snapshot∘checkout∘snapshot must be identity on the address");
    }

    #[test]
    fn identical_files_share_one_blob() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        write(work.path(), "a.txt", b"same bytes");
        write(work.path(), "b.txt", b"same bytes");
        let root = snapshot(&s, work.path()).unwrap();
        let t = match s.get(&root).unwrap().unwrap() {
            Object::Tree(t) => t,
            _ => panic!("root is not a tree"),
        };
        let a = t.entries.iter().find(|e| e.name == "a.txt").unwrap().id;
        let b = t.entries.iter().find(|e| e.name == "b.txt").unwrap().id;
        assert_eq!(a, b, "identical content deduplicates to one blob");
    }

    #[test]
    fn deterministic_regardless_of_fs_order() {
        let sd = TmpDir::new();
        let w1 = TmpDir::new();
        let w2 = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        for w in [&w1, &w2] {
            write(w.path(), "z.txt", b"z");
            write(w.path(), "a.txt", b"a");
            write(w.path(), "m/x", b"x");
        }
        assert_eq!(snapshot(&s, w1.path()).unwrap(), snapshot(&s, w2.path()).unwrap());
    }

    #[test]
    fn snapshot_uncached_bypasses_the_stat_cache() {
        let work = TmpDir::new();
        write(work.path(), "f.txt", b"same-size-payload");
        // cached snapshot populates the stat cache and yields a root
        let sd = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        let r_cached = snapshot(&s, work.path()).unwrap();
        let (m, epoch) = s.load_stat_cache().unwrap();
        assert!(!m.is_empty(), "cached snapshot writes the cache");
        assert!(epoch > 0, "cached snapshot stamps a racy-guard epoch");

        // uncached snapshot on a fresh store gives the SAME root (content-addressed) but
        // leaves the stat cache untouched — it always re-reads, never trusts mtime+size.
        let sd2 = TmpDir::new();
        let s2 = Store::open(sd2.path()).unwrap();
        let r_unc = snapshot_uncached(&s2, work.path()).unwrap();
        assert_eq!(r_cached, r_unc, "same content → same address, cache or not");
        assert!(s2.load_stat_cache().unwrap().0.is_empty(), "uncached leaves the stat cache empty");
    }

    #[test]
    fn stat_cache_survives_gc_of_its_blobs() {
        // The stat cache reuses a blob id only if the blob is still present — so a GC that
        // reclaimed the (unreferenced) blob must not cause a snapshot to build a tree that
        // references a missing blob.
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let out = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        write(work.path(), "a.txt", b"alpha\n");
        write(work.path(), "sub/b.txt", b"beta\n");

        let root1 = snapshot(&s, work.path()).unwrap(); // populates the stat cache
        // nothing references root1, so GC reclaims its blobs — but the stat cache still names them
        let stats = s.gc().unwrap();
        assert!(stats.objects_removed > 0, "unreferenced blobs were collected ({stats:?})");

        let root2 = snapshot(&s, work.path()).unwrap();
        assert_eq!(root1, root2, "same content → same root, cache or not");
        // the guard must have re-stored the blobs: a checkout now fully materializes
        checkout(&s, root2, out.path()).unwrap();
        assert_eq!(fs::read(out.path().join("a.txt")).unwrap(), b"alpha\n");
        assert_eq!(fs::read(out.path().join("sub/b.txt")).unwrap(), b"beta\n");
    }

    #[test]
    fn stat_cache_detects_content_change() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        write(work.path(), "f.txt", b"one\n");
        let r1 = snapshot(&s, work.path()).unwrap();
        // change content (size + mtime differ) → cache miss → new root
        write(work.path(), "f.txt", b"two different\n");
        let r2 = snapshot(&s, work.path()).unwrap();
        assert_ne!(r1, r2, "a content change must invalidate the cached blob");
        // and a no-op snapshot is stable (fast path)
        assert_eq!(r2, snapshot(&s, work.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn exec_bit_survives_roundtrip() {
        use std::os::unix::fs::PermissionsExt;
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let out = TmpDir::new();
        let s = Store::open(sd.path()).unwrap();
        let p = work.path().join("run.sh");
        fs::write(&p, b"#!/bin/sh\n").unwrap();
        let mut perm = fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&p, perm).unwrap();

        let id1 = snapshot(&s, work.path()).unwrap();
        checkout(&s, id1, out.path()).unwrap();
        assert_eq!(id1, snapshot(&s, out.path()).unwrap(), "exec bit is part of the address");
        let m = fs::metadata(out.path().join("run.sh")).unwrap().permissions().mode();
        assert!(m & 0o111 != 0, "checked-out file must be executable");
    }
}
