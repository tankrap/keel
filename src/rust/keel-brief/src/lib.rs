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

use keel_coord::{Conflict, PredictedConflict};
use keel_graph::LiveGraph;
use keel_resolve::{Resolve, Router, SliceDef};
use keel_store::{Object, ObjectId, Repo, Session, StoreError, Verification};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

// Re-export the field types so callers (e.g. the CLI) don't need every subcrate.
pub use keel_coord::Conflict as CoordConflict;
pub use keel_coord::{Coordinator, PredictedConflict as CoordPredicted};
pub use keel_resolve::SliceDef as ContextDef;

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
    /// whether this session recorded the context it was served — the feedback-edge signal
    /// (`context_served → change → verified`) the selector learns from.
    pub has_context: bool,
}

/// The fused response.
#[derive(Debug, Clone)]
pub struct Brief {
    pub task: String,
    pub file: String,
    pub symbol: Option<String>,
    /// task-relevant code (target + resolved cross-file callees), budget-bounded
    pub context: Vec<SliceDef>,
    /// why context is empty, if the slice couldn't run (missing symbol / resolver error).
    /// The brief still returns the other pillars rather than failing the whole fetch.
    pub context_error: Option<String>,
    /// files the target imports
    pub deps: Vec<String>,
    /// files that import the target (blast radius)
    pub rdeps: Vec<String>,
    /// verification of the file's most recent change (post-hoc CI result if recorded, else
    /// the change's committed state) — "is what I'm about to edit currently green?"
    pub verification: Verification,
    /// changes that modified this file, newest first
    pub provenance: Vec<Provenance>,
    /// files in the working set currently held by *other* agents (coordination). Empty
    /// when uncontended; non-empty means back off / pick other work.
    pub coordination: Vec<Conflict>,
    /// *predicted* soft conflicts — other agents working in the same directory as the target
    /// (likely to collide even if not the exact file). "Someone's in this module, spread out."
    pub predicted: Vec<PredictedConflict>,
    /// relevant prior sessions (with the lessons they recorded), from the graph
    /// neighborhood — the compounding flywheel: how related code was changed before.
    pub sessions: Vec<RelevantSession>,
    /// curated invariants pinned to symbols in play (target + sliced defs), always served —
    /// the cold-start lever: a single pin steers every future brief that touches the symbol.
    pub invariants: Vec<(String, String)>,
    /// estimated token size of `context`
    pub tokens: usize,
    /// true if the budget forced context to be trimmed
    pub truncated: bool,
}

impl Brief {
    /// Serialize to a stable JSON value (serde_json sorts object keys via BTreeMap, so the
    /// output is deterministic). Shared by the CLI and the daemon.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        json!({
            "task": self.task,
            "file": self.file,
            "symbol": self.symbol,
            "context": self.context.iter()
                .map(|d| json!({"file": d.file, "symbol": d.symbol, "text": d.text}))
                .collect::<Vec<_>>(),
            "context_error": self.context_error,
            "verification": match self.verification {
                Verification::Green => "green",
                Verification::Red => "red",
                Verification::Unverified => "unverified",
            },
            "deps": self.deps,
            "rdeps": self.rdeps,
            "provenance": self.provenance.iter()
                .map(|p| json!({"change": p.change, "intent": p.intent, "author": p.author, "verified": p.verified}))
                .collect::<Vec<_>>(),
            "coordination": self.coordination.iter()
                .map(|c| json!({"file": c.file, "agent": c.agent, "task": c.task}))
                .collect::<Vec<_>>(),
            "predicted": self.predicted.iter()
                .map(|p| json!({"held_file": p.held_file, "agent": p.agent, "task": p.task, "dir": p.dir}))
                .collect::<Vec<_>>(),
            "sessions": self.sessions.iter()
                .map(|s| json!({"change": s.change, "task": s.task, "lesson": s.lesson, "verified": s.verified, "has_context": s.has_context}))
                .collect::<Vec<_>>(),
            "invariants": self.invariants.iter()
                .map(|(sym, lesson)| json!({"symbol": sym, "lesson": lesson}))
                .collect::<Vec<_>>(),
            "tokens": self.tokens,
            "truncated": self.truncated,
        })
    }
}

pub struct BriefService {
    root: PathBuf,
    repo: Repo,
    graph: LiveGraph,
    resolver: Router,
    agent: String,
    coord: Coordinator,
}

impl BriefService {
    /// Open a brief service: `root` is the live working tree, `store_path` the object store
    /// (history), `sidecar_dir` holds the resolver scripts (`resolve.mjs`, `resolve-c.mjs`).
    /// A [`Router`] spawns the right sidecar per file lazily, so one service handles a
    /// mixed-language repo and drives both the graph and the slicer. Builds the graph on
    /// open. Defaults to agent `"local"` with a private coordinator; use [`Self::with_agent`]
    /// / [`Self::with_coordinator`] to join a shared multi-agent coordinator.
    pub fn open(root: &Path, store_path: &Path, sidecar_dir: &Path) -> io::Result<BriefService> {
        let repo = Repo::open(store_path).map_err(to_io)?;
        let mut resolver = Router::new(sidecar_dir);
        let mut graph = LiveGraph::new(root);
        // Load a persisted graph if present and reconcile it with the tree, so a cold brief
        // re-resolves only files that CHANGED rather than the whole repo (the dominant cost on a
        // large repo — the kernel is ~19s of whole-tree import resolution otherwise). First run
        // builds + saves; subsequent runs are a stat sweep. The cache lives under `.keel/` (git-
        // excluded); a stale/corrupt file just triggers re-resolution.
        let graph_path = root.join(".keel").join("graph");
        let had_cache = graph.load(&graph_path).is_ok();
        let changed = graph.refresh(&mut resolver)?;
        if !had_cache || !changed.is_empty() {
            let _ = graph.save(&graph_path); // best-effort persistence
        }
        Ok(BriefService {
            root: root.to_path_buf(),
            repo,
            graph,
            resolver,
            agent: "local".to_string(),
            coord: Coordinator::new(),
        })
    }

    pub fn with_agent(mut self, agent: &str) -> Self {
        self.agent = agent.to_string();
        self
    }

    /// Set the requesting agent for subsequent briefs. The daemon calls this per request (it
    /// holds ONE shared coordinator across all agents), so reservations/predictions are
    /// evaluated against the *right* agent — this is what makes multi-agent coordination work
    /// through the warm daemon rather than only within one process.
    pub fn set_agent(&mut self, agent: &str) {
        self.agent = agent.to_string();
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

    /// Start fs-watching the working tree so [`refresh`](Self::refresh) becomes O(changed)
    /// instead of walking the whole tree. Best called once right after opening (the warm
    /// daemon does this); if unsupported it just leaves refresh on its full-walk path.
    pub fn watch(&mut self) -> io::Result<()> {
        self.graph.watch()
    }

    /// Bring the graph up to the current working-tree state (incremental).
    pub fn refresh(&mut self) -> io::Result<()> {
        self.graph.refresh(&mut self.resolver)?;
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
        let (full, context_error) = match symbol {
            Some(sym) => match self.resolver.slice(&self.root, file, sym, 1) {
                Ok(defs) => (defs, None),
                // a missing symbol / resolver hiccup degrades to empty context (with a
                // reason); deps + provenance + coordination + sessions still come back.
                Err(e) => (Vec::new(), Some(e.to_string())),
            },
            None => (Vec::new(), None),
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
        let predicted = self.coord.predict(&self.agent, &working_set);

        // history of the target, newest first; verification prefers the post-hoc side-table
        // (CI result) over the change's committed state.
        let touching = self.repo.history_touching(file).map_err(to_io)?;
        let verify_of = |id: &ObjectId, baked: Verification| -> Verification {
            match self.repo.store().verification(id) {
                Ok(Verification::Unverified) => baked, // nothing recorded → fall back to committed
                Ok(v) => v,
                Err(_) => baked,
            }
        };
        let verification = touching
            .first()
            .map(|(id, c)| verify_of(id, c.verification))
            .unwrap_or(Verification::Unverified);
        let provenance = touching
            .iter()
            .take(5)
            .map(|(id, c)| Provenance {
                change: id.to_hex(),
                intent: c.intent.clone(),
                author: c.author.clone(),
                verified: matches!(verify_of(id, c.verification), Verification::Green),
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
                            has_context: s.context_served.is_some(),
                        });
                    }
                }
            }
        }
        sessions.truncate(5);

        // pinned invariants for the symbols in play (target + sliced defs) — always served,
        // regardless of history, so a single curated pin steers every relevant future brief.
        let mut inv_syms: Vec<String> = symbol.map(str::to_string).into_iter().collect();
        inv_syms.extend(context.iter().map(|d| d.symbol.clone()));
        inv_syms.sort();
        inv_syms.dedup();
        let mut invariants = Vec::new();
        for sym in &inv_syms {
            if let Some(lesson) = self.repo.store().pin(sym).map_err(to_io)? {
                invariants.push((sym.clone(), lesson));
            }
        }

        Ok(Brief {
            task: task.to_string(),
            file: file.to_string(),
            symbol: symbol.map(str::to_string),
            context,
            context_error,
            verification,
            deps,
            rdeps,
            provenance,
            coordination,
            predicted,
            sessions,
            invariants,
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

    fn sidecar_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar")
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

        let mut svc = match BriefService::open(&work, &store, &sidecar_dir()) {
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
        let mut svc = match BriefService::open(&work, &store, &sidecar_dir()) {
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

        let mut svc = match BriefService::open(&work, &store, &sidecar_dir()) {
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
