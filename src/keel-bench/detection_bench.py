#!/usr/bin/env python3
"""Deterministic detection benchmark for keel's semantic-diff anomaly engine (no LLM, no API cost).

The flywheel benchmarks measure lesson RETRIEVAL lift. This one measures the other half of keel's
value — the deterministic REVIEW engine (`keel-store/src/semdiff.rs` + `keel-cmd`): does it catch the
subtle, easy-to-miss bugs an agent smuggles into a bulk edit, and does it stay QUIET on clean and
legitimately-varied edits? Because the detection is deterministic (no model), the right way to verify
it is to run it over many generated scenarios and measure two rates directly:

  recall  — of N changes each carrying ONE injected bug, how many does keel flag?  (want ~100%)
  FP rate — of N clean/legitimately-varied changes, how many does keel WRONGLY flag?  (want ~0%)

Bug classes injected (each a one-line slip hidden in an otherwise-mechanical bulk edit):
  op-flip-added    a flipped comparison/logical operator among the added lines   (Level 1b)
  literal-smuggle  a minority numeric literal among the added lines              (Level 1)
  op-flip-removed  a dropped guard whose removed operator broke from its bulk    (removed side, 1b)

Clean controls (must NOT flag):
  clean-uniform    a bulk edit with uniform operator + literal
  clean-varied-op  a bulk edit where operators legitimately vary (a comparison ladder — no majority)
  clean-varied-lit a bulk edit where a literal legitimately varies (an index 0..N)

Each scenario builds a throwaway keel repo, sets a native baseline, applies the change to the working
tree, and runs `keel native diff --semantic --json`; the check is a pure predicate over that JSON.

    KEEL_BIN=/path/to/keel python3 detection_bench.py            # ~60 scenarios, a few seconds
    N=20 python3 detection_bench.py                              # more scenarios per class
"""
import json
import os
import pathlib
import random
import subprocess
import tempfile

from bench_common import KEEL, sh, wilson

N = int(os.environ.get("N", "12"))  # scenarios per class
SEED = int(os.environ.get("SEED", "1094"))  # fixed → reproducible scenario tables

IDS = ["idx", "cur", "val", "off", "pos", "len", "cnt", "key", "row", "col", "amt", "qty", "raw", "acc"]
COMPARE = ["<=", "<", ">=", ">", "==", "!="]
LOGICAL = ["&&", "||"]


def _repo():
    """A fresh git+keel repo in a temp dir; returns its path (caller removes it)."""
    d = pathlib.Path(tempfile.mkdtemp(prefix="keel-detect-"))
    subprocess.run(["git", "init", "-q"], cwd=d, check=True)
    subprocess.run(["git", "config", "user.email", "b@b.co"], cwd=d, check=True)
    subprocess.run(["git", "config", "user.name", "bench"], cwd=d, check=True)
    return d


def _baseline(d, name, text):
    (d / name).write_text(text)
    sh(["add", name], cwd=d)
    sh(["native", "commit", "-m", "base"], cwd=d)


def _semantic_json(d):
    r = sh(["native", "diff", "--semantic", "--json"], cwd=d)
    try:
        return json.loads(r.stdout)
    except Exception:
        return {}


# ── the two anomaly-presence predicates over the semantic-diff JSON ───────────────────────────────
def _added_flagged(j):
    """Any anomaly on the added side — a group literal anomaly or an operator anomaly."""
    op = bool(j.get("operator_anomalies"))
    lit = any(g.get("anomalies") for g in j.get("groups", []))
    return op or lit


def _removed_flagged(j):
    rm = j.get("removed", {})
    op = bool(rm.get("operator_anomalies"))
    lit = any(g.get("anomalies") for g in rm.get("groups", []))
    return op or lit


def _total(j):
    return int(j.get("anomalies", 0)) + int(j.get("removed", {}).get("anomalies", 0))


# ── scenario generators: return (baseline_text, changed_text) for `mod.ts` ────────────────────────
def _lines_added(rng, op_of, lit_of, n):
    """n near-identical added lines; `op_of(k)`/`lit_of(k)` pick each line's operator/literal."""
    a, b = rng.sample(IDS, 2)
    return "".join(f"const r{k} = {a}{k} {op_of(k)} lim && ok({b}{k}, {lit_of(k)});\n" for k in range(n))


def gen_op_flip_added(rng):
    n = rng.randint(6, 9)
    maj = rng.choice(COMPARE + LOGICAL)
    minr = rng.choice([o for o in (COMPARE + LOGICAL) if o != maj])
    odd = rng.randrange(n)
    base = "export const lim = 10;\n"
    change = base + _lines_added(rng, lambda k: minr if k == odd else maj, lambda k: 1, n)
    return base, change, ("added", True)


def gen_literal_smuggle(rng):
    n = rng.randint(6, 9)
    maj, minr = rng.sample([2, 3, 4, 5, 8, 16], 2)
    odd = rng.randrange(n)
    base = "export const lim = 10;\n"
    change = base + _lines_added(rng, lambda k: "<=", lambda k: minr if k == odd else maj, n)
    return base, change, ("added", True)


def gen_op_flip_removed(rng):
    """Baseline holds n guard lines (one with a flipped operator); the change deletes them all —
    the removed side must surface the odd guard as an operator anomaly (the 'dropped guard' case)."""
    n = rng.randint(6, 9)
    maj = rng.choice(COMPARE)
    minr = rng.choice([o for o in COMPARE if o != maj])
    odd = rng.randrange(n)
    a = rng.choice(IDS)
    guards = "".join(f"if ({a}{k} {minr if k == odd else maj} max) throw Error();\n" for k in range(n))
    base = "export const max = 99;\n" + guards
    change = "export const max = 99;\n"  # every guard deleted
    return base, change, ("removed", True)


def gen_clean_uniform(rng):
    n = rng.randint(6, 9)
    op = rng.choice(COMPARE + LOGICAL)
    base = "export const lim = 10;\n"
    change = base + _lines_added(rng, lambda k: op, lambda k: 7, n)
    return base, change, ("clean", False)


def gen_clean_varied_op(rng):
    """Operators legitimately vary line-to-line (a comparison ladder) → no dominant value → no flag."""
    n = len(COMPARE)
    base = "export const lim = 10;\n"
    change = base + _lines_added(rng, lambda k: COMPARE[k % len(COMPARE)], lambda k: 1, n)
    return base, change, ("clean", False)


def gen_clean_varied_lit(rng):
    n = rng.randint(6, 9)
    base = "export const lim = 10;\n"
    change = base + _lines_added(rng, lambda k: "<=", lambda k: k, n)  # literal = index, uniform spread
    return base, change, ("clean", False)


CLASSES = {
    "op-flip-added": gen_op_flip_added,
    "literal-smuggle": gen_literal_smuggle,
    "op-flip-removed": gen_op_flip_removed,
    "clean-uniform": gen_clean_uniform,
    "clean-varied-op": gen_clean_varied_op,
    "clean-varied-lit": gen_clean_varied_lit,
}


def run_scenario(gen, rng):
    """Build the repo, apply the change, run the engine, and return whether the OUTCOME was correct
    (bug flagged for a bug class; nothing flagged for a clean class)."""
    base, change, (side, is_bug) = gen(rng)
    d = _repo()
    try:
        _baseline(d, "mod.ts", base)
        (d / "mod.ts").write_text(change)
        j = _semantic_json(d)
        if side == "added":
            got = _added_flagged(j)
        elif side == "removed":
            got = _removed_flagged(j)
        else:  # clean
            got = _total(j) > 0
        # correct == flagged-a-bug (recall) or stayed-quiet-on-clean (precision)
        return (got == is_bug), _total(j)
    finally:
        subprocess.run(["rm", "-rf", str(d)])


def main():
    if not pathlib.Path(KEEL).exists():
        raise SystemExit(f"keel binary not found at {KEEL} (set KEEL_BIN)")
    rng = random.Random(SEED)
    print(f"keel semantic-diff detection benchmark · {N} scenarios/class · KEEL={KEEL}\n")
    results = {}
    bug_k = bug_n = fp_k = clean_n = 0
    for name, gen in CLASSES.items():
        ok = 0
        for _ in range(N):
            correct, _t = run_scenario(gen, rng)
            ok += correct
        results[name] = (ok, N)
        is_clean = name.startswith("clean")
        lo, hi = wilson(ok, N)
        kind = "quiet " if is_clean else "recall"
        print(f"  {name:18s}  {kind} {ok:2d}/{N}   ({100*ok/N:5.1f}%   95% CI [{100*lo:4.1f}, {100*hi:4.1f}])")
        if is_clean:
            fp_k += (N - ok)  # a clean scenario "not ok" == a false positive
            clean_n += N
        else:
            bug_k += ok
            bug_n += N
    rlo, rhi = wilson(bug_k, bug_n)
    flo, fhi = wilson(fp_k, clean_n)
    print("\n  ── pooled ─────────────────────────────────────────────────")
    print(f"  recall (bugs caught):     {bug_k}/{bug_n}   ({100*bug_k/bug_n:.1f}%   CI [{100*rlo:.1f}, {100*rhi:.1f}])")
    print(f"  false positives (clean):  {fp_k}/{clean_n}   ({100*fp_k/clean_n:.1f}%   CI [{100*flo:.1f}, {100*fhi:.1f}])")
    out = {
        "benchmark": "semantic-diff-detection",
        "n_per_class": N,
        "seed": SEED,
        "per_class": {k: {"ok": v[0], "n": v[1]} for k, v in results.items()},
        "recall": {"k": bug_k, "n": bug_n},
        "false_positives": {"k": fp_k, "n": clean_n},
    }
    (pathlib.Path(__file__).resolve().parent / "detection_result.json").write_text(json.dumps(out, indent=2))
    print("\n  wrote detection_result.json")


if __name__ == "__main__":
    main()
