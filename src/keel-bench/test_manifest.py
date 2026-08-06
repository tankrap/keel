#!/usr/bin/env python3
"""Self-test for the reproducibility manifest (NEW-1094). No API, no token.

Locks the manifest's guarantees: it is deterministic, a change to any pinned input changes the SHA,
canonical hashing is key-order-independent, and a config/scenario-count drift is caught. Run:

    python3 test_manifest.py
"""
import copy
import json
import os
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

    # source is actually WIRED INTO the manifest SHA (not just recorded): collapse every source hash
    # to a constant and the manifest SHA must move — i.e. an edit to harness/bench_common source flows
    # through to manifest_sha256. Exercises the real build path, not just field shape.
    orig_ssha = bench_manifest._source_sha
    try:
        bench_manifest._source_sha = lambda name: "0" * 64
        check(
            bench_manifest.build_manifest(CFG)["manifest_sha256"] != m["manifest_sha256"],
            "harness/bench_common source flows into the manifest SHA (editing source moves it)",
        )
    finally:
        bench_manifest._source_sha = orig_ssha

    # the REAL harness SCEN table is what's hashed (not some placeholder): the per-benchmark
    # scenario_sha256 equals a fresh hash of that module's live SCEN.
    real_mod = CFG["benchmarks"][0]["module"]
    real_sha, _ = bench_manifest.scenario_sha(real_mod)
    entry = next(b for b in m["benchmarks"] if b["module"] == real_mod)
    check(entry["scenario_sha256"] == real_sha, "the real harness SCEN table is what the manifest hashes")

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

    # a real-corpus benchmark binds WHICH revision it ran against: changing a pinned commit moves the
    # SHA; a synthetic (no-corpus) benchmark carries no corpus field.
    corpus_idx = next((i for i, b in enumerate(CFG["benchmarks"]) if b.get("corpus")), None)
    if corpus_idx is not None:
        cfg_commit = copy.deepcopy(CFG)
        cfg_commit["benchmarks"][corpus_idx]["corpus"]["commit"] = "0" * 40
        check(
            bench_manifest.build_manifest(cfg_commit)["manifest_sha256"] != m["manifest_sha256"],
            "a pinned corpus-commit change yields a different manifest SHA",
        )
        cid = CFG["benchmarks"][corpus_idx]["id"]
        entry = next(b for b in m["benchmarks"] if b["id"] == cid)
        check("commit" in entry.get("corpus", {}), "a corpus benchmark records its pinned commit in the manifest")
    check(
        all("corpus" not in b for b in m["benchmarks"] if b["module"] == "flywheel_bench"),
        "a synthetic (no-corpus) benchmark carries no corpus pin",
    )

    # verify_corpus must require a CLEAN tree, not just a matching HEAD: a staged/unstaged edit to a
    # corpus file changes the bytes the benchmark reads while HEAD is unchanged. Build a throwaway
    # repo, pin its HEAD, and confirm clean → match, one modified tracked file → dirty.
    import shutil
    import subprocess
    import tempfile
    if shutil.which("git") is None:
        print("  … skipped verify_corpus dirty-gate (git not on PATH)")
    else:
        d = tempfile.mkdtemp(prefix="keel-corpus-verify-")
        try:
            def g(*a):
                return subprocess.run(["git", "-C", d, *a], capture_output=True, text=True)
            g("init", "-q"); g("config", "user.email", "a@b.co"); g("config", "user.name", "a")
            (pathlib.Path(d) / "f.txt").write_text("hello\n")
            g("add", "-A"); g("commit", "-qm", "c1", "--no-gpg-sign")
            head = g("rev-parse", "HEAD").stdout.strip()
            check(len(head) == 40, "throwaway repo produced a real HEAD (dirty-gate test is not vacuous)")
            tcfg = {"benchmarks": [{"id": "tmp", "module": "x",
                                    "corpus": {"repo": "r", "commit": head,
                                               "src_env": "KEEL_TEST_CORPUS_UNSET", "src_default": d}}]}
            os.environ.pop("KEEL_TEST_CORPUS_UNSET", None)
            r_clean = bench_manifest.verify_corpus(tcfg)
            check(r_clean and r_clean[0]["status"] == "match", "verify_corpus: clean checkout at pinned commit → match")
            (pathlib.Path(d) / "f.txt").write_text("hello\n// drift\n")  # modify a tracked file
            r_dirty = bench_manifest.verify_corpus(tcfg)
            check(r_dirty and r_dirty[0]["status"] == "dirty",
                  "verify_corpus: a modified tracked file (HEAD unchanged) → dirty, not match")
        finally:
            shutil.rmtree(d, ignore_errors=True)

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
