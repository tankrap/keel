#!/usr/bin/env python3
"""Self-test for the reproduce/equivalence comparator (NEW-1094). No API, no token.

Encodes the honest three-way contract, including the crucial one: a moderate true difference the
sample is too small to resolve must come back INCONCLUSIVE, never a false "equivalent".

    python3 test_compare.py
"""
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench_compare  # noqa: E402


def _cond(bid, wo, wi, tot):
    return {"id": bid,
            "without": {"correct": wo, "total": tot, "pct": round(100 * wo / tot, 1)},
            "with": {"correct": wi, "total": tot, "pct": round(100 * wi / tot, 1)}}


def _report(manifest, benches, keel="cafe1234abcd", wilson_z=1.96):
    return {"manifest": {"manifest_sha256": manifest},
            "environment": {"keel": {"git_commit": keel}},
            "run": {"wilson_z": wilson_z},
            "benchmarks": benches}


def main():
    fails = 0

    def check(cond, msg):
        nonlocal fails
        print(("  ✓ " if cond else "  ✗ ") + msg)
        fails += 0 if cond else 1

    M = "a" * 64

    # identical runs (95% base rate, n=80) → EQUIVALENT within the default ±20pp margin
    a = _report(M, [_cond("b1", 40, 76, 80)])
    r = bench_compare.compare(a, a)
    check(r["verdict"] == "equivalent", "identical runs → EQUIVALENT")
    check(r["manifest_match"] and r["keel_match"], "identical runs: manifest + keel match")

    # a large regression (with: 100% → 0% at n=80) → REGRESSION (Holm-corrected, real difference)
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 80, 80)]),
                              _report(M, [_cond("b1", 40, 0, 80)]))
    check(r["verdict"] == "regression", "a 100%→0% shift → REGRESSION")
    bad = [c for c in r["conditions"] if c["regressed"]]
    check(len(bad) == 1 and bad[0]["condition"] == "with", "the WITH condition is the one flagged")

    # THE KEY CASE: a moderate ~17pp difference at n=48 is NOT significant AND cannot be certified
    # equivalent within ±20pp → INCONCLUSIVE, never a false "equivalent". This is the whole point:
    # low power makes the tool refuse to certify, not rubber-stamp.
    r = bench_compare.compare(_report(M, [_cond("b1", 24, 30, 48)]),
                              _report(M, [_cond("b1", 32, 30, 48)]))
    check(r["verdict"] == "inconclusive", "a moderate underpowered gap → INCONCLUSIVE (not a false 'equivalent')")
    check(not any(c["equivalent"] for c in r["conditions"] if c["condition"] == "without"),
          "the underpowered WITHOUT condition is not certified equivalent")

    # widening the margin to ±40pp lets that same gap certify as EQUIVALENT (margin is honest/tunable)
    r = bench_compare.compare(_report(M, [_cond("b1", 24, 30, 48)]),
                              _report(M, [_cond("b1", 32, 30, 48)]), margin=0.40)
    check(r["verdict"] == "equivalent", "widening --margin to ±40pp certifies the same gap (tunable)")

    # different inputs (manifest mismatch) → never a reproduction verdict
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 76, 80)]),
                              _report("b" * 64, [_cond("b1", 40, 76, 80)]))
    check(r["verdict"] == "manifest_mismatch", "a manifest mismatch is a hard mismatch, not a reproduction")

    # Holm correction: two mildly-significant conditions (raw p≈0.03 each) must NOT be flagged as a
    # regression at family-wise α (controls the ~40% false-fail rate a naive per-test 0.05 would give).
    #   58/80 vs 44/80 → raw z≈2.28, p≈0.023; with m=2 conditions Holm needs p ≤ 0.05/2 = 0.025 for the
    #   smallest, so a single such condition is borderline — use two to exercise the family-wise guard.
    r = bench_compare.compare(_report(M, [_cond("b1", 58, 58, 80), _cond("b2", 58, 58, 80)]),
                              _report(M, [_cond("b1", 44, 44, 80), _cond("b2", 44, 44, 80)]))
    raw_sig = [c for c in r["conditions"] if c["p_val"] < 0.05]
    flagged = [c for c in r["conditions"] if c["regressed"]]
    check(len(raw_sig) >= 2 and len(flagged) < len(raw_sig),
          "Holm correction spares at least one raw-significant condition (family-wise control)")

    # both conditions all-pass (se == 0 path) → equivalent, no divide-by-zero
    r = bench_compare.compare(_report(M, [_cond("b1", 80, 80, 80)]),
                              _report(M, [_cond("b1", 80, 80, 80)]))
    check(r["verdict"] == "equivalent", "both-all-pass compares cleanly (no divide-by-zero)")

    # malformed condition (missing 'total') is skipped, not crashed → no comparable data
    bad_report = {"manifest": {"manifest_sha256": M}, "run": {"wilson_z": 1.96},
                  "benchmarks": [{"id": "b1", "without": {"correct": 5}, "with": {"correct": 5}}]}
    r = bench_compare.compare(bad_report, bad_report)
    check(r["verdict"] == "no_data" and not r["conditions"], "a malformed condition is skipped, not crashed")

    # a benchmark missing an 'id' doesn't crash _by_id
    idless = {"manifest": {"manifest_sha256": M}, "run": {"wilson_z": 1.96},
              "benchmarks": [{"without": {"correct": 1, "total": 4}, "with": {"correct": 1, "total": 4}}]}
    r = bench_compare.compare(idless, idless)
    check(r["verdict"] == "no_data", "a benchmark without an id is ignored, not crashed")

    print()
    if fails == 0:
        print("compare self-test: PASS")
        return 0
    print(f"compare self-test: FAIL ({fails})")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
