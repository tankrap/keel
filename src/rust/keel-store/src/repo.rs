//! The change DAG + refs on top of the object store — a minimal but real VCS core.
//!
//! A [`Repo`] tracks a `HEAD`-like branch ref (`main`). Committing snapshots the working
//! tree into content-addressed objects, then atomically writes the `change` and advances
//! the branch in one transaction (see [`Store::apply`]) — so `HEAD` never points at a
//! half-written commit. Snapshot objects are written first and become durable before the
//! ref advances; a crash mid-snapshot leaves only unreachable objects (future GC), never a
//! dangling HEAD.

use crate::object::{Change, Object, ObjectId, Verification};
use crate::snapshot;
use crate::store::{Result, Store, StoreError};
use std::collections::HashSet;
use std::path::Path;

pub struct Repo {
    store: Store,
    branch: String,
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
        let parents = self.head()?.into_iter().collect::<Vec<_>>();
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
        // fused commit: the change lands and the branch advances atomically
        self.store.apply(&[obj], &[(self.branch.as_str(), id)])?;
        Ok(id)
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
        let tree = snapshot::snapshot(&self.store, work_dir)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;
    use std::fs;

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
