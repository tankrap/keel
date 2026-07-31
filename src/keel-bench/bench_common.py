#!/usr/bin/env python3
"""Shared plumbing for keel's live-LLM flywheel benchmarks (NEW-1094).

Both `flywheel_bench.py` (synthetic conventions) and `corpus_bench.py` (real VS Code
conventions) import from here so the API client, the Wilson CI, the dual judge, the keel
shell-out and the parallel trial runner exist in exactly one place.

Key handling: the Anthropic API key is read from ~/.claude-token, lazily and cached, and is
NEVER printed or logged. Reading it lazily (inside api(), not at import) is what lets
`run_suite.py --dry-run` import every harness for free on a machine without a token.

Models are pinned: solver = claude-opus-4-8 (the agent under test), judge = claude-sonnet-5.
No `temperature` field is ever sent — opus-4-8 uses adaptive thinking, which forces temp=1 and
returns HTTP 400 if a temperature is supplied. The 3-trials-per-scenario design absorbs the
resulting run-to-run variance directly.
"""
import json
import math
import os
import pathlib
import random
import subprocess
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

API_URL = "https://api.anthropic.com/v1/messages"
API_VERSION = "2023-06-01"
SOLVER = "claude-opus-4-8"   # the agent under test — must be capable
JUDGE = "claude-sonnet-5"    # strict rule-compliance yes/no

KEEL = os.environ.get("KEEL_BIN", str(pathlib.Path.home() / "keel/src/rust/target/release/keel"))
TRIALS = int(os.environ.get("TRIALS", "3"))
WORKERS = int(os.environ.get("WORKERS", "6"))

_KEY = None


def _key():
    """Read the API key from ~/.claude-token once, cache it, and never expose it."""
    global _KEY
    if _KEY is None:
        _KEY = (pathlib.Path.home() / ".claude-token").read_text().strip()
    return _KEY


def api(system, user, model, max_tokens=1600):
    """One Messages API call with retry/backoff on 429/500/529. No temperature field (see module doc)."""
    body = json.dumps({"model": model, "max_tokens": max_tokens, "system": system,
                       "messages": [{"role": "user", "content": user}]}).encode()
    for attempt in range(6):
        req = urllib.request.Request(API_URL, data=body, headers={
            "x-api-key": _key(), "anthropic-version": API_VERSION, "content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                d = json.load(r)
            return "".join(b.get("text", "") for b in d.get("content", []) if b.get("type") == "text")
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 529) and attempt < 5:
                time.sleep((2 ** attempt) + random.random())
                continue
            raise
        except Exception:
            if attempt < 5:
                time.sleep((2 ** attempt) + random.random())
                continue
            raise
    return ""


def sh(args, cwd=None):
    """Shell out to the keel binary."""
    return subprocess.run([KEEL] + args, cwd=cwd, capture_output=True, text=True)


def judge(label, code, crit):
    """Strict dual judge: two Sonnet votes, PASS only if both say YES (fewer false positives)."""
    q = (f"Compliance check: the code {crit}.\n\nCode for `{label}`:\n```\n{code}\n```\n\n"
         "Does the code satisfy the check? Answer with exactly one word: YES or NO.")
    votes = 0
    for _ in range(2):
        a = api("You are a strict code reviewer. Judge only the stated compliance check.", q, JUDGE, max_tokens=600).strip().upper()
        toks = a.split()
        if (toks and toks[-1].startswith("YES")) or a.startswith("YES"):
            votes += 1
    return votes >= 2


def wilson(k, n, z=1.96):
    """Wilson score interval for a binomial proportion (default 95%)."""
    if n == 0:
        return (0.0, 0.0)
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = (z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n))) / d
    return (max(0, c - h), min(1, c + h))


def run_trials(scen, trial_fn, trials=None, workers=None):
    """Fan every (scenario × condition × trial) out across a thread pool.

    `trial_fn(item)` receives `(scenario_row, cond, trial_index)` and must return
    `(key, cond, ok_bool)`. Results come back as a dict keyed by `(key, cond)` → [ok, ...].
    """
    trials = TRIALS if trials is None else trials
    workers = WORKERS if workers is None else workers
    items = [(si, cond, t) for si in scen for cond in ("without", "with") for t in range(trials)]
    results = {}
    with ThreadPoolExecutor(max_workers=workers) as ex:
        futs = {ex.submit(trial_fn, it): it for it in items}
        for f in as_completed(futs):
            key, cond, ok = f.result()
            results.setdefault((key, cond), []).append(ok)
    return items, results


def build_summary(bench_id, n, trials, retr_ok, wo, wi, per_scenario):
    """Standard result record for one benchmark, with Wilson 95% CIs on each condition."""
    tot = n * trials
    lw, hw = wilson(wo, tot)
    li, hi = wilson(wi, tot)
    return {
        "id": bench_id,
        "solver": SOLVER,
        "judge": JUDGE,
        "scenarios": n,
        "trials": trials,
        "samples_per_condition": tot,
        "retrieved": retr_ok,
        "without": {"correct": wo, "total": tot,
                    "pct": round(100 * wo / tot, 1) if tot else 0.0,
                    "ci95_pct": [round(100 * lw, 1), round(100 * hw, 1)]},
        "with": {"correct": wi, "total": tot,
                 "pct": round(100 * wi / tot, 1) if tot else 0.0,
                 "ci95_pct": [round(100 * li, 1), round(100 * hi, 1)]},
        "lift_points": round(100 * (wi - wo) / tot) if tot else 0,
        "per_scenario": per_scenario,
    }
