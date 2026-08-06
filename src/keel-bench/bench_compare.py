#!/usr/bin/env python3
"""Do two benchmark runs agree within statistical noise? (NEW-1094)

The LLM parts of the suite are stochastic — the same pinned inputs won't reproduce byte-for-byte, so
"reproducible" for them means *no statistically significant difference* between two runs, not equal
counts. This compares two `bench-report.json` artifacts and decides that, per benchmark × condition,
with a two-proportion z-test at the suite's Wilson z (default 1.96 ≈ 95%).

Crucially it is gated on the manifest: if the two runs' `manifest_sha256` differ, their *inputs*
differed, so a noise comparison is meaningless and this reports a hard mismatch instead. (A differing
`environment.keel` commit is surfaced as a note — a different keel build is expected to still
reproduce within noise, but you should know it changed.)

No API, no token: it reads two JSON files. Run:

    python3 run_suite.py --compare run_a.json run_b.json
"""
import json
import math

Z_DEFAULT = 1.96  # 95%, matches bench-config.json run.wilson_z


def _two_proportion_consistent(k1, n1, k2, n2, z=Z_DEFAULT):
    """Two-sided two-proportion z-test. Returns (consistent, zstat). `consistent` means the two runs'
    success rates do NOT differ significantly at the given z — i.e. they reproduce within noise."""
    if n1 == 0 or n2 == 0:
        return False, None  # can't compare an empty condition
    p1, p2 = k1 / n1, k2 / n2
    pool = (k1 + k2) / (n1 + n2)
    se = math.sqrt(pool * (1 - pool) * (1 / n1 + 1 / n2))
    if se == 0:
        # pooled rate is exactly 0 or 1 → both runs are all-fail or all-pass → identical, consistent
        return (p1 == p2), 0.0
    zstat = (p1 - p2) / se
    return abs(zstat) <= z, zstat


def _manifest_sha(report):
    return (report.get("manifest") or {}).get("manifest_sha256")


def _keel_commit(report):
    return ((report.get("environment") or {}).get("keel") or {}).get("git_commit")


def _by_id(report):
    return {b["id"]: b for b in report.get("benchmarks", [])}


def compare(report_a, report_b, z=Z_DEFAULT):
    """Compare two report dicts. Returns a result dict:
      {manifest_match, manifest_a, manifest_b, keel_a, keel_b, keel_match,
       conditions: [{id, condition, a_pct, b_pct, a, b, z, consistent}],
       unmatched: [...ids present in only one report...],
       reproduces_within_noise: bool}
    `reproduces_within_noise` is True only if the manifests match, at least one condition was
    compared, and every compared condition is consistent."""
    ma, mb = _manifest_sha(report_a), _manifest_sha(report_b)
    manifest_match = ma is not None and ma == mb
    ka, kb = _keel_commit(report_a), _keel_commit(report_b)

    a_by, b_by = _by_id(report_a), _by_id(report_b)
    shared = sorted(set(a_by) & set(b_by))
    unmatched = sorted(set(a_by) ^ set(b_by))

    conditions = []
    for bid in shared:
        for cond in ("without", "with"):
            ca, cb = a_by[bid].get(cond), b_by[bid].get(cond)
            if not ca or not cb:
                continue
            consistent, zstat = _two_proportion_consistent(
                ca["correct"], ca["total"], cb["correct"], cb["total"], z)
            conditions.append({
                "id": bid, "condition": cond,
                "a_pct": ca.get("pct"), "b_pct": cb.get("pct"),
                "a": [ca["correct"], ca["total"]], "b": [cb["correct"], cb["total"]],
                "z": None if zstat is None else round(zstat, 2),
                "consistent": consistent,
            })

    reproduces = bool(manifest_match and conditions and all(c["consistent"] for c in conditions))
    return {
        "manifest_a": ma, "manifest_b": mb, "manifest_match": manifest_match,
        "keel_a": ka, "keel_b": kb, "keel_match": (ka == kb),
        "conditions": conditions, "unmatched": unmatched,
        "reproduces_within_noise": reproduces,
    }


def load_report(path):
    with open(path) as f:
        return json.load(f)


if __name__ == "__main__":
    import sys
    if len(sys.argv) != 3:
        print("usage: bench_compare.py REPORT_A.json REPORT_B.json", file=sys.stderr)
        raise SystemExit(2)
    print(json.dumps(compare(load_report(sys.argv[1]), load_report(sys.argv[2])), indent=2))
