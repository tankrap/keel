//! keel live dependency graph.
//!
//! A file-level import graph maintained **over the working tree** (not a committed
//! snapshot), so it sees in-flight, uncommitted edits — the write-path liveness a static
//! committed-state index (Sourcegraph) structurally can't have (benchmarked 76% vs 0%).
//!
//! It is **incremental**: [`LiveGraph::refresh`] re-resolves only the files that changed
//! since the last pass, and updates just their edges — not the whole repo. Change detection
//! is by **mtime+size** (a `stat`, no file read), so a clean refresh reads nothing — that's
//! what keeps it affordable at fleet/kernel scale (a content-hash pass re-read every file
//! and cost ~8 s on the linux kernel; NEW-1075 / NEW-1110). Edges come from the resolver
//! sidecar; this first layer uses file-level relative imports (NEW-1080).

use keel_resolve::Resolve;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Paths the fs-watcher has seen change since the last refresh. `full_rescan` is set when
/// the OS signals the event stream overflowed (or errored) — then refresh falls back to a
/// full walk rather than trust a partial set.
#[derive(Default)]
struct ChangeSet {
    paths: HashSet<String>,
    full_rescan: bool,
}

/// A live fs-watch: the watcher handle (kept alive — dropping it stops watching) and the
/// shared set its background thread feeds.
struct Watch {
    _watcher: RecommendedWatcher,
    changed: Arc<Mutex<ChangeSet>>,
}

pub struct LiveGraph {
    root: PathBuf,
    deps: HashMap<String, Vec<String>>,      // file → files it imports (repo-relative)
    rdeps: HashMap<String, HashSet<String>>, // file → files that import it
    stats: HashMap<String, (u64, u64)>,      // file → (mtime_ns, size) for change detection
    watch: Option<Watch>,                    // Some once fs-watching (refresh goes O(changed))
}

impl LiveGraph {
    /// A new, empty live graph rooted at `root`. The graph is resolver-agnostic: it owns
    /// no sidecar — callers pass a `&mut Sidecar` to [`build`](Self::build) /
    /// [`refresh`](Self::refresh). That lets one process (e.g. a `BriefService`) drive
    /// both the graph and its symbol slicer from a *single* resolver, instead of spawning
    /// one node subprocess per consumer.
    pub fn new(root: &Path) -> LiveGraph {
        LiveGraph {
            root: root.to_path_buf(),
            deps: HashMap::new(),
            rdeps: HashMap::new(),
            stats: HashMap::new(),
            watch: None,
        }
    }

    /// Start OS filesystem watching so [`refresh`](Self::refresh) only re-resolves files that
    /// actually changed — no full-tree walk. Call it once (typically right after [`build`]).
    /// If the OS signals an overflow, the next refresh transparently falls back to a full
    /// walk, so correctness never depends on catching every event.
    pub fn watch(&mut self) -> io::Result<()> {
        let changed = Arc::new(Mutex::new(ChangeSet::default()));
        let sink = Arc::clone(&changed);
        // Strip against the CANONICAL root: macOS FSEvents reports `/private/var/…` paths
        // while the root may be `/var/…`, and a mismatch would silently drop every event.
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let mut cs = sink.lock().unwrap();
            match res {
                Ok(ev) => {
                    if ev.need_rescan() {
                        cs.full_rescan = true;
                        return;
                    }
                    for p in ev.paths {
                        if let Ok(rel) = p.strip_prefix(&root) {
                            let rel = rel.to_string_lossy().replace('\\', "/");
                            if !is_ignored_path(&rel) && is_source(&rel) {
                                cs.paths.insert(rel);
                            }
                        }
                    }
                }
                Err(_) => cs.full_rescan = true,
            }
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
        watcher
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.watch = Some(Watch { _watcher: watcher, changed });
        Ok(())
    }

    /// Full (re)build over the current working tree. Returns files indexed.
    pub fn build(&mut self, sc: &mut impl Resolve) -> io::Result<usize> {
        let files = walk_ts(&self.root);
        for f in &files {
            self.reindex_file(sc, f)?;
        }
        Ok(files.len())
    }

    /// Persist the resolved graph (each file's imports + its `(mtime,size)`) to `path`, so a later
    /// process can [`load`](Self::load) it and [`refresh`](Self::refresh) only the files that
    /// changed — turning a cold whole-repo re-resolve (seconds on a large repo) into a stat sweep.
    /// Reverse edges aren't stored; they're rebuilt from the forward edges on load.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = String::new();
        for (f, imports) in &self.deps {
            if f.contains('\t') || f.contains('\n') {
                continue; // path unrepresentable in this line format — skip (it'll just re-resolve)
            }
            let (mtime, size) = self.stats.get(f).copied().unwrap_or((0, 0));
            out.push_str(f);
            out.push('\t');
            out.push_str(&mtime.to_string());
            out.push('\t');
            out.push_str(&size.to_string());
            for t in imports {
                if !t.contains('\t') && !t.contains('\n') {
                    out.push('\t');
                    out.push_str(t);
                }
            }
            out.push('\n');
        }
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("tmp"); // atomic: never leave a half-written graph
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, path)
    }

    /// Load a graph previously written by [`save`]. Populates forward + reverse edges and the
    /// stat index; a following [`refresh`] reconciles it with the current tree. Best-effort:
    /// a missing/corrupt line is skipped (that file simply re-resolves).
    pub fn load(&mut self, path: &Path) -> io::Result<()> {
        let data = std::fs::read_to_string(path)?;
        for line in data.lines() {
            let mut it = line.split('\t');
            let f = match it.next() {
                Some(f) if !f.is_empty() => f.to_string(),
                _ => continue,
            };
            let mtime: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let size: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let imports: Vec<String> = it.map(|s| s.to_string()).collect();
            for t in &imports {
                self.rdeps.entry(t.clone()).or_default().insert(f.clone());
            }
            self.deps.insert(f.clone(), imports);
            self.stats.insert(f, (mtime, size));
        }
        Ok(())
    }

    /// Incrementally bring the graph to the current working-tree state. Re-resolves only
    /// files whose content changed (added / modified) and drops deleted files. Returns
    /// the files that were re-resolved or removed — for observability and for proving
    /// the graph didn't touch unchanged files.
    pub fn refresh(&mut self, sc: &mut impl Resolve) -> io::Result<Vec<String>> {
        // Fast path: if fs-watching and the OS hasn't signalled a rescan, re-resolve only the
        // files the watcher flagged — no full-tree walk. Drain the set under a short lock,
        // then act (reindex or drop) without holding it.
        let watched = self.watch.as_ref().map(|w| {
            let mut cs = w.changed.lock().unwrap();
            let rescan = std::mem::take(&mut cs.full_rescan);
            if rescan {
                cs.paths.clear();
                (true, Vec::new())
            } else {
                (false, cs.paths.drain().collect::<Vec<_>>())
            }
        });
        if let Some((false, paths)) = watched {
            let mut changed = Vec::new();
            for f in paths {
                if self.root.join(&f).is_file() {
                    self.reindex_file(sc, &f)?;
                } else {
                    self.remove_file(&f);
                }
                changed.push(f);
            }
            changed.sort();
            return Ok(changed);
        }
        // Slow path: not watching, or a rescan was requested — walk the whole tree.
        let files = walk_ts(&self.root);
        let current: HashSet<&String> = files.iter().collect();
        let mut changed = Vec::new();

        for f in &files {
            if self.stats.get(f) != Some(&file_stat(&self.root, f)) {
                self.reindex_file(sc, f)?;
                changed.push(f.clone());
            }
        }
        let known: Vec<String> = self.stats.keys().cloned().collect();
        for f in known {
            if !current.contains(&f) {
                self.remove_file(&f);
                changed.push(f);
            }
        }
        changed.sort();
        Ok(changed)
    }

    fn reindex_file(&mut self, sc: &mut impl Resolve, f: &str) -> io::Result<()> {
        let targets = sc.imports(&self.root, f)?;
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
        self.stats.insert(f.to_string(), file_stat(&self.root, f));
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
        self.stats.remove(f);
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

/// Source files the graph resolves over. The sidecar decides how to resolve them (TS
/// imports, C `#include`s, …); the walk just has to find them, so it spans the languages
/// keel has resolvers for. `.d.ts` is skipped (declarations, no edges of interest).
fn is_source(name: &str) -> bool {
    if name.ends_with(".d.ts") {
        return false;
    }
    const EXTS: &[&str] = &[
        ".ts", ".tsx", ".js", ".jsx", ".mjs", ".c", ".h", ".cc", ".cpp", ".cxx", ".hh", ".hpp",
        ".py", ".pyi", ".go",
    ];
    EXTS.iter().any(|e| name.ends_with(e))
}

/// Whether a repo-relative path lies under an ignored dir (dotfiles like `.keel`/`.git`,
/// or build dirs) — mirrors the walk's skip rules, so watcher events for the store's own
/// `.keel/*.mdb` writes or `node_modules` churn never trigger a re-resolve.
fn is_ignored_path(rel: &str) -> bool {
    rel.split('/')
        .any(|c| c.starts_with('.') || c == "node_modules" || c == "dist" || c == "build")
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
            } else if is_source(&name) {
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

/// `(mtime_ns, size)` for change detection — a `stat`, no file read. mtime+size (rather than
/// a content hash) means a touch with identical content triggers one cheap re-resolve of just
/// that file — acceptable, and how git's index works. On stat error, `(0, 0)`.
fn file_stat(root: &Path, f: &str) -> (u64, u64) {
    match std::fs::metadata(root.join(f)) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (mtime, m.len())
        }
        Err(_) => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_resolve::Sidecar;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

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

    /// With fs-watching on, `refresh` picks up a newly created file (and its edges) without a
    /// full-tree walk. Timing-tolerant: polls refresh until the OS delivers the event.
    #[test]
    fn fs_watch_delivers_changes_to_refresh() {
        let dir = tmp();
        fs::write(dir.join("b.ts"), "export const b = 1;\n").unwrap();
        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping fs-watch test: node not available ({e})");
                return;
            }
        };
        let mut g = LiveGraph::new(&dir);
        g.build(&mut sc).unwrap();
        if let Err(e) = g.watch() {
            eprintln!("skipping fs-watch test: watch unavailable ({e})");
            return;
        }

        // let the watcher establish, then create a new file that imports b
        std::thread::sleep(Duration::from_millis(250));
        fs::write(dir.join("c.ts"), "import { b } from './b';\n").unwrap();

        // poll refresh until the watcher delivers c.ts (bounded ~3s for FSEvents latency)
        let mut seen = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(50));
            if g.refresh(&mut sc).unwrap().iter().any(|f| f == "c.ts") {
                seen = true;
                break;
            }
        }
        assert!(seen, "fs-watch delivered the new file to refresh without a full walk");
        assert_eq!(g.deps("c.ts"), vec!["b.ts"], "the new file's edge was resolved");
        assert!(g.rdeps("b.ts").contains(&"c.ts".to_string()), "reverse edge installed");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_graph_is_incremental_and_tracks_the_working_tree() {
        let dir = tmp();
        fs::write(dir.join("a.ts"), "import { b } from './b';\n").unwrap();
        fs::write(dir.join("b.ts"), "import { c } from './c';\nexport const b = 1;\n").unwrap();
        fs::write(dir.join("c.ts"), "export const c = 1;\n").unwrap();

        let mut sc = match Sidecar::spawn(&script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping: node not available ({e})");
                return;
            }
        };
        let mut g = LiveGraph::new(&dir);
        assert_eq!(g.build(&mut sc).unwrap(), 3);

        // dependency edges
        assert_eq!(g.deps("a.ts"), vec!["b.ts"]);
        assert_eq!(g.transitive_deps("a.ts"), vec!["b.ts", "c.ts"]);
        // reverse edges = impact / blast radius
        assert_eq!(g.rdeps("c.ts"), vec!["b.ts"]);
        assert_eq!(g.transitive_rdeps("c.ts"), vec!["a.ts", "b.ts"]);

        // ── the live+incremental property ──
        // edit a.ts to also import c directly
        fs::write(dir.join("a.ts"), "import { b } from './b';\nimport { c } from './c';\n").unwrap();
        let changed = g.refresh(&mut sc).unwrap();
        assert_eq!(changed, vec!["a.ts"], "ONLY the edited file re-resolved");
        assert_eq!(g.deps("a.ts"), vec!["b.ts", "c.ts"]);
        assert_eq!(g.transitive_rdeps("c.ts"), vec!["a.ts", "b.ts"]);

        // a no-op refresh does no work
        assert!(g.refresh(&mut sc).unwrap().is_empty(), "unchanged tree → nothing re-resolved");

        // deleting b.ts drops it and its edges
        fs::remove_file(dir.join("b.ts")).unwrap();
        let changed = g.refresh(&mut sc).unwrap();
        assert_eq!(changed, vec!["b.ts"]);
        assert!(g.deps("b.ts").is_empty());
        assert!(!g.rdeps("c.ts").contains(&"b.ts".to_string()), "deleted file's edges retracted");

        let _ = fs::remove_dir_all(&dir);
    }
}
