//! Coordination for many agents on shared code (Linear NEW-1092).
//!
//! The model is **prevent-not-detect**, piggybacked on the brief: when an agent fetches
//! a brief for a task it reserves the files it's about to edit. If another agent already
//! holds one, that's returned as a **conflict** (the caller redirects to other work)
//! rather than both editing and merge-conflicting later. Uncontended reservations are
//! free — a hashmap insert on the fetch the agent already makes.
//!
//! This first cut is an in-process registry behind a mutex (the single-daemon model). Prediction is
//! import-graph-aware — a held file that the target imports (or that imports the target) is a soft
//! conflict even across directories — with same-directory kept as the fallback signal for files the
//! graph has no edge for (a brand-new file, or a language without a resolver). The ordered-authority
//! layer (distributed consistency) is still later work.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a reservation lives without a heartbeat before it's considered abandoned and swept. The
/// brief service calls [`Coordinator::heartbeat`] on every fetch, so an actively-working agent (which
/// keeps fetching briefs) never loses a hold; a crashed or wandered-off agent stops heartbeating and
/// its holds free themselves after this, instead of blocking others forever as stale conflicts. Ten
/// minutes is far longer than the gap between an active agent's brief fetches, so false expiry of a
/// live hold doesn't happen in practice.
const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// A file wanted by the caller but currently held by another agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub file: String,
    pub agent: String,
    pub task: String,
}

/// Why a held file is a *predicted* (soft) conflict with the caller's target — a held file that is
/// import-linked to the target (or, failing that, in the same directory) is close enough to likely
/// collide even though it isn't the exact file the caller reserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The held file is imported BY the target: the caller depends on code someone else is editing.
    Imports,
    /// The held file IMPORTS the target: the caller's change may break code someone else is editing.
    ImportedBy,
    /// Not import-linked — just the same directory/module. The fallback signal when the graph has no
    /// edge between the two (e.g. a brand-new file, or a language without a resolver sidecar).
    /// Top-level files (no directory) are excluded: the repo root isn't a meaningful module.
    SameDir,
}

impl Relation {
    /// A stable lowercase tag for JSON / display.
    pub fn as_str(self) -> &'static str {
        match self {
            Relation::Imports => "imports",
            Relation::ImportedBy => "imported-by",
            Relation::SameDir => "same-dir",
        }
    }
}

/// A *predicted* (soft) conflict: another agent holds a file that is import-linked to (or in the same
/// directory as) one the caller intends to touch — not the exact file, but close enough to likely
/// collide. Surfaced so a fleet spreads across modules instead of piling into the same area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedConflict {
    pub held_file: String,
    pub agent: String,
    pub task: String,
    pub relation: Relation,
}

/// A read-only view of one active hold, for observability (`keel reservations`): who holds a file,
/// for what task, how long it's been held, and how long until it would be swept if not renewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub file: String,
    pub agent: String,
    pub task: String,
    pub age_secs: u64,
    pub ttl_remaining_secs: u64,
}

/// One held file: the holder, its task, and when the hold was last taken or renewed (for TTL expiry).
struct Reservation {
    agent: String,
    task: String,
    at: Instant,
}

#[derive(Default)]
struct Registry {
    held: HashMap<String, Reservation>, // file → reservation
}

/// A shared reservation registry. Cheap to clone (shares one registry), so every agent's
/// brief service holds a handle to the same coordinator.
#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<Mutex<Registry>>,
    ttl: Duration,
}

impl Default for Coordinator {
    fn default() -> Self {
        // NB: a derived Default would give ttl = Duration::ZERO → every reservation expires instantly.
        Coordinator { inner: Arc::new(Mutex::new(Registry::default())), ttl: DEFAULT_TTL }
    }
}

impl Coordinator {
    pub fn new() -> Coordinator {
        Coordinator::default()
    }

    /// Override the reservation TTL (default 300s): a hold not renewed within this window is swept.
    /// Tune shorter for faster reclamation after a crash, longer for slow tasks between brief fetches.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Reserve `files` for `agent`/`task`. Files held by a *different* agent are returned
    /// as conflicts and left with their current holder (the caller backs off). Free files
    /// — or files already held by `agent` — are reserved to `agent`. Idempotent.
    pub fn reserve(&self, agent: &str, task: &str, files: &[String]) -> Vec<Conflict> {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl); // reclaim abandoned holds before deciding conflicts
        let mut conflicts = Vec::new();
        for f in files {
            match reg.held.get(f) {
                Some(r) if r.agent != agent => {
                    conflicts.push(Conflict { file: f.clone(), agent: r.agent.clone(), task: r.task.clone() });
                }
                // free, or already ours — (re)take it and refresh the TTL (a brief fetch = heartbeat)
                _ => {
                    reg.held.insert(
                        f.clone(),
                        Reservation { agent: agent.to_string(), task: task.to_string(), at: Instant::now() },
                    );
                }
            }
        }
        conflicts
    }

    /// Heartbeat: refresh the TTL on **every** file `agent` still holds. The brief service calls this
    /// on each fetch — an agent that's actively working keeps fetching briefs, so all of its holds
    /// stay alive; only one that has crashed or gone idle stops heartbeating and ages out. This is
    /// what keeps the TTL from reclaiming a live reservation whose file just isn't being re-briefed.
    pub fn heartbeat(&self, agent: &str) {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        let now = Instant::now();
        for r in reg.held.values_mut() {
            if r.agent == agent {
                r.at = now;
            }
        }
    }

    /// Who (other than `agent`) currently holds any of `files` — no reservation taken.
    pub fn peek(&self, agent: &str, files: &[String]) -> Vec<Conflict> {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        files
            .iter()
            .filter_map(|f| match reg.held.get(f) {
                Some(r) if r.agent != agent => {
                    Some(Conflict { file: f.clone(), agent: r.agent.clone(), task: r.task.clone() })
                }
                _ => None,
            })
            .collect()
    }

    /// Release everything held by `agent` (e.g. when its work lands). Returns how many holds freed.
    pub fn release_agent(&self, agent: &str) -> usize {
        let mut reg = self.inner.lock().unwrap();
        let before = reg.held.len();
        reg.held.retain(|_, r| r.agent != agent);
        before - reg.held.len()
    }

    /// Release specific `files` held by `agent`. Returns how many holds freed (files not held by
    /// `agent`, or not held at all, are ignored).
    pub fn release_files(&self, agent: &str, files: &[String]) -> usize {
        let mut reg = self.inner.lock().unwrap();
        let mut freed = 0;
        for f in files {
            if reg.held.get(f).is_some_and(|r| r.agent == agent) {
                reg.held.remove(f);
                freed += 1;
            }
        }
        freed
    }

    /// A snapshot of every currently-held file (expired holds swept first), sorted by file — for
    /// observability (`keel reservations`). Read-only: takes no reservation and refreshes no TTL.
    pub fn snapshot(&self) -> Vec<Hold> {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        let now = Instant::now();
        let mut out: Vec<Hold> = reg
            .held
            .iter()
            .map(|(file, r)| {
                let age = now.duration_since(r.at);
                Hold {
                    file: file.clone(),
                    agent: r.agent.clone(),
                    task: r.task.clone(),
                    age_secs: age.as_secs(),
                    ttl_remaining_secs: self.ttl.saturating_sub(age).as_secs(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.file.cmp(&b.file));
        out
    }

    /// Predict soft conflicts for `files`: reservations by *other* agents on files that are
    /// import-linked to a requested file — one the target imports (`imports` = graph deps) or one
    /// that imports the target (`imported_by` = graph rdeps) — or, failing an import edge, one in the
    /// same directory. Exact-file collisions are excluded (`reserve`/`peek` already report those as
    /// hard conflicts). This is the "someone is already working on code your change touches — consider
    /// elsewhere" signal that lets a fleet self-spread; the import edges catch collisions the plain
    /// same-directory heuristic misses (two agents in different folders editing import-linked files).
    /// The caller supplies the adjacency (it already has the live graph); the coordinator stays a pure
    /// registry. It is the single ordered authority, so this view is consistent by construction.
    pub fn predict(
        &self,
        agent: &str,
        files: &[String],
        imports: &[String],
        imported_by: &[String],
    ) -> Vec<PredictedConflict> {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        let want: HashSet<&str> = files.iter().map(String::as_str).collect();
        let imports: HashSet<&str> = imports.iter().map(String::as_str).collect();
        let imported_by: HashSet<&str> = imported_by.iter().map(String::as_str).collect();
        let want_dirs: HashSet<&str> = files.iter().map(|f| dir_of(f)).collect();
        let mut out: Vec<PredictedConflict> = reg
            .held
            .iter()
            .filter(|(held, r)| r.agent != agent && !want.contains(held.as_str()))
            .filter_map(|(held, r)| {
                // strongest specific signal first: a direct import edge beats mere co-location.
                let relation = if imports.contains(held.as_str()) {
                    Relation::Imports
                } else if imported_by.contains(held.as_str()) {
                    Relation::ImportedBy
                } else {
                    let d = dir_of(held);
                    // top-level files (dir == "") don't form a meaningful module: grouping every
                    // repo-root file together would flag unrelated configs/entrypoints as neighbors.
                    // Require a non-empty shared directory — genuinely-related root files still get
                    // caught by the import edges above.
                    if !d.is_empty() && want_dirs.contains(d) {
                        Relation::SameDir
                    } else {
                        return None;
                    }
                };
                Some(PredictedConflict {
                    held_file: held.clone(),
                    agent: r.agent.clone(),
                    task: r.task.clone(),
                    relation,
                })
            })
            .collect();
        out.sort_by(|a, b| a.held_file.cmp(&b.held_file));
        out
    }

    pub fn held_count(&self) -> usize {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        reg.held.len()
    }
}

/// Drop reservations not renewed within `ttl` — an agent that crashed or wandered off shouldn't hold
/// files forever. Called on every registry access, so expiry is lazy (no background thread needed).
fn sweep(reg: &mut Registry, ttl: Duration) {
    let now = Instant::now();
    reg.held.retain(|_, r| now.duration_since(r.at) < ttl);
}

/// The directory portion of a repo-relative path (everything before the last `/`), or `""`
/// for a top-level file.
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(fs: &[&str]) -> Vec<String> {
        fs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reserve_reports_conflicts_and_is_free_when_uncontended() {
        let c = Coordinator::new();
        // alice takes a.ts uncontended → no conflict
        assert!(c.reserve("alice", "t1", &files(&["a.ts"])).is_empty());
        // bob wants a.ts + b.ts: a.ts conflicts with alice, b.ts is free (bob gets it)
        let conf = c.reserve("bob", "t2", &files(&["a.ts", "b.ts"]));
        assert_eq!(conf, vec![Conflict { file: "a.ts".into(), agent: "alice".into(), task: "t1".into() }]);
        assert_eq!(c.held_count(), 2, "a.ts→alice, b.ts→bob");
        // alice now sees b.ts is bob's
        assert_eq!(
            c.peek("alice", &files(&["b.ts"])),
            vec![Conflict { file: "b.ts".into(), agent: "bob".into(), task: "t2".into() }]
        );
    }

    #[test]
    fn release_frees_reservations() {
        let c = Coordinator::new();
        c.reserve("alice", "t", &files(&["a.ts", "b.ts"]));
        assert_eq!(c.release_files("alice", &files(&["a.ts"])), 1, "one file freed");
        assert_eq!(c.release_files("alice", &files(&["a.ts"])), 0, "already free → nothing freed");
        assert_eq!(c.release_files("bob", &files(&["b.ts"])), 0, "not bob's → nothing freed");
        // a.ts free → bob takes it; b.ts still alice's → conflict
        let conf = c.reserve("bob", "t2", &files(&["a.ts", "b.ts"]));
        assert_eq!(conf, vec![Conflict { file: "b.ts".into(), agent: "alice".into(), task: "t".into() }]);
        assert_eq!(c.release_agent("alice"), 1, "alice's remaining b.ts freed");
        assert!(c.reserve("bob", "t2", &files(&["b.ts"])).is_empty());
    }

    #[test]
    fn snapshot_lists_holds_with_ages_and_ttl() {
        let c = Coordinator::new().with_ttl(Duration::from_secs(600));
        c.reserve("alice", "auth", &files(&["src/b.ts", "src/a.ts"]));
        c.reserve("bob", "ui", &files(&["src/c.ts"]));
        let snap = c.snapshot();
        // sorted by file, one entry per hold
        assert_eq!(snap.iter().map(|h| h.file.as_str()).collect::<Vec<_>>(), ["src/a.ts", "src/b.ts", "src/c.ts"]);
        let a = &snap[0];
        assert_eq!((a.agent.as_str(), a.task.as_str()), ("alice", "auth"));
        assert_eq!(snap[2].agent, "bob");
        // just-taken holds: age ~0, ttl close to the full window (allow scheduler slack)
        assert!(snap.iter().all(|h| h.age_secs <= 1 && h.ttl_remaining_secs >= 598));
    }

    #[test]
    fn snapshot_omits_expired_holds() {
        let c = Coordinator::new().with_ttl(Duration::from_millis(200));
        c.reserve("alice", "t", &files(&["a.ts"]));
        assert_eq!(c.snapshot().len(), 1);
        std::thread::sleep(Duration::from_millis(350));
        assert!(c.snapshot().is_empty(), "expired hold swept from the snapshot");
    }

    #[test]
    fn predict_warns_on_same_directory_not_exact_file() {
        let c = Coordinator::new();
        c.reserve("alice", "auth", &files(&["src/auth/login.rs", "src/auth/token.rs"]));
        c.reserve("carol", "ui", &files(&["src/ui/page.rs"]));

        // bob wants a DIFFERENT file in src/auth, with no import edges → same-dir predictions
        // with alice, and nothing about src/ui (different module).
        let pred = c.predict("bob", &files(&["src/auth/session.rs"]), &[], &[]);
        assert_eq!(pred.len(), 2, "both of alice's src/auth files are near; got {pred:?}");
        assert!(pred.iter().all(|p| p.agent == "alice" && p.relation == Relation::SameDir));

        // an exact-file want is a HARD conflict (reserve/peek), excluded from prediction
        let pred2 = c.predict("bob", &files(&["src/auth/login.rs"]), &[], &[]);
        assert!(
            pred2.iter().all(|p| p.held_file != "src/auth/login.rs"),
            "exact file is a hard conflict, not a prediction; got {pred2:?}"
        );
        // own reservations never predicted against self
        assert!(c.predict("alice", &files(&["src/auth/session.rs"]), &[], &[]).is_empty());
    }

    #[test]
    fn predict_flags_import_linked_files_across_directories() {
        let c = Coordinator::new();
        // alice holds a file bob's target IMPORTS, and one that IMPORTS bob's target — both in
        // OTHER directories, so the same-directory heuristic would miss them entirely.
        c.reserve("alice", "lib", &files(&["src/lib/util.rs", "src/api/handler.rs"]));

        // bob will edit src/core/engine.rs, which imports src/lib/util.rs and is imported by
        // src/api/handler.rs.
        let pred = c.predict(
            "bob",
            &files(&["src/core/engine.rs"]),
            &files(&["src/lib/util.rs"]),    // engine imports util
            &files(&["src/api/handler.rs"]), // handler imports engine
        );
        let by_file: std::collections::HashMap<_, _> =
            pred.iter().map(|p| (p.held_file.as_str(), p.relation)).collect();
        assert_eq!(by_file.get("src/lib/util.rs"), Some(&Relation::Imports), "target imports it: {pred:?}");
        assert_eq!(by_file.get("src/api/handler.rs"), Some(&Relation::ImportedBy), "it imports target: {pred:?}");
        assert_eq!(pred.len(), 2, "no same-dir here (engine.rs is alone in src/core); got {pred:?}");

        // a direct import edge outranks mere co-location: a held file that is BOTH in the target's
        // directory AND imported by the target reports as Imports, not SameDir.
        let c2 = Coordinator::new();
        c2.reserve("alice", "t", &files(&["src/core/dep.rs"]));
        let pred2 = c2.predict(
            "bob",
            &files(&["src/core/engine.rs"]),
            &files(&["src/core/dep.rs"]),
            &[],
        );
        assert_eq!(pred2.len(), 1);
        assert_eq!(pred2[0].relation, Relation::Imports, "import edge beats same-dir; got {pred2:?}");
    }

    #[test]
    fn predict_does_not_group_unrelated_top_level_files() {
        let c = Coordinator::new();
        // two other agents hold unrelated repo-root files; the same-dir fallback must NOT group all
        // top-level files together (dir == "" is not a module), so a plain top-level target with no
        // import edges predicts nothing.
        c.reserve("alice", "cfg", &files(&["webpack.config.ts"]));
        c.reserve("carol", "cfg", &files(&["vite.config.ts"]));
        assert!(
            c.predict("bob", &files(&["index.ts"]), &[], &[]).is_empty(),
            "unrelated root files must not be same-dir neighbors"
        );
        // but a genuine import edge between top-level files IS still predicted
        let pred = c.predict("bob", &files(&["index.ts"]), &files(&["webpack.config.ts"]), &[]);
        assert_eq!(pred.len(), 1);
        assert_eq!(pred[0].relation, Relation::Imports, "import edge still fires at top level; got {pred:?}");
    }

    #[test]
    fn same_agent_reserve_is_idempotent() {
        let c = Coordinator::new();
        assert!(c.reserve("alice", "t", &files(&["a.ts"])).is_empty());
        assert!(c.reserve("alice", "t", &files(&["a.ts"])).is_empty(), "re-reserving own file is fine");
        assert_eq!(c.held_count(), 1);
    }

    #[test]
    fn abandoned_reservations_expire_but_a_heartbeat_keeps_them() {
        // Generous margins (≥250ms against a 400ms ttl) so scheduler jitter on a loaded runner can't
        // flip an assertion — thread::sleep only guarantees a lower bound, so overshoot is the risk.
        let c = Coordinator::new().with_ttl(Duration::from_millis(400));
        c.reserve("alice", "t", &files(&["a.ts", "b.ts"]));
        assert_eq!(c.peek("bob", &files(&["a.ts"])).len(), 1, "held right after reserve");

        // a heartbeat well within the ttl refreshes ALL of alice's holds, not just one file
        std::thread::sleep(Duration::from_millis(150));
        c.heartbeat("alice");
        std::thread::sleep(Duration::from_millis(150)); // 300ms since reserve, only 150 since heartbeat
        assert_eq!(c.peek("bob", &files(&["a.ts", "b.ts"])).len(), 2, "heartbeat kept both holds alive");

        // stop heartbeating → both age out and free up
        std::thread::sleep(Duration::from_millis(650));
        assert_eq!(c.held_count(), 0, "abandoned holds swept after ttl");
        assert!(c.reserve("bob", "t2", &files(&["a.ts"])).is_empty(), "bob takes the freed file");
    }
}
