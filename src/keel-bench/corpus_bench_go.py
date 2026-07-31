#!/usr/bin/env python3
"""
Real-corpus flywheel benchmark — Go (Prometheus). Third language, alongside VS Code (TS) and Django
(PY), to show the flywheel lift holds across a statically-typed systems language too.

Same method as corpus_bench.py: 12 real, idiomatic Go conventions grounded by their frequency in a
fresh prometheus/prometheus checkout (fmt.Errorf %w 849, errors.Is/As 226, ctx context.Context 519,
t.Helper 163, defer .Close 326, labels.* 4114, testify 11532…). Each attached to the real Go file
it governs; retrieved by `keel brief`. Solve WITHOUT vs WITH, T trials; dual judge (Sonnet 5) scores
compliance. Solver = Opus 4.8.

Env: CORPUS_SRC (a prometheus checkout, default ~/keel-go-demo), TRIALS (3), WORKERS (6).
"""
import json, os, subprocess, tempfile, shutil, pathlib

from bench_common import (api, judge, sh, run_trials, build_summary,
                          SOLVER, JUDGE, TRIALS, WORKERS)

CORPUS = os.environ.get("CORPUS_SRC", str(pathlib.Path.home() / "keel-go-demo"))

# (real prometheus file, module hint, task, real grounded convention, judge compliance-criterion)
SCEN = [
 ("storage/fanout.go", "the storage fan-out module",
  "Implement readAll(readers) that reads from each reader and returns the first error, annotated with context.",
  "Wrap errors with fmt.Errorf using the %w verb (fmt.Errorf(\"read failed: %w\", err)) to preserve the error chain; never %v or a bare string.",
  "wraps the error with fmt.Errorf(...: %w, err); does NOT use %v or a plain concatenated string"),
 ("scrape/scrape.go", "the scrape module",
  "Handle a scrape error: return nil early if it is the sentinel errBodySizeLimitExceeded, else propagate.",
  "Compare against sentinel errors with errors.Is(err, ErrX); never err == ErrX (that breaks on wrapped errors).",
  "uses errors.Is(err, ...) to test the sentinel; does NOT use == against the sentinel error"),
 ("config/config.go", "the config module",
  "Implement Load(source) that reads a config file and validates it (does I/O).",
  "Functions that do I/O or long-running work take ctx context.Context as their FIRST parameter.",
  "takes ctx context.Context as the first parameter"),
 ("promql/engine.go", "the PromQL engine",
  "Implement evalQuery that runs a query but must not run longer than a deadline.",
  "Bound a long-running operation with context.WithTimeout and respect ctx.Done()/cancellation; never an unbounded loop.",
  "uses context.WithTimeout (or otherwise respects ctx cancellation/Done); NOT an unbounded operation"),
 ("tsdb/head.go", "the TSDB head block",
  "Implement a method that takes the head's mutex, mutates a counter, and returns.",
  "Release a lock/resource with defer right after acquiring it (mu.Lock(); defer mu.Unlock()); never unlock manually at each return path.",
  "uses defer to release the lock (e.g. defer mu.Unlock()); NOT a manual unlock at each return"),
 ("tsdb/db.go", "the TSDB database",
  "Add lazy one-time initialization of a background compactor to the DB.",
  "Guard one-time initialization with sync.Once; never a hand-rolled boolean flag plus mutex.",
  "uses sync.Once for the one-time init; does NOT use a manual bool flag + mutex"),
 ("model/labels/labels_common.go", "the labels module",
  "Build a Labels set for a metric from a list of name/value pairs.",
  "Construct label sets with labels.FromStrings(...) or a labels.Builder; never build the underlying slice/map by hand.",
  "uses labels.FromStrings(...) or labels.Builder; does NOT hand-build the label slice/map"),
 ("web/api/v1/api.go", "the HTTP API v1",
  "Implement a handler that parses a 'query' param, loads data, and writes JSON.",
  "Handle errors with guard clauses (if err != nil { return ... }) immediately after each call; never nest the happy path inside else branches.",
  "uses guard-clause error handling (if err != nil { return }); does NOT nest the happy path inside else"),
 ("notifier/alert.go", "the notifier/alert module",
  "Add a metric counting the number of alerts sent by the notifier.",
  "Define metrics with prometheus.NewCounter(...) and register them (MustRegister); never a plain global counter variable.",
  "uses prometheus.NewCounter (or NewCounterVec) and registration; NOT a plain global int counter"),
 ("rules/manager.go", "the rules manager",
  "Evaluate a set of rule groups concurrently and wait for all of them to finish.",
  "Coordinate concurrent goroutines with sync.WaitGroup (or errgroup) and Wait before returning; never fire-and-forget goroutines.",
  "uses sync.WaitGroup/errgroup and waits (wg.Wait / g.Wait) before returning; NOT fire-and-forget goroutines"),
 ("storage/remote/client.go", "the remote-write client",
  "Implement send(ctx, req) that POSTs bytes to the remote endpoint and reads the response.",
  "Use the request context and always defer resp.Body.Close() on an HTTP response; never leak the body.",
  "closes the response body with defer resp.Body.Close() and uses the request context"),
 ("util/httputil/compression.go", "the HTTP compression util",
  "Implement a helper that wraps a byte stream to gzip-compress it.",
  "Accept io.Reader/io.Writer interfaces for stream parameters (not concrete *os.File / *bytes.Buffer); return the wrapped interface.",
  "accepts io.Reader / io.Writer interface parameters, not concrete stream types"),
]

def build_and_retrieve(path, lesson):
    repo = tempfile.mkdtemp(prefix="keel-go-")
    for u in (["config", "user.email", "b@e.com"], ["config", "user.name", "bench"]):
        subprocess.run(["git", "-C", repo] + u, capture_output=True)
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
    sysmsg = ("You are a senior engineer working in the Prometheus codebase (Go). Follow the "
              "codebase's own conventions. Output ONLY the function/method code — no prose, no fences.")
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
    if not pathlib.Path(CORPUS, "promql/engine.go").exists():
        raise SystemExit(f"corpus not found at {CORPUS} (set CORPUS_SRC to a prometheus checkout)")
    scen, retr_ok = [], 0
    for s in SCEN:
        got = build_and_retrieve(s[0], s[3]); retr_ok += 1 if (got and s[3][:24] in got) else 0
        scen.append(s + (got,))
    n = len(SCEN)
    if verbose:
        print(f"corpus=go(prometheus) scenarios={n} trials={trials} solver={SOLVER} judge={JUDGE} · {n*2*trials} solves…")
    _items, results = run_trials(scen, trial, trials, workers)

    per, wo, wi = [], 0, 0
    for s in SCEN:
        rw = results.get((s[0], "without"), []); ri = results.get((s[0], "with"), [])
        wo += sum(rw); wi += sum(ri)
        per.append({"scenario": s[0], "without": f"{sum(rw)}/{len(rw)}", "with": f"{sum(ri)}/{len(ri)}"})
    summary = build_summary("corpus-go", n, trials, retr_ok, wo, wi, per)

    if verbose:
        tot = summary["samples_per_condition"]
        lw, hw = summary["without"]["ci95_pct"]; li, hi = summary["with"]["ci95_pct"]
        print(f"\n{'convention (real prometheus file)':<46} {'WITHOUT':>9} {'WITH':>7}")
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
