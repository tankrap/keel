//! Coordination for many agents on shared code (Linear NEW-1092).
//!
//! The model is **prevent-not-detect**, piggybacked on the brief: when an agent fetches
//! a brief for a task it reserves the files it's about to edit. If another agent already
//! holds one, that's returned as a **conflict** (the caller redirects to other work)
//! rather than both editing and merge-conflicting later. Uncontended reservations are
//! free — a hashmap insert on the fetch the agent already makes.
//!
//! This first cut is an in-process registry behind a mutex (the single-daemon model). The
//! ordered-authority / subgraph-overlap prediction is a later layer.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// A file wanted by the caller but currently held by another agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub file: String,
    pub agent: String,
    pub task: String,
}

/// A *predicted* (soft) conflict: another agent holds a file in the same directory as one the
/// caller intends to touch — not the exact file, but close enough to likely collide. Surfaced
/// so a fleet spreads across modules instead of piling into the same area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedConflict {
    pub held_file: String,
    pub agent: String,
    pub task: String,
    pub dir: String,
}

#[derive(Default)]
struct Registry {
    held: HashMap<String, (String, String)>, // file → (agent, task)
}

/// A shared reservation registry. Cheap to clone (shares one registry), so every agent's
/// brief service holds a handle to the same coordinator.
#[derive(Clone, Default)]
pub struct Coordinator {
    inner: Arc<Mutex<Registry>>,
}

impl Coordinator {
    pub fn new() -> Coordinator {
        Coordinator::default()
    }

    /// Reserve `files` for `agent`/`task`. Files held by a *different* agent are returned
    /// as conflicts and left with their current holder (the caller backs off). Free files
    /// — or files already held by `agent` — are reserved to `agent`. Idempotent.
    pub fn reserve(&self, agent: &str, task: &str, files: &[String]) -> Vec<Conflict> {
        let mut reg = self.inner.lock().unwrap();
        let mut conflicts = Vec::new();
        for f in files {
            match reg.held.get(f) {
                Some((a, t)) if a != agent => {
                    conflicts.push(Conflict { file: f.clone(), agent: a.clone(), task: t.clone() });
                }
                _ => {
                    reg.held.insert(f.clone(), (agent.to_string(), task.to_string()));
                }
            }
        }
        conflicts
    }

    /// Who (other than `agent`) currently holds any of `files` — no reservation taken.
    pub fn peek(&self, agent: &str, files: &[String]) -> Vec<Conflict> {
        let reg = self.inner.lock().unwrap();
        files
            .iter()
            .filter_map(|f| match reg.held.get(f) {
                Some((a, t)) if a != agent => {
                    Some(Conflict { file: f.clone(), agent: a.clone(), task: t.clone() })
                }
                _ => None,
            })
            .collect()
    }

    /// Release everything held by `agent` (e.g. when its work lands).
    pub fn release_agent(&self, agent: &str) {
        self.inner.lock().unwrap().held.retain(|_, (a, _)| a != agent);
    }

    /// Release specific `files` held by `agent`.
    pub fn release_files(&self, agent: &str, files: &[String]) {
        let mut reg = self.inner.lock().unwrap();
        for f in files {
            if reg.held.get(f).is_some_and(|(a, _)| a == agent) {
                reg.held.remove(f);
            }
        }
    }

    /// Predict soft conflicts for `files`: reservations by *other* agents that share a
    /// directory with any requested file (excluding exact-file collisions, which `reserve`
    /// and `peek` already report as hard conflicts). This is the "someone is already working
    /// in this module — consider elsewhere" signal that lets a fleet self-spread. The
    /// coordinator is the single ordered authority, so this view is consistent by construction.
    pub fn predict(&self, agent: &str, files: &[String]) -> Vec<PredictedConflict> {
        let reg = self.inner.lock().unwrap();
        let want: HashSet<&str> = files.iter().map(String::as_str).collect();
        let want_dirs: HashSet<&str> = files.iter().map(|f| dir_of(f)).collect();
        let mut out: Vec<PredictedConflict> = reg
            .held
            .iter()
            .filter(|(held, (a, _))| a != agent && !want.contains(held.as_str()))
            .filter(|(held, _)| want_dirs.contains(dir_of(held)))
            .map(|(held, (a, t))| PredictedConflict {
                held_file: held.clone(),
                agent: a.clone(),
                task: t.clone(),
                dir: dir_of(held).to_string(),
            })
            .collect();
        out.sort_by(|a, b| a.held_file.cmp(&b.held_file));
        out
    }

    pub fn held_count(&self) -> usize {
        self.inner.lock().unwrap().held.len()
    }
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
        c.release_files("alice", &files(&["a.ts"]));
        // a.ts free → bob takes it; b.ts still alice's → conflict
        let conf = c.reserve("bob", "t2", &files(&["a.ts", "b.ts"]));
        assert_eq!(conf, vec![Conflict { file: "b.ts".into(), agent: "alice".into(), task: "t".into() }]);
        c.release_agent("alice");
        assert!(c.reserve("bob", "t2", &files(&["b.ts"])).is_empty());
    }

    #[test]
    fn predict_warns_on_same_directory_not_exact_file() {
        let c = Coordinator::new();
        c.reserve("alice", "auth", &files(&["src/auth/login.rs", "src/auth/token.rs"]));
        c.reserve("carol", "ui", &files(&["src/ui/page.rs"]));

        // bob wants a DIFFERENT file in src/auth → predicted (soft) conflict with alice,
        // and nothing about src/ui (different module).
        let pred = c.predict("bob", &files(&["src/auth/session.rs"]));
        assert_eq!(pred.len(), 2, "both of alice's src/auth files are near; got {pred:?}");
        assert!(pred.iter().all(|p| p.agent == "alice" && p.dir == "src/auth"));

        // an exact-file want is a HARD conflict (reserve/peek), excluded from prediction
        let pred2 = c.predict("bob", &files(&["src/auth/login.rs"]));
        assert!(
            pred2.iter().all(|p| p.held_file != "src/auth/login.rs"),
            "exact file is a hard conflict, not a prediction; got {pred2:?}"
        );
        // own reservations never predicted against self
        assert!(c.predict("alice", &files(&["src/auth/session.rs"])).is_empty());
    }

    #[test]
    fn same_agent_reserve_is_idempotent() {
        let c = Coordinator::new();
        assert!(c.reserve("alice", "t", &files(&["a.ts"])).is_empty());
        assert!(c.reserve("alice", "t", &files(&["a.ts"])).is_empty(), "re-reserving own file is fine");
        assert_eq!(c.held_count(), 1);
    }
}
