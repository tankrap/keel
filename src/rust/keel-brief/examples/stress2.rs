//! Round 2 stress — target the higher layers (coordination, history, brief edges).
//! Run: `cargo run --release -p keel-brief --example stress2`

use keel_brief::{BriefService, Coordinator};
use keel_store::{Object, Repo, Session, Verification};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn td(tag: &str) -> PathBuf {
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("keel-stress2-{tag}-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
fn script() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../keel-resolve/sidecar"))
}

fn main() {
    let mut pass = 0;
    let mut weak = 0;
    macro_rules! stage {
        ($name:expr, $body:expr) => {{
            println!("▶ {}", $name);
            let t = Instant::now();
            let r: Result<String, String> = $body;
            match r {
                Ok(m) => { pass += 1; println!("  ✓ {m}  [{:.2}s]", t.elapsed().as_secs_f64()); }
                Err(e) => { weak += 1; println!("  ✗ WEAKNESS: {e}  [{:.2}s]", t.elapsed().as_secs_f64()); }
            }
        }};
    }

    stage!("coordination: 100 threads reserve 10 unique files each (no loss)", coord_concurrent());
    stage!("coordination: 200 threads race the SAME file (exactly one wins)", coord_contention());
    stage!("long history: 600 commits, then history_touching + brief", long_history());
    stage!("brief edge cases (missing symbol/file, budget 0)", brief_edges());

    println!("\n=== stress2 summary: {pass} passed, {weak} weakness(es) ===");
}

fn coord_concurrent() -> Result<String, String> {
    let c = Coordinator::new();
    let handles: Vec<_> = (0..100)
        .map(|t| {
            let c = c.clone();
            std::thread::spawn(move || {
                let files: Vec<String> = (0..10).map(|i| format!("t{t}_f{i}.ts")).collect();
                let conflicts = c.reserve(&format!("agent{t}"), "task", &files);
                assert!(conflicts.is_empty(), "unique files should never conflict");
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "a coordination thread panicked".to_string())?;
    }
    let held = c.held_count();
    if held != 1000 {
        return Err(format!("expected 1000 reservations, got {held} (lost under concurrency)"));
    }
    for t in 0..100 {
        c.release_agent(&format!("agent{t}"));
    }
    if c.held_count() != 0 {
        return Err("release_agent left reservations behind".into());
    }
    Ok("1000 concurrent reservations all landed, all released".into())
}

fn coord_contention() -> Result<String, String> {
    let c = Coordinator::new();
    let winners = std::sync::Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..200)
        .map(|t| {
            let c = c.clone();
            let w = winners.clone();
            std::thread::spawn(move || {
                let conflicts = c.reserve(&format!("a{t}"), "task", &["hot.ts".to_string()]);
                if conflicts.is_empty() {
                    w.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "thread panicked".to_string())?;
    }
    let w = winners.load(Ordering::SeqCst);
    if w == 1 && c.held_count() == 1 {
        Ok("200 threads raced one file → exactly 1 winner (no double-grant)".into())
    } else {
        Err(format!("race broken: {w} winners, held={} (should be 1/1)", c.held_count()))
    }
}

fn long_history() -> Result<String, String> {
    let sd = td("hist-store");
    let work = td("hist-work");
    let repo = Repo::open(&sd).map_err(|e| e.to_string())?;
    for i in 0..600 {
        std::fs::write(work.join("f.ts"), format!("export const v = {i};\n")).unwrap();
        let sess = if i % 50 == 0 {
            let s = Session {
                task: format!("commit {i}"),
                model: "cli".into(),
                lesson: format!("lesson at {i}"),
                prompts: None,
                context_served: None,
                tool_calls: vec![],
                tool_results: vec![],
                verification: Verification::Green,
                tokens_in: 0,
                tokens_out: 0,
            };
            Some(repo.store().put(&Object::Session(s)).map_err(|e| e.to_string())?)
        } else {
            None
        };
        repo.commit_dir(&work, &format!("c{i}"), "acct", i, sess).map_err(|e| e.to_string())?;
    }
    let t = Instant::now();
    let touched = repo.history_touching("f.ts").map_err(|e| e.to_string())?;
    let ht_ms = t.elapsed().as_secs_f64() * 1000.0;
    let _ = std::fs::remove_dir_all(&sd);
    let _ = std::fs::remove_dir_all(&work);
    // f.ts changed every commit → 600 touching entries
    if touched.len() == 600 {
        Ok(format!("600-commit history: history_touching walked all 600 in {ht_ms:.0}ms"))
    } else {
        Err(format!("history_touching wrong: {} entries (expected 600), {ht_ms:.0}ms", touched.len()))
    }
}

fn brief_edges() -> Result<String, String> {
    let work = td("edge-work");
    let store = td("edge-store");
    std::fs::write(work.join("a.ts"), "export function doA(): number { return 1; }\n").unwrap();
    let mut svc = match BriefService::open(&work, &store, &script()) {
        Ok(s) => s,
        Err(e) => return Err(format!("open failed: {e}")),
    };
    svc.commit("init", "acct", 1).map_err(|e| e.to_string())?;

    // missing symbol → degrades to empty context + a reason, does NOT fail the fetch
    let b1 = svc.brief("t", "a.ts", Some("doesNotExist"), 8000, false).map_err(|e| format!("missing-symbol brief STILL errored (should degrade): {e}"))?;
    if !b1.context.is_empty() || b1.context_error.is_none() {
        return Err(format!("missing symbol: expected empty context + reason; got {} defs, err={:?}", b1.context.len(), b1.context_error));
    }
    // missing file → empty everything, no crash
    let _b2 = svc.brief("t", "nope/missing.ts", Some("x"), 8000, false).map_err(|e| format!("missing-file brief errored: {e}"))?;
    // budget 0 → target kept, truncated
    let b3 = svc.brief("t", "a.ts", Some("doA"), 0, false).map_err(|e| format!("budget-0 brief errored: {e}"))?;
    // symbol None → no slice, still returns
    let _b4 = svc.brief("t", "a.ts", None, 8000, false).map_err(|e| format!("no-symbol brief errored: {e}"))?;

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&store);
    Ok(format!("all edge cases handled without crash (budget-0 kept {} def, truncated={})", b3.context.len(), b3.truncated))
}
