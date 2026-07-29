//! The fused fetch — `keel brief`.
//!
//! One call returns everything an agent needs to act on a task, from the **live working
//! tree** (Linear NEW-1082):
//!   1. **context**    — the task-relevant code (import-aware symbol slice), budget-bounded
//!   2. **deps/rdeps** — the target's dependency neighborhood and blast radius (live graph)
//!   3. **provenance** — how this file was changed before (which changes touched it)
//!
//! This is the fusion the whole thesis rests on: a static index, a review layer, or a git
//! wrapper can each supply one of these, but only a substrate that owns the write path and
//! the graph and the history can return all three, consistent, in one fetch. Coordination
//! and relevant-prior-sessions are the next fields to fuse in (NEW-1092 / NEW-1076).

use keel_coord::{Conflict, Coordinator};
use keel_graph::LiveGraph;
use keel_resolve::{Sidecar, SliceDef};
use keel_store::{Object, ObjectId, Repo, Session, StoreError, Verification};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// A single change that touched the briefed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub change: String, // hex address
    pub intent: String,
    pub author: String,
    pub verified: bool,
}

/// A relevant prior session surfaced for this task — retrieved from the graph neighborhood
/// (a change that touched the target file or one of its deps/rdeps, including cross-file).
/// Its `lesson` is the non-obvious constraint that makes the next agent correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelevantSession {
    pub change: String,
    pub task: String,
    pub lesson: String,
    pub verified: bool,
}

/// The fused response.
#[derive(Debug, Clone)]
pub struct Brief {
    pub task: String,
    pub file: String,
    pub symbol: Option<String>,
    /// task-relevant code (target + resolved cross-file callees), budget-bounded
    pub context: Vec<SliceDef>,
    /// files the target imports
    pub deps: Vec<String>,
    /// files that import the target (blast radius)
    pub rdeps: Vec<String>,
    /// changes that modified this file, newest first
    pub provenance: Vec<Provenance>,
    /// files in the working set currently held by *other* agents (coordination). Empty
    /// when uncontended; non-empty means back off / pick other work.
    pub coordination: Vec<Conflict>,
    /// relevant prior sessions (with the lessons they recorded), from the graph
    /// neighborhood — the compounding flywheel: how related code was changed before.
    pub sessions: Vec<RelevantSession>,
    /// estimated token size of `context`
    pub tokens: usize,
    /// true if the budget forced context to be trimmed
    pub truncated: bool,
}

pub struct BriefService {
    root: PathBuf,
    repo: Repo,
    graph: LiveGraph,
    slicer: Sidecar,
    agent: String,
    coord: Coordinator,
}

impl BriefService {
    /// Open a brief service: `root` is the live working tree, `store_path` the object
    /// store (history), `script` the resolver sidecar. Builds the graph on open. Defaults
    /// to agent `"local"` with a private coordinator; use [`Self::with_agent`] /
    /// [`Self::with_coordinator`] to join a shared multi-agent coordinator.
    pub fn open(root: &Path, store_path: &Path, script: &Path) -> io::Result<BriefService> {
        let repo = Repo::open(store_path).map_err(to_io)?;
        let mut graph = LiveGraph::open(root, script)?;
        graph.build()?;
        let slicer = Sidecar::spawn(script)?;
        Ok(BriefService {
            root: root.to_path_buf(),
            repo,
            graph,
            slicer,
            agent: "local".to_string(),
            coord: Coordinator::new(),
        })
    }

    pub fn with_agent(mut self, agent: &str) -> Self {
        self.agent = agent.to_string();
        self
    }

    /// Join a shared coordinator so this agent sees (and takes) reservations against others.
    pub fn with_coordinator(mut self, coord: Coordinator) -> Self {
        self.coord = coord;
        self
    }

    /// Commit the current working tree (so future briefs have provenance).
    pub fn commit(&mut self, intent: &str, author: &str, timestamp: u64) -> io::Result<ObjectId> {
        self.repo.commit_dir(&self.root, intent, author, timestamp, None).map_err(to_io)
    }

    /// Commit the working tree together with the agent `session` that produced it — the
    /// session is stored and linked to the change (total provenance + flywheel fuel).
    pub fn commit_with_session(
        &mut self,
        intent: &str,
        author: &str,
        timestamp: u64,
        session: Session,
    ) -> io::Result<ObjectId> {
        let sid = self.repo.store().put(&Object::Session(session)).map_err(to_io)?;
        self.repo.commit_dir(&self.root, intent, author, timestamp, Some(sid)).map_err(to_io)
    }

    /// Bring the graph up to the current working-tree state (incremental).
    pub fn refresh(&mut self) -> io::Result<()> {
        self.graph.refresh()?;
        Ok(())
    }

    /// The fused fetch. `budget_tokens` caps `context` (the target definition is always
    /// kept even if it alone exceeds the budget). If `reserve` is true, the working set
    /// (the target file) is reserved for this agent; either way, files in the working set
    /// held by *other* agents are reported in `coordination`.
    pub fn brief(
        &mut self,
        task: &str,
        file: &str,
        symbol: Option<&str>,
        budget_tokens: usize,
        reserve: bool,
    ) -> io::Result<Brief> {
        let full = match symbol {
            Some(sym) => self.slicer.slice(&self.root, file, sym, 1)?,
            None => Vec::new(),
        };
        let full_len = full.len();

        // budget-bound: keep definitions until the budget is hit, always keep the target.
        let mut tokens = 0usize;
        let mut context = Vec::new();
        for d in full {
            let t = d.text.len() / 4;
            if !context.is_empty() && tokens + t > budget_tokens {
                break;
            }
            tokens += t;
            context.push(d);
        }
        let truncated = context.len() < full_len;

        let deps = self.graph.deps(file);
        let rdeps = self.graph.rdeps(file);

        // coordination: the working set the agent will edit (the target file). Reserve it
        // (piggybacked) or just peek — either way surface files held by other agents.
        let working_set = [file.to_string()];
        let coordination = if reserve {
            self.coord.reserve(&self.agent, task, &working_set)
        } else {
            self.coord.peek(&self.agent, &working_set)
        };

        let provenance = self
            .repo
            .history_touching(file)
            .map_err(to_io)?
            .into_iter()
            .take(5)
            .map(|(id, c)| Provenance {
                change: id.to_hex(),
                intent: c.intent,
                author: c.author,
                verified: matches!(c.verification, Verification::Green),
            })
            .collect();

        // relevant prior sessions: sessions of changes that touched the target file OR its
        // graph neighborhood (deps + rdeps) — this is how cross-file retrieval happens (a
        // session that touched a dependency is surfaced for a task on the dependent). Dedup
        // by session; keep only those that recorded a lesson.
        let mut neighborhood = vec![file.to_string()];
        neighborhood.extend(deps.iter().cloned());
        neighborhood.extend(rdeps.iter().cloned());
        let mut seen = HashSet::new();
        let mut sessions = Vec::new();
        for f in &neighborhood {
            for (cid, c) in self.repo.history_touching(f).map_err(to_io)? {
                let Some(sid) = c.session else { continue };
                if !seen.insert(sid) {
                    continue;
                }
                if let Some(Object::Session(s)) = self.repo.store().get(&sid).map_err(to_io)? {
                    if !s.lesson.is_empty() {
                        // "verified" reflects whether THIS session's own work passed — the
                        // change is Unverified until tests run; the session carries the result.
                        let verified = matches!(s.verification, Verification::Green);
                        sessions.push(RelevantSession {
                            change: cid.to_hex(),
                            task: s.task,
                            lesson: s.lesson,
                            verified,
                        });
                    }
                }
            }
        }
        sessions.truncate(5);

        Ok(Brief {
            task: task.to_string(),
            file: file.to_string(),
            symbol: symbol.map(str::to_string),
            context,
            deps,
            rdeps,
            provenance,
            coordination,
            sessions,
            tokens,
            truncated,
        })
    }
}

fn to_io(e: StoreError) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("keel-brief-{tag}-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar/resolve.mjs")
    }

    #[test]
    fn brief_fuses_context_graph_and_provenance() {
        let work = tmp("work");
        let store = tmp("store");
        fs::write(work.join("b.ts"), "export function helper(x: number): number {\n  return x * 2;\n}\n").unwrap();
        fs::write(
            work.join("a.ts"),
            "import { helper } from './b.js';\nexport function doA(): number {\n  return helper(21);\n}\n",
        )
        .unwrap();

        let mut svc = match BriefService::open(&work, &store, &script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping brief test: {e}");
                return;
            }
        };
        // two commits so provenance has history (c1 adds a.ts, c2 modifies it)
        let _c1 = svc.commit("initial", "acct:x", 1).unwrap();
        fs::write(
            work.join("a.ts"),
            "import { helper } from './b.js';\nexport function doA(): number {\n  return helper(21) + helper(1);\n}\n",
        )
        .unwrap();
        svc.refresh().unwrap();
        let _c2 = svc.commit("tweak doA", "acct:x", 2).unwrap();

        let brief = svc.brief("understand doA", "a.ts", Some("doA"), 100_000, false).unwrap();

        // 1. context: the slice resolved doA + its cross-file callee helper (through './b.js')
        assert!(brief.context.iter().any(|d| d.symbol == "doA"), "context has target");
        assert!(
            brief.context.iter().any(|d| d.symbol == "helper" && d.file == "b.ts"),
            "context resolved cross-file callee; got {:?}",
            brief.context
        );
        // 2. graph: a.ts depends on b.ts
        assert!(brief.deps.contains(&"b.ts".to_string()), "deps has b.ts; got {:?}", brief.deps);
        assert!(!brief.rdeps.contains(&"a.ts".to_string()), "nothing imports a.ts");
        // 3. provenance: both commits touched a.ts, newest first
        assert_eq!(brief.provenance.len(), 2, "two changes touched a.ts");
        assert_eq!(brief.provenance[0].intent, "tweak doA");
        assert_eq!(brief.provenance[1].intent, "initial");
        assert!(!brief.truncated);
        assert!(brief.coordination.is_empty(), "uncontended → no coordination conflicts");

        // budget bound: a tiny budget keeps only the target and marks truncated
        let tight = svc.brief("understand doA", "a.ts", Some("doA"), 1, false).unwrap();
        assert_eq!(tight.context.len(), 1, "budget keeps only the target def");
        assert!(tight.truncated, "budget forced truncation");

        let _ = fs::remove_dir_all(&work);
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn brief_surfaces_and_takes_reservations() {
        let work = tmp("cwork");
        let store = tmp("cstore");
        fs::write(work.join("x.ts"), "export function f() { return 1; }\n").unwrap();
        fs::write(work.join("y.ts"), "export function g() { return 2; }\n").unwrap();

        let coord = Coordinator::new();
        let mut svc = match BriefService::open(&work, &store, &script()) {
            Ok(s) => s.with_agent("me").with_coordinator(coord.clone()),
            Err(e) => {
                eprintln!("skipping coord test: {e}");
                return;
            }
        };
        // another agent already holds x.ts
        coord.reserve("other", "their task", &["x.ts".to_string()]);

        // brief on x.ts (reserve) → reports the conflict, does NOT take it
        let b = svc.brief("edit x", "x.ts", None, 10_000, true).unwrap();
        assert_eq!(b.coordination.len(), 1);
        assert_eq!(b.coordination[0].agent, "other");
        assert_eq!(b.coordination[0].file, "x.ts");

        // brief on the free y.ts (reserve) → no conflict, and now held by "me"
        let b2 = svc.brief("edit y", "y.ts", None, 10_000, true).unwrap();
        assert!(b2.coordination.is_empty());
        assert_eq!(coord.peek("bystander", &["y.ts".to_string()]).len(), 1, "y.ts is now reserved");

        let _ = fs::remove_dir_all(&work);
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn brief_retrieves_relevant_prior_sessions_incl_cross_file() {
        let work = tmp("swork");
        let store = tmp("sstore");
        fs::write(work.join("b.ts"), "export function helper(x: number): number {\n  return x * 2;\n}\n").unwrap();
        fs::write(
            work.join("a.ts"),
            "import { helper } from './b.js';\nexport function doA(): number {\n  return helper(21);\n}\n",
        )
        .unwrap();

        let mut svc = match BriefService::open(&work, &store, &script()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping session test: {e}");
                return;
            }
        };
        // c1 adds both files (no session)
        svc.commit("initial", "acct", 1).unwrap();
        // c2 modifies ONLY a.ts, carrying a session that recorded a lesson
        fs::write(
            work.join("a.ts"),
            "import { helper } from './b.js';\nexport function doA(): number {\n  return helper(21) + 1;\n}\n",
        )
        .unwrap();
        svc.refresh().unwrap();
        let session = Session {
            task: "tune doA".into(),
            model: "claude-opus-4-8".into(),
            lesson: "helper must receive a settled value first".into(),
            prompts: None,
            context_served: None,
            tool_calls: vec![],
            tool_results: vec![],
            verification: Verification::Green,
            tokens_in: 0,
            tokens_out: 0,
        };
        svc.commit_with_session("tune a", "acct", 2, session).unwrap();

        // same-file: a brief on a.ts surfaces the session that touched a.ts
        let b = svc.brief("work on doA", "a.ts", Some("doA"), 100_000, false).unwrap();
        assert!(
            b.sessions.iter().any(|s| s.lesson.contains("settled value") && s.verified),
            "same-file session retrieval; got {:?}",
            b.sessions
        );

        // CROSS-FILE: b.ts was untouched by c2, but a.ts (its rdep) carries the session —
        // a brief on b.ts surfaces it via the graph neighborhood.
        let b2 = svc.brief("work on helper", "b.ts", Some("helper"), 100_000, false).unwrap();
        assert!(
            b2.sessions.iter().any(|s| s.lesson.contains("settled value")),
            "cross-file retrieval via rdeps; got {:?}",
            b2.sessions
        );

        let _ = fs::remove_dir_all(&work);
        let _ = fs::remove_dir_all(&store);
    }
}
