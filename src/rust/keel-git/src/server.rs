//! Serving side of the git wire protocol (epic NEW-1113, M6): compute the object set a client
//! needs and pack it. This module is transport-agnostic — the actual smart-HTTP/ssh plumbing
//! sits above it — so it's unit-testable against `git index-pack`.

use crate::{mirror, parse_tree, Headed, Kind, Oid};
use keel_store::store::Store;
use std::collections::HashSet;
use std::io;

/// Every object id reachable from `wants`: commits pull in their tree + parents, trees their
/// entries (skipping gitlink/submodule commits, which live in another repo), tags their target.
/// Missing objects (e.g. a shallow-clone boundary parent) are skipped rather than erroring.
pub fn reachable(store: &Store, wants: &[Oid]) -> io::Result<Vec<Oid>> {
    let mut seen: HashSet<[u8; 20]> = HashSet::new();
    let mut stack: Vec<Oid> = wants.to_vec();
    let mut out = Vec::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(*oid.as_bytes()) {
            continue;
        }
        let (kind, payload) = match mirror::get_object(store, &oid)? {
            Some(x) => x,
            None => continue,
        };
        out.push(oid);
        match kind {
            Kind::Commit => {
                let h = Headed::parse(&payload);
                if let Ok(t) = h.tree() {
                    stack.push(t);
                }
                if let Ok(parents) = h.parents() {
                    stack.extend(parents);
                }
            }
            Kind::Tree => {
                if let Ok(entries) = parse_tree(&payload) {
                    for e in entries {
                        if e.mode != b"160000" {
                            stack.push(e.oid);
                        }
                    }
                }
            }
            Kind::Tag => {
                if let Some(obj) = Headed::parse(&payload).header_value("object") {
                    if let Ok(o) = Oid::from_hex(obj) {
                        stack.push(o);
                    }
                }
            }
            Kind::Blob => {}
        }
    }
    Ok(out)
}

/// Build a packfile of everything reachable from `wants` — a full clone/fetch (no `have`s). The
/// pack is undeltified (see [`crate::pack`]); git accepts it and repacks locally.
pub fn pack_for(store: &Store, wants: &[Oid]) -> io::Result<Vec<u8>> {
    let oids = reachable(store, wants)?;
    let mut objects = Vec::with_capacity(oids.len());
    for oid in &oids {
        if let Some(obj) = mirror::get_object(store, oid)? {
            objects.push(obj);
        }
    }
    Ok(crate::pack::write(&objects))
}

/// The direct (non-symbolic) refs to advertise, as `(name, oid)`. Symbolic refs like HEAD are
/// resolved to their target so a clone knows which commit HEAD points at.
pub fn advertised_refs(store: &Store) -> io::Result<Vec<(String, Oid)>> {
    let all = mirror::refs(store)?;
    let lookup = |name: &str| -> Option<Oid> {
        all.iter().find(|(n, _)| n == name).and_then(|(_, v)| {
            if let Some(t) = v.strip_prefix("ref: ") {
                all.iter().find(|(n, _)| n == t).and_then(|(_, hv)| Oid::from_hex(hv.as_bytes()).ok())
            } else {
                Oid::from_hex(v.as_bytes()).ok()
            }
        })
    };
    let mut out = Vec::new();
    for (name, val) in &all {
        if val.starts_with("ref: ") {
            if let Some(oid) = lookup(name) {
                out.push((name.clone(), oid));
            }
        } else if let Ok(oid) = Oid::from_hex(val.as_bytes()) {
            out.push((name.clone(), oid));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
