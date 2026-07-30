//! git history → keel's native fused-graph model (epic NEW-1113, "M4-deep").
//!
//! The mirror ([`crate::mirror`]) holds git objects verbatim, which is enough to regenerate a
//! byte-identical `.git` — but keel's own value (provenance, `history_touching`, `keel native
//! log`, the brief's history section) runs on keel-native **Change/Tree** objects. This module
//! builds those from the mirrored git commits, so after an import *everything in keel works* on
//! the imported history — not just the git round-trip.
//!
//! It walks commits parents-first, converts each git tree into a keel tree (sharing subtrees via
//! a cache — the whole point, since git trees are massively shared across commits), builds a keel
//! `Change` per commit with mapped parents, and points keel's `main` ref at the git HEAD commit.
//! keel object ids are content addresses, so they're computed up front and the objects are
//! **batch-written** (a keel-scale tree count would otherwise be one fsync per object).

use crate::{parse_tree, Headed, Kind, Oid};
use keel_store::object::{Change, Object, ObjectId, Tree, TreeEntry, Verification};
use keel_store::snapshot::{MODE_DIR, MODE_EXEC, MODE_FILE};
use keel_store::store::Store;
use std::collections::HashMap;
use std::io;

const NS_OBJ: &str = "gobj";
const NS_BLOB: &str = "gblob";
const NS_REF: &str = "gref";
const NS_CHANGE: &str = "gchange"; // git-commit-oid(20) → keel change id(32)

#[derive(Debug, Default, Clone, Copy)]
pub struct BridgeStats {
    pub commits: u64,
    pub trees: u64,
}

/// Build keel-native Change/Tree history from the mirrored git commits. Idempotent: keel ids are
/// content addresses, so re-running produces the same objects (already-present writes are no-ops).
pub fn bridge(store: &Store) -> io::Result<BridgeStats> {
    // Preload the mirror maps once (on-demand `aux_get` per object would be far too many txns).
    let mut blob_map: HashMap<[u8; 20], ObjectId> = HashMap::new();
    for (k, v) in store.aux_iter(NS_BLOB).map_err(to_io)? {
        if k.len() == 20 && v.len() == 32 {
            let (mut a, mut b) = ([0u8; 20], [0u8; 32]);
            a.copy_from_slice(&k);
            b.copy_from_slice(&v);
            blob_map.insert(a, ObjectId(b));
        }
    }
    let mut obj_map: HashMap<[u8; 20], (Kind, Vec<u8>)> = HashMap::new();
    for (k, v) in store.aux_iter(NS_OBJ).map_err(to_io)? {
        if k.len() == 20 && !v.is_empty() {
            if let Some(kind) = kind_untag(v[0]) {
                let mut a = [0u8; 20];
                a.copy_from_slice(&k);
                obj_map.insert(a, (kind, v[1..].to_vec()));
            }
        }
    }

    let commit_oids: Vec<[u8; 20]> =
        obj_map.iter().filter(|(_, (k, _))| *k == Kind::Commit).map(|(o, _)| *o).collect();

    let mut tree_cache: HashMap<[u8; 20], ObjectId> = HashMap::new();
    let mut change_map: HashMap<[u8; 20], ObjectId> = HashMap::new();
    let mut pending: Vec<Object> = Vec::new();
    let mut stats = BridgeStats::default();

    // Iterative post-order over the commit DAG so a commit's parents are mapped before it. (A
    // shallow-clone boundary commit simply has parents that aren't in the set — they're skipped.)
    for start in &commit_oids {
        if change_map.contains_key(start) {
            continue;
        }
        let mut stack = vec![(*start, false)];
        while let Some((o, processed)) = stack.pop() {
            if change_map.contains_key(&o) {
                continue;
            }
            let (kind, payload) = match obj_map.get(&o) {
                Some(x) => x,
                None => continue,
            };
            if *kind != Kind::Commit {
                continue;
            }
            let headed = Headed::parse(payload);
            let parents: Vec<[u8; 20]> =
                headed.parents().unwrap_or_default().iter().map(|p| *p.as_bytes()).collect();
            if !processed {
                stack.push((o, true));
                for p in &parents {
                    let present = obj_map.get(p).map(|(k, _)| *k == Kind::Commit).unwrap_or(false);
                    if present && !change_map.contains_key(p) {
                        stack.push((*p, false));
                    }
                }
            } else {
                let git_tree = headed.tree().map_err(|_| bad("commit has no tree"))?;
                let keel_tree = build_keel_tree(
                    git_tree.as_bytes(),
                    &obj_map,
                    &blob_map,
                    &mut tree_cache,
                    &mut pending,
                    &mut stats,
                )?;
                let mapped_parents: Vec<ObjectId> =
                    parents.iter().filter_map(|p| change_map.get(p).copied()).collect();
                let (author, timestamp) = parse_author(&headed);
                let intent = String::from_utf8_lossy(&headed.message).trim_end().to_string();
                let change = Object::Change(Change {
                    parents: mapped_parents,
                    tree: keel_tree,
                    session: None,
                    intent,
                    author,
                    timestamp,
                    verification: Verification::Unverified,
                });
                change_map.insert(o, change.id());
                pending.push(change);
                stats.commits += 1;
                if pending.len() >= 4096 {
                    store.put_many(&pending).map_err(to_io)?;
                    pending.clear();
                }
            }
        }
    }
    store.put_many(&pending).map_err(to_io)?;

    // Point keel's `main` ref at the git HEAD commit's keel change, so `keel native log` / brief
    // provenance have a head to walk.
    if let Some(head_oid) = resolve_head(store)? {
        if let Some(change) = change_map.get(head_oid.as_bytes()) {
            store.set_ref("main", change).map_err(to_io)?;
        }
    }
    // Persist the git-commit → keel-change map for cross-reference / incremental re-bridge.
    let items: Vec<(Vec<u8>, Vec<u8>)> =
        change_map.iter().map(|(o, c)| (o.to_vec(), c.0.to_vec())).collect();
    store.aux_put_many(NS_CHANGE, &items).map_err(to_io)?;

    Ok(stats)
}

/// Convert a git tree (and its subtrees) into a keel tree, reusing already-built subtrees via
/// `cache` (git-tree-oid → keel-tree-id). Objects are pushed to `pending` for a batched write.
fn build_keel_tree(
    git_tree_oid: &[u8; 20],
    obj_map: &HashMap<[u8; 20], (Kind, Vec<u8>)>,
    blob_map: &HashMap<[u8; 20], ObjectId>,
    cache: &mut HashMap<[u8; 20], ObjectId>,
    pending: &mut Vec<Object>,
    stats: &mut BridgeStats,
) -> io::Result<ObjectId> {
    if let Some(id) = cache.get(git_tree_oid) {
        return Ok(*id);
    }
    let (kind, payload) = obj_map.get(git_tree_oid).ok_or_else(|| bad("missing tree in mirror"))?;
    if *kind != Kind::Tree {
        return Err(bad("expected a tree object"));
    }
    let entries = parse_tree(payload).map_err(|_| bad("unparsable tree"))?;
    let mut keel_entries = Vec::with_capacity(entries.len());
    for e in entries {
        let name = String::from_utf8_lossy(&e.name).into_owned();
        let (mode, id) = if e.mode == b"40000" {
            (MODE_DIR, build_keel_tree(e.oid.as_bytes(), obj_map, blob_map, cache, pending, stats)?)
        } else if e.mode == b"160000" {
            continue; // gitlink / submodule: points at a commit, not a blob — nothing to store
        } else {
            // 100644 file, 100755 exec, 120000 symlink (keel stores the target as file content)
            let mode = if e.mode == b"100755" { MODE_EXEC } else { MODE_FILE };
            match blob_map.get(e.oid.as_bytes()) {
                Some(id) => (mode, *id),
                None => continue, // blob absent from mirror (shouldn't happen for a full ingest)
            }
        };
        keel_entries.push(TreeEntry { name, mode, id });
    }
    let obj = Object::Tree(Tree { entries: keel_entries });
    let id = obj.id();
    cache.insert(*git_tree_oid, id);
    pending.push(obj);
    stats.trees += 1;
    Ok(id)
}

/// Parse a git `author`/`committer` header (`Name <email> unixtime tz`) into keel's
/// `(author, timestamp)`.
fn parse_author(h: &Headed) -> (String, u64) {
    let line = h.header_value("author").or_else(|| h.header_value("committer"));
    match line {
        Some(bytes) => {
            let s = String::from_utf8_lossy(bytes);
            if let Some(gt) = s.rfind('>') {
                let who = s[..=gt].trim().to_string();
                let ts = s[gt + 1..].split_whitespace().next().and_then(|t| t.parse().ok()).unwrap_or(0);
                (who, ts)
            } else {
                (s.trim().to_string(), 0)
            }
        }
        None => ("unknown".to_string(), 0),
    }
}

/// Resolve the mirror's HEAD (possibly symbolic) to a commit oid.
fn resolve_head(store: &Store) -> io::Result<Option<Oid>> {
    let head = match store.aux_get(NS_REF, b"HEAD").map_err(to_io)? {
        Some(v) => v,
        None => return Ok(None),
    };
    let s = String::from_utf8_lossy(&head).trim().to_string();
    let hex = if let Some(target) = s.strip_prefix("ref: ") {
        match store.aux_get(NS_REF, target.as_bytes()).map_err(to_io)? {
            Some(v) => String::from_utf8_lossy(&v).trim().to_string(),
            None => return Ok(None),
        }
    } else {
        s
    };
    Ok(Oid::from_hex(hex.as_bytes()).ok())
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
fn bad(msg: &'static str) -> io::Error {
    io::Error::other(format!("bridge: {msg}"))
}
fn to_io(e: keel_store::store::StoreError) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash, mirror};

    fn record(kind: Kind, payload: &[u8]) -> Vec<u8> {
        let oid = hash(kind, payload);
        let mut v = format!("{} {} {}\n", oid.to_hex(), kind.as_str(), payload.len()).into_bytes();
        v.extend_from_slice(payload);
        v.push(b'\n');
        v
    }

    #[test]
    fn bridges_two_commits_into_keel_history() {
        let dir = std::env::temp_dir().join(format!("keelgit-bridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("store")).unwrap();

        // commit A: one file; commit B: adds a second file, parent = A
        let blob_a = b"a\n";
        let a_oid = hash(Kind::Blob, blob_a);
        let mut tree_a = Vec::new();
        tree_a.extend_from_slice(b"100644 a.txt\0");
        tree_a.extend_from_slice(a_oid.as_bytes());
        let tree_a_oid = hash(Kind::Tree, &tree_a);
        let commit_a = format!(
            "tree {}\nauthor A <a@e> 1000 +0000\ncommitter A <a@e> 1000 +0000\n\nfirst\n",
            tree_a_oid.to_hex()
        )
        .into_bytes();
        let commit_a_oid = hash(Kind::Commit, &commit_a);

        let blob_b = b"b\n";
        let b_oid = hash(Kind::Blob, blob_b);
        let mut tree_b = Vec::new();
        tree_b.extend_from_slice(b"100644 a.txt\0");
        tree_b.extend_from_slice(a_oid.as_bytes());
        tree_b.extend_from_slice(b"100644 b.txt\0");
        tree_b.extend_from_slice(b_oid.as_bytes());
        let tree_b_oid = hash(Kind::Tree, &tree_b);
        let commit_b = format!(
            "tree {}\nparent {}\nauthor A <a@e> 2000 +0000\ncommitter A <a@e> 2000 +0000\n\nsecond\n",
            tree_b_oid.to_hex(),
            commit_a_oid.to_hex()
        )
        .into_bytes();
        let commit_b_oid = hash(Kind::Commit, &commit_b);

        let mut stream = Vec::new();
        for r in [
            record(Kind::Blob, blob_a),
            record(Kind::Blob, blob_b),
            record(Kind::Tree, &tree_a),
            record(Kind::Tree, &tree_b),
            record(Kind::Commit, &commit_a),
            record(Kind::Commit, &commit_b),
        ] {
            stream.extend(r);
        }
        mirror::ingest_batch_stream(&store, &stream).unwrap();
        mirror::ingest_refs(&store, &[
            ("refs/heads/main".into(), commit_b_oid.to_hex()),
            ("HEAD".into(), "ref: refs/heads/main".into()),
        ])
        .unwrap();

        let stats = bridge(&store).unwrap();
        assert_eq!(stats.commits, 2);

        // keel-native history now works: HEAD → B → A, and files resolve at each change
        let repo = keel_store::repo::Repo::open(&dir.join("store")).unwrap();
        let log = repo.log().unwrap();
        assert_eq!(log.len(), 2, "keel log walks the bridged history");
        let head = repo.head().unwrap().unwrap();
        assert_eq!(repo.change(head).unwrap().unwrap().intent, "second");
        assert!(repo.file_at(head, "b.txt").unwrap().is_some(), "provenance sees b.txt at HEAD");
        // and the parent change's tree lacks b.txt
        let parent = repo.change(head).unwrap().unwrap().parents[0];
        assert!(repo.file_at(parent, "b.txt").unwrap().is_none(), "b.txt absent in first commit");
        assert_eq!(repo.change(parent).unwrap().unwrap().intent, "first");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
