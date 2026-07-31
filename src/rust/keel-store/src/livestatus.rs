//! Warm, incremental working-tree status — the engine behind `keel status` when `keeld` is up.
//!
//! A plain `keel status` stats the whole tree on every call. `LiveStatus` instead keeps two flat
//! maps warm — the head tree (`path -> blob id`, git's index analogue) and the working tree
//! (`path -> (mtime, size, id)`) — plus the derived dirty set. It is seeded once with a single
//! walk, then the daemon feeds it filesystem events: each change re-stats **only** the affected
//! path and reclassifies it against head. So a repeat `status` over the socket is O(changed),
//! not O(tree) — which is what closes the gap to git's index scan on a large repo.

use crate::ignorerules::Ignore;
use crate::object::{Object, ObjectId};
use crate::repo::{ChangeKind, PathChange, Repo};
use crate::snapshot;
use crate::store::{Result, StoreError};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

pub struct LiveStatus {
    root: PathBuf,
    head_id: Option<ObjectId>,                   // the change `head` was flattened from
    head: HashMap<String, ObjectId>,             // head tree flattened: rel path -> blob id
    work: HashMap<String, (u64, u64, ObjectId)>, // warm working tree: rel path -> (mtime, size, id)
    dirty: BTreeMap<String, ChangeKind>,
}

impl LiveStatus {
    /// Seed from one walk: flatten head, stat the working tree (reusing the stat cache's blob ids
    /// so an unchanged tree hashes nothing), and compute the initial dirty set.
    pub fn seed(repo: &Repo, root: &Path) -> Result<LiveStatus> {
        let store = repo.store();
        let head_id = repo.head()?;
        let mut head = HashMap::new();
        if let Some(h) = head_id {
            let tree = repo.change(h)?.ok_or(StoreError::Corrupt(h))?.tree;
            flatten_head(store, tree, String::new(), &mut head)?;
        }
        let (cache, epoch) = store.load_stat_cache()?;
        let mut work = HashMap::new();
        walk_work(root, "", &Ignore::load(root), &cache, epoch, &mut work)?;
        let mut ls = LiveStatus { root: root.to_path_buf(), head_id, head, work, dirty: BTreeMap::new() };
        ls.recompute_all();
        Ok(ls)
    }

    /// The change id whose tree this tracker is diffing against — lets a caller notice a commit
    /// (head moved) and call [`refresh_head`](Self::refresh_head).
    pub fn head_id(&self) -> Option<ObjectId> {
        self.head_id
    }

    /// The current dirty set as a sorted list of changes — O(dirty), no filesystem access.
    pub fn changes(&self) -> Vec<PathChange> {
        self.dirty
            .iter()
            .map(|(path, &kind)| PathChange { path: path.clone(), kind })
            .collect()
    }

    /// A filesystem event touched these absolute paths: re-stat each and reclassify it alone.
    pub fn on_paths(&mut self, paths: &[PathBuf]) {
        for abs in paths {
            let Some(rel) = self.rel_of(abs) else { continue };
            self.refresh_path(&rel);
        }
    }

    /// Head moved (a commit landed): re-flatten head and reclassify every path still in play.
    pub fn refresh_head(&mut self, repo: &Repo) -> Result<()> {
        let store = repo.store();
        self.head_id = repo.head()?;
        let mut head = HashMap::new();
        if let Some(h) = self.head_id {
            let tree = repo.change(h)?.ok_or(StoreError::Corrupt(h))?.tree;
            flatten_head(store, tree, String::new(), &mut head)?;
        }
        self.head = head;
        self.recompute_all();
        Ok(())
    }

    // ── internals ───────────────────────────────────────────────────────────────────────────

    fn rel_of(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            return None;
        }
        Some(s)
    }

    /// Re-stat one working-tree path, update its warm entry, and reclassify it against head. An
    /// ignored path (`.gitignore`/`.keelignore`) is treated as absent, so a changed build artifact
    /// never shows up as dirty — matching git.
    fn refresh_path(&mut self, rel: &str) {
        let abs = self.root.join(rel);
        match fs::symlink_metadata(&abs) {
            Ok(md) if md.is_file() && !Ignore::resolve(&self.root, rel, false) => {
                let (size, mtime) = (md.len(), mtime_ns(&md));
                let unchanged = self.work.get(rel).is_some_and(|&(m, s, _)| m == mtime && s == size);
                if !unchanged {
                    match fs::read(&abs) {
                        Ok(bytes) => { self.work.insert(rel.to_string(), (mtime, size, Object::Blob(bytes).id())); }
                        Err(_) => { self.work.remove(rel); }
                    }
                }
            }
            _ => { self.work.remove(rel); } // deleted, ignored, or became a dir/symlink
        }
        self.reclassify(rel);
    }

    /// Set/clear the dirty entry for `rel` by comparing its work and head ids.
    fn reclassify(&mut self, rel: &str) {
        let w = self.work.get(rel).map(|&(_, _, id)| id);
        let h = self.head.get(rel).copied();
        let kind = match (h, w) {
            (Some(a), Some(b)) if a != b => Some(ChangeKind::Modified),
            (None, Some(_)) => Some(ChangeKind::Added),
            (Some(_), None) => Some(ChangeKind::Deleted),
            _ => None, // identical, or absent both sides
        };
        match kind {
            Some(k) => { self.dirty.insert(rel.to_string(), k); }
            None => { self.dirty.remove(rel); }
        }
    }

    fn recompute_all(&mut self) {
        self.dirty.clear();
        let keys: Vec<String> = self.work.keys().chain(self.head.keys()).cloned().collect();
        for k in keys {
            self.reclassify(&k);
        }
    }
}

/// Flatten a head tree into `path -> blob id` for every file beneath it.
fn flatten_head(
    store: &crate::store::Store,
    tree: ObjectId,
    prefix: String,
    out: &mut HashMap<String, ObjectId>,
) -> Result<()> {
    let entries = match store.get(&tree)? {
        Some(Object::Tree(t)) => t.entries,
        _ => return Ok(()),
    };
    for e in entries {
        let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
        if e.mode == snapshot::MODE_DIR {
            flatten_head(store, e.id, path, out)?;
        } else {
            out.insert(path, e.id);
        }
    }
    Ok(())
}

/// Walk the working tree, populating `path -> (mtime, size, id)` — reusing the stat cache's id
/// for unchanged files (no hashing), matching the racy-git epoch guard used by snapshots.
fn walk_work(
    dir: &Path,
    rel: &str,
    ig: &Ignore,
    cache: &HashMap<String, (u64, u64, ObjectId)>,
    epoch: u64,
    out: &mut HashMap<String, (u64, u64, ObjectId)>,
) -> Result<()> {
    let Ok(rd) = fs::read_dir(dir) else { return Ok(()) };
    for de in rd {
        let de = de?;
        let ft = de.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let name = de.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if ig.is_ignored(&path, ft.is_dir()) {
            continue;
        }
        if ft.is_dir() {
            walk_work(&de.path(), &path, &ig.descend(&path, &de.path()), cache, epoch, out)?;
        } else if ft.is_file() {
            let md = de.metadata()?;
            let (size, mtime) = (md.len(), mtime_ns(&md));
            let id = match cache.get(&path).copied() {
                Some((cm, cs, cid)) if cm == mtime && cs == size && cm < epoch => cid,
                _ => Object::Blob(fs::read(de.path())?).id(),
            };
            out.insert(path, (mtime, size, id));
        }
    }
    Ok(())
}

fn mtime_ns(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    #[test]
    fn live_status_tracks_edits_incrementally() {
        let dir = TmpDir::new();
        let root = dir.path();
        let repo = Repo::open(&root.join(".keel/store")).unwrap();
        std::fs::write(root.join("a.txt"), b"one").unwrap();
        std::fs::write(root.join("b.txt"), b"two").unwrap();
        let tree = snapshot::snapshot(repo.store(), root).unwrap();
        repo.commit_tree(tree, "init", "t", 0, None).unwrap();

        let mut ls = LiveStatus::seed(&repo, root).unwrap();
        assert!(ls.changes().is_empty(), "clean after commit");

        // edit a.txt → exactly one Modified, learned from a single path event
        std::fs::write(root.join("a.txt"), b"one-changed").unwrap();
        ls.on_paths(&[root.join("a.txt")]);
        let ch = ls.changes();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].path, "a.txt");
        assert_eq!(ch[0].kind, ChangeKind::Modified);

        // add c.txt → Added; delete b.txt → Deleted
        std::fs::write(root.join("c.txt"), b"three").unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        ls.on_paths(&[root.join("c.txt"), root.join("b.txt")]);
        let mut kinds: Vec<_> = ls.changes().into_iter().map(|c| (c.path, c.kind)).collect();
        kinds.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(kinds, vec![
            ("a.txt".into(), ChangeKind::Modified),
            ("b.txt".into(), ChangeKind::Deleted),
            ("c.txt".into(), ChangeKind::Added),
        ]);

        // revert a.txt to its committed content → drops out of dirty
        std::fs::write(root.join("a.txt"), b"one").unwrap();
        ls.on_paths(&[root.join("a.txt")]);
        assert!(!ls.changes().iter().any(|c| c.path == "a.txt"), "reverted file is clean again");
    }
}
