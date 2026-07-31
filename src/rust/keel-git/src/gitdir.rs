//! Read a git repository's on-disk state natively — loose objects, packfiles, and refs — with no
//! `git` binary. Combined with the object codec + pack reader, this lets `keel mirror-in` ingest a
//! repo entirely on its own (the last piece of "keel doesn't depend on git").

use crate::{hash, loose, pack, Kind, Oid};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// Locate the git directory for a repo path: `<repo>/.git` for a working tree, or `<repo>` itself
/// if it looks bare (has an `objects/` dir).
pub fn locate(repo: &Path) -> Option<PathBuf> {
    let dotgit = repo.join(".git");
    if dotgit.join("objects").is_dir() {
        return Some(dotgit);
    }
    if repo.join("objects").is_dir() {
        return Some(repo.to_path_buf());
    }
    None
}

/// Every object in the repo (loose + packed), fully resolved to `(kind, payload)`. Cross-pack and
/// loose bases for thin deltas are resolved from the accumulating set.
pub fn read_all_objects(git_dir: &Path) -> io::Result<Vec<(Kind, Vec<u8>)>> {
    let mut all: HashMap<[u8; 20], (Kind, Vec<u8>)> = HashMap::new();

    // loose objects: .git/objects/<xx>/<38 hex>
    let objects = git_dir.join("objects");
    if let Ok(dirs) = std::fs::read_dir(&objects) {
        for d in dirs.flatten() {
            let name = d.file_name();
            let name = name.to_string_lossy();
            if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let rest = f.file_name();
                let rest = rest.to_string_lossy();
                if rest.len() != 38 {
                    continue;
                }
                if let Ok(oid) = Oid::from_hex(format!("{name}{rest}").as_bytes()) {
                    if let Ok((kind, payload)) = loose::decode(&std::fs::read(f.path())?) {
                        all.insert(*oid.as_bytes(), (kind, payload));
                    }
                }
            }
        }
    }

    // packfiles: .git/objects/pack/*.pack
    let pack_dir = objects.join("pack");
    if let Ok(entries) = std::fs::read_dir(&pack_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "pack").unwrap_or(false) {
                let bytes = std::fs::read(&p)?;
                let base = |oid: &Oid| all.get(oid.as_bytes()).cloned();
                let objs = pack::read(&bytes, &base)?; // borrow of `all` ends here
                for (kind, payload) in objs {
                    all.insert(*hash(kind, &payload).as_bytes(), (kind, payload));
                }
            }
        }
    }

    Ok(all.into_values().collect())
}

/// All refs (loose + packed) + symbolic HEAD, as `(name, value)` — value is a 40-hex oid, or
/// `"ref: <target>"` for a symbolic ref. Loose refs override packed-refs.
pub fn read_refs(git_dir: &Path) -> io::Result<Vec<(String, String)>> {
    let mut refs: HashMap<String, String> = HashMap::new();

    // packed-refs first (loose overrides below)
    if let Ok(text) = std::fs::read_to_string(git_dir.join("packed-refs")) {
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue; // header / peeled-tag line
            }
            if let Some((oid, name)) = line.split_once(' ') {
                refs.insert(name.trim().to_string(), oid.trim().to_string());
            }
        }
    }

    // loose refs under refs/
    let refs_root = git_dir.join("refs");
    let mut stack = vec![refs_root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(git_dir) {
                let name = rel.to_string_lossy().replace('\\', "/");
                if let Ok(v) = std::fs::read_to_string(&p) {
                    refs.insert(name, v.trim().to_string());
                }
            }
        }
    }

    // HEAD (symbolic or detached)
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        refs.insert("HEAD".to_string(), head.trim().to_string());
    }

    let mut out: Vec<(String, String)> = refs.into_iter().collect();
    out.sort();
    Ok(out)
}
