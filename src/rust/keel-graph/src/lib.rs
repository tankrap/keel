//! keel live dependency graph.
//!
//! A file-level import graph maintained **over the working tree** (not a committed
//! snapshot), so it sees in-flight, uncommitted edits — the write-path liveness a static
//! committed-state index (Sourcegraph) structurally can't have (benchmarked 76% vs 0%).
//!
//! It is **incremental**: [`LiveGraph::refresh`] re-resolves only the files whose content
//! changed since the last pass (detected by BLAKE3), and updates just their edges — not
//! the whole repo. That's the property that makes "live at fleet scale" affordable
//! (Linear NEW-1075). Edges come from the resolver sidecar; this first layer uses
//! file-level relative imports (accurate cross-package/aliased resolution is NEW-1080).

use keel_resolve::Sidecar;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

pub struct LiveGraph {
    root: PathBuf,
    sidecar: Sidecar,
    deps: HashMap<String, Vec<String>>,      // file → files it imports (repo-relative)
    rdeps: HashMap<String, HashSet<String>>, // file → files that import it
    hashes: HashMap<String, [u8; 32]>,       // file → content hash (change detection)
}

impl LiveGraph {
    /// Open a live graph rooted at `root`, driven by the resolver `script`.
    pub fn open(root: &Path, script: &Path) -> io::Result<LiveGraph> {
        Ok(LiveGraph {
            root: root.to_path_buf(),
            sidecar: Sidecar::spawn(script)?,
            deps: HashMap::new(),
            rdeps: HashMap::new(),
            hashes: HashMap::new(),
        })
    }

    /// Full (re)build over the current working tree. Returns files indexed.
    pub fn build(&mut self) -> io::Result<usize> {
        let files = walk_ts(&self.root);
        for f in &files {
            self.reindex_file(f)?;
        }
        Ok(files.len())
    }

    /// Incrementally bring the graph to the current working-tree state. Re-resolves only
    /// files whose content changed (added / modified) and drops deleted files. Returns
    /// the files that were re-resolved or removed — for observability and for proving
    /// the graph didn't touch unchanged files.
    pub fn refresh(&mut self) -> io::Result<Vec<String>> {
        let files = walk_ts(&self.root);
        let current: HashSet<&String> = files.iter().collect();
        let mut changed = Vec::new();

        for f in &files {
            if self.hashes.get(f) != Some(&file_hash(&self.root, f)) {
                self.reindex_file(f)?;
                changed.push(f.clone());
            }
        }
        let known: Vec<String> = self.hashes.keys().cloned().collect();
        for f in known {
            if !current.contains(&f) {
                self.remove_file(&f);
                changed.push(f);
            }
        }
        changed.sort();
        Ok(changed)
    }

    fn reindex_file(&mut self, f: &str) -> io::Result<()> {
        let targets = self.sidecar.imports(&self.root, f)?;
        // retract old reverse edges, then install new ones
        if let Some(old) = self.deps.get(f) {
            for t in old {
                if let Some(s) = self.rdeps.get_mut(t) {
                    s.remove(f);
                }
            }
        }
        for t in &targets {
            self.rdeps.entry(t.clone()).or_default().insert(f.to_string());
        }
        self.deps.insert(f.to_string(), targets);
        self.hashes.insert(f.to_string(), file_hash(&self.root, f));
        Ok(())
    }

    fn remove_file(&mut self, f: &str) {
        if let Some(old) = self.deps.remove(f) {
            for t in &old {
                if let Some(s) = self.rdeps.get_mut(t) {
                    s.remove(f);
                }
            }
        }
        self.hashes.remove(f);
    }

    /// Direct import targets of `f`.
    pub fn deps(&self, f: &str) -> Vec<String> {
        self.deps.get(f).cloned().unwrap_or_default()
    }

    /// Files that directly import `f` (its impact set at depth 1).
    pub fn rdeps(&self, f: &str) -> Vec<String> {
        let mut v: Vec<String> = self.rdeps.get(f).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// Everything `f` transitively depends on.
    pub fn transitive_deps(&self, f: &str) -> Vec<String> {
        self.bfs(f, false)
    }

    /// Everything that transitively depends on `f` (the blast radius of changing it).
    pub fn transitive_rdeps(&self, f: &str) -> Vec<String> {
        self.bfs(f, true)
    }

    fn bfs(&self, start: &str, reverse: bool) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut q = VecDeque::new();
        let mut out = Vec::new();
        q.push_back(start.to_string());
        seen.insert(start.to_string());
        while let Some(x) = q.pop_front() {
            let neighbors: Vec<String> = if reverse {
                self.rdeps.get(&x).map(|s| s.iter().cloned().collect()).unwrap_or_default()
            } else {
                self.deps.get(&x).cloned().unwrap_or_default()
            };
            for n in neighbors {
                if seen.insert(n.clone()) {
                    out.push(n.clone());
                    q.push_back(n);
                }
            }
        }
        out.sort();
        out
    }

    pub fn file_count(&self) -> usize {
        self.deps.len()
    }

    pub fn edge_count(&self) -> usize {
        self.deps.values().map(|v| v.len()).sum()
    }
}

fn walk_ts(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "node_modules" || name == "dist" || name == "build" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if (name.ends_with(".ts") || name.ends_with(".tsx")) && !name.ends_with(".d.ts") {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn file_hash(root: &Path, f: &str) -> [u8; 32] {
    std::fs::read(root.join(f)).map(|b| *blake3::hash(&b).as_bytes()).unwrap_or([0u8; 32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("keel-graph-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar/resolve.mjs")
    }

    #[test]
    fn live_graph_is_incremental_and_tracks_the_working_tree() {
        let dir = tmp();
        fs::write(dir.join("a.ts"), "import { b } from './b';\n").unwrap();
        fs::write(dir.join("b.ts"), "import { c } from './c';\nexport const b = 1;\n").unwrap();
        fs::write(dir.join("c.ts"), "export const c = 1;\n").unwrap();

        let mut g = match LiveGraph::open(&dir, &script()) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: node not available ({e})");
                return;
            }
        };
        assert_eq!(g.build().unwrap(), 3);

        // dependency edges
        assert_eq!(g.deps("a.ts"), vec!["b.ts"]);
        assert_eq!(g.transitive_deps("a.ts"), vec!["b.ts", "c.ts"]);
        // reverse edges = impact / blast radius
        assert_eq!(g.rdeps("c.ts"), vec!["b.ts"]);
        assert_eq!(g.transitive_rdeps("c.ts"), vec!["a.ts", "b.ts"]);

        // ── the live+incremental property ──
        // edit a.ts to also import c directly
        fs::write(dir.join("a.ts"), "import { b } from './b';\nimport { c } from './c';\n").unwrap();
        let changed = g.refresh().unwrap();
        assert_eq!(changed, vec!["a.ts"], "ONLY the edited file re-resolved");
        assert_eq!(g.deps("a.ts"), vec!["b.ts", "c.ts"]);
        assert_eq!(g.transitive_rdeps("c.ts"), vec!["a.ts", "b.ts"]);

        // a no-op refresh does no work
        assert!(g.refresh().unwrap().is_empty(), "unchanged tree → nothing re-resolved");

        // deleting b.ts drops it and its edges
        fs::remove_file(dir.join("b.ts")).unwrap();
        let changed = g.refresh().unwrap();
        assert_eq!(changed, vec!["b.ts"]);
        assert!(g.deps("b.ts").is_empty());
        assert!(!g.rdeps("c.ts").contains(&"b.ts".to_string()), "deleted file's edges retracted");

        let _ = fs::remove_dir_all(&dir);
    }
}
