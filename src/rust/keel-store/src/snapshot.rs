//! Working-tree ⇄ object-store bridge: snapshot a directory into content-addressed
//! trees + blobs, and materialize a tree back onto disk.
//!
//! Snapshot is deterministic — the same directory content always yields the same root
//! tree address, regardless of filesystem enumeration order (tree entries sort by name
//! on encode) — so `snapshot ∘ checkout ∘ snapshot` is the identity on the address.
//! Identical file content deduplicates to one blob.

use crate::object::{Object, ObjectId, Tree, TreeEntry};
use crate::store::{Result, Store, StoreError};
use std::fs;
use std::path::Path;

pub const MODE_FILE: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
pub const MODE_DIR: u32 = 0o040000;

/// Names skipped during snapshot — build artifacts + VCS internals. A real ignore policy
/// (.gitignore / .keelignore) is a follow-up; this is a sane default so snapshotting a repo
/// root doesn't ingest `node_modules`, `.git`, etc.
const IGNORED: &[&str] = &["node_modules", ".git", "target", "dist", "build"];

/// Snapshot `dir` recursively, returning the root tree's address. Symlinks and other
/// non-regular files are skipped (a real ignore policy comes later).
pub fn snapshot(store: &Store, dir: &Path) -> Result<ObjectId> {
    // Build every object in memory first — ids are pure functions of content, so no
    // store round-trips are needed to compute them — then commit the whole tree in ONE
    // atomic transaction. A snapshot is all-or-nothing, and this avoids a per-file fsync
    // (per-file transactions measured ~40x slower). For very large trees a future
    // optimization can flush the batch once accumulated bytes cross a threshold.
    let mut objs = Vec::new();
    let root = build(dir, &mut objs)?;
    store.put_many(&objs)?;
    Ok(root)
}

fn build(dir: &Path, objs: &mut Vec<Object>) -> Result<ObjectId> {
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
            let id = build(&de.path(), objs)?;
            entries.push(TreeEntry { name, mode: MODE_DIR, id });
        } else if ft.is_file() {
            let bytes = fs::read(de.path())?;
            let blob = Object::Blob(bytes);
            let id = blob.id();
            objs.push(blob);
            entries.push(TreeEntry { name, mode: file_mode(&de)?, id });
        }
    }
    // encode() sorts entries by name, so id is order-independent.
    let tree = Object::Tree(Tree { entries });
    let id = tree.id();
    objs.push(tree);
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
fn file_mode(de: &fs::DirEntry) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let m = de.metadata()?;
    Ok(if m.permissions().mode() & 0o111 != 0 { MODE_EXEC } else { MODE_FILE })
}
#[cfg(not(unix))]
fn file_mode(_de: &fs::DirEntry) -> Result<u32> {
    Ok(MODE_FILE)
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
