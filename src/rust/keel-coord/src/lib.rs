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
use std::time::{Duration, Instant};

/// How long a reservation lives without renewal before it's considered abandoned and swept. Agents
/// renew implicitly every time they re-fetch a brief (`reserve`) or explicitly via `renew`, so an
/// actively-worked hold never expires; a crashed or wandered-off agent's holds free themselves after
/// this, instead of blocking others forever as stale conflicts.
const DEFAULT_TTL: Duration = Duration::from_secs(300);

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

    /// Heartbeat: refresh the TTL on `files` still held by `agent`, without taking new ones — for a
    /// long task that keeps working without re-fetching a brief. Files it no longer holds are ignored.
    pub fn renew(&self, agent: &str, files: &[String]) {
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        let now = Instant::now();
        for f in files {
            if let Some(r) = reg.held.get_mut(f) {
                if r.agent == agent {
                    r.at = now;
                }
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

    /// Release everything held by `agent` (e.g. when its work lands).
    pub fn release_agent(&self, agent: &str) {
        self.inner.lock().unwrap().held.retain(|_, r| r.agent != agent);
    }

    /// Release specific `files` held by `agent`.
    pub fn release_files(&self, agent: &str, files: &[String]) {
        let mut reg = self.inner.lock().unwrap();
        for f in files {
            if reg.held.get(f).is_some_and(|r| r.agent == agent) {
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
        let mut reg = self.inner.lock().unwrap();
        sweep(&mut reg, self.ttl);
        let want: HashSet<&str> = files.iter().map(String::as_str).collect();
        let want_dirs: HashSet<&str> = files.iter().map(|f| dir_of(f)).collect();
        let mut out: Vec<PredictedConflict> = reg
            .held
            .iter()
            .filter(|(held, r)| r.agent != agent && !want.contains(held.as_str()))
            .filter(|(held, _)| want_dirs.contains(dir_of(held)))
            .map(|(held, r)| PredictedConflict {
                held_file: held.clone(),
                agent: r.agent.clone(),
                task: r.task.clone(),
                dir: dir_of(held).to_string(),
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

    #[test]
    fn abandoned_reservations_expire_but_a_heartbeat_keeps_them() {
        let ttl = Duration::from_millis(100);
        let c = Coordinator::new().with_ttl(ttl);
        c.reserve("alice", "t", &files(&["a.ts"]));
        assert_eq!(c.peek("bob", &files(&["a.ts"])).len(), 1, "held right after reserve");

        // renew before expiry keeps it alive past the original TTL window
        std::thread::sleep(Duration::from_millis(60));
        c.renew("alice", &files(&["a.ts"]));
        std::thread::sleep(Duration::from_millis(60)); // 120ms since reserve, but only 60 since renew
        assert_eq!(c.peek("bob", &files(&["a.ts"])).len(), 1, "heartbeat kept the hold alive");

        // stop renewing → it's reclaimed, so bob can take it
        std::thread::sleep(Duration::from_millis(160));
        assert_eq!(c.held_count(), 0, "abandoned hold swept after ttl");
        assert!(c.reserve("bob", "t2", &files(&["a.ts"])).is_empty(), "bob takes the freed file");
    }
}
