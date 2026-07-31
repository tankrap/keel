#!/usr/bin/env python3
"""
Real-corpus flywheel benchmark — Rust (Tokio). Fourth language, alongside VS Code (TS), Django (PY)
and Prometheus (Go), and the language keel itself is written in — a project a real team would build
on with agents.

Same method as corpus_bench.py: 12 real, idiomatic Tokio conventions grounded by their frequency in
a fresh tokio-rs/tokio checkout (io::Result 1457, ready! 418, #[tokio::test] 953, assert_ready/pending
1033, assert_ok/err 610, loom 484, #[track_caller] 200, tokio::pin! 202, # Panics 139, // SAFETY: 145,
# Cancel safety 125, tracing:: 77). Each attached to the real Tokio file it governs; retrieved by
`keel brief`. Solve WITHOUT vs WITH, T trials; dual judge (Sonnet 5) scores compliance.
Solver = Opus 4.8.

Env: CORPUS_SRC (a tokio checkout, default ~/keel-rust-demo), TRIALS (3), WORKERS (6).
"""
import json, os, subprocess, tempfile, shutil, pathlib

from bench_common import (api, judge, sh, run_trials, build_summary,
                          SOLVER, JUDGE, TRIALS, WORKERS)

CORPUS = os.environ.get("CORPUS_SRC", str(pathlib.Path.home() / "keel-rust-demo"))

# (real tokio file, module hint, task, real grounded convention, judge compliance-criterion)
SCEN = [
 ("tokio/src/io/util/async_read_ext.rs", "the async read extension utilities",
  "Implement a poll method poll_fill(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> that polls an inner reader (self.inner.poll_read(cx, buf)) and only proceeds once it is ready.",
  "In poll functions, unwrap a Poll with the ready!(...) macro, which returns early on Poll::Pending; never hand-write `match ... { Poll::Pending => return Poll::Pending, Poll::Ready(v) => v }`.",
  "uses the ready!(...) macro to poll the inner reader; does NOT hand-roll a match that returns Poll::Pending"),
 ("tokio/src/sync/mpsc/bounded.rs", "the bounded mpsc channel",
  "Write the doc comment and signature for an async method `recv(&mut self) -> Option<T>` on the Receiver, including how it behaves if the returned future is dropped mid-await.",
  "Async methods document their behavior under cancellation in a `# Cancel safety` section of the doc comment.",
  "the doc comment includes a `# Cancel safety` section describing cancellation behavior"),
 ("tokio/src/fs/file.rs", "the async file module",
  "Implement an async method read_exact_at(&mut self, len: usize) that reads exactly len bytes from the file and returns them, or an error.",
  "Fallible I/O functions return std::io::Result<T> (io::Result<T>); never a custom error type or Result<T, String> for I/O.",
  "the function returns io::Result<...> (std::io::Result); does NOT use a custom error type or Result<_, String>"),
 ("tokio/src/time/sleep.rs", "the sleep/timer module",
  "Implement a helper fn require_positive(dur: Duration) that panics with a clear message if the duration is zero.",
  "Helper functions that panic on behalf of their caller are annotated #[track_caller] so the panic location points at the caller, not the helper.",
  "the function is annotated with #[track_caller]"),
 ("tokio/src/sync/oneshot.rs", "the oneshot channel",
  "Write a test that creates a oneshot channel, sends a value, awaits it on the receiver, and asserts the received value.",
  "Async tests use the #[tokio::test] attribute; never #[test] with a manually constructed runtime or block_on.",
  "the test uses #[tokio::test]; does NOT use #[test] with a hand-built runtime or block_on"),
 ("tokio/src/sync/notify.rs", "the Notify primitive",
  "Write a test that polls a notified() future (expecting it pending), then notifies, then expects the future ready.",
  "In poll-based tests, assert readiness with the tokio-test macros assert_pending!(...) and assert_ready!(...); never a manual match on Poll or assert!(matches!(...)).",
  "uses assert_pending!(...) and assert_ready!(...); does NOT hand-roll a match on Poll"),
 ("tokio/src/net/tcp/stream.rs", "the TCP stream",
  "Write a test asserting that a fallible call returning io::Result (for example stream.set_nodelay(true)) succeeds.",
  "In tests, unwrap-and-assert Result values with tokio-test's assert_ok!(...) / assert_err!(...); never a bare .unwrap() or assert!(res.is_ok()).",
  "uses assert_ok!(...) (or assert_err!) rather than .unwrap() or assert!(res.is_ok())"),
 ("tokio/src/sync/mutex.rs", "the async Mutex",
  "Write a concurrency test in which two tasks contend on the mutex and each increments a shared counter, then check the final value.",
  "Concurrency tests use loom: types from loom::sync / loom::thread under #[cfg(loom)], driven inside loom::model(|| { ... }); never std::thread for a concurrency test.",
  "the test drives the scenario inside loom::model(...) using loom types; does NOT use std::thread directly"),
 ("tokio/src/sync/broadcast.rs", "the broadcast channel",
  "Implement channel(capacity: usize) that creates a broadcast channel and panics if capacity is 0.",
  "Functions that can panic document the triggering conditions under a `# Panics` section of the doc comment.",
  "the doc comment includes a `# Panics` section describing when it panics"),
 ("tokio/src/task/join_set.rs", "the JoinSet",
  "Implement a small method that reads a value through a raw pointer the struct already holds (it contains exactly one unsafe block).",
  "Every unsafe block is preceded by a `// SAFETY:` comment explaining why the operation is sound.",
  "there is a `// SAFETY:` comment immediately before the unsafe block"),
 ("tokio/src/time/timeout.rs", "the timeout combinator",
  "Implement an async fn run_with_deadline(fut, dur) that polls fut but returns Err(Elapsed) if it does not finish within dur, without heap-allocating the future.",
  "Pin a future to the stack with tokio::pin!(fut) so it can be polled by reference; never Box::pin just to obtain a Pin.",
  "uses tokio::pin!(...) to pin the future on the stack; does NOT use Box::pin"),
 ("tokio/src/runtime/handle.rs", "the runtime handle",
  "Add diagnostic logging that records when a task is spawned on the runtime.",
  "Emit diagnostics with the tracing crate macros (tracing::trace!/debug!/info!); never println! or the log crate.",
  "uses a tracing:: macro (for example tracing::trace! or tracing::debug!); does NOT use println! or log::"),
]

def build_and_retrieve(path, lesson):
    repo = tempfile.mkdtemp(prefix="keel-rust-")
    sh(["init"], cwd=repo)
    for u in (["config", "user.email", "b@e.com"], ["config", "user.name", "bench"]):
        subprocess.run(["git", "-C", repo] + u, capture_output=True)
    dst = pathlib.Path(repo, path)
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(pathlib.Path(CORPUS, path), dst)
    sh(["add", "-A"], cwd=repo)
    sh(["commit", "-m", f"add {path}"], cwd=repo)
    sh(["learn", "--task", path, "--lesson", lesson], cwd=repo)
    r = sh(["brief", "--file", path, "--json"], cwd=repo)
    shutil.rmtree(repo, ignore_errors=True)
    try:
        s = json.loads(r.stdout).get("sessions", [])
        return s[0]["lesson"] if s else None
    except Exception:
        return None

def solve(path, hint, task, lesson):
    sysmsg = ("You are a senior engineer working in the Tokio codebase (Rust). Follow the codebase's "
              "own conventions. Output ONLY the function/method/test code — no prose, no fences.")
    user = f"File `{path}` ({hint}).\nTask: {task}"
    if lesson:
        user += f"\n\nRetrieved from this codebase's history (a convention that applies here):\n\"{lesson}\""
    return api(sysmsg, user, SOLVER, max_tokens=1800)

def trial(item):
    (si, cond, _t) = item
    path, hint, task, lesson, crit, got = si
    code = solve(path, hint, task, (got or lesson) if cond == "with" else None)
    return (path, cond, judge(path, code, crit))

def run(trials=None, workers=None, verbose=True):
    trials = TRIALS if trials is None else trials
    workers = WORKERS if workers is None else workers
    if not pathlib.Path(CORPUS, "tokio/src/sync/mutex.rs").exists():
        raise SystemExit(f"corpus not found at {CORPUS} (set CORPUS_SRC to a tokio checkout)")
    scen, retr_ok = [], 0
    for s in SCEN:
        got = build_and_retrieve(s[0], s[3]); retr_ok += 1 if (got and s[3][:24] in got) else 0
        scen.append(s + (got,))
    n = len(SCEN)
    if verbose:
        print(f"corpus=rust(tokio) scenarios={n} trials={trials} solver={SOLVER} judge={JUDGE} · {n*2*trials} solves…")
    _items, results = run_trials(scen, trial, trials, workers)

    per, wo, wi = [], 0, 0
    for s in SCEN:
        rw = results.get((s[0], "without"), []); ri = results.get((s[0], "with"), [])
        wo += sum(rw); wi += sum(ri)
        per.append({"scenario": s[0], "without": f"{sum(rw)}/{len(rw)}", "with": f"{sum(ri)}/{len(ri)}"})
    summary = build_summary("corpus-rust", n, trials, retr_ok, wo, wi, per)

    if verbose:
        tot = summary["samples_per_condition"]
        lw, hw = summary["without"]["ci95_pct"]; li, hi = summary["with"]["ci95_pct"]
        print(f"\n{'convention (real tokio file)':<46} {'WITHOUT':>9} {'WITH':>7}")
        print("-" * 64)
        for s in SCEN:
            p = next(x for x in per if x["scenario"] == s[0])
            print(f"{s[0]:<46} {p['without']:>9} {p['with']:>7}")
        print("-" * 64)
        print(f"real conventions retrieved by keel brief: {retr_ok}/{n}")
        print(f"WITHOUT keel brief: {wo}/{tot} = {100*wo/tot:.0f}%   (95% CI {lw:.0f}-{hw:.0f}%)")
        print(f"WITH    keel brief: {wi}/{tot} = {100*wi/tot:.0f}%   (95% CI {li:.0f}-{hi:.0f}%)")
        print(f"LIFT: +{100*(wi-wo)/tot:.0f} points")
    return summary

def main():
    run()

if __name__ == "__main__":
    main()
