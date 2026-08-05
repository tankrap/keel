#!/usr/bin/env python3
"""Self-test for the reproducibility manifest (NEW-1094). No API, no token.

Locks the manifest's guarantees: it is deterministic, a change to any pinned input changes the SHA,
canonical hashing is key-order-independent, and a config/scenario-count drift is caught. Run:

    python3 test_manifest.py
"""
import copy
import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench_manifest  # noqa: E402

CFG = json.loads((HERE / "bench-config.json").read_text())


def main():
    fails = 0

    def check(cond, msg):
        nonlocal fails
        print(("  ✓ " if cond else "  ✗ ") + msg)
        fails += 0 if cond else 1

    m = bench_manifest.build_manifest(CFG)
    check(m == bench_manifest.build_manifest(CFG), "manifest is deterministic across builds")
    check(len(m["manifest_sha256"]) == 64 and len(m["config_sha256"]) == 64, "sha256 fields are 64-hex")
    check(len(m["benchmarks"]) == len(CFG["benchmarks"]), "one manifest entry per configured benchmark")

    # the prompts/max_tokens that live in code (not SCEN) are bound: every entry carries its harness's
    # source SHA, and the shared judge prompt + model constants are bound via bench_common's source.
    check(
        all(len(b.get("source_sha256", "")) == 64 for b in m["benchmarks"]),
        "each benchmark binds its harness source (prompts + max_tokens), not just SCEN",
    )
    check(len(m.get("common_source_sha256", "")) == 64, "shared bench_common source is bound (judge prompt, model constants)")

    # a config change (one more trial) must change the manifest SHA — inputs are truly bound in
    cfg_trials = copy.deepcopy(CFG)
    cfg_trials["run"]["trials"] += 1
    check(
        bench_manifest.build_manifest(cfg_trials)["manifest_sha256"] != m["manifest_sha256"],
        "a result-affecting config change (trials) yields a different manifest SHA",
    )

    # scenario CONTENT is bound: two different scenario tables must hash differently (not just count)
    check(
        bench_manifest._sha(bench_manifest._canon([["a", "b"]]))
        != bench_manifest._sha(bench_manifest._canon([["a", "c"]])),
        "scenario content is bound (different SCEN rows → different SHA)",
    )

    # workers is pure parallelism — it must NOT change the SHA (else identical runs look different)
    cfg_workers = copy.deepcopy(CFG)
    cfg_workers["run"]["workers"] += 3
    check(
        bench_manifest.build_manifest(cfg_workers)["manifest_sha256"] == m["manifest_sha256"],
        "worker count does NOT change the SHA (not over-bound to parallelism)",
    )

    # the pinned model IDs must equal the constants the run actually uses — a drift is refused loudly
    cfg_model = copy.deepcopy(CFG)
    cfg_model["models"]["solver"] = "claude-not-the-real-model"
    try:
        bench_manifest.build_manifest(cfg_model)
        check(False, "a config-vs-code model-ID drift raises (config can't be decorative)")
    except ValueError:
        check(True, "a config-vs-code model-ID drift raises (config can't be decorative)")

    # canonical hashing ignores dict key order (so platform/insertion order can't change the SHA)
    check(
        bench_manifest._canon({"a": 1, "b": 2}) == bench_manifest._canon({"b": 2, "a": 1}),
        "canonical serialization is key-order-independent",
    )

    # a scenario-count drift between the config and a harness's SCEN table is caught, not hashed over
    cfg_bad = copy.deepcopy(CFG)
    cfg_bad["benchmarks"][0]["scenarios"] += 1
    try:
        bench_manifest.build_manifest(cfg_bad)
        check(False, "a config/SCEN scenario-count mismatch raises")
    except ValueError:
        check(True, "a config/SCEN scenario-count mismatch raises")

    print()
    if fails == 0:
        print("manifest self-test: PASS")
        return 0
    print(f"manifest self-test: FAIL ({fails})")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
