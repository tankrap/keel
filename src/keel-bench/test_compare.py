#!/usr/bin/env python3
"""Self-test for the reproduce-within-noise comparator (NEW-1094). No API, no token.

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


def _report(manifest, benches, keel="cafe1234abcd"):
    return {"manifest": {"manifest_sha256": manifest},
            "environment": {"keel": {"git_commit": keel}},
            "benchmarks": benches}


def main():
    fails = 0

    def check(cond, msg):
        nonlocal fails
        print(("  ✓ " if cond else "  ✗ ") + msg)
        fails += 0 if cond else 1

    M = "a" * 64

    # identical runs → reproduces within noise
    a = _report(M, [_cond("b1", 40, 76, 80)])
    r = bench_compare.compare(a, a)
    check(r["reproduces_within_noise"], "identical runs reproduce within noise")
    check(r["manifest_match"] and r["keel_match"], "identical runs: manifest + keel match")

    # small run-to-run wiggle (50% vs 55% at n=80) → not significant → still reproduces
    b_wiggle = _report(M, [_cond("b1", 44, 74, 80)])
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 76, 80)]), b_wiggle)
    check(r["reproduces_within_noise"], "a small wiggle (50%↔55%) is within noise")

    # a large shift in one condition (100% vs 0% at n=80) → significant → FAILS
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 80, 80)]),
                              _report(M, [_cond("b1", 40, 0, 80)]))
    check(not r["reproduces_within_noise"], "a 100%↔0% shift is flagged (not within noise)")
    bad = [c for c in r["conditions"] if not c["consistent"]]
    check(len(bad) == 1 and bad[0]["condition"] == "with", "the WITH condition is the one flagged")

    # different inputs (manifest mismatch) → hard mismatch, never 'reproduces'
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 76, 80)]),
                              _report("b" * 64, [_cond("b1", 40, 76, 80)]))
    check(not r["manifest_match"] and not r["reproduces_within_noise"],
          "a manifest mismatch is a hard fail, not a noise comparison")

    # both conditions all-pass (se == 0 path) → consistent, no divide-by-zero
    r = bench_compare.compare(_report(M, [_cond("b1", 80, 80, 80)]),
                              _report(M, [_cond("b1", 80, 80, 80)]))
    check(r["reproduces_within_noise"], "both-all-pass compares cleanly (no divide-by-zero)")

    # a different keel build is a note, not a failure (still reproduces within noise)
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 76, 80)], keel="1111"),
                              _report(M, [_cond("b1", 42, 74, 80)], keel="2222"))
    check(not r["keel_match"] and r["reproduces_within_noise"],
          "a different keel build is noted but still reproduces within noise")

    # no shared benchmarks → cannot claim reproduction
    r = bench_compare.compare(_report(M, [_cond("b1", 40, 76, 80)]),
                              _report(M, [_cond("b2", 40, 76, 80)]))
    check(not r["reproduces_within_noise"] and set(r["unmatched"]) == {"b1", "b2"},
          "no shared benchmarks → not a reproduction, unmatched reported")

    print()
    if fails == 0:
        print("compare self-test: PASS")
        return 0
    print(f"compare self-test: FAIL ({fails})")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
