#!/usr/bin/env python3
"""
Adversarial control for the flywheel benchmark — does the lift come from the SPECIFIC retrieved rule,
or just from appending *any* authoritative-looking "convention from this codebase's history"?

Three conditions on the synthetic scenarios (arbitrary, non-guessable rules):
  without : no lesson (baseline)
  with    : the CORRECT lesson for the task
  decoy   : a DIFFERENT scenario's real lesson — a plausible, authoritative-looking, but WRONG rule

Hypotheses:
  H1 (flywheel real):   the lift is the specific content   → decoy ≈ without ≪ with
  H0 (prompting artifact): any lesson helps                → decoy ≈ with

Solver = Opus 4.8, dual judge = Sonnet 5, T trials, Wilson 95% CIs. Reuses the synthetic harness.
"""
import math
from concurrent.futures import ThreadPoolExecutor, as_completed

import flywheel_bench as F
from bench_common import judge, wilson, SOLVER, JUDGE, TRIALS, WORKERS

CONDS = ("without", "with", "decoy")


def lesson_for(idx, cond):
    """The lesson shown for scenario `idx` under a condition. decoy = the NEXT scenario's real rule."""
    if cond == "without":
        return None
    if cond == "with":
        return F.SCEN[idx][3]
    # decoy: a real, authoritative-looking rule from another scenario — wrong for THIS task
    return F.SCEN[(idx + 1) % len(F.SCEN)][3]


def trial(item):
    idx, cond, _t = item
    fname, starter, task, _lesson, crit = F.SCEN[idx]
    code = F.solve(fname, starter, task, lesson_for(idx, cond))
    return (idx, cond, judge(fname, code, crit))


def run(trials=None, workers=None):
    trials = TRIALS if trials is None else trials
    workers = WORKERS if workers is None else workers
    n = len(F.SCEN)
    print(f"adversarial (3-condition) · {n} scenarios × {trials} trials × {len(CONDS)} conds "
          f"= {n*trials*len(CONDS)} solves · solver={SOLVER} judge={JUDGE}")
    items = [(idx, c, t) for idx in range(n) for c in CONDS for t in range(trials)]
    results = {}
    with ThreadPoolExecutor(max_workers=workers) as ex:
        futs = {ex.submit(trial, it): it for it in items}
        for f in as_completed(futs):
            idx, cond, ok = f.result()
            results.setdefault(cond, []).append(ok)

    tot = n * trials
    print(f"\n{'condition':<10} {'correct':>10} {'pct':>6}   95% CI")
    print("-" * 44)
    out = {}
    for c in CONDS:
        k = sum(results.get(c, []))
        lo, hi = wilson(k, tot)
        out[c] = {"correct": k, "total": tot, "pct": round(100 * k / tot, 1), "ci95_pct": [lo, hi]}
        print(f"{c:<10} {f'{k}/{tot}':>10} {100*k/tot:>5.0f}%   {lo:.0f}-{hi:.0f}%")
    print("-" * 44)
    dw = out["with"]["pct"] - out["without"]["pct"]
    dd = out["decoy"]["pct"] - out["without"]["pct"]
    print(f"correct lesson lift : +{dw:.0f} points")
    print(f"decoy lesson  lift  : {dd:+.0f} points   (near 0 ⇒ the lift is the SPECIFIC rule, not just 'a lesson')")
    return {"bench_id": "adversarial-synthetic", "scenarios": n, "trials": trials,
            "samples_per_condition": tot, "conditions": out,
            "lift_correct": round(dw), "lift_decoy": round(dd)}


if __name__ == "__main__":
    run()
