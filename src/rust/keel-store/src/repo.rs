//! The change DAG + refs on top of the object store — a minimal but real VCS core.
//!
//! A [`Repo`] tracks a `HEAD`-like branch ref (`main`). Committing snapshots the working
//! tree into content-addressed objects, then atomically writes the `change` and advances
//! the branch in one transaction (see [`Store::apply`]) — so `HEAD` never points at a
//! half-written commit. Snapshot objects are written first and become durable before the
//! ref advances; a crash mid-snapshot leaves only unreachable objects (future GC), never a
//! dangling HEAD.

use crate::object::{Change, Object, ObjectId, Tree, TreeEntry, Verification};
use crate::snapshot;
use crate::store::{Applied, Result, Store, StoreError};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub struct Repo {
    store: Store,
    branch: String,
}

/// How a path differs between two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    /// Single-letter status marker (git-like): `A` / `M` / `D`.
    pub fn marker(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        }
    }
}

/// One path that differs between two trees (e.g. working tree vs `HEAD`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub path: String,
    pub kind: ChangeKind,
}

impl Repo {
    /// Open (creating if needed) a repo rooted at `path`, on branch `main`.
    pub fn open(path: &Path) -> Result<Repo> {
        Ok(Repo { store: Store::open(path)?, branch: "main".to_string() })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The current tip change, if any.
    pub fn head(&self) -> Result<Option<ObjectId>> {
        self.store.get_ref(&self.branch)
    }

    /// Paths tracked in HEAD — every file, plus each ancestor directory. The snapshot/status walk
    /// uses this to exempt already-tracked paths from `.gitignore` (git only ignores untracked
    /// paths), so a force-added ignored file isn't dropped or reported as a spurious deletion.
    fn tracked_paths(&self) -> Result<HashSet<String>> {
        let mut set = HashSet::new();
        let head_tree = match self.head()? {
            Some(h) => self.change(h)?.ok_or(StoreError::Corrupt(h))?.tree,
            None => return Ok(set),
        };
        let mut files = Vec::new();
        self.tree_files(head_tree, "", &mut files)?;
        for (path, _) in files {
            // insert every ancestor directory prefix, then the file itself
            let mut idx = 0;
            while let Some(pos) = path[idx..].find('/') {
                let end = idx + pos;
                set.insert(path[..end].to_string());
                idx = end + 1;
            }
            set.insert(path);
        }
        Ok(set)
    }

    /// Commit an already-snapshotted `tree` as a new change, advancing the branch.
    /// Parents are the current tip (empty for the first commit).
    pub fn commit_tree(
        &self,
        tree: ObjectId,
        intent: &str,
        author: &str,
        timestamp: u64,
        session: Option<ObjectId>,
    ) -> Result<ObjectId> {
        // Compare-and-swap against the current head, retrying if a concurrent committer moved
        // the branch — so two un-coordinated commits LINEARIZE (never silently lose one). Each
        // retry re-parents onto the new head (so the change id changes, which is correct).
        for _ in 0..64 {
            let expected = self.head()?;
            let obj = Object::Change(Change {
                parents: expected.into_iter().collect(),
                tree,
                session,
                intent: intent.to_string(),
                author: author.to_string(),
                timestamp,
                verification: Verification::Unverified,
            });
            let id = obj.id();
            match self.store.apply_cas(&[obj], self.branch.as_str(), expected, id)? {
                Applied::Committed => return Ok(id),
                Applied::Conflict(_) => continue, // head moved under us — rebuild on it
            }
        }
        Err(StoreError::Io(std::io::Error::other("branch too contended: gave up after 64 commit retries")))
    }

    /// Store a change with EXPLICIT parents — for replaying an external commit DAG (e.g. `keel
    /// import`) where parents are known ids, not the current head. Unlike [`Self::commit_tree`]
    /// this does not CAS against head or move any ref; the caller advances the branch with
    /// [`Self::set_branch`] once the whole graph is written. Parents must already be stored.
    pub fn commit_tree_with_parents(
        &self,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        intent: &str,
        author: &str,
        timestamp: u64,
        session: Option<ObjectId>,
    ) -> Result<ObjectId> {
        let obj = Object::Change(Change {
            parents,
            tree,
            session,
            intent: intent.to_string(),
            author: author.to_string(),
            timestamp,
            verification: Verification::Unverified,
        });
        let id = obj.id();
        self.store.put(&obj)?;
        Ok(id)
    }

    /// Point the branch ref at `id` (used after replaying a DAG via [`Self::commit_tree_with_parents`]).
    pub fn set_branch(&self, id: ObjectId) -> Result<()> {
        self.store.set_ref(&self.branch, &id)
    }

    /// Snapshot `work_dir` and commit it in one step.
    pub fn commit_dir(
        &self,
        work_dir: &Path,
        intent: &str,
        author: &str,
        timestamp: u64,
        session: Option<ObjectId>,
    ) -> Result<ObjectId> {
        // Hold the shared commit lock across snapshot + ref-advance so a concurrent GC can't sweep
        // the just-written objects before they're referenced (see `Store::gc`).
        let _lock = self.store.commit_lock()?;
        let tracked = self.tracked_paths()?;
        let tree = snapshot::snapshot_tracked(&self.store, work_dir, &tracked)?;
        self.commit_tree(tree, intent, author, timestamp, session)
    }

    /// Like [`commit_dir`](Self::commit_dir) but bypasses the stat cache (always re-hashes).
    /// For committing distinct trees in rapid succession at the same paths — e.g. `keel
    /// import` replaying git history — where the mtime+size cache could racily false-hit.
    pub fn commit_dir_uncached(
        &self,
        work_dir: &Path,
        intent: &str,
        author: &str,
        timestamp: u64,
        session: Option<ObjectId>,
    ) -> Result<ObjectId> {
        let _lock = self.store.commit_lock()?; // see `commit_dir`
        let tracked = self.tracked_paths()?;
        let tree = snapshot::snapshot_uncached_tracked(&self.store, work_dir, &tracked)?;
        self.commit_tree(tree, intent, author, timestamp, session)
    }

    /// Load a change by address.
    pub fn change(&self, id: ObjectId) -> Result<Option<Change>> {
        Ok(match self.store.get(&id)? {
            Some(Object::Change(c)) => Some(c),
            _ => None,
        })
    }

    /// Materialize a change's tree onto `dir`.
    pub fn checkout_change(&self, id: ObjectId, dir: &Path) -> Result<()> {
        let c = self.change(id)?.ok_or(StoreError::Corrupt(id))?;
        snapshot::checkout(&self.store, c.tree, dir)
    }

    /// First-parent history from `HEAD`, newest first.
    pub fn log(&self) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        let mut cur = self.head()?;
        while let Some(id) = cur {
            out.push(id);
            cur = self.change(id)?.and_then(|c| c.parents.first().copied());
        }
        Ok(out)
    }

    /// All ancestors of `id` (including `id`), via every parent edge.
    pub fn ancestors(&self, id: ObjectId) -> Result<Vec<ObjectId>> {
        let mut seen = HashSet::new();
        let mut stack = vec![id];
        let mut out = Vec::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            out.push(x);
            if let Some(c) = self.change(x)? {
                for p in c.parents {
                    stack.push(p);
                }
            }
        }
        Ok(out)
    }

    /// Resolve a repo-relative `path` (e.g. `src/a.ts`) to the object id it points at in
    /// `change`'s tree — a blob for a file, a tree for a directory — or None if absent.
    pub fn file_at(&self, change: ObjectId, path: &str) -> Result<Option<ObjectId>> {
        match self.change(change)? {
            Some(c) => self.path_in_tree(c.tree, path),
            None => Ok(None),
        }
    }

    /// The raw bytes of `path` as of `change` (its blob content), or `None` if the path is
    /// absent or resolves to a directory. Chunked/deduped blobs are reassembled by the store.
    pub fn file_bytes_at(&self, change: ObjectId, path: &str) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.file_at(change, path)? else { return Ok(None) };
        match self.store.get(&id)? {
            Some(Object::Blob(bytes)) => Ok(Some(bytes)),
            _ => Ok(None), // a tree (directory) or missing object → not a file
        }
    }

    fn path_in_tree(&self, tree: ObjectId, path: &str) -> Result<Option<ObjectId>> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut cur = tree;
        for (i, part) in parts.iter().enumerate() {
            let t = match self.store.get(&cur)? {
                Some(Object::Tree(t)) => t,
                _ => return Ok(None),
            };
            let entry = match t.entries.iter().find(|e| &e.name == part) {
                Some(e) => e,
                None => return Ok(None),
            };
            if i == parts.len() - 1 {
                return Ok(Some(entry.id));
            }
            cur = entry.id;
        }
        Ok(None)
    }

    /// Working-tree status: how `work_dir` differs from `HEAD` (added / modified / deleted
    /// files), path-sorted. With no commits yet, every file reads as added.
    ///
    /// Note: this snapshots `work_dir` into the store to get its tree (content-addressed,
    /// so re-runs are near-free via dedup); the snapshot objects are unreferenced until a
    /// real commit and are reclaimed by GC — `status` never advances a ref.
    pub fn status(&self, work_dir: &Path) -> Result<Vec<PathChange>> {
        // Read-only snapshot: compute the work root + its trees in memory, writing nothing.
        let tracked = self.tracked_paths()?;
        let (work_root, work_trees) = snapshot::snapshot_ro_tracked(&self.store, work_dir, &tracked)?;
        let head_tree = match self.head()? {
            Some(h) => Some(self.change(h)?.ok_or(StoreError::Corrupt(h))?.tree),
            None => None,
        };
        // Clean fast path: identical roots ⇒ no changes, so skip the diff (and every tree read)
        // entirely. This is the common `status` on an unmodified checkout.
        if head_tree == Some(work_root) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.diff_trees(head_tree, Some(work_root), &work_trees, "", &mut out, 0)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// The entries of a tree object, or `[]` for `None` / a non-tree id.
    fn tree_entries(&self, tree: Option<ObjectId>) -> Result<Vec<TreeEntry>> {
        match tree {
            None => Ok(Vec::new()),
            Some(id) => match self.store.get(&id)? {
                Some(Object::Tree(t)) => Ok(t.entries),
                _ => Ok(Vec::new()),
            },
        }
    }

    /// Like [`Self::tree_entries`] but resolves work-side trees from an in-memory map first
    /// (the unpersisted trees from [`snapshot::snapshot_ro`]), falling back to the store for
    /// head-side trees. Ids are content-addressed, so a given id lives in exactly one place.
    fn tree_entries_src(
        &self,
        tree: Option<ObjectId>,
        extra: &HashMap<ObjectId, Tree>,
    ) -> Result<Vec<TreeEntry>> {
        match tree {
            Some(id) if extra.contains_key(&id) => Ok(extra[&id].entries.clone()),
            other => self.tree_entries(other),
        }
    }

    /// Recursively diff two (optional) trees, appending per-file changes under `prefix`.
    /// `extra` supplies the work-side trees that `status` never persisted.
    fn diff_trees(
        &self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        extra: &HashMap<ObjectId, Tree>,
        prefix: &str,
        out: &mut Vec<PathChange>,
        depth: u32,
    ) -> Result<()> {
        if depth > snapshot::MAX_TREE_DEPTH {
            return Err(StoreError::Corrupt(new.or(old).unwrap_or(ObjectId([0; 32]))));
        }
        // name → (in old?, in new?), each carrying (id, mode). Mode is kept in full (not reduced to
        // is_dir) so a mode-only change — e.g. `chmod +x`, or a file⇄symlink swap with equal bytes —
        // is still reported: it changes the tree id but not the blob id, so an id-only compare misses it.
        type Side = Option<(ObjectId, u32)>;
        let mut map: BTreeMap<String, (Side, Side)> = BTreeMap::new();
        for e in self.tree_entries_src(old, extra)? {
            map.entry(e.name).or_default().0 = Some((e.id, e.mode));
        }
        for e in self.tree_entries_src(new, extra)? {
            map.entry(e.name).or_default().1 = Some((e.id, e.mode));
        }
        for (name, (o, n)) in map {
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            match (o, n) {
                (None, Some((id, m))) => self.emit_side(id, m == snapshot::MODE_DIR, extra, &path, ChangeKind::Added, out, depth)?,
                (Some((id, m)), None) => self.emit_side(id, m == snapshot::MODE_DIR, extra, &path, ChangeKind::Deleted, out, depth)?,
                (Some((oid, om)), Some((nid, nm))) => {
                    let (od, nd) = (om == snapshot::MODE_DIR, nm == snapshot::MODE_DIR);
                    if oid == nid && om == nm {
                        continue; // identical subtree/blob and mode — content-addressing makes this exact
                    }
                    match (od, nd) {
                        (true, true) => self.diff_trees(Some(oid), Some(nid), extra, &path, out, depth + 1)?,
                        (false, false) => out.push(PathChange { path, kind: ChangeKind::Modified }),
                        // a file replaced a directory (or vice-versa): delete one side, add the other
                        _ => {
                            self.emit_side(oid, od, extra, &path, ChangeKind::Deleted, out, depth)?;
                            self.emit_side(nid, nd, extra, &path, ChangeKind::Added, out, depth)?;
                        }
                    }
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// Emit `kind` for a single entry: one line for a blob, or every blob beneath a dir.
    #[allow(clippy::too_many_arguments)] // recursive tree walker; the extra arg is the depth cap
    fn emit_side(
        &self,
        id: ObjectId,
        is_dir: bool,
        extra: &HashMap<ObjectId, Tree>,
        path: &str,
        kind: ChangeKind,
        out: &mut Vec<PathChange>,
        depth: u32,
    ) -> Result<()> {
        if depth > snapshot::MAX_TREE_DEPTH {
            return Err(StoreError::Corrupt(id));
        }
        if !is_dir {
            out.push(PathChange { path: path.to_string(), kind });
            return Ok(());
        }
        for e in self.tree_entries_src(Some(id), extra)? {
            let child = format!("{path}/{}", e.name);
            self.emit_side(e.id, e.mode == snapshot::MODE_DIR, extra, &child, kind, out, depth + 1)?;
        }
        Ok(())
    }

    /// First-parent history changes (newest first) that modified `path` — i.e. where the
    /// file's content differs from the first parent's (covers add / modify / remove).
    pub fn history_touching(&self, path: &str) -> Result<Vec<(ObjectId, Change)>> {
        let mut out = Vec::new();
        for id in self.log()? {
            let c = match self.change(id)? {
                Some(c) => c,
                None => continue,
            };
            let here = self.path_in_tree(c.tree, path)?;
            let parent_here = match c.parents.first() {
                Some(p) => match self.change(*p)? {
                    Some(pc) => self.path_in_tree(pc.tree, path)?,
                    None => None,
                },
                None => None,
            };
            if here != parent_here {
                out.push((id, c));
            }
        }
        Ok(out)
    }

    /// Every `(path, blob-id)` under `tree` (recursively), depth-first — the leaf inventory
    /// used by `repack` to build per-path version chains.
    fn tree_files(&self, tree: ObjectId, prefix: &str, out: &mut Vec<(String, ObjectId)>) -> Result<()> {
        self.tree_files_depth(tree, prefix, out, 0)
    }

    fn tree_files_depth(&self, tree: ObjectId, prefix: &str, out: &mut Vec<(String, ObjectId)>, depth: u32) -> Result<()> {
        if depth > snapshot::MAX_TREE_DEPTH {
            return Err(StoreError::Corrupt(tree));
        }
        for e in self.tree_entries(Some(tree))? {
            let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            if e.mode == snapshot::MODE_DIR {
                self.tree_files_depth(e.id, &path, out, depth + 1)?;
            } else {
                out.push((path, e.id));
            }
        }
        Ok(())
    }

    /// Delta-compress history in place: for each path, store each successive version as a
    /// prefix/suffix byte-delta against its previous version, keeping a full "keyframe" every
    /// [`KEYFRAME`] versions so any blob reconstructs in a bounded number of hops. This is the
    /// storage-only pass that narrows keel's gap to git's cross-object delta compression; it
    /// never changes an address (a delta is re-verified against its id on read) and never
    /// regresses size (`deltify` adopts a delta only when it beats the full compressed blob).
    ///
    /// Idempotent: a re-run finds already-deltified versions gone from `objects` and skips them.
    /// Run it after a bulk `import` or periodically, like `git gc`.
    pub fn repack(&self) -> Result<RepackStats> {
        let mut commits = self.log()?; // newest-first
        commits.reverse(); // walk oldest→newest so a base is an earlier version than its target
        let mut chains: HashMap<String, Vec<ObjectId>> = HashMap::new();
        // Global first-appearance rank per blob. Delta edges only ever point from a
        // higher-rank blob to a STRICTLY lower-rank one, so the delta graph is a DAG even when
        // the same blob recurs across commits in different orders (reverts, identical files, a
        // blob shared by two paths) — that acyclicity is what makes reconstruct terminate.
        let mut rank: HashMap<ObjectId, u64> = HashMap::new();
        let mut next_rank = 0u64;
        for cid in &commits {
            let tree = match self.change(*cid)? {
                Some(c) => c.tree,
                None => continue,
            };
            let mut files = Vec::new();
            self.tree_files(tree, "", &mut files)?;
            for (path, blob) in files {
                rank.entry(blob).or_insert_with(|| {
                    let r = next_rank;
                    next_rank += 1;
                    r
                });
                let v = chains.entry(path).or_default();
                if v.last() != Some(&blob) {
                    v.push(blob); // record only when the content actually changed
                }
            }
        }

        let mut stats = RepackStats::default();
        // ── Pass 1: same-path chains (each version deltas against its predecessor) ──────────
        for versions in chains.values() {
            for i in 1..versions.len() {
                if i % KEYFRAME == 0 {
                    continue; // leave a full keyframe so delta chains stay shallow (bounds depth)
                }
                let (target, base) = (&versions[i], &versions[i - 1]);
                // Only delta downward in rank — guarantees the acyclicity described above.
                if rank[base] >= rank[target] {
                    continue;
                }
                let saved = self.store.deltify(target, base)?;
                if saved > 0 {
                    stats.deltified += 1;
                    stats.bytes_saved += saved;
                }
            }
        }
        self.repack_similar(&rank, &mut stats)?;
        Ok(stats)
    }

    /// ── Pass 2: cross-path similarity ──────────────────────────────────────────────────────
    /// Delta each *remaining full* blob against a **similar, lower-rank, full** blob found via a
    /// MinHash sketch (base stays full → the delta is depth 1, so reads stay fast). Catches
    /// content the same-path pass can't — a file copied/renamed, generated files, near-duplicate
    /// configs/tests — that share no same-path history. Rank-downward + full-base keeps the delta
    /// graph an acyclic, shallow DAG.
    fn repack_similar(&self, rank: &HashMap<ObjectId, u64>, stats: &mut RepackStats) -> Result<()> {
        const SKETCH_K: usize = 24;
        const MIN_SHARED: u32 = 8; // require ~1/3 of the sketch to overlap before attempting

        // Full blobs that survived pass 1, ascending rank (so any base is processed before its
        // targets and is strictly lower-rank).
        let mut fulls: Vec<(u64, ObjectId, Vec<u8>)> = Vec::new();
        for (id, &rk) in rank {
            if let Some(bytes) = self.store.blob_bytes_if_full(id)? {
                fulls.push((rk, *id, bytes));
            }
        }
        fulls.sort_by_key(|(rk, _, _)| *rk);
        let sketches: Vec<Vec<u64>> =
            fulls.iter().map(|(_, _, b)| crate::delta::sketch(b, SKETCH_K)).collect();

        // sketch-hash → indices of already-seen full blobs eligible as bases
        let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
        let mut tally: HashMap<usize, u32> = HashMap::new();
        for t in 0..fulls.len() {
            tally.clear();
            for h in &sketches[t] {
                if let Some(cands) = index.get(h) {
                    for &c in cands {
                        *tally.entry(c).or_default() += 1;
                    }
                }
            }
            let best = tally.iter().max_by_key(|(_, n)| **n).map(|(&c, &n)| (c, n));
            let mut deltified = false;
            if let Some((c, shared)) = best {
                if shared >= MIN_SHARED {
                    let saved = self.store.deltify(&fulls[t].1, &fulls[c].1)?;
                    if saved > 0 {
                        stats.deltified += 1;
                        stats.bytes_saved += saved;
                        deltified = true; // now a delta — must NOT become a base for others
                    }
                }
            }
            if !deltified {
                for h in &sketches[t] {
                    index.entry(*h).or_default().push(t);
                }
            }
        }
        Ok(())
    }
}

/// Longest delta chain before a full keyframe — bounds reconstruct hops (and recursion depth).
const KEYFRAME: usize = 16;

/// What a [`Repo::repack`] pass accomplished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepackStats {
    /// Blobs re-stored as deltas this pass.
    pub deltified: u64,
    /// On-disk bytes reclaimed (sum of full-form size minus delta size).
    pub bytes_saved: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use std::fs;
    use std::sync::Arc;

    #[test]
    fn concurrent_commits_linearize_no_loss() {
        // N threads commit distinct trees to the SAME branch at once; the CAS + retry must
        // linearize them so every commit survives (the pre-CAS bug lost all but the last).
        let sd = TmpDir::new();
        let repo = Arc::new(Repo::open(sd.path()).unwrap());
        let n = 16u64;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let repo = Arc::clone(&repo);
                std::thread::spawn(move || {
                    let w = TmpDir::new();
                    fs::write(w.path().join("f.txt"), format!("commit {i}")).unwrap();
                    repo.commit_dir(w.path(), &format!("c{i}"), "a", i, None).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(repo.log().unwrap().len(), n as usize, "all {n} concurrent commits kept");
    }

    #[test]
    fn commit_chain_advances_head_and_links_parents() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        assert_eq!(repo.head().unwrap(), None);

        fs::write(work.path().join("f.txt"), b"v1").unwrap();
        let c1 = repo.commit_dir(work.path(), "first", "acct:x", 1, None).unwrap();
        assert_eq!(repo.head().unwrap(), Some(c1));

        fs::write(work.path().join("f.txt"), b"v2").unwrap();
        let c2 = repo.commit_dir(work.path(), "second", "acct:x", 2, None).unwrap();
        assert_eq!(repo.head().unwrap(), Some(c2));

        assert_ne!(c1, c2);
        assert_eq!(repo.log().unwrap(), vec![c2, c1]);
        assert_eq!(repo.change(c2).unwrap().unwrap().parents, vec![c1]);
        assert!(repo.change(c1).unwrap().unwrap().parents.is_empty());
    }

    #[test]
    fn repack_deltifies_history_and_reads_back_identically() {
        // A file edited across many commits (localized change each time) should delta-compress,
        // and EVERY historical version must still reconstruct byte-for-byte after repack.
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        let base: String = (0..2000).map(|i| format!("line {i}\n")).collect();

        let mut versions = Vec::new(); // (change-id, expected bytes)
        for v in 0..40 {
            // change only one line deep in the middle → long shared prefix + suffix
            let content = base.replace("line 1000\n", &format!("line 1000 EDIT v{v}\n"));
            fs::write(work.path().join("big.txt"), &content).unwrap();
            let c = repo.commit_dir(work.path(), &format!("v{v}"), "acct", v, None).unwrap();
            versions.push((c, content.into_bytes()));
        }

        let stats = repo.repack().unwrap();
        assert!(stats.deltified > 0, "repack should have deltified some versions");
        // 39 edits of a ~14KB file, each changing one line → each delta is a few hundred bytes
        // vs a multi-KB compressed blob; the saving should be most of the redundant history.
        assert!(stats.bytes_saved > 10_000, "expected real savings, got {}", stats.bytes_saved);

        // gc reclaims the now-orphaned full forms; EVERY version must still read back byte-identical
        // (delta bases stay reachable via the delta→base edge in the GC mark).
        repo.store().gc().unwrap();
        for (c, expected) in &versions {
            let got = repo.file_bytes_at(*c, "big.txt").unwrap().expect("version present");
            assert_eq!(&got, expected, "reconstructed bytes differ from what was committed");
        }
        // idempotent: a second repack finds the deltified versions gone from `objects` → no-op
        assert_eq!(repo.repack().unwrap().deltified, 0, "repack must be idempotent");
    }

    #[test]
    fn repack_survives_blobs_recurring_in_reverse_order() {
        // The cycle trap: two files swap two near-identical blobs between commits, so path A's
        // chain is [X,Y] and path B's is [Y,X]. Without the global-rank rule this produced a
        // delta cycle X→Y→X and reconstruct overflowed the stack. It must terminate and every
        // version must still read back exactly.
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        let x: String = (0..2000).map(|i| format!("x line {i}\n")).collect();
        let y = x.replace("x line 1000\n", "x line 1000 DIFFERENT\n"); // near-identical → deltafiable

        fs::write(work.path().join("a.txt"), &x).unwrap();
        fs::write(work.path().join("b.txt"), &y).unwrap();
        let c1 = repo.commit_dir(work.path(), "c1", "acct", 1, None).unwrap();
        fs::write(work.path().join("a.txt"), &y).unwrap(); // swap
        fs::write(work.path().join("b.txt"), &x).unwrap();
        let c2 = repo.commit_dir(work.path(), "c2", "acct", 2, None).unwrap();

        repo.repack().unwrap(); // must not overflow
        repo.store().gc().unwrap();
        assert_eq!(repo.file_bytes_at(c1, "a.txt").unwrap().unwrap(), x.as_bytes());
        assert_eq!(repo.file_bytes_at(c1, "b.txt").unwrap().unwrap(), y.as_bytes());
        assert_eq!(repo.file_bytes_at(c2, "a.txt").unwrap().unwrap(), y.as_bytes());
        assert_eq!(repo.file_bytes_at(c2, "b.txt").unwrap().unwrap(), x.as_bytes());
    }

    #[test]
    fn repack_cross_path_similarity_deltifies_a_near_duplicate() {
        // Two files that share NO same-path history but are near-identical (a copy with one edit).
        // The same-path pass can't touch them; the cross-path similarity pass must delta the
        // later one against the earlier, and both must read back exactly.
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        let a: String = (0..1500).map(|i| format!("shared line {i}\n")).collect();
        let b = a.replace("shared line 700\n", "shared line 700 -- the one change\n");

        fs::write(work.path().join("original.txt"), &a).unwrap();
        let c1 = repo.commit_dir(work.path(), "add original", "acct", 1, None).unwrap();
        fs::write(work.path().join("copy.txt"), &b).unwrap(); // a near-dup at a different path
        let c2 = repo.commit_dir(work.path(), "add near-dup copy", "acct", 2, None).unwrap();

        let before = repo.store().delta_count().unwrap();
        repo.repack().unwrap();
        assert!(
            repo.store().delta_count().unwrap() > before,
            "cross-path similarity pass should have deltified the near-duplicate"
        );
        repo.store().gc().unwrap();
        assert_eq!(repo.file_bytes_at(c1, "original.txt").unwrap().unwrap(), a.as_bytes());
        assert_eq!(repo.file_bytes_at(c2, "copy.txt").unwrap().unwrap(), b.as_bytes());
    }

    #[test]
    fn repack_never_regresses_on_scattered_edits() {
        // When edits are scattered (no long shared ends), the delta can't beat the whole blob,
        // so repack must adopt zero deltas rather than storing something larger.
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        for v in 0..5u64 {
            // completely different content each commit → no meaningful common prefix/suffix
            let content: String = (0..500).map(|i| format!("{v}-{}-{i}\n", (i * 7 + v as usize) % 97)).collect();
            fs::write(work.path().join("f.txt"), content).unwrap();
            repo.commit_dir(work.path(), &format!("v{v}"), "acct", v, None).unwrap();
        }
        let stats = repo.repack().unwrap();
        assert_eq!(stats.deltified, 0, "scattered edits must not be deltified (would grow the store)");
    }

    #[test]
    fn identical_working_tree_commits_are_content_stable() {
        // same tree + same metadata → same change address (deterministic commits)
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::write(work.path().join("f"), b"x").unwrap();
        let tree = snapshot::snapshot(repo.store(), work.path()).unwrap();
        let a = repo.commit_tree(tree, "msg", "acct", 42, None).unwrap();
        // re-commit the same tree from no-parent state in a fresh repo → same id
        let sd2 = TmpDir::new();
        let repo2 = Repo::open(sd2.path()).unwrap();
        let b = repo2.commit_tree(tree, "msg", "acct", 42, None).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tracked_file_is_not_ignored_by_a_later_gitignore() {
        // git only ignores UNtracked paths: once a file is committed, adding a .gitignore that
        // matches it must not make status treat it as deleted (regression for the force-added /
        // vendored-file data-loss bug).
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::write(work.path().join("app.min.js"), b"vendored\n").unwrap();
        repo.commit_dir(work.path(), "add vendored", "acct", 1, None).unwrap();
        // now introduce an ignore rule that would match the already-tracked file
        fs::write(work.path().join(".gitignore"), b"*.min.js\n").unwrap();
        repo.commit_dir(work.path(), "add gitignore", "acct", 2, None).unwrap();

        // the tracked file is present and unmodified → status must be clean, not "deleted"
        let st = repo.status(work.path()).unwrap();
        assert!(st.is_empty(), "tracked-but-ignored file wrongly reported: {st:?}");
        // and it must still be tracked in HEAD
        let head = repo.head().unwrap().unwrap();
        assert!(repo.file_at(head, "app.min.js").unwrap().is_some(), "tracked file dropped from HEAD");
    }

    #[test]
    #[cfg(unix)]
    fn status_reports_a_mode_only_change() {
        use std::os::unix::fs::PermissionsExt;
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        let f = work.path().join("run.sh");
        fs::write(&f, b"#!/bin/sh\necho hi\n").unwrap();
        repo.commit_dir(work.path(), "add", "acct", 1, None).unwrap();

        // chmod +x: identical bytes (same blob id) but a new mode → the tree id changes while the
        // blob id does not. A blob-id-only diff would miss this and report the tree as clean.
        let mut perm = fs::metadata(&f).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&f, perm).unwrap();

        let st = repo.status(work.path()).unwrap();
        assert!(
            st.iter().any(|c| c.path == "run.sh" && matches!(c.kind, ChangeKind::Modified)),
            "chmod +x must be reported as a modification, got {st:?}"
        );
    }

    #[test]
    fn checkout_change_restores_tree() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let out = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::create_dir_all(work.path().join("d")).unwrap();
        fs::write(work.path().join("d/a.txt"), b"content").unwrap();
        let c = repo.commit_dir(work.path(), "c", "acct", 1, None).unwrap();

        repo.checkout_change(c, out.path()).unwrap();
        assert_eq!(fs::read(out.path().join("d/a.txt")).unwrap(), b"content");
    }

    #[test]
    fn history_touching_tracks_per_file_changes() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::write(work.path().join("a"), b"1").unwrap();
        fs::write(work.path().join("b"), b"x").unwrap();
        let c1 = repo.commit_dir(work.path(), "add", "acct", 1, None).unwrap();
        fs::write(work.path().join("a"), b"2").unwrap();
        let c2 = repo.commit_dir(work.path(), "edit a", "acct", 2, None).unwrap();
        fs::write(work.path().join("b"), b"y").unwrap();
        let c3 = repo.commit_dir(work.path(), "edit b", "acct", 3, None).unwrap();

        let a_hist: Vec<_> = repo.history_touching("a").unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(a_hist, vec![c2, c1], "a: added at c1, modified at c2, untouched at c3");
        let b_hist: Vec<_> = repo.history_touching("b").unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(b_hist, vec![c3, c1], "b: added at c1, modified at c3");

        assert!(repo.file_at(c2, "a").unwrap().is_some());
        assert_eq!(repo.file_at(c2, "missing").unwrap(), None);
    }

    #[test]
    fn status_reports_add_modify_delete_vs_head() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();

        // no HEAD yet → every file reads as added, nested dirs walked
        fs::create_dir_all(work.path().join("src")).unwrap();
        fs::write(work.path().join("src/a.ts"), b"1").unwrap();
        fs::write(work.path().join("src/b.ts"), b"1").unwrap();
        fs::write(work.path().join("keep.ts"), b"k").unwrap();
        // keel's own metadata dir must never show up in status (it's ignored like .git);
        // the exact-match assertions below would fail if `.keel/...` leaked in.
        fs::create_dir_all(work.path().join(".keel/store")).unwrap();
        fs::write(work.path().join(".keel/store/data.mdb"), b"db").unwrap();
        let pre: Vec<_> = repo.status(work.path()).unwrap();
        assert_eq!(
            pre.iter().map(|c| (c.kind.marker(), c.path.as_str())).collect::<Vec<_>>(),
            vec![('A', "keep.ts"), ('A', "src/a.ts"), ('A', "src/b.ts")],
            "no-HEAD status = everything added, path-sorted"
        );

        repo.commit_dir(work.path(), "init", "acct", 1, None).unwrap();
        // clean tree right after commit
        assert!(repo.status(work.path()).unwrap().is_empty(), "no diff immediately after commit");

        // now: modify one, add one, delete one (keep one untouched)
        fs::write(work.path().join("src/a.ts"), b"2").unwrap(); // modified
        fs::write(work.path().join("src/c.ts"), b"new").unwrap(); // added
        fs::remove_file(work.path().join("src/b.ts")).unwrap(); // deleted
        let st: Vec<_> =
            repo.status(work.path()).unwrap().iter().map(|c| (c.kind.marker(), c.path.clone())).collect();
        assert_eq!(
            st,
            vec![
                ('M', "src/a.ts".to_string()),
                ('D', "src/b.ts".to_string()),
                ('A', "src/c.ts".to_string()),
            ],
            "keep.ts unchanged & omitted; a modified, b deleted, c added"
        );
    }

    #[test]
    fn file_bytes_at_returns_blob_content() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::create_dir_all(work.path().join("d")).unwrap();
        fs::write(work.path().join("d/a.txt"), b"hello bytes").unwrap();
        let c = repo.commit_dir(work.path(), "c", "acct", 1, None).unwrap();

        assert_eq!(repo.file_bytes_at(c, "d/a.txt").unwrap().as_deref(), Some(&b"hello bytes"[..]));
        assert_eq!(repo.file_bytes_at(c, "d/missing.txt").unwrap(), None, "absent path → None");
        assert_eq!(repo.file_bytes_at(c, "d").unwrap(), None, "a directory is not a file → None");
    }

    #[test]
    fn ancestors_covers_history() {
        let sd = TmpDir::new();
        let work = TmpDir::new();
        let repo = Repo::open(sd.path()).unwrap();
        fs::write(work.path().join("f"), b"1").unwrap();
        let c1 = repo.commit_dir(work.path(), "1", "a", 1, None).unwrap();
        fs::write(work.path().join("f"), b"2").unwrap();
        let c2 = repo.commit_dir(work.path(), "2", "a", 2, None).unwrap();

        let anc = repo.ancestors(c2).unwrap();
        assert!(anc.contains(&c1) && anc.contains(&c2));
        assert_eq!(repo.ancestors(c1).unwrap(), vec![c1]);
    }
}
