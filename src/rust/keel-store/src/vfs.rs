//! A lazy, read-only filesystem view of a change's tree, served straight from the content-addressed
//! store.
//!
//! This is the substrate for `keel mount`. Constructing a [`Vfs`] reads only the root tree id;
//! directory listings and file contents are fetched from the store on access. So the cost to "mount"
//! a repo is O(1) in its size — a million-file checkout opens as cheaply as a 200-file one — and only
//! the files a reader actually opens are ever fetched. The transport that turns this into a real
//! kernel filesystem (FUSE on Linux, an NFS loopback on macOS, which needs no kernel extension) is a
//! thin adapter over the four operations here: [`Vfs::getattr`], [`Vfs::readdir`], [`Vfs::read_at`],
//! and [`Vfs::readlink`].
//!
//! One caveat worth stating: a file's size in [`Vfs::getattr`] currently requires reading its blob,
//! because the chunk manifest stores chunk ids but not a total length. That keeps *whole-repo*
//! laziness (only accessed paths are touched) but not *per-stat* laziness, which a high-throughput
//! mount would want. The fix is a size index in the store; it is deliberately out of scope here so
//! the read core can land and be exercised first.

use crate::snapshot::{MAX_TREE_DEPTH, MODE_DIR, MODE_SYMLINK};
use crate::store::{Result, Store};
use crate::{Object, ObjectId, Tree};
use std::sync::atomic::{AtomicU64, Ordering};

/// What a path resolves to in the tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Dir,
    File,
    Symlink,
}

/// File metadata for a `stat`, resolved lazily (only the trees along the path are read).
#[derive(Clone, Debug)]
pub struct Attr {
    pub kind: NodeKind,
    pub mode: u32,
    pub size: u64,
}

/// One entry in a directory listing.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub kind: NodeKind,
    pub mode: u32,
    pub id: ObjectId,
}

/// Counts of what a view actually fetched — the evidence that access is lazy. A freshly constructed
/// [`Vfs`] has read nothing; each resolved path adds only the trees it walked and the blobs it opened.
#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub trees_read: u64,
    pub blobs_read: u64,
    pub bytes_read: u64,
}

/// A lazy, read-only view of one change's tree.
pub struct Vfs<'a> {
    store: &'a Store,
    root: ObjectId,
    trees_read: AtomicU64,
    blobs_read: AtomicU64,
    bytes_read: AtomicU64,
}

fn kind_of(mode: u32) -> NodeKind {
    if mode == MODE_DIR {
        NodeKind::Dir
    } else if mode == MODE_SYMLINK {
        NodeKind::Symlink
    } else {
        NodeKind::File
    }
}

impl<'a> Vfs<'a> {
    /// A lazy view rooted at `root_tree` (a change's `tree`). Reads nothing yet.
    pub fn new(store: &'a Store, root_tree: ObjectId) -> Self {
        Vfs {
            store,
            root: root_tree,
            trees_read: AtomicU64::new(0),
            blobs_read: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        }
    }

    /// What this view has fetched so far — proof that only accessed paths were touched.
    pub fn stats(&self) -> Stats {
        Stats {
            trees_read: self.trees_read.load(Ordering::Relaxed),
            blobs_read: self.blobs_read.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
        }
    }

    fn read_tree(&self, id: ObjectId) -> Result<Option<Tree>> {
        match self.store.get(&id)? {
            Some(Object::Tree(t)) => {
                self.trees_read.fetch_add(1, Ordering::Relaxed);
                Ok(Some(t))
            }
            _ => Ok(None),
        }
    }

    fn read_blob(&self, id: ObjectId) -> Result<Option<Vec<u8>>> {
        match self.store.get(&id)? {
            Some(Object::Blob(b)) => {
                self.blobs_read.fetch_add(1, Ordering::Relaxed);
                self.bytes_read.fetch_add(b.len() as u64, Ordering::Relaxed);
                Ok(Some(b))
            }
            _ => Ok(None),
        }
    }

    /// Resolve a repo-relative path to `(id, mode)`, or `None` if absent. `""` / `"/"` is the root
    /// directory. Only the trees along the path are read, and the walk is bounded by
    /// [`MAX_TREE_DEPTH`] so a crafted deep path can't run away.
    fn resolve(&self, path: &str) -> Result<Option<(ObjectId, u32)>> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Ok(Some((self.root, MODE_DIR)));
        }
        if parts.len() as u32 > MAX_TREE_DEPTH {
            return Ok(None);
        }
        let mut cur = self.root;
        for (i, part) in parts.iter().enumerate() {
            let Some(t) = self.read_tree(cur)? else { return Ok(None) };
            let Some(e) = t.entries.iter().find(|e| e.name == *part) else { return Ok(None) };
            if i + 1 == parts.len() {
                return Ok(Some((e.id, e.mode)));
            }
            if e.mode != MODE_DIR {
                return Ok(None); // a non-directory can't have children
            }
            cur = e.id;
        }
        Ok(None)
    }

    /// `stat` a path.
    pub fn getattr(&self, path: &str) -> Result<Option<Attr>> {
        let Some((id, mode)) = self.resolve(path)? else { return Ok(None) };
        let kind = kind_of(mode);
        let size = match kind {
            NodeKind::Dir => 0,
            NodeKind::File | NodeKind::Symlink => {
                self.read_blob(id)?.map(|b| b.len() as u64).unwrap_or(0)
            }
        };
        Ok(Some(Attr { kind, mode, size }))
    }

    /// List a directory (path-sorted), or `None` if `path` is absent or not a directory.
    pub fn readdir(&self, path: &str) -> Result<Option<Vec<DirEntry>>> {
        let Some((id, mode)) = self.resolve(path)? else { return Ok(None) };
        if mode != MODE_DIR {
            return Ok(None);
        }
        let Some(t) = self.read_tree(id)? else { return Ok(None) };
        let mut out: Vec<DirEntry> = t
            .entries
            .into_iter()
            .map(|e| DirEntry { name: e.name, kind: kind_of(e.mode), mode: e.mode, id: e.id })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(out))
    }

    /// Read a whole file, or `None` if the path is absent or a directory.
    pub fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let Some((id, mode)) = self.resolve(path)? else { return Ok(None) };
        if mode == MODE_DIR {
            return Ok(None);
        }
        self.read_blob(id)
    }

    /// Read the byte range `[offset, offset + len)` of a file, clamped to its end. A real mount calls
    /// this per `read(2)`; today it fetches the whole blob and slices (streaming range reads are a
    /// follow-up that pairs with the size index).
    pub fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Option<Vec<u8>>> {
        let Some(bytes) = self.read(path)? else { return Ok(None) };
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(len).min(bytes.len());
        Ok(Some(bytes[start..end].to_vec()))
    }

    /// The target of a symlink, or `None` if `path` isn't one. Targets are stored as the blob content
    /// (see [`crate::snapshot`]).
    pub fn readlink(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let Some((id, mode)) = self.resolve(path)? else { return Ok(None) };
        if mode != MODE_SYMLINK {
            return Ok(None);
        }
        self.read_blob(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{MODE_FILE, MODE_SYMLINK};
    use crate::testutil::TmpDir;
    use crate::TreeEntry;

    /// Build a small tree in a fresh store and return `(tmp, store, root_tree_id)`:
    /// ```text
    /// /
    /// ├── README        (file, "hello\n")
    /// ├── link          (symlink -> README)
    /// └── src/
    ///     └── main.rs   (file, "fn main() {}\n")
    /// ```
    fn fixture() -> (TmpDir, Store, ObjectId) {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let readme = store.put(&Object::Blob(b"hello\n".to_vec())).unwrap();
        let link = store.put(&Object::Blob(b"README".to_vec())).unwrap();
        let main = store.put(&Object::Blob(b"fn main() {}\n".to_vec())).unwrap();

        let src = store
            .put(&Object::Tree(Tree {
                entries: vec![TreeEntry { name: "main.rs".into(), mode: MODE_FILE, id: main }],
            }))
            .unwrap();
        let root = store
            .put(&Object::Tree(Tree {
                entries: vec![
                    TreeEntry { name: "README".into(), mode: MODE_FILE, id: readme },
                    TreeEntry { name: "link".into(), mode: MODE_SYMLINK, id: link },
                    TreeEntry { name: "src".into(), mode: MODE_DIR, id: src },
                ],
            }))
            .unwrap();
        (dir, store, root)
    }

    #[test]
    fn construction_reads_nothing_then_access_is_lazy() {
        let (_d, store, root) = fixture();
        let vfs = Vfs::new(&store, root);
        // Nothing fetched until we ask for something.
        let s0 = vfs.stats();
        assert_eq!((s0.trees_read, s0.blobs_read), (0, 0));

        // Reading one nested file walks exactly the two trees on its path (root, src) and one blob —
        // NOT the README/link blobs, which we never opened. This is the whole-repo-laziness property.
        let bytes = vfs.read("src/main.rs").unwrap().unwrap();
        assert_eq!(bytes, b"fn main() {}\n");
        let s1 = vfs.stats();
        assert_eq!(s1.trees_read, 2, "walked root + src only");
        assert_eq!(s1.blobs_read, 1, "opened one blob, not the sibling files");
        assert_eq!(s1.bytes_read, b"fn main() {}\n".len() as u64);
    }

    #[test]
    fn readdir_lists_root_sorted_with_kinds() {
        let (_d, store, root) = fixture();
        let vfs = Vfs::new(&store, root);
        let ents = vfs.readdir("").unwrap().unwrap();
        let names: Vec<&str> = ents.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["README", "link", "src"]);
        assert_eq!(ents[0].kind, NodeKind::File);
        assert_eq!(ents[1].kind, NodeKind::Symlink);
        assert_eq!(ents[2].kind, NodeKind::Dir);
        // readdir on a file is not a directory listing
        assert!(vfs.readdir("README").unwrap().is_none());
    }

    #[test]
    fn getattr_reports_kind_and_size() {
        let (_d, store, root) = fixture();
        let vfs = Vfs::new(&store, root);
        let a = vfs.getattr("README").unwrap().unwrap();
        assert_eq!(a.kind, NodeKind::File);
        assert_eq!(a.size, 6);
        let d = vfs.getattr("src").unwrap().unwrap();
        assert_eq!(d.kind, NodeKind::Dir);
        assert!(vfs.getattr("does/not/exist").unwrap().is_none());
    }

    #[test]
    fn read_at_slices_and_clamps() {
        let (_d, store, root) = fixture();
        let vfs = Vfs::new(&store, root);
        assert_eq!(vfs.read_at("README", 0, 3).unwrap().unwrap(), b"hel");
        assert_eq!(vfs.read_at("README", 3, 100).unwrap().unwrap(), b"lo\n"); // len clamps to EOF
        assert_eq!(vfs.read_at("README", 999, 4).unwrap().unwrap(), b""); // offset past EOF → empty
        // a directory has no bytes
        assert!(vfs.read_at("src", 0, 10).unwrap().is_none());
    }

    #[test]
    fn readlink_returns_target_only_for_symlinks() {
        let (_d, store, root) = fixture();
        let vfs = Vfs::new(&store, root);
        assert_eq!(vfs.readlink("link").unwrap().unwrap(), b"README");
        assert!(vfs.readlink("README").unwrap().is_none()); // a regular file is not a symlink
    }
}
